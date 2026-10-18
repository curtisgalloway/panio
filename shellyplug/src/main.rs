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
// limitations under the License.

//! shellyplug — power control for Shelly Gen2+ smart plugs and relays.
//!
//! A one-shot panio power helper: each invocation makes a single stateless
//! HTTP RPC call to the device and exits. Hook-facing subcommands follow the
//! panio helper conventions (docs/dev/adding-power-helpers.md):
//!   state <id>    prints exactly `on` or `off`
//!   on/off <id>   switch + read-back confirm
//!   cycle <id>    off → confirm → delay → on → confirm
//!   doctor --json read-only health report for `panio doctor`

mod rpc;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};

use rpc::Client;

/// Settle time between commanding the relay and reading state back. The relay
/// switches synchronously, but a brief pause lets the metering subsystem catch
/// up so the read-back reflects the command.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Parser)]
#[command(
    name = "shellyplug",
    version,
    about = "Power control for Shelly Gen2+ smart plugs/relays over the local HTTP RPC API",
    long_about = "Power control for Shelly Gen2+ smart plugs and relays (Plus / Pro / Gen3 / \
Gen4) over the device's local HTTP RPC API — no cloud, no Home Assistant, no Matter.

MENTAL MODEL
  - A device is addressed by its address on your network: pass --device with an
    IP or hostname (10.0.0.5, shelly.local), optionally with a scheme or port.
    A DHCP reservation or the device's .local mDNS name keeps the hook stable
    across reboots.
  - A switch is addressed by its component id (the positional [ID], default 0).
    Single-outlet plugs only have id 0; multi-channel devices (e.g. a Pro 4PM)
    use 0..N.
  - on/off/cycle confirm by reading the relay state back, so a hook that
    silently failed surfaces as a non-zero exit.

TYPICAL USE
  shellyplug -d 10.0.0.5 status        device info + switch state and power
  shellyplug -d 10.0.0.5 state         prints exactly `on` or `off`
  shellyplug -d 10.0.0.5 on|off [id]
  shellyplug -d 10.0.0.5 cycle [id] [--delay-ms 3000]
  shellyplug -d 10.0.0.5 doctor [id] [--json]   read-only health check

Only devices with authentication disabled are supported for now; an
auth-enabled device answers HTTP 401 with a clear message."
)]
struct Cli {
    /// Device address: IP or hostname, optionally `http://host` or `host:port`
    /// (e.g. 10.0.0.5, shelly.local, http://10.0.0.5:8080).
    #[arg(short = 'd', long = "device", value_name = "HOST", global = true)]
    device: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print exactly `on` or `off` for a switch (hook: state_cmd).
    State {
        /// Switch component id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Switch on and confirm by read-back (hook: on_cmd).
    On {
        /// Switch component id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Switch off and confirm by read-back (hook: off_cmd).
    Off {
        /// Switch component id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Power-cycle: off → confirm → delay → on → confirm (hook: cycle_cmd).
    Cycle {
        /// Switch component id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
        /// Milliseconds to hold the switch off before restoring power.
        #[arg(long, default_value_t = 3000)]
        delay_ms: u64,
    },
    /// Human-readable device info plus a switch's state and power metering.
    Status {
        /// Switch component id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Read-only health check (panio doctor): is the device reachable, does
    /// it demand authentication, and does it have the switch. Never switches
    /// the relay.
    ///
    /// Without an id it lists every switch the device has; with one it checks
    /// that switch. `--json` prints panio's doctor report
    /// (`{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}`) and
    /// exits 0 whatever the checks found; without it, one line per check and
    /// exit 1 if any failed.
    Doctor {
        /// Switch component id to check (default: list them all).
        id: Option<u32>,
        /// Print the JSON report panio doctor reads.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Doctor { id, json } = cli.cmd {
        let checks = match cli.device.as_deref() {
            Some(d) => doctor_checks(&Client::new(d), id),
            None => vec![Check::fail("device", "no --device (-d) given")],
        };
        return report_doctor(&checks, json);
    }
    let device = cli
        .device
        .as_deref()
        .ok_or_else(|| anyhow!("required option '--device <HOST>' (-d) was not provided"))?;
    let client = Client::new(device);
    match cli.cmd {
        Cmd::State { id } => cmd_state(&client, id),
        Cmd::On { id } => cmd_switch(&client, id, true),
        Cmd::Off { id } => cmd_switch(&client, id, false),
        Cmd::Cycle { id, delay_ms } => cmd_cycle(&client, id, delay_ms),
        Cmd::Status { id } => cmd_status(&client, id),
        Cmd::Doctor { .. } => unreachable!("handled above"),
    }
}

/// One entry of the doctor report (docs/dev/adding-power-helpers.md,
/// "Doctor check").
#[derive(Debug, PartialEq, Eq)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            status: "ok",
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            status: "fail",
            detail: detail.into(),
        }
    }
}

/// The read-only health checks, in order; each later check needs the one
/// before it to have passed, so a failure ends the list. Only
/// `Shelly.GetDeviceInfo`, `Shelly.GetStatus` and `Switch.GetStatus` are
/// called — never `Switch.Set`.
fn doctor_checks(client: &Client, id: Option<u32>) -> Vec<Check> {
    let info = match client.device_info() {
        Ok(info) => info,
        Err(e) => return vec![Check::fail("reachable", format!("{e:#}"))],
    };
    let mut checks = vec![Check::ok(
        "reachable",
        format!(
            "{} {}, gen {}, firmware {}",
            info.model,
            info.id,
            info.generation,
            info.ver.as_deref().unwrap_or("?")
        ),
    )];
    if info.auth_en {
        checks.push(Check::fail(
            "auth",
            "the device requires authentication; shellyplug does not support \
             password-protected devices yet, so every hook will fail",
        ));
        return checks;
    }
    checks.push(Check::ok("auth", "authentication disabled"));
    checks.push(match id {
        Some(id) => match client.switch_status(id) {
            Ok(st) => Check::ok(
                "switch",
                format!("switch:{id} present (currently {})", onoff(st.output)),
            ),
            Err(e) => Check::fail("switch", format!("switch:{id}: {e:#}")),
        },
        None => match client.all_status() {
            Ok(status) => {
                let switches = switch_components(&status);
                if switches.is_empty() {
                    Check::fail("switch", "the device has no switch component")
                } else {
                    Check::ok("switch", format!("{} present", switches.join(", ")))
                }
            }
            Err(e) => Check::fail("switch", format!("{e:#}")),
        },
    });
    checks
}

/// The `switch:<id>` component keys of a `Shelly.GetStatus` result, in id
/// order.
fn switch_components(status: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<u32> = status
        .as_object()
        .map(|o| {
            o.keys()
                .filter_map(|k| k.strip_prefix("switch:")?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable();
    ids.into_iter().map(|i| format!("switch:{i}")).collect()
}

/// Print the doctor report. With `--json` it is the contract's JSON and the
/// exit is 0 whatever the checks found; otherwise one line per check, and a
/// failed check fails the command.
fn report_doctor(checks: &[Check], json: bool) -> Result<()> {
    if json {
        println!("{}", doctor_json(checks));
        return Ok(());
    }
    for c in checks {
        println!("{:<5} {:<10} {}", c.status, c.name, c.detail);
    }
    if checks.iter().any(|c| c.status == "fail") {
        bail!("doctor found a problem");
    }
    Ok(())
}

fn doctor_json(checks: &[Check]) -> serde_json::Value {
    serde_json::json!({
        "checks": checks
            .iter()
            .map(|c| serde_json::json!({"name": c.name, "status": c.status, "detail": c.detail}))
            .collect::<Vec<_>>()
    })
}

fn onoff(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// The two operations the hook sequencing needs from a device: command the
/// relay, and read its state back. Abstracted so the on/off/cycle logic can
/// be exercised against a fake, which is the only way to prove the
/// confirmation steps actually run without a plug on the bench.
trait Switch {
    fn set(&self, id: u32, on: bool) -> Result<()>;
    fn is_on(&self, id: u32) -> Result<bool>;
}

impl Switch for Client {
    fn set(&self, id: u32, on: bool) -> Result<()> {
        self.switch_set(id, on)
    }

    fn is_on(&self, id: u32) -> Result<bool> {
        Ok(self.switch_status(id)?.output)
    }
}

fn cmd_state(client: &Client, id: u32) -> Result<()> {
    let st = client.switch_status(id)?;
    println!("{}", onoff(st.output));
    Ok(())
}

/// Command the relay and confirm by read-back, failing on a mismatch.
fn set_confirmed(sw: &impl Switch, id: u32, on: bool) -> Result<()> {
    sw.set(id, on)?;
    thread::sleep(SETTLE);
    let now = sw.is_on(id)?;
    if now != on {
        bail!(
            "switch {id}: commanded {} but device reports {}",
            onoff(on),
            onoff(now)
        );
    }
    Ok(())
}

fn cmd_switch(sw: &impl Switch, id: u32, on: bool) -> Result<()> {
    set_confirmed(sw, id, on)?;
    println!("switch {id}: {}", onoff(on));
    Ok(())
}

/// off → confirm → delay → on → confirm. The off phase is confirmed before
/// the hold starts: a relay that ignored the off command would otherwise
/// produce a "cycle" that never removed power, with the final read-back
/// happily reporting on.
fn cmd_cycle(sw: &impl Switch, id: u32, delay_ms: u64) -> Result<()> {
    set_confirmed(sw, id, false).map_err(|e| anyhow!("{e} (cycle aborted before the off-hold)"))?;
    thread::sleep(Duration::from_millis(delay_ms));
    set_confirmed(sw, id, true).map_err(|e| anyhow!("{e} (after the off-hold)"))?;
    println!("switch {id}: cycled ({delay_ms} ms off)");
    Ok(())
}

fn cmd_status(client: &Client, id: u32) -> Result<()> {
    let info = client.device_info()?;
    let name = info.name.as_deref().unwrap_or("(unnamed)");
    let app = info
        .app
        .as_deref()
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();
    let ver = info.ver.as_deref().unwrap_or("?");
    println!("device   {} — {}{}", info.id, info.model, app);
    println!("name     {name}");
    println!("gen/fw   gen {}, {ver}", info.generation);
    println!(
        "auth     {}",
        if info.auth_en { "enabled" } else { "disabled" }
    );

    let st = client.switch_status(id)?;
    println!("switch {id}  {}", onoff(st.output));
    if let Some(w) = st.apower {
        println!("  power    {w:.1} W");
    }
    if let Some(v) = st.voltage {
        println!("  voltage  {v:.1} V");
    }
    if let Some(a) = st.current {
        println!("  current  {a:.3} A");
    }
    if let Some(t) = st.temperature.as_ref().and_then(|t| t.t_c) {
        println!("  temp     {t:.1} °C");
    }
    if let Some(e) = st.aenergy.as_ref().and_then(|e| e.total) {
        println!("  energy   {e:.1} Wh total");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A relay that records every call and can be told to ignore commands,
    /// standing in for a plug whose `Switch.Set` returned OK without acting.
    struct Fake {
        on: Cell<bool>,
        ignore_off: bool,
        ignore_on: bool,
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(on: bool) -> Self {
            Fake {
                on: Cell::new(on),
                ignore_off: false,
                ignore_on: false,
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl Switch for Fake {
        fn set(&self, id: u32, on: bool) -> Result<()> {
            self.log
                .borrow_mut()
                .push(format!("set {id} {}", onoff(on)));
            let ignored = if on { self.ignore_on } else { self.ignore_off };
            if !ignored {
                self.on.set(on);
            }
            Ok(())
        }

        fn is_on(&self, id: u32) -> Result<bool> {
            self.log.borrow_mut().push(format!("status {id}"));
            Ok(self.on.get())
        }
    }

    #[test]
    fn cycle_confirms_off_then_on() {
        let plug = Fake::new(true);
        cmd_cycle(&plug, 0, 0).unwrap();
        assert!(plug.on.get());
        assert_eq!(
            *plug.log.borrow(),
            ["set 0 off", "status 0", "set 0 on", "status 0"]
        );
    }

    #[test]
    fn cycle_aborts_before_the_hold_when_off_did_not_take() {
        let plug = Fake {
            ignore_off: true,
            ..Fake::new(true)
        };
        let err = cmd_cycle(&plug, 3, 0).expect_err("a relay stuck on must fail the cycle");
        assert!(err.to_string().contains("commanded off"), "{err}");
        // It must stop right there: no power-on was ever sent.
        assert_eq!(*plug.log.borrow(), ["set 3 off", "status 3"]);
    }

    #[test]
    fn cycle_fails_when_on_did_not_take() {
        let plug = Fake {
            ignore_on: true,
            ..Fake::new(true)
        };
        let err = cmd_cycle(&plug, 0, 0).expect_err("a relay stuck off must fail the cycle");
        assert!(err.to_string().contains("commanded on"), "{err}");
        assert!(!plug.on.get());
    }

    #[test]
    fn on_and_off_confirm_by_read_back() {
        let plug = Fake::new(false);
        cmd_switch(&plug, 1, true).unwrap();
        assert!(plug.on.get());
        cmd_switch(&plug, 1, false).unwrap();
        assert!(!plug.on.get());

        let stuck = Fake {
            ignore_on: true,
            ..Fake::new(false)
        };
        let err = cmd_switch(&stuck, 1, true).unwrap_err();
        assert!(err.to_string().contains("device reports off"), "{err}");
    }

    // ── doctor, against a stub of the device's HTTP RPC API ─────────────

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A loopback HTTP server answering `GET /rpc/<Method>` from `routes`
    /// (method, HTTP status, JSON body; anything else is 404), recording
    /// every method it was asked for. It drives the real [`Client`], so the
    /// doctor checks run over the same HTTP path a plug would see.
    fn stub(routes: &[(&'static str, u16, &'static str)]) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let routes = routes.to_vec();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&req)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let path = line.split(' ').nth(1).unwrap_or("");
                let method = path
                    .strip_prefix("/rpc/")
                    .unwrap_or(path)
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .to_string();
                log.lock().unwrap().push(method.clone());
                let (code, body) = routes
                    .iter()
                    .find(|(m, _, _)| *m == method)
                    .map(|(_, c, b)| (*c, *b))
                    .unwrap_or((404, "{\"code\":404,\"message\":\"No handler\"}"));
                let _ = write!(
                    s,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (addr.to_string(), seen)
    }

    const INFO: &str = r#"{"name":null,"id":"shellyplusplugs-aa00bb11cc22","model":"SNPL-00112EU","gen":2,"ver":"1.4.4","auth_en":false}"#;
    const INFO_AUTH: &str = r#"{"id":"shellyplusplugs-aa00bb11cc22","model":"SNPL-00112EU","gen":2,"ver":"1.4.4","auth_en":true}"#;
    const ALL: &str =
        r#"{"sys":{},"wifi":{},"switch:1":{"output":true},"switch:0":{"output":false}}"#;

    fn statuses(checks: &[Check]) -> Vec<(&str, &str)> {
        checks.iter().map(|c| (c.name, c.status)).collect()
    }

    /// Never `Switch.Set`: the contract says a doctor check changes nothing.
    fn assert_read_only(seen: &Arc<Mutex<Vec<String>>>) {
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|m| m == "Switch.Set"), "{seen:?}");
    }

    #[test]
    fn doctor_reports_a_healthy_plug_and_lists_its_switches() {
        let (addr, seen) = stub(&[
            ("Shelly.GetDeviceInfo", 200, INFO),
            ("Shelly.GetStatus", 200, ALL),
        ]);
        let checks = doctor_checks(&Client::new(&addr), None);
        assert_eq!(
            statuses(&checks),
            [("reachable", "ok"), ("auth", "ok"), ("switch", "ok")]
        );
        assert!(checks[0].detail.contains("SNPL-00112EU"), "{checks:?}");
        assert_eq!(checks[2].detail, "switch:0, switch:1 present");
        assert_read_only(&seen);

        // The JSON is exactly the contract panio parses.
        let v = doctor_json(&checks);
        assert_eq!(v["checks"][2]["name"], "switch");
        assert_eq!(v["checks"][2]["status"], "ok");
        assert!(v["checks"][2]["detail"].is_string());
    }

    #[test]
    fn doctor_checks_one_named_switch() {
        let (addr, seen) = stub(&[
            ("Shelly.GetDeviceInfo", 200, INFO),
            ("Switch.GetStatus", 200, r#"{"id":0,"output":true}"#),
        ]);
        let checks = doctor_checks(&Client::new(&addr), Some(0));
        assert_eq!(
            checks[2],
            Check::ok("switch", "switch:0 present (currently on)")
        );
        assert_read_only(&seen);

        let (addr, _) = stub(&[
            ("Shelly.GetDeviceInfo", 200, INFO),
            (
                "Switch.GetStatus",
                404,
                r#"{"code":-105,"message":"Argument 'id', value 3 not found!"}"#,
            ),
        ]);
        let checks = doctor_checks(&Client::new(&addr), Some(3));
        assert_eq!(checks[2].status, "fail");
        assert!(checks[2].detail.contains("not found"), "{checks:?}");
    }

    #[test]
    fn doctor_fails_a_device_with_no_switch() {
        let (addr, _) = stub(&[
            ("Shelly.GetDeviceInfo", 200, INFO),
            ("Shelly.GetStatus", 200, r#"{"sys":{},"cover:0":{}}"#),
        ]);
        let checks = doctor_checks(&Client::new(&addr), None);
        assert_eq!(
            checks[2],
            Check::fail("switch", "the device has no switch component")
        );
    }

    #[test]
    fn doctor_stops_at_an_auth_enabled_device() {
        let (addr, seen) = stub(&[("Shelly.GetDeviceInfo", 200, INFO_AUTH)]);
        let checks = doctor_checks(&Client::new(&addr), None);
        assert_eq!(statuses(&checks), [("reachable", "ok"), ("auth", "fail")]);
        assert!(checks[1].detail.contains("requires authentication"));
        // It does not go on to call methods that would only answer 401.
        assert_eq!(*seen.lock().unwrap(), ["Shelly.GetDeviceInfo"]);
    }

    #[test]
    fn doctor_reports_an_unreachable_device_as_one_failure() {
        // A port nothing listens on: bind one, learn its number, drop it.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let checks = doctor_checks(&Client::new(&addr), None);
        assert_eq!(statuses(&checks), [("reachable", "fail")]);
        assert!(
            checks[0].detail.contains("cannot reach Shelly"),
            "{checks:?}"
        );
        // A Gen1 device (no /rpc) answers 404: reachable, but not a Gen2+ API.
        let (addr, _) = stub(&[]);
        let checks = doctor_checks(&Client::new(&addr), None);
        assert_eq!(statuses(&checks), [("reachable", "fail")]);
    }

    #[test]
    fn switch_components_are_listed_in_id_order() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"switch:10":{},"switch:2":{},"switch:x":{},"input:0":{}}"#)
                .unwrap();
        assert_eq!(switch_components(&v), ["switch:2", "switch:10"]);
    }
}
