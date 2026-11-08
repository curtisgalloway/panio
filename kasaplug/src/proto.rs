// Copyright 2026 The Panio Authors
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
// limitations under the License.

//! TP-Link Kasa legacy local protocol client.
//!
//! Kasa plugs and power strips answer JSON commands on TCP port 9999. Each
//! message, in both directions, is a 4-byte big-endian length followed by the
//! JSON text run through an XOR autokey cipher: the key starts at 171, and
//! each cipher byte becomes the key for the next. It is obfuscation, not
//! security; there is no login. One connection carries one request, which
//! keeps every call a single stateless exchange, exactly what the one-shot
//! power-hook contract wants.
//!
//! Auth: newer firmware on some models closes this API and speaks only KLAP,
//! an authenticated HTTP protocol this helper does not implement. Such a
//! device refuses TCP 9999; [`Client::call`] is where it surfaces.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// The legacy protocol's TCP port.
pub const PORT: u16 = 9999;

/// Deadline for one whole exchange: connect, send, and the complete reply.
/// It is one budget, not a per-read timeout, so a peer that drips its reply
/// a byte at a time cannot hold a hook open past it. Generous enough for a
/// sleepy Wi-Fi device to answer, short enough that a wrong/dead address
/// fails a hook promptly and a doctor check stays inside panio's 10 s.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound on a reply's length prefix. A six-outlet strip's sysinfo is
/// about 1.3 KB; anything near this is a corrupt prefix, not a reply.
const MAX_REPLY: usize = 64 * 1024;

/// Longest device-supplied string kept, in characters; a cut is marked `…`.
/// The bound panio's own doctor applies to a helper's report.
const MAX_FIELD_CHARS: usize = 300;

/// A device-supplied string made safe to print: control characters become
/// spaces and the length is capped, as `printable` in cli/src/doctor.rs
/// does. The API has no login, so anyone on the network can set an alias;
/// unfiltered, a peer could plant escape sequences or forge output lines.
fn printable(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_FIELD_CHARS)
        .collect();
    if s.chars().count() > MAX_FIELD_CHARS {
        out.push('…');
    }
    out.trim().to_string()
}

/// Apply the XOR autokey cipher (key 171, each output byte keys the next).
pub fn encrypt(plain: &[u8]) -> Vec<u8> {
    let mut key = 171u8;
    plain
        .iter()
        .map(|&b| {
            key ^= b;
            key
        })
        .collect()
}

/// Undo [`encrypt`]: each cipher byte keys the next.
pub fn decrypt(cipher: &[u8]) -> Vec<u8> {
    let mut key = 171u8;
    cipher
        .iter()
        .map(|&c| {
            let p = key ^ c;
            key = c;
            p
        })
        .collect()
}

/// Frame a request: 4-byte big-endian length, then the ciphered JSON.
pub fn frame(req: &Value) -> Vec<u8> {
    let body = encrypt(req.to_string().as_bytes());
    let mut msg = (body.len() as u32).to_be_bytes().to_vec();
    msg.extend(body);
    msg
}

/// Split a `-d` value into host and port. Accepts `192.0.2.10`,
/// `plug.local`, `192.0.2.10:9999`, `2001:db8::1` and `[2001:db8::1]:9999`;
/// the port defaults to 9999.
pub fn split_device(device: &str) -> (String, u16) {
    let d = device.trim();
    if let Ok(sa) = d.parse::<SocketAddr>() {
        return (sa.ip().to_string(), sa.port());
    }
    let bare = d.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return (ip.to_string(), PORT);
    }
    if let Some((host, port)) = d.rsplit_once(':') {
        if let Ok(port) = port.parse() {
            return (host.to_string(), port);
        }
    }
    (d.to_string(), PORT)
}

/// A relay state as the device reports it: 1 is on, 0 is off. Anything else
/// is an error carrying the raw value, never a guessed `off`.
pub fn relay_on(raw: &Value) -> Result<bool> {
    match raw.as_i64() {
        Some(1) => Ok(true),
        Some(0) => Ok(false),
        _ => bail!("unrecognized relay state {raw} (expected 0 or 1)"),
    }
}

/// One switchable outlet: a strip's child, or a plug's own relay.
#[derive(Debug, Clone)]
pub struct Outlet {
    /// The child id to name in a request's `context`; `None` for a plug's
    /// own relay. Kept verbatim, since it goes back to the device; it is
    /// never printed.
    pub child_id: Option<String>,
    pub alias: String,
    /// The relay state exactly as reported; interpret with [`relay_on`].
    pub state: Value,
}

/// Subset of `system.get_sysinfo` we surface. Every string field is
/// [`printable`].
#[derive(Debug, Clone)]
pub struct SysInfo {
    pub device_id: String,
    pub model: String,
    pub hw_ver: String,
    pub sw_ver: String,
    pub alias: String,
    /// In the device's own order: a strip's `children`, or a plug's single
    /// relay. Index `i` is outlet id `i`.
    pub outlets: Vec<Outlet>,
}

impl SysInfo {
    fn parse(v: &Value) -> Result<SysInfo> {
        let s = |k: &str| printable(v.get(k).and_then(Value::as_str).unwrap_or("?"));
        let outlets = if let Some(children) = v.get("children").and_then(Value::as_array) {
            children
                .iter()
                .map(|c| {
                    let id = c
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("a child in get_sysinfo has no id: {c}"))?;
                    Ok(Outlet {
                        child_id: Some(id.to_string()),
                        alias: printable(c.get("alias").and_then(Value::as_str).unwrap_or("")),
                        state: c.get("state").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else if let Some(state) = v.get("relay_state") {
            vec![Outlet {
                child_id: None,
                alias: s("alias"),
                state: state.clone(),
            }]
        } else {
            Vec::new()
        };
        Ok(SysInfo {
            device_id: s("deviceId"),
            model: s("model"),
            hw_ver: s("hw_ver"),
            sw_ver: s("sw_ver"),
            alias: s("alias"),
            outlets,
        })
    }

    /// The outlet `id` names, or an error listing the ids that exist.
    pub fn outlet(&self, id: u32) -> Result<&Outlet> {
        let n = self.outlets.len();
        match self.outlets.get(id as usize) {
            Some(o) => Ok(o),
            None if n == 0 => bail!("outlet {id}: the device has no switchable outlet"),
            None => bail!(
                "outlet {id} not found: the device has {n} outlet(s), ids 0..{}",
                n - 1
            ),
        }
    }
}

/// Subset of `emeter.get_realtime` we surface, in SI units. Firmware reports
/// either milli-units (`power_mw`, `total_wh`, ...) or plain units (`power`,
/// `total` in kWh, ...).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Realtime {
    pub watts: Option<f64>,
    pub volts: Option<f64>,
    pub amps: Option<f64>,
    pub watt_hours: Option<f64>,
}

impl Realtime {
    fn parse(v: &Value) -> Realtime {
        let plain = |k: &str| v.get(k).and_then(Value::as_f64);
        let milli = |k: &str| plain(k).map(|x| x / 1000.0);
        Realtime {
            watts: milli("power_mw").or_else(|| plain("power")),
            volts: milli("voltage_mv").or_else(|| plain("voltage")),
            amps: milli("current_ma").or_else(|| plain("current")),
            watt_hours: plain("total_wh").or_else(|| plain("total").map(|kwh| kwh * 1000.0)),
        }
    }
}

/// A Kasa device addressed over its legacy local protocol.
pub struct Client {
    host: String,
    port: u16,
    /// Deadline per exchange; [`TIMEOUT`] outside tests.
    timeout: Duration,
}

impl Client {
    /// Build a client from a user-supplied address (see [`split_device`]).
    pub fn new(device: &str) -> Self {
        let (host, port) = split_device(device);
        Client {
            host,
            port,
            timeout: TIMEOUT,
        }
    }

    /// The same client with another deadline, so a test of it runs in
    /// milliseconds.
    #[cfg(test)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `host:port`, for messages.
    pub fn addr(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Open a TCP connection within one deadline, as `doctor`'s first probe.
    /// The `io::Error` stays in the chain, so `doctor` can tell a refused
    /// port (the host is there, the API is not) from a host that does not
    /// answer.
    pub fn connect(&self) -> Result<TcpStream> {
        self.connect_by(Instant::now() + self.timeout)
    }

    /// [`Client::connect`] within an exchange's deadline: every address
    /// tried shares it.
    fn connect_by(&self, deadline: Instant) -> Result<TcpStream> {
        let addrs: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve {}", self.host))?
            .collect();
        let mut last = None;
        for a in &addrs {
            match time_left(deadline).and_then(|left| TcpStream::connect_timeout(a, left)) {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        match last {
            Some(e) => Err(anyhow::Error::new(e)
                .context(format!("cannot reach Kasa device at {}", self.addr()))),
            None => Err(anyhow!("{} resolved to no address", self.host)),
        }
    }

    /// The one place every request is issued. Returns the decoded reply,
    /// mapping transport failures to clear messages. One deadline, set here,
    /// bounds the whole exchange.
    fn call(&self, req: &Value) -> Result<Value> {
        let deadline = Instant::now() + self.timeout;
        let mut s = self.connect_by(deadline)?;
        // A request is under 200 bytes and fits the socket's send buffer in
        // one write, so a single timeout bounds it.
        time_left(deadline)
            .and_then(|left| s.set_write_timeout(Some(left)))
            .and_then(|()| s.write_all(&frame(req)))
            .with_context(|| format!("sending to {}", self.addr()))?;
        let mut len = [0u8; 4];
        read_exact_by(&mut s, &mut len, deadline)
            .with_context(|| format!("no legacy reply from {}", self.addr()))?;
        let n = u32::from_be_bytes(len) as usize;
        if n > MAX_REPLY {
            bail!(
                "{} sent a reply length of {n} bytes; not a Kasa reply",
                self.addr()
            );
        }
        let mut body = vec![0u8; n];
        read_exact_by(&mut s, &mut body, deadline)
            .with_context(|| format!("reading a {n}-byte reply from {}", self.addr()))?;
        serde_json::from_slice(&decrypt(&body))
            .with_context(|| format!("decoding the reply from {}", self.addr()))
    }

    /// `system.get_sysinfo` — identity, firmware, every outlet's state.
    pub fn sysinfo(&self) -> Result<SysInfo> {
        let v = self.call(&json!({"system": {"get_sysinfo": {}}}))?;
        SysInfo::parse(&result(&v, "system", "get_sysinfo")?).context("parsing get_sysinfo")
    }

    /// `system.set_relay_state` — command one outlet on or off. The caller
    /// confirms by re-reading [`Client::sysinfo`] (the contract wants
    /// read-back).
    pub fn set_relay(&self, outlet: &Outlet, on: bool) -> Result<()> {
        let req = with_context(
            json!({"system": {"set_relay_state": {"state": u8::from(on)}}}),
            outlet,
        );
        result(&self.call(&req)?, "system", "set_relay_state").map(|_| ())
    }

    /// `emeter.get_realtime` for one outlet; `Ok(None)` when the device has
    /// no meter.
    pub fn realtime(&self, outlet: &Outlet) -> Result<Option<Realtime>> {
        let req = with_context(json!({"emeter": {"get_realtime": {}}}), outlet);
        parse_realtime(&self.call(&req)?)
    }
}

/// The module-level `err_code` a device returns for a module it does not
/// have: `{"emeter": {"err_code": -1, "err_msg": "module not support"}}`.
/// Seen on an HS300 for a module it lacks; an unknown method is -2 and an
/// unknown outlet -14, both inside the method's object.
const NO_SUCH_MODULE: i64 = -1;

/// An `emeter.get_realtime` reply. `Ok(None)` only when the device says it
/// has no meter module; any other error is an error, so a metered device
/// that fails a read is not mistaken for one without a meter.
fn parse_realtime(v: &Value) -> Result<Option<Realtime>> {
    if module_code(v, "emeter") == Some(NO_SUCH_MODULE) {
        return Ok(None);
    }
    Ok(Some(Realtime::parse(&result(v, "emeter", "get_realtime")?)))
}

/// Time left before `deadline`; a `TimedOut` error once it has passed.
/// Never zero, which a socket timeout rejects.
fn time_left(deadline: Instant) -> io::Result<Duration> {
    match deadline.checked_duration_since(Instant::now()) {
        Some(left) if !left.is_zero() => Ok(left),
        _ => Err(timed_out()),
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out")
}

/// `read_exact` against a deadline: the socket timeout is reset to the time
/// left before every read, so the whole read is bounded, not each byte.
fn read_exact_by(s: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        s.set_read_timeout(Some(time_left(deadline)?))?;
        match s.read(&mut buf[got..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) => match e.kind() {
                io::ErrorKind::Interrupted => {}
                // A read timeout is WouldBlock on Unix, TimedOut on Windows.
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => return Err(timed_out()),
                _ => return Err(e),
            },
        }
    }
    Ok(())
}

/// Address a request at one outlet of a strip; a plug's own relay needs no
/// context.
fn with_context(mut req: Value, outlet: &Outlet) -> Value {
    if let Some(id) = &outlet.child_id {
        req["context"] = json!({ "child_ids": [id] });
    }
    req
}

/// A module-level `err_code`: one directly in the module's object, not in a
/// method's.
fn module_code(v: &Value, module: &str) -> Option<i64> {
    v.get(module)?.get("err_code")?.as_i64()
}

/// A module-level error, such as [`NO_SUCH_MODULE`], as a message.
fn module_error(v: &Value, module: &str) -> Option<String> {
    let m = v.get(module)?;
    let code = module_code(v, module)?;
    (code != 0).then(|| {
        let msg = printable(m.get("err_msg").and_then(Value::as_str).unwrap_or(""));
        format!("{module}: device error {code} {msg}")
            .trim_end()
            .to_string()
    })
}

/// The `module.method` object of a reply, failing on a non-zero `err_code`.
fn result(v: &Value, module: &str, method: &str) -> Result<Value> {
    if let Some(e) = module_error(v, module) {
        bail!("{e}");
    }
    let r = v
        .get(module)
        .and_then(|m| m.get(method))
        .ok_or_else(|| anyhow!("{module}.{method}: unexpected reply {v}"))?;
    match r.get("err_code").and_then(Value::as_i64) {
        None | Some(0) => Ok(r.clone()),
        Some(code) => {
            let msg = printable(r.get("err_msg").and_then(Value::as_str).unwrap_or(""));
            bail!("{module}.{method}: device error {code} {msg}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn cipher_matches_a_known_good_request() {
        // `{"system":{"get_sysinfo":{}}}` as a known-good client framed it
        // when talking to a real strip.
        let req = json!({"system": {"get_sysinfo": {}}});
        assert_eq!(
            frame(&req),
            [
                0x00, 0x00, 0x00, 0x1d, 0xd0, 0xf2, 0x81, 0xf8, 0x8b, 0xff, 0x9a, 0xf7, 0xd5, 0xef,
                0x94, 0xb6, 0xd1, 0xb4, 0xc0, 0x9f, 0xec, 0x95, 0xe6, 0x8f, 0xe1, 0x87, 0xe8, 0xca,
                0xf0, 0x8b, 0xf6, 0x8b, 0xf6,
            ]
        );
        let text = br#"{"system":{"set_relay_state":{"state":1}}}"#;
        assert_eq!(decrypt(&encrypt(text)), text);
    }

    #[test]
    fn device_address_normalization() {
        assert_eq!(split_device("192.0.2.10"), ("192.0.2.10".into(), 9999));
        assert_eq!(
            split_device(" 192.0.2.10:9998 "),
            ("192.0.2.10".into(), 9998)
        );
        assert_eq!(split_device("plug.local"), ("plug.local".into(), 9999));
        assert_eq!(split_device("plug.local:80"), ("plug.local".into(), 80));
        assert_eq!(split_device("2001:db8::1"), ("2001:db8::1".into(), 9999));
        assert_eq!(split_device("[2001:db8::1]"), ("2001:db8::1".into(), 9999));
        assert_eq!(
            split_device("[2001:db8::1]:9998"),
            ("2001:db8::1".into(), 9998)
        );
    }

    #[test]
    fn relay_states() {
        assert!(relay_on(&json!(1)).unwrap());
        assert!(!relay_on(&json!(0)).unwrap());
        let err = relay_on(&json!(2)).unwrap_err().to_string();
        assert!(err.contains("unrecognized relay state 2"), "{err}");
        assert!(relay_on(&json!("on")).is_err());
        assert!(relay_on(&Value::Null).is_err());
    }

    #[test]
    fn sysinfo_of_a_strip_and_a_plug() {
        let strip = SysInfo::parse(&json!({
            "model": "HS300(US)", "hw_ver": "2.0", "sw_ver": "1.1.2", "alias": "strip",
            "deviceId": "AA00BB11CC22DD33",
            "children": [
                {"id": "AA00BB11CC22DD3300", "state": 1, "alias": "Plug 1"},
                {"id": "AA00BB11CC22DD3301", "state": 0, "alias": "Plug 2"}
            ]
        }))
        .unwrap();
        assert_eq!(strip.outlets.len(), 2);
        assert_eq!(
            strip.outlet(1).unwrap().child_id.as_deref(),
            Some("AA00BB11CC22DD3301")
        );
        let err = strip.outlet(2).unwrap_err().to_string();
        assert!(err.contains("has 2 outlet(s), ids 0..1"), "{err}");

        let plug = SysInfo::parse(&json!({
            "model": "HS103(US)", "alias": "lamp", "relay_state": 0
        }))
        .unwrap();
        assert!(plug.outlet(0).unwrap().child_id.is_none());
        assert_eq!(plug.outlet(0).unwrap().alias, "lamp");
        assert_eq!(plug.hw_ver, "?");

        let bulb = SysInfo::parse(&json!({"model": "KL110(US)"})).unwrap();
        assert!(bulb.outlets.is_empty());
        assert!(bulb.outlet(0).is_err());
    }

    #[test]
    fn device_strings_are_made_printable() {
        let clean = |s: &str| !s.chars().any(char::is_control);
        // As the wire carries them: JSON escapes, which serde decodes into
        // the control characters themselves.
        let info = SysInfo::parse(
            &serde_json::from_str(
                r#"{"model": "HS300(US)\u001b[2J", "alias": "strip\nkasaplug: outlet 0: off",
                    "deviceId": "AA00\u009b31m", "sw_ver": "1.1.2\r",
                    "children": [{"id": "AA00BB11CC22DD3300\u0007", "state": 1,
                                  "alias": "Plug 1\u001b]0;title\u0007"}]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(info.model, "HS300(US) [2J");
        assert_eq!(info.alias, "strip kasaplug: outlet 0: off");
        assert_eq!(info.device_id, "AA00 31m");
        assert_eq!(info.sw_ver, "1.1.2");
        assert_eq!(info.outlets[0].alias, "Plug 1 ]0;title");
        // The id goes back to the device as it came.
        assert_eq!(
            info.outlets[0].child_id.as_deref(),
            Some("AA00BB11CC22DD3300\u{7}")
        );

        let long = SysInfo::parse(&json!({"alias": "x".repeat(400)})).unwrap();
        assert_eq!(long.alias.chars().count(), MAX_FIELD_CHARS + 1);
        assert!(long.alias.ends_with('…'), "{}", long.alias);

        let v = json!({"emeter": {"err_code": -3, "err_msg": "no\u{1b}[2Jpe"}});
        let err = module_error(&v, "emeter").unwrap();
        assert!(clean(&err), "{err:?}");
        let v = json!({"system": {"get_sysinfo": {"err_code": -3, "err_msg": "a\nb"}}});
        let err = result(&v, "system", "get_sysinfo").unwrap_err().to_string();
        assert!(clean(&err), "{err:?}");
    }

    #[test]
    fn realtime_in_either_unit() {
        let milli = Realtime::parse(&json!({
            "power_mw": 3095, "voltage_mv": 120716, "current_ma": 50, "total_wh": 12
        }));
        assert_eq!(milli.watts, Some(3.095));
        assert_eq!(milli.volts, Some(120.716));
        assert_eq!(milli.amps, Some(0.05));
        assert_eq!(milli.watt_hours, Some(12.0));
        let plain = Realtime::parse(&json!({"power": 3.1, "voltage": 120.7, "total": 0.5}));
        assert_eq!(plain.watts, Some(3.1));
        assert_eq!(plain.amps, None);
        assert_eq!(plain.watt_hours, Some(500.0));
    }

    #[test]
    fn a_reply_dripped_byte_by_byte_ends_at_the_deadline() {
        // Promises a 64-byte reply, then sends a byte every 50 ms. Each read
        // returns well inside the deadline, so only a deadline over the
        // whole exchange ends it before the 3.2 s the drip takes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                thread::spawn(move || {
                    let _ = s.write_all(&64u32.to_be_bytes());
                    for _ in 0..64 {
                        thread::sleep(Duration::from_millis(50));
                        if s.write_all(&[0]).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let client = Client::new(&addr).with_timeout(Duration::from_millis(500));
        let start = Instant::now();
        let err = client.sysinfo().unwrap_err();
        let took = start.elapsed();
        assert!(took < Duration::from_secs(2), "took {took:?}: {err:#}");
        let err = format!("{err:#}");
        assert!(err.contains("reading a 64-byte reply"), "{err}");
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn only_a_missing_meter_module_reads_as_no_meter() {
        // Replies an HS300 gave, and one module-level code it has not.
        let absent = json!({"emeter": {"err_code": -1, "err_msg": "module not support"}});
        assert_eq!(parse_realtime(&absent).unwrap(), None);
        let other = json!({"emeter": {"err_code": -3, "err_msg": "other"}});
        let err = parse_realtime(&other).unwrap_err().to_string();
        assert!(err.contains("emeter: device error -3 other"), "{err}");
        let outlet =
            json!({"emeter": {"get_realtime": {"err_code": -14, "err_msg": "entry not exist"}}});
        let err = parse_realtime(&outlet).unwrap_err().to_string();
        assert!(err.contains("device error -14 entry not exist"), "{err}");
        let read = json!({"emeter": {"get_realtime": {
            "slot_id": 4, "current_ma": 0, "voltage_mv": 121818, "power_mw": 0,
            "total_wh": 43, "err_code": 0
        }}});
        let rt = parse_realtime(&read).unwrap().unwrap();
        assert_eq!((rt.watts, rt.watt_hours), (Some(0.0), Some(43.0)));
    }

    #[test]
    fn device_errors_surface() {
        let v = json!({"system": {"set_relay_state": {"err_code": -2, "err_msg": "member not support"}}});
        let err = result(&v, "system", "set_relay_state")
            .unwrap_err()
            .to_string();
        assert!(err.contains("device error -2 member not support"), "{err}");
        let v = json!({"emeter": {"err_code": -1, "err_msg": "module not support"}});
        assert!(module_error(&v, "emeter").is_some());
        assert!(result(&v, "emeter", "get_realtime").is_err());
    }
}
