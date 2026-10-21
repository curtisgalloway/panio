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

//! Backend-independent logic: the error kinds (exit codes), the backend
//! trait, and every safety rule. Both backends are driven through these
//! functions, so the rules exist once and are tested against both.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

/// What went wrong, mapped to the exit statuses in docs/errors.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The command ran and failed (1).
    Failed,
    /// Bad arguments, or an operation this backend does not support (2).
    Usage,
    /// A required credential or option is missing (3).
    NotConfigured,
    /// The switch did not answer (4).
    Unreachable,
    /// The write took effect but the link did not follow in time (22).
    LinkTimeout,
}

impl Kind {
    pub fn exit_code(self) -> u8 {
        match self {
            Kind::Failed => 1,
            Kind::Usage => 2,
            Kind::NotConfigured => 3,
            Kind::Unreachable => 4,
            Kind::LinkTimeout => 22,
        }
    }
}

#[derive(Debug)]
pub struct SwitchError {
    pub kind: Kind,
    pub msg: String,
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for SwitchError {}

pub fn err(kind: Kind, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SwitchError {
        kind,
        msg: msg.into(),
    })
}

/// The kind of any error, for the exit status.
pub fn kind_of(e: &anyhow::Error) -> Kind {
    if let Some(se) = e.downcast_ref::<SwitchError>() {
        return se.kind;
    }
    if secret::is_not_configured(e.as_ref()) {
        return Kind::NotConfigured;
    }
    Kind::Failed
}

/// One port as the switch reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortStatus {
    pub admin_up: bool,
    pub link_up: bool,
    /// Negotiated speed, when the link is up and the backend knows it.
    pub speed: Option<String>,
}

/// A switch the helper can drive. Ports are 1-based as printed on the switch.
pub trait Backend {
    /// Confirm this is a switch the backend is willing to drive; returns a
    /// description for `doctor`. Errors when the identity is refused.
    fn check_identity(&mut self) -> Result<String>;
    /// How many ports (or interfaces, for SNMP) the switch reports.
    fn port_count(&mut self) -> Result<u32>;
    fn port_status(&mut self, port: u32) -> Result<PortStatus>;
    fn set_admin(&mut self, port: u32, up: bool) -> Result<()>;
    fn poe_status(&mut self, port: u32) -> Result<bool>;
    fn set_poe(&mut self, port: u32, on: bool) -> Result<()>;
}

/// Safety and timing settings shared by every write.
#[derive(Debug, Clone)]
pub struct Opts {
    /// The switch port the control host reaches the switch through.
    pub mgmt_port: Option<u32>,
    /// How long to wait for the link to follow a write (`--wait-link`); zero,
    /// the default, reports the link without waiting.
    pub wait: Duration,
    /// Delay between link polls.
    pub poll: Duration,
}

/// Refuse to turn off the port the helper itself uses. Runs before any
/// request is sent, so a refusal costs the switch nothing.
pub fn guard_mgmt(opts: &Opts, port: u32, what: &str) -> Result<()> {
    match opts.mgmt_port {
        None => Err(err(
            Kind::NotConfigured,
            format!(
                "refusing to turn {what} {port} off: --mgmt-port was not given, so the \
                 helper cannot tell whether port {port} is the one it reaches the switch \
                 through, and turning that port off would cut the helper's own management \
                 path (the switch would then need fixing by hand). Pass --mgmt-port <N> \
                 naming the switch port your control host is plugged into"
            ),
        )),
        Some(m) if m == port => Err(err(
            Kind::Usage,
            format!(
                "refusing to turn {what} {port} off: port {port} is --mgmt-port, and \
                 turning it off would cut the helper's own management path to the switch"
            ),
        )),
        Some(_) => Ok(()),
    }
}

fn updown(up: bool) -> &'static str {
    if up {
        "up"
    } else {
        "down"
    }
}

fn onoff(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// Set a port's admin state, verify by read-back, then wait for the link to
/// follow. Returns the final status.
pub fn port_set(be: &mut dyn Backend, opts: &Opts, port: u32, up: bool) -> Result<PortStatus> {
    if !up {
        guard_mgmt(opts, port, "port")?;
    }
    be.check_identity()?;
    be.set_admin(port, up)?;
    let mut st = be.port_status(port)?;
    if st.admin_up != up {
        return Err(err(
            Kind::Failed,
            format!(
                "port {port}: commanded admin {} but the switch reports {} on read-back",
                onoff(up),
                onoff(st.admin_up)
            ),
        ));
    }
    if opts.wait.is_zero() {
        return Ok(st);
    }
    let deadline = Instant::now() + opts.wait;
    while st.link_up != up {
        if Instant::now() >= deadline {
            let detail = if up {
                "nothing plugged in, or the peer is down?"
            } else {
                "the port did not drop its link"
            };
            return Err(err(
                Kind::LinkTimeout,
                format!(
                    "port {port}: admin state is {} but link is still {} after {:.1}s ({detail})",
                    onoff(up),
                    updown(st.link_up),
                    opts.wait.as_secs_f64()
                ),
            ));
        }
        thread::sleep(opts.poll);
        st = be.port_status(port)?;
    }
    Ok(st)
}

pub fn port_state(be: &mut dyn Backend, port: u32) -> Result<PortStatus> {
    be.check_identity()?;
    be.port_status(port)
}

/// Set PoE and verify by read-back.
pub fn poe_set(be: &mut dyn Backend, opts: &Opts, port: u32, on: bool) -> Result<()> {
    if !on {
        guard_mgmt(opts, port, "PoE on port")?;
    }
    be.check_identity()?;
    be.set_poe(port, on)?;
    let now = be.poe_status(port)?;
    if now != on {
        return Err(err(
            Kind::Failed,
            format!(
                "port {port}: commanded PoE {} but the switch reports {} on read-back",
                onoff(on),
                onoff(now)
            ),
        ));
    }
    Ok(())
}

pub fn poe_state(be: &mut dyn Backend, port: u32) -> Result<bool> {
    be.check_identity()?;
    be.poe_status(port)
}

pub fn status_line(port: u32, st: &PortStatus) -> String {
    let mut s = format!(
        "port {port}: admin {}, link {}",
        onoff(st.admin_up),
        updown(st.link_up)
    );
    if let Some(sp) = &st.speed {
        s.push_str(&format!(" ({sp})"));
    }
    s
}

pub fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn status_json(port: u32, st: &PortStatus) -> String {
    let speed = match &st.speed {
        Some(s) => json_str(s),
        None => "null".to_string(),
    };
    format!(
        "{{\"port\":{port},\"admin\":\"{}\",\"link\":\"{}\",\"speed\":{speed}}}",
        onoff(st.admin_up),
        updown(st.link_up)
    )
}

pub fn poe_json(port: u32, on: bool) -> String {
    format!("{{\"port\":{port},\"poe\":\"{}\"}}", onoff(on))
}
