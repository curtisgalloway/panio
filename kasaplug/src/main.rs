// Copyright 2026 Bayanasar Khurd
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

//! kasaplug — power control for TP-Link Kasa smart plugs and power strips.
//!
//! A one-shot panio power helper: each invocation makes a few stateless
//! requests to the device's legacy local API and exits. Hook-facing
//! subcommands follow the panio helper conventions
//! (docs/dev/adding-power-helpers.md):
//!   state <id>    prints exactly `on` or `off`
//!   on/off <id>   switch + read-back confirm
//!   cycle <id>    off → confirm → delay → on → confirm
//!   doctor --json read-only health report for `panio doctor`

mod proto;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};

use proto::{relay_on, Client};

/// Why every hook will fail when TCP 9999 does not speak the protocol.
const NO_LOCAL_API: &str = "not a Kasa device, or its local API is off \
                            (Kasa app: Third-Party Integration); every hook will fail";

#[derive(Parser)]
#[command(
    name = "kasaplug",
    version,
    about = "Power control for TP-Link Kasa smart plugs and power strips over the legacy local protocol",
    long_about = "Power control for TP-Link Kasa smart plugs and power strips (HS, KP, EP \
series) over the device's legacy local protocol on TCP 9999 — no cloud, no TP-Link \
account, no Home Assistant.

MENTAL MODEL
  - A device is addressed by its address on your network: pass --device with an
    IP or hostname, optionally with a port (192.0.2.10, 192.0.2.10:9999). A DHCP
    reservation keeps the hook stable across reboots.
  - An outlet is addressed by its index in the device's own outlet list (the
    positional [ID], default 0). Single-outlet plugs only have id 0; a strip
    uses 0..N-1, so the Kasa app's \"Plug 5\" is id 4.
  - on/off/cycle confirm by reading the relay state back, so a hook that
    silently failed surfaces as a non-zero exit.

TYPICAL USE
  kasaplug -d 192.0.2.10 status        device info + outlet state and power
  kasaplug -d 192.0.2.10 state         prints exactly `on` or `off`
  kasaplug -d 192.0.2.10 on|off [id]
  kasaplug -d 192.0.2.10 cycle [id] [--delay-ms 3000]
  kasaplug -d 192.0.2.10 doctor [id] [--json]   read-only health check

Only the legacy local API is supported. Newer firmware on some models turns it
off (the device then refuses TCP 9999); the Kasa app's Third-Party Integration
setting turns it back on."
)]
struct Cli {
    /// Device address: IP or hostname, optionally with a port
    /// (e.g. 192.0.2.10, 192.0.2.10:9999, plug.local).
    #[arg(short = 'd', long = "device", value_name = "HOST", global = true)]
    device: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print exactly `on` or `off` for an outlet (hook: state_cmd).
    State {
        /// Outlet id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Switch on and confirm by read-back (hook: on_cmd).
    On {
        /// Outlet id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Switch off and confirm by read-back (hook: off_cmd).
    Off {
        /// Outlet id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Power-cycle: off → confirm → delay → on → confirm (hook: cycle_cmd).
    Cycle {
        /// Outlet id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
        /// Milliseconds to hold the outlet off before restoring power.
        #[arg(long, default_value_t = 3000)]
        delay_ms: u64,
    },
    /// Human-readable device info plus an outlet's state and power metering.
    Status {
        /// Outlet id (default 0).
        #[arg(default_value_t = 0)]
        id: u32,
    },
    /// Read-only health check (panio doctor): is the device reachable, does
    /// it answer the local API without a login, and does it have the outlet.
    /// Never switches an outlet.
    ///
    /// Without an id it lists every outlet the device has; with one it checks
    /// that outlet. `--json` prints panio's doctor report
    /// (`{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}`) and
    /// exits 0 whatever the checks found; without it, one line per check and
    /// exit 1 if any failed.
    Doctor {
        /// Outlet id to check (default: list them all).
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

/// True when the error chain holds a refused connection: the host answered,
/// but nothing listens on the port.
fn refused(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
    })
}

/// The read-only health checks, in order; each later check needs the one
/// before it to have passed, so a failure ends the list. Only
/// `system.get_sysinfo` is sent — never `set_relay_state`.
fn doctor_checks(client: &Client, id: Option<u32>) -> Vec<Check> {
    match client.connect() {
        Ok(_) => {}
        Err(e) if refused(&e) => {
            return vec![
                Check::ok(
                    "reachable",
                    format!("host answers, but {} refused the connection", client.addr()),
                ),
                Check::fail("auth", NO_LOCAL_API),
            ]
        }
        Err(e) => return vec![Check::fail("reachable", format!("{e:#}"))],
    }
    let info = match client.sysinfo() {
        Ok(info) => info,
        Err(e) => {
            return vec![
                Check::ok(
                    "reachable",
                    format!("{} accepted the connection", client.addr()),
                ),
                Check::fail("auth", format!("{NO_LOCAL_API} ({e:#})")),
            ]
        }
    };
    let mut checks = vec![
        Check::ok(
            "reachable",
            format!(
                "{} {}, hardware {}, firmware {}",
                info.model, info.device_id, info.hw_ver, info.sw_ver
            ),
        ),
        Check::ok("auth", "no login needed (legacy local API)"),
    ];
    checks.push(match id {
        Some(id) => match info.outlet(id) {
            Ok(o) => match relay_on(&o.state) {
                Ok(on) => Check::ok(
                    "switch",
                    format!("outlet {id} present (currently {})", onoff(on)),
                ),
                Err(e) => Check::fail("switch", format!("outlet {id}: {e:#}")),
            },
            Err(e) => Check::fail("switch", format!("{e:#}")),
        },
        None => match info.outlets.len() {
            0 => Check::fail("switch", "the device has no switchable outlet"),
            1 => Check::ok("switch", "outlet 0 present"),
            n => Check::ok("switch", format!("outlets 0..{} present", n - 1)),
        },
    });
    checks
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

/// An outlet's relay state, read live.
fn is_on(client: &Client, id: u32) -> Result<bool> {
    relay_on(&client.sysinfo()?.outlet(id)?.state)
}

fn cmd_state(client: &Client, id: u32) -> Result<()> {
    println!("{}", onoff(is_on(client, id)?));
    Ok(())
}

/// Command the outlet and confirm by read-back, failing on a mismatch.
fn set_confirmed(client: &Client, id: u32, on: bool) -> Result<()> {
    let info = client.sysinfo()?;
    client.set_relay(info.outlet(id)?, on)?;
    let now = is_on(client, id)?;
    if now != on {
        bail!(
            "outlet {id}: commanded {} but device reports {}",
            onoff(on),
            onoff(now)
        );
    }
    Ok(())
}

fn cmd_switch(client: &Client, id: u32, on: bool) -> Result<()> {
    set_confirmed(client, id, on)?;
    println!("outlet {id}: {}", onoff(on));
    Ok(())
}

/// off → confirm → delay → on → confirm. The off phase is confirmed before
/// the hold starts: an outlet that ignored the off command would otherwise
/// produce a "cycle" that never removed power, with the final read-back
/// happily reporting on.
fn cmd_cycle(client: &Client, id: u32, delay_ms: u64) -> Result<()> {
    set_confirmed(client, id, false)
        .map_err(|e| anyhow!("{e} (cycle aborted before the off-hold)"))?;
    thread::sleep(Duration::from_millis(delay_ms));
    set_confirmed(client, id, true).map_err(|e| anyhow!("{e} (after the off-hold)"))?;
    println!("outlet {id}: cycled ({delay_ms} ms off)");
    Ok(())
}

fn cmd_status(client: &Client, id: u32) -> Result<()> {
    let info = client.sysinfo()?;
    println!("device   {} — {}", info.device_id, info.model);
    println!("name     {}", info.alias);
    println!("hw/fw    hardware {}, {}", info.hw_ver, info.sw_ver);
    println!("auth     none (legacy local API)");

    let outlet = info.outlet(id)?;
    println!(
        "outlet {id}  {}  {}",
        onoff(relay_on(&outlet.state)?),
        outlet.alias
    );
    if let Some(rt) = client.realtime(outlet)? {
        if let Some(w) = rt.watts {
            println!("  power    {w:.1} W");
        }
        if let Some(v) = rt.volts {
            println!("  voltage  {v:.1} V");
        }
        if let Some(a) = rt.amps {
            println!("  current  {a:.3} A");
        }
        if let Some(e) = rt.watt_hours {
            println!("  energy   {e:.1} Wh total");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{decrypt, encrypt};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    // ── a loopback device speaking the legacy protocol ─────────────────

    /// The stub device's relays and the faults to inject.
    struct Device {
        /// `false` answers as a single-outlet plug (`relay_state`), `true` as
        /// a strip (`children`).
        strip: bool,
        states: Vec<Value>,
        ignore_on: bool,
        ignore_off: bool,
        /// Every request, as `method` or `method@<child suffix>`.
        seen: Vec<String>,
    }

    const DEVICE_ID: &str = "AA00BB11CC22DD33";

    /// A loopback TCP server speaking the legacy protocol (length framing,
    /// XOR autokey) and answering `get_sysinfo`, `set_relay_state` (with
    /// `context.child_ids`) and `emeter.get_realtime`. It drives the real
    /// [`Client`], so the commands run over the same wire path a strip sees.
    fn stub(strip: bool, states: &[Value]) -> (Client, Arc<Mutex<Device>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let dev = Arc::new(Mutex::new(Device {
            strip,
            states: states.to_vec(),
            ignore_on: false,
            ignore_off: false,
            seen: Vec::new(),
        }));
        let shared = Arc::clone(&dev);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut len = [0u8; 4];
                if s.read_exact(&mut len).is_err() {
                    continue;
                }
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                if s.read_exact(&mut body).is_err() {
                    continue;
                }
                let req: Value = serde_json::from_slice(&decrypt(&body)).unwrap();
                let reply = answer(&mut shared.lock().unwrap(), &req);
                let enc = encrypt(reply.to_string().as_bytes());
                let _ = s.write_all(&(enc.len() as u32).to_be_bytes());
                let _ = s.write_all(&enc);
            }
        });
        (Client::new(&addr), dev)
    }

    fn answer(dev: &mut Device, req: &Value) -> Value {
        let child = req["context"]["child_ids"][0]
            .as_str()
            .map(|c| c[DEVICE_ID.len()..].to_string());
        let idx = child.as_deref().map_or(0, |n| n.parse().unwrap());
        let tag = |m: &str| match &child {
            Some(c) => format!("{m}@{c}"),
            None => m.to_string(),
        };
        if req["system"].get("get_sysinfo").is_some() {
            dev.seen.push(tag("get_sysinfo"));
            let mut info = json!({
                "model": if dev.strip { "HS300(US)" } else { "HS103(US)" },
                "hw_ver": "2.0", "sw_ver": "1.1.2", "alias": "bench strip",
                "deviceId": DEVICE_ID, "err_code": 0
            });
            if dev.strip {
                info["children"] = dev
                    .states
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        json!({"id": format!("{DEVICE_ID}{i:02}"), "state": s,
                               "alias": format!("Plug {}", i + 1)})
                    })
                    .collect();
            } else {
                info["relay_state"] = dev.states[0].clone();
            }
            return json!({"system": {"get_sysinfo": info}});
        }
        if let Some(set) = req["system"].get("set_relay_state") {
            dev.seen.push(tag("set_relay_state"));
            let on = set["state"] == 1;
            if !(if on { dev.ignore_on } else { dev.ignore_off }) {
                dev.states[idx] = json!(u8::from(on));
            }
            return json!({"system": {"set_relay_state": {"err_code": 0}}});
        }
        if req["emeter"].get("get_realtime").is_some() {
            dev.seen.push(tag("get_realtime"));
            return json!({"emeter": {"get_realtime": {
                "power_mw": 3095, "voltage_mv": 120716, "current_ma": 50,
                "total_wh": 12, "err_code": 0
            }}});
        }
        json!({"err_code": -1, "err_msg": "module not support"})
    }

    fn strip(states: &[i64]) -> (Client, Arc<Mutex<Device>>) {
        let states: Vec<Value> = states.iter().map(|s| json!(s)).collect();
        stub(true, &states)
    }

    fn seen(dev: &Arc<Mutex<Device>>) -> Vec<String> {
        std::mem::take(&mut dev.lock().unwrap().seen)
    }

    fn states(dev: &Arc<Mutex<Device>>) -> Vec<Value> {
        dev.lock().unwrap().states.clone()
    }

    // ── state / on / off / cycle ───────────────────────────────────────

    #[test]
    fn state_reads_the_named_outlet_live() {
        let (c, _) = strip(&[1, 0, 1]);
        assert!(is_on(&c, 0).unwrap());
        assert!(!is_on(&c, 1).unwrap());
        let err = is_on(&c, 3).unwrap_err().to_string();
        assert!(err.contains("outlet 3 not found"), "{err}");

        let (plug, _) = stub(false, &[json!(1)]);
        assert!(is_on(&plug, 0).unwrap());
        assert!(is_on(&plug, 1).is_err());
    }

    #[test]
    fn an_unknown_state_is_an_error_carrying_the_raw_value() {
        let (c, _) = stub(true, &[json!(1), json!(7)]);
        let err = is_on(&c, 1).unwrap_err().to_string();
        assert!(err.contains("unrecognized relay state 7"), "{err}");
    }

    #[test]
    fn on_and_off_confirm_by_read_back() {
        let (c, dev) = strip(&[0, 0, 0]);
        cmd_switch(&c, 2, true).unwrap();
        assert_eq!(
            seen(&dev),
            ["get_sysinfo", "set_relay_state@02", "get_sysinfo"]
        );
        assert_eq!(states(&dev), [json!(0), json!(0), json!(1)]);
        cmd_switch(&c, 2, false).unwrap();
        assert_eq!(states(&dev)[2], json!(0));

        let (plug, dev) = stub(false, &[json!(0)]);
        cmd_switch(&plug, 0, true).unwrap();
        // A plug's own relay is addressed without a context.
        assert_eq!(
            seen(&dev),
            ["get_sysinfo", "set_relay_state", "get_sysinfo"]
        );
    }

    #[test]
    fn a_read_back_mismatch_fails() {
        let (c, dev) = strip(&[0, 0]);
        dev.lock().unwrap().ignore_on = true;
        let err = cmd_switch(&c, 1, true).unwrap_err().to_string();
        assert!(
            err.contains("outlet 1: commanded on but device reports off"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_outlet_is_refused_before_switching() {
        let (c, dev) = strip(&[1, 1]);
        let err = cmd_cycle(&c, 5, 0).unwrap_err().to_string();
        assert!(err.contains("outlet 5 not found"), "{err}");
        assert_eq!(seen(&dev), ["get_sysinfo"]);
    }

    #[test]
    fn cycle_confirms_off_then_on() {
        let (c, dev) = strip(&[1, 1, 1, 1, 1, 1]);
        cmd_cycle(&c, 4, 0).unwrap();
        assert_eq!(
            seen(&dev),
            [
                "get_sysinfo",
                "set_relay_state@04",
                "get_sysinfo",
                "get_sysinfo",
                "set_relay_state@04",
                "get_sysinfo"
            ]
        );
        assert_eq!(states(&dev)[4], json!(1));
    }

    #[test]
    fn cycle_aborts_before_the_hold_when_off_did_not_take() {
        let (c, dev) = strip(&[1, 1]);
        dev.lock().unwrap().ignore_off = true;
        let err = cmd_cycle(&c, 0, 0)
            .expect_err("an outlet stuck on must fail the cycle")
            .to_string();
        assert!(err.contains("commanded off"), "{err}");
        assert!(err.contains("cycle aborted before the off-hold"), "{err}");
        // It must stop right there: no power-on was ever sent.
        assert_eq!(
            seen(&dev),
            ["get_sysinfo", "set_relay_state@00", "get_sysinfo"]
        );
    }

    #[test]
    fn cycle_fails_when_on_did_not_take() {
        let (c, dev) = strip(&[1]);
        dev.lock().unwrap().ignore_on = true;
        let err = cmd_cycle(&c, 0, 0).unwrap_err().to_string();
        assert!(err.contains("commanded on"), "{err}");
        assert!(err.contains("after the off-hold"), "{err}");
        assert_eq!(states(&dev)[0], json!(0));
    }

    #[test]
    fn status_reads_the_outlet_and_its_meter() {
        let (c, dev) = strip(&[1, 0]);
        cmd_status(&c, 1).unwrap();
        assert_eq!(seen(&dev), ["get_sysinfo", "get_realtime@01"]);
    }

    // ── doctor ─────────────────────────────────────────────────────────

    fn statuses(checks: &[Check]) -> Vec<(&str, &str)> {
        checks.iter().map(|c| (c.name, c.status)).collect()
    }

    /// Never `set_relay_state`: the contract says a doctor check changes
    /// nothing.
    fn assert_read_only(dev: &Arc<Mutex<Device>>) {
        let seen = seen(dev);
        assert!(
            !seen.iter().any(|m| m.starts_with("set_relay_state")),
            "{seen:?}"
        );
    }

    #[test]
    fn doctor_reports_a_healthy_strip_and_lists_its_outlets() {
        let (c, dev) = strip(&[1, 0, 1]);
        let checks = doctor_checks(&c, None);
        assert_eq!(
            statuses(&checks),
            [("reachable", "ok"), ("auth", "ok"), ("switch", "ok")]
        );
        assert!(checks[0].detail.contains("HS300(US)"), "{checks:?}");
        assert_eq!(checks[2].detail, "outlets 0..2 present");
        assert_read_only(&dev);

        // The JSON is exactly the contract panio parses.
        let v = doctor_json(&checks);
        assert_eq!(v["checks"][2]["name"], "switch");
        assert_eq!(v["checks"][2]["status"], "ok");
        assert!(v["checks"][2]["detail"].is_string());
    }

    #[test]
    fn doctor_checks_one_named_outlet() {
        let (c, dev) = strip(&[1, 0]);
        let checks = doctor_checks(&c, Some(1));
        assert_eq!(
            checks[2],
            Check::ok("switch", "outlet 1 present (currently off)")
        );
        let checks = doctor_checks(&c, Some(3));
        assert_eq!(checks[2].status, "fail");
        assert!(checks[2].detail.contains("not found"), "{checks:?}");
        assert_read_only(&dev);

        let (plug, _) = stub(false, &[json!(0)]);
        assert_eq!(
            doctor_checks(&plug, None)[2],
            Check::ok("switch", "outlet 0 present")
        );
    }

    #[test]
    fn doctor_fails_auth_when_the_local_api_is_off() {
        // A port nothing listens on: bind one, learn its number, drop it.
        // The host answers with a refusal, as a device with the legacy API
        // turned off does.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let checks = doctor_checks(&Client::new(&format!("127.0.0.1:{port}")), None);
        assert_eq!(statuses(&checks), [("reachable", "ok"), ("auth", "fail")]);
        assert!(checks[0].detail.contains("refused"), "{checks:?}");
        assert!(
            checks[1].detail.contains("Third-Party Integration"),
            "{checks:?}"
        );
        // Still the contract's shape: exit 0 with --json, 1 without.
        assert!(report_doctor(&checks, true).is_ok());
        assert!(report_doctor(&checks, false).is_err());
    }

    #[test]
    fn doctor_fails_auth_when_the_port_does_not_speak_the_protocol() {
        // Accepts, then hangs up without a reply.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for s in listener.incoming() {
                drop(s);
            }
        });
        let checks = doctor_checks(&Client::new(&addr), None);
        assert_eq!(statuses(&checks), [("reachable", "ok"), ("auth", "fail")]);
        assert!(checks[1].detail.contains("not a Kasa device"), "{checks:?}");
    }

    #[test]
    fn doctor_reports_an_unresolvable_device_as_one_failure() {
        // `.invalid` never resolves (RFC 2606).
        let checks = doctor_checks(&Client::new("kasa.invalid"), None);
        assert_eq!(statuses(&checks), [("reachable", "fail")]);
    }
}
