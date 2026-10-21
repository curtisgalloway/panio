// Copyright 2026 Curtis Galloway
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and

//! SNMP v2c backend: IF-MIB admin/oper status and POWER-ETHERNET-MIB PoE.

use std::net::ToSocketAddrs;
use std::time::Duration;

use anyhow::Result;
use snmp2::{Oid, SyncSession, Value};

use crate::ops::{err, Backend, Kind, PortStatus};

const SYS_DESCR: &[u64] = &[1, 3, 6, 1, 2, 1, 1, 1, 0];
const IF_NUMBER: &[u64] = &[1, 3, 6, 1, 2, 1, 2, 1, 0];
const IF_ADMIN_STATUS: &[u64] = &[1, 3, 6, 1, 2, 1, 2, 2, 1, 7];
const IF_OPER_STATUS: &[u64] = &[1, 3, 6, 1, 2, 1, 2, 2, 1, 8];
const IF_HIGH_SPEED: &[u64] = &[1, 3, 6, 1, 2, 1, 31, 1, 1, 1, 15];
const PETH_PSE_PORT_ADMIN_ENABLE: &[u64] = &[1, 3, 6, 1, 2, 1, 105, 1, 1, 1, 3];

/// IF-MIB `up(1)` / `down(2)`; POWER-ETHERNET-MIB `true(1)` / `false(2)`.
const UP: i64 = 1;
const DOWN: i64 = 2;

pub struct Snmp {
    sess: SyncSession,
    host: String,
    if_offset: i64,
    poe_group: u64,
}

impl Snmp {
    pub fn new(
        device: &str,
        community: &str,
        if_offset: i64,
        poe_group: u64,
        timeout: Duration,
    ) -> Result<Self> {
        let addr = if device.contains(':') {
            device.to_string()
        } else {
            format!("{device}:161")
        };
        let sa = addr
            .to_socket_addrs()
            .map_err(|e| err(Kind::Unreachable, format!("cannot resolve {device}: {e}")))?
            .next()
            .ok_or_else(|| err(Kind::Unreachable, format!("no address for {device}")))?;
        let sess = SyncSession::new_v2c(sa, community.as_bytes(), Some(timeout), 1)
            .map_err(|e| err(Kind::Unreachable, format!("SNMP socket for {device}: {e}")))?;
        Ok(Snmp {
            sess,
            host: device.to_string(),
            if_offset,
            poe_group,
        })
    }

    fn if_index(&self, port: u32) -> Result<u64> {
        let idx = i64::from(port) + self.if_offset;
        u64::try_from(idx).ok().filter(|i| *i > 0).ok_or_else(|| {
            err(
                Kind::Usage,
                format!(
                    "port {port} with --if-index-offset {} is not a valid ifIndex",
                    self.if_offset
                ),
            )
        })
    }

    fn oid(arcs: &[u64]) -> Result<Oid<'static>> {
        Oid::from(arcs).map_err(|_| err(Kind::Failed, "internal: bad OID"))
    }

    fn no_reply(host: &str, what: &str, e: snmp2::Error) -> anyhow::Error {
        err(
            Kind::Unreachable,
            format!(
                "no usable SNMP reply from {} for {what} ({e:?}): wrong community, SNMP \
                 disabled, or the switch is unreachable",
                host
            ),
        )
    }

    /// GET one integer-like value; `None` for noSuchObject/noSuchInstance.
    fn get_int(&mut self, arcs: &[u64], what: &str) -> Result<Option<i64>> {
        let oid = Self::oid(arcs)?;
        let host = self.host.clone();
        let pdu = self
            .sess
            .get(&oid)
            .map_err(|e| Self::no_reply(&host, what, e))?;
        if pdu.error_status != 0 {
            return Err(err(
                Kind::Failed,
                format!("SNMP error status {} reading {what}", pdu.error_status),
            ));
        }
        let mut vb = pdu.varbinds;
        match vb.next().map(|(_, v)| v) {
            Some(Value::Integer(n)) => Ok(Some(n)),
            Some(Value::Unsigned32(n) | Value::Counter32(n) | Value::Timeticks(n)) => {
                Ok(Some(i64::from(n)))
            }
            Some(Value::NoSuchObject | Value::NoSuchInstance | Value::EndOfMibView) => Ok(None),
            other => Err(err(
                Kind::Failed,
                format!("unexpected SNMP value for {what}: {other:?}"),
            )),
        }
    }

    fn require_int(&mut self, arcs: &[u64], what: &str) -> Result<i64> {
        self.get_int(arcs, what)?.ok_or_else(|| {
            err(
                Kind::Failed,
                format!(
                    "{what}: the switch has no such object (wrong port, or a different \
                     ifIndex numbering: try --if-index-offset)"
                ),
            )
        })
    }

    fn set_int(&mut self, arcs: &[u64], value: i64, what: &str) -> Result<()> {
        let oid = Self::oid(arcs)?;
        let host = self.host.clone();
        let pdu = self
            .sess
            .set(&[(&oid, Value::Integer(value))])
            .map_err(|e| Self::no_reply(&host, what, e))?;
        if pdu.error_status != 0 {
            return Err(err(
                Kind::Failed,
                format!(
                    "the switch refused the SNMP SET for {what} (error status {}): the \
                     community may be read-only",
                    pdu.error_status
                ),
            ));
        }
        Ok(())
    }

    fn row(base: &[u64], last: &[u64]) -> Vec<u64> {
        let mut v = base.to_vec();
        v.extend_from_slice(last);
        v
    }

    fn poe_oid(&self, port: u32) -> Vec<u64> {
        Self::row(
            PETH_PSE_PORT_ADMIN_ENABLE,
            &[self.poe_group, u64::from(port)],
        )
    }
}

impl Backend for Snmp {
    fn check_identity(&mut self) -> Result<String> {
        let oid = Self::oid(SYS_DESCR)?;
        let host = self.host.clone();
        let pdu = self
            .sess
            .get(&oid)
            .map_err(|e| Self::no_reply(&host, "sysDescr", e))?;
        let mut vb = pdu.varbinds;
        match vb.next().map(|(_, v)| v) {
            Some(Value::OctetString(s)) => Ok(String::from_utf8_lossy(s).trim().to_string()),
            other => Err(err(
                Kind::Failed,
                format!("unexpected sysDescr value: {other:?}"),
            )),
        }
    }

    fn port_count(&mut self) -> Result<u32> {
        let n = self.require_int(IF_NUMBER, "ifNumber")?;
        Ok(u32::try_from(n).unwrap_or(0))
    }

    fn port_status(&mut self, port: u32) -> Result<PortStatus> {
        let idx = self.if_index(port)?;
        let admin = self.require_int(
            &Self::row(IF_ADMIN_STATUS, &[idx]),
            &format!("ifAdminStatus.{idx}"),
        )?;
        let oper = self.require_int(
            &Self::row(IF_OPER_STATUS, &[idx]),
            &format!("ifOperStatus.{idx}"),
        )?;
        let link_up = oper == UP;
        let speed = if link_up {
            self.get_int(&Self::row(IF_HIGH_SPEED, &[idx]), "ifHighSpeed")
                .ok()
                .flatten()
                .filter(|m| *m > 0)
                .map(|m| format!("{m}M"))
        } else {
            None
        };
        Ok(PortStatus {
            admin_up: admin == UP,
            link_up,
            speed,
        })
    }

    fn set_admin(&mut self, port: u32, up: bool) -> Result<()> {
        let idx = self.if_index(port)?;
        self.set_int(
            &Self::row(IF_ADMIN_STATUS, &[idx]),
            if up { UP } else { DOWN },
            &format!("ifAdminStatus.{idx}"),
        )
    }

    fn poe_status(&mut self, port: u32) -> Result<bool> {
        let v = self.require_int(&self.poe_oid(port), "pethPsePortAdminEnable")?;
        Ok(v == UP)
    }

    fn set_poe(&mut self, port: u32, on: bool) -> Result<()> {
        self.set_int(
            &self.poe_oid(port),
            if on { UP } else { DOWN },
            "pethPsePortAdminEnable",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{self, kind_of, Opts};
    use std::collections::HashMap;
    use std::net::UdpSocket;
    use std::sync::{Arc, Mutex};
    use std::thread;

    // A small hand-written BER codec, independent of snmp2, so the fake agent
    // does not share a bug with the client under test.
    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        if content.len() < 128 {
            v.push(content.len() as u8);
        } else {
            v.extend_from_slice(&[0x82, (content.len() >> 8) as u8, content.len() as u8]);
        }
        v.extend_from_slice(content);
        v
    }

    fn read_tlv(buf: &[u8]) -> (u8, &[u8], &[u8]) {
        let tag = buf[0];
        let (len, off) = if buf[1] < 128 {
            (buf[1] as usize, 2)
        } else {
            let n = (buf[1] & 0x7f) as usize;
            let len = buf[2..2 + n]
                .iter()
                .fold(0usize, |a, b| (a << 8) | *b as usize);
            (len, 2 + n)
        };
        (tag, &buf[off..off + len], &buf[off + len..])
    }

    fn dec_int(c: &[u8]) -> i64 {
        let mut n: i64 = if c[0] & 0x80 != 0 { -1 } else { 0 };
        for b in c {
            n = (n << 8) | i64::from(*b);
        }
        n
    }

    fn enc_int(tag: u8, n: i64) -> Vec<u8> {
        let mut bytes = n.to_be_bytes().to_vec();
        while bytes.len() > 1
            && ((bytes[0] == 0 && bytes[1] & 0x80 == 0)
                || (bytes[0] == 0xff && bytes[1] & 0x80 != 0))
        {
            bytes.remove(0);
        }
        tlv(tag, &bytes)
    }

    fn dec_oid(c: &[u8]) -> Vec<u64> {
        let mut out = vec![u64::from(c[0] / 40), u64::from(c[0] % 40)];
        let mut acc = 0u64;
        for b in &c[1..] {
            acc = (acc << 7) | u64::from(b & 0x7f);
            if b & 0x80 == 0 {
                out.push(acc);
                acc = 0;
            }
        }
        out
    }

    fn enc_oid(arcs: &[u64]) -> Vec<u8> {
        let mut c = vec![(arcs[0] * 40 + arcs[1]) as u8];
        for a in &arcs[2..] {
            let mut parts = vec![(a & 0x7f) as u8];
            let mut x = a >> 7;
            while x > 0 {
                parts.push((x & 0x7f) as u8 | 0x80);
                x >>= 7;
            }
            parts.reverse();
            c.extend(parts);
        }
        tlv(0x06, &c)
    }

    #[derive(Clone)]
    enum Val {
        Int(i64),
        Str(&'static str),
    }

    struct Agent {
        community: &'static str,
        mib: HashMap<Vec<u64>, Val>,
        plugged: Vec<bool>,
        offset: u64,
        ignore_writes: bool,
        requests: usize,
        sets: usize,
    }

    fn arcs(base: &[u64], tail: &[u64]) -> Vec<u64> {
        let mut v = base.to_vec();
        v.extend_from_slice(tail);
        v
    }

    fn start(offset: u64) -> (Arc<Mutex<Agent>>, String) {
        let plugged = vec![true, false, true, false, false, false, false, false];
        let mut mib = HashMap::new();
        mib.insert(SYS_DESCR.to_vec(), Val::Str("Fake 8-port switch"));
        mib.insert(IF_NUMBER.to_vec(), Val::Int(8));
        for (i, p) in plugged.iter().enumerate() {
            let idx = i as u64 + 1 + offset;
            mib.insert(arcs(IF_ADMIN_STATUS, &[idx]), Val::Int(1));
            mib.insert(
                arcs(IF_OPER_STATUS, &[idx]),
                Val::Int(if *p { 1 } else { 2 }),
            );
            mib.insert(
                arcs(IF_HIGH_SPEED, &[idx]),
                Val::Int(if *p { 1000 } else { 0 }),
            );
            mib.insert(
                arcs(PETH_PSE_PORT_ADMIN_ENABLE, &[1, i as u64 + 1]),
                Val::Int(1),
            );
        }
        let agent = Arc::new(Mutex::new(Agent {
            community: "bench",
            mib,
            plugged,
            offset,
            ignore_writes: false,
            requests: 0,
            sets: 0,
        }));
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap().to_string();
        let shared = Arc::clone(&agent);
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf) else {
                    return;
                };
                if let Some(reply) = answer(&shared, &buf[..n]) {
                    let _ = sock.send_to(&reply, from);
                }
            }
        });
        (agent, addr)
    }

    /// Decode a v2c GET/SET and build the Response; `None` = stay silent
    /// (what a real agent does for a wrong community).
    fn answer(agent: &Arc<Mutex<Agent>>, msg: &[u8]) -> Option<Vec<u8>> {
        let mut a = agent.lock().unwrap();
        let (_, body, _) = read_tlv(msg);
        let (_, ver, rest) = read_tlv(body);
        let (_, comm, rest) = read_tlv(rest);
        a.requests += 1;
        if comm != a.community.as_bytes() {
            return None;
        }
        let (ptag, pdu, _) = read_tlv(rest);
        let (_, reqid, rest) = read_tlv(pdu);
        let (_, _, rest) = read_tlv(rest);
        let (_, _, rest) = read_tlv(rest);
        let (_, mut vbs, _) = read_tlv(rest);
        let mut out = Vec::new();
        while !vbs.is_empty() {
            let (_, vb, next) = read_tlv(vbs);
            vbs = next;
            let (_, oid, val) = read_tlv(vb);
            let oid = dec_oid(oid);
            if ptag == 0xA3 {
                a.sets += 1;
                let (_, v, _) = read_tlv(val);
                let n = dec_int(v);
                if !a.ignore_writes {
                    a.mib.insert(oid.clone(), Val::Int(n));
                    if oid.starts_with(IF_ADMIN_STATUS) {
                        let idx = *oid.last().unwrap();
                        let plugged = a.plugged[(idx - 1 - a.offset) as usize];
                        let oper = if n == 1 && plugged { 1 } else { 2 };
                        a.mib.insert(arcs(IF_OPER_STATUS, &[idx]), Val::Int(oper));
                    }
                }
            }
            let value = match a.mib.get(&oid) {
                Some(Val::Int(n)) => enc_int(0x02, *n),
                Some(Val::Str(s)) => tlv(0x04, s.as_bytes()),
                None => vec![0x81, 0],
            };
            let mut inner = enc_oid(&oid);
            inner.extend(value);
            out.extend(tlv(0x30, &inner));
        }
        let mut p = tlv(0x02, reqid);
        p.extend(enc_int(0x02, 0));
        p.extend(enc_int(0x02, 0));
        p.extend(tlv(0x30, &out));
        let mut m = tlv(0x02, ver);
        m.extend(tlv(0x04, comm));
        m.extend(tlv(0xA2, &p));
        Some(tlv(0x30, &m))
    }

    fn client(addr: &str, community: &str, offset: i64) -> Snmp {
        Snmp::new(addr, community, offset, 1, Duration::from_millis(300)).unwrap()
    }

    fn opts(mgmt: Option<u32>, wait_ms: u64) -> Opts {
        Opts {
            mgmt_port: mgmt,
            wait: Duration::from_millis(wait_ms),
            poll: Duration::from_millis(10),
        }
    }

    #[test]
    fn status_reads_admin_oper_and_speed() {
        let (_a, addr) = start(0);
        let mut be = client(&addr, "bench", 0);
        let st = ops::port_state(&mut be, 3).unwrap();
        assert!(st.admin_up && st.link_up);
        assert_eq!(st.speed.as_deref(), Some("1000M"));
        let st = ops::port_state(&mut be, 2).unwrap();
        assert!(st.admin_up && !st.link_up && st.speed.is_none());
    }

    #[test]
    fn off_then_on_with_read_back_and_link_wait() {
        let (agent, addr) = start(0);
        let mut be = client(&addr, "bench", 0);
        let st = ops::port_set(&mut be, &opts(Some(1), 1000), 3, false).unwrap();
        assert!(!st.admin_up && !st.link_up);
        assert_eq!(
            agent.lock().unwrap().mib[&arcs(IF_ADMIN_STATUS, &[3])]
                .clone()
                .int(),
            2
        );
        let st = ops::port_set(&mut be, &opts(Some(1), 1000), 3, true).unwrap();
        assert!(st.admin_up && st.link_up);
    }

    #[test]
    fn link_wait_is_opt_in_and_times_out() {
        let (_a, addr) = start(0);
        let mut be = client(&addr, "bench", 0);
        // Port 2 has no peer: no wait by default, a distinct error with one.
        let st = ops::port_set(&mut be, &opts(Some(1), 0), 2, true).unwrap();
        assert!(st.admin_up && !st.link_up);
        let e = ops::port_set(&mut be, &opts(Some(1), 100), 2, true).unwrap_err();
        assert_eq!(kind_of(&e), Kind::LinkTimeout);
    }

    #[test]
    fn mgmt_port_off_is_refused_without_any_request() {
        let (agent, addr) = start(0);
        let mut be = client(&addr, "bench", 0);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 1, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Usage);
        let e = ops::port_set(&mut be, &opts(None, 0), 3, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::NotConfigured);
        let e = ops::poe_set(&mut be, &opts(Some(1), 0), 1, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Usage);
        assert_eq!(agent.lock().unwrap().requests, 0);
    }

    #[test]
    fn wrong_community_is_an_error_and_writes_nothing() {
        let (agent, addr) = start(0);
        let mut be = client(&addr, "public", 0);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Unreachable);
        assert!(format!("{e}").contains("community"), "{e}");
        assert_eq!(agent.lock().unwrap().sets, 0);
    }

    #[test]
    fn ignored_write_fails_read_back() {
        let (agent, addr) = start(0);
        agent.lock().unwrap().ignore_writes = true;
        let mut be = client(&addr, "bench", 0);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Failed);
        assert!(format!("{e}").contains("read-back"), "{e}");
    }

    #[test]
    fn poe_set_and_read_back() {
        let (agent, addr) = start(0);
        let mut be = client(&addr, "bench", 0);
        assert!(ops::poe_state(&mut be, 3).unwrap());
        ops::poe_set(&mut be, &opts(Some(1), 0), 3, false).unwrap();
        assert!(!ops::poe_state(&mut be, 3).unwrap());
        agent.lock().unwrap().ignore_writes = true;
        let e = ops::poe_set(&mut be, &opts(Some(1), 0), 3, true).unwrap_err();
        assert!(format!("{e}").contains("read-back"), "{e}");
    }

    #[test]
    fn if_index_offset_maps_ports() {
        let (agent, addr) = start(100);
        let mut be = client(&addr, "bench", 100);
        ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap();
        assert_eq!(
            agent.lock().unwrap().mib[&arcs(IF_ADMIN_STATUS, &[103])]
                .clone()
                .int(),
            2
        );
        // Without the offset the ifIndex does not exist: a clear error.
        let mut plain = client(&addr, "bench", 0);
        let e = ops::port_state(&mut plain, 3).unwrap_err();
        assert!(format!("{e}").contains("--if-index-offset"), "{e}");
    }

    impl Val {
        fn int(self) -> i64 {
            match self {
                Val::Int(n) => n,
                Val::Str(_) => panic!("not an int"),
            }
        }
    }
}
