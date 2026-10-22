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

//! dsdble — drive a DSD TECH SH-BT01E-style Bluetooth LE relay board.
//!
//! Subcommands:
//!   on <channel>      Power the target on (one BLE write; not confirmed).
//!   off <channel>     Power the target off (one BLE write; not confirmed).
//!   state <channel>   Always fails: the board cannot report its state.
//!   cycle <channel>   off -> delay -> on.
//!   doctor [channel]  Read-only health report (`--json` for `panio doctor`).
//!   scan              List peripherals advertising the relay's service.
//!
//! The command logic is written against the small [`Relay`] trait so tests
//! drive it with a fake that records frames; [`ble`] is the btleplug glue.

mod ble;
mod proto;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};

use proto::{parse_channel, target_frame};

/// Where frames go. The real implementation is a BLE connection; tests use a
/// fake that records what was written.
pub trait Relay {
    /// Write one command frame, returning once the board acknowledged the
    /// write (write-with-response). That acknowledgement is all the feedback
    /// the board gives.
    fn write_frame(&mut self, frame: [u8; 4]) -> Result<()>;
}

#[derive(Parser)]
#[command(
    name = "dsdble",
    version,
    about = "Power control for DSD TECH SH-BT01E Bluetooth LE relay boards (targets on the NC contact, no read-back)",
    long_about = "Power control for DSD TECH SH-BT01E Bluetooth LE relay boards \
(1 channel; 2- and 4-channel variants use the same protocol). Each command is \
a 4-byte frame written to GATT characteristic FFE1 of service FFE0.

WIRING (hardcoded)
  Targets are wired through the relay's NC (normally closed) contact, so the
  relay's coil is the inverse of the target:
    on  <ch>   releases the coil   target powered
    off <ch>   energizes the coil  target unpowered
  An unpowered board leaves every target powered.

DEVICE / ADDRESS
  --address (-a) is required before the verb for on, off, state, cycle and
  doctor:
    dsdble -a 00:11:22:33:44:55 cycle 1                      (Linux: the MAC)
    dsdble -a 00000000-0000-4000-8000-000000000001 cycle 1   (macOS: UUID)
  On Linux the address is the board's Bluetooth MAC. On macOS CoreBluetooth
  hides MACs and gives each peripheral a UUID that differs per Mac; take it
  from `dsdble scan` on the same machine. Matching is case-insensitive. All
  boards advertise the same name (`DSD TECH`) and the same factory serial, so
  the address is the only way to tell two boards apart.

  Find boards with `dsdble scan` (read-only: it connects to nothing).

PLATFORMS
  Linux (BlueZ over D-Bus) is the supported platform. macOS is best effort:
  the binary works only when launched from a terminal app that has been
  granted Bluetooth permission (System Settings, Privacy & Security,
  Bluetooth); an unbundled CLI started any other way, including over SSH or
  from a power hook, is killed by the system. Use it from a Linux control
  host. Windows builds but is untested.

CHANNELS
  Channels are numbered from 1 to 4. The board cannot report how many
  channels it has; a channel it lacks is silently ignored.

NO READ-BACK
  The board never reports state: FFE1 cannot be read, it never notifies, and
  there is no other state characteristic. on/off/cycle therefore succeed when
  the BLE write is acknowledged, which proves the board received the frame,
  not that the relay moved or that the target changed state. `state` always
  fails with `state unknown` (exit 1, nothing on stdout) rather than guess;
  dsdble does not remember what it last wrote. Do not wire a state_cmd for
  this helper.

The helper only ever writes to FFE1. The board's firmware-update service is
never touched."
)]
struct Cli {
    /// Board address: MAC on Linux (00:11:22:33:44:55), CoreBluetooth UUID on
    /// macOS (see `scan`). Case-insensitive. Required for every verb except
    /// `scan`.
    #[arg(short = 'a', long = "address", value_name = "ADDR", global = true)]
    address: Option<String>,

    /// Seconds to scan for the board before giving up.
    #[arg(
        long = "find-timeout",
        value_name = "SECS",
        default_value_t = 10,
        global = true
    )]
    find_timeout: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Always fails with `state unknown`: the board has no read-back, and
    /// dsdble never guesses or caches. Prints nothing on stdout, so a
    /// state_cmd hook is an error, never a wrong answer.
    State {
        /// Relay channel (1 to 4).
        #[arg(value_parser = parse_channel)]
        channel: u8,
    },
    /// Power the target on: release the coil (NC wiring). Succeeds when the
    /// BLE write is acknowledged; the result is not confirmed (hook: on_cmd).
    On {
        /// Relay channel (1 to 4).
        #[arg(value_parser = parse_channel)]
        channel: u8,
    },
    /// Power the target off: energize the coil (NC wiring). Succeeds when the
    /// BLE write is acknowledged; the result is not confirmed (hook: off_cmd).
    Off {
        /// Relay channel (1 to 4).
        #[arg(value_parser = parse_channel)]
        channel: u8,
    },
    /// Power-cycle on one connection: off -> hold -> on (hook: cycle_cmd).
    /// Neither phase is confirmed.
    Cycle {
        /// Relay channel (1 to 4).
        #[arg(value_parser = parse_channel)]
        channel: u8,
        /// Milliseconds to hold the target off before restoring power.
        #[arg(long, default_value_t = 3000)]
        delay_ms: u64,
    },
    /// Read-only health check (panio doctor): is a Bluetooth adapter present
    /// and on, is the board in range, can we connect, is FFE1 writable.
    /// Never writes to the board.
    ///
    /// Checks `adapter`, `device`, `connect` and `characteristic`, within the
    /// 10 s budget panio gives a doctor check (the scan is capped at 4 s).
    /// `--json` prints panio's doctor report
    /// (`{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}`) and
    /// exits 0 whatever the checks found; without it, one line per check and
    /// exit 1 if any failed. The optional channel is only range-checked: the
    /// board cannot be asked about a channel.
    Doctor {
        /// Channel to check (1 to 4; default 1).
        #[arg(value_parser = parse_channel)]
        channel: Option<u8>,
        /// Print the JSON report panio doctor reads.
        #[arg(long)]
        json: bool,
    },
    /// List peripherals advertising the relay service (FFE0): address (the
    /// CoreBluetooth UUID on macOS), name, RSSI. Connects to nothing.
    Scan {
        /// Seconds to scan.
        #[arg(long, value_name = "SECS", default_value_t = 8)]
        timeout: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let find = Duration::from_secs(cli.find_timeout);
    match cli.cmd {
        Cmd::Scan { timeout } => {
            let found = ble::scan(Duration::from_secs(timeout))?;
            if found.is_empty() {
                bail!("no peripherals advertising the relay service (FFE0) found");
            }
            print!("{}", format_scan(&found));
            Ok(())
        }
        Cmd::State { channel } => {
            require_address(&cli.address)?;
            Err(state_unknown(channel))
        }
        Cmd::On { channel } => {
            let mut r = ble::connect(require_address(&cli.address)?, find)?;
            let res = set_target(&mut r, channel, true);
            r.close();
            res?;
            println!("channel {channel}: on (written; not confirmed, the board has no read-back)");
            Ok(())
        }
        Cmd::Off { channel } => {
            let mut r = ble::connect(require_address(&cli.address)?, find)?;
            let res = set_target(&mut r, channel, false);
            r.close();
            res?;
            println!("channel {channel}: off (written; not confirmed, the board has no read-back)");
            Ok(())
        }
        Cmd::Cycle { channel, delay_ms } => {
            let mut r = ble::connect(require_address(&cli.address)?, find)?;
            let res = cycle(
                &mut r,
                channel,
                Duration::from_millis(delay_ms),
                thread::sleep,
            );
            r.close();
            res?;
            println!(
                "channel {channel}: cycled (off {delay_ms} ms, now on; not confirmed, \
                 the board has no read-back)"
            );
            Ok(())
        }
        Cmd::Doctor { json, .. } => {
            let address = require_address(&cli.address)?.to_string();
            let find = find.min(DOCTOR_FIND_CAP);
            let mut probe = ble::Session::new(address, find);
            let checks = doctor_checks(&mut probe);
            report_doctor(&checks, json)
        }
    }
}

/// The scan cap for `doctor`: panio kills a doctor check after 10 s.
const DOCTOR_FIND_CAP: Duration = Duration::from_secs(4);

fn require_address(address: &Option<String>) -> Result<&str> {
    match address.as_deref() {
        Some(a) if !a.trim().is_empty() => Ok(a),
        _ => bail!("--address (-a) is required (find it with `dsdble scan`)"),
    }
}

/// The error `state` returns. There is no reading to report and none is
/// invented: no stdout, a clear stderr message, a non-zero exit, which
/// panio's `power-state` hook treats as a failed hook.
fn state_unknown(channel: u8) -> anyhow::Error {
    anyhow!(
        "state unknown: the board has no read-back, so channel {channel}'s state cannot be \
         read (do not use dsdble as a state_cmd)"
    )
}

/// Write the frame that puts the target on or off. One write; the board
/// cannot confirm it.
fn set_target<R: Relay>(r: &mut R, channel: u8, on: bool) -> Result<()> {
    let channel = proto::validate_channel(channel)?;
    r.write_frame(target_frame(channel, on))
}

/// off -> hold -> on, on the connection the caller holds open. `sleep` is
/// injected so tests do not wait. Neither phase can be confirmed.
fn cycle<R: Relay>(
    r: &mut R,
    channel: u8,
    delay: Duration,
    sleep: impl FnOnce(Duration),
) -> Result<()> {
    set_target(r, channel, false)?;
    sleep(delay);
    set_target(r, channel, true).map_err(|e| {
        anyhow!("{e:#} (the cycle's power-on phase failed; the target may be left off)")
    })
}

/// One peripheral shown by `scan`.
#[derive(Debug, PartialEq, Eq)]
pub struct Seen {
    /// MAC (Linux) or platform id (macOS), per [`proto::display_address`].
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

fn format_scan(found: &[Seen]) -> String {
    let mut out = String::new();
    for s in found {
        let rssi = s
            .rssi
            .map_or_else(|| "-".to_string(), |r| format!("{r} dBm"));
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            s.address,
            s.name
                .as_deref()
                .map_or_else(|| "-".to_string(), proto::printable_name),
            rssi
        ));
    }
    out
}

/// The read-only steps `doctor` walks, each a fresh result. The real one is
/// [`ble::Session`]; tests use a fake. No step writes to the board.
pub trait Probe {
    /// A Bluetooth adapter exists and is powered on.
    fn adapter(&mut self) -> Result<String>;
    /// The board was seen advertising within the scan budget.
    fn locate(&mut self) -> Result<String>;
    /// A connection and service discovery succeed.
    fn connect(&mut self) -> Result<String>;
    /// FFE1 is present and writable.
    fn characteristic(&mut self) -> Result<String>;
}

/// One entry of the doctor report (docs/dev/adding-power-helpers.md,
/// "Doctor check").
#[derive(Debug, PartialEq, Eq)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

/// The checks in order; each needs the one before it, so a failure ends the
/// list.
fn doctor_checks<P: Probe>(p: &mut P) -> Vec<Check> {
    type Step<P> = fn(&mut P) -> Result<String>;
    let steps: [(&'static str, Step<P>); 4] = [
        ("adapter", P::adapter),
        ("device", P::locate),
        ("connect", P::connect),
        ("characteristic", P::characteristic),
    ];
    let mut checks = Vec::new();
    for (name, step) in steps {
        match step(p) {
            Ok(detail) => checks.push(Check {
                name,
                status: "ok",
                detail,
            }),
            Err(e) => {
                checks.push(Check {
                    name,
                    status: "fail",
                    detail: format!("{e:#}"),
                });
                break;
            }
        }
    }
    checks
}

fn report_doctor(checks: &[Check], json: bool) -> Result<()> {
    if json {
        println!("{}", doctor_json(checks));
        return Ok(());
    }
    for c in checks {
        println!("{:<5} {:<14} {}", c.status, c.name, c.detail);
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
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<String>>>;

    /// Records every frame written, and sleeps, in one ordered log.
    struct FakeRelay {
        log: Log,
        frames: Vec<[u8; 4]>,
        /// Fail the Nth write (0-based).
        fail_write: Option<usize>,
    }

    impl FakeRelay {
        fn new(log: &Log) -> Self {
            FakeRelay {
                log: Rc::clone(log),
                frames: Vec::new(),
                fail_write: None,
            }
        }
    }

    impl Relay for FakeRelay {
        fn write_frame(&mut self, frame: [u8; 4]) -> Result<()> {
            if self.fail_write == Some(self.frames.len()) {
                bail!("write refused");
            }
            self.log.borrow_mut().push(format!("write {frame:02x?}"));
            self.frames.push(frame);
            Ok(())
        }
    }

    fn log() -> Log {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn on_releases_and_off_energizes_the_coil() {
        let l = log();
        let mut r = FakeRelay::new(&l);
        set_target(&mut r, 1, true).unwrap();
        set_target(&mut r, 1, false).unwrap();
        assert_eq!(
            r.frames,
            [[0xa0, 0x01, 0x00, 0xa1], [0xa0, 0x01, 0x01, 0xa2]]
        );
    }

    #[test]
    fn set_target_rejects_a_channel_out_of_range_without_writing() {
        let l = log();
        let mut r = FakeRelay::new(&l);
        assert!(set_target(&mut r, 0, true).is_err());
        assert!(set_target(&mut r, 5, false).is_err());
        assert!(r.frames.is_empty());
    }

    #[test]
    fn cycle_is_off_then_hold_then_on() {
        let l = log();
        let mut r = FakeRelay::new(&l);
        let sleeper = Rc::clone(&l);
        let mut slept = None;
        cycle(&mut r, 2, Duration::from_millis(7), |d| {
            slept = Some(d);
            sleeper.borrow_mut().push("sleep".to_string());
        })
        .unwrap();
        assert_eq!(slept, Some(Duration::from_millis(7)));
        assert_eq!(
            *l.borrow(),
            ["write [a0, 02, 01, a3]", "sleep", "write [a0, 02, 00, a2]"],
            "off, hold, on"
        );
    }

    #[test]
    fn cycle_whose_off_write_fails_never_holds() {
        let l = log();
        let mut r = FakeRelay::new(&l);
        r.fail_write = Some(0);
        let mut slept = false;
        let e = cycle(&mut r, 1, Duration::ZERO, |_| slept = true).unwrap_err();
        assert!(e.to_string().contains("write refused"), "{e}");
        assert!(!slept, "must not report a hold that never happened");
        assert!(r.frames.is_empty());
    }

    #[test]
    fn cycle_whose_on_write_fails_warns_the_target_may_be_off() {
        let l = log();
        let mut r = FakeRelay::new(&l);
        r.fail_write = Some(1);
        let e = cycle(&mut r, 1, Duration::ZERO, |_| {}).unwrap_err();
        assert!(e.to_string().contains("target may be left off"), "{e}");
        assert_eq!(r.frames, [[0xa0, 0x01, 0x01, 0xa2]]);
    }

    #[test]
    fn state_is_unknown_and_says_so() {
        let e = state_unknown(1);
        let msg = e.to_string();
        assert!(msg.starts_with("state unknown"), "{msg}");
        assert!(!msg.starts_with("on") && !msg.starts_with("off"));
    }

    #[test]
    fn address_is_required() {
        assert!(require_address(&None).is_err());
        assert!(require_address(&Some("  ".into())).is_err());
        assert_eq!(
            require_address(&Some("00:11:22:33:44:55".into())).unwrap(),
            "00:11:22:33:44:55"
        );
    }

    #[test]
    fn scan_lists_one_line_per_peripheral_with_dashes_for_gaps() {
        let found = [
            Seen {
                address: "00:11:22:33:44:55".into(),
                name: Some("DSD TECH".into()),
                rssi: Some(-61),
            },
            Seen {
                address: "00000000-0000-4000-8000-000000000001".into(),
                name: None,
                rssi: None,
            },
        ];
        assert_eq!(
            format_scan(&found),
            "00:11:22:33:44:55\tDSD TECH\t-61 dBm\n\
             00000000-0000-4000-8000-000000000001\t-\t-\n"
        );
    }

    /// A probe that fails at a chosen step and counts what it was asked.
    struct FakeProbe {
        fail_at: Option<&'static str>,
        asked: Vec<&'static str>,
    }

    impl FakeProbe {
        fn step(&mut self, name: &'static str) -> Result<String> {
            self.asked.push(name);
            if self.fail_at == Some(name) {
                bail!("{name} broke");
            }
            Ok(format!("{name} fine"))
        }
    }

    impl Probe for FakeProbe {
        fn adapter(&mut self) -> Result<String> {
            self.step("adapter")
        }
        fn locate(&mut self) -> Result<String> {
            self.step("device")
        }
        fn connect(&mut self) -> Result<String> {
            self.step("connect")
        }
        fn characteristic(&mut self) -> Result<String> {
            self.step("characteristic")
        }
    }

    fn summary(checks: &[Check]) -> Vec<(&'static str, &'static str)> {
        checks.iter().map(|c| (c.name, c.status)).collect()
    }

    #[test]
    fn doctor_reports_a_healthy_board() {
        let mut p = FakeProbe {
            fail_at: None,
            asked: vec![],
        };
        let checks = doctor_checks(&mut p);
        assert_eq!(
            summary(&checks),
            [
                ("adapter", "ok"),
                ("device", "ok"),
                ("connect", "ok"),
                ("characteristic", "ok")
            ]
        );
    }

    #[test]
    fn doctor_stops_at_the_first_failure() {
        let mut p = FakeProbe {
            fail_at: Some("device"),
            asked: vec![],
        };
        let checks = doctor_checks(&mut p);
        assert_eq!(summary(&checks), [("adapter", "ok"), ("device", "fail")]);
        assert_eq!(p.asked, ["adapter", "device"], "later steps never ran");
        assert_eq!(checks[1].detail, "device broke");
    }

    #[test]
    fn doctor_json_has_the_contract_shape() {
        let mut p = FakeProbe {
            fail_at: Some("connect"),
            asked: vec![],
        };
        let json = doctor_json(&doctor_checks(&mut p));
        assert_eq!(
            json,
            "{\"checks\": [{\"name\": \"adapter\", \"status\": \"ok\", \"detail\": \"adapter fine\"}, \
             {\"name\": \"device\", \"status\": \"ok\", \"detail\": \"device fine\"}, \
             {\"name\": \"connect\", \"status\": \"fail\", \"detail\": \"connect broke\"}]}"
        );
    }

    #[test]
    fn doctor_json_escapes_awkward_detail() {
        let checks = [Check {
            name: "device",
            status: "fail",
            detail: "bad \"quote\" \\ and\nnewline \u{1}".into(),
        }];
        assert_eq!(
            doctor_json(&checks),
            "{\"checks\": [{\"name\": \"device\", \"status\": \"fail\", \
             \"detail\": \"bad \\\"quote\\\" \\\\ and\\nnewline \\u0001\"}]}"
        );
    }
}
