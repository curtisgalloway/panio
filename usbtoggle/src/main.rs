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

//! usbtoggle — a `usb` channel helper for a USB sharing switch that only
//! toggles. A relay (any kind) across the switch's button presses it; the
//! position is verified by looking for the peripheral on the control host.
//!
//! Subcommands:
//!   usb host|target|state   The panio `usb` channel contract.
//!   press                   One press, no verification (wiring tests).
//!   doctor [--json]         Read-only health report; never presses.

mod doctor;
mod logic;
mod sys;

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use logic::{Config, Probe, Side, Source};
use sys::{DeviceSpec, NoProbe, ShellRunner, SystemClock, UsbProbe};

#[derive(Parser)]
#[command(
    name = "usbtoggle",
    version,
    about = "usb channel helper for a toggle-only USB sharing switch, pressed by any relay",
    long_about = "usb channel helper for a toggle-only USB sharing switch (one peripheral \
port, two computer ports, a momentary button) pressed by any relay.

The switch has no readable position: pressing the button only toggles which \
computer the peripheral is connected to. usbtoggle presses it by running your \
--close-cmd and --open-cmd (any relay: `dsdrelay`, `phidget`, a script) and \
verifies the result by looking for the peripheral on the control host \
(--device) or by asking --state-cmd.

  usbtoggle [OPTIONS] usb host|target|state
  usbtoggle [OPTIONS] press
  usbtoggle [OPTIONS] doctor [--json]

PANIO CHANNEL (docs/usb.md)
  panio usb set -t TARGET --cmd \"usbtoggle --device 046d:c08b \\
      --close-cmd 'dsdrelay -s AA00BB11CC22DD33 off 1' \\
      --open-cmd 'dsdrelay -s AA00BB11CC22DD33 on 1'\"
  panio runs `<cmd> usb host|target|state` through the shell.

READING THE POSITION
  --device: the peripheral present on this host means `host`. Absent means
  `target`, but only as an inference (it could be unplugged): a loud warning
  goes to stderr unless --target-check-cmd exits 0 to confirm the target sees
  it. Absent and the check failing is an error (\"seen on neither side\").
  --state-cmd: a command that prints `host` or `target` replaces the probe.

SAFETY
  `usb host|target` presses at most once, then waits up to --settle-ms for the
  expected position and fails on timeout; it never retries. The open command
  always runs after the close command was attempted. stdout carries only the
  resulting side; the relay commands' output goes to stderr."
)]
struct Cli {
    /// Shell command that closes the relay across the button. Required for
    /// `usb host`, `usb target` and `press`.
    #[arg(long, value_name = "CMD")]
    close_cmd: Option<String>,

    /// Shell command that opens the relay again. Required for `usb host`,
    /// `usb target` and `press`; always run once a close was attempted.
    #[arg(long, value_name = "CMD")]
    open_cmd: Option<String>,

    /// Milliseconds between the close and the open command.
    #[arg(long, value_name = "MS", default_value_t = 200)]
    hold_ms: u64,

    /// The peripheral to look for on this host, as hex VID:PID[:SERIAL]
    /// (e.g. 046d:c08b). Present means `host`. Give this or --state-cmd for
    /// the `usb` verbs.
    #[arg(long, value_name = "VID:PID[:SERIAL]", conflicts_with = "state_cmd")]
    device: Option<DeviceSpec>,

    /// Command that prints `host` or `target`; replaces the probe. Give this
    /// or --device for the `usb` verbs.
    #[arg(long, value_name = "CMD")]
    state_cmd: Option<String>,

    /// Optional shell command; exit 0 means the target sees the device.
    /// Turns "absent from the host" from an inference into a reading.
    #[arg(long, value_name = "CMD")]
    target_check_cmd: Option<String>,

    /// Milliseconds to wait after a press for the expected position (polled
    /// about every 50 ms).
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    settle_ms: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// The panio usb channel verbs; prints the resulting side on stdout.
    Usb {
        #[command(subcommand)]
        verb: UsbVerb,
    },
    /// Press the button once (close, hold, open) with no verification. For
    /// testing the wiring.
    Press,
    /// Read-only health check (panio doctor): does the probe or --state-cmd
    /// work, is the device present, does --target-check-cmd succeed. Never
    /// runs the close or open command.
    ///
    /// `--json` prints panio's doctor report
    /// (`{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}`) and
    /// exits 0 whatever the checks found; without it, one line per check and
    /// exit 1 if any failed.
    Doctor {
        /// Print the JSON report panio doctor reads.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum UsbVerb {
    /// Connect the peripheral to the control host (hook: `usb host`).
    Host,
    /// Connect the peripheral to the target (hook: `usb target`).
    Target,
    /// Print `host` or `target` (hook: `usb state`).
    State,
}

impl Cli {
    fn config(&self) -> Config {
        Config {
            close_cmd: self.close_cmd.clone(),
            open_cmd: self.open_cmd.clone(),
            hold: Duration::from_millis(self.hold_ms),
            settle: Duration::from_millis(self.settle_ms),
            poll: Duration::from_millis(50),
            source: match &self.state_cmd {
                Some(c) => Source::StateCmd(c.clone()),
                None => Source::Device,
            },
            target_check: self.target_check_cmd.clone(),
        }
    }

    fn probe(&self) -> Box<dyn Probe> {
        match &self.device {
            Some(spec) => Box::new(UsbProbe { spec: spec.clone() }),
            None => Box::new(NoProbe),
        }
    }

    /// The `usb` verbs need exactly one way to read the position.
    fn require_position_source(&self) -> Result<()> {
        if self.device.is_none() && self.state_cmd.is_none() {
            bail!("give --device VID:PID[:SERIAL] or --state-cmd CMD: the switch has no readable position, so it must be probed");
        }
        Ok(())
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("usbtoggle: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let cfg = cli.config();
    let mut runner = ShellRunner;
    let mut probe = cli.probe();
    match cli.cmd {
        Cmd::Usb { verb } => {
            cli.require_position_source()?;
            let mut clock = SystemClock::new();
            let outcome = match verb {
                UsbVerb::State => logic::state(&cfg, &mut runner, probe.as_mut())?,
                UsbVerb::Host => {
                    logic::switch_to(Side::Host, &cfg, &mut runner, probe.as_mut(), &mut clock)?
                }
                UsbVerb::Target => {
                    logic::switch_to(Side::Target, &cfg, &mut runner, probe.as_mut(), &mut clock)?
                }
            };
            logic::emit(outcome, &mut io::stdout(), &mut io::stderr())?;
            Ok(())
        }
        Cmd::Press => {
            logic::press(&cfg, &mut runner, &mut SystemClock::new())?;
            eprintln!("usbtoggle: pressed once (not verified)");
            Ok(())
        }
        Cmd::Doctor { json } => {
            let checks = doctor::checks(&cfg, cli.device.as_ref(), &mut runner, probe.as_mut());
            let mut out = io::stdout();
            if json {
                writeln!(out, "{}", doctor::to_json(&checks))?;
                return Ok(());
            }
            for c in &checks {
                writeln!(out, "{:<5} {:<13} {}", c.status, c.name, c.detail)?;
            }
            if checks.iter().any(|c| c.status == "fail") {
                bail!("doctor found a problem");
            }
            Ok(())
        }
    }
}
