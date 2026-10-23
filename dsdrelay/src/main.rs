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

//! dsdrelay — drive a DSD TECH SH-UR01A/SH-UR04A-style USB serial relay board.
//!
//! Subcommands:
//!   on <channel>      Power the target on and confirm by read-back.
//!   off <channel>     Power the target off and confirm by read-back.
//!   state <channel>   Print exactly `on` or `off`.
//!   cycle <channel>   off -> confirm -> delay -> on -> confirm.
//!   doctor [channel]  Read-only health report (`--json` for `panio doctor`).
//!   list              Enumerate candidate USB ports without opening any.

mod proto;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};

use proto::{
    onoff, open, ping, prefer_cu, query_coil, query_target, relay_ports, select_port,
    serial_warning, set_target, Candidate, Transport,
};

#[derive(Parser)]
#[command(
    name = "dsdrelay",
    version,
    about = "Power control for DSD TECH USB serial AT relay boards (targets on the NC contact)",
    long_about = "Power control for DSD TECH SH-UR01A (1-channel, CP2102N) and \
SH-UR04A (4-channel, older CP2102) USB serial relay boards (9600 8N1, \
AT+CHn commands, each terminated by CR LF).

WIRING (hardcoded)
  Targets are wired through the relay's NC (normally closed) contact, so the
  relay's coil is the inverse of the target:
    on  <ch>   releases the coil   (AT+CH<ch>=0)   target powered
    off <ch>   energizes the coil  (AT+CH<ch>=1)   target unpowered
  An unpowered board (USB unplugged, control host rebooting) leaves every
  target powered.

DEVICE
  Exactly one of --device (-d) or --serial (-s) is required, before the verb:
    dsdrelay -d /dev/cu.usbserial-0001 cycle 1
    dsdrelay -s AA00BB11CC22DD33 state 1
  --serial matches the chip's USB serial number, which survives re-plugging
  into another port (the /dev node name does not). Find it with `list`. Use it
  for the SH-UR01A, whose CP2102N has a unique serial. The SH-UR04A ships with
  the same factory serial (0001) as every other one, so either use --device
  for it (on Linux a /dev/serial/by-path/... name is tied to the physical USB
  port; on macOS the /dev/cu.usbserial-* node depends on the serial and USB
  topology), or rewrite its serial and then use --serial. Rewriting has worked
  but is at your own risk; see docs/power.md, \"Giving an SH-UR04A a unique
  serial\".

CHANNELS
  Channels are numbered from 1; a channel the board lacks is an error. A
  1-channel board has only channel 1; the SH-UR04A has channels 1 to 4.

on/off/cycle confirm by reading the state back, so a hook that silently
failed surfaces as a non-zero exit."
)]
struct Cli {
    /// Serial port path (e.g. /dev/cu.usbserial-XXXX, /dev/ttyUSB0, or a
    /// /dev/serial/by-id/ path). Give this or --serial.
    #[arg(short = 'd', long = "device", value_name = "PATH", global = true)]
    device: Option<String>,

    /// USB serial number of the board's CP2102/CP2102N (see `list`); selects
    /// the port by identity instead of by path. Only for boards with a
    /// unique serial: the SH-UR01A, or an SH-UR04A whose factory 0001 was
    /// rewritten (otherwise use --device). Give this or --device.
    #[arg(short = 's', long = "serial", value_name = "USB-SERIAL", global = true)]
    serial: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print exactly `on` or `off` for a channel's target (hook: state_cmd).
    /// Any other reading is an error carrying the raw reply.
    State {
        /// Relay channel (1 or higher).
        #[arg(value_parser = clap::value_parser!(u8).range(1..))]
        channel: u8,
    },
    /// Power the target on: release the coil (NC wiring), confirm by
    /// read-back (hook: on_cmd).
    On {
        /// Relay channel (1 or higher).
        #[arg(value_parser = clap::value_parser!(u8).range(1..))]
        channel: u8,
    },
    /// Power the target off: energize the coil (NC wiring), confirm by
    /// read-back (hook: off_cmd).
    Off {
        /// Relay channel (1 or higher).
        #[arg(value_parser = clap::value_parser!(u8).range(1..))]
        channel: u8,
    },
    /// Power-cycle: off -> confirm -> delay -> on -> confirm (hook:
    /// cycle_cmd).
    Cycle {
        /// Relay channel (1 or higher).
        #[arg(value_parser = clap::value_parser!(u8).range(1..))]
        channel: u8,
        /// Milliseconds to hold the target off before restoring power.
        #[arg(long, default_value_t = 3000)]
        delay_ms: u64,
    },
    /// Read-only health check (panio doctor): is the port usable, does the
    /// board answer `AT`, and what state is the channel in. Never switches
    /// the relay.
    ///
    /// Checks `device`, `responds` and `channel` (default channel 1).
    /// `--json` prints panio's doctor report
    /// (`{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}`) and
    /// exits 0 whatever the checks found; without it, one line per check and
    /// exit 1 if any failed.
    Doctor {
        /// Channel to check (default 1).
        #[arg(value_parser = clap::value_parser!(u8).range(1..))]
        channel: Option<u8>,
        /// Print the JSON report panio doctor reads.
        #[arg(long)]
        json: bool,
    },
    /// List USB serial ports that use the boards' CP2102/CP2102N chip (VID
    /// 10c4, PID ea60): path, USB serial number, product, manufacturer. One
    /// line per device (the macOS /dev/tty.* twin of a /dev/cu.* node is
    /// dropped); warns on stderr when serial numbers are not unique.
    ///
    /// Enumeration only: `list` never opens a port. That matters because a
    /// Sonoff ZBDongle-P Zigbee coordinator has the same VID:PID, and
    /// opening its port can reset its CC2652 into the bootloader. Pick the
    /// relay by its USB serial number, and do not probe other entries.
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => cmd_list(),
        Cmd::Doctor { channel, json } => {
            let channel = channel.unwrap_or(1);
            let opened = resolve_and_open(cli.device.as_deref(), cli.serial.as_deref());
            let checks = doctor_checks(opened, channel);
            report_doctor(&checks, json)
        }
        Cmd::State { channel } => {
            let mut t = connect(&cli)?;
            println!("{}", onoff(query_target(&mut t, channel)?));
            Ok(())
        }
        Cmd::On { channel } => {
            let mut t = connect(&cli)?;
            set_target(&mut t, channel, true)?;
            println!("channel {channel}: on");
            Ok(())
        }
        Cmd::Off { channel } => {
            let mut t = connect(&cli)?;
            set_target(&mut t, channel, false)?;
            println!("channel {channel}: off");
            Ok(())
        }
        Cmd::Cycle { channel, delay_ms } => {
            let mut t = connect(&cli)?;
            cycle(
                &mut t,
                channel,
                Duration::from_millis(delay_ms),
                thread::sleep,
            )?;
            println!("channel {channel}: cycled (off {delay_ms} ms, now on)");
            Ok(())
        }
    }
}

/// A resolved, opened port: its path (for reports) and the transport.
type Opened = (String, proto::SerialTransport);

fn resolve_and_open(device: Option<&str>, serial: Option<&str>) -> Result<Opened> {
    // Only a serial-number lookup needs the (non-opening) enumeration.
    let ports = if serial.is_some() {
        relay_ports()?
    } else {
        Vec::new()
    };
    let path = select_port(&ports, device, serial)?;
    let t = open(&path)?;
    Ok((path, t))
}

fn connect(cli: &Cli) -> Result<proto::SerialTransport> {
    resolve_and_open(cli.device.as_deref(), cli.serial.as_deref()).map(|(_, t)| t)
}

/// off -> confirm -> hold -> on -> confirm. `sleep` is injected so tests do
/// not wait. Both phases are confirmed, so a relay that ignored `off` is not
/// reported as a successful cycle.
fn cycle<T: Transport>(
    t: &mut T,
    channel: u8,
    delay: Duration,
    sleep: impl FnOnce(Duration),
) -> Result<()> {
    set_target(t, channel, false)?;
    sleep(delay);
    set_target(t, channel, true).map_err(|e| {
        anyhow!("{e:#} (the cycle's power-on phase failed; the target may be left off)")
    })
}

fn cmd_list() -> Result<()> {
    let ports = relay_ports()?;
    if ports.is_empty() {
        bail!("no CP2102N USB serial ports (VID 10c4, PID ea60) found");
    }
    let shown = prefer_cu(ports.iter().collect());
    print!("{}", format_list(&shown));
    if let Some(w) = serial_warning(&shown) {
        eprintln!("{w}");
    }
    Ok(())
}

fn format_list(ports: &[&Candidate]) -> String {
    let dash = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".to_string());
    let mut out = String::new();
    for p in ports {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            p.path,
            dash(&p.serial),
            dash(&p.product),
            dash(&p.manufacturer)
        ));
    }
    out
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

/// The read-only checks, in order; each needs the one before it, so a
/// failure ends the list. Only `AT` and `AT+CHn=?` are sent, never a set.
fn doctor_checks<T: Transport>(opened: Result<(String, T)>, channel: u8) -> Vec<Check> {
    let (path, mut t) = match opened {
        Ok(o) => o,
        Err(e) => return vec![Check::fail("device", format!("{e:#}"))],
    };
    let mut checks = vec![Check::ok("device", format!("{path} opened at 9600 8N1"))];
    if let Err(e) = ping(&mut t) {
        checks.push(Check::fail("responds", format!("{e:#}")));
        return checks;
    }
    checks.push(Check::ok("responds", "AT -> OK"));
    checks.push(match query_coil(&mut t, channel) {
        Ok(true) => Check::ok(
            "channel",
            format!("channel {channel}: relay energized, target off (NC)"),
        ),
        Ok(false) => Check::ok(
            "channel",
            format!("channel {channel}: relay released, target on (NC)"),
        ),
        Err(e) => Check::fail("channel", format!("{e:#}")),
    });
    checks
}

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

/// Escape a string as a JSON string literal (no serde_json dependency).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn doctor_json(checks: &[Check]) -> String {
    let items: Vec<String> = checks
        .iter()
        .map(|c| {
            format!(
                "{{\"name\": {}, \"status\": {}, \"detail\": {}}}",
                json_string(c.name),
                json_string(c.status),
                json_string(&c.detail)
            )
        })
        .collect();
    format!("{{\"checks\": [{}]}}", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::proto::fake::FakeRelay;
    use super::*;
    use std::cell::Cell;

    fn opened(d: FakeRelay) -> Result<(String, FakeRelay)> {
        Ok(("/dev/fake0".to_string(), d))
    }

    /// Parse the minimal JSON `doctor_json` emits, enough to check shape.
    fn names_and_statuses(json: &str) -> Vec<(String, String)> {
        let body = json
            .strip_prefix("{\"checks\": [")
            .and_then(|s| s.strip_suffix("]}"))
            .expect("report shape");
        body.split("}, {")
            .map(|item| {
                let field = |key: &str| {
                    let start = item.find(&format!("\"{key}\": \"")).unwrap() + key.len() + 5;
                    let rest = &item[start..];
                    rest[..rest.find('"').unwrap()].to_string()
                };
                (field("name"), field("status"))
            })
            .collect()
    }

    #[test]
    fn cycle_runs_both_phases_and_ends_on() {
        let mut d = FakeRelay::new(1);
        let slept = Cell::new(None);
        cycle(&mut d, 1, Duration::from_millis(5), |x| slept.set(Some(x))).unwrap();
        assert_eq!(slept.get(), Some(Duration::from_millis(5)));
        assert_eq!(
            d.log,
            ["AT+CH1=1", "AT+CH1=?", "AT+CH1=0", "AT+CH1=?"],
            "off, confirm, on, confirm"
        );
        assert!(!d.coils[0], "cycle must end with the coil released");
    }

    #[test]
    fn cycle_with_a_board_that_ignores_off_fails_before_holding() {
        let mut d = FakeRelay::new(1);
        d.ignore_energize = true;
        let slept = Cell::new(false);
        let e = cycle(&mut d, 1, Duration::ZERO, |_| slept.set(true)).unwrap_err();
        assert!(e.to_string().contains("commanded off"), "{e}");
        assert!(!slept.get(), "must not report a hold that never happened");
    }

    #[test]
    fn cycle_on_a_missing_channel_fails() {
        let mut d = FakeRelay::new(1);
        let e = cycle(&mut d, 2, Duration::ZERO, |_| {}).unwrap_err();
        assert!(e.to_string().contains("channel 2 not present"), "{e}");
    }

    #[test]
    fn cycle_power_on_failure_warns_target_may_be_off() {
        let mut d = FakeRelay::new(1);
        d.ignore_all_sets = true;
        // Pre-energize so the off phase "confirms", then on is ignored.
        d.coils[0] = true;
        let e = cycle(&mut d, 1, Duration::ZERO, |_| {}).unwrap_err();
        assert!(e.to_string().contains("target may be left off"), "{e}");
    }

    struct Borrowed<'a>(&'a mut FakeRelay);
    impl std::io::Read for Borrowed<'_> {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(b)
        }
    }
    impl std::io::Write for Borrowed<'_> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Transport for Borrowed<'_> {
        fn clear_input(&mut self) -> std::io::Result<()> {
            self.0.clear_input()
        }
        fn settle(&mut self, _d: Duration) {}
    }

    #[test]
    fn doctor_never_executes_a_leftover_command() {
        for (seed, coil) in [(&b"AT+CH1=1"[..], false), (&b"AT+CH1=0"[..], true)] {
            let mut d = FakeRelay::new(1);
            d.coils[0] = coil;
            d.inbuf.extend_from_slice(seed);
            let checks = doctor_checks(Ok(("/dev/fake0".to_string(), Borrowed(&mut d))), 1);
            assert!(checks.iter().all(|c| c.status == "ok"), "after {seed:?}");
            assert_eq!(d.coils, [coil], "doctor after {seed:?}");
        }
    }

    #[test]
    fn doctor_reports_a_healthy_released_relay() {
        let checks = doctor_checks(opened(FakeRelay::new(1)), 1);
        let got: Vec<_> = checks.iter().map(|c| (c.name, c.status)).collect();
        assert_eq!(
            got,
            [("device", "ok"), ("responds", "ok"), ("channel", "ok")]
        );
        assert_eq!(
            checks[2].detail,
            "channel 1: relay released, target on (NC)"
        );
    }

    #[test]
    fn doctor_reports_an_energized_relay_and_never_sets() {
        let mut d = FakeRelay::new(1);
        d.coils[0] = true;
        let (_, d) = {
            let mut o = opened(d).unwrap();
            let checks = doctor_checks(Ok((o.0.clone(), &mut o.1)), 1);
            assert_eq!(
                checks[2].detail,
                "channel 1: relay energized, target off (NC)"
            );
            o
        };
        assert_eq!(d.log, ["AT", "AT+CH1=?"], "doctor must only read");
    }

    #[test]
    fn doctor_flags_a_missing_channel() {
        let checks = doctor_checks(opened(FakeRelay::new(1)), 4);
        assert_eq!(checks[2].status, "fail");
        assert!(checks[2].detail.contains("channel 4 not present"));
    }

    #[test]
    fn doctor_stops_at_a_silent_port() {
        let mut d = FakeRelay::new(1);
        d.canned = Some(Vec::new());
        let checks = doctor_checks(opened(d), 1);
        let got: Vec<_> = checks.iter().map(|c| (c.name, c.status)).collect();
        assert_eq!(got, [("device", "ok"), ("responds", "fail")]);
    }

    #[test]
    fn doctor_reports_an_unopenable_device() {
        let checks = doctor_checks::<FakeRelay>(Err(anyhow!("cannot open /dev/nope")), 1);
        assert_eq!(checks.len(), 1);
        assert_eq!((checks[0].name, checks[0].status), ("device", "fail"));
        assert!(checks[0].detail.contains("cannot open /dev/nope"));
    }

    #[test]
    fn doctor_json_has_the_contract_shape() {
        let checks = doctor_checks(opened(FakeRelay::new(1)), 1);
        let json = doctor_json(&checks);
        assert_eq!(
            names_and_statuses(&json),
            [
                ("device".to_string(), "ok".to_string()),
                ("responds".to_string(), "ok".to_string()),
                ("channel".to_string(), "ok".to_string()),
            ]
        );
        assert!(json.contains("\"detail\": \"channel 1: relay released, target on (NC)\""));
    }

    #[test]
    fn doctor_json_escapes_awkward_detail() {
        let checks = [Check::fail("device", "bad \"quote\" \\ and\nnewline \u{1}")];
        assert_eq!(
            doctor_json(&checks),
            "{\"checks\": [{\"name\": \"device\", \"status\": \"fail\", \
             \"detail\": \"bad \\\"quote\\\" \\\\ and\\nnewline \\u0001\"}]}"
        );
    }

    #[test]
    fn list_formats_one_line_per_port_with_dashes_for_gaps() {
        let ports = [&Candidate {
            path: "/dev/cu.usbserial-0001".into(),
            vid: proto::USB_VID,
            pid: proto::USB_PID,
            serial: Some("AA00BB11CC22DD33".into()),
            product: None,
            manufacturer: Some("Silicon Labs".into()),
        }];
        assert_eq!(
            format_list(&ports),
            "/dev/cu.usbserial-0001\tAA00BB11CC22DD33\t-\tSilicon Labs\n"
        );
    }

    #[test]
    fn list_drops_the_tty_twin_of_a_cu_node() {
        let mk = |path: &str| Candidate {
            path: path.into(),
            vid: proto::USB_VID,
            pid: proto::USB_PID,
            serial: Some("0001".into()),
            product: None,
            manufacturer: None,
        };
        let all = [
            mk("/dev/tty.usbserial-0001"),
            mk("/dev/cu.usbserial-0001"),
            mk("/dev/tty.usbserial-1"),
        ];
        let shown = prefer_cu(all.iter().collect());
        let text = format_list(&shown);
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(!text.contains("/dev/tty.usbserial-0001"), "{text}");
    }
}
