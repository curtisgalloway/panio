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

//! TP-Link Easy Smart backend (plain HTTP web UI).
//!
//! Protocol notes (measured on a TL-SG108PE hardware 3.0, firmware 1.0.0
//! Build 20200415; see AGENTS.md): no cookie, the session is bound to the
//! client IP; `POST /logon.cgi` logs in; pages embed JavaScript object
//! literals (`var all_info = {state:[…], …};`) that this module parses.

use std::time::Duration;

use anyhow::Result;

use crate::ops::{err, Backend, Kind, PortStatus};

/// Hardware/firmware pairs the helper will drive. TP-Link changes these
/// pages between hardware versions, so unknown pairs are refused.
/// `(hardwareStr contains, firmwareStr starts with)`.
const ALLOWED: &[(&str, &str)] = &[
    ("TL-SG108PE 3.0", "1.0.0 Build 20200415"),
    ("TL-SG205E 1.0", "1.0.1 Build 20250609"),
];

/// Logins one process may attempt: the first, plus one re-login after an
/// expired session, plus a spare for a session lost between two pages.
const MAX_LOGINS: u32 = 3;

/// The 2025 firmware silently rejects a login POST that does not look like
/// it came from a browser; which of Origin, Referer and User-Agent it checks
/// is unknown, so all three are sent on every login.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

/// Which web UI the switch speaks, learned from the login reply.
#[derive(Debug, Clone, PartialEq)]
enum Ui {
    /// 2020 firmware: `*Rpm.htm` pages, session bound to the client IP.
    Old,
    /// 2025 firmware: `sys_*.html` pages and `portCur.xml`, session is the
    /// `Cookies=<token>` cookie (stored here as `Cookies=<token>`).
    New(String),
}

pub struct EasySmart {
    base: String,
    agent: ureq::Agent,
    user: String,
    password: String,
    allow_unknown: bool,
    logins: u32,
    ui: Option<Ui>,
    /// True from a login until the next page that is not the login page.
    fresh: bool,
    identity: Option<String>,
}

impl EasySmart {
    pub fn new(device: &str, user: &str, password: &str, allow_unknown: bool) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build();
        EasySmart {
            base: base_url(device),
            agent,
            user: user.to_string(),
            password: password.to_string(),
            allow_unknown,
            logins: 0,
            ui: None,
            fresh: false,
            identity: None,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn map_transport(&self, e: ureq::Error) -> anyhow::Error {
        match e {
            ureq::Error::Status(code, _) => err(
                Kind::Failed,
                format!("switch answered HTTP {code} at {}", self.base),
            ),
            ureq::Error::Transport(t) => err(
                Kind::Unreachable,
                format!("cannot reach the switch at {}: {t}", self.base),
            ),
        }
    }

    fn http_get(&self, path: &str) -> Result<String> {
        let mut req = self.agent.get(&self.url(path));
        if let Some(Ui::New(cookie)) = &self.ui {
            req = req
                .set("Cookie", cookie)
                .set("Referer", &format!("{}/homepage.html", self.base))
                .set("User-Agent", USER_AGENT);
        }
        let resp = req.call().map_err(|e| self.map_transport(e))?;
        resp.into_string()
            .map_err(|e| err(Kind::Unreachable, format!("reading {path}: {e}")))
    }

    /// Log in and learn which UI the switch speaks. A bad password is
    /// reported once and never retried: the switch counts failures.
    fn login(&mut self) -> Result<()> {
        if self.logins >= MAX_LOGINS {
            return Err(err(
                Kind::Failed,
                "the switch keeps dropping the session; giving up after repeated logins",
            ));
        }
        self.logins += 1;
        let resp = self
            .agent
            .post(&self.url("/logon.cgi"))
            .set("Origin", &self.base)
            .set("Referer", &format!("{}/index.html", self.base))
            .set("User-Agent", USER_AGENT)
            .send_form(&[
                ("username", self.user.as_str()),
                ("password", self.password.as_str()),
                ("cpassword", ""),
                ("logon", "Login"),
            ])
            .map_err(|e| self.map_transport(e))?;
        let set_cookie = resp.header("Set-Cookie").map(str::to_string);
        let body = resp
            .into_string()
            .map_err(|e| err(Kind::Unreachable, format!("reading login reply: {e}")))?;
        if body.contains("logonInfo") {
            return match logon_code(&body) {
                Some(0) => {
                    self.ui = Some(Ui::Old);
                    self.fresh = true;
                    Ok(())
                }
                Some(1) => Err(rejected(&self.user)),
                Some(n) => Err(err(
                    Kind::Failed,
                    format!("the switch refused the login with error code {n} (not retrying)"),
                )),
                None => Err(no_login_info()),
            };
        }
        match allow_code(&body) {
            Some(1) => {
                let cookie = set_cookie
                    .as_deref()
                    .and_then(|c| c.split(';').next())
                    .map(str::trim)
                    .filter(|c| c.starts_with("Cookies="));
                let Some(cookie) = cookie else {
                    return Err(err(
                        Kind::Failed,
                        "the switch accepted the login but set no Cookies= session cookie",
                    ));
                };
                self.ui = Some(Ui::New(cookie.to_string()));
                self.fresh = true;
                Ok(())
            }
            Some(0) => Err(rejected(&self.user)),
            Some(2) => Err(err(
                Kind::Failed,
                "the switch demands a first-login password change before it can be driven; \
                 finish the setup in a web browser, then retry (not retrying)",
            )),
            Some(n) => Err(err(
                Kind::Failed,
                format!("the switch refused the login with code {n} (not retrying)"),
            )),
            None => Err(no_login_info()),
        }
    }

    fn is_new(&self) -> bool {
        matches!(self.ui, Some(Ui::New(_)))
    }

    /// GET a protected page; `old` and `new` are its path on each UI. The UI
    /// is unknown until a login, and the old pages answer the new UI with an
    /// empty reply, so the first fetch logs in. The login page (or the new
    /// UI's logged-out stub) coming back later means the session expired: log
    /// in and ask again, once.
    fn fetch(&mut self, old: &str, new: &str, marker: Option<&str>) -> Result<String> {
        if self.ui.is_none() {
            self.login()?;
        }
        let path = if self.is_new() { new } else { old }.to_string();
        let mut body = self.http_get(&path)?;
        if is_login_page(&body) {
            if self.fresh {
                return Err(err(
                    Kind::Failed,
                    format!("the switch returned its login page for {path} right after a successful login"),
                ));
            }
            self.login()?;
            let path = if self.is_new() { new } else { old };
            body = self.http_get(path)?;
            if is_login_page(&body) {
                return Err(err(
                    Kind::Failed,
                    format!("the switch returned its login page for {path} right after a successful login"),
                ));
            }
        }
        self.fresh = false;
        if let Some(m) = marker {
            if !body.contains(m) {
                return Err(err(
                    Kind::Failed,
                    format!("{path}: reply did not contain {m}; unexpected page"),
                ));
            }
        }
        Ok(body)
    }

    fn ports(&mut self) -> Result<PortTable> {
        let body = self.fetch(
            "/PortSettingRpm.htm",
            "/sys_portsetting.html",
            Some("all_info"),
        )?;
        let mut t = parse_ports(&body)?;
        if self.is_new() {
            let cur = self.fetch("/portCur.xml", "/portCur.xml", None)?;
            t.spd_act = parse_port_cur(&cur, t.count)?;
        }
        Ok(t)
    }

    fn index(&mut self, port: u32) -> Result<(PortTable, usize)> {
        let t = self.ports()?;
        if port < 1 || port as usize > t.count {
            return Err(err(
                Kind::Usage,
                format!(
                    "port {port} is out of range: the switch has ports 1-{}",
                    t.count
                ),
            ));
        }
        Ok((t, port as usize - 1))
    }
}

fn rejected(user: &str) -> anyhow::Error {
    err(
        Kind::Failed,
        format!(
            "the switch rejected the username or password for {user:?} (not retrying: \
             repeated failed logins can lock the web UI out)"
        ),
    )
}

fn no_login_info() -> anyhow::Error {
    err(
        Kind::Failed,
        "the switch's login reply had neither logonInfo nor allow; is this an Easy Smart switch?",
    )
}

impl Backend for EasySmart {
    fn check_identity(&mut self) -> Result<String> {
        if let Some(i) = &self.identity {
            return Ok(i.clone());
        }
        let body = self.fetch("/SystemInfoRpm.htm", "/sys_SystemInfoRpm.html", None)?;
        let info = parse_system_info(&body)?;
        let desc = format!(
            "{} (hardware {}, firmware {})",
            info.description, info.hardware, info.firmware
        );
        if !identity_allowed(&info.hardware, &info.firmware) {
            if !self.allow_unknown {
                return Err(err(
                    Kind::Failed,
                    format!(
                        "refusing to drive an unrecognized switch: {desc}. Easy Smart pages \
                         differ between hardware versions; pass --allow-unknown-model to try \
                         anyway"
                    ),
                ));
            }
            eprintln!("warning: unrecognized switch {desc}; continuing (--allow-unknown-model)");
        }
        self.identity = Some(desc.clone());
        Ok(desc)
    }

    fn port_count(&mut self) -> Result<u32> {
        Ok(self.ports()?.count as u32)
    }

    fn port_status(&mut self, port: u32) -> Result<PortStatus> {
        let (t, i) = self.index(port)?;
        let act = t.spd_act[i];
        Ok(PortStatus {
            admin_up: t.state[i] != 0,
            link_up: act != 0,
            speed: speed_name(act),
        })
    }

    fn set_admin(&mut self, port: u32, up: bool) -> Result<()> {
        self.index(port)?;
        let old = format!(
            "/port_setting.cgi?portid={port}&state={}&speed=7&flowcontrol=7&apply=Apply",
            u8::from(up)
        );
        let new = format!(
            "/port_setting.cgi?port_bit={}&state={}",
            1u32 << (port - 1),
            u8::from(up)
        );
        self.fetch(&old, &new, None)?;
        Ok(())
    }

    fn poe_status(&mut self, _port: u32) -> Result<bool> {
        Err(poe_unsupported())
    }

    fn set_poe(&mut self, _port: u32, _on: bool) -> Result<()> {
        Err(poe_unsupported())
    }
}

fn poe_unsupported() -> anyhow::Error {
    err(
        Kind::Usage,
        "PoE control is not supported by the easysmart backend yet (the PoE page and CGI \
         are not recorded); use --backend snmp on a switch that exposes POWER-ETHERNET-MIB",
    )
}

/// `host`, `host:port` or `http://host[:port]` to a base URL without a
/// trailing slash. Easy Smart is plain HTTP only.
pub fn base_url(device: &str) -> String {
    let d = device.trim().trim_end_matches('/');
    if d.contains("://") {
        d.to_string()
    } else {
        format!("http://{d}")
    }
}

/// The login reply embeds `logonInfo = new Array(<err>, …)`.
fn logon_code(body: &str) -> Option<i64> {
    let i = body.find("logonInfo")?;
    let rest = &body[i..];
    let j = rest.find("new Array(")? + "new Array(".len();
    let digits: String = rest[j..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// The new UI's login reply and logged-out stub embed `var allow = N;`
/// (1 logged in, 0 rejected or logged out, 2 password change required).
fn allow_code(body: &str) -> Option<i64> {
    let rest = &body[body.find("var allow")? + "var allow".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The old UI answers a missing session with its login form (`logon.cgi`);
/// the new one with a stub that has `var allow = 0` and no form.
fn is_login_page(body: &str) -> bool {
    body.to_ascii_lowercase().contains("logon.cgi") || allow_code(body) == Some(0)
}

fn identity_allowed(hardware: &str, firmware: &str) -> bool {
    ALLOWED.iter().any(|(hw, fw)| {
        let hw_ok = hardware.find(hw).is_some_and(|i| {
            // "TL-SG108PE 3.0" must not match "TL-SG108PE 3.01".
            let next = hardware[i + hw.len()..].chars().next();
            !next.is_some_and(|c| c.is_ascii_digit() || c == '.')
        });
        hw_ok && firmware.trim().starts_with(fw)
    })
}

/// `spd_act` codes: 0 link down, then the same numbering as `spd_cfg`
/// (2 = 10M half … 6 = 1000M full). Unknown codes are kept, not guessed.
fn speed_name(code: i64) -> Option<String> {
    match code {
        0 => None,
        2 => Some("10M half".into()),
        3 => Some("10M full".into()),
        4 => Some("100M half".into()),
        5 => Some("100M full".into()),
        6 => Some("1000M full".into()),
        n => Some(format!("speed code {n}")),
    }
}

#[derive(Debug)]
struct SystemInfo {
    description: String,
    hardware: String,
    firmware: String,
}

fn parse_system_info(body: &str) -> Result<SystemInfo> {
    let (obj, desc_key) = match extract(body, "info_ds") {
        Ok(o) => (o, "descriStr"),
        Err(_) => (extract(body, "data_info")?, "sysnameStr"),
    };
    let s = |k: &str| {
        obj.get(k)
            .and_then(Js::first_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(SystemInfo {
        description: s(desc_key),
        hardware: s("hardwareStr"),
        firmware: s("firmwareStr"),
    })
}

#[derive(Debug)]
struct PortTable {
    count: usize,
    state: Vec<i64>,
    spd_act: Vec<i64>,
}

fn parse_ports(body: &str) -> Result<PortTable> {
    let obj = extract(body, "all_info")?;
    let ints = |k: &str| -> Vec<i64> { obj.get(k).map(Js::ints).unwrap_or_default() };
    let state = ints("state");
    let mut spd_act = ints("spd_act");
    let count = match extract(body, "max_port_num") {
        Ok(Js::Num(n)) if n > 0 => n as usize,
        Ok(Js::Arr(a)) => match a.first() {
            Some(Js::Num(n)) if *n > 0 => *n as usize,
            _ => state.len(),
        },
        _ => state.len(),
    };
    if count == 0 || state.len() < count {
        return Err(err(
            Kind::Failed,
            "port settings page: could not read the per-port state; unexpected page",
        ));
    }
    spd_act.resize(count.max(spd_act.len()), 0);
    Ok(PortTable {
        count,
        state,
        spd_act,
    })
}

/// `portCur.xml` (new UI): four `&`-separated groups of comma-separated
/// per-port values, link, speed (1 = 10M, 2 = 100M, 3 = 1000M), duplex
/// (0 half, 1 full) and flow control. Returned in the old UI's `spd_act`
/// numbering so one `speed_name` serves both: 0 link down, 2/3 = 10M
/// half/full, 4/5 = 100M, 6 = 1000M; an unknown speed becomes 100 + code.
fn parse_port_cur(body: &str, count: usize) -> Result<Vec<i64>> {
    let groups: Vec<Vec<i64>> = body
        .trim()
        .split('&')
        .map(|g| {
            g.split(',')
                .filter(|v| !v.trim().is_empty())
                .map(|v| v.trim().parse().unwrap_or(-1))
                .collect()
        })
        .collect();
    if groups.len() < 3 || groups[0].len() < count || groups[1].len() < count {
        return Err(err(
            Kind::Failed,
            "portCur.xml: could not read per-port link and speed; unexpected page",
        ));
    }
    let duplex = |i: usize| groups[2].get(i).copied().unwrap_or(1);
    Ok((0..count)
        .map(|i| {
            if groups[0][i] != 1 {
                return 0;
            }
            let half = duplex(i) == 0;
            match groups[1][i] {
                1 => 3 - i64::from(half),
                2 => 5 - i64::from(half),
                3 => 6,
                n => 100 + n,
            }
        })
        .collect())
}

/// A parsed JavaScript literal; only what the switch's pages use.
#[derive(Debug, Clone, PartialEq)]
pub enum Js {
    Num(i64),
    Str(String),
    Arr(Vec<Js>),
    Obj(Vec<(String, Js)>),
    Other(String),
}

impl Js {
    fn get(&self, key: &str) -> Option<&Js> {
        match self {
            Js::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn first_str(&self) -> Option<&str> {
        match self {
            Js::Str(s) => Some(s),
            Js::Arr(a) => a.first().and_then(Js::first_str),
            _ => None,
        }
    }

    fn ints(&self) -> Vec<i64> {
        match self {
            Js::Arr(a) => a
                .iter()
                .map(|v| if let Js::Num(n) = v { *n } else { 0 })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Find `<name> = <literal>` in a page and parse the literal.
pub fn extract(body: &str, name: &str) -> Result<Js> {
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(off) = body[from..].find(name) {
        let start = from + off;
        let end = start + name.len();
        from = end;
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if !(before_ok && after_ok) {
            continue;
        }
        let mut p = Parser { s: bytes, i: end };
        p.skip_ws();
        if p.peek() != Some(b'=') {
            continue;
        }
        p.i += 1;
        return p
            .value()
            .map_err(|e| err(Kind::Failed, format!("parsing {name}: {e}")));
    }
    Err(err(
        Kind::Failed,
        format!("the page has no `{name}` assignment; unexpected page"),
    ))
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn value(&mut self) -> std::result::Result<Js, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(q @ (b'"' | b'\'')) => self.string(q).map(Js::Str),
            Some(c) if c.is_ascii_digit() || c == b'-' => {
                let start = self.i;
                self.i += 1;
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'.')
                {
                    self.i += 1;
                }
                let tok = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                Ok(tok.parse().map(Js::Num).unwrap_or(Js::Other(tok)))
            }
            Some(_) => {
                let start = self.i;
                while self
                    .peek()
                    .is_some_and(|c| !matches!(c, b',' | b'}' | b']' | b';' | b'\n'))
                {
                    self.i += 1;
                }
                Ok(Js::Other(
                    String::from_utf8_lossy(&self.s[start..self.i])
                        .trim()
                        .to_string(),
                ))
            }
            None => Err("unexpected end of page".into()),
        }
    }

    fn string(&mut self, quote: u8) -> std::result::Result<String, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".into()),
                Some(c) if c == quote => {
                    self.i += 1;
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                Some(b'\\') => {
                    self.i += 1;
                    if let Some(c) = self.peek() {
                        out.push(c);
                        self.i += 1;
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn array(&mut self) -> std::result::Result<Js, String> {
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Err("unterminated array".into()),
                Some(b']') => {
                    self.i += 1;
                    return Ok(Js::Arr(items));
                }
                Some(b',') => self.i += 1,
                Some(_) => items.push(self.value()?),
            }
        }
    }

    fn object(&mut self) -> std::result::Result<Js, String> {
        self.i += 1;
        let mut kv = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Err("unterminated object".into()),
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Js::Obj(kv));
                }
                Some(b',') => self.i += 1,
                Some(q @ (b'"' | b'\'')) => {
                    let key = self.string(q)?;
                    kv.push((key, self.after_key()?));
                }
                Some(c) if is_ident(c) => {
                    let start = self.i;
                    while self.peek().is_some_and(is_ident) {
                        self.i += 1;
                    }
                    let key = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                    kv.push((key, self.after_key()?));
                }
                Some(c) => return Err(format!("unexpected {:?} in object", c as char)),
            }
        }
    }

    fn after_key(&mut self) -> std::result::Result<Js, String> {
        self.skip_ws();
        if self.peek() != Some(b':') {
            return Err("expected ':' after object key".into());
        }
        self.i += 1;
        self.value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{self, kind_of, Opts};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// What the fake switch looks like and what it was asked.
    struct Fake {
        state: Vec<i64>,
        link: Vec<i64>,
        plugged: Vec<bool>,
        logged_in: bool,
        always_login_page: bool,
        ignore_writes: bool,
        link_stuck: bool,
        hardware: String,
        firmware: String,
        requests: Vec<String>,
        /// Speak the 2025 web UI.
        new_ui: bool,
        /// The new UI's valid session cookie, if any.
        cookie: Option<String>,
        /// Make the new UI answer a good login with this `allow` code.
        force_allow: Option<i64>,
        /// Cookie and Referer headers seen on protected new-UI requests.
        bad_headers: usize,
    }

    impl Fake {
        fn logon_count(&self) -> usize {
            self.requests
                .iter()
                .filter(|r| r.starts_with("POST /logon.cgi"))
                .count()
        }

        fn writes(&self) -> usize {
            self.requests
                .iter()
                .filter(|r| r.contains("port_setting.cgi"))
                .count()
        }

        fn ports_page(&self) -> String {
            let arr = |v: &[i64]| {
                v.iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!(
                "<script>var max_port_num = 8;\nvar all_info = {{\nstate:[{}],\nspd_cfg:[1,1,1,1,1,1,1,1],\n\
                 spd_act:[{}],\nfc_cfg:[0,0,0,0,0,0,0,0],\nfc_act:[0,0,0,0,0,0,0,0],\n\
                 trunk_info:[0,0,0,0,0,0,0,0]\n}};</script>",
                arr(&self.state),
                arr(&self.link)
            )
        }

        fn info_page_new(&self) -> String {
            format!(
                "<!DOCTYPE html><html> <script>var data_info = {{sysnameStr:[\"TL-SG205E\"],\
                 macStr:[\"00:11:22:33:44:55\"],ipStr:[\"192.0.2.10\"],firmwareStr:[\"{}\"], \
                 hardwareStr:[\"{}\"],serialNum:[\"AA00BB11CC22\"]}};\nvar tip = \"\";\n</script>",
                self.firmware, self.hardware
            )
        }

        fn ports_page_new(&self) -> String {
            let arr = |v: &[i64]| v.iter().map(|n| format!("{n},")).collect::<String>();
            format!(
                "<!DOCTYPE html><html> <script>\nvar max_port_num = [{}];\n\
                 var all_info = {{\nstate:[{}],\nspd_cfg:[0,0,0,0,0,],\n}};\nvar tip = \"\";\n</script>",
                self.state.len(),
                arr(&self.state)
            )
        }

        fn port_cur(&self) -> String {
            let up = |v: &[i64]| {
                v.iter()
                    .map(|n| format!("{},", i64::from(*n != 0)))
                    .collect::<String>()
            };
            let each = |x: i64| {
                self.link
                    .iter()
                    .map(|n| {
                        if *n != 0 {
                            format!("{x},")
                        } else {
                            "0,".into()
                        }
                    })
                    .collect::<String>()
            };
            format!(
                "{}&{}&{}&{}&",
                up(&self.link),
                each(3),
                each(1),
                "0,".repeat(self.link.len())
            )
        }

        fn info_page(&self) -> String {
            format!(
                "<script>var info_ds = {{\ndescriStr:[\"TL-SG108PE\"],\nmacStr:[\"00-11-22-33-44-55\"],\n\
                 ipStr:[\"192.0.2.2\"],\nnetmaskStr:[\"255.255.255.0\"],\ngatewayStr:[\"0.0.0.0\"],\n\
                 firmwareStr:[\"{}\"],\nhardwareStr:[\"{}\"]\n}};</script>",
                self.firmware, self.hardware
            )
        }
    }

    const LOGIN_PAGE: &str =
        "<html><form action=\"logon.cgi\" method=\"post\"><input name=\"username\"></form></html>";

    fn query_get(q: &str, key: &str) -> Option<String> {
        q.split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
    }

    const LOGGED_OUT: &str = "<!DOCTYPE html><html> <script>var allow = 0;</script> <script>function redirect(){top.location.href=\"index.html\"}</script><body onload=\"redirect()\"></body></html>";

    fn header(head: &str, name: &str) -> Option<String> {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    /// The 2025 UI: browser headers on login, cookie session, new page
    /// names; the old `*Rpm.htm` pages get no reply at all (`None`).
    fn handle_new(f: &mut Fake, head: &str, body: &str, method: &str, target: &str) -> Reply {
        if method == "POST" && target == "/logon.cgi" {
            let browserish = header(head, "origin")
                .is_some_and(|o| o.starts_with("http://127.0.0.1:"))
                && header(head, "referer").is_some_and(|r| r.ends_with("/index.html"))
                && header(head, "user-agent").is_some_and(|u| u.starts_with("Mozilla/5.0"));
            let ok = browserish
                && body.contains("username=admin")
                && body.contains("password=secret")
                && body.contains("logon=Login");
            let page =
                |n: i64| format!("<!DOCTYPE html><html> <script>var allow = {n};</script></html>");
            if !ok {
                return Some((page(0), vec![]));
            }
            if let Some(n) = f.force_allow {
                return Some((page(n), vec![]));
            }
            let tok = format!("tok{}", f.logon_count());
            f.cookie = Some(tok.clone());
            return Some((
                page(1),
                vec![("Set-cookie".into(), format!("Cookies={tok}; path=/"))],
            ));
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if !matches!(
            path,
            "/sys_SystemInfoRpm.html"
                | "/sys_portsetting.html"
                | "/portCur.xml"
                | "/port_setting.cgi"
        ) {
            return None;
        }
        let authed = f
            .cookie
            .as_ref()
            .is_some_and(|c| header(head, "cookie").as_deref() == Some(&format!("Cookies={c}")));
        if !authed {
            return Some((LOGGED_OUT.into(), vec![]));
        }
        if !header(head, "referer").is_some_and(|r| r.ends_with("/homepage.html")) {
            f.bad_headers += 1;
        }
        let page = match path {
            "/sys_SystemInfoRpm.html" => f.info_page_new(),
            "/sys_portsetting.html" => f.ports_page_new(),
            "/portCur.xml" => f.port_cur(),
            _ => {
                let bit: usize = query_get(query, "port_bit").unwrap().parse().unwrap();
                let st: i64 = query_get(query, "state").unwrap().parse().unwrap();
                let id = bit.trailing_zeros() as usize + 1;
                if !f.ignore_writes {
                    f.state[id - 1] = st;
                    if !f.link_stuck {
                        f.link[id - 1] = if st == 1 && f.plugged[id - 1] { 6 } else { 0 };
                    }
                }
                f.ports_page_new()
            }
        };
        Some((page, vec![]))
    }

    type Reply = Option<(String, Vec<(String, String)>)>;

    fn handle(fake: &Arc<Mutex<Fake>>, head: &str, body: &str) -> Reply {
        let mut f = fake.lock().unwrap();
        let line = head.lines().next().unwrap().to_string();
        let mut parts = line.split(' ');
        let method = parts.next().unwrap();
        let target = parts.next().unwrap();
        f.requests.push(format!("{method} {target}"));
        if f.new_ui {
            return handle_new(&mut f, head, body, method, target);
        }
        if method == "POST" && target == "/logon.cgi" {
            let ok = body.contains("username=admin")
                && body.contains("password=secret")
                && body.contains("logon=Login");
            if ok {
                f.logged_in = true;
                return Some(("logonInfo = new Array(0,0,0);".into(), vec![]));
            }
            return Some(("logonInfo = new Array(1,0,0);".into(), vec![]));
        }
        if !f.logged_in || f.always_login_page {
            return Some((LOGIN_PAGE.into(), vec![]));
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let page = match path {
            "/SystemInfoRpm.htm" => f.info_page(),
            "/PortSettingRpm.htm" => f.ports_page(),
            "/port_setting.cgi" => {
                let id: usize = query_get(query, "portid").unwrap().parse().unwrap();
                let st: i64 = query_get(query, "state").unwrap().parse().unwrap();
                if !f.ignore_writes {
                    f.state[id - 1] = st;
                    if !f.link_stuck {
                        f.link[id - 1] = if st == 1 && f.plugged[id - 1] { 6 } else { 0 };
                    }
                }
                f.ports_page()
            }
            _ => "404".into(),
        };
        Some((page, vec![]))
    }

    fn start() -> (Arc<Mutex<Fake>>, String) {
        // Ports 1 (management) and 3 (target) are plugged in and up.
        let plugged = vec![true, false, true, false, false, false, false, false];
        let link = plugged.iter().map(|p| if *p { 6 } else { 0 }).collect();
        let fake = Arc::new(Mutex::new(Fake {
            state: vec![1; 8],
            link,
            plugged,
            logged_in: false,
            always_login_page: false,
            ignore_writes: false,
            link_stuck: false,
            hardware: "TL-SG108PE 3.0".into(),
            firmware: "1.0.0 Build 20200415 Rel.34571".into(),
            requests: Vec::new(),
            new_ui: false,
            cookie: None,
            force_allow: None,
            bad_headers: 0,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let shared = Arc::clone(&fake);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                let (head, mut body) = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (String::from_utf8_lossy(&buf).into_owned(), Vec::new());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (
                            String::from_utf8_lossy(&buf[..i]).into_owned(),
                            buf[i + 4..].to_vec(),
                        );
                    }
                };
                let want: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while body.len() < want {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let Some((reply, extra)) = handle(&shared, &head, &String::from_utf8_lossy(&body))
                else {
                    continue;
                };
                let extra: String = extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                );
            }
        });
        (fake, addr)
    }

    /// A fake TL-SG205E speaking the 2025 UI (ports 1 and 3 plugged in).
    fn start_new() -> (Arc<Mutex<Fake>>, String) {
        let (fake, addr) = start();
        {
            let mut f = fake.lock().unwrap();
            f.new_ui = true;
            f.hardware = "TL-SG205E 1.0".into();
            f.firmware = "1.0.1 Build 20250609 Rel.75246".into();
            f.state.truncate(5);
            f.link.truncate(5);
            f.plugged.truncate(5);
        }
        (fake, addr)
    }

    fn client(addr: &str) -> EasySmart {
        EasySmart::new(addr, "admin", "secret", false)
    }

    fn opts(mgmt: Option<u32>, wait_ms: u64) -> Opts {
        Opts {
            mgmt_port: mgmt,
            wait: Duration::from_millis(wait_ms),
            poll: Duration::from_millis(10),
        }
    }

    #[test]
    fn login_and_status() {
        let (fake, addr) = start();
        let mut be = client(&addr);
        let st = ops::port_state(&mut be, 3).unwrap();
        assert!(st.admin_up && st.link_up);
        assert_eq!(st.speed.as_deref(), Some("1000M full"));
        let st = ops::port_state(&mut be, 4).unwrap();
        assert!(st.admin_up && !st.link_up && st.speed.is_none());
        assert_eq!(fake.lock().unwrap().logon_count(), 1);
    }

    #[test]
    fn bad_login_is_not_retried() {
        let (fake, addr) = start();
        let mut be = EasySmart::new(&addr, "admin", "wrong", false);
        let e = ops::port_state(&mut be, 3).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Failed);
        assert!(format!("{e}").contains("rejected"), "{e}");
        assert_eq!(fake.lock().unwrap().logon_count(), 1);
    }

    #[test]
    fn expired_session_relogs_in_once() {
        let (fake, addr) = start();
        let mut be = client(&addr);
        ops::port_state(&mut be, 3).unwrap();
        fake.lock().unwrap().logged_in = false;
        ops::port_state(&mut be, 3).unwrap();
        assert_eq!(fake.lock().unwrap().logon_count(), 2);
    }

    #[test]
    fn login_page_after_successful_login_fails() {
        let (fake, addr) = start();
        fake.lock().unwrap().always_login_page = true;
        let mut be = client(&addr);
        let e = ops::port_state(&mut be, 3).unwrap_err();
        assert!(format!("{e}").contains("login page"), "{e}");
        assert_eq!(fake.lock().unwrap().logon_count(), 1);
    }

    #[test]
    fn unknown_hardware_is_refused_before_any_write() {
        let (fake, addr) = start();
        fake.lock().unwrap().hardware = "TL-SG108PE 4.0".into();
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap_err();
        assert!(format!("{e}").contains("unrecognized"), "{e}");
        assert_eq!(fake.lock().unwrap().writes(), 0);
    }

    #[test]
    fn unknown_firmware_is_refused_and_escape_hatch_works() {
        let (fake, addr) = start();
        fake.lock().unwrap().firmware = "1.1.0 Build 20230101".into();
        let mut be = client(&addr);
        assert!(ops::port_set(&mut be, &opts(Some(1), 0), 3, false).is_err());
        assert_eq!(fake.lock().unwrap().writes(), 0);
        let mut be = EasySmart::new(&addr, "admin", "secret", true);
        ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap();
        assert_eq!(fake.lock().unwrap().writes(), 1);
    }

    #[test]
    fn mgmt_port_off_is_refused_without_any_request() {
        let (fake, addr) = start();
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 1, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Usage);
        assert!(format!("{e}").contains("management path"), "{e}");
        let e = ops::port_set(&mut be, &opts(None, 0), 3, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::NotConfigured);
        assert!(format!("{e}").contains("--mgmt-port"), "{e}");
        assert!(fake.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn off_and_on_are_verified_and_reported() {
        let (fake, addr) = start();
        let mut be = client(&addr);
        let st = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap();
        assert!(!st.admin_up);
        {
            let f = fake.lock().unwrap();
            assert_eq!(f.state[2], 0);
            assert!(f
                .requests
                .iter()
                .any(|r| r
                    == "GET /port_setting.cgi?portid=3&state=0&speed=7&flowcontrol=7&apply=Apply"));
        }
        let st = ops::port_set(&mut be, &opts(Some(1), 0), 3, true).unwrap();
        assert!(st.admin_up);
        assert_eq!(fake.lock().unwrap().state[2], 1);
    }

    #[test]
    fn read_back_mismatch_is_an_error() {
        let (fake, addr) = start();
        fake.lock().unwrap().ignore_writes = true;
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Failed);
        assert!(format!("{e}").contains("read-back"), "{e}");
        assert_eq!(fake.lock().unwrap().writes(), 1);
    }

    #[test]
    fn default_does_not_wait_for_link() {
        let (_fake, addr) = start();
        let mut be = client(&addr);
        // Port 4 has nothing plugged in: admin on succeeds, link stays down.
        let st = ops::port_set(&mut be, &opts(Some(1), 0), 4, true).unwrap();
        assert!(st.admin_up && !st.link_up);
    }

    #[test]
    fn wait_link_succeeds_when_link_follows() {
        let (_fake, addr) = start();
        let mut be = client(&addr);
        let st = ops::port_set(&mut be, &opts(Some(1), 1000), 3, false).unwrap();
        assert!(!st.link_up);
        let st = ops::port_set(&mut be, &opts(Some(1), 1000), 3, true).unwrap();
        assert!(st.link_up);
    }

    #[test]
    fn wait_link_times_out_with_a_distinct_error() {
        let (fake, addr) = start();
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 100), 4, true).unwrap_err();
        assert_eq!(kind_of(&e), Kind::LinkTimeout);
        assert!(format!("{e}").contains("admin state is on"), "{e}");
        fake.lock().unwrap().link_stuck = true;
        let e = ops::port_set(&mut be, &opts(Some(1), 100), 3, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::LinkTimeout);
        assert_eq!(fake.lock().unwrap().state[2], 0);
    }

    #[test]
    fn out_of_range_port_is_usage() {
        let (_fake, addr) = start();
        let mut be = client(&addr);
        let e = ops::port_state(&mut be, 9).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Usage);
    }

    #[test]
    fn poe_is_unsupported_on_easysmart() {
        let (_fake, addr) = start();
        let mut be = client(&addr);
        let e = ops::poe_state(&mut be, 3).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Usage);
        assert!(format!("{e}").contains("not supported"), "{e}");
    }

    #[test]
    fn unreachable_switch_is_exit_4() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        let mut be = client(&addr);
        let e = ops::port_state(&mut be, 3).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Unreachable);
    }

    #[test]
    fn js_parser_is_tolerant() {
        let page = "x = 1; var a = { k: [1, 2, -3,], 'q': \"s\\\"t\", n: {z:[ 'u' ]}, bare: foo };";
        let js = extract(page, "a").unwrap();
        assert_eq!(js.get("k").unwrap().ints(), vec![1, 2, -3]);
        assert_eq!(js.get("q").unwrap().first_str(), Some("s\"t"));
        assert_eq!(
            js.get("n").unwrap().get("z").unwrap().first_str(),
            Some("u")
        );
        assert_eq!(js.get("bare"), Some(&Js::Other("foo".into())));
        assert!(extract("var data = {};", "a").is_err());
    }

    #[test]
    fn logon_code_and_identity_matching() {
        assert_eq!(logon_code("var logonInfo = new Array(1,0,0);"), Some(1));
        assert_eq!(logon_code("nothing"), None);
        assert!(identity_allowed(
            "TL-SG108PE 3.0",
            "1.0.0 Build 20200415 Rel.1"
        ));
        assert!(!identity_allowed("TL-SG108PE 3.01", "1.0.0 Build 20200415"));
        assert!(!identity_allowed("TL-SG108PE 3.0", "1.0.1 Build 20210101"));
    }

    #[test]
    fn recorded_pages_from_the_bench_switch_parse() {
        let info =
            parse_system_info(include_str!("../tests/fixtures/raw/SystemInfoRpm.htm")).unwrap();
        assert_eq!(info.description, "TL-SG108PE");
        assert!(identity_allowed(&info.hardware, &info.firmware), "{info:?}");
        let t = parse_ports(include_str!("../tests/fixtures/raw/PortSettingRpm.htm")).unwrap();
        assert_eq!(t.count, 8);
        assert_eq!(&t.state[..8], &[1; 8]);
        assert_eq!(&t.spd_act[..8], &[6, 0, 0, 0, 0, 0, 0, 6]);
        let logon = include_str!("../tests/fixtures/raw/logon-success.html");
        assert_eq!(logon_code(logon), Some(0));
        // The recorded pages other than the login page are not login pages.
        assert!(!is_login_page(include_str!(
            "../tests/fixtures/raw/PortSettingRpm.htm"
        )));
        assert!(is_login_page(logon));
    }

    #[test]
    fn new_ui_login_and_status() {
        let (fake, addr) = start_new();
        let mut be = client(&addr);
        let st = ops::port_state(&mut be, 3).unwrap();
        assert!(st.admin_up && st.link_up);
        assert_eq!(st.speed.as_deref(), Some("1000M full"));
        let st = ops::port_state(&mut be, 4).unwrap();
        assert!(st.admin_up && !st.link_up && st.speed.is_none());
        let f = fake.lock().unwrap();
        assert_eq!(f.logon_count(), 1);
        assert_eq!(f.bad_headers, 0);
        assert!(
            !f.requests.iter().any(|r| r.ends_with("Rpm.htm")),
            "{:?}",
            f.requests
        );
    }

    #[test]
    fn new_ui_off_and_on_are_verified() {
        let (fake, addr) = start_new();
        let mut be = client(&addr);
        let st = ops::port_set(&mut be, &opts(Some(1), 0), 5, false).unwrap();
        assert!(!st.admin_up);
        {
            let f = fake.lock().unwrap();
            assert_eq!(f.state[4], 0);
            assert!(f
                .requests
                .iter()
                .any(|r| r == "GET /port_setting.cgi?port_bit=16&state=0"));
        }
        let st = ops::port_set(&mut be, &opts(Some(1), 0), 5, true).unwrap();
        assert!(st.admin_up);
        assert_eq!(fake.lock().unwrap().state[4], 1);
        let st = ops::port_set(&mut be, &opts(Some(1), 1000), 3, false).unwrap();
        assert!(!st.link_up);
    }

    #[test]
    fn new_ui_read_back_mismatch_is_an_error() {
        let (fake, addr) = start_new();
        fake.lock().unwrap().ignore_writes = true;
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap_err();
        assert!(format!("{e}").contains("read-back"), "{e}");
    }

    #[test]
    fn new_ui_mgmt_port_off_is_refused_without_any_request() {
        let (fake, addr) = start_new();
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 1, false).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Usage);
        assert!(fake.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn new_ui_expired_cookie_relogs_in_once() {
        let (fake, addr) = start_new();
        let mut be = client(&addr);
        ops::port_state(&mut be, 3).unwrap();
        fake.lock().unwrap().cookie = None;
        ops::port_state(&mut be, 3).unwrap();
        assert_eq!(fake.lock().unwrap().logon_count(), 2);
    }

    #[test]
    fn new_ui_rejected_login_is_not_retried() {
        let (fake, addr) = start_new();
        let mut be = EasySmart::new(&addr, "admin", "wrong", false);
        let e = ops::port_state(&mut be, 3).unwrap_err();
        assert_eq!(kind_of(&e), Kind::Failed);
        assert!(format!("{e}").contains("rejected"), "{e}");
        assert_eq!(fake.lock().unwrap().logon_count(), 1);
    }

    #[test]
    fn new_ui_password_change_demand_tells_the_user() {
        let (fake, addr) = start_new();
        fake.lock().unwrap().force_allow = Some(2);
        let mut be = client(&addr);
        let e = ops::port_state(&mut be, 3).unwrap_err();
        assert!(format!("{e}").contains("web browser"), "{e}");
        assert_eq!(fake.lock().unwrap().logon_count(), 1);
    }

    #[test]
    fn new_ui_unknown_model_is_refused_before_any_write() {
        let (fake, addr) = start_new();
        fake.lock().unwrap().hardware = "TL-SG205E 2.0".into();
        let mut be = client(&addr);
        let e = ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap_err();
        assert!(format!("{e}").contains("unrecognized"), "{e}");
        assert_eq!(fake.lock().unwrap().writes(), 0);
        let mut be = EasySmart::new(&addr, "admin", "secret", true);
        ops::port_set(&mut be, &opts(Some(1), 0), 3, false).unwrap();
        assert_eq!(fake.lock().unwrap().writes(), 1);
    }

    #[test]
    fn port_cur_decoding() {
        assert_eq!(
            parse_port_cur("1,0,0,0,1,&3,1,1,1,3,&1,0,0,0,1,&0,0,0,0,0,&", 5).unwrap(),
            vec![6, 0, 0, 0, 6]
        );
        assert_eq!(
            parse_port_cur("1,1,1,&1,2,2,&0,0,1,&0,0,0,&", 3).unwrap(),
            vec![2, 4, 5]
        );
        assert!(parse_port_cur("1,&", 3).is_err());
    }

    #[test]
    fn recorded_sg205e_pages_parse() {
        let info = parse_system_info(include_str!(
            "../tests/fixtures/raw/sg205e/sys_SystemInfoRpm.html"
        ))
        .unwrap();
        assert_eq!(info.description, "TL-SG205E");
        assert!(identity_allowed(&info.hardware, &info.firmware), "{info:?}");
        let t = parse_ports(include_str!(
            "../tests/fixtures/raw/sg205e/sys_portsetting.html"
        ))
        .unwrap();
        assert_eq!(t.count, 5);
        assert_eq!(&t.state[..5], &[1; 5]);
        let cur = include_str!("../tests/fixtures/raw/sg205e/portCur.xml");
        assert_eq!(parse_port_cur(cur, 5).unwrap(), vec![6, 0, 0, 0, 6]);
        let t = parse_ports(include_str!(
            "../tests/fixtures/raw/sg205e/port_setting-reply.html"
        ))
        .unwrap();
        assert_eq!(&t.state[..5], &[1, 1, 1, 1, 0]);
        assert_eq!(
            allow_code(include_str!(
                "../tests/fixtures/raw/sg205e/logon-success.html"
            )),
            Some(1)
        );
        let out = include_str!("../tests/fixtures/raw/sg205e/logged-out.html");
        assert_eq!(allow_code(out), Some(0));
        assert!(is_login_page(out));
        assert!(is_login_page(include_str!(
            "../tests/fixtures/raw/sg205e/login-page.html"
        )));
        assert!(!is_login_page(cur));
    }
}
