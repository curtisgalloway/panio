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

//! switchport — link and PoE control for one port of a managed switch.
//!
//! A one-shot, stateless panio helper (docs/dev/adding-power-helpers.md): it
//! is wired in as a link hook (`panio netboot set --link-down-cmd …`), not a
//! power hook. Safety rules (management-port refusal, read-back, link wait)
//! live in `ops`; `easysmart` and `snmp` are the two backends.

mod easysmart;
mod ops;
mod snmp;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use ops::{err, Backend, Kind, Opts};

const PASSWORD: secret::Spec = secret::Spec {
    what: "switch web-UI password",
    env: "SWITCHPORT_PASSWORD",
    file_flag: "--password-file",
    command_flag: "--password-command",
};

const COMMUNITY: secret::Spec = secret::Spec {
    what: "SNMP community",
    env: "SWITCHPORT_COMMUNITY",
    file_flag: "--community-file",
    command_flag: "--community-command",
};

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq, Debug)]
enum BackendKind {
    Easysmart,
    Snmp,
}

#[derive(Clone, Copy, ValueEnum)]
enum Action {
    On,
    Off,
    Status,
}

#[derive(Parser)]
#[command(
    name = "switchport",
    version,
    about = "Link and PoE control for one port of a managed Ethernet switch",
    long_about = "Link and PoE control for one port of a managed Ethernet switch, so a target \
sees a real link drop (USB Ethernet adapters keep their PHY powered when the host interface is \
down).

MENTAL MODEL
  - Ports are numbered from 1, as printed on the switch.
  - --mgmt-port names the switch port YOUR control host reaches the switch through. Turning \
that port off would cut the helper's own path to the switch, so `port N off` and `poe N off` \
refuse without it, and refuse when N equals it.
  - Every write is verified by reading the state back. The link is reported, not awaited, unless \
--wait-link SECS is given: then `port N on|off` waits for the link to follow and exits 22 if it \
does not.
  - Secrets come from the environment, a file, or a command, never a flag value: \
SWITCHPORT_PASSWORD / --password-file / --password-command (easysmart) and \
SWITCHPORT_COMMUNITY / --community-file / --community-command (snmp; needed for SET).
  - Easy Smart (TL-SG108PE, TL-SG205E) is cleartext HTTP; the older web UI binds the session \
to the client IP, the 2025 one uses a cookie (detected at login): isolated bench networks \
only. Logging in from here can end a browser session from the same host.

TYPICAL USE
  switchport -d 192.0.2.2 --mgmt-port 8 port 3 off
  switchport -d 192.0.2.2 --mgmt-port 8 port 3 on
  switchport -d 192.0.2.2 port 3 status --json
  switchport -d 192.0.2.2 --backend snmp --mgmt-port 8 poe 3 off
  switchport -d 192.0.2.2 --mgmt-port 8 doctor --json

Put boolean flags (--json, --allow-unknown-model) after the verb so panio's doctor-command \
derivation (which treats an option before the verb as taking a value) is not confused.

EXIT STATUS
  0 ok   1 failed   2 usage / unsupported   3 not configured (missing secret or \
--mgmt-port)   4 switch unreachable   22 link did not follow within --wait-link"
)]
struct Cli {
    /// Switch address: IP or hostname, optionally host:port (easysmart also
    /// accepts http://host).
    #[arg(short = 'd', long = "device", value_name = "HOST", global = true)]
    device: Option<String>,

    /// Which protocol to speak to the switch.
    #[arg(long, value_enum, default_value = "easysmart", global = true)]
    backend: BackendKind,

    /// The switch port this control host reaches the switch through; `off` is
    /// refused for it, and for everything when this is not given.
    #[arg(long, value_name = "N", global = true)]
    mgmt_port: Option<u32>,

    /// Easy Smart web-UI username.
    #[arg(long, default_value = "admin", global = true)]
    user: String,

    /// Read the Easy Smart password from this file (else SWITCHPORT_PASSWORD).
    #[arg(long, value_name = "PATH", global = true)]
    password_file: Option<PathBuf>,

    /// Read the Easy Smart password from this command's stdout.
    #[arg(long, value_name = "CMD", global = true)]
    password_command: Option<String>,

    /// Read the SNMP community from this file (else SWITCHPORT_COMMUNITY).
    #[arg(long, value_name = "PATH", global = true)]
    community_file: Option<PathBuf>,

    /// Read the SNMP community from this command's stdout.
    #[arg(long, value_name = "CMD", global = true)]
    community_command: Option<String>,

    /// SNMP: ifIndex = port + K (default 0: ifIndex equals the port number).
    #[arg(
        long,
        value_name = "K",
        default_value_t = 0,
        allow_negative_numbers = true,
        global = true
    )]
    if_index_offset: i64,

    /// SNMP: POWER-ETHERNET-MIB PSE group number for `poe`.
    #[arg(long, value_name = "G", default_value_t = 1, global = true)]
    poe_group: u64,

    /// Wait up to SECS for the link to follow `port N on|off`, and exit 22 if
    /// it does not. Default 0: verify the admin state by read-back and report
    /// the current link state without waiting (a powered-off target has no
    /// link yet).
    #[arg(long, value_name = "SECS", default_value_t = 0, global = true)]
    wait_link: u64,

    /// Easy Smart: drive a switch whose hardware/firmware is not on the
    /// built-in allowlist (prints a warning).
    #[arg(long, global = true)]
    allow_unknown_model: bool,

    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Admin state and link of a port: on|off set it (verified by read-back,
    /// then optionally waits for the link, see --wait-link), status reads it.
    Port {
        /// Port number, from 1 as printed on the switch.
        port: u32,
        action: Action,
    },
    /// PoE on a port: on|off set it (verified by read-back), status reads it.
    /// Supported by the snmp backend (POWER-ETHERNET-MIB) only.
    Poe {
        /// Port number, from 1 as printed on the switch.
        port: u32,
        action: Action,
    },
    /// Read-only health check (panio doctor): switch identity, credentials,
    /// port count. Never changes the switch. `--json` prints panio's doctor
    /// report and exits 0 whatever the checks found.
    Doctor,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::from(ops::kind_of(&e).exit_code())
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let opts = Opts {
        mgmt_port: cli.mgmt_port,
        wait: Duration::from_secs(cli.wait_link),
        poll: Duration::from_millis(500),
    };
    if let Cmd::Doctor = cli.cmd {
        let checks = doctor_checks(&cli);
        return report_doctor(&checks, cli.json);
    }
    let (port, action, is_poe) = match cli.cmd {
        Cmd::Port { port, action } => (port, action, false),
        Cmd::Poe { port, action } => (port, action, true),
        Cmd::Doctor => unreachable!("handled above"),
    };
    if port == 0 {
        return Err(err(Kind::Usage, "ports are numbered from 1"));
    }
    // Refuse before building the backend: no credential lookup, no request.
    if matches!(action, Action::Off) {
        ops::guard_mgmt(&opts, port, if is_poe { "PoE on port" } else { "port" })?;
    }
    let mut be = build_backend(&cli)?;
    match (is_poe, action) {
        (false, Action::Status) => print_port(port, &ops::port_state(be.as_mut(), port)?, cli.json),
        (false, a) => {
            let st = ops::port_set(be.as_mut(), &opts, port, matches!(a, Action::On))?;
            print_port(port, &st, cli.json);
        }
        (true, Action::Status) => print_poe(port, ops::poe_state(be.as_mut(), port)?, cli.json),
        (true, a) => {
            let on = matches!(a, Action::On);
            ops::poe_set(be.as_mut(), &opts, port, on)?;
            print_poe(port, on, cli.json);
        }
    }
    Ok(())
}

fn print_port(port: u32, st: &ops::PortStatus, json: bool) {
    if json {
        println!("{}", ops::status_json(port, st));
    } else {
        println!("{}", ops::status_line(port, st));
    }
}

fn print_poe(port: u32, on: bool, json: bool) {
    if json {
        println!("{}", ops::poe_json(port, on));
    } else {
        println!("poe {port}: {}", if on { "on" } else { "off" });
    }
}

fn build_backend(cli: &Cli) -> Result<Box<dyn Backend>> {
    let device = cli.device.as_deref().ok_or_else(|| {
        err(
            Kind::Usage,
            "required option '--device <HOST>' (-d) was not provided",
        )
    })?;
    match cli.backend {
        BackendKind::Easysmart => {
            let password = secret::require(
                &PASSWORD,
                &secret::Sources {
                    file: cli.password_file.clone(),
                    command: cli.password_command.clone(),
                },
            )?;
            Ok(Box::new(easysmart::EasySmart::new(
                device,
                &cli.user,
                password.expose(),
                cli.allow_unknown_model,
            )))
        }
        BackendKind::Snmp => {
            let community = secret::require(
                &COMMUNITY,
                &secret::Sources {
                    file: cli.community_file.clone(),
                    command: cli.community_command.clone(),
                },
            )?;
            Ok(Box::new(snmp::Snmp::new(
                device,
                community.expose(),
                cli.if_index_offset,
                cli.poe_group,
                Duration::from_secs(3),
            )?))
        }
    }
}

/// One entry of the doctor report.
#[derive(Debug, PartialEq, Eq)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

fn check(name: &'static str, status: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

/// Read-only: identity and port count. No SET and no port_setting.cgi.
fn doctor_checks(cli: &Cli) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut be = match build_backend(cli) {
        Ok(b) => b,
        Err(e) => return vec![check("credentials", "fail", format!("{e:#}"))],
    };
    match be.check_identity() {
        Ok(d) => checks.push(check("identity", "ok", d)),
        Err(e) => {
            checks.push(check("identity", "fail", format!("{e:#}")));
            return checks;
        }
    }
    match be.port_count() {
        Ok(n) => checks.push(check("ports", "ok", format!("{n} reported"))),
        Err(e) => checks.push(check("ports", "fail", format!("{e:#}"))),
    }
    checks.push(match cli.mgmt_port {
        Some(m) => check(
            "mgmt-port",
            "ok",
            format!("port {m} is protected from `off`"),
        ),
        None => check(
            "mgmt-port",
            "warn",
            "no --mgmt-port: every `off` will be refused until it is given",
        ),
    });
    checks
}

fn report_doctor(checks: &[Check], json: bool) -> Result<()> {
    if json {
        let items: Vec<String> = checks
            .iter()
            .map(|c| {
                format!(
                    "{{\"name\":{},\"status\":{},\"detail\":{}}}",
                    ops::json_str(c.name),
                    ops::json_str(c.status),
                    ops::json_str(&c.detail)
                )
            })
            .collect();
        println!("{{\"checks\":[{}]}}", items.join(","));
        return Ok(());
    }
    for c in checks {
        println!("{:<5} {:<10} {}", c.status, c.name, c.detail);
    }
    if checks.iter().any(|c| c.status == "fail") {
        return Err(err(Kind::Failed, "doctor found a problem"));
    }
    Ok(())
}
