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

//! phidget — switch channels of Phidgets VINT modules behind a HUB0002 hub.
//!
//! Subcommands:
//!   list                  Show attached hubs and the module on each port.
//!   on <port>/<channel>   Close the relay (one packet; the hub's ACK only).
//!   off <port>/<channel>  Open the relay.
//!   cycle <port>/<channel> off -> delay -> on.
//!   state <port>/<channel> Always fails: there is no read-back.
//!   doctor [addr]         Read-only health report (`--json` for `panio doctor`).
//!
//! The command logic is written against the small [`hub::Link`] trait so
//! tests drive it with a fake that records packets.

mod devices;
mod hub;
mod proto;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};

use devices::{check_target, module_for, Class};
use hub::{HubInfo, Link, UsbLink};
use proto::{parse_address, set_output_packet, Address, PortsDescriptor};

#[derive(Parser)]
#[command(
    name = "phidget",
    version,
    about = "Power control for Phidgets VINT relay modules (REL1100) through a HUB0002, over raw USB (no read-back)",
    long_about = "Power control for Phidgets VINT modules through a HUB0002 hub, over raw USB \
(no Phidgets library needed). Today that means the REL1100 4-channel solid-state \
relay; more VINT modules can be added to the table in src/devices.rs.

ADDRESS
  Every verb takes <PORT>/<CHANNEL>, both 0-based, matching the labels printed
  on the hardware: hub port 0 to 5, module channel 0 to 3.
    phidget cycle 0/0

HUB
  -s/--serial is the hub's USB serial-number string. It is optional when
  exactly one supported hub is attached, and an error listing the attached
  serials when several are. Put it before the verb:
    phidget -s AA00BB11 cycle 0/0
  `phidget list` shows the serials.

WIRING
  The REL1100 is wired normally open: on closes the contact (conducting),
  off opens it. There is no NC inversion.

NO READ-BACK
  The protocol has no way to read an output. on/off/cycle succeed when the
  hub ACKs the packet, which proves a module is attached to that port, not
  that the relay moved. `state` always fails with `state unknown` (exit 1,
  nothing on stdout) rather than guess, and phidget never remembers what it
  last wrote. Do not wire a state_cmd for this helper. A module reboot (hub
  port power loss) turns every relay on it off.

SAFETY
  Before every write the helper reads the hub's VINT ports descriptor and
  refuses an empty port, a module it does not know, or a channel the module
  lacks. It never sends the library's reset or enable packets or port power
  commands, so switching one channel leaves the others alone.

PLATFORMS
  Linux and macOS use raw USB access. On Linux a udev rule must grant access
  to USB vendor 06c2 (see docs/power.md). Windows builds but is unverified
  (it needs the WinUSB driver bound to the hub)."
)]
struct Cli {
    /// Hub USB serial number. Optional when exactly one hub is attached.
    #[arg(short = 's', long = "serial", value_name = "HUB-SERIAL", global = true)]
    serial: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List attached hubs (serial, SKU, firmware) and the module on each
    /// port (SKU, firmware, channel count) or `empty`. Read-only.
    List,
    /// Close the relay (conducting). Succeeds when the hub ACKs the packet;
    /// the relay's movement is not confirmed (hook: on_cmd).
    On {
        /// `<PORT>/<CHANNEL>`, 0-based (port 0 to 5, channel 0 to 3).
        #[arg(value_parser = parse_address)]
        address: Address,
    },
    /// Open the relay (not conducting). Succeeds when the hub ACKs the
    /// packet; the relay's movement is not confirmed (hook: off_cmd).
    Off {
        /// `<PORT>/<CHANNEL>`, 0-based (port 0 to 5, channel 0 to 3).
        #[arg(value_parser = parse_address)]
        address: Address,
    },
    /// Power-cycle on one connection: off -> hold -> on (hook: cycle_cmd).
    /// Neither phase is confirmed beyond the hub's ACK.
    Cycle {
        /// `<PORT>/<CHANNEL>`, 0-based (port 0 to 5, channel 0 to 3).
        #[arg(value_parser = parse_address)]
        address: Address,
        /// Milliseconds to hold the output off before restoring it.
        #[arg(long, default_value_t = 3000)]
        delay_ms: u64,
    },
    /// Always fails with `state unknown`: the protocol has no read-back, and
    /// phidget never guesses or caches. Prints nothing on stdout, so a
    /// state_cmd hook is an error, never a wrong answer.
    State {
        /// `<PORT>/<CHANNEL>`, 0-based (port 0 to 5, channel 0 to 3).
        #[arg(value_parser = parse_address)]
        address: Address,
    },
    /// Read-only health check (panio doctor): is a hub present and
    /// readable, and, if an address is given, does a supported module sit
    /// on that port with that channel. Reads descriptors only; never writes
    /// a packet.
    ///
    /// Checks `hub` and (with an address) `module`. `--json` prints panio's
    /// doctor report
    /// (`{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}`) and
    /// exits 0 whatever the checks found; without it, one line per check and
    /// exit 1 if any failed.
    Doctor {
        /// `<PORT>/<CHANNEL>` to check (optional).
        #[arg(value_parser = parse_address)]
        address: Option<Address>,
        /// Print the JSON report panio doctor reads.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let serial = cli.serial.as_deref();
    match cli.cmd {
        Cmd::List => list(serial),
        Cmd::State { address } => Err(state_unknown(address)),
        Cmd::On { address } => {
            let mut link = open_selected(serial)?;
            set_output(&mut link, address, true)?;
            println!("{address}: on (hub acknowledged; not confirmed, there is no read-back)");
            Ok(())
        }
        Cmd::Off { address } => {
            let mut link = open_selected(serial)?;
            set_output(&mut link, address, false)?;
            println!("{address}: off (hub acknowledged; not confirmed, there is no read-back)");
            Ok(())
        }
        Cmd::Cycle { address, delay_ms } => {
            let mut link = open_selected(serial)?;
            cycle(
                &mut link,
                address,
                Duration::from_millis(delay_ms),
                thread::sleep,
            )?;
            println!(
                "{address}: cycled (off {delay_ms} ms, now on; hub acknowledged both, \
                 not confirmed, there is no read-back)"
            );
            Ok(())
        }
        Cmd::Doctor { address, json } => {
            let hub = hub::select_hub(serial).and_then(|h| {
                let detail = format!("{} serial {}, firmware {}", h.sku, h.serial, h.firmware);
                UsbLink::open(&h).map(|l| (detail, l))
            });
            let checks = doctor_checks(hub, address);
            report_doctor(&checks, json)
        }
    }
}

fn open_selected(serial: Option<&str>) -> Result<UsbLink> {
    let h = hub::select_hub(serial)?;
    UsbLink::open(&h)
}

/// The error `state` returns: nothing on stdout, a clear stderr message.
fn state_unknown(a: Address) -> anyhow::Error {
    anyhow!(
        "state unknown: the VINT protocol has no read-back, so {a}'s state cannot be read \
         (do not use phidget as a state_cmd)"
    )
}

/// Validate the target against the live ports descriptor, then write one
/// packet and wait for the hub's ACK. Never writes blind.
fn set_output<L: Link>(link: &mut L, a: Address, on: bool) -> Result<()> {
    let desc = link.ports()?;
    let module = check_target(&desc, a).map_err(|e| anyhow!(e))?;
    match module.class {
        Class::DigitalOutput => {
            let packet = set_output_packet(a.port, module.vint_id, a.channel, on);
            link.send_acked(&packet, proto::packet_id(a.port))
        }
    }
}

/// off -> hold -> on on the connection the caller holds open. `sleep` is
/// injected so tests do not wait.
fn cycle<L: Link>(
    link: &mut L,
    a: Address,
    delay: Duration,
    sleep: impl FnOnce(Duration),
) -> Result<()> {
    set_output(link, a, false)?;
    sleep(delay);
    set_output(link, a, true).map_err(|e| {
        anyhow!("{e:#} (the cycle's power-on phase failed; the output may be left off)")
    })
}

fn list(serial: Option<&str>) -> Result<()> {
    let mut hubs = hub::find_hubs()?;
    if hubs.is_empty() {
        // Same message as every other verb.
        hub::select_index(&[], None)?;
    }
    if let Some(s) = serial {
        let serials: Vec<&str> = hubs.iter().map(|h| h.serial.as_str()).collect();
        let i = hub::select_index(&serials, Some(s))?;
        hubs = vec![hubs.swap_remove(i)];
    }
    let mut failed = false;
    for h in &hubs {
        let ports = UsbLink::open(h).and_then(|mut l| l.ports());
        failed |= ports.is_err();
        print!("{}", format_hub(h, &ports));
    }
    if failed {
        bail!("could not read every hub");
    }
    Ok(())
}

fn format_hub(h: &HubInfo, ports: &Result<PortsDescriptor>) -> String {
    format_hub_parts(&h.serial, h.sku, h.firmware, ports)
}

fn format_hub_parts(
    serial: &str,
    sku: &str,
    firmware: u32,
    ports: &Result<PortsDescriptor>,
) -> String {
    let mut out = format!("hub {serial}  {sku}  firmware {firmware}\n");
    match ports {
        Err(e) => out.push_str(&format!("  error: {e:#}\n")),
        Ok(d) => {
            for (i, p) in d.ports.iter().enumerate() {
                if p.id == 0 {
                    out.push_str(&format!("  port {i}  empty\n"));
                } else if let Some(m) = module_for(p.id) {
                    out.push_str(&format!(
                        "  port {i}  {}  firmware {}  {} channels\n",
                        m.sku,
                        proto::decode_version(p.version),
                        m.channels
                    ));
                } else {
                    out.push_str(&format!(
                        "  port {i}  VINT id 0x{:02X} (unsupported)  firmware {}\n",
                        p.id,
                        proto::decode_version(p.version)
                    ));
                }
            }
        }
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

/// The checks in order; `module` needs `hub`, so a hub failure ends the list.
/// `hub` carries the hub description and an open link, or why there is none.
fn doctor_checks<L: Link>(hub: Result<(String, L)>, addr: Option<Address>) -> Vec<Check> {
    let (detail, mut link) = match hub {
        Ok(h) => h,
        Err(e) => {
            return vec![Check {
                name: "hub",
                status: "fail",
                detail: format!("{e:#}"),
            }]
        }
    };
    let mut checks = vec![Check {
        name: "hub",
        status: "ok",
        detail,
    }];
    if let Some(a) = addr {
        let result = link
            .ports()
            .and_then(|d| check_target(&d, a).map_err(|e| anyhow!(e)));
        checks.push(match result {
            Ok(m) => Check {
                name: "module",
                status: "ok",
                detail: format!(
                    "{} on port {} ({} channels), channel {} in range",
                    m.sku, a.port, m.channels, a.channel
                ),
            },
            Err(e) => Check {
                name: "module",
                status: "fail",
                detail: format!("{e:#}"),
            },
        });
    }
    checks
}

fn report_doctor(checks: &[Check], json: bool) -> Result<()> {
    if json {
        println!("{}", doctor_json(checks));
        return Ok(());
    }
    for c in checks {
        println!("{:<5} {:<8} {}", c.status, c.name, c.detail);
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

    /// Records every packet sent; serves a fixed ports descriptor.
    struct FakeLink {
        desc: PortsDescriptor,
        sent: Vec<(Vec<u8>, u8)>,
        fail_send: Option<usize>,
    }

    fn descriptor(entries: &[(usize, u16, u16)]) -> PortsDescriptor {
        let mut b = vec![0u8; 42];
        b[0] = 42;
        b[1] = 0x43;
        for &(port, id, ver) in entries {
            let o = 2 + 4 * port;
            b[o..o + 2].copy_from_slice(&id.to_le_bytes());
            b[o + 2..o + 4].copy_from_slice(&ver.to_le_bytes());
        }
        PortsDescriptor::parse(&b).unwrap()
    }

    impl FakeLink {
        fn rel_on_port0() -> Self {
            FakeLink {
                desc: descriptor(&[(0, 0x1A, 130), (2, 0x77, 5)]),
                sent: Vec::new(),
                fail_send: None,
            }
        }
    }

    impl Link for FakeLink {
        fn ports(&mut self) -> Result<PortsDescriptor> {
            Ok(self.desc.clone())
        }
        fn send_acked(&mut self, packet: &[u8], id: u8) -> Result<()> {
            if self.fail_send == Some(self.sent.len()) {
                bail!("hub refused");
            }
            self.sent.push((packet.to_vec(), id));
            Ok(())
        }
    }

    fn addr(port: u8, channel: u8) -> Address {
        Address { port, channel }
    }

    #[test]
    fn on_sends_the_golden_packet() {
        let mut l = FakeLink::rel_on_port0();
        set_output(&mut l, addr(0, 1), true).unwrap();
        assert_eq!(
            l.sent,
            vec![(
                vec![0x00, 0x1a, 0x00, 0x01, 0x44, 0x01, 0x0a, 0x80, 0x00],
                1
            )]
        );
    }

    #[test]
    fn off_sends_zero_duty() {
        let mut l = FakeLink::rel_on_port0();
        set_output(&mut l, addr(0, 0), false).unwrap();
        assert_eq!(
            l.sent[0].0,
            vec![0x00, 0x1a, 0x00, 0x01, 0x03, 0x0a, 0x00, 0x00]
        );
    }

    #[test]
    fn refused_targets_write_nothing() {
        let mut l = FakeLink::rel_on_port0();
        let empty = set_output(&mut l, addr(1, 0), true)
            .unwrap_err()
            .to_string();
        assert!(empty.contains("empty"), "{empty}");
        let unsupported = set_output(&mut l, addr(2, 0), true)
            .unwrap_err()
            .to_string();
        assert!(unsupported.contains("does not support"), "{unsupported}");
        l.desc = descriptor(&[(0, 0x1A, 130)]);
        // the channel parser admits 0 to 3; a 2-channel module would refuse 3
        assert!(set_output(&mut l, addr(5, 3), true).is_err());
        assert!(l.sent.is_empty());
    }

    #[test]
    fn cycle_is_off_sleep_on() {
        let mut l = FakeLink::rel_on_port0();
        let mut slept = None;
        cycle(&mut l, addr(0, 0), Duration::from_millis(1234), |d| {
            slept = Some(d)
        })
        .unwrap();
        assert_eq!(slept, Some(Duration::from_millis(1234)));
        assert_eq!(l.sent.len(), 2);
        assert_eq!(&l.sent[0].0[6..], &[0x00, 0x00]);
        assert_eq!(&l.sent[1].0[6..], &[0x80, 0x00]);
    }

    #[test]
    fn cycle_names_a_failed_power_on_phase() {
        let mut l = FakeLink::rel_on_port0();
        l.fail_send = Some(1);
        let e = cycle(&mut l, addr(0, 0), Duration::ZERO, |_| {})
            .unwrap_err()
            .to_string();
        assert!(e.contains("power-on phase failed"), "{e}");
    }

    #[test]
    fn cycle_stops_when_off_fails() {
        let mut l = FakeLink::rel_on_port0();
        l.fail_send = Some(0);
        let mut slept = false;
        assert!(cycle(&mut l, addr(0, 0), Duration::ZERO, |_| slept = true).is_err());
        assert!(!slept);
    }

    #[test]
    fn state_is_always_unknown() {
        let e = state_unknown(addr(0, 2)).to_string();
        assert!(e.starts_with("state unknown:") && e.contains("0/2"), "{e}");
    }

    #[test]
    fn doctor_hub_only_and_with_module() {
        let l = FakeLink::rel_on_port0();
        let checks = doctor_checks(
            Ok(("HUB0002 serial AA00BB11, firmware 101".into(), l)),
            None,
        );
        assert_eq!(checks.len(), 1);
        assert_eq!((checks[0].name, checks[0].status), ("hub", "ok"));

        let l = FakeLink::rel_on_port0();
        let checks = doctor_checks(Ok(("hub".into(), l)), Some(addr(0, 3)));
        assert_eq!(checks.len(), 2);
        assert_eq!((checks[1].name, checks[1].status), ("module", "ok"));
        assert!(checks[1].detail.contains("REL1100"), "{}", checks[1].detail);
    }

    #[test]
    fn doctor_module_failure_and_no_hub() {
        let l = FakeLink::rel_on_port0();
        let checks = doctor_checks(Ok(("hub".into(), l)), Some(addr(4, 0)));
        assert_eq!((checks[1].name, checks[1].status), ("module", "fail"));

        let none: Result<(String, FakeLink)> = Err(anyhow!("no supported Phidgets hub found"));
        let checks = doctor_checks(none, Some(addr(0, 0)));
        assert_eq!(checks.len(), 1);
        assert_eq!((checks[0].name, checks[0].status), ("hub", "fail"));
    }

    #[test]
    fn doctor_json_shape() {
        let c = vec![Check {
            name: "hub",
            status: "ok",
            detail: "a \"b\"".into(),
        }];
        assert_eq!(
            doctor_json(&c),
            "{\"checks\": [{\"name\": \"hub\", \"status\": \"ok\", \"detail\": \"a \\\"b\\\"\"}]}"
        );
    }

    #[test]
    fn list_formats_every_port_kind() {
        let d = descriptor(&[(0, 0x1A, 130), (2, 0x77, 0x0101)]);
        let out = format_hub_parts("AA00BB11", "HUB0002", 101, &Ok(d));
        assert!(
            out.starts_with("hub AA00BB11  HUB0002  firmware 101\n"),
            "{out}"
        );
        assert!(
            out.contains("  port 0  REL1100  firmware 130  4 channels\n"),
            "{out}"
        );
        assert!(out.contains("  port 1  empty\n"), "{out}");
        assert!(
            out.contains("  port 2  VINT id 0x77 (unsupported)  firmware 101\n"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 7);
    }
}
