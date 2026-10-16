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

//! paniolo — agent-controlled target machine wrangler.
//!
//! Rust rewrite of the control plane (see docs/config-redesign.md). The lab
//! file is the single, CLI-managed source of truth for hosts, targets, and the
//! channels that connect them. This module is the CLI surface; the model and
//! editor live in [`model`] and [`labfile`].

mod actions;
mod adb;
mod daemons;
mod discover;
mod dispatch;
mod doctor;
mod error;
mod labfile;
mod mcp;
mod model;
mod netboot;
mod netif;
mod platform;
mod power;
mod serial;
mod setup;
mod skills;
mod ssh;
mod state;
#[cfg(test)]
mod stubhttp;
mod video;

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};

use labfile::LabFile;
use model::{Lab, ResolvedChannel, ResolvedTarget};

/// What `--version` prints. The tag is the only version source in this repo
/// (every `Cargo.toml` stays at its placeholder), so the release workflow
/// exports `PANIOLO_VERSION` from the tag when it builds each package and the
/// binary bakes it in here. A build without it — `cargo install --path`, a
/// dev checkout, `brew install --HEAD` — says so, rather than print the
/// placeholder as if it were an installed version. `cli/build.rs` makes cargo
/// rebuild when the variable changes.
const VERSION: &str = match option_env!("PANIOLO_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), " (unversioned dev build)"),
};

#[derive(Parser)]
#[command(
    name = "paniolo",
    version = VERSION,
    about = "Agent-controlled target machine wrangler.",
    after_help = error::EXIT_STATUS_HELP
)]
struct Cli {
    /// Path to the lab config file (default: $PANIOLO_LAB or ~/.config/paniolo/lab.toml).
    #[arg(long, global = true)]
    lab: Option<String>,
    /// On failure, also print a one-line JSON error object as the last line
    /// of stderr (same as PANIOLO_JSON_ERRORS=1).
    // Overrides itself so a repeat is accepted: dispatch always adds one for
    // the remote paniolo, whether or not the user's argv already had it.
    #[arg(long, global = true, overrides_with = "json_errors")]
    json_errors: bool,
    /// Plugin roots for out-of-tree helpers and skills, in PATH syntax
    /// (`:`-separated; `;` on Windows). Replaces PANIOLO_PLUGIN_PATH for this
    /// command and everything it starts. Dispatch passes a host's lab
    /// `plugin_path` this way. Goes before the subcommand.
    // Not `global`: `host add/set --plugin-path` is the lab field of the same
    // name, and a global flag would collide with it.
    #[arg(long, value_name = "LIST")]
    plugin_path: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an empty lab file.
    Init {
        /// Where to create it (default: --lab path or ~/.config/paniolo/lab.toml).
        #[arg(long)]
        path: Option<String>,
    },
    /// Inspect and edit the lab configuration file.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage control hosts declared in the lab.
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Manage targets.
    Target {
        #[command(subcommand)]
        cmd: TargetCmd,
    },
    /// Manage a target's serial channels.
    Serial {
        #[command(subcommand)]
        cmd: SerialCmd,
    },
    /// Configure a target's netboot channel and run DHCP+TFTP netboot.
    Netboot {
        #[command(subcommand)]
        cmd: NetbootCmd,
    },
    /// Manage the USB-Ethernet link: switch mode (netboot | link | ffx | off).
    Netif {
        #[command(subcommand)]
        cmd: NetifCmd,
    },
    /// Configure and drive a target's power channel (default hooks and named levels).
    Power {
        #[command(subcommand)]
        cmd: PowerCmd,
    },
    /// Configure and drive a target's HDMI-capture (video) channel.
    Video {
        #[command(subcommand)]
        cmd: VideoCmd,
    },
    /// Inject USB HID keyboard/mouse input via the target's configured helper.
    Hid {
        #[command(subcommand)]
        cmd: HidCmd,
    },
    /// Switch a shared USB device between the control host and the target —
    /// an Openterface KVM-Go's onboard microSD card, or a Mini-KVM's
    /// switchable USB-A port. Only one side sees it at a time.
    Usb {
        #[command(subcommand)]
        cmd: UsbCmd,
    },
    /// Run a target's named actions: reset, recovery, boot straps and other
    /// controls that are not power, each an opaque hook command in the lab.
    Action {
        #[command(subcommand)]
        cmd: ActionCmd,
    },
    /// Drive an Android target over adb (console, screencap, input).
    Adb {
        #[command(subcommand)]
        cmd: AdbCmd,
    },
    /// Open the combined video+serial dashboard, starting daemons if needed.
    ///
    /// Inside an SSH session (SSH_CONNECTION, SSH_CLIENT or SSH_TTY set) no
    /// browser is launched on this machine: instead it prints the `ssh -N -L`
    /// command that forwards every dashboard port to your own machine, and
    /// writes the token-bearing URL to a 0600 file (or prints it with
    /// `--print-url`).
    Console {
        #[command(flatten)]
        target: TargetArg,
        /// Serial interface name to preselect in the dashboard terminal.
        #[arg(long, short)]
        interface: Option<String>,
        /// Launch a browser on this machine even inside an SSH session.
        #[arg(long)]
        open: bool,
        /// Print the dashboard URL, token included. Treat the output as a
        /// credential: it lands in your terminal's scrollback.
        #[arg(long)]
        print_url: bool,
    },
    /// Run the target's configured power-cycle command.
    ///
    /// With a state_cmd at the chosen level, waits until it reports power back on.
    PowerCycle {
        #[command(flatten)]
        target: TargetArg,
        /// Use this named power level's cycle_cmd (and state_cmd) instead of
        /// the default hooks, e.g. `hard` for a mains cut. `default` names the
        /// default hooks.
        #[arg(long, value_name = "NAME")]
        level: Option<String>,
    },
    /// Show whether the target is powered on (via state_cmd or the serial sense line).
    ///
    /// Without --level, a target with named power levels gets one line per
    /// level, the default level first.
    PowerState {
        #[command(flatten)]
        target: TargetArg,
        /// Report only this named power level (its state_cmd); `default`
        /// reports only the default level.
        #[arg(long, value_name = "NAME")]
        level: Option<String>,
    },
    /// Check required configuration and probe channels against reality over SSH.
    ///
    /// One line per channel: target, channel, @host, status, detail. A power,
    /// hid, usb, or actions channel set with `--doctor` also runs its helper's
    /// own read-only check (`<program> <transport options> doctor --json`,
    /// 10 s limit) and lists each result under the channel as `ok`, `warn`, or
    /// `FAIL`; a helper without that check shows `no-check`. Exits 1 when any
    /// line is a problem (MISSING, unreachable, CONFLICT, MISMATCH, FAIL);
    /// `warn` and `no-check` are not problems.
    Doctor {
        /// Target to check (default: all).
        target: Option<String>,
        /// Only check channels on this host.
        #[arg(long)]
        host: Option<String>,
    },
    /// List this host's hardware for lab authoring (USB-Ethernet, serial, capture).
    Discover {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Propose a `[targets.<name>]` lab block from hardware discovered on a host.
    Configure {
        /// Target name to propose a block for.
        target: String,
        /// Lab host the target is wired to ('local' or a declared host).
        #[arg(long, short = 'H')]
        host: String,
    },
    /// Build and install paniolo's binaries (daemons + CLI) from a source clone.
    Setup {
        /// Provision this lab host over SSH instead of locally (requires the
        /// paniolo CLI + source already on the host).
        #[arg(long)]
        host: Option<String>,
        /// Do only the steps that need nothing beyond cargo: build and
        /// install the Rust crates, copy the bundled agent skills, and drop
        /// stale helper copies. Skips whatever needs sudo or a second
        /// toolchain — the OCR helper and zigplug everywhere, the setuid
        /// bpf-helper on macOS, the device-group check on Linux. The fast
        /// path for iterating on the Rust code.
        #[arg(long)]
        rust_only: bool,
    },
    /// Serve runtime commands as MCP tools over stdio, for agent harnesses
    /// (experimental).
    ///
    /// Speaks newline-delimited JSON-RPC on stdin/stdout until stdin closes;
    /// configure it in the harness as a stdio server whose command is
    /// `paniolo mcp`. Tools: target_list, video_shot, video_read, serial_status, serial_evidence, serial_wait, serial_log,
    /// serial_send, hid_send, action_list, action_run, power_state, power_on,
    /// power_off, power_cycle.
    /// Each tool runs the
    /// matching paniolo command, so remote targets dispatch over SSH as usual.
    Mcp,
    /// Run a helper binary from paniolo's private libexec dir (omit NAME to
    /// list the installed helpers).
    ///
    /// Everything after the helper name goes to the helper, so
    /// paniolo helper serialcap --help shows the helper's own help.
    ///
    /// Runs locally unless --host or -t names where the hardware is:
    /// paniolo helper -t pi5 --channel power -- cambrionix -d /dev/ttyUSB0 state
    /// runs on the host the lab binds pi5's power channel to. Put these
    /// flags before the helper name.
    ///
    /// Helpers from a PANIOLO_PLUGIN_PATH root are listed as
    /// "<name><TAB>plugin: <root>" and never replace a bundled helper.
    Helper {
        /// Run on this lab host instead of locally.
        #[arg(long, conflicts_with = "target")]
        host: Option<String>,
        /// Run on this target's host: the host of --channel, or the one host
        /// all of its channels share.
        #[arg(short, long)]
        target: Option<String>,
        /// Channel whose host to run on (netboot, serial, power, video, hid,
        /// usb, adb). Requires -t.
        #[arg(long, requires = "target")]
        channel: Option<String>,
        /// Serial interface whose host to run on, when the target's serial
        /// interfaces are on more than one host. Requires --channel serial.
        #[arg(short, long, requires = "channel")]
        interface: Option<String>,
        /// Helper name (e.g. hdmicap, hidrig, zigplug), then the arguments
        /// passed through to it.
        #[arg(
            value_names = ["NAME", "ARGS"],
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },
    /// List the agent skills bundled with paniolo, or print one's SKILL.md
    /// (the guides that teach an agent how to drive a target). Omit NAME to
    /// list them. Skills from a PANIOLO_PLUGIN_PATH root are listed too,
    /// marked "plugin: <root>".
    Skill {
        /// Skill to print (e.g. paniolo, kvm-puppeting).
        name: Option<String>,
        /// Print the SKILL.md file path instead of its contents.
        #[arg(long)]
        path: bool,
    },
    /// List, stop, or restart paniolo's background daemons on this host.
    Daemons {
        #[command(subcommand)]
        cmd: Option<DaemonsCmd>,
    },
}

/// A runtime verb's target: named either positionally (`netboot stop pi5`) or
/// as `-t/--target` (`netboot stop -t pi5`), never both, and optional wherever
/// the lab has exactly one target.
///
/// Flattened into every runtime verb rather than spelled out per variant, so
/// the two spellings cannot drift apart again (GitHub #203) and so the
/// "specify one with -t" that [`resolve_single_target`] prints is true of
/// every command that can print it. The config verbs (`set`, `rm`, `add`)
/// deliberately do not use this: they require `-t`, because a command that
/// creates or destroys configuration should not act on an implicit target.
#[derive(clap::Args, Debug, Clone, Default)]
struct TargetArg {
    /// Target to act on (optional when the lab has exactly one).
    #[arg(value_name = "TARGET", conflicts_with = "target")]
    target_pos: Option<String>,
    /// The same target, named the way the config verbs name it. Give it this
    /// way or positionally, not both.
    #[arg(long, short)]
    target: Option<String>,
}

impl TargetArg {
    /// The target the user named, whichever spelling they used; `None` when
    /// they named none and the sole configured target should be resolved.
    ///
    /// The single place the two spellings are merged. Every runtime verb goes
    /// through it, so there is no per-verb merge to get wrong or forget.
    fn name(&self) -> Option<&str> {
        self.target_pos.as_deref().or(self.target.as_deref())
    }
}

/// `--doctor` / `--no-doctor` on the `set` verbs of the channels whose hooks
/// name a helper (power, hid, usb, action): opt the channel in to, or out
/// of, the helper's own read-only `doctor --json` check (#304).
#[derive(clap::Args, Debug, Clone, Default)]
struct DoctorFlag {
    /// Have `paniolo doctor` also run the helper's own read-only health
    /// check, `<program> <transport options> doctor --json`. Only for a
    /// helper that implements it (docs/dev/adding-power-helpers.md).
    #[arg(long, conflicts_with = "no_doctor")]
    doctor: bool,
    /// Stop running the helper's doctor check (the default).
    #[arg(long)]
    no_doctor: bool,
}

impl DoctorFlag {
    /// `Some(true)`/`Some(false)` when either flag was given, else `None`
    /// (leave the lab file's setting alone).
    fn value(&self) -> Option<bool> {
        match (self.doctor, self.no_doctor) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

#[derive(Subcommand)]
enum DaemonsCmd {
    /// List running daemons and stray helper processes (the default).
    List,
    /// Stop daemons — named ones, or every one with --all.
    Stop {
        /// Daemons as `paniolo daemons` lists them: a bare name (serialcap,
        /// zigplug, netbootd) stops every instance of that daemon; the
        /// `NAME[TARGET]` form (`'serialcap[pi5]'` — quote it, `[]` is a
        /// shell glob) stops one target's instance.
        names: Vec<String>,
        /// Stop every daemon, and TERM stray helper processes too.
        #[arg(long)]
        all: bool,
        /// SIGKILL anything still alive after the 3 s grace period.
        #[arg(long)]
        force: bool,
    },
    /// Restart capture daemons (serialcap, hdmicap) from their current
    /// binary — the clean fix after an upgrade leaves stale ones running.
    Restart {
        /// Daemons to restart, as `paniolo daemons` lists them: a bare name
        /// (serialcap, hdmicap) restarts every instance of that daemon; the
        /// `NAME[TARGET]` form (`'hdmicap[pi5]'` — quote it, `[]` is a shell
        /// glob) restarts one target's instance. Empty with --all restarts
        /// every restartable daemon.
        names: Vec<String>,
        /// Restart every running serialcap/hdmicap daemon.
        #[arg(long)]
        all: bool,
        /// Restart only daemons with proven binary content changes; unknown identity is skipped.
        #[arg(long)]
        stale: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Show the whole lab: hosts and targets, each channel with its host.
    Show,
    /// Print the active lab file path.
    Path,
    /// Open the raw lab file in $EDITOR.
    Edit,
}

#[derive(Subcommand)]
enum HostCmd {
    /// List control hosts.
    List,
    /// Show a host and the channels that live on it.
    Show { name: String },
    /// Declare a control host.
    Add {
        name: String,
        /// ssh destination: user@host, an ssh_config alias, or 'local'.
        #[arg(long)]
        ssh: String,
        /// Free-text description of this control host (its role, location, …).
        #[arg(long)]
        description: Option<String>,
        /// This host's FQDN, so a machine can recognize itself when the lab
        /// file is shared across hosts (matched against `hostname -f`).
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long)]
        identity: Option<String>,
        #[arg(long)]
        control_path: Option<String>,
        #[arg(long)]
        paniolo_cmd: Option<String>,
        /// Plugin roots on this host for out-of-tree helpers and skills (its
        /// PANIOLO_PLUGIN_PATH syntax), passed to the remote paniolo on every
        /// dispatch here.
        #[arg(long, value_name = "LIST")]
        plugin_path: Option<String>,
    },
    /// Update an existing host (only the options you pass change).
    Set {
        name: String,
        #[arg(long)]
        ssh: Option<String>,
        /// Free-text description of this control host (its role, location, …).
        #[arg(long)]
        description: Option<String>,
        /// This host's FQDN for self-recognition (matched against `hostname -f`).
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long)]
        identity: Option<String>,
        #[arg(long)]
        control_path: Option<String>,
        #[arg(long)]
        paniolo_cmd: Option<String>,
        /// Plugin roots on this host (its PANIOLO_PLUGIN_PATH syntax), passed
        /// to the remote paniolo on every dispatch here. Empty clears it.
        #[arg(long, value_name = "LIST")]
        plugin_path: Option<String>,
    },
    /// Remove a host (refused if any target still binds to it).
    Rm { name: String },
}

#[derive(Subcommand)]
enum TargetCmd {
    /// List targets with their host(s) and channels.
    List,
    /// Show target configuration(s), each channel with its resolved host.
    Show { name: Option<String> },
    /// Create a target.
    Add {
        name: String,
        #[arg(long)]
        host: Option<String>,
        /// Free-text description of the target (accepts `--note` as an alias).
        #[arg(long, alias = "note")]
        description: Option<String>,
    },
    /// Update a target's default host or description.
    Set {
        name: String,
        #[arg(long)]
        host: Option<String>,
        /// Free-text description of the target (accepts `--note` as an alias).
        #[arg(long, alias = "note")]
        description: Option<String>,
    },
    /// Rename a target, carrying all its channels.
    Rename { old: String, new: String },
    /// Remove a target and all its channels.
    Rm { name: String },
}

#[derive(Subcommand)]
enum SerialCmd {
    /// Add a named serial console to a target.
    Add {
        name: String,
        #[arg(long, short)]
        target: String,
        #[arg(long, short)]
        device: String,
        #[arg(long, default_value_t = 115200)]
        baud: i64,
        #[arg(long)]
        sense: Option<String>,
        /// This interface's FTDI DTR line is wired to the J2 power-button
        /// header — enables `serial dtr` / `serial reset` on it.
        #[arg(long)]
        power_button: bool,
        #[arg(long)]
        host: Option<String>,
        /// Fixed port for the target's serialcap daemon (it serves every
        /// interface, so set it on one). 0 or unset = OS-assigned.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Update an existing serial interface (only the options you pass change).
    Set {
        name: String,
        #[arg(long, short)]
        target: String,
        #[arg(long, short)]
        device: Option<String>,
        #[arg(long)]
        baud: Option<i64>,
        #[arg(long)]
        sense: Option<String>,
        /// Opt this interface in/out of DTR power-button control (`--power-button`
        /// to enable, `--power-button false` to revoke).
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        power_button: Option<bool>,
        #[arg(long)]
        host: Option<String>,
        /// Fixed port for the target's serialcap daemon; 0 clears it
        /// (OS-assigned). Takes effect when the daemon next starts.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Remove a named serial interface from a target.
    Rm {
        name: String,
        #[arg(long, short)]
        target: String,
    },
    /// Open an interactive serial console (via tio) on the channel's host.
    Connect {
        #[command(flatten)]
        target: TargetArg,
        /// Interface name (default: the only one).
        #[arg(long, short)]
        interface: Option<String>,
    },
    /// Start the serialcap daemon (owning every configured interface).
    Watch {
        #[command(flatten)]
        target: TargetArg,
        #[arg(long, default_value_t = serial::DEFAULT_PORT)]
        port: u16,
    },
    /// Stop the running serialcap daemon.
    Stop {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Send a line of input to the console through the running daemon.
    Send {
        /// With two positionals the first is the target (`serial send pi5
        /// "text"`); with one, it is the text itself (`serial send "text"`).
        #[arg(value_name = "TARGET|TEXT")]
        first: String,
        /// Text to send (when the first positional is the target).
        #[arg(value_name = "TEXT")]
        second: Option<String>,
        #[arg(long, short, conflicts_with = "second")]
        target: Option<String>,
        #[arg(long, short)]
        interface: Option<String>,
        /// Per-byte pacing in ms for slow polled consoles (0 = full rate).
        #[arg(long, default_value_t = 0)]
        pace_ms: u32,
        /// Don't append a carriage return after the text.
        #[arg(long)]
        no_newline: bool,
    },
    /// Print captured serial output (reads serialcap's on-disk log).
    Log {
        #[command(flatten)]
        target: TargetArg,
        #[arg(long, short)]
        interface: Option<String>,
        /// Show only the most recent N lines.
        #[arg(long, short = 'n')]
        tail: Option<u64>,
        /// Lowest line sequence number (inclusive).
        #[arg(long)]
        from: Option<u64>,
        /// Highest line sequence number (inclusive).
        #[arg(long)]
        to: Option<u64>,
        /// Only lines newer than this sequence number.
        #[arg(long)]
        since: Option<u64>,
        /// Keep raw bytes (ANSI/control) instead of cleaning.
        #[arg(long)]
        raw: bool,
        /// Emit JSON Lines instead of formatted text.
        #[arg(long)]
        json: bool,
        /// Exclude the current unterminated line.
        #[arg(long)]
        no_pending: bool,
        /// Fail (exit 100, daemon_down) instead of reading the on-disk log
        /// when the serialcap daemon is not running. Without it, a stopped
        /// daemon only prints a warning on stderr: the log may be stale.
        #[arg(long)]
        require_live: bool,
    },
    /// Read immutable received chunks as JSON without opening a device.
    /// Includes exact bytes, original RX times, capture epoch, generation,
    /// observation time and explicit retention/corruption diagnostics.
    Evidence {
        #[command(flatten)]
        target: TargetArg,
        /// Interface name (optional when only one is configured).
        #[arg(long, short)]
        interface: Option<String>,
        /// Capture epoch (defaults to latest retained); required with --since.
        #[arg(long)]
        epoch: Option<String>,
        /// Return RX sequences strictly after this cursor within --epoch.
        #[arg(long, requires = "epoch")]
        since: Option<u64>,
        /// Maximum raw bytes (1..=1048576, default 65536); JSON overhead is extra.
        /// Stops before oversized chunks and reports a continuation cursor.
        #[arg(long, default_value_t = serial::DEFAULT_EVIDENCE_BYTES, value_parser = clap::value_parser!(u64).range(1..=serial::MAX_EVIDENCE_BYTES))]
        max_bytes: u64,
    },
    /// Acquire fresh serial evidence until a pattern, deadline or byte cap.
    /// Returns JSON with timing, raw records and explicit stop status;
    /// never sends input, resets hardware or restarts a daemon.
    Wait {
        #[command(flatten)]
        target: TargetArg,
        /// Interface name (optional when only one is configured).
        #[arg(long, short)]
        interface: Option<String>,

        /// Capture epoch from serial status/evidence; prevents stale data matching.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        epoch: String,
        /// Exclusive RX chunk sequence boundary within that epoch.
        #[arg(long)]
        since: u64,
        /// Finite acquisition deadline in ms (0 = one immediate observation; max 1 hour).
        #[arg(long, default_value_t = 30000, value_parser = clap::value_parser!(u64).range(0..=3600000))]
        timeout_ms: u64,
        /// Raw evidence byte cap, including fault bytes (1..=1048576).
        #[arg(long, default_value_t = 65536, value_parser = clap::value_parser!(u64).range(1..=1048576))]
        max_bytes: u64,
        /// Total JSON stdout byte cap, including follow events (4096..=8388608).
        #[arg(long, default_value_t = 8388608, value_parser = clap::value_parser!(u64).range(4096..=8388608))]
        max_output_bytes: u64,
        /// Stop when this literal UTF-8 string appears in fresh raw bytes.
        /// A match is observation, not proof of target command execution.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        pattern: Option<String>,
        /// Include the trailing unterminated bytes acquired after the boundary.
        #[arg(long)]
        include_pending: bool,
        /// Deliver newline-terminated rows as NDJSON before the final result.
        /// The first row may start mid-line and is marked prefix_unknown.
        #[arg(long)]
        follow: bool,
    },
    /// Send input and wait for its reply as one locked exchange.
    ///
    /// Prints the `serial wait` result JSON (status, records, pending, …) plus
    /// sent_bytes. Only bytes received after the send count. Other
    /// transactions on the interface queue behind this one; `serial send`
    /// and dashboard keystrokes are refused while it runs. A match is
    /// observation, not proof the target executed the command; nothing is
    /// retried. Needs a running daemon (`paniolo serial watch`).
    Transact {
        /// Target to act on (optional when the lab has exactly one).
        #[arg(long, short)]
        target: Option<String>,
        /// Interface name (optional when only one is configured).
        #[arg(long, short)]
        interface: Option<String>,
        /// Stop when this literal UTF-8 string appears in the reply.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        until: String,
        /// Deadline in ms for the whole call: queueing behind other
        /// transactions, sending and waiting (1..=3600000).
        #[arg(long, default_value_t = 30000, value_parser = clap::value_parser!(u64).range(1..=3600000))]
        timeout_ms: u64,
        /// Raw reply byte cap (1..=1048576).
        #[arg(long, default_value_t = 65536, value_parser = clap::value_parser!(u64).range(1..=1048576))]
        max_bytes: u64,
        /// Per-byte pacing in ms for slow polled consoles (0 = full rate).
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=10000))]
        pace_ms: u32,
        /// Don't append a carriage return after the text.
        #[arg(long)]
        no_newline: bool,
        /// Text to send (put it after `--` if it starts with `-`).
        #[arg(value_name = "TEXT")]
        text: String,
    },
    /// List available serial devices on this machine.
    Devices,
    /// Read serial health as JSON without starting or changing the daemon.
    /// Process liveness, port connection and RX traffic are separate evidence;
    /// none proves that a target executed a command.
    Status {
        #[command(flatten)]
        target: TargetArg,
        #[arg(long, short)]
        interface: Option<String>,
    },
    /// Show a target's serial interfaces and the daemon status.
    Show {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Pulse the DTR line (J2 power-button header) on a serial interface.
    Dtr {
        #[command(flatten)]
        target: TargetArg,
        /// Pulse duration in ms (≤500 = soft power-button event, ≥3000 = hard off).
        #[arg(long, default_value_t = 200)]
        ms: u64,
        /// Interface name (default: the power channel's serial_interface, or the
        /// only one).
        #[arg(long, short)]
        interface: Option<String>,
    },
    /// Send a soft-reset signal via a brief J2 power-button press.
    Reset {
        #[command(flatten)]
        target: TargetArg,
        #[arg(long, default_value_t = 200)]
        ms: u64,
        #[arg(long, short)]
        interface: Option<String>,
    },
}

#[derive(Subcommand)]
enum NetbootCmd {
    /// Configure the target's netboot channel (one per target).
    Set {
        #[arg(long, short)]
        target: String,
        #[arg(long, short)]
        interface: Option<String>,
        #[arg(long)]
        host_ip: Option<String>,
        #[arg(long, short = 'r')]
        tftp_root: Option<String>,
        /// Boot program served to UEFI clients (filename under tftp_root, e.g.
        /// grubaa64.efi). Bare TFTP filename for PXE; wrapped in an http:// URL
        /// for HTTP Boot.
        #[arg(long)]
        boot_file: Option<String>,
        /// HTTP server port, also embedded in the HTTP Boot URL (default 80).
        #[arg(long)]
        http_port: Option<String>,
        /// Content-Type for HTTP responses (default application/octet-stream).
        #[arg(long)]
        content_type: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Remove the target's netboot channel.
    Rm {
        #[arg(long, short)]
        target: String,
    },
    /// Start DHCP+TFTP netboot (the netbootd daemon) for a target.
    Start {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Stop netboot and restore the interface.
    Stop {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Show netboot daemon status.
    Status {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Show the netboot log (combined DHCP+TFTP).
    Logs {
        #[command(flatten)]
        target: TargetArg,
        /// Show only the most recent N lines.
        #[arg(long, short = 'n', default_value_t = 50)]
        tail: usize,
        /// Keep printing new lines as they arrive.
        #[arg(long, short)]
        follow: bool,
    },
    /// Print the target's TFTP root path (exit 3 if no root is configured).
    TftpRoot {
        #[command(flatten)]
        target: TargetArg,
    },
    /// List candidate USB-Ethernet interfaces on this machine.
    Devices,
}

#[derive(Subcommand)]
enum NetifCmd {
    /// Switch the link mode: netboot | link | ffx | off (idempotent).
    ///
    /// `link` brings just the host IP up (no DHCP/TFTP daemon, no ffx
    /// link-local) and `off` releases it — together they test the bare link
    /// up/down. `off` releases the IP only; it does not force the carrier down.
    Mode {
        /// netboot | link | ffx | off.
        mode: String,
        #[command(flatten)]
        target: TargetArg,
    },
    /// Force the link down hard: release addresses, disable Wake-on-LAN, and
    /// admin-down the interface so the peer sees carrier loss.
    ///
    /// `mode off` only releases the host IP and can leave the carrier up (a NIC
    /// with Wake-on-LAN keeps the PHY energized) — use this when the target must
    /// actually detect link loss. Bring it back with `mode link`/`mode netboot`.
    DownHard {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Show which mode the link is in, its carrier state, and its addresses.
    Status {
        #[command(flatten)]
        target: TargetArg,
    },
}

#[derive(Subcommand)]
enum PowerCmd {
    /// Configure the target's power channel (one per target), or one named power level.
    ///
    /// With --level NAME the hook flags set that level's hooks instead of the
    /// default ones; the level is created on first use and needs at least one
    /// hook. --serial-interface and --host belong to the whole channel, and
    /// so does --doctor, which covers the default hooks and every level's.
    Set {
        #[arg(long, short)]
        target: String,
        /// Set the hooks of this named power level (e.g. `hard`) rather than
        /// the default ones.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["serial_interface", "host"])]
        level: Option<String>,
        /// Shell command to power-cycle the target.
        #[arg(long)]
        cycle_cmd: Option<String>,
        /// Shell command to power the target on.
        #[arg(long)]
        on_cmd: Option<String>,
        /// Shell command to power the target off.
        #[arg(long)]
        off_cmd: Option<String>,
        /// Shell command to query power state (stdout must begin with 'on' or 'off').
        #[arg(long)]
        state_cmd: Option<String>,
        /// Serial interface whose sense line reports power state when the
        /// default level has no state_cmd. Whole channel; not with --level.
        #[arg(long)]
        serial_interface: Option<String>,
        /// Control host the power channel (every level) runs on. Not with --level.
        #[arg(long)]
        host: Option<String>,
        #[command(flatten)]
        doctor: DoctorFlag,
    },
    /// Remove the target's power channel, or with --level only that power level.
    Rm {
        #[arg(long, short)]
        target: String,
        /// Remove only this named power level, keeping the rest of the channel.
        #[arg(long, value_name = "NAME")]
        level: Option<String>,
    },
    /// Power on the target via the configured on_cmd.
    On {
        #[command(flatten)]
        target: TargetArg,
        /// Use this named power level's on_cmd instead of the default one.
        #[arg(long, value_name = "NAME")]
        level: Option<String>,
    },
    /// Power off the target via the configured off_cmd.
    Off {
        #[command(flatten)]
        target: TargetArg,
        /// Use this named power level's off_cmd instead of the default one.
        #[arg(long, value_name = "NAME")]
        level: Option<String>,
    },
}

#[derive(Subcommand)]
enum ActionCmd {
    /// List the named actions of one target, or of every target.
    List {
        /// Target to list (default: every target).
        #[arg(long, short)]
        target: Option<String>,
        /// Print JSON: an array of {target, name, description, requires_power, host}.
        #[arg(long)]
        json: bool,
    },
    /// Run one named action on the host its actions are bound to.
    Run {
        /// Target (optional when the lab has exactly one).
        #[arg(long, short)]
        target: Option<String>,
        /// Action to run, as `action list` names it.
        name: String,
        /// Internal: the near side already checked requires_power (set on
        /// dispatch to the action's host, which may not have the power
        /// channel).
        #[arg(long, hide = true)]
        power_checked: bool,
    },
    /// Create or update a named action. `--host` and `--doctor`/`--no-doctor`
    /// apply to all of the target's actions.
    Set {
        #[arg(long, short)]
        target: String,
        /// Action name (letters, digits, `.`, `_`, `-`).
        name: String,
        /// Shell command to run (required for a new action). Runs like a
        /// power hook: `sh -c`, with paniolo's helper dir first on PATH.
        #[arg(long)]
        cmd: Option<String>,
        /// One line saying what the action does, shown by `action list`.
        #[arg(long)]
        description: Option<String>,
        /// Refuse to run unless the target's power is on/off; `none` clears it.
        #[arg(long, value_parser = ["on", "off", "none"])]
        requires_power: Option<String>,
        /// Control host for all of this target's actions.
        #[arg(long)]
        host: Option<String>,
        // `--doctor`/`--no-doctor` apply to all of this target's actions,
        // like `--host`: each action program's doctor check runs once.
        #[command(flatten)]
        doctor: DoctorFlag,
    },
    /// Remove a named action (the channel goes with the last one).
    Rm {
        #[arg(long, short)]
        target: String,
        name: String,
    },
}

#[derive(Subcommand)]
enum HidCmd {
    /// Configure the target's hid channel (one per target).
    Set {
        #[arg(long, short)]
        target: String,
        /// Injection helper command; `hid send` arguments are appended to it
        /// (e.g. "hidrig -d /dev/cu.usbserial-XXXX").
        #[arg(long)]
        cmd: String,
        #[arg(long)]
        host: Option<String>,
        /// Fixed port for the hid daemon (`<cmd> serve --port N`); 0 clears
        /// it (OS-assigned). Takes effect when the daemon next starts.
        #[arg(long)]
        port: Option<u16>,
        #[command(flatten)]
        doctor: DoctorFlag,
    },
    /// Remove the target's hid channel.
    Rm {
        #[arg(long, short)]
        target: String,
    },
    /// Run the configured helper with the given arguments appended,
    /// e.g. `paniolo hid send -t pi5 type hello`.
    Send {
        #[arg(long, short)]
        target: Option<String>,
        /// Arguments appended to the configured cmd (the helper's CLI).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Start the injection daemon (the KVM path): the helper owns the UART and
    /// re-exposes it over a WebSocket. `paniolo console` starts it on demand;
    /// run this to warm it ahead of time. Idempotent.
    Serve {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Stop the running injection daemon.
    Stop {
        #[command(flatten)]
        target: TargetArg,
    },
}

#[derive(Subcommand)]
enum UsbCmd {
    /// Configure the target's usb channel (one per target).
    Set {
        #[arg(long, short)]
        target: String,
        /// Mux helper command. paniolo appends a fixed verb — `usb host`,
        /// `usb target`, or `usb state` (e.g. "ch9329 -d /dev/cu.usbmodemXXXX").
        #[arg(long)]
        cmd: String,
        #[arg(long)]
        host: Option<String>,
        #[command(flatten)]
        doctor: DoctorFlag,
    },
    /// Remove the target's usb channel.
    Rm {
        #[arg(long, short)]
        target: String,
    },
    /// Route the shared device to the control host — where you can write an
    /// image to it, then hand it to the target with `attach-target`.
    AttachHost {
        #[arg(long, short)]
        target: Option<String>,
    },
    /// Route the shared device to the target, where it appears as ordinary
    /// physical media (visible to firmware, unlike streamed virtual media).
    AttachTarget {
        #[arg(long, short)]
        target: Option<String>,
    },
    /// Report which side the shared device is on. Always ask rather than
    /// assuming: the position does not survive the KVM losing power.
    State {
        #[arg(long, short)]
        target: Option<String>,
    },
}

#[derive(Subcommand)]
enum AdbCmd {
    /// Configure the target's adb channel (one per target).
    Set {
        #[arg(long, short)]
        target: String,
        /// `adb -s <serial>` device id (omit for the sole attached device).
        #[arg(long)]
        serial: Option<String>,
        /// Override the adb binary (default: adb on PATH).
        #[arg(long)]
        adb: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Remove the target's adb channel.
    Rm {
        #[arg(long, short)]
        target: String,
    },
    /// Show the target's adb channel config and device state.
    Show {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Open an interactive `adb shell` on the channel's host.
    Shell {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Run a one-shot command on the device, e.g.
    /// `paniolo adb run -t pixel getprop ro.build.version.release`.
    Run {
        #[arg(long, short)]
        target: Option<String>,
        /// Command (and args) run via `adb shell`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Capture one PNG screenshot (`adb exec-out screencap`).
    Screencap {
        #[command(flatten)]
        target: TargetArg,
        /// Output path; "-" for stdout.
        #[arg(long, short, default_value = "-")]
        out: String,
    },
    /// Inject input events, e.g.
    /// `paniolo adb input -t pixel keyevent KEYCODE_HOME`.
    Input {
        #[arg(long, short)]
        target: Option<String>,
        /// Arguments to `adb shell input` (keyevent/text/tap/swipe …).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// List adb devices visible on a host (default: local).
    Devices {
        #[arg(long, short = 'H')]
        host: Option<String>,
    },
}

#[derive(Subcommand)]
enum VideoCmd {
    /// Configure the target's video channel (one per target).
    Set {
        #[arg(long, short)]
        target: String,
        /// Capture device: an hdmicap device name substring, index, or /dev path;
        /// or a network RFB source: `rfb+hid:` (the RFB feed of this target's
        /// own hid daemon, e.g. a network KVM; needs a hid channel on the same
        /// host, set first) or `rfb://HOST:PORT` (plain RFB over TCP, no
        /// authentication).
        #[arg(long, short)]
        device: String,
        /// OCR engine selection: "gui" picks the GUI-tuned engine (only
        /// meaningful on Linux; see docs/video.md "OCR"), "text" is the
        /// platform default. Leave unset for the default on every platform.
        #[arg(long, value_parser = model::VALID_OCR_MODES)]
        ocr_mode: Option<String>,
        /// Frame-change sensitivity, 0-255: how far any single pixel's luma
        /// must move from the last accepted frame before the frame hash
        /// changes (`video shot --changed-since`, `--stable`). Smaller moves
        /// are capture noise. Default 16 when unset; 0 = exact (any differing
        /// pixel counts). A lossy H.264 source (network KVM) was measured at
        /// noise of at most 8 levels on an unchanged screen. Takes effect when
        /// the daemon next starts (`video watch --restart`).
        #[arg(long, value_parser = clap::value_parser!(u8))]
        change_threshold: Option<u8>,
        #[arg(long)]
        host: Option<String>,
        /// Fixed port for the hdmicap daemon, so a standing `ssh -L` reaches
        /// the dashboard across restarts; 0 clears it (OS-assigned). Takes
        /// effect when the daemon next starts (`video watch --restart`).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Remove the target's video channel.
    Rm {
        #[arg(long, short)]
        target: String,
    },
    /// Start the hdmicap warm-stream daemon for the target's capture device.
    Watch {
        #[command(flatten)]
        target: TargetArg,
        #[arg(long, default_value_t = video::DEFAULT_PORT)]
        port: u16,
        /// Force-restart a running (possibly stalled) daemon.
        #[arg(long)]
        restart: bool,
    },
    /// Stop the running hdmicap daemon.
    Stop {
        #[command(flatten)]
        target: TargetArg,
    },
    /// Fetch one PNG screenshot from the running daemon.
    Shot {
        #[command(flatten)]
        target: TargetArg,
        /// Wait until the signal is stable before capturing.
        #[arg(long)]
        stable: bool,
        /// Only return once the frame differs from this hex hash.
        #[arg(long)]
        changed_since: Option<String>,
        /// Timeout in ms.
        #[arg(long, default_value_t = 2000)]
        timeout: u64,
        /// Output path; "-" for stdout.
        #[arg(long, short, default_value = "-")]
        out: String,
    },
    /// OCR the current frame via the running daemon, printing the text.
    Read {
        #[command(flatten)]
        target: TargetArg,
        /// Wait until the signal is stable before reading.
        #[arg(long)]
        stable: bool,
        /// Timeout in ms for the stable wait.
        #[arg(long, default_value_t = 2000)]
        timeout: u64,
        /// Emit the full OCR envelope (per-line text, confidence, bbox, and
        /// which engine produced it) instead of just the text. See docs/dev/ocr.md.
        #[arg(long)]
        json: bool,
    },
    /// Print the live-preview URL of the target's running daemon.
    ///
    /// The URL carries the daemon's token (a browser can present it no other
    /// way), so the output is a credential: paste it into a browser, not into
    /// a log. `--open` hands it to the browser instead and prints only the
    /// token-free address.
    Preview {
        #[command(flatten)]
        target: TargetArg,
        /// Open the URL in the default browser instead of printing it.
        #[arg(long)]
        open: bool,
    },
    /// List available capture devices.
    Devices,
    /// Show the target's video channel and daemon status.
    Show {
        #[command(flatten)]
        target: TargetArg,
    },
}

fn main() {
    error::exit_quietly_on_closed_stdout();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => std::process::exit(error::report_parse_error(&e, &args)),
    };
    // Single-threaded here, before any child is spawned: record the request
    // and drop the variable, so no hook or helper inherits it (one JSON
    // object per failure). Dispatch passes `--json-errors` to a remote.
    error::take_json_request(cli.json_errors);
    // Also single-threaded: `--plugin-path` becomes PANIOLO_PLUGIN_PATH, so
    // the helper and skill search here, and every hook, helper and daemon
    // this process starts, see the same roots (#303). An argument rather than
    // a forwarded variable for the same reason as `--json-errors`: it crosses
    // interactive dispatch and a non-POSIX control host too.
    if let Some(p) = &cli.plugin_path {
        // Safe: no other thread exists yet.
        unsafe { std::env::set_var(daemons::PLUGIN_PATH_ENV, p) };
    }
    if let Err(e) = run(cli) {
        // `report` prints the full `.context()` chain (`{e:#}`), not just the
        // outermost message — a bare `{e}` drops every cause a command added
        // on the way up (Review low #7) — then exits with the kind's code.
        std::process::exit(error::report(&e));
    }
}

fn run(cli: Cli) -> Result<()> {
    let lab_flag = cli.lab.as_deref();
    match cli.command {
        Command::Init { path } => cmd_init(lab_flag, path.as_deref()),
        Command::Config { cmd } => match cmd {
            ConfigCmd::Show => config_show(lab_flag),
            ConfigCmd::Path => config_path(lab_flag),
            ConfigCmd::Edit => config_edit(lab_flag),
        },
        Command::Host { cmd } => host_cmd(lab_flag, cmd),
        Command::Target { cmd } => target_cmd(lab_flag, cmd),
        Command::Serial { cmd } => serial_cmd(lab_flag, cmd),
        Command::Netboot { cmd } => netboot_cmd(lab_flag, cmd),
        Command::Netif { cmd } => netif_cmd(lab_flag, cmd),
        Command::Power { cmd } => power_cmd(lab_flag, cmd),
        Command::Video { cmd } => video_cmd(lab_flag, cmd),
        Command::Hid { cmd } => hid_cmd(lab_flag, cmd),
        Command::Usb { cmd } => usb_cmd(lab_flag, cmd),
        Command::Action { cmd } => action_cmd(lab_flag, cmd),
        Command::Adb { cmd } => adb_cmd(lab_flag, cmd),
        Command::Console {
            target,
            interface,
            open,
            print_url,
        } => cmd_console(
            lab_flag,
            target.name(),
            interface.as_deref(),
            ConsoleView { open, print_url },
        ),
        Command::PowerCycle { target, level } => {
            cmd_power_cycle(lab_flag, target.name(), level.as_deref())
        }
        Command::PowerState { target, level } => {
            cmd_power_state(lab_flag, target.name(), level.as_deref())
        }
        Command::Doctor { target, host } => {
            cmd_doctor(lab_flag, target.as_deref(), host.as_deref())
        }
        Command::Discover { json } => cmd_discover(json),
        Command::Configure { target, host } => cmd_configure(lab_flag, &target, &host),
        Command::Setup { host, rust_only } => cmd_setup(lab_flag, host.as_deref(), rust_only),
        Command::Mcp => mcp::serve(lab_flag),
        Command::Helper {
            host,
            target,
            channel,
            interface,
            command,
        } => cmd_helper_at(
            lab_flag,
            HelperPlace {
                host: host.as_deref(),
                target: target.as_deref(),
                channel: channel.as_deref(),
                interface: interface.as_deref(),
            },
            command.first().map(String::as_str),
            command.get(1..).unwrap_or_default(),
        ),
        Command::Skill { name, path } => skills::run(name.as_deref(), path),
        Command::Daemons { cmd } => match cmd.unwrap_or(DaemonsCmd::List) {
            DaemonsCmd::List => cmd_daemons_list(),
            DaemonsCmd::Stop { names, all, force } => cmd_daemons_stop(&names, all, force),
            DaemonsCmd::Restart { names, all, stale } => {
                cmd_daemons_restart(lab_flag, &names, all, stale)
            }
        },
    }
}

// ── daemon inventory ────────────────────────────────────────────────────────

/// Running netboot engines, found via their per-target state files.
use state::running_netboots;

/// A daemon named on the `daemons stop` / `daemons restart` command line, in
/// either form `paniolo daemons` prints: bare `serialcap` (every instance of
/// that daemon) or `serialcap[pi5]` (one target's instance). netbootd's
/// instances are its targets, so `netbootd[pi5]` reads the same way.
struct DaemonSpec {
    name: String,
    instance: Option<String>,
}

impl DaemonSpec {
    fn parse(arg: &str) -> Self {
        match arg.strip_suffix(']').and_then(|s| s.split_once('[')) {
            Some((name, instance)) => DaemonSpec {
                name: name.to_string(),
                instance: Some(instance.to_string()),
            },
            None => DaemonSpec {
                name: arg.to_string(),
                instance: None,
            },
        }
    }

    /// Does this spec select the daemon `name` running as `instance`?
    fn matches(&self, name: &str, instance: Option<&str>) -> bool {
        self.name == name
            && match self.instance.as_deref() {
                Some(want) => Some(want) == instance,
                None => true,
            }
    }
}

impl std::fmt::Display for DaemonSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.instance {
            Some(inst) => write!(f, "{}[{inst}]", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// `serialcap[pi5]` / `zigplug` — the name `paniolo daemons` prints for a
/// discovered daemon, and the form [`DaemonSpec`] accepts back.
fn daemon_label(d: &daemons::DaemonInfo) -> String {
    match &d.instance {
        Some(inst) => format!("{}[{inst}]", d.name),
        None => d.name.clone(),
    }
}

/// Escape ASCII/C1 control characters (`\x00`–`\x1F`, `\x7F`–`\x9F`) as
/// `\xHH`; everything else — including non-ASCII UTF-8 — passes through
/// unchanged. For text that is *not* paniolo's own: another process's raw
/// argv (`paniolo daemons`' stray-helper listing), which may contain
/// terminal escape sequences (cursor moves, screen clears, OSC payloads) a
/// hostile or merely weird process chose to put there. Printing it verbatim
/// would let that process draw into this terminal (Review low #12).
fn escape_control_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Command-line needles that identify a discovery daemon's process: the
/// daemon's own binary name, except the `hid` channel, which any conforming
/// injector may serve (the mapping `cmd_helper` applies, in reverse).
fn daemon_process_needles(name: &str) -> Vec<String> {
    match name {
        HID_DAEMON => vec![
            "hidrig".to_string(),
            "ch9329".to_string(),
            "jetkvm".to_string(),
        ],
        n => vec![n.to_string()],
    }
}

/// True if `pid` is alive and its command line names one of `needles` —
/// `state::is_named_child_alive` over several candidate names. A discovery
/// file's pid is checked this way right before it is signalled, so a pid the
/// kernel has since recycled for something else is skipped, not killed.
fn pid_runs_one_of(pid: i32, needles: &[String]) -> bool {
    if !state::is_pid_alive(pid) {
        return false;
    }
    let cmdline = state::pid_cmdline(pid);
    needles.iter().any(|n| cmdline.contains(n.as_str()))
}

/// Send `signal` to `pid`, printing the `TERM`/`KILL` line only once it has
/// actually been delivered; a refusal (EPERM for a process that is not ours)
/// is reported instead. Returns whether it landed.
fn signal_or_report(label: &str, pid: i32, signal: platform::Signal) -> bool {
    match platform::try_signal_pid(pid, signal) {
        Ok(()) => {
            let verb = match signal {
                platform::Signal::Term => "TERM",
                platform::Signal::Kill => "KILL",
            };
            println!("{verb} {label} (pid {pid})");
            true
        }
        Err(e) => {
            let why = if e.kind() == std::io::ErrorKind::PermissionDenied {
                "permission denied".to_string()
            } else {
                e.to_string()
            };
            eprintln!("could not signal {label} (pid {pid}): {why}");
            false
        }
    }
}

/// One unified view of every paniolo background process on this host: the
/// discovery-file daemons, netbootd (state files), and stray helper
/// processes running out of the libexec dir.
fn cmd_daemons_list() -> Result<()> {
    let discovered = daemons::list_discovered();
    let netboots = running_netboots();
    let mut known: Vec<i32> = discovered.iter().map(|d| d.pid).collect();
    known.extend(netboots.iter().map(|(_, st)| st.dhcp_pid));
    // A stray running a `daemon` subcommand is not a wedged one-shot: it is a
    // daemon whose discovery file went missing under it (#187). It still holds
    // its device, so it is reported as what it is, with the verb that reclaims
    // it — not buried in the stray list with a shrug.
    let mut untracked: Vec<daemons::Untracked> = Vec::new();
    let mut strays: Vec<(i32, String)> = Vec::new();
    for (pid, args) in daemons::list_stray_helpers(&known) {
        match daemons::untracked_of(pid, &args) {
            Some(u) => untracked.push(u),
            None => strays.push((pid, args)),
        }
    }

    if discovered.is_empty() && netboots.is_empty() && strays.is_empty() && untracked.is_empty() {
        println!("No paniolo daemons running.");
        return Ok(());
    }
    let mut any_stale = false;
    for d in &discovered {
        let port = d.port.map_or("-".to_string(), |p| p.to_string());
        let name = daemon_label(d);
        any_stale |= d.binary_status.is_stale() == Some(true);
        let stale = d.binary_status.note();
        println!("{name}\tpid {}\tport {}\t{}{stale}", d.pid, port, d.detail);
    }
    if any_stale {
        println!("\nStale daemons run an older binary than what's installed now.");
        println!("Restart them from the current binary with `paniolo daemons restart --stale`.");
    }
    for (target, st) in &netboots {
        println!(
            "netbootd\tpid {}\tport -\ttarget {target} ({})",
            st.dhcp_pid, st.interface
        );
    }
    if !untracked.is_empty() {
        println!("\nUntracked daemons (running, but no discovery file names them):");
        for u in &untracked {
            println!(
                "  {}\tpid {}\tport ?\t{}",
                u.name,
                u.pid,
                u.devices_display()
            );
        }
        println!(
            "These still hold their devices. `paniolo video watch` / `paniolo serial watch` \n\
             reclaim a capture one; `paniolo daemons stop --all` reaps the rest."
        );
    }
    if !strays.is_empty() {
        println!("\nStray helper processes (not daemons — wedged one-shots?):");
        for (pid, args) in &strays {
            // `args` is another process's raw command line — not paniolo's
            // own text — so it may carry ANSI/terminal escape sequences a
            // hostile or merely weird process chose to put in argv. Escaping
            // control characters keeps `paniolo daemons` from replaying them
            // into this terminal (Review low #12).
            println!("  pid {pid}\t{}", escape_control_chars(args));
        }
        println!("Stop everything with `paniolo daemons stop --all [--force]`.");
    }
    Ok(())
}

/// Stop daemons by name or wholesale: netbootd via its proper teardown
/// (interface cleanup), everything else via SIGTERM, with an optional
/// SIGKILL escalation after a grace period. A pid from a discovery file is
/// signalled only while its command line still names the daemon (a recycled
/// pid is skipped with a message), and a signal the OS refuses is reported
/// and fails the command instead of being printed as if it had landed.
fn cmd_daemons_stop(names: &[String], all: bool, force: bool) -> Result<()> {
    if !all && names.is_empty() {
        bail!("name one or more daemons (see `paniolo daemons`), or pass --all");
    }
    let specs: Vec<DaemonSpec> = names.iter().map(|n| DaemonSpec::parse(n)).collect();
    let wanted =
        |name: &str, instance: Option<&str>| all || specs.iter().any(|s| s.matches(name, instance));

    // netbootd first — its stop also restores the interface.
    let netboots = running_netboots();
    for (target, _) in &netboots {
        if wanted("netbootd", Some(target)) {
            netboot::stop(target)?;
            println!("netbootd stopped (target {target}).");
        }
    }

    let discovered = daemons::list_discovered();
    for spec in &specs {
        let running = if spec.name == "netbootd" {
            netboots
                .iter()
                .any(|(t, _)| spec.matches("netbootd", Some(t)))
        } else {
            discovered
                .iter()
                .any(|d| spec.matches(&d.name, d.instance.as_deref()))
        };
        if !running {
            eprintln!("warning: no running daemon named '{spec}'");
        }
    }

    // (label, pid, command-line needles that prove the pid is still it)
    let mut victims: Vec<(String, i32, Vec<String>)> = discovered
        .iter()
        .filter(|d| wanted(&d.name, d.instance.as_deref()))
        .map(|d| (daemon_label(d), d.pid, daemon_process_needles(&d.name)))
        .collect();
    let known: Vec<i32> = victims.iter().map(|(_, pid, _)| *pid).collect();
    if all {
        for (pid, args) in daemons::list_stray_helpers(&known) {
            let short = args
                .split_whitespace()
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
            // A stray was just seen running out of a helper dir; that program
            // is what must still be on the pid when it is signalled.
            let program = args
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            victims.push((format!("stray: {short}"), pid, vec![program]));
        }
    }
    if victims.is_empty() {
        println!("Nothing to stop.");
        return Ok(());
    }

    let mut failed = false;
    let mut pending: Vec<(String, i32, Vec<String>)> = Vec::new();
    for (label, pid, needles) in victims {
        if !pid_runs_one_of(pid, &needles) {
            eprintln!(
                "skipping {label} (pid {pid}): no longer running {}",
                needles.join("/")
            );
            continue;
        }
        if signal_or_report(&label, pid, platform::Signal::Term) {
            pending.push((label, pid, needles));
        } else {
            failed = true;
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        pending.retain(|(_, pid, _)| state::is_pid_alive(*pid));
        if pending.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    // Name-verified again: the grace period is long enough for a pid to be
    // recycled, and a KILL must never reach the newcomer.
    pending.retain(|(_, pid, needles)| pid_runs_one_of(*pid, needles));
    for (label, pid, _) in &pending {
        if force {
            if !signal_or_report(label, *pid, platform::Signal::Kill) {
                failed = true;
            }
        } else {
            eprintln!("still alive: {label} (pid {pid}) — re-run with --force to SIGKILL");
        }
    }
    if failed || (!force && !pending.is_empty()) {
        // Not a "negative answer" (exit 1 under the error contract): some
        // daemon the caller asked to stop is still running.
        return Err(error::PanioloError::new(
            error::Kind::Internal,
            "not every daemon was stopped (see above)",
        )
        .into());
    }
    Ok(())
}

/// Restart the per-target capture daemons (serialcap, hdmicap) from their
/// current on-disk binary. This is the safe heal after an upgrade or rebuild
/// leaves stale daemons running: the package can't reap per-user daemons, so
/// the operator restarts them, reusing the lab's channel config so the new
/// daemon owns the same devices. netbootd is intentionally excluded — restart
/// it via `paniolo netboot start/stop`, since that touches an in-flight boot.
fn cmd_daemons_restart(
    lab_flag: Option<&str>,
    names: &[String],
    all: bool,
    stale_only: bool,
) -> Result<()> {
    if !all && !stale_only && names.is_empty() {
        bail!("name one or more daemons (serialcap, hdmicap), or pass --all / --stale");
    }
    let lab = load_for_read(lab_flag)?;
    let restartable = [serial::DAEMON, video::DAEMON];
    let specs: Vec<DaemonSpec> = names.iter().map(|n| DaemonSpec::parse(n)).collect();
    for spec in &specs {
        if !restartable.contains(&spec.name.as_str()) {
            eprintln!(
                "warning: '{spec}' is not a restartable capture daemon \
                 (serialcap, hdmicap); skipping"
            );
        }
    }

    let selected: Vec<(String, String)> = daemons::list_discovered()
        .into_iter()
        .filter(|d| restartable.contains(&d.name.as_str()))
        .filter(|d| {
            all || stale_only
                || specs
                    .iter()
                    .any(|s| s.matches(&d.name, d.instance.as_deref()))
        })
        .filter(|d| !stale_only || d.binary_status.is_stale() == Some(true))
        .filter_map(|d| d.instance.map(|inst| (d.name, inst)))
        .collect();
    if selected.is_empty() {
        println!("No matching capture daemons to restart.");
        return Ok(());
    }

    let mut failures = 0;
    for (name, target) in selected {
        if !lab.targets.contains_key(&target) {
            eprintln!("warning: {name}[{target}] has no matching lab target; skipping");
            failures += 1;
            continue;
        }
        match restart_capture_daemon(&lab, &name, &target) {
            Ok(url) => println!("{name}[{target}] restarted — {url}"),
            Err(e) => {
                eprintln!("{name}[{target}] restart failed: {e:#}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        return Err(error::PanioloError::new(
            error::Kind::Internal,
            format!("{failures} daemon restart(s) failed (see above)"),
        )
        .into());
    }
    Ok(())
}

/// Stop the `name` capture daemon for `target` and wait for its *process* to
/// exit — not just for its discovery file to clear, which a daemon releases
/// while still shutting down. Until the process is gone it owns the device (a
/// V4L2 node or a serial port can't be opened twice), and while it is alive
/// its discovery file can still satisfy a startup wait on behalf of a
/// replacement that actually failed to start. A process that overstays the
/// 5 s grace is SIGKILLed — after re-checking, by name, that the pid is still
/// the daemon — and one that survives even that is an error rather than a
/// silent race. Every path that replaces a running capture daemon comes
/// through here: `daemons restart`, and the stale-binary restart in
/// `serial watch` / `video watch`.
///
/// Returns the pid it stopped, if there was one, so the caller can refuse that
/// pid's leftover discovery file while waiting for the replacement — see
/// [`daemons::wait_for_replacement`].
///
/// Two things here exist because of GitHub #193, where a restart reported
/// success while the replacement had died on the old daemon's advisory lock:
/// the wait is for [`state::is_named_process_pending`] rather than a bare name
/// match (a SIGKILLed process reads as alive-but-nameless while the kernel is
/// still finishing with it, which released the device early), and the killed
/// daemon's discovery file is removed here, since it is not around to do it.
fn stop_capture_daemon_and_wait(name: &str, target: &str) -> Result<Option<i32>> {
    let old_pid = daemons::daemon_pid(name, Some(target));
    // The daemon's own `stop` owns the clean shutdown; it failing (already
    // gone, no discovery file) is not fatal — the pid wait is the real gate.
    let _ = if name == serial::DAEMON {
        serial::stop_daemon(target)
    } else {
        video::stop_daemon(target)
    };
    let Some(pid) = old_pid else {
        return Ok(None);
    };
    let pending = || state::is_named_process_pending(pid, name);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while pending() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if pending() {
        // Only signal a pid that is still identifiably the daemon; a pid in
        // teardown is already dying and one that was recycled is not ours.
        if state::is_named_child_alive(pid, name) {
            eprintln!("old {name} daemon (pid {pid}) ignored stop; killing it");
            platform::try_signal_pid(pid, platform::Signal::Kill)
                .map_err(|e| anyhow!("could not SIGKILL the old {name} daemon (pid {pid}): {e}"))?;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while pending() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if pending() {
            bail!(
                "old {name} daemon (pid {pid}) is still running after SIGKILL; not starting a \
                 replacement that would race it for the device"
            );
        }
    }
    // A daemon that did not exit gracefully leaves its discovery file behind.
    daemons::remove_discovery_for_pid(name, Some(target), pid);
    Ok(Some(pid))
}

/// TERM (then KILL) an untracked daemon and wait for the process to go.
///
/// The same gate as [`stop_capture_daemon_and_wait`] — until the process is
/// gone it still owns the device — but there is no discovery file to reach
/// this one through, so its own `stop` (an authenticated HTTP call to a port
/// only that file recorded) is not available and a signal is the only handle
/// left. See the "untracked daemons" note in daemons.rs for how a daemon ends
/// up in this state.
fn reap_untracked(u: &daemons::Untracked) -> Result<()> {
    let (pid, name) = (u.pid, u.name.clone());
    let still_it = || state::is_named_child_alive(pid, &name);
    if !still_it() {
        return Ok(());
    }
    eprintln!("Reaping untracked {name} daemon (pid {pid}) that still holds the device…");
    if !signal_or_report(&format!("untracked {name}"), pid, platform::Signal::Term) {
        bail!("could not stop the untracked {name} daemon (pid {pid})");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while still_it() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !still_it() {
        return Ok(());
    }
    eprintln!("untracked {name} daemon (pid {pid}) ignored TERM; killing it");
    platform::try_signal_pid(pid, platform::Signal::Kill)
        .map_err(|e| anyhow!("could not SIGKILL the untracked {name} daemon (pid {pid}): {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while still_it() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if still_it() {
        bail!(
            "untracked {name} daemon (pid {pid}) is still running after SIGKILL; not starting \
             a replacement that would race it for the device"
        );
    }
    Ok(())
}

/// Stop one capture daemon and start it again from the lab's channel config,
/// returning the new daemon's URL. The stop goes through
/// [`stop_capture_daemon_and_wait`], so the replacement never races the old
/// process for an exclusive device.
fn restart_capture_daemon(lab: &Lab, name: &str, target: &str) -> Result<String> {
    // Resolve start parameters up front so a stop failure can't strand us.
    enum Start {
        Serial(Vec<model::SerialChannel>),
        // Device plus the channel's OCR mode: the daemon is handed its OCR
        // helper at spawn, so the mode has to travel with the device.
        Video(String, Option<String>, Option<u8>),
    }
    let start = if name == serial::DAEMON {
        let serials = local_serials(lab, target)?;
        if serials.is_empty() {
            return Err(error::channel_missing(
                target,
                "serial",
                format!("no serial interfaces for '{target}' in the lab"),
            )
            .into());
        }
        Start::Serial(serials)
    } else if name == video::DAEMON {
        let v = local_video(lab, target)?;
        let device = v.device.clone().ok_or_else(|| {
            error::channel_missing(
                target,
                "video",
                format!("video channel for '{target}' has no device set"),
            )
        })?;
        Start::Video(device, v.ocr_mode.clone(), v.change_threshold)
    } else {
        bail!("'{name}' is not a restartable capture daemon");
    };

    // A fixed port from the lab survives the restart; unset stays OS-assigned.
    let t = lab.targets.get(target);
    let serial_port = t.map_or(0, model::Target::serial_port);
    let video_port = t
        .and_then(|t| t.video.as_ref())
        .map_or(0, |v| model::fixed_port(v.port));

    let replaced = stop_capture_daemon_and_wait(name, target)?;

    let hid_input = match &start {
        Start::Video(device, _, _) => ensure_video_source(lab, target, device)?,
        Start::Serial(_) => None,
    };
    let mut child = match start {
        Start::Serial(serials) => serial::start_daemon(&serials, serial_port, target)?,
        Start::Video(device, mode, threshold) => video::start_daemon(
            &device,
            video_port,
            target,
            mode.as_deref(),
            threshold,
            hid_input.as_deref(),
        )?,
    };
    wait_for_started_daemon(name, Some(target), replaced, Some(&mut child))
}

/// Wait for a just-spawned daemon to publish discovery, refusing the discovery
/// file of `replaced` (the pid this one is taking over from, if any) — a
/// SIGKILLed daemon's file outlives it, and accepting it reports a restart
/// that never happened (GitHub #193). Failure carries the daemon's own stderr.
fn wait_for_started_daemon(
    name: &str,
    instance: Option<&str>,
    replaced: Option<i32>,
    spawned: Option<&mut std::process::Child>,
) -> Result<String> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let url = match replaced {
        Some(old) => daemons::wait_for_replacement(name, instance, TIMEOUT, old),
        None => daemons::wait_for_daemon(name, instance, TIMEOUT),
    };
    url.ok_or_else(|| daemons::start_failure(name, instance, TIMEOUT, spawned))
}

// ── helper passthrough ──────────────────────────────────────────────────────

/// Where `paniolo helper` was asked to run (#300); all `None` means locally.
struct HelperPlace<'a> {
    host: Option<&'a str>,
    target: Option<&'a str>,
    channel: Option<&'a str>,
    interface: Option<&'a str>,
}

/// `paniolo helper`, run on the lab host `place` resolves to. Locally that is
/// [`cmd_helper`]; remotely it re-execs `paniolo helper -- NAME ARGS…` there
/// over SSH. The remote side needs no lab — `helper` never reads one — so no
/// slice is shipped, and the helper gets that host's own state/runtime dirs.
fn cmd_helper_at(
    lab_flag: Option<&str>,
    place: HelperPlace,
    name: Option<&str>,
    args: &[String],
) -> Result<()> {
    if place.host.is_none() && place.target.is_none() {
        return cmd_helper(name, args);
    }
    let lab = load_for_read(lab_flag)?;
    let host_name = dispatch::helper_host(
        &lab,
        place.host,
        place.target,
        place.channel,
        place.interface,
    )?;
    if lab.host(&host_name).is_local(&host_name) {
        return cmd_helper(name, args);
    }
    let code = dispatch::run_on_host(&lab, &host_name, &helper_remote_argv(name, args))?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// The remote argv for a dispatched `helper`: the `--` keeps the helper's own
/// flags (`--help` included) away from the remote paniolo's parser.
fn helper_remote_argv(name: Option<&str>, args: &[String]) -> Vec<String> {
    let mut argv = vec!["helper".to_string()];
    if let Some(name) = name {
        argv.push("--".to_string());
        argv.push(name.to_string());
        argv.extend(args.iter().cloned());
    }
    argv
}

/// Run a libexec helper with stdio passed through, propagating its exit code;
/// with no name, list the installed helpers.
fn cmd_helper(name: Option<&str>, args: &[String]) -> Result<()> {
    let Some(name) = name else {
        // Search every dir `find_binary` resolves helpers from — per-user
        // libexec, CLI-relative (Homebrew/prefix), and the system package's
        // /usr/libexec/paniolo/bin — not just the per-user dir, so a packaged
        // (.deb/tarball) install with no per-user build still lists helpers —
        // and then the plugin roots (PANIOLO_PLUGIN_PATH, #303), whose
        // entries carry a `plugin: <root>` column.
        let (entries, _shadowed) = daemons::helper_entries();
        if entries.is_empty() {
            let searched = daemons::all_helper_dirs()
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "No helpers found (searched {searched}) — install the paniolo \
                 package or run `paniolo setup` from a source checkout."
            );
            return Ok(());
        }
        for e in &entries {
            println!("{}", daemons::helper_list_line(e));
        }
        return Ok(());
    };
    let binary = daemons::find_binary(name).ok_or_else(|| {
        crate::error::PanioloError::not_configured(format!(
            "helper '{name}' not found — run `paniolo setup`"
        ))
    })?;
    // State/runtime dirs are keyed by helper name, except channel daemons
    // whose discovery name is the channel (any conforming helper may serve
    // it): hidrig publishes under "hid".
    let env_name = match name {
        "hidrig" | "ch9329" | "jetkvm" => HID_DAEMON.to_string(),
        // Sanitized like every other name that becomes a path component
        // (Review low #10): `name` is a typed CLI argument, and with no
        // `instance` this host-singleton branch is the one place nothing
        // downstream sanitizes it for us.
        n => daemons::sanitize_component(n),
    };
    // Manual escape hatch: no target context, so use the non-instanced dir.
    // (Per-target capture daemons are normally launched via their own
    // subcommands, which supply the target.)
    let status = std::process::Command::new(binary)
        .args(args)
        .envs(daemons::helper_env(&env_name, None))
        .status()?;
    if !status.success() {
        // A passthrough (design D3): the caller asked for this helper's own
        // status, so it is kept; only a signal death is rewritten (128+N).
        std::process::exit(error::shell_code(status));
    }
    Ok(())
}

// ── discover / configure / setup ────────────────────────────────────────────

fn cmd_discover(json: bool) -> Result<()> {
    let inv = discover::local_inventory();
    if json {
        println!("{}", serde_json::to_string(&inv)?);
        return Ok(());
    }
    let eths: Vec<String> = inv["ethernet"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    let dev = e["device"].as_str().unwrap_or("");
                    let star = if e["active"].as_bool().unwrap_or(false) {
                        "*"
                    } else {
                        ""
                    };
                    format!("{dev}{star}")
                })
                .collect()
        })
        .unwrap_or_default();
    println!(
        "usb-ethernet\t{}",
        if eths.is_empty() {
            "(none)".to_string()
        } else {
            eths.join(" ")
        }
    );
    let serials: Vec<&str> = inv["serial"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
        .unwrap_or_default();
    println!(
        "serial\t{}",
        if serials.is_empty() {
            "(none)".to_string()
        } else {
            serials.join("\n\t")
        }
    );
    let captures: Vec<String> = inv["video"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|d| {
                    let id = d["id"].as_str().unwrap_or("");
                    let id_note = if id.is_empty() {
                        String::new()
                    } else {
                        format!("  id={id}")
                    };
                    format!(
                        "{}: {}{id_note}",
                        d["index"],
                        d["name"].as_str().unwrap_or("")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    println!(
        "capture\t{}",
        if captures.is_empty() {
            "(none)".to_string()
        } else {
            captures.join("\n\t")
        }
    );
    let adbs: Vec<String> = inv["adb"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|d| {
                    let serial = d["serial"].as_str().unwrap_or("");
                    match d["model"].as_str() {
                        Some(m) => format!("{serial}  ({m})"),
                        None => serial.to_string(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    println!(
        "adb\t{}",
        if adbs.is_empty() {
            "(none)".to_string()
        } else {
            adbs.join("\n\t")
        }
    );
    println!("(* = carrier up)");
    Ok(())
}

/// Look up a control host for host-scoped commands ('local' = the dev machine).
fn resolve_host(lab_flag: Option<&str>, name: &str) -> Result<model::Host> {
    if name == model::LOCAL {
        return Ok(model::Host {
            ssh: model::LOCAL.to_string(),
            ..Default::default()
        });
    }
    let lab = load_for_read(lab_flag)?;
    lab.hosts.get(name).cloned().ok_or_else(|| {
        let have: Vec<&str> = lab.hosts.keys().map(String::as_str).collect();
        anyhow!(
            "host '{name}' not in lab (hosts: {})",
            if have.is_empty() {
                "(none)".to_string()
            } else {
                have.join(", ")
            }
        )
    })
}

fn cmd_configure(lab_flag: Option<&str>, target: &str, host: &str) -> Result<()> {
    let resolved = resolve_host(lab_flag, host)?;
    let inv = if resolved.is_local(host) {
        discover::local_inventory()
    } else {
        eprintln!("Discovering hardware on {host} ({})…", resolved.ssh);
        let mut argv = resolved.paniolo_argv();
        argv.extend(["discover".to_string(), "--json".to_string()]);
        let out = ssh::run(&resolved, &argv, None, &[])?;
        if out.status != 0 {
            bail!(
                "discover failed on {host}: {}",
                if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                }
            );
        }
        serde_json::from_str(out.stdout.trim())
            .map_err(|e| anyhow!("unparseable discover output from {host}: {e}"))?
    };

    let block = discover::propose_target_block(target, host, &inv);
    println!("# Proposed lab block — review, add to your lab file, and commit.");
    if let Ok(lab) = load_for_read(lab_flag) {
        if lab.targets.contains_key(target) {
            println!("# NOTE: target '{target}' already exists in the lab — reconcile by hand.");
        }
    }
    println!("{block}");
    Ok(())
}

fn cmd_setup(lab_flag: Option<&str>, host: Option<&str>, rust_only: bool) -> Result<()> {
    if let Some(name) = host {
        let resolved = resolve_host(lab_flag, name)?;
        if !resolved.is_local(name) {
            eprintln!("Running paniolo setup on {name} ({})…", resolved.ssh);
            let mut argv = resolved.paniolo_argv();
            argv.push("setup".to_string());
            if rust_only {
                argv.push("--rust-only".to_string());
            }
            let code = ssh::run_interactive(&resolved, &argv)?;
            if code == error::SSH_TRANSPORT_FAILURE {
                return Err(error::unreachable_host(name, error::ssh_255_message(name)).into());
            }
            std::process::exit(code);
        }
        eprintln!("'{name}' is the local machine; setting up here.");
    }
    match setup::find_repo_root() {
        Some(repo) => setup::run(&repo, rust_only, lab_flag),
        None if rust_only => Err(anyhow!(
            "paniolo source checkout not found. `paniolo setup --rust-only` rebuilds \
             the Rust crates from source, so it must run from inside a clone \
             (e.g. `make rust`). cd into the paniolo repo and try again."
        )),
        // Packaged install (Homebrew, .deb, tarball): no sources to build —
        // just finish the platform steps against the installed binaries.
        None => setup::run_packaged(),
    }
}

/// Resolve a runtime command's target: the given name, or the sole target.
fn resolve_single_target(lab: &Lab, name: Option<&str>) -> Result<String> {
    if let Some(n) = name {
        return Ok(n.to_string());
    }
    let names = lab.target_names();
    match names.len() {
        1 => Ok(names[0].to_string()),
        0 => Err(error::PanioloError::not_configured("No targets configured.").into()),
        // Answerable by the caller alone (name a target), so a usage error.
        _ => Err(error::PanioloError::new(
            error::Kind::Usage,
            format!(
                "Multiple targets ({}) — specify one with -t.",
                names.join(", ")
            ),
        )
        .into()),
    }
}

fn cmd_doctor(lab_flag: Option<&str>, target: Option<&str>, host: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let problems = doctor::run(&lab, target, host);
    if problems > 0 {
        eprintln!("{problems} problem(s) found.");
        std::process::exit(1);
    }
    Ok(())
}

// ── lab open helpers ────────────────────────────────────────────────────────

fn load_for_read(lab_flag: Option<&str>) -> Result<Lab> {
    let path = model::resolve_lab_path(lab_flag).ok_or_else(|| {
        error::PanioloError::not_configured(
            "No lab configured. Create one with `paniolo init`, or point at one \
             with --lab / PANIOLO_LAB.",
        )
    })?;
    Ok(model::load(&path)?)
}

/// Open the lab for editing (creating it if absent), apply `mutate`, save.
/// Config writes are always local and pure — they never touch a target or SSH.
/// Apply a `set` verb's `--doctor`/`--no-doctor` to channel `kind`, if
/// either was given.
fn set_doctor_flag(
    lf: &mut LabFile,
    target: &str,
    kind: &str,
    flag: &DoctorFlag,
) -> Result<(), model::LabError> {
    match flag.value() {
        Some(on) => lf.set_doctor(target, kind, on),
        None => Ok(()),
    }
}

fn edit_lab<F>(lab_flag: Option<&str>, mutate: F) -> Result<()>
where
    F: FnOnce(&mut LabFile) -> Result<(), model::LabError>,
{
    let path = model::resolve_lab_path(lab_flag).unwrap_or_else(model::default_lab_path);
    let created = !path.exists();
    let mut lf = if created {
        LabFile::create(&path)
    } else {
        LabFile::load(&path)?
    };
    mutate(&mut lf)?;
    lf.save()?;
    if created {
        eprintln!("Created new lab: {}", path.display());
    }
    Ok(())
}

// ── init / config ───────────────────────────────────────────────────────────

fn cmd_init(lab_flag: Option<&str>, path_opt: Option<&str>) -> Result<()> {
    let path: PathBuf = path_opt
        .map(model::expand_tilde)
        .or_else(|| model::resolve_lab_path(lab_flag))
        .unwrap_or_else(model::default_lab_path);
    if path.exists() {
        bail!("Lab file already exists: {}", path.display());
    }
    LabFile::create(&path).save()?;
    println!("Created lab: {}", path.display());
    println!("  Add a host:   paniolo host add <name> --ssh user@host");
    println!("  Add a target: paniolo target add <name>");
    Ok(())
}

fn config_path(lab_flag: Option<&str>) -> Result<()> {
    match model::resolve_lab_path(lab_flag) {
        Some(p) => println!("{}", p.display()),
        None => {
            println!("{}", model::default_lab_path().display());
            eprintln!("(does not exist yet — create it with `paniolo init`)");
        }
    }
    Ok(())
}

fn config_edit(lab_flag: Option<&str>) -> Result<()> {
    let path = model::resolve_lab_path(lab_flag)
        .ok_or_else(|| anyhow!("No lab to edit. Create one with `paniolo init`."))?;
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(prog)
        .args(parts)
        .arg(&path)
        .status()
        .map_err(|e| -> anyhow::Error {
            if e.kind() == std::io::ErrorKind::NotFound {
                error::PanioloError::not_configured(format!("editor '{prog}' not found")).into()
            } else {
                e.into()
            }
        })?;
    // A passthrough (design D3): the editor's own status.
    std::process::exit(error::shell_code(status));
}

fn config_show(lab_flag: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let path = model::resolve_lab_path(lab_flag).unwrap_or_default();
    println!("Lab {}", path.display());

    println!("  Hosts");
    if lab.hosts.is_empty() {
        println!("    (none — everything runs on the local dev machine)");
    } else {
        for (name, h) in &lab.hosts {
            let id = h
                .identity
                .as_deref()
                .map(|i| format!("  identity={i}"))
                .unwrap_or_default();
            println!("    {name}  {}{id}", h.ssh);
            if let Some(description) = &h.description {
                println!("      description: {description}");
            }
        }
    }

    println!("  Targets");
    if lab.targets.is_empty() {
        println!("    (none)");
    } else {
        for name in lab.targets.keys() {
            let rt = lab.resolved_target(name).unwrap();
            println!("    {}", target_headline(&rt));
            if let Some(description) = &rt.description {
                println!("      description: {description}");
            }
            for ch in &rt.channels {
                println!("      {}", channel_label(ch));
            }
            if rt.channels.is_empty() {
                println!("      (no channels)");
            }
        }
    }
    Ok(())
}

// ── host ────────────────────────────────────────────────────────────────────

fn host_cmd(lab_flag: Option<&str>, cmd: HostCmd) -> Result<()> {
    match cmd {
        HostCmd::List => host_list(lab_flag),
        HostCmd::Show { name } => host_show(lab_flag, &name),
        HostCmd::Add {
            name,
            ssh,
            description,
            hostname,
            identity,
            control_path,
            paniolo_cmd,
            plugin_path,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.add_host(
                    &name,
                    &ssh,
                    description.as_deref(),
                    hostname.as_deref(),
                    identity.as_deref(),
                    control_path.as_deref(),
                    paniolo_cmd.as_deref(),
                    plugin_path.as_deref(),
                )
            })?;
            println!("Host '{name}' added. ({ssh})");
            Ok(())
        }
        HostCmd::Set {
            name,
            ssh,
            description,
            hostname,
            identity,
            control_path,
            paniolo_cmd,
            plugin_path,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.update_host(
                    &name,
                    ssh.as_deref(),
                    description.as_deref(),
                    hostname.as_deref(),
                    identity.as_deref(),
                    control_path.as_deref(),
                    paniolo_cmd.as_deref(),
                    plugin_path.as_deref(),
                )
            })?;
            println!("Host '{name}' updated.");
            Ok(())
        }
        HostCmd::Rm { name } => {
            edit_lab(lab_flag, |lf| lf.remove_host(&name))?;
            println!("Host '{name}' removed.");
            Ok(())
        }
    }
}

fn host_list(lab_flag: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    if lab.hosts.is_empty() {
        println!("No hosts declared. (everything runs locally)");
        return Ok(());
    }
    for (name, h) in &lab.hosts {
        let local = if h.is_local(name) {
            "  (this machine)"
        } else {
            ""
        };
        println!(
            "{name}\t{}\t{}{local}",
            h.ssh,
            h.hostname.as_deref().unwrap_or("")
        );
    }
    if let Some(fqdn) = model::local_fqdn() {
        println!("\nthis machine: {fqdn}");
    }
    Ok(())
}

fn host_show(lab_flag: Option<&str>, name: &str) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    if name != model::LOCAL && !lab.hosts.contains_key(name) {
        let have: Vec<&str> = lab.hosts.keys().map(String::as_str).collect();
        bail!(
            "Host '{name}' not in lab. Hosts: {}",
            if have.is_empty() {
                "(none)".into()
            } else {
                have.join(", ")
            }
        );
    }
    match lab.hosts.get(name) {
        Some(h) => {
            println!("Host: {name}");
            println!("  ssh           {}", h.ssh);
            if let Some(v) = &h.description {
                println!("  description   {v}");
            }
            if let Some(v) = &h.hostname {
                println!("  hostname      {v}");
            }
            if let Some(v) = &h.identity {
                println!("  identity      {v}");
            }
            if let Some(v) = &h.control_path {
                println!("  control_path  {v}");
            }
            if let Some(v) = &h.paniolo_cmd {
                println!("  paniolo_cmd   {v}");
            }
            if let Some(v) = &h.plugin_path {
                println!("  plugin_path   {v}");
            }
            if h.is_local(name) {
                println!("  -> resolves as LOCAL on this machine");
            }
        }
        None => println!("Host: local  (the dev machine)"),
    }
    let pairs = lab.channels_on_host(name);
    if pairs.is_empty() {
        println!("  (no channels bound to this host)");
        return Ok(());
    }
    for (tname, ch) in pairs {
        println!(
            "  {tname}\t{}\t{}",
            channel_name(&ch),
            fields_str(&ch.fields)
        );
    }
    Ok(())
}

// ── target ────────────────────────────────────────────────────────────────────

fn target_cmd(lab_flag: Option<&str>, cmd: TargetCmd) -> Result<()> {
    match cmd {
        TargetCmd::List => target_list(lab_flag),
        TargetCmd::Show { name } => target_show(lab_flag, name.as_deref()),
        TargetCmd::Add {
            name,
            host,
            description,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.add_target(&name, host.as_deref(), description.as_deref())
            })?;
            println!("Target '{name}' added.");
            Ok(())
        }
        TargetCmd::Set {
            name,
            host,
            description,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.update_target(&name, host.as_deref(), description.as_deref())
            })?;
            println!("Target '{name}' updated.");
            Ok(())
        }
        TargetCmd::Rename { old, new } => {
            edit_lab(lab_flag, |lf| lf.rename_target(&old, &new))?;
            println!("Target '{old}' renamed to '{new}'.");
            println!(
                "Running daemons keep the old name — if `serial watch`, `video watch`, \
                 or `hid serve` were up for '{old}', stop and restart them as '{new}'."
            );
            Ok(())
        }
        TargetCmd::Rm { name } => {
            edit_lab(lab_flag, |lf| lf.remove_target(&name))?;
            println!("Target '{name}' removed.");
            Ok(())
        }
    }
}

fn target_list(lab_flag: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    if lab.targets.is_empty() {
        println!("No targets configured. (paniolo target add <name>)");
        return Ok(());
    }
    for name in lab.targets.keys() {
        let rt = lab.resolved_target(name).unwrap();
        let chans: Vec<String> = rt
            .channels
            .iter()
            .map(|c| {
                if c.name == c.kind.as_str() {
                    c.kind.as_str().to_string()
                } else {
                    format!("{}:{}", c.kind.as_str(), c.name)
                }
            })
            .collect();
        println!("{name}\t{}\t{}", rt.hosts().join(", "), chans.join(", "));
    }
    Ok(())
}

fn target_show(lab_flag: Option<&str>, name: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let names: Vec<String> = match name {
        Some(n) => vec![n.to_string()],
        None => lab.targets.keys().cloned().collect(),
    };
    if names.is_empty() {
        println!("No targets configured.");
        return Ok(());
    }
    for tname in names {
        match lab.resolved_target(&tname) {
            Some(rt) => print_resolved_target(&rt),
            None => eprintln!("Target '{tname}' not found in lab."),
        }
    }
    Ok(())
}

// ── serial / netboot / power (config) ───────────────────────────────────────

fn serial_cmd(lab_flag: Option<&str>, cmd: SerialCmd) -> Result<()> {
    match cmd {
        SerialCmd::Add {
            name,
            target,
            device,
            baud,
            sense,
            power_button,
            host,
            port,
        } => {
            let sense = normalize_sense(sense.as_deref())?;
            edit_lab(lab_flag, |lf| {
                lf.add_serial(
                    &target,
                    &name,
                    &device,
                    baud,
                    sense.as_deref(),
                    power_button,
                    host.as_deref(),
                    port,
                )
            })?;
            println!("Serial '{name}' added to '{target}': {device} @ {baud}");
            Ok(())
        }
        SerialCmd::Set {
            name,
            target,
            device,
            baud,
            sense,
            power_button,
            host,
            port,
        } => {
            let sense = match sense.as_deref() {
                None => None,
                Some(s) => match normalize_sense(Some(s))? {
                    Some(v) => Some(v),
                    None => bail!(
                        "`set` can't clear a sense signal. Remove and re-add the \
                         interface to clear it."
                    ),
                },
            };
            edit_lab(lab_flag, |lf| {
                lf.update_serial(
                    &target,
                    &name,
                    device.as_deref(),
                    baud,
                    sense.as_deref(),
                    power_button,
                    host.as_deref(),
                    port,
                )
            })?;
            println!("Serial '{name}' updated on '{target}'.");
            Ok(())
        }
        SerialCmd::Rm { name, target } => {
            edit_lab(lab_flag, |lf| lf.remove_serial(&target, &name))?;
            println!("Serial '{name}' removed from '{target}'.");
            Ok(())
        }
        SerialCmd::Connect { target, interface } => {
            cmd_serial_connect(lab_flag, target.name(), interface.as_deref())
        }
        SerialCmd::Watch { target, port } => cmd_serial_watch(lab_flag, target.name(), port),
        SerialCmd::Stop { target } => {
            // Resolve the target (routing to its serial channel's host if
            // remote) so we stop the right per-target daemon instance.
            let (target, serials) =
                serial_runtime(lab_flag, target.name(), None, dispatch::Mode::Reexec)?;
            // An orphan has no discovery file, so `serialcap stop` cannot see
            // it and the operator has nothing left but `ps` and `kill` (#187).
            // Only consulted when nothing is tracked: a healthy daemon is never
            // in this set, and its own stop is the clean shutdown.
            if serial::daemon(&target).is_none() {
                if let Some(orphan) = serial::untracked(&serials) {
                    reap_untracked(&orphan)?;
                    println!(
                        "Untracked serial daemon for '{target}' (pid {}) stopped.",
                        orphan.pid
                    );
                    return Ok(());
                }
            }
            let status = serial::stop_daemon(&target)?;
            if status.success() {
                println!("Serial daemon for '{target}' stopped.");
                Ok(())
            } else {
                Err(error::hook_failed(
                    status,
                    format!(
                        "serialcap stop exited with code {}",
                        error::shell_code(status)
                    ),
                )
                .target(&target)
                .channel("serial")
                .into())
            }
        }
        SerialCmd::Send {
            first,
            second,
            target,
            interface,
            pace_ms,
            no_newline,
        } => {
            // Two positionals = target + text; one = text (clap rejects -t
            // alongside a second positional).
            let (target, text) = match second {
                Some(text) => (Some(first), text),
                None => (target, first),
            };
            cmd_serial_send(
                lab_flag,
                target.as_deref(),
                interface.as_deref(),
                &text,
                pace_ms,
                !no_newline,
            )
        }
        SerialCmd::Log {
            target,
            interface,
            tail,
            from,
            to,
            since,
            raw,
            json,
            no_pending,
            require_live,
        } => cmd_serial_log(
            lab_flag,
            target.name(),
            interface.as_deref(),
            tail,
            from,
            to,
            since,
            raw,
            json,
            no_pending,
            require_live,
        ),
        SerialCmd::Evidence {
            target,
            interface,
            epoch,
            since,
            max_bytes,
        } => {
            let (target, serials) = serial_runtime(
                lab_flag,
                target.name(),
                interface.as_deref(),
                dispatch::Mode::Reexec,
            )?;
            let channel = pick_serial(&target, &serials, interface.as_deref())?;
            println!(
                "{}",
                serial::read_evidence(&target, &channel.name, epoch.as_deref(), since, max_bytes)?
            );
            Ok(())
        }
        SerialCmd::Wait {
            target,
            interface,
            epoch,
            since,
            timeout_ms,
            max_bytes,
            max_output_bytes,
            pattern,
            include_pending,
            follow,
        } => {
            let (target, serials) = serial_runtime(
                lab_flag,
                target.name(),
                interface.as_deref(),
                dispatch::Mode::Reexec,
            )?;
            let channel = pick_serial(&target, &serials, interface.as_deref())?;
            serial::wait_for_evidence(
                &target,
                &channel.name,
                serial::WaitOptions {
                    epoch: &epoch,
                    since,
                    timeout_ms,
                    max_bytes,
                    max_output_bytes,
                    pattern: pattern.as_deref(),
                    include_pending,
                    follow,
                },
            )
        }
        SerialCmd::Transact {
            target,
            interface,
            until,
            timeout_ms,
            max_bytes,
            pace_ms,
            no_newline,
            text,
        } => cmd_serial_transact(
            lab_flag,
            target.as_deref(),
            interface.as_deref(),
            &text,
            !no_newline,
            &serial::TransactOptions {
                until: &until,
                timeout_ms,
                max_bytes,
                pace_ms,
            },
        ),
        SerialCmd::Devices => {
            let devices = serial::list_devices();
            if devices.is_empty() {
                println!("No serial devices found.");
            }
            for d in devices {
                println!("  {d}");
            }
            Ok(())
        }
        SerialCmd::Status { target, interface } => {
            let (target, serials) = serial_runtime(
                lab_flag,
                target.name(),
                interface.as_deref(),
                dispatch::Mode::Reexec,
            )?;
            if let Some(name) = interface.as_deref() {
                pick_serial(&target, &serials, Some(name))?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serial::health_snapshot(
                    &target,
                    &serials,
                    interface.as_deref()
                )?)?
            );
            Ok(())
        }
        SerialCmd::Show { target } => cmd_serial_show(lab_flag, target.name()),
        SerialCmd::Dtr {
            target,
            ms,
            interface,
        } => cmd_serial_dtr(
            lab_flag,
            target.name(),
            ms,
            interface.as_deref(),
            "DTR pulse",
        ),
        SerialCmd::Reset {
            target,
            ms,
            interface,
        } => cmd_serial_dtr(
            lab_flag,
            target.name(),
            ms,
            interface.as_deref(),
            "Soft reset",
        ),
    }
}

/// Pulse DTR on a target's serial interface — via the serialcap daemon when it
/// is running (it owns the port), else directly.
///
/// DTR power control is opt-in per interface (`power_button = true`): wiring the
/// FTDI DTR line to the board's J2 header is the rare exception, and toggling an
/// unwired line silently no-ops. We resolve the interface, then require the
/// opt-in — never fall back to a lone console — so a "reboot over serial" that
/// really meant the console `reboot` command fails loudly with a redirect
/// instead of pretending to power-cycle.
fn cmd_serial_dtr(
    lab_flag: Option<&str>,
    target: Option<&str>,
    ms: u64,
    interface: Option<&str>,
    label: &str,
) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    let iface = resolve_dtr_interface(&lab, &target, interface)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Serial,
        Some(&iface),
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let serials = local_serials(&lab, &target)?;
    let ch = pick_serial(&target, &serials, Some(&iface))?;
    if let Some(daemon) = serial::daemon(&target) {
        eprintln!("{label} on '{target}' ({ms} ms via serialcap daemon)");
        power::dtr_press_daemon(&daemon, &ch.name, ms)
            .map_err(|e| error::with_target(e, &target))?;
    } else {
        eprintln!("{label} on '{target}' ({ms} ms via {} directly)", ch.device);
        power::dtr_press_direct(&ch.device, ms)?;
    }
    println!("Done.");
    Ok(())
}

/// Resolve which serial interface a DTR power-button command (`serial dtr` /
/// `serial reset`) acts on, enforcing the `power_button` opt-in.
///
/// Selection order: explicit `-i`, then the power channel's `serial_interface`,
/// then the sole interface that has `power_button = true`. The chosen interface
/// MUST have `power_button = true`; otherwise this returns a hint that redirects
/// to the target's real power method and the console `reboot` path rather than
/// toggling a possibly-unwired DTR line. The full (all-host) interface list is
/// used so a DTR-wired interface on a remote host is still found pre-dispatch.
fn resolve_dtr_interface(lab: &Lab, target: &str, interface: Option<&str>) -> Result<String> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let have = || {
        t.serial
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let chosen = if let Some(name) = interface {
        t.serial.iter().find(|s| s.name == name).ok_or_else(|| {
            error::channel_missing(
                target,
                "serial",
                format!(
                    "no serial interface '{name}' on '{target}' (have: {})",
                    have()
                ),
            )
        })?
    } else if let Some(name) = t.power.as_ref().and_then(|p| p.serial_interface.as_deref()) {
        t.serial.iter().find(|s| s.name == name).ok_or_else(|| {
            error::channel_missing(
                target,
                "power",
                format!(
                    "power serial_interface '{name}' not found among '{target}' interfaces ({})",
                    have()
                ),
            )
        })?
    } else {
        let buttons: Vec<&model::SerialChannel> =
            t.serial.iter().filter(|s| s.power_button).collect();
        match buttons.as_slice() {
            [one] => *one,
            [] => {
                return Err(error::channel_missing(
                    target,
                    "serial",
                    dtr_opt_in_hint(target, t, None),
                )
                .into())
            }
            _ => {
                return Err(error::PanioloError::new(
                    error::Kind::Usage,
                    format!(
                        "multiple DTR power-button interfaces on '{target}' ({}); pick one with -i",
                        buttons
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
                .target(target)
                .channel("serial")
                .into())
            }
        }
    };
    if !chosen.power_button {
        bail!("{}", dtr_opt_in_hint(target, t, Some(&chosen.name)));
    }
    Ok(chosen.name.clone())
}

/// The error shown when a DTR power-button command targets an interface that
/// hasn't opted in. It actively redirects to the three correct paths: the
/// configured power hook, the console `reboot`, and how to declare DTR wiring.
fn dtr_opt_in_hint(target: &str, t: &model::Target, iface: Option<&str>) -> String {
    let mut out = match iface {
        Some(name) => format!(
            "serial interface '{name}' on '{target}' is not wired for DTR power \
             control (no `power_button = true`)."
        ),
        None => format!("no DTR power-button interface configured for '{target}'."),
    };
    if t.power
        .as_ref()
        .and_then(|p| p.cycle_cmd.as_deref())
        .is_some()
    {
        out.push_str(&format!(
            "\n  • To power-cycle it: paniolo power-cycle {target}"
        ));
    }
    out.push_str(&format!(
        "\n  • To reboot from a logged-in serial console: paniolo serial send {target} \"reboot\""
    ));
    let example = iface
        .or_else(|| t.serial.first().map(|s| s.name.as_str()))
        .unwrap_or("console");
    out.push_str(&format!(
        "\n  • If the FTDI DTR line really is wired to J2: \
         paniolo serial set {example} -t {target} --power-button"
    ));
    out
}

// ── console (composite: video + serial dashboard) ───────────────────────────

/// The other daemons the dashboard connects to, as URL query parameters. Each
/// is a complete cross-port WebSocket URL with that daemon's own token inside
/// (`daemons::Endpoint::ws_url`) — the local and the remote/tunnel paths look
/// the same to the page.
#[derive(Default)]
struct DashboardLinks<'a> {
    serial_ws: Option<&'a str>,
    interface: Option<&'a str>,
    hid_ws: Option<&'a str>,
    /// The target has no serial channel at all (a KVM-only console, #223):
    /// tells the page to drop its terminal pane instead of trying the
    /// hand-opened default port and reporting a failed connection.
    video_only: bool,
}

/// The dashboard URL: the video daemon's `GET /`, its token as `?token=` (the
/// page reads it back and puts it on every request it makes), plus the links.
fn dashboard_url(video: &daemons::Endpoint, links: &DashboardLinks) -> String {
    let mut params: Vec<String> = Vec::new();
    if let Some(t) = &video.token {
        params.push(format!("token={}", daemons::query_escape(t)));
    }
    if let Some(ws) = links.serial_ws {
        params.push(format!("serialws={}", daemons::query_escape(ws)));
    }
    if let Some(i) = links.interface {
        params.push(format!("interface={}", daemons::query_escape(i)));
    }
    if let Some(ws) = links.hid_ws {
        params.push(format!("hidws={}", daemons::query_escape(ws)));
    }
    if links.video_only {
        params.push("serial=none".to_string());
    }
    let base = video.base_url();
    if params.is_empty() {
        base
    } else {
        format!("{base}/?{}", params.join("&"))
    }
}

/// What `console` prints once the dashboard URL is in the browser's hands.
///
/// The URL itself carries every running daemon's token and this line lands in
/// transcripts, CI logs and pasted terminal output (#196), so only the
/// token-free address is printed. `withheld` says whether there was anything
/// to withhold: against a daemon old enough to have no token, and with no
/// serial or hid links, `dashboard_url` returns exactly this address, and
/// claiming a credential was held back would be false and would steer the
/// reader away from the one address that works.
fn console_banner(video: &daemons::Endpoint, withheld: bool) -> String {
    let base = video.base_url();
    if withheld {
        format!(
            "Opened the dashboard in the browser. Its URL carries the daemons' tokens, \
             so it is not printed here; the page is at {base}."
        )
    } else {
        format!("Opened {base} in the browser.")
    }
}

/// `video show`'s daemon line: the token-free address, plus whatever
/// `stale_note` had to say.
///
/// Its own function so a test can execute it. The token belongs nowhere in
/// here — an agent driving a target runs `video show` constantly and every
/// line of it lands in the transcript (#196) — and a rendering this small is
/// exactly the kind that gets reverted by a one-word edit with the suite still
/// green.
fn video_show_daemon_line(url: &str, stale: &str) -> String {
    format!("daemon\trunning at {url}{stale}")
}

/// What to print when the OS opener could not be launched.
///
/// An address the reader cannot open would strand them — on a headless control
/// host, and worse over SSH, where the tunnels die with the command and the
/// ephemeral ports the URL names never come back. So the URL has to be
/// reachable. It goes to a 0600 file rather than to stdout: the same
/// protection the discovery file gets, and the reader can open it without a
/// credential passing through a transcript on the way.
///
/// If even that fails there is nothing left to protect the token *with*, so
/// the URL is printed — being locked out of your own dashboard is the worse
/// outcome, and the caller is told what it is handing them.
fn console_fallback(target: &str, url: &str) -> String {
    match write_private_url(target, url) {
        Ok(path) => format!(
            "Could not launch a browser. The URL carries the daemons' tokens, so it is \
             in {} (mode 0600) rather than printed — open it with \
             `open \"$(cat {})\"` (macOS) or `xdg-open \"$(cat {})\"`.",
            path.display(),
            path.display(),
            path.display()
        ),
        Err(e) => format!(
            "Could not launch a browser, and could not write the URL to a private file \
             ({e}). Open this yourself — it carries the daemons' tokens, so treat it as \
             a credential:\n{url}"
        ),
    }
}

/// Write `url` to a 0600 file in the target's runtime dir and return its path.
fn write_private_url(target: &str, url: &str) -> Result<std::path::PathBuf> {
    let dir = daemons::ensure_runtime_dir(video::DAEMON, Some(target))?;
    let path = dir.join("dashboard-url.txt");
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&path)?;
    use std::io::Write;
    writeln!(f, "{url}")?;
    Ok(path)
}

/// `console`'s two output switches (`--open`, `--print-url`).
#[derive(Debug, Clone, Copy, Default)]
struct ConsoleView {
    open: bool,
    print_url: bool,
}

/// Whether this process runs inside an SSH session, judged from the variables
/// sshd sets. `get` is the environment lookup, so a test can supply one.
///
/// A browser launched here would open on the control host's display (or fail
/// there), not in front of the person who typed the command — they are on the
/// far end of the SSH connection.
fn in_ssh_session(get: impl Fn(&str) -> Option<String>) -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|k| get(k).is_some_and(|v| !v.is_empty()))
}

/// This machine's name as `hostname` reports it, for the `ssh -L` hint.
fn local_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// The command that makes the dashboard reachable from the SSH client's own
/// machine: one `-L` per port the dashboard URL names, each forwarded to the
/// same port number so the URL works unchanged. The daemons accept only a
/// loopback `Host` and `Origin`, which `127.0.0.1:<port>` on the far end of a
/// tunnel satisfies.
fn ssh_forward_hint(host: &str, ports: &[u16]) -> String {
    let forwards: Vec<String> = ports
        .iter()
        .map(|p| format!("-L {p}:127.0.0.1:{p}"))
        .collect();
    format!(
        "Running over SSH, so no browser was opened here. To view the dashboard \
         from your own machine, run this there and keep it running:\n  \
         ssh -N {} {host}",
        forwards.join(" ")
    )
}

/// Where to find the URL when it was neither opened nor printed: the 0600 file,
/// fetched across the SSH connection.
fn console_url_over_ssh(target: &str, host: &str, url: &str) -> String {
    match write_private_url(target, url) {
        Ok(path) => format!(
            "The dashboard URL carries the daemons' tokens, so it is in {} (mode 0600) \
             rather than printed. Fetch it from your machine with:\n  \
             ssh {host} cat {}\nor rerun with --print-url.",
            path.display(),
            path.display()
        ),
        Err(e) => format!(
            "Could not write the dashboard URL to a private file ({e}). Open this \
             yourself — it carries the daemons' tokens, so treat it as a credential:\n{url}"
        ),
    }
}

/// Hand the dashboard to the reader: a browser here, unless this is an SSH
/// session (or the launch fails), in which case the `ssh -L` hint and the URL
/// (printed with `--print-url`, otherwise in a 0600 file).
fn show_console(
    target: &str,
    video: &daemons::Endpoint,
    url: &str,
    ports: &[u16],
    view: ConsoleView,
) {
    let over_ssh = !view.open && in_ssh_session(|k| std::env::var(k).ok());
    if !over_ssh && open_in_browser(url) {
        println!("{}", console_banner(video, url != video.base_url()));
        if view.print_url {
            println!("{url}");
        }
        return;
    }
    if !over_ssh {
        // The launch itself failed on a machine with no SSH session.
        if view.print_url {
            println!("Could not launch a browser. Dashboard URL:\n{url}");
        } else {
            println!("{}", console_fallback(target, url));
        }
        return;
    }
    let host = local_hostname().unwrap_or_else(|| "<control-host>".to_string());
    println!("{}", ssh_forward_hint(&host, ports));
    if view.print_url {
        println!("Then open:\n{url}");
    } else {
        println!("{}", console_url_over_ssh(target, &host, url));
    }
}

/// Hand `url` to the OS opener. Returns whether the opener could be launched
/// at all — false on a headless host, in a container, or anywhere `xdg-open`
/// is not installed.
///
/// The caller has to know: since #196 the token-bearing URL is no longer
/// printed alongside, so a silently failed launch would leave the dashboard
/// unreachable with nothing to paste. A successful spawn is not proof a window
/// appeared (the opener may still fail asynchronously), but it distinguishes
/// "no opener here" from "handed off", which is the case that strands people.
fn open_in_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Where `console` runs, and whether it has a serial leg. A target with no
/// serial channel is still a console (#223): the dashboard shows the screen
/// and, with a hid channel, takes input; only the terminal pane is absent.
/// With serial, both channels must share a host, because the page is served
/// by the video daemon and reaches serialcap over loopback.
fn console_host(rt: &model::ResolvedTarget, interface: Option<&str>) -> Result<(String, bool)> {
    let has_serial = rt
        .channels
        .iter()
        .any(|c| c.kind == model::ChannelKind::Serial);
    let video_host = model::channel_host(rt, model::ChannelKind::Video, None)?;
    if !has_serial {
        if interface.is_some() {
            // Names an interface the target cannot have; channel_host words it.
            model::channel_host(rt, model::ChannelKind::Serial, interface)?;
        }
        return Ok((video_host, false));
    }
    let serial_host = model::channel_host(rt, model::ChannelKind::Serial, interface)?;
    if serial_host != video_host {
        bail!(
            "console needs the serial and video channels on one host; \
             serial is on '{serial_host}', video on '{video_host}'"
        );
    }
    Ok((serial_host, true))
}

/// The combined dashboard is a composite command: it needs the video channel
/// and, when the target has one, the serial channel co-located on one host.
/// Locally it ensures the daemons; remotely it starts them over SSH and holds
/// tunnels to them.
fn cmd_console(
    lab_flag: Option<&str>,
    target: Option<&str>,
    interface: Option<&str>,
    view: ConsoleView,
) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    let rt = lab
        .resolved_target(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let (host_name, has_serial) = console_host(&rt, interface)?;
    let host = lab.host(&host_name);
    if !host.is_local(&host_name) {
        return remote_console(&lab, &target, &host_name, interface, has_serial, view);
    }

    // Local: ensure both daemons (OS-assigned ports; discovery finds them).
    if video::daemon(&target).is_none() {
        let v = local_video(&lab, &target)?;
        let device = v.device.clone().ok_or_else(|| {
            error::channel_missing(
                &target,
                "video",
                format!("video channel for '{target}' has no device set"),
            )
        })?;
        let hid_input = ensure_video_source(&lab, &target, &device)?;
        eprintln!("Starting video daemon…");
        let mut child = video::start_daemon(
            &device,
            model::fixed_port(v.port),
            &target,
            v.ocr_mode.as_deref(),
            v.change_threshold,
            hid_input.as_deref(),
        )?;
        daemons::wait_for_daemon(
            video::DAEMON,
            Some(&target),
            std::time::Duration::from_secs(5),
        )
        .ok_or_else(|| {
            daemons::start_failure(
                video::DAEMON,
                Some(&target),
                std::time::Duration::from_secs(5),
                Some(&mut child),
            )
        })?;
    }
    if has_serial && serial::daemon(&target).is_none() {
        let serials = local_serials(&lab, &target)?;
        if serials.is_empty() {
            return Err(error::channel_missing(
                &target,
                "serial",
                format!("no serial interfaces configured for '{target}' (paniolo serial add ...)"),
            )
            .into());
        }
        eprintln!("Starting serial daemon…");
        let port = lab
            .targets
            .get(&target)
            .map_or(0, model::Target::serial_port);
        let mut child = serial::start_daemon(&serials, port, &target)?;
        daemons::wait_for_daemon(
            serial::DAEMON,
            Some(&target),
            std::time::Duration::from_secs(5),
        )
        .ok_or_else(|| {
            daemons::start_failure(
                serial::DAEMON,
                Some(&target),
                std::time::Duration::from_secs(5),
                Some(&mut child),
            )
        })?;
    }
    // Optional KVM leg: if the target has a local hid channel, ensure its
    // daemon and hand the dashboard its WebSocket URL (?hidws=). Absent/remote
    // hid channels just leave the console without input injection.
    let hid = ensure_hid_daemon_local(&lab, &target).unwrap_or_else(|e| {
        eprintln!("hid (KVM) disabled: {e}");
        None
    });

    // The dashboard's panes can't discover the daemons' OS-assigned ports or
    // tokens themselves — hand each over as a complete WebSocket URL, token
    // inside (?serialws= / ?hidws=), and the page's own token as ?token=.
    let video = video::daemon(&target).ok_or_else(|| {
        error::daemon_down(
            video::DAEMON,
            format!("video daemon for '{target}' is not running"),
        )
        .target(&target)
    })?;
    let serial = if has_serial {
        serial::daemon(&target)
    } else {
        None
    };
    let serial_ws = serial.as_ref().map(|d| d.ws_url("/stream"));
    let hid_ws = hid.as_ref().map(|d| d.ws_url("/hid"));
    let ports: Vec<u16> = [Some(&video), serial.as_ref(), hid.as_ref()]
        .into_iter()
        .flatten()
        .map(|d| d.port)
        .collect();
    let url = dashboard_url(
        &video,
        &DashboardLinks {
            serial_ws: serial_ws.as_deref(),
            interface,
            hid_ws: hid_ws.as_deref(),
            video_only: !has_serial,
        },
    );
    show_console(&target, &video, &url, &ports, view);
    Ok(())
}

fn remote_console(
    lab: &Lab,
    target: &str,
    host_name: &str,
    interface: Option<&str>,
    has_serial: bool,
    view: ConsoleView,
) -> Result<()> {
    let host = lab.host(host_name);
    eprintln!("Starting daemons on {host_name}…");
    let mut subs = vec![["video", "watch"]];
    if has_serial {
        subs.push(["serial", "watch"]);
    }
    for sub in subs {
        let out = dispatch::run_subcommand(lab, target, host_name, &[sub[0], sub[1], target])?;
        if out.status != 0 {
            let msg = if out.stderr.trim().is_empty() {
                out.stdout.trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            bail!(
                "failed to start '{} {}' on {host_name}: {msg}",
                sub[0],
                sub[1]
            );
        }
    }
    let video =
        dispatch::remote_daemon_endpoint(&host, &daemons::runtime_rel("hdmicap", Some(target)))
            .ok_or_else(|| anyhow!("could not read the hdmicap daemon port on {host_name}"))?;
    let fwd_video = ssh::forward(&host, video.port)?;
    // The local ends of every tunnel, for the `ssh -L` hint when this command
    // itself runs inside an SSH session.
    let mut ports = vec![fwd_video.local_port];
    // Through the tunnels the daemons answer on local ports; their tokens
    // travel unchanged.
    let video = daemons::Endpoint {
        port: fwd_video.local_port,
        ..video
    };
    // The tunnel must live as long as the page does; the binding keeps it.
    let mut _fwd_serial = None;
    let serial_ws: Option<String> = if has_serial {
        let serial = dispatch::remote_daemon_endpoint(
            &host,
            &daemons::runtime_rel("serialcap", Some(target)),
        )
        .ok_or_else(|| anyhow!("could not read the serialcap daemon port on {host_name}"))?;
        let fwd = ssh::forward(&host, serial.port)?;
        ports.push(fwd.local_port);
        let serial = daemons::Endpoint {
            port: fwd.local_port,
            ..serial
        };
        _fwd_serial = Some(fwd);
        Some(serial.ws_url("/stream"))
    } else {
        None
    };

    // Optional KVM leg: start the hid daemon on the host if its channel lives
    // there too, then tunnel its port. A KVM-less console still works.
    let hid_on_host = lab
        .resolved_target(target)
        .map(|rt| model::channel_host(&rt, model::ChannelKind::Hid, None).ok())
        .unwrap_or(None)
        .as_deref()
        == Some(host_name)
        && lab
            .targets
            .get(target)
            .and_then(|t| t.hid.as_ref())
            .is_some();
    let mut _fwd_hid = None;
    let hid_ws: Option<String> = if hid_on_host {
        match dispatch::run_subcommand(lab, target, host_name, &["hid", "serve", target]) {
            Ok(out) if out.status == 0 => dispatch::remote_daemon_endpoint(
                &host,
                &daemons::runtime_rel(HID_DAEMON, Some(target)),
            )
            .and_then(|ep| ssh::forward(&host, ep.port).ok().map(|fwd| (ep, fwd)))
            .map(|(ep, fwd)| {
                let url = daemons::Endpoint {
                    port: fwd.local_port,
                    ..ep
                }
                .ws_url("/hid");
                ports.push(fwd.local_port);
                _fwd_hid = Some(fwd);
                url
            }),
            _ => {
                eprintln!("hid (KVM) disabled: could not start the hid daemon on {host_name}");
                None
            }
        }
    } else {
        None
    };

    let url = dashboard_url(
        &video,
        &DashboardLinks {
            serial_ws: serial_ws.as_deref(),
            interface,
            hid_ws: hid_ws.as_deref(),
            video_only: !has_serial,
        },
    );
    show_console(target, &video, &url, &ports, view);
    println!("Tunnels to {host_name} open. Press Ctrl-C to close.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

// ── power runtime bodies ────────────────────────────────────────────────────

/// True when a channel whose optional `host` field defaults to `default_host`
/// resolves to *this* machine. Uses the same self-identification the dispatch
/// layer does ([`model::Host::is_local`] — the `local` sentinel or a matching
/// FQDN), so a host that is local by FQDN is recognized here too. Comparing the
/// host name against the bare `local` sentinel instead would reject it.
fn channel_is_local(lab: &Lab, channel_host: Option<&str>, default_host: &str) -> bool {
    let name = channel_host.unwrap_or(default_host);
    lab.host(name).is_local(name)
}

/// The target's power channel as visible on *this* host.
fn local_power(lab: &Lab, target: &str) -> Result<model::PowerChannel> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let dh = t.default_host().to_string();
    let p = t.power.clone().ok_or_else(|| {
        error::channel_missing(
            target,
            "power",
            format!("target '{target}' has no power channel (paniolo power set -t {target} ...)"),
        )
    })?;
    if !channel_is_local(lab, p.host.as_deref(), &dh) {
        return Err(error::channel_missing(
            target,
            "power",
            format!("power channel for '{target}' is not on this host"),
        )
        .into());
    }
    Ok(p)
}

/// `--level`, normalized: `None` for the default level, whether spelled by
/// omitting the flag or as `--level default`.
fn named_level(level: Option<&str>) -> Option<&str> {
    level.filter(|l| *l != model::DEFAULT_POWER_LEVEL)
}

/// `" at level 'NAME'"` for a named level, empty for the default one, so the
/// default level's messages read exactly as before levels existed.
fn level_suffix(level: Option<&str>) -> String {
    match level {
        Some(l) => format!(" at level '{l}'"),
        None => String::new(),
    }
}

/// The hooks of `level` (see [`named_level`]) on the target's power channel
/// as visible on *this* host. An unknown level is `not_configured`, listing
/// the levels the target does have.
fn local_power_level(lab: &Lab, target: &str, level: Option<&str>) -> Result<model::PowerLevel> {
    let p = local_power(lab, target)?;
    p.level(level).ok_or_else(|| {
        let name = level.unwrap_or_default();
        let have: Vec<&str> = p.levels.keys().map(String::as_str).collect();
        error::channel_missing(
            target,
            "power",
            format!(
                "target '{target}' has no power level '{name}' (have: {}; \
                 paniolo power set -t {target} --level {name} --cycle-cmd ...)",
                if have.is_empty() {
                    "none".to_string()
                } else {
                    have.join(", ")
                }
            ),
        )
        .into()
    })
}

/// `not_configured` for a power verb whose hook `key` (set with `--flag`) the
/// chosen level lacks. A named level never borrows the default level's hook,
/// so the message names the level and how to give it one.
fn missing_power_hook(target: &str, level: Option<&str>, key: &str, flag: &str) -> anyhow::Error {
    let msg = match level {
        None => format!(
            "no {key} configured for '{target}' \
             (paniolo power set -t {target} --{flag} /path/to/script)"
        ),
        Some(l) => format!(
            "no {key} configured for '{target}' power level '{l}' \
             (paniolo power set -t {target} --level {l} --{flag} /path/to/script)"
        ),
    };
    error::channel_missing(target, "power", msg).into()
}

/// State/runtime-dir env pairs for an opaque hook command (named after the
/// hook's program basename); empty when underivable.
fn hook_envs(cmd: &str) -> Vec<(&'static str, std::path::PathBuf)> {
    daemons::hook_helper_name(cmd)
        .map(|n| daemons::helper_env(&n, None))
        .unwrap_or_default()
}

/// Run an opaque shell hook via `sh -c`, propagating its exit code. The
/// libexec dir is prepended to PATH so lab files can name helpers bare, and
/// the helper state/runtime dirs ride along as env vars.
/// `label` is a human-readable description shown in the progress message.
///
/// Holds the target's hook lock, shared with the action hooks: both often
/// drive the same exclusive device ([`actions`]).
fn run_power_hook(cmd: &str, label: &str, target: &str, level: Option<&str>) -> Result<()> {
    let lock = actions::hook_lock(target, actions::LOCK_WAIT)?;
    eprintln!("{label} '{target}'{} via {cmd}", level_suffix(level));
    let (k, v) = lock.env();
    let status = platform::shell_command(cmd)
        .env("PATH", daemons::hook_path())
        .envs(hook_envs(cmd))
        .env(k, v)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(error::hook_failed(
            status,
            format!(
                "{label} script{} exited with code {}",
                level_suffix(level),
                error::shell_code(status)
            ),
        )
        .target(target)
        .channel("power")
        .into())
    }
}

/// Run a power channel's `state_cmd` and read its verdict: `true` for on,
/// `false` for off, from the first whitespace-delimited token of its stdout
/// (case-insensitive). A failing command is `hook_failed`; any other first
/// token is an error naming what came back.
fn read_state_cmd(cmd: &str, target: &str) -> Result<bool> {
    let lock = actions::hook_lock(target, actions::LOCK_WAIT)?;
    let (k, v) = lock.env();
    let out = platform::shell_command(cmd)
        .env("PATH", daemons::hook_path())
        .envs(hook_envs(cmd))
        .env(k, v)
        .output()?;
    drop(lock);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(error::hook_failed(
            out.status,
            format!(
                "state_cmd '{cmd}' exited with code {} — stdout: {stdout} stderr: {stderr}",
                error::shell_code(out.status)
            ),
        )
        .target(target)
        .channel("power")
        .into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let token = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match token.as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => bail!("state_cmd '{cmd}' output did not begin with 'on' or 'off' — got: {text}"),
    }
}

/// How often `power-cycle` re-reads `state_cmd` while waiting for power to
/// come back, and how long it waits in all.
const POWER_CYCLE_POLL: std::time::Duration = std::time::Duration::from_millis(500);
const POWER_CYCLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Call `is_on` every `poll` until it returns true, for at most `deadline`
/// (always at least once). Ok(true) when power came back, Ok(false) at the
/// deadline; an error from `is_on` ends the wait at once.
fn wait_for_power_on(
    mut is_on: impl FnMut() -> Result<bool>,
    poll: std::time::Duration,
    deadline: std::time::Duration,
) -> Result<bool> {
    let start = std::time::Instant::now();
    loop {
        if is_on()? {
            return Ok(true);
        }
        if start.elapsed() + poll > deadline {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

fn cmd_power_cycle(
    lab_flag: Option<&str>,
    target: Option<&str>,
    level: Option<&str>,
) -> Result<()> {
    let level = named_level(level);
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Power,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let p = local_power_level(&lab, &target, level)?;
    let cmd = p
        .cycle_cmd
        .ok_or_else(|| missing_power_hook(&target, level, "cycle_cmd", "cycle-cmd"))?;
    run_power_hook(&cmd, "Power cycling", &target, level)?;
    // A cycle_cmd may return before the cycle ends: a relay board that
    // accepts "cycle" and times the off period itself (#259). With a
    // state_cmd, "complete" means power is back on; without one, paniolo
    // cannot tell, so it says only that the hook finished. A named level
    // confirms with its own state_cmd only, never the default level's.
    let sfx = level_suffix(level);
    let Some(state_cmd) = p.state_cmd else {
        println!("Power cycle requested{sfx} (cycle_cmd exited 0; no state_cmd to confirm power).");
        return Ok(());
    };
    confirm_power_on(
        &state_cmd,
        &target,
        level,
        POWER_CYCLE_POLL,
        POWER_CYCLE_DEADLINE,
    )?;
    println!("Power cycle complete{sfx} (power is back on).");
    Ok(())
}

/// Wait for `state_cmd` to report `target` on; `timeout` (exit 22, outcome
/// unknown) when it still reports off at `deadline`.
fn confirm_power_on(
    state_cmd: &str,
    target: &str,
    level: Option<&str>,
    poll: std::time::Duration,
    deadline: std::time::Duration,
) -> Result<()> {
    if wait_for_power_on(|| read_state_cmd(state_cmd, target), poll, deadline)? {
        return Ok(());
    }
    Err(error::PanioloError::new(
        error::Kind::Timeout,
        format!(
            "cycle_cmd exited 0, but state_cmd still reports '{target}' off{} after {} ms",
            level_suffix(level),
            deadline.as_millis()
        ),
    )
    .target(target)
    .channel("power")
    .into())
}

fn cmd_power_on(lab_flag: Option<&str>, target: Option<&str>, level: Option<&str>) -> Result<()> {
    let level = named_level(level);
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Power,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let p = local_power_level(&lab, &target, level)?;
    let cmd = p
        .on_cmd
        .ok_or_else(|| missing_power_hook(&target, level, "on_cmd", "on-cmd"))?;
    run_power_hook(&cmd, "Powering on", &target, level)?;
    println!("Power on complete{}.", level_suffix(level));
    Ok(())
}

fn cmd_power_off(lab_flag: Option<&str>, target: Option<&str>, level: Option<&str>) -> Result<()> {
    let level = named_level(level);
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Power,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let p = local_power_level(&lab, &target, level)?;
    let cmd = p
        .off_cmd
        .ok_or_else(|| missing_power_hook(&target, level, "off_cmd", "off-cmd"))?;
    run_power_hook(&cmd, "Powering off", &target, level)?;
    println!("Power off complete{}.", level_suffix(level));
    Ok(())
}

fn cmd_power_state(
    lab_flag: Option<&str>,
    target: Option<&str>,
    level: Option<&str>,
) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Power,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    match level {
        // `--level default`: the default level alone, exactly as before levels.
        Some(model::DEFAULT_POWER_LEVEL) => {}
        Some(l) => {
            let p = local_power_level(&lab, &target, Some(l))?;
            let cmd = p
                .state_cmd
                .ok_or_else(|| missing_power_hook(&target, Some(l), "state_cmd", "state-cmd"))?;
            let on = read_state_cmd(&cmd, &target)?;
            println!("{}", power_state_line(&target, Some(l), Ok(on)));
            return Ok(());
        }
        None if !local_power(&lab, &target)?.levels.is_empty() => {
            return power_state_all_levels(&lab, &target);
        }
        None => {}
    }
    let state = if local_power_is_on(&lab, &target)? {
        "ON"
    } else {
        "OFF"
    };
    println!("Power {state}  ({target})");
    Ok(())
}

/// What one level's state read came to, for [`power_state_line`].
#[derive(Debug)]
enum LevelState {
    /// The level has no way to read state (no `state_cmd`, and for the
    /// default level no sense signal either).
    Unconfigured,
    /// Reading it failed; the error is reported after every line.
    Failed,
}

/// One line of `power-state`: `Power ON  (pi5)` for the default level,
/// `Power OFF  (pi5, level hard)` for a named one, and `Power UNKNOWN …`
/// with the reason when the level cannot be read. Every line starts with
/// `Power <VERDICT>`, so readers that take the first line still work.
fn power_state_line(target: &str, level: Option<&str>, state: Result<bool, LevelState>) -> String {
    let at = match level {
        Some(l) => format!("{target}, level {l}"),
        None => target.to_string(),
    };
    match state {
        Ok(true) => format!("Power ON  ({at})"),
        Ok(false) => format!("Power OFF  ({at})"),
        Err(LevelState::Unconfigured) => format!("Power UNKNOWN  ({at}: no state_cmd)"),
        Err(LevelState::Failed) => format!("Power UNKNOWN  ({at}: state read failed)"),
    }
}

/// `power-state` without `--level` on a target with named levels: one line
/// per level, the default level first (the line the dashboard and the
/// `requires_power` check read). A level without a state source is reported
/// as such, not as an error; a failed read is reported in its line and its
/// error returned after every line is printed. When no level can be read at
/// all, the default level's error is returned, as without levels.
fn power_state_all_levels(lab: &Lab, target: &str) -> Result<()> {
    let p = local_power(lab, target)?;
    let default_readable = p.state_cmd.is_some() || p.serial_interface.is_some();
    if !default_readable && p.levels.values().all(|l| l.state_cmd.is_none()) {
        local_power_is_on(lab, target)?;
    }
    let mut first_err: Option<anyhow::Error> = None;
    let mut report = |level: Option<&str>, read: Option<Result<bool>>| {
        let state = match read {
            None => Err(LevelState::Unconfigured),
            Some(Ok(on)) => Ok(on),
            Some(Err(e)) => {
                first_err.get_or_insert(e);
                Err(LevelState::Failed)
            }
        };
        println!("{}", power_state_line(target, level, state));
    };
    report(
        None,
        default_readable.then(|| local_power_is_on(lab, target)),
    );
    for (name, l) in &p.levels {
        report(
            Some(name),
            l.state_cmd.as_deref().map(|c| read_state_cmd(c, target)),
        );
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Read the target's power state on *this* host: `state_cmd` when configured,
/// else the serial sense signal. The body of `power-state`, shared with the
/// `requires_power` check of `action run`.
fn local_power_is_on(lab: &Lab, target: &str) -> Result<bool> {
    let p = local_power(lab, target)?;
    if let Some(cmd) = p.state_cmd {
        return read_state_cmd(&cmd, target);
    }
    let si = p.serial_interface.ok_or_else(|| {
        error::channel_missing(
            target,
            "power",
            format!(
                "no power serial_interface configured for '{target}' \
             (paniolo power set -t {target} --serial-interface <name>)"
            ),
        )
    })?;
    let daemon = serial::daemon(target).ok_or_else(|| {
        error::daemon_down(
            serial::DAEMON,
            "serialcap daemon not running — start it with `paniolo serial watch`",
        )
        .target(target)
    })?;
    match power::read_power_state(&daemon, &si) {
        Some(on) => Ok(on),
        None => bail!(
            "power state unknown — the sense signal may not be configured on '{si}' \
             (paniolo serial set {si} -t {target} --sense <cts|dsr|dcd|ri>)"
        ),
    }
}

// ── serial runtime bodies ───────────────────────────────────────────────────

/// The target's serial channels visible on *this* host (all of them when
/// running against a shipped slice, the local ones otherwise).
fn local_serials(lab: &Lab, target: &str) -> Result<Vec<model::SerialChannel>> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let dh = t.default_host().to_string();
    Ok(t.serial
        .iter()
        .filter(|s| channel_is_local(lab, s.host.as_deref(), &dh))
        .cloned()
        .collect())
}

fn pick_serial<'a>(
    target: &str,
    serials: &'a [model::SerialChannel],
    name: Option<&str>,
) -> Result<&'a model::SerialChannel> {
    let have = || {
        serials
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match name {
        Some(n) => serials.iter().find(|s| s.name == n).ok_or_else(|| {
            error::channel_missing(
                target,
                "serial",
                format!("no serial interface '{n}' (have: {})", have()),
            )
            .into()
        }),
        None => match serials.len() {
            1 => Ok(&serials[0]),
            0 => Err(error::channel_missing(
                target,
                "serial",
                "no serial interfaces configured (paniolo serial add ...)".to_string(),
            )
            .into()),
            _ => Err(error::PanioloError::new(
                error::Kind::Usage,
                format!(
                    "multiple serial interfaces ({}); specify one with -i",
                    have()
                ),
            )
            .target(target)
            .channel("serial")
            .into()),
        },
    }
}

/// Common preamble for serial runtime commands: resolve the target, dispatch
/// to the channel's host if remote, and return the local serial channels.
fn serial_runtime(
    lab_flag: Option<&str>,
    target: Option<&str>,
    interface: Option<&str>,
    mode: dispatch::Mode,
) -> Result<(String, Vec<model::SerialChannel>)> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) =
        dispatch::maybe_dispatch(&lab, &target, model::ChannelKind::Serial, interface, mode)?
    {
        std::process::exit(code);
    }
    let serials = local_serials(&lab, &target)?;
    Ok((target, serials))
}

fn cmd_serial_connect(
    lab_flag: Option<&str>,
    target: Option<&str>,
    interface: Option<&str>,
) -> Result<()> {
    let (target, serials) =
        serial_runtime(lab_flag, target, interface, dispatch::Mode::Interactive)?;
    let ch = pick_serial(&target, &serials, interface)?;
    serial::exec_tio(&ch.device, ch.baud)
}

fn cmd_serial_watch(lab_flag: Option<&str>, target: Option<&str>, port: u16) -> Result<()> {
    let (target, serials) = serial_runtime(lab_flag, target, None, dispatch::Mode::Reexec)?;
    if serials.is_empty() {
        return Err(error::channel_missing(
            &target,
            "serial",
            "no serial interfaces configured (paniolo serial add ...)".to_string(),
        )
        .into());
    }
    let mut replaced = None;
    if let Some(url) = serial::daemon_url(&target) {
        if daemons::binary_is_stale(serial::DAEMON, Some(&target)) == Some(true) {
            eprintln!("Serial daemon for '{target}' was built from an older binary; restarting…");
            replaced = stop_capture_daemon_and_wait(serial::DAEMON, &target)?;
        } else {
            println!("Serial daemon for '{target}' already running at {url}");
            return Ok(());
        }
    }
    // Nothing tracked is running. An orphan that outlived its discovery file
    // still holds the serial ports and the advisory lock, so a replacement
    // started over the top of it dies on the lock while every other command
    // keeps calling the channel stopped (#187).
    if replaced.is_none() {
        if let Some(orphan) = serial::untracked(&serials) {
            reap_untracked(&orphan)?;
        }
    }
    // `--port` wins; without it, the lab's fixed port (or OS-assigned).
    let port = if port != 0 {
        port
    } else {
        serials
            .iter()
            .filter_map(|s| s.port)
            .find(|p| *p != 0)
            .unwrap_or(0)
    };
    let mut child = serial::start_daemon(&serials, port, &target)?;
    let names: Vec<&str> = serials.iter().map(|s| s.name.as_str()).collect();
    eprintln!(
        "Starting serial daemon for '{target}' ({} interface(s): {})…",
        serials.len(),
        names.join(", ")
    );
    let url = wait_for_started_daemon(serial::DAEMON, Some(&target), replaced, Some(&mut child))?;
    println!("Serial daemon started. {url}");
    Ok(())
}

fn cmd_serial_send(
    lab_flag: Option<&str>,
    target: Option<&str>,
    interface: Option<&str>,
    text: &str,
    pace_ms: u32,
    newline: bool,
) -> Result<()> {
    let (target, serials) = serial_runtime(lab_flag, target, interface, dispatch::Mode::Reexec)?;
    let ch = pick_serial(&target, &serials, interface)?;
    let daemon = serial::daemon(&target).ok_or_else(|| {
        error::daemon_down(
            serial::DAEMON,
            "serialcap daemon not running — start it with `paniolo serial watch`",
        )
        .target(&target)
    })?;
    let mut payload = text.as_bytes().to_vec();
    if newline {
        payload.push(b'\r');
    }
    let pace_note = if pace_ms > 0 {
        format!(" paced {pace_ms} ms/byte")
    } else {
        String::new()
    };
    eprintln!(
        "Sending {} bytes to '{}'{pace_note}",
        payload.len(),
        ch.name
    );
    serial::send_input(&daemon, &ch.name, &payload, pace_ms)
        .map_err(|e| error::with_target(e, &target))?;
    println!("Sent.");
    Ok(())
}

fn cmd_serial_transact(
    lab_flag: Option<&str>,
    target: Option<&str>,
    interface: Option<&str>,
    text: &str,
    newline: bool,
    options: &serial::TransactOptions<'_>,
) -> Result<()> {
    let (target, serials) = serial_runtime(lab_flag, target, interface, dispatch::Mode::Reexec)?;
    let ch = pick_serial(&target, &serials, interface)?;
    let daemon = serial::daemon(&target).ok_or_else(|| {
        error::daemon_down(
            serial::DAEMON,
            "serialcap daemon not running — start it with `paniolo serial watch`",
        )
        .target(&target)
    })?;
    let mut payload = text.as_bytes().to_vec();
    if newline {
        payload.push(b'\r');
    }
    let result = serial::transact(&daemon, &ch.name, &payload, options)
        .map_err(|e| error::with_target(e, &target))?;
    println!("{result}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_serial_log(
    lab_flag: Option<&str>,
    target: Option<&str>,
    interface: Option<&str>,
    tail: Option<u64>,
    from: Option<u64>,
    to: Option<u64>,
    since: Option<u64>,
    raw: bool,
    json: bool,
    no_pending: bool,
    require_live: bool,
) -> Result<()> {
    // Dispatch to the channel's host; the capture log lives where the daemon ran.
    let (target, serials) = serial_runtime(lab_flag, target, interface, dispatch::Mode::Reexec)?;
    // The log is read from disk either way, so a stopped daemon still serves
    // it, with nothing new since it stopped (design D4). Say so; with
    // --require-live, refuse instead. "Stopped" has to mean no serialcap holds
    // the ports: one whose discovery file was swept away is still capturing
    // (#187, #195), so it counts as live, as in `serial show`.
    if serial::daemon(&target).is_none() && serial::untracked(&serials).is_none() {
        let msg = format!(
            "serialcap daemon for '{target}' is not running — the log shows \
             nothing captured since it stopped (start it with `paniolo serial watch`)"
        );
        if require_live {
            return Err(error::daemon_down(serial::DAEMON, msg)
                .target(&target)
                .into());
        }
        eprintln!("warning: {msg}");
    }
    // serialcap reads its own on-disk log, so this works daemon-up or -down.
    // The per-target env points it at this target's capture dir.
    let binary = daemons::find_binary(serial::DAEMON).ok_or_else(|| {
        crate::error::PanioloError::not_configured(
            "serialcap not found — run `paniolo setup`".to_string(),
        )
    })?;
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("log");
    cmd.envs(daemons::helper_env(serial::DAEMON, Some(&target)));
    // Name the interface explicitly when we can (sole or selected).
    if let Some(name) = interface.or_else(|| {
        if serials.len() == 1 {
            Some(serials[0].name.as_str())
        } else {
            None
        }
    }) {
        cmd.arg("--interface").arg(name);
    }
    if let Some(n) = tail {
        cmd.arg("--tail").arg(n.to_string());
    }
    if let Some(n) = from {
        cmd.arg("--from").arg(n.to_string());
    }
    if let Some(n) = to {
        cmd.arg("--to").arg(n.to_string());
    }
    if let Some(n) = since {
        cmd.arg("--since").arg(n.to_string());
    }
    if raw {
        cmd.arg("--raw");
    }
    if json {
        cmd.arg("--json");
    }
    if no_pending {
        cmd.arg("--no-pending");
    }
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(error::hook_failed(
            status,
            format!(
                "serialcap log exited with code {}",
                error::shell_code(status)
            ),
        )
        .target(&target)
        .channel("serial")
        .into())
    }
}

fn cmd_serial_show(lab_flag: Option<&str>, target: Option<&str>) -> Result<()> {
    let (target, serials) = serial_runtime(lab_flag, target, None, dispatch::Mode::Reexec)?;
    if serials.is_empty() {
        println!("No serial interfaces configured. (paniolo serial add ...)");
        return Ok(());
    }
    for ch in &serials {
        let sense = ch
            .power_sense_signal
            .as_deref()
            .map(|s| format!("  (sense: {s})"))
            .unwrap_or_default();
        println!("{}\t{} @ {}{sense}", ch.name, ch.device, ch.baud);
    }
    match serial::daemon_url(&target) {
        Some(url) => println!(
            "daemon\trunning at {url}{}",
            stale_note(serial::DAEMON, &target)
        ),
        // "stopped" has to mean the ports are free. A daemon whose discovery
        // file was deleted under it is still running and still holding them
        // (#187).
        None => match serial::untracked(&serials) {
            Some(u) => println!(
                "daemon\trunning, untracked (pid {}) — no discovery file; \
                 `paniolo serial watch` reclaims it",
                u.pid
            ),
            None => println!("daemon\tstopped"),
        },
    }
    Ok(())
}

/// Content change or unknown identity, appended to channel status lines.
fn stale_note(daemon: &str, target: &str) -> String {
    daemons::binary_status(daemon, Some(target)).note()
}

// ── video runtime bodies ────────────────────────────────────────────────────

/// The target's video channel as visible on *this* host.
fn local_video(lab: &Lab, target: &str) -> Result<model::VideoChannel> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let dh = t.default_host().to_string();
    let v = t.video.clone().ok_or_else(|| {
        error::channel_missing(target, "video", format!("target '{target}' has no video channel (paniolo video set -t {target} --device ...)"))
    })?;
    if !channel_is_local(lab, v.host.as_deref(), &dh) {
        return Err(error::channel_missing(
            target,
            "video",
            format!("video channel for '{target}' is not on this host"),
        )
        .into());
    }
    Ok(v)
}

/// Refuse `--open` when the target's video channel lives on another host.
///
/// A remote channel re-execs the whole argv on the control host
/// (`dispatch::subcommand_args`), `--open` included, so the browser would open
/// *there* — on a headless bench machine, silently, while the caller is told it
/// opened. Checked before `video_runtime`, which is where that re-exec happens
/// and never returns.
fn refuse_remote_open(lab_flag: Option<&str>, target: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    let rt = lab
        .resolved_target(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let host_name = model::channel_host(&rt, model::ChannelKind::Video, None)?;
    if lab.host(&host_name).is_local(&host_name) {
        return Ok(());
    }
    bail!(
        "`video preview --open` opens a browser on the machine that runs it, and \
         '{target}'s video channel is on '{host_name}' — it would open there. \
         Use `paniolo console {target}`, which forwards the ports and opens a browser \
         here, or `paniolo video preview {target}` to print the URL and open it yourself."
    )
}

fn video_runtime(
    lab_flag: Option<&str>,
    target: Option<&str>,
) -> Result<(String, model::VideoChannel)> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Video,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let v = local_video(&lab, &target)?;
    Ok((target, v))
}

fn video_cmd(lab_flag: Option<&str>, cmd: VideoCmd) -> Result<()> {
    match cmd {
        VideoCmd::Set {
            target,
            device,
            ocr_mode,
            change_threshold,
            host,
            port,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_video(
                    &target,
                    Some(&device),
                    ocr_mode.as_deref(),
                    change_threshold,
                    host.as_deref(),
                    port,
                )
            })?;
            println!("video channel set for '{target}'.");
            Ok(())
        }
        VideoCmd::Rm { target } => {
            edit_lab(lab_flag, |lf| lf.remove_video(&target))?;
            println!("video channel removed from '{target}'.");
            Ok(())
        }
        VideoCmd::Watch {
            target,
            port,
            restart,
        } => {
            let (target, v) = video_runtime(lab_flag, target.name())?;
            let device = v.device.ok_or_else(|| {
                error::channel_missing(
                    &target,
                    "video",
                    format!("video channel for '{target}' has no device set"),
                )
            })?;
            let mut replaced = None;
            // `daemon_url`, not `preview_url`: this scope only needs to know
            // whether a daemon is there and what to print, and no token-bearing
            // string should exist here to be printed by accident (#196).
            if let Some(url) = video::daemon_url(&target) {
                let stale = daemons::binary_is_stale(video::DAEMON, Some(&target)) == Some(true);
                if !restart && !stale {
                    // A status line, not an invitation: this branch starts
                    // nothing, and it is the idempotent "make sure the daemon
                    // is up" call an agent makes before every screenshot, so
                    // the token would land in the transcript on every one
                    // (#196). `video preview` is where the openable URL lives.
                    println!(
                        "Video daemon for '{target}' already running at {url} — \
                         `paniolo video preview {target}` opens the dashboard."
                    );
                    return Ok(());
                }
                if stale && !restart {
                    eprintln!(
                        "Video daemon for '{target}' was built from an older binary; restarting…"
                    );
                }
                replaced = stop_capture_daemon_and_wait(video::DAEMON, &target)?;
            }
            // Nothing tracked is running. An orphan that outlived its
            // discovery file still holds the capture device and the advisory
            // lock, so a replacement started over the top of it dies on the
            // lock ("another hdmicap daemon is already running") while every
            // other command keeps calling the channel stopped (#187).
            else if let Some(orphan) = video::untracked(&device) {
                reap_untracked(&orphan)?;
            }
            let hid_input = ensure_video_source(&load_for_read(lab_flag)?, &target, &device)?;
            eprintln!("Starting video daemon for '{target}' ('{device}')…");
            // `--port` wins; without it, the lab's fixed port (or OS-assigned).
            let port = if port != 0 {
                port
            } else {
                model::fixed_port(v.port)
            };
            let mut child = video::start_daemon(
                &device,
                port,
                &target,
                v.ocr_mode.as_deref(),
                v.change_threshold,
                hid_input.as_deref(),
            )?;
            let url =
                wait_for_started_daemon(video::DAEMON, Some(&target), replaced, Some(&mut child))?;
            // Token-free, like every other line paniolo prints: the openable
            // URL lives behind `video preview`, which is the one command whose
            // job is to produce it (#196). Printing it here would put a live
            // credential in the transcript of every daemon start.
            let url = video::daemon_url(&target).unwrap_or(url);
            println!(
                "Video daemon started at {url} — `paniolo video preview {target}` \
                 opens the dashboard."
            );
            Ok(())
        }
        VideoCmd::Stop { target } => {
            // Resolve the target (routing to its video channel's host if
            // remote) so we stop the right per-target daemon instance.
            let (target, v) = video_runtime(lab_flag, target.name())?;
            // An orphan has no discovery file, so `hdmicap stop` cannot see it
            // and the operator has nothing left but `ps` and `kill` (#187).
            // Only consulted when nothing is tracked: a healthy daemon is
            // never in this set, and its own stop is the clean shutdown.
            if video::daemon(&target).is_none() {
                if let Some(orphan) = v.device.as_deref().and_then(video::untracked) {
                    reap_untracked(&orphan)?;
                    println!(
                        "Untracked video daemon for '{target}' (pid {}) stopped.",
                        orphan.pid
                    );
                    return Ok(());
                }
            }
            let status = video::stop_daemon(&target)?;
            if status.success() {
                println!("Video daemon for '{target}' stopped.");
                Ok(())
            } else {
                Err(error::hook_failed(
                    status,
                    format!(
                        "hdmicap stop exited with code {}",
                        error::shell_code(status)
                    ),
                )
                .target(&target)
                .channel("video")
                .into())
            }
        }
        VideoCmd::Shot {
            target,
            stable,
            changed_since,
            timeout,
            out,
        } => {
            // Remote channel + a real --out path: `--out` must mean the
            // invoking machine's filesystem, so run the remote shot with
            // `--out -` and stream the PNG into the local file instead of
            // re-execing verbatim (which would write on the control host).
            if out != "-" {
                let lab = load_for_read(lab_flag)?;
                let target_name = resolve_single_target(&lab, target.name())?;
                let rt = lab
                    .resolved_target(&target_name)
                    .ok_or_else(|| error::target_not_found(&target_name))?;
                let host_name = model::channel_host(&rt, model::ChannelKind::Video, None)?;
                if !lab.host(&host_name).is_local(&host_name) {
                    let mut sub = vec![
                        "video".to_string(),
                        "shot".to_string(),
                        target_name.clone(),
                        "--timeout".to_string(),
                        timeout.to_string(),
                        "--out".to_string(),
                        "-".to_string(),
                    ];
                    if stable {
                        sub.push("--stable".to_string());
                    }
                    if let Some(h) = &changed_since {
                        sub.push("--changed-since".to_string());
                        sub.push(h.clone());
                    }
                    let code = dispatch::dispatch_stdout_to_file(
                        &lab,
                        &target_name,
                        &host_name,
                        &sub,
                        &out,
                    )?;
                    if code == 0 {
                        eprintln!("saved to {out}");
                    }
                    std::process::exit(code);
                }
            }
            let (target, _v) = video_runtime(lab_flag, target.name())?;
            let mut args = vec![
                "shot".to_string(),
                "--timeout".to_string(),
                timeout.to_string(),
                "--out".to_string(),
                out,
            ];
            if stable {
                args.push("--stable".to_string());
            }
            if let Some(h) = changed_since {
                args.push("--changed-since".to_string());
                args.push(h);
            }
            std::process::exit(video::passthrough(&args, Some(&target))?);
        }
        VideoCmd::Read {
            target,
            stable,
            timeout,
            json,
        } => {
            let (target, _v) = video_runtime(lab_flag, target.name())?;
            let body = video::ocr(&target, stable, timeout)?;
            let out = if json { body } else { video::text_of(&body) };
            print!("{out}");
            if !out.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        VideoCmd::Preview { target, open } => {
            if open {
                refuse_remote_open(lab_flag, target.name())?;
            }
            let (target, _v) = video_runtime(lab_flag, target.name())?;
            let Some(daemon) = video::daemon(&target) else {
                return Err(error::daemon_down(
                    video::DAEMON,
                    "no video daemon running — start one with `paniolo video watch`",
                )
                .target(&target)
                .into());
            };
            let url = daemon.http_url("/");
            if open {
                // The token goes to the browser, not the terminal (#196) —
                // unless there is no browser, in which case printing it is the
                // only way the caller reaches the page at all.
                if open_in_browser(&url) {
                    println!("Opened {} in the browser.", daemon.base_url());
                } else {
                    println!("{}", console_fallback(&target, &url));
                }
            } else {
                // A browser can only present the token as ?token=, and the
                // caller asked for the URL to open, so this one carries it.
                println!("{url}");
            }
            Ok(())
        }
        VideoCmd::Devices => {
            std::process::exit(video::passthrough(&["devices".to_string()], None)?);
        }
        VideoCmd::Show { target } => {
            let (target, v) = video_runtime(lab_flag, target.name())?;
            println!("device\t{}", v.device.as_deref().unwrap_or("(not set)"));
            match v.change_threshold {
                Some(n) => println!("change_threshold\t{n}"),
                None => println!("change_threshold\t(default)"),
            }
            match video::daemon_url(&target) {
                Some(url) => {
                    println!(
                        "{}",
                        video_show_daemon_line(&url, &stale_note(video::DAEMON, &target))
                    );
                    // A one-time pointer, not a field: `show`'s stdout is
                    // `key<TAB>value` that scripts split on, and this is prose.
                    eprintln!(
                        "note: `paniolo video preview` prints the openable URL; it \
                         carries the daemon's token, which is not shown here (#196)."
                    );
                }
                // "stopped" has to mean the device is free. A daemon whose
                // discovery file was deleted under it is still running and
                // still holding the device, and saying otherwise is what makes
                // the next `video watch` fail inexplicably (#187).
                None => match v.device.as_deref().and_then(video::untracked) {
                    Some(u) => println!(
                        "daemon\trunning, untracked (pid {}) — no discovery file; \
                         `paniolo video watch` reclaims it",
                        u.pid
                    ),
                    None => println!("daemon\tstopped"),
                },
            }
            Ok(())
        }
    }
}

fn netboot_cmd(lab_flag: Option<&str>, cmd: NetbootCmd) -> Result<()> {
    match cmd {
        NetbootCmd::Set {
            target,
            interface,
            host_ip,
            tftp_root,
            boot_file,
            http_port,
            content_type,
            host,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_netboot(
                    &target,
                    interface.as_deref(),
                    host_ip.as_deref(),
                    tftp_root.as_deref(),
                    boot_file.as_deref(),
                    http_port.as_deref(),
                    content_type.as_deref(),
                    host.as_deref(),
                )
            })?;
            println!("netboot channel set for '{target}'.");
            Ok(())
        }
        NetbootCmd::Rm { target } => {
            edit_lab(lab_flag, |lf| lf.remove_netboot(&target))?;
            println!("netboot channel removed from '{target}'.");
            Ok(())
        }
        NetbootCmd::Start { target } => {
            let runtime = netboot_runtime(lab_flag, target.name())?;
            let root = runtime.require_tftp_root()?.to_string();
            let NetbootRuntime {
                target,
                interface: iface,
                host_ip,
                boot,
                ..
            } = runtime;
            netboot::start(&target, &iface, &host_ip, &root, &boot)?;
            println!("netboot started for '{target}' on {iface} ({host_ip}, tftp {root}).");
            Ok(())
        }
        NetbootCmd::Stop { target } => {
            let NetbootRuntime { target, .. } = netboot_runtime(lab_flag, target.name())?;
            netboot::stop(&target)?;
            println!("netboot stopped for '{target}'.");
            Ok(())
        }
        NetbootCmd::Status { target } => {
            let NetbootRuntime {
                target, host_ip, ..
            } = netboot_runtime(lab_flag, target.name())?;
            let st = netboot::status(&target);
            match st.state {
                None => println!("netboot\tnot running (no state)"),
                Some(s) => {
                    println!(
                        "netboot\t{}",
                        if st.running {
                            "running"
                        } else {
                            "NOT running (stale state)"
                        }
                    );
                    println!("pid\t{}", s.dhcp_pid);
                    println!("interface\t{}", s.interface);
                    println!("host_ip\t{host_ip}");
                    println!("tftp_root\t{}", s.tftp_root);
                    if let Some(up) = st.uptime_seconds {
                        println!("uptime\t{:.0}s", up);
                    }
                }
            }
            Ok(())
        }
        NetbootCmd::Logs {
            target,
            tail,
            follow,
        } => {
            let NetbootRuntime { target, .. } = netboot_runtime(lab_flag, target.name())?;
            cmd_netboot_logs(&target, tail, follow)
        }
        NetbootCmd::TftpRoot { target } => {
            let runtime = netboot_runtime(lab_flag, target.name())?;
            println!("{}", runtime.require_tftp_root()?);
            Ok(())
        }
        NetbootCmd::Devices => {
            let ifaces = netif::list_usb_ethernet_interfaces();
            if ifaces.is_empty() {
                println!("No candidate Ethernet interfaces found.");
            }
            for i in ifaces {
                println!(
                    "{}\t{}\t{}",
                    i.device,
                    i.port,
                    if i.active { "active" } else { "inactive" }
                );
            }
            Ok(())
        }
    }
}

/// The resolved local netboot channel for a runtime command.
struct NetbootRuntime {
    target: String,
    host: String,
    interface: String,
    host_ip: String,
    tftp_root: Option<String>,
    boot: netboot::BootOptions,
}

impl NetbootRuntime {
    /// Read required configuration before any interface or daemon operation.
    fn require_tftp_root(&self) -> Result<&str> {
        self.tftp_root
            .as_deref()
            .filter(|root| !root.is_empty())
            .ok_or_else(|| {
                error::channel_missing(
                    &self.target,
                    "netboot",
                    format!(
                        "no tftp_root configured \
                         (paniolo netboot set -t {} --tftp-root <path>)",
                        self.target
                    ),
                )
                .host(&self.host)
                .into()
            })
    }
}

/// Common preamble for netboot/netif runtime commands: resolve, dispatch to the
/// netboot channel's host if remote, and return the local channel's runtime.
fn netboot_runtime(lab_flag: Option<&str>, target: Option<&str>) -> Result<NetbootRuntime> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Netboot,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let t = lab
        .targets
        .get(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let dh = t.default_host().to_string();
    let nb = t.netboot.clone().ok_or_else(|| {
        error::channel_missing(
            &target,
            "netboot",
            format!(
                "target '{target}' has no netboot channel (paniolo netboot set -t {target} ...)"
            ),
        )
    })?;
    if !channel_is_local(&lab, nb.host.as_deref(), &dh) {
        return Err(error::channel_missing(
            &target,
            "netboot",
            format!("netboot channel for '{target}' is not on this host"),
        )
        .into());
    }
    let host_ip = nb.effective_host_ip().to_string();
    let interface = nb.interface.ok_or_else(|| {
        error::channel_missing(
            &target,
            "netboot",
            format!("netboot channel for '{target}' has no interface set"),
        )
    })?;
    Ok(NetbootRuntime {
        target,
        host: nb.host.unwrap_or(dh),
        interface,
        host_ip,
        tftp_root: nb.tftp_root,
        boot: netboot::BootOptions {
            boot_file: nb.boot_file,
            http_port: nb.http_port,
            content_type: nb.content_type,
        },
    })
}

fn cmd_netboot_logs(target: &str, tail: usize, follow: bool) -> Result<()> {
    let path = state::netboot_log_path(target);
    if !path.exists() {
        bail!("no netboot log for '{target}' yet ({})", path.display());
    }
    let text = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail);
    for line in &lines[start..] {
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }
    // Follow: poll for appended bytes (Ctrl-C to stop).
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(&path)?;
    f.seek(std::io::SeekFrom::End(0))?;
    let mut id = file_identity(&path);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bytes, not `read_to_string`: netbootd's stderr can carry a stray
        // non-UTF-8 byte (a client's MAC or hostname echoed back, say), and
        // `read_to_string` would abort the whole `--follow` loop with an
        // error on the first one instead of just showing it oddly.
        f.read_to_end(&mut buf)?;
        if !buf.is_empty() {
            print!("{}", String::from_utf8_lossy(&buf));
            use std::io::Write;
            std::io::stdout().flush()?;
        }
        // `daemons::create_log` recreates this file on every daemon start
        // (truncate-on-open); the already-open handle above keeps reading
        // the old, now-unlinked inode, which never gets new bytes again — a
        // restart mid-`--follow` would otherwise go silent forever. Reopen
        // and read the new file's content immediately rather than waiting
        // out a sleep first.
        let current = file_identity(&path);
        let pos = f.stream_position()?;
        let len = std::fs::metadata(&path).ok().map(|m| m.len());
        if log_was_replaced(id, current, pos, len) {
            if let Ok(reopened) = std::fs::File::open(&path) {
                f = reopened;
                id = current;
                continue;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Whether the log a `--follow` loop is reading has been replaced under it.
/// Two signals, either sufficient: the path now names a different file
/// (`create_log` on some platforms, or an editor's rename-into-place), or the
/// file at the path is shorter than the position we have read to (truncate-
/// in-place reuses the inode, so identity alone would never notice). A
/// momentarily missing file (mid-recreate) is neither: keep the handle.
fn log_was_replaced(
    opened_id: Option<u64>,
    current_id: Option<u64>,
    read_pos: u64,
    current_len: Option<u64>,
) -> bool {
    let different_file = current_id.is_some() && current_id != opened_id;
    let truncated = current_len.is_some_and(|len| len < read_pos);
    different_file || truncated
}

/// A value identifying which underlying file `path` currently names — the
/// inode on Unix — so a poll loop can tell "this path now names a new file"
/// from "this path's file just grew". `None` when the file is momentarily
/// missing (mid-recreate), and always on Windows, where std's file index is
/// still unstable; there [`log_was_replaced`] falls back to the shrunk-file
/// signal alone, which is the one that fires for an in-place truncation on
/// every platform anyway.
fn file_identity(path: &std::path::Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn netif_cmd(lab_flag: Option<&str>, cmd: NetifCmd) -> Result<()> {
    match cmd {
        NetifCmd::Mode { mode, target } => {
            if !netif::MODES.contains(&mode.as_str()) {
                bail!(
                    "unknown mode '{mode}' (use one of: {})",
                    netif::MODES.join(", ")
                );
            }
            let runtime = netboot_runtime(lab_flag, target.name())?;
            let NetbootRuntime {
                target,
                interface: iface,
                host_ip,
                boot,
                ..
            } = &runtime;
            match mode.as_str() {
                "netboot" => {
                    netif::mode_netboot(
                        target,
                        iface,
                        host_ip,
                        runtime.require_tftp_root()?,
                        boot,
                    )?;
                }
                "link" => netif::mode_link(target, iface, host_ip)?,
                "ffx" => netif::mode_ffx(target, iface)?,
                _ => netif::mode_off(target, iface, host_ip)?,
            }
            print_netif_status(target, iface, host_ip);
            Ok(())
        }
        NetifCmd::DownHard { target } => {
            let NetbootRuntime {
                target,
                interface: iface,
                host_ip,
                ..
            } = netboot_runtime(lab_flag, target.name())?;
            netif::down_hard(&target, &iface, &host_ip)?;
            print_netif_status(&target, &iface, &host_ip);
            Ok(())
        }
        NetifCmd::Status { target } => {
            let NetbootRuntime {
                target,
                interface: iface,
                host_ip,
                ..
            } = netboot_runtime(lab_flag, target.name())?;
            print_netif_status(&target, &iface, &host_ip);
            Ok(())
        }
    }
}

fn print_netif_status(target: &str, iface: &str, host_ip: &str) {
    let s = netif::get_status(target, iface, host_ip);
    println!("target\t{target}");
    println!("interface\t{iface}");
    println!("mode\t{}", s.mode);
    println!("carrier\t{}", if s.carrier { "up" } else { "down" });
    println!(
        "inet\t{}",
        if s.inet.is_empty() {
            "(none)".to_string()
        } else {
            s.inet.join(" ")
        }
    );
    println!(
        "inet6\t{}",
        if s.inet6.is_empty() {
            "(none)".to_string()
        } else {
            s.inet6.join(" ")
        }
    );
    if s.mode == "netboot" {
        println!("dhcp+tftp\tserving on {host_ip}/24");
    }
    if s.mode == "link" {
        println!("link\thost IP {host_ip} up, no daemon (bare link)");
    }
    if s.mode == "ffx" {
        if s.peers.is_empty() {
            println!("peer\t(none discovered yet — power-cycle the target and wait for SLAAC)");
        }
        for peer in &s.peers {
            println!("peer\t{peer}%{iface}  (try: ffx target add {peer}%{iface})");
        }
    }
}

fn power_cmd(lab_flag: Option<&str>, cmd: PowerCmd) -> Result<()> {
    match cmd {
        PowerCmd::Set {
            target,
            level: Some(level),
            cycle_cmd,
            on_cmd,
            off_cmd,
            state_cmd,
            doctor,
            ..
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_power_level(
                    &target,
                    &level,
                    cycle_cmd.as_deref(),
                    on_cmd.as_deref(),
                    off_cmd.as_deref(),
                    state_cmd.as_deref(),
                )?;
                // Whole channel, like --host, but harmless beside --level:
                // the flag already covers every level's hooks.
                set_doctor_flag(lf, &target, "power", &doctor)
            })?;
            println!("power level '{level}' set for '{target}'.");
            Ok(())
        }
        PowerCmd::Set {
            target,
            level: None,
            cycle_cmd,
            on_cmd,
            off_cmd,
            state_cmd,
            serial_interface,
            host,
            doctor,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_power(
                    &target,
                    cycle_cmd.as_deref(),
                    on_cmd.as_deref(),
                    off_cmd.as_deref(),
                    state_cmd.as_deref(),
                    serial_interface.as_deref(),
                    host.as_deref(),
                )?;
                set_doctor_flag(lf, &target, "power", &doctor)
            })?;
            println!("power channel set for '{target}'.");
            Ok(())
        }
        PowerCmd::Rm {
            target,
            level: Some(level),
        } => {
            edit_lab(lab_flag, |lf| lf.remove_power_level(&target, &level))?;
            println!("power level '{level}' removed from '{target}'.");
            Ok(())
        }
        PowerCmd::Rm {
            target,
            level: None,
        } => {
            edit_lab(lab_flag, |lf| lf.remove_power(&target))?;
            println!("power channel removed from '{target}'.");
            Ok(())
        }
        PowerCmd::On { target, level } => cmd_power_on(lab_flag, target.name(), level.as_deref()),
        PowerCmd::Off { target, level } => cmd_power_off(lab_flag, target.name(), level.as_deref()),
    }
}

fn hid_cmd(lab_flag: Option<&str>, cmd: HidCmd) -> Result<()> {
    match cmd {
        HidCmd::Set {
            target,
            cmd,
            host,
            port,
            doctor,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_hid(&target, Some(&cmd), host.as_deref(), port)?;
                set_doctor_flag(lf, &target, "hid", &doctor)
            })?;
            println!("hid channel set for '{target}'.");
            Ok(())
        }
        HidCmd::Rm { target } => {
            edit_lab(lab_flag, |lf| lf.remove_hid(&target))?;
            println!("hid channel removed from '{target}'.");
            Ok(())
        }
        HidCmd::Send { target, args } => cmd_hid_send(lab_flag, target.as_deref(), &args),
        HidCmd::Serve { target } => cmd_hid_serve(lab_flag, target.name()),
        HidCmd::Stop { target } => cmd_hid_stop(lab_flag, target.name()),
    }
}

/// The `hid` daemon discovery name (must match hidrig's DISCOVERY_NAME).
const HID_DAEMON: &str = "hid";

fn action_cmd(lab_flag: Option<&str>, cmd: ActionCmd) -> Result<()> {
    match cmd {
        ActionCmd::List { target, json } => cmd_action_list(lab_flag, target.as_deref(), json),
        ActionCmd::Run {
            target,
            name,
            power_checked,
        } => cmd_action_run(lab_flag, target.as_deref(), &name, power_checked),
        ActionCmd::Set {
            target,
            name,
            cmd,
            description,
            requires_power,
            host,
            doctor,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_action(
                    &target,
                    &name,
                    cmd.as_deref(),
                    description.as_deref(),
                    requires_power.as_deref(),
                    host.as_deref(),
                )?;
                set_doctor_flag(lf, &target, "actions", &doctor)
            })?;
            println!("action '{name}' set for '{target}'.");
            Ok(())
        }
        ActionCmd::Rm { target, name } => {
            edit_lab(lab_flag, |lf| lf.remove_action(&target, &name))?;
            println!("action '{name}' removed from '{target}'.");
            Ok(())
        }
    }
}

/// One row of `action list`: what the action does and where it runs.
fn action_rows(lab: &Lab, target: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let names: Vec<String> = match target {
        Some(t) => {
            if !lab.targets.contains_key(t) {
                return Err(error::target_not_found(t).into());
            }
            vec![t.to_string()]
        }
        None => lab.targets.keys().cloned().collect(),
    };
    let mut rows = Vec::new();
    for tn in names {
        let t = &lab.targets[&tn];
        let Some(acts) = &t.actions else { continue };
        let host = acts.host.as_deref().unwrap_or(t.default_host());
        for (name, a) in &acts.actions {
            rows.push(serde_json::json!({
                "target": tn,
                "name": name,
                "description": a.description,
                "requires_power": a.requires_power,
                "host": host,
            }));
        }
    }
    Ok(rows)
}

/// `action list`: config only, so it reads the lab here and never dispatches.
fn cmd_action_list(lab_flag: Option<&str>, target: Option<&str>, json: bool) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let rows = action_rows(&lab, target)?;
    if json {
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No actions configured (paniolo action set -t <target> <name> --cmd ...).");
        return Ok(());
    }
    for r in rows {
        let mut line = format!(
            "{}\t{}\t{}",
            r["target"].as_str().unwrap_or(""),
            r["name"].as_str().unwrap_or(""),
            r["description"].as_str().unwrap_or("")
        );
        if let Some(rp) = r["requires_power"].as_str() {
            line.push_str(&format!("  (requires power {rp})"));
        }
        println!("{line}");
    }
    Ok(())
}

/// `action run`: check `requires_power` (wherever the power channel is), then
/// run the hook on the actions' host — locally under the target's hook lock,
/// or by re-exec there with `--power-checked` so the far side, whose lab
/// slice may hold no power channel, does not check again.
///
/// The power check runs before the lock is taken, because reading the state
/// takes that same lock; a power change in between is not prevented.
fn cmd_action_run(
    lab_flag: Option<&str>,
    target: Option<&str>,
    name: &str,
    power_checked: bool,
) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    let action = actions::find(&lab, &target, name)?.clone();
    if let (Some(requires), false) = (action.requires_power.as_deref(), power_checked) {
        let is_on = power_is_on_anywhere(&lab, &target)?;
        if let Some(refusal) = actions::power_refusal(&target, name, requires, is_on) {
            return Err(refusal.into());
        }
    }
    let rt = lab
        .resolved_target(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let host_name = model::channel_host(&rt, model::ChannelKind::Actions, None)?;
    if !lab.host(&host_name).is_local(&host_name) {
        let mut argv = dispatch::subcommand_args();
        if !power_checked {
            argv.push("--power-checked".to_string());
        }
        let code = dispatch::dispatch(&lab, &target, &host_name, dispatch::Mode::Reexec, &argv)?;
        std::process::exit(code);
    }
    let lock = actions::hook_lock(&target, actions::LOCK_WAIT)?;
    actions::run_hook(&lock, &target, name, &action.cmd)?;
    println!("Action '{name}' complete.");
    Ok(())
}

/// The target's power state, read on whichever host has its power channel:
/// in process when that is this host, else `power-state` over SSH. A remote
/// failure keeps its kind (from the exit status) and its stderr.
fn power_is_on_anywhere(lab: &Lab, target: &str) -> Result<bool> {
    let rt = lab
        .resolved_target(target)
        .ok_or_else(|| error::target_not_found(target))?;
    if !rt
        .channels
        .iter()
        .any(|c| c.kind == model::ChannelKind::Power)
    {
        return Err(error::channel_missing(
            target,
            "power",
            format!(
                "an action of '{target}' sets requires_power, but the target has no power \
                 channel to read (paniolo power set -t {target} --state-cmd ...)"
            ),
        )
        .into());
    }
    let host_name = model::channel_host(&rt, model::ChannelKind::Power, None)?;
    if lab.host(&host_name).is_local(&host_name) {
        return local_power_is_on(lab, target);
    }
    // With named levels, `power-state` alone reports every level; ask for the
    // default one, which is what requires_power means. Without levels the
    // plain form is kept, so an older paniolo on the power host still works.
    let has_levels = lab
        .targets
        .get(target)
        .and_then(|t| t.power.as_ref())
        .is_some_and(|p| !p.levels.is_empty());
    let mut argv = vec!["power-state", "-t", target];
    if has_levels {
        argv.extend(["--level", model::DEFAULT_POWER_LEVEL]);
    }
    let out = dispatch::run_subcommand(lab, target, &host_name, &argv)?;
    if out.status != 0 {
        let kind = error::Kind::from_exit_code(out.status).unwrap_or(error::Kind::Internal);
        return Err(error::PanioloError::new(
            kind,
            format!(
                "reading power state on '{host_name}' failed: {}",
                out.stderr.trim()
            ),
        )
        .target(target)
        .channel("power")
        .into());
    }
    parse_power_state_line(&out.stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "unexpected power-state output from '{host_name}': {}",
            out.stdout.trim()
        )
    })
}

/// The verdict in `power-state`'s output (`Power ON  (pi5)`).
fn parse_power_state_line(stdout: &str) -> Option<bool> {
    let line = stdout.lines().find(|l| l.starts_with("Power "))?;
    match line.split_whitespace().nth(1)? {
        "ON" => Some(true),
        "OFF" => Some(false),
        _ => None,
    }
}

fn usb_cmd(lab_flag: Option<&str>, cmd: UsbCmd) -> Result<()> {
    match cmd {
        UsbCmd::Set {
            target,
            cmd,
            host,
            doctor,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_usb(&target, Some(&cmd), host.as_deref())?;
                set_doctor_flag(lf, &target, "usb", &doctor)
            })?;
            println!("usb channel set for '{target}'.");
            Ok(())
        }
        UsbCmd::Rm { target } => {
            edit_lab(lab_flag, |lf| lf.remove_usb(&target))?;
            println!("usb channel removed from '{target}'.");
            Ok(())
        }
        UsbCmd::AttachHost { target } => cmd_usb_run(lab_flag, target.as_deref(), "host"),
        UsbCmd::AttachTarget { target } => cmd_usb_run(lab_flag, target.as_deref(), "target"),
        UsbCmd::State { target } => cmd_usb_run(lab_flag, target.as_deref(), "state"),
    }
}

/// Run the target's usb helper as `<cmd> usb <verb>`; a failure is
/// `helper_failed` with the helper's code in `child_exit`. paniolo stays agnostic to the helper, like the power hooks — but the
/// vocabulary is fixed rather than passed through, so a constrained remote
/// host only ever sees the three verbs.
///
/// A successful `host`/`target` means the *mux* moved, which the helper
/// confirms by reading the resulting position back. It does **not** mean the
/// media has finished enumerating on the receiving side: switching physically
/// detaches and reattaches a USB device, and the block device can take a few
/// seconds to appear. Wait for the device, not for this command.
fn cmd_usb_run(lab_flag: Option<&str>, target: Option<&str>, verb: &str) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Usb,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let t = lab
        .targets
        .get(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let dh = t.default_host().to_string();
    let u = t.usb.clone().ok_or_else(|| {
        error::channel_missing(
            &target,
            "usb",
            format!("target '{target}' has no usb channel (paniolo usb set -t {target} --cmd ...)"),
        )
    })?;
    if !channel_is_local(&lab, u.host.as_deref(), &dh) {
        return Err(error::channel_missing(
            &target,
            "usb",
            format!("usb channel for '{target}' is not on this host"),
        )
        .into());
    }
    let cmd = u.cmd.ok_or_else(|| {
        error::channel_missing(
            &target,
            "usb",
            format!(
                "no usb cmd configured for '{target}' \
             (paniolo usb set -t {target} --cmd 'ch9329 -d /dev/...')"
            ),
        )
    })?;
    // Deliberately the *hid* daemon's runtime dir, not a usb one. On the
    // devices this drives, the mux and the HID injector are the same MCU behind
    // the same serial port, so when `paniolo console` has a hid daemon running
    // it owns that port. The helper discovers the daemon through this directory
    // and routes the switch through it; without it, every switch attempted
    // while the console is up would fail on a busy port.
    let status = platform::shell_command(&format!("{cmd} usb {verb}"))
        .env("PATH", daemons::hook_path())
        .envs(daemons::helper_env(HID_DAEMON, Some(&target)))
        .status()?;
    if !status.success() {
        return Err(error::hook_failed(
            status,
            format!("usb helper exited with code {}", error::shell_code(status)),
        )
        .target(&target)
        .channel("usb")
        .into());
    }
    Ok(())
}

/// Prefix a daemon command with `exec` where the shell supports it.
///
/// On Unix `exec` makes the daemon replace the shell, so the pid paniolo
/// records is the daemon's own. `cmd.exe` has no `exec`, so on Windows the
/// shell stays as a parent and the recorded pid is the shell's — which still
/// answers `platform::pid_alive` correctly, because that shell lives exactly
/// as long as the daemon it is waiting on.
fn exec_prefixed(cmd: &str) -> String {
    if cfg!(unix) {
        format!("exec {cmd}")
    } else {
        cmd.to_string()
    }
}

/// Ensure the hid injection daemon is running locally for `target`, returning
/// its port, or None when the target has no local hid channel. The helper's
/// `cmd` is run as `<cmd> serve --port 0` via `sh -c`; the contract is that it
/// publishes `<runtime-base>/hid/<target>/daemon.json` (its
/// per-target `PANIOLO_RUNTIME_DIR`).
/// It may run in the foreground (the bundled helpers do) or fork and exit 0;
/// either way readiness is the discovery file, not its exit.
fn ensure_hid_daemon_local(lab: &Lab, target: &str) -> Result<Option<daemons::Endpoint>> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let dh = t.default_host().to_string();
    let h = match &t.hid {
        Some(h) => h,
        None => return Ok(None),
    };
    if !channel_is_local(lab, h.host.as_deref(), &dh) {
        return Ok(None);
    }
    if let Some(ep) = daemons::daemon_endpoint(HID_DAEMON, Some(target)) {
        return Ok(Some(ep));
    }
    let cmd = h.cmd.clone().ok_or_else(|| {
        error::channel_missing(
            target,
            "hid",
            format!(
                "hid channel for '{target}' has no cmd (paniolo hid set -t {target} --cmd ...)"
            ),
        )
    })?;
    eprintln!("Starting hid daemon for '{target}'…");
    let log = daemons::create_log(HID_DAEMON, Some(target))?;
    let port = model::fixed_port(h.port);
    let mut command =
        platform::shell_command(&exec_prefixed(&format!("{cmd} serve --port {port}")));
    command
        .env("PATH", daemons::hook_path())
        // The hid daemon's discovery dir is the channel name ("hid"), not
        // the helper binary's name — pass it explicitly, namespaced by target.
        .envs(daemons::helper_env(HID_DAEMON, Some(target)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    platform::detach(&mut command);
    let mut child = command.spawn()?;
    Ok(Some(
        daemons::wait_for_daemon(HID_DAEMON, Some(target), std::time::Duration::from_secs(5))
            .and_then(|_| daemons::daemon_endpoint(HID_DAEMON, Some(target)))
            .ok_or_else(|| {
                daemons::start_failure(
                    HID_DAEMON,
                    Some(target),
                    std::time::Duration::from_secs(5),
                    Some(&mut child),
                )
            })?,
    ))
}

/// Before starting hdmicap for `device`, bring up whatever feeds it and
/// whatever it types through.
///
/// Returns the target's hid daemon discovery file when the target has a hid
/// channel on this host (its daemon is started if it is not running), which
/// hdmicap's noVNC view uses for keyboard and mouse; `None` leaves that view
/// watch-only. For `rfb+hid:` the same daemon is also the video source, so a
/// missing one is an error; for every other device a hid daemon that will not
/// start only costs the view its input, and says so on stderr.
fn ensure_video_source(
    lab: &Lab,
    target: &str,
    device: &str,
) -> Result<Option<std::path::PathBuf>> {
    let hid = match ensure_hid_daemon_local(lab, target) {
        Ok(hid) => hid,
        Err(e) if device != model::RFB_HID_DEVICE => {
            eprintln!("note: no keyboard/mouse in the browser view of '{target}': {e:#}");
            None
        }
        Err(e) => return Err(e),
    };
    match hid {
        Some(_) => Ok(Some(video::hid_discovery_file(target)?)),
        None if device == model::RFB_HID_DEVICE => Err(error::channel_missing(
            target,
            "hid",
            format!(
                "video device '{device}' reads the hid daemon's RFB feed, but '{target}' has \
                 no hid channel on this host"
            ),
        )
        .into()),
        None => Ok(None),
    }
}

fn cmd_hid_serve(lab_flag: Option<&str>, target: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Hid,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    match ensure_hid_daemon_local(&lab, &target)? {
        Some(ep) => {
            println!("hid daemon running for '{target}' (port {}).", ep.port);
            Ok(())
        }
        None => Err(error::channel_missing(
            &target,
            "hid",
            format!("target '{target}' has no hid channel on this host"),
        )
        .into()),
    }
}

fn cmd_hid_stop(lab_flag: Option<&str>, target: Option<&str>) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Hid,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let t = lab
        .targets
        .get(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let cmd = t.hid.as_ref().and_then(|h| h.cmd.clone()).ok_or_else(|| {
        error::channel_missing(
            &target,
            "hid",
            format!("target '{target}' has no hid channel"),
        )
    })?;
    // The helper owns its own stop (e.g. `hidrig stop`); strip any trailing
    // device args isn't needed — `<cmd> stop` ignores extra args it doesn't use.
    let status = platform::shell_command(&format!("{cmd} stop"))
        .env("PATH", daemons::hook_path())
        .envs(daemons::helper_env(HID_DAEMON, Some(&target)))
        .status()?;
    if !status.success() {
        return Err(error::hook_failed(
            status,
            format!(
                "hid helper stop exited with code {}",
                error::shell_code(status)
            ),
        )
        .target(&target)
        .channel("hid")
        .into());
    }
    Ok(())
}

/// Run the target's hid helper with `args` appended; a failure is
/// `helper_failed` with the helper's code in `child_exit`.
/// Paniolo is agnostic to the helper's CLI — the configured cmd owns it (see
/// docs/hid.md), exactly like the power hooks.
fn cmd_hid_send(lab_flag: Option<&str>, target: Option<&str>, args: &[String]) -> Result<()> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) = dispatch::maybe_dispatch(
        &lab,
        &target,
        model::ChannelKind::Hid,
        None,
        dispatch::Mode::Reexec,
    )? {
        std::process::exit(code);
    }
    let t = lab
        .targets
        .get(&target)
        .ok_or_else(|| error::target_not_found(&target))?;
    let dh = t.default_host().to_string();
    let h = t.hid.clone().ok_or_else(|| {
        error::channel_missing(
            &target,
            "hid",
            format!("target '{target}' has no hid channel (paniolo hid set -t {target} --cmd ...)"),
        )
    })?;
    if !channel_is_local(&lab, h.host.as_deref(), &dh) {
        return Err(error::channel_missing(
            &target,
            "hid",
            format!("hid channel for '{target}' is not on this host"),
        )
        .into());
    }
    let cmd = h.cmd.ok_or_else(|| {
        error::channel_missing(
            &target,
            "hid",
            format!(
                "no hid cmd configured for '{target}' \
             (paniolo hid set -t {target} --cmd 'hidrig -d /dev/...')"
            ),
        )
    })?;
    let quoted: Vec<String> = args.iter().map(|a| ssh::shell_quote(a)).collect();
    let full = format!("{cmd} {}", quoted.join(" "));
    let status = platform::shell_command(&full)
        .env("PATH", daemons::hook_path())
        .envs(daemons::helper_env(HID_DAEMON, Some(&target)))
        .status()?;
    if !status.success() {
        return Err(error::hook_failed(
            status,
            format!("hid helper exited with code {}", error::shell_code(status)),
        )
        .target(&target)
        .channel("hid")
        .into());
    }
    Ok(())
}

// ── adb runtime bodies ──────────────────────────────────────────────────────

/// The target's adb channel as visible on *this* host.
fn local_adb(lab: &Lab, target: &str) -> Result<model::AdbChannel> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let dh = t.default_host().to_string();
    let a = t.adb.clone().ok_or_else(|| {
        error::channel_missing(
            target,
            "adb",
            format!(
                "target '{target}' has no adb channel (paniolo adb set -t {target} --serial <id>)"
            ),
        )
    })?;
    if !channel_is_local(lab, a.host.as_deref(), &dh) {
        return Err(error::channel_missing(
            target,
            "adb",
            format!("adb channel for '{target}' is not on this host"),
        )
        .into());
    }
    Ok(a)
}

/// Common preamble for adb runtime commands: resolve the target, dispatch to
/// the adb channel's host if remote, and return the local channel. `mode`
/// selects the SSH transport when remote (`Interactive` for `adb shell`).
fn adb_runtime(
    lab_flag: Option<&str>,
    target: Option<&str>,
    mode: dispatch::Mode,
) -> Result<model::AdbChannel> {
    let lab = load_for_read(lab_flag)?;
    let target = resolve_single_target(&lab, target)?;
    if let Some(code) =
        dispatch::maybe_dispatch(&lab, &target, model::ChannelKind::Adb, None, mode)?
    {
        std::process::exit(code);
    }
    local_adb(&lab, &target)
}

fn adb_cmd(lab_flag: Option<&str>, cmd: AdbCmd) -> Result<()> {
    match cmd {
        AdbCmd::Set {
            target,
            serial,
            adb,
            host,
        } => {
            edit_lab(lab_flag, |lf| {
                lf.set_adb(&target, serial.as_deref(), adb.as_deref(), host.as_deref())
            })?;
            println!("adb channel set for '{target}'.");
            Ok(())
        }
        AdbCmd::Rm { target } => {
            edit_lab(lab_flag, |lf| lf.remove_adb(&target))?;
            println!("adb channel removed from '{target}'.");
            Ok(())
        }
        AdbCmd::Show { target } => cmd_adb_show(lab_flag, target.name()),
        AdbCmd::Shell { target } => {
            let a = adb_runtime(lab_flag, target.name(), dispatch::Mode::Interactive)?;
            adb::exec_shell(a.adb.as_deref(), a.serial.as_deref())
        }
        AdbCmd::Run { target, args } => {
            let a = adb_runtime(lab_flag, target.as_deref(), dispatch::Mode::Reexec)?;
            let mut rest = vec!["shell".to_string()];
            rest.extend(args);
            std::process::exit(adb::run_passthrough(
                a.adb.as_deref(),
                a.serial.as_deref(),
                &rest,
            )?);
        }
        AdbCmd::Screencap { target, out } => {
            // Remote channel + a real --out path: `--out` must mean the
            // invoking machine's filesystem, so run the remote screencap with
            // `--out -` and stream the PNG into the local file instead of
            // re-execing verbatim (which would write it on the control
            // host) — mirrors `video shot` above.
            if out != "-" {
                let lab = load_for_read(lab_flag)?;
                let target_name = resolve_single_target(&lab, target.name())?;
                let rt = lab
                    .resolved_target(&target_name)
                    .ok_or_else(|| error::target_not_found(&target_name))?;
                let host_name = model::channel_host(&rt, model::ChannelKind::Adb, None)?;
                if !lab.host(&host_name).is_local(&host_name) {
                    let sub = vec![
                        "adb".to_string(),
                        "screencap".to_string(),
                        target_name.clone(),
                        "--out".to_string(),
                        "-".to_string(),
                    ];
                    let code = dispatch::dispatch_stdout_to_file(
                        &lab,
                        &target_name,
                        &host_name,
                        &sub,
                        &out,
                    )?;
                    if code == 0 {
                        eprintln!("saved to {out}");
                    }
                    std::process::exit(code);
                }
            }
            let a = adb_runtime(lab_flag, target.name(), dispatch::Mode::Reexec)?;
            adb::screencap(a.adb.as_deref(), a.serial.as_deref(), &out)
        }
        AdbCmd::Input { target, args } => {
            let a = adb_runtime(lab_flag, target.as_deref(), dispatch::Mode::Reexec)?;
            let mut rest = vec!["shell".to_string(), "input".to_string()];
            rest.extend(args);
            std::process::exit(adb::run_passthrough(
                a.adb.as_deref(),
                a.serial.as_deref(),
                &rest,
            )?);
        }
        AdbCmd::Devices { host } => cmd_adb_devices(lab_flag, host.as_deref()),
    }
}

fn cmd_adb_show(lab_flag: Option<&str>, target: Option<&str>) -> Result<()> {
    let a = adb_runtime(lab_flag, target, dispatch::Mode::Reexec)?;
    println!("serial\t{}", a.serial.as_deref().unwrap_or("(sole device)"));
    println!("adb\t{}", a.adb.as_deref().unwrap_or(adb::DEFAULT_ADB));
    // Best-effort device state — don't fail `show` when the device is offline.
    let av = adb::argv(
        a.adb.as_deref(),
        a.serial.as_deref(),
        &["get-state".to_string()],
    );
    match std::process::Command::new(&av[0]).args(&av[1..]).output() {
        Ok(o) if o.status.success() => {
            println!("state\t{}", String::from_utf8_lossy(&o.stdout).trim());
        }
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.trim();
            println!(
                "state\t{}",
                if msg.is_empty() { "unreachable" } else { msg }
            );
        }
        Err(_) => println!("state\t(adb not installed)"),
    }
    Ok(())
}

/// `adb devices -l` on a host (local by default) — discovery for lab authoring,
/// so it doesn't require a configured adb channel.
fn cmd_adb_devices(lab_flag: Option<&str>, host: Option<&str>) -> Result<()> {
    let rest = vec!["devices".to_string(), "-l".to_string()];
    let host = host.unwrap_or(model::LOCAL);
    let resolved = resolve_host(lab_flag, host)?;
    if resolved.is_local(host) {
        std::process::exit(adb::run_passthrough(None, None, &rest)?);
    }
    let mut argv = vec![adb::DEFAULT_ADB.to_string()];
    argv.extend(rest);
    let code = ssh::run_passthrough(&resolved, &argv, &[])?;
    if code == error::SSH_TRANSPORT_FAILURE {
        return Err(error::unreachable_host(host, error::ssh_255_message(host)).into());
    }
    std::process::exit(code);
}

// ── rendering helpers ───────────────────────────────────────────────────────

fn normalize_sense(sense: Option<&str>) -> Result<Option<String>> {
    match sense {
        None => Ok(None),
        Some(s) if s.eq_ignore_ascii_case("none") => Ok(None),
        Some(s) => {
            let v = s.to_ascii_lowercase();
            if !model::VALID_SENSE_SIGNALS.contains(&v.as_str()) {
                bail!(
                    "Unknown sense signal '{s}'. Valid: {}, none",
                    model::VALID_SENSE_SIGNALS.join(", ")
                );
            }
            Ok(Some(v))
        }
    }
}

fn fields_str(fields: &[(&'static str, String)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn channel_name(ch: &ResolvedChannel) -> String {
    if ch.name == ch.kind.as_str() {
        ch.kind.as_str().to_string()
    } else {
        format!("{} {}", ch.kind.as_str(), ch.name)
    }
}

fn channel_label(ch: &ResolvedChannel) -> String {
    let fields = fields_str(&ch.fields);
    let tail = if fields.is_empty() {
        String::new()
    } else {
        format!("  {fields}")
    };
    format!("{} @{}{tail}", channel_name(ch), ch.host)
}

fn target_headline(rt: &ResolvedTarget) -> String {
    let hosts = rt.hosts();
    if hosts.len() > 1 {
        format!("{}  (spans {})", rt.name, hosts.join(", "))
    } else if let Some(h) = hosts.first() {
        format!("{}  @{h}", rt.name)
    } else {
        rt.name.clone()
    }
}

fn print_resolved_target(rt: &ResolvedTarget) {
    println!("Target: {}", target_headline(rt));
    println!("  default host  {}", rt.default_host);
    if let Some(description) = &rt.description {
        println!("  description   {description}");
    }
    if rt.channels.is_empty() {
        println!("  channels      (none)");
    } else {
        for ch in &rt.channels {
            println!("  channel       {}", channel_label(ch));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `power-cycle` waits for power to come back (#259): the reader is
    /// called until it says on, and not again after.
    #[test]
    fn wait_for_power_on_polls_until_on() {
        let mut calls = 0;
        let on = wait_for_power_on(
            || {
                calls += 1;
                Ok(calls >= 3)
            },
            std::time::Duration::from_millis(1),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(on);
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_for_power_on_gives_up_at_the_deadline() {
        let start = std::time::Instant::now();
        let on = wait_for_power_on(
            || Ok(false),
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(50),
        )
        .unwrap();
        assert!(!on);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    /// A state_cmd that never reports on is `timeout` (exit 22): the cycle
    /// may still finish, so the outcome is unknown.
    #[cfg(unix)]
    #[test]
    fn confirm_power_on_times_out_as_timeout() {
        let err = confirm_power_on(
            "echo off",
            "nuc",
            None,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(50),
        )
        .unwrap_err();
        let pe = error::classify(&err);
        assert_eq!(pe.kind, error::Kind::Timeout, "{err:#}");
        assert_eq!(pe.kind.exit_code(), 22);
    }

    /// A lab file for the power-level tests (#302): `t` has default hooks and
    /// a `hard` level with only some of its own.
    fn power_levels_lab(dir: &std::path::Path, extra: &str) -> String {
        let path = dir.join("lab.toml");
        std::fs::write(
            &path,
            format!(
                "[targets.t.power]\ncycle_cmd = \"true\"\non_cmd = \"true\"\n\
                 off_cmd = \"true\"\nstate_cmd = \"echo on\"\n\
                 [targets.t.power.levels.hard]\n{extra}"
            ),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Each verb resolves `--level`: an unknown level and a level without the
    /// verb's hook are `not_configured` naming the level, never a fallback to
    /// the default level's hook. Fails before any hook runs.
    #[test]
    fn power_verbs_refuse_a_level_without_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let lab = power_levels_lab(dir.path(), "cycle_cmd = \"true\"\n");
        let lab = Some(lab.as_str());
        let not_configured = |r: Result<()>, want: &[&str]| {
            let e = r.unwrap_err();
            let pe = error::classify(&e);
            assert_eq!(pe.kind, error::Kind::NotConfigured, "{e:#}");
            for w in want {
                assert!(format!("{e:#}").contains(w), "{e:#} lacks {w}");
            }
        };
        not_configured(
            cmd_power_on(lab, Some("t"), Some("hard")),
            &[
                "no on_cmd configured for 't' power level 'hard'",
                "--level hard --on-cmd",
            ],
        );
        not_configured(
            cmd_power_off(lab, Some("t"), Some("hard")),
            &["no off_cmd configured for 't' power level 'hard'"],
        );
        not_configured(
            cmd_power_state(lab, Some("t"), Some("hard")),
            &["no state_cmd configured for 't' power level 'hard'"],
        );
        not_configured(
            cmd_power_cycle(lab, Some("t"), Some("soft")),
            &["has no power level 'soft' (have: hard"],
        );
        // The default level's message is unchanged.
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("lab.toml");
        std::fs::write(&path, "[targets.t.power]\nstate_cmd = \"echo on\"\n").unwrap();
        not_configured(
            cmd_power_on(path.to_str(), Some("t"), None),
            &["no on_cmd configured for 't' (paniolo power set -t t --on-cmd /path/to/script)"],
        );
    }

    /// The level's own hooks run (under the per-target hook lock, in a
    /// private runtime base), `--level default` is the default level, and a
    /// level's cycle confirms with its own state_cmd.
    #[cfg(unix)]
    #[test]
    fn power_verbs_run_the_chosen_levels_hooks() {
        let _guard = daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PANIOLO_RUNTIME_BASE");
        // Safe: serialized by RUNTIME_BASE_LOCK; restored below.
        unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", base.path()) };
        let dir = tempfile::tempdir().unwrap();
        let mark = |n: &str| dir.path().join(n);
        let lab = power_levels_lab(
            dir.path(),
            &format!(
                "on_cmd = \"touch {}\"\ncycle_cmd = \"touch {}\"\nstate_cmd = \"echo on\"\n",
                mark("hard-on").display(),
                mark("hard-cycle").display()
            ),
        );
        // Point the default on_cmd at its own marker.
        let text = std::fs::read_to_string(&lab).unwrap().replacen(
            "on_cmd = \"true\"",
            &format!("on_cmd = \"touch {}\"", mark("default-on").display()),
            1,
        );
        std::fs::write(&lab, text).unwrap();
        let lab = Some(lab.as_str());
        let on_hard = cmd_power_on(lab, Some("t"), Some("hard"));
        let hard_on = mark("hard-on").exists();
        let default_on_after_hard = mark("default-on").exists();
        let on_default = cmd_power_on(lab, Some("t"), Some("default"));
        let cycle_hard = cmd_power_cycle(lab, Some("t"), Some("hard"));
        let state_hard = cmd_power_state(lab, Some("t"), Some("hard"));
        match prev {
            Some(p) => unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", p) },
            None => unsafe { std::env::remove_var("PANIOLO_RUNTIME_BASE") },
        }
        on_hard.unwrap();
        assert!(hard_on);
        assert!(!default_on_after_hard, "the default on_cmd must not run");
        on_default.unwrap();
        assert!(mark("default-on").exists());
        cycle_hard.unwrap();
        assert!(mark("hard-cycle").exists());
        state_hard.unwrap();
    }

    /// `power-state` with levels: one line per level, default first, the
    /// verdict always the second word; an unreadable level is reported, not
    /// an error; a failed read fails the command after every line; nothing
    /// readable at all is the default level's own error.
    #[cfg(unix)]
    #[test]
    fn power_state_reports_every_level() {
        assert_eq!(power_state_line("pi5", None, Ok(true)), "Power ON  (pi5)");
        assert_eq!(
            power_state_line("pi5", Some("hard"), Ok(false)),
            "Power OFF  (pi5, level hard)"
        );
        assert_eq!(
            power_state_line("pi5", Some("pmic"), Err(LevelState::Unconfigured)),
            "Power UNKNOWN  (pi5, level pmic: no state_cmd)"
        );
        assert_eq!(
            parse_power_state_line("Power ON  (pi5)\nPower OFF  (pi5, level hard)\n"),
            Some(true)
        );

        let _guard = daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PANIOLO_RUNTIME_BASE");
        // Safe: serialized by RUNTIME_BASE_LOCK; restored below.
        unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", base.path()) };
        let parse = |s: &str| model::parse(s).unwrap();
        let ok = power_state_all_levels(
            &parse(
                "[targets.t.power]\nstate_cmd = \"echo on\"\n\
                 [targets.t.power.levels.hard]\nstate_cmd = \"echo off\"\n\
                 [targets.t.power.levels.pmic]\non_cmd = \"true\"\n",
            ),
            "t",
        );
        let failed = power_state_all_levels(
            &parse(
                "[targets.t.power]\nstate_cmd = \"echo on\"\n\
                 [targets.t.power.levels.hard]\nstate_cmd = \"exit 3\"\n",
            ),
            "t",
        );
        let nothing = power_state_all_levels(
            &parse("[targets.t.power]\n[targets.t.power.levels.hard]\non_cmd = \"true\"\n"),
            "t",
        );
        match prev {
            Some(p) => unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", p) },
            None => unsafe { std::env::remove_var("PANIOLO_RUNTIME_BASE") },
        }
        ok.unwrap();
        let e = failed.unwrap_err();
        assert_eq!(error::classify(&e).kind, error::Kind::HelperFailed, "{e:#}");
        let e = nothing.unwrap_err();
        assert_eq!(
            error::classify(&e).kind,
            error::Kind::NotConfigured,
            "{e:#}"
        );
        assert!(
            format!("{e:#}").contains("no power serial_interface"),
            "{e:#}"
        );
    }

    /// `--version` is the stamped release version when the build had one,
    /// and never the bare manifest placeholder when it did not.
    #[test]
    fn version_is_the_stamp_or_says_it_is_a_dev_build() {
        use clap::CommandFactory;
        let rendered = Cli::command().render_version();
        assert!(rendered.starts_with("paniolo "), "{rendered}");
        match option_env!("PANIOLO_VERSION") {
            Some(v) => assert_eq!(rendered.trim(), format!("paniolo {v}")),
            None => assert_eq!(
                rendered.trim(),
                format!(
                    "paniolo {} (unversioned dev build)",
                    env!("CARGO_PKG_VERSION")
                )
            ),
        }
    }

    /// `main()`'s top-level `eprintln!("{e:#}")` (anyhow's *alternate* Display)
    /// prints the whole `.context()` chain; the old plain `{e}` printed only
    /// the outermost message and silently dropped every cause a command added
    /// on the way up (Review low #7). `main()` itself can't be unit-tested
    /// (it calls `std::process::exit`), so this pins the exact formatting
    /// distinction it now relies on.
    #[test]
    fn error_alternate_display_includes_the_full_context_chain() {
        use anyhow::Context;
        let err = Err::<(), _>(anyhow!("root cause"))
            .context("middle step")
            .context("outer command")
            .unwrap_err();
        let short = format!("{err}");
        let full = format!("{err:#}");
        assert_eq!(short, "outer command", "plain Display is outermost only");
        for part in ["outer command", "middle step", "root cause"] {
            assert!(full.contains(part), "{part:?} missing from {full:?}");
        }
        assert_ne!(full, short, "{{e:#}} must show more than {{e}}");
    }

    fn endpoint(port: u16, token: Option<&str>) -> daemons::Endpoint {
        daemons::Endpoint {
            pid: 1,
            port,
            token: token.map(str::to_string),
        }
    }

    /// What `console` prints names the port and never a token, while the URL
    /// it hands the browser for the same endpoint still carries one (#196).
    #[test]
    fn console_banner_names_the_port_and_never_the_token() {
        let video = endpoint(1000, Some("s3cr3t"));
        let banner = console_banner(&video, true);
        assert!(banner.contains("http://127.0.0.1:1000"), "{banner}");
        assert!(!banner.contains("token="), "{banner}");
        assert!(!banner.contains("s3cr3t"), "{banner}");
        let url = dashboard_url(
            &video,
            &DashboardLinks {
                serial_ws: None,
                interface: None,
                hid_ws: None,
                video_only: false,
            },
        );
        assert_eq!(url, "http://127.0.0.1:1000/?token=s3cr3t");
    }

    /// Against a daemon too old to have a token, and with no links, the
    /// dashboard URL *is* the base address — so the banner must not claim a
    /// credential was withheld and send the reader looking for another URL.
    #[test]
    fn console_banner_claims_nothing_withheld_when_there_is_no_token() {
        let video = endpoint(1000, None);
        let url = dashboard_url(
            &video,
            &DashboardLinks {
                serial_ws: None,
                interface: None,
                hid_ws: None,
                video_only: false,
            },
        );
        assert_eq!(url, "http://127.0.0.1:1000", "nothing to withhold");
        let banner = console_banner(&video, url != video.base_url());
        assert!(banner.contains("http://127.0.0.1:1000"), "{banner}");
        assert!(
            !banner.contains("not printed"),
            "must not claim a withheld URL that does not exist: {banner}"
        );
    }

    /// When no browser could be launched the URL still has to be reachable —
    /// an address the reader cannot open would strand them, and over SSH the
    /// tunnels die with the command. It goes to a 0600 file rather than to
    /// stdout, so `console` prints no token under any circumstance.
    /// Any one of the variables sshd sets marks an SSH session; an empty value
    /// (a shell that cleared it with `SSH_TTY=`) does not.
    #[test]
    fn ssh_session_is_detected_from_sshd_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(!in_ssh_session(env(&[])));
        assert!(in_ssh_session(env(&[("SSH_CONNECTION", "a 1 b 22")])));
        assert!(in_ssh_session(env(&[("SSH_CLIENT", "a 1 22")])));
        assert!(in_ssh_session(env(&[("SSH_TTY", "/dev/pts/3")])));
        assert!(!in_ssh_session(env(&[("SSH_TTY", "")])));
        assert!(!in_ssh_session(env(&[("DISPLAY", ":0")])));
    }

    /// Every port the dashboard URL names is forwarded to the same number, so
    /// the URL works unchanged on the far end; no token appears in the hint.
    #[test]
    fn ssh_forward_hint_forwards_every_port_to_the_same_number() {
        let hint = ssh_forward_hint("bench1", &[1000, 2000, 3000]);
        assert!(
            hint.contains(
                "ssh -N -L 1000:127.0.0.1:1000 -L 2000:127.0.0.1:2000 \
                 -L 3000:127.0.0.1:3000 bench1"
            ),
            "{hint}"
        );
        assert!(!hint.contains("token"), "{hint}");
    }

    /// Over SSH without --print-url the URL goes to the 0600 file, and the
    /// message says how to fetch it across the connection — never the token.
    #[test]
    fn console_over_ssh_points_at_the_file_not_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = std::env::var_os("PANIOLO_RUNTIME_BASE");
        // Safe: serialized by the lock above; restored below.
        unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", dir.path()) };
        let url = "http://127.0.0.1:1000/?token=s3cr3t";
        let msg = console_url_over_ssh("dut", "bench1", url);
        match prev {
            Some(v) => unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", v) },
            None => unsafe { std::env::remove_var("PANIOLO_RUNTIME_BASE") },
        }
        assert!(!msg.contains("s3cr3t"), "{msg}");
        assert!(msg.contains("ssh bench1 cat "), "{msg}");
        assert!(msg.contains("--print-url"), "{msg}");
    }

    #[test]
    fn console_falls_back_to_a_private_file_not_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        // PANIOLO_RUNTIME_BASE is process-global, not test-local: restoring it
        // before this test returns does nothing for the daemons tests running
        // in parallel threads, which read it between planting a discovery file
        // and listing it. Hold the crate-wide lock they hold.
        let _guard = crate::daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = std::env::var_os("PANIOLO_RUNTIME_BASE");
        // Safe: serialized by the lock above; restored below.
        unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", dir.path()) };

        let video = endpoint(1000, Some("s3cr3t"));
        let url = dashboard_url(
            &video,
            &DashboardLinks {
                serial_ws: Some("ws://127.0.0.1:2000/stream?token=ser"),
                interface: None,
                hid_ws: None,
                video_only: false,
            },
        );
        let msg = console_fallback("dut", &url);

        match prev {
            Some(v) => unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", v) },
            None => unsafe { std::env::remove_var("PANIOLO_RUNTIME_BASE") },
        }

        assert!(
            !msg.contains("s3cr3t") && !msg.contains("token="),
            "the token must not reach the terminal: {msg}"
        );
        assert!(
            msg.contains("dashboard-url.txt"),
            "and the reader must be told where it is: {msg}"
        );
        let path = msg
            .split_whitespace()
            .find(|w| w.ends_with("dashboard-url.txt"))
            .expect("the message names the file");
        let body = std::fs::read_to_string(path).expect("the file was written");
        assert_eq!(body.trim(), url);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the URL file must be private");
        }
    }

    /// The `video show` daemon line — the command the issue names, and the one
    /// an agent runs constantly. Rendered by its own function so this executes
    /// the real thing; before, a one-word revert put the token back with the
    /// whole suite still green.
    #[test]
    fn video_show_daemon_line_never_carries_the_token() {
        let ep = endpoint(1000, Some("s3cr3t"));
        let line = video_show_daemon_line(&ep.base_url(), "");
        assert_eq!(line, "daemon\trunning at http://127.0.0.1:1000");
        assert!(!line.contains("s3cr3t"), "{line}");
        assert!(!line.contains("token="), "{line}");
        // The stale marker still rides along, and still carries no token.
        let stale = video_show_daemon_line(&ep.base_url(), " (stale)");
        assert!(stale.ends_with(" (stale)"), "{stale}");
        assert!(!stale.contains("s3cr3t"), "{stale}");
        // The openable form, for contrast: same endpoint, token attached.
        assert_eq!(ep.http_url("/"), "http://127.0.0.1:1000/?token=s3cr3t");
    }

    /// `paniolo helper` (#300): placement flags are paniolo's only before the
    /// helper name. Everything after it is the helper's, so a helper's own
    /// `-t`, `-i` or `--help` is never taken by paniolo, with or without `--`.
    #[test]
    fn helper_flags_after_the_name_belong_to_the_helper() {
        let helper = |argv: &[&str]| {
            let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.command {
                Command::Helper {
                    host,
                    target,
                    channel,
                    interface,
                    command,
                } => (host, target, channel, interface, command),
                _ => panic!("{argv:?}: parsed as a different command"),
            }
        };
        let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();

        let (host, target, channel, _, command) = helper(&[
            "paniolo",
            "helper",
            "-t",
            "pi5",
            "--channel",
            "power",
            "--",
            "cambrionix",
            "-d",
            "/dev/ttyUSB0",
            "state",
        ]);
        assert_eq!(
            (host, target.as_deref(), channel.as_deref()),
            (None, Some("pi5"), Some("power"))
        );
        assert_eq!(command, s(&["cambrionix", "-d", "/dev/ttyUSB0", "state"]));

        let (host, target, _, interface, command) = helper(&[
            "paniolo",
            "helper",
            "serialcap",
            "log",
            "-i",
            "console",
            "-t",
            "x",
        ]);
        assert_eq!((host, target, interface), (None, None, None));
        assert_eq!(
            command,
            s(&["serialcap", "log", "-i", "console", "-t", "x"])
        );

        let (.., command) = helper(&["paniolo", "helper", "cambrionix", "--help"]);
        assert_eq!(command, s(&["cambrionix", "--help"]));

        let (host, .., command) =
            helper(&["paniolo", "helper", "--host", "bench1", "zigplug", "list"]);
        assert_eq!(host.as_deref(), Some("bench1"));
        assert_eq!(command, s(&["zigplug", "list"]));

        assert!(helper(&["paniolo", "helper"]).4.is_empty());
        // --channel needs -t; --host and -t are two answers to one question.
        assert!(Cli::try_parse_from(["paniolo", "helper", "--channel", "power"]).is_err());
        assert!(Cli::try_parse_from(["paniolo", "helper", "--host", "a", "-t", "b"]).is_err());
    }

    /// The re-exec'd `helper` puts `--` before the name, so the remote parser
    /// cannot take the helper's flags, even if it is an older paniolo whose
    /// `helper` still parsed flags after the name.
    #[test]
    fn helper_remote_argv_separates_the_helper() {
        let args = vec!["--help".to_string()];
        assert_eq!(
            helper_remote_argv(Some("serialcap"), &args),
            vec!["helper", "--", "serialcap", "--help"]
        );
        assert_eq!(helper_remote_argv(None, &[]), vec!["helper"]);
    }

    #[test]
    fn power_state_line_parses_both_verdicts() {
        assert_eq!(parse_power_state_line("Power ON  (pi5)\n"), Some(true));
        assert_eq!(
            parse_power_state_line("note\nPower OFF  (pi5)\n"),
            Some(false)
        );
        assert_eq!(parse_power_state_line("Power maybe\n"), None);
        assert_eq!(parse_power_state_line(""), None);
    }

    /// `video preview --open` is the form that keeps the token off the
    /// terminal; without it the flag is false and the URL is printed. The
    /// target still takes either spelling (#203).
    #[test]
    fn video_preview_parses_the_open_flag() {
        let preview = |argv: &[&str]| {
            let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.command {
                Command::Video {
                    cmd: VideoCmd::Preview { target, open },
                } => (target.name().map(str::to_string), open),
                _ => panic!("{argv:?}: parsed as a different command"),
            }
        };
        assert_eq!(
            preview(&["paniolo", "video", "preview", "--open"]),
            (None, true)
        );
        assert_eq!(
            preview(&["paniolo", "video", "preview", "dut"]),
            (Some("dut".to_string()), false)
        );
        assert_eq!(
            preview(&["paniolo", "video", "preview", "-t", "dut", "--open"]),
            (Some("dut".to_string()), true)
        );
    }

    /// `video preview --open` must be refused when the target's video channel
    /// is on another host, and allowed when it is here (#196).
    ///
    /// A remote channel re-execs the whole argv on the control host, `--open`
    /// included, so without this gate the browser opens on the bench machine —
    /// silently, while the caller is told it opened. That is the shape of
    /// failure nobody reports: nothing errors, a window just never appears.
    ///
    /// Parsing the flag was pinned; acting on it was not, and the refusal is
    /// one `if` away from being deleted with the suite still green. This drives
    /// `refuse_remote_open` itself, through a real lab file on disk, so the
    /// host resolution it does is executed rather than described.
    #[test]
    fn video_preview_open_is_refused_when_the_channel_is_on_another_host() {
        let dir = tempfile::tempdir().unwrap();

        let remote = dir.path().join("remote.toml");
        std::fs::write(
            &remote,
            "[hosts.bench]\n\
             ssh = \"u@bench\"\n\
             [targets.dut]\n\
             host = \"bench\"\n\
             [targets.dut.video]\n\
             device = \"/dev/video0\"\n",
        )
        .unwrap();
        let err = refuse_remote_open(remote.to_str(), Some("dut"))
            .expect_err("a video channel on 'bench' must refuse --open")
            .to_string();
        assert!(err.contains("bench"), "{err}");
        // The refusal has to name a way through, or it is just a wall: both
        // `console` (which forwards the ports) and plain `preview`.
        assert!(err.contains("paniolo console dut"), "{err}");
        assert!(err.contains("paniolo video preview dut"), "{err}");

        // The same target with its video channel here is not refused — the
        // gate must not cost a local operator the flag.
        let local = dir.path().join("local.toml");
        std::fs::write(
            &local,
            "[hosts.here]\n\
             ssh = \"local\"\n\
             [targets.dut]\n\
             host = \"here\"\n\
             [targets.dut.video]\n\
             device = \"/dev/video0\"\n",
        )
        .unwrap();
        refuse_remote_open(local.to_str(), Some("dut"))
            .expect("a local video channel must still accept --open");
    }

    /// The dashboard URL carries the video daemon's token for the page itself
    /// and each other daemon's complete WebSocket URL — that daemon's token
    /// inside, encoded as one value — so the page can authenticate to all
    /// A target with only a KVM (video + hid) is a console: it runs on the
    /// video channel's host, and the page is told there is no serial (#223).
    #[test]
    fn console_host_accepts_a_target_without_serial() {
        let lab = model::parse(
            "[hosts.bench]\nssh = \"u@bench\"\n\
             [targets.t]\n\
             [targets.t.video]\nhost = \"bench\"\ndevice = \"/dev/video0\"\n",
        )
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        assert_eq!(
            console_host(&rt, None).unwrap(),
            ("bench".to_string(), false)
        );
        // Asking for an interface it cannot have is still an error, in
        // channel_host's words.
        let e = console_host(&rt, Some("console")).unwrap_err().to_string();
        assert!(
            e.contains("no serial interface 'console' (have: none)"),
            "{e}"
        );
        let video = daemons::Endpoint {
            pid: 1,
            port: 1000,
            token: Some("vt".into()),
        };
        let url = dashboard_url(
            &video,
            &DashboardLinks {
                serial_ws: None,
                interface: None,
                hid_ws: None,
                video_only: true,
            },
        );
        assert_eq!(url, "http://127.0.0.1:1000/?token=vt&serial=none");
    }

    /// With a serial channel the rule is unchanged: serial and video must
    /// share a host, and the console runs there with its serial leg.
    #[test]
    fn console_host_still_requires_serial_and_video_on_one_host() {
        let split = model::parse(
            "[hosts.bench]\nssh = \"u@bench\"\n\
             [targets.t]\n\
             [[targets.t.serial]]\nname = \"console\"\ndevice = \"/dev/ttyUSB0\"\n\
             [targets.t.video]\nhost = \"bench\"\ndevice = \"/dev/video0\"\n",
        )
        .unwrap();
        let rt = split.resolved_target("t").unwrap();
        let e = console_host(&rt, None).unwrap_err().to_string();
        assert!(e.contains("on one host"), "{e}");

        let together = model::parse(
            "[hosts.bench]\nssh = \"u@bench\"\n\
             [targets.t]\nhost = \"bench\"\n\
             [[targets.t.serial]]\nname = \"console\"\ndevice = \"/dev/ttyUSB0\"\n\
             [targets.t.video]\ndevice = \"/dev/video0\"\n",
        )
        .unwrap();
        let rt = together.resolved_target("t").unwrap();
        assert_eq!(
            console_host(&rt, None).unwrap(),
            ("bench".to_string(), true)
        );
    }

    /// three without any of them sharing a secret.
    #[test]
    fn dashboard_url_carries_every_daemons_token() {
        let video = daemons::Endpoint {
            pid: 1,
            port: 1000,
            token: Some("vt".into()),
        };
        let serial = daemons::Endpoint {
            pid: 2,
            port: 2000,
            token: Some("st".into()),
        };
        let hid = daemons::Endpoint {
            pid: 3,
            port: 3000,
            token: None,
        };
        let serial_ws = serial.ws_url("/stream");
        let hid_ws = hid.ws_url("/hid");
        let url = dashboard_url(
            &video,
            &DashboardLinks {
                serial_ws: Some(&serial_ws),
                interface: Some("console"),
                hid_ws: Some(&hid_ws),
                video_only: false,
            },
        );
        assert_eq!(
            url,
            "http://127.0.0.1:1000/?token=vt\
             &serialws=ws%3A%2F%2F127.0.0.1%3A2000%2Fstream%3Ftoken%3Dst\
             &interface=console\
             &hidws=ws%3A%2F%2F127.0.0.1%3A3000%2Fhid"
        );
        // A tokenless (older) video daemon and no links: the bare base URL.
        let old = daemons::Endpoint {
            token: None,
            ..video
        };
        assert_eq!(
            dashboard_url(&old, &DashboardLinks::default()),
            "http://127.0.0.1:1000"
        );
    }

    // Regression: the per-channel runtime helpers must use the same
    // self-identification as the dispatch layer ([`model::Host::is_local`]).
    // A host that is local only by FQDN (not the bare `local` sentinel) was
    // rejected by an earlier raw `host != "local"` compare, which broke every
    // runtime command (power/serial/video/netboot/hid/adb) on an
    // FQDN-identified control host even though `doctor` reported it healthy.
    #[test]
    fn channel_is_local_recognizes_fqdn_host() {
        let Some(fqdn) = model::local_fqdn() else {
            return; // hostname -f unavailable; covered by model's own tests
        };
        let lab = model::parse(&format!(
            "[hosts.dev]\n\
             ssh = \"dev.ssh.alias\"\n\
             hostname = \"{fqdn}\"\n\
             [targets.t]\n\
             host = \"dev\"\n\
             [targets.t.video]\n\
             device = \"/dev/video0\"\n"
        ))
        .unwrap();
        // The video channel inherits the target's default host "dev", which is
        // local by FQDN even though its ssh field is an alias, not "local".
        assert!(channel_is_local(&lab, None, "dev"));
    }

    #[test]
    fn channel_is_local_rejects_remote_host() {
        let lab = model::parse("[hosts.bench]\nssh = \"u@bench\"\n[targets.t]\nhost = \"bench\"\n")
            .unwrap();
        assert!(!channel_is_local(&lab, None, "bench"));
        // An explicit per-channel host overrides the target default.
        assert!(!channel_is_local(&lab, Some("bench"), "local"));
    }

    #[test]
    fn channel_is_local_honors_local_sentinel() {
        let lab = model::Lab::default();
        assert!(channel_is_local(&lab, Some(model::LOCAL), "somehost"));
        assert!(channel_is_local(&lab, None, model::LOCAL));
    }

    // A target with a console but no DTR opt-in (the pi5 shape: power is a
    // cycle_cmd hook, J2 DTR unwired) must REFUSE `serial dtr`/`reset` instead
    // of silently toggling the lone console — and the error must redirect to the
    // real power method and the console-reboot path.
    #[test]
    fn dtr_refuses_target_without_power_button_optin() {
        let lab = model::parse(
            "[targets.pi5]\n\
             [[targets.pi5.serial]]\n\
             name = \"console\"\n\
             device = \"/dev/ttyUSB0\"\n\
             [targets.pi5.power]\n\
             cycle_cmd = \"zigplug cycle\"\n",
        )
        .unwrap();
        let err = resolve_dtr_interface(&lab, "pi5", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no DTR power-button interface configured"),
            "{err}"
        );
        assert!(err.contains("power-cycle pi5"), "{err}");
        assert!(err.contains("serial send pi5"), "{err}");
        assert!(err.contains("--power-button"), "{err}");
    }

    // An explicit `-i` does NOT bypass the opt-in: choosing an un-opted-in
    // interface by name still refuses (with the interface-specific hint).
    #[test]
    fn dtr_explicit_interface_still_requires_optin() {
        let lab = model::parse(
            "[targets.t]\n\
             [[targets.t.serial]]\n\
             name = \"console\"\n\
             device = \"/dev/ttyUSB0\"\n",
        )
        .unwrap();
        let err = resolve_dtr_interface(&lab, "t", Some("console"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'console'"), "{err}");
        assert!(err.contains("power_button"), "{err}");
    }

    // With the opt-in declared, the interface resolves — both as the sole
    // power_button interface and when selected explicitly.
    #[test]
    fn dtr_resolves_opted_in_interface() {
        let lab = model::parse(
            "[targets.dut]\n\
             [[targets.dut.serial]]\n\
             name = \"console\"\n\
             device = \"/dev/ttyUSB0\"\n\
             power_button = true\n",
        )
        .unwrap();
        assert_eq!(resolve_dtr_interface(&lab, "dut", None).unwrap(), "console");
        assert_eq!(
            resolve_dtr_interface(&lab, "dut", Some("console")).unwrap(),
            "console"
        );
    }

    /// `daemons stop` / `restart` accept a daemon as `paniolo daemons` prints
    /// it: bare (every instance of that daemon) or `name[target]` (one).
    #[test]
    fn daemon_spec_parses_both_forms_and_matches_instances() {
        let bare = DaemonSpec::parse("serialcap");
        assert!(bare.matches("serialcap", Some("pi5")));
        assert!(bare.matches("serialcap", Some("nuc")));
        assert!(bare.matches("serialcap", None));
        assert!(!bare.matches("hdmicap", Some("pi5")));
        assert_eq!(bare.to_string(), "serialcap");

        let one = DaemonSpec::parse("serialcap[pi5]");
        assert_eq!(one.name, "serialcap");
        assert_eq!(one.instance.as_deref(), Some("pi5"));
        assert!(one.matches("serialcap", Some("pi5")));
        assert!(!one.matches("serialcap", Some("nuc")));
        assert!(!one.matches("serialcap", None));
        assert!(!one.matches("hdmicap", Some("pi5")));
        assert_eq!(one.to_string(), "serialcap[pi5]");

        // An unterminated bracket is just a (strange) bare name.
        let odd = DaemonSpec::parse("serialcap[pi5");
        assert_eq!(odd.name, "serialcap[pi5");
        assert!(odd.instance.is_none());
    }

    /// The `hid` discovery dir is served by whichever injector the lab names,
    /// so its process is recognised by either helper's name; every other
    /// daemon by its own.
    #[test]
    fn daemon_process_needles_cover_the_hid_injectors() {
        assert_eq!(daemon_process_needles("serialcap"), vec!["serialcap"]);
        assert_eq!(
            daemon_process_needles(HID_DAEMON),
            vec!["hidrig", "ch9329", "jetkvm"]
        );
    }

    /// `netboot logs --follow` must notice a daemon restart and reopen
    /// instead of reading a stalled handle forever (Review low #9). The
    /// decision is a pure function of what the loop can observe; Linux
    /// happily reuses an inode for a file recreated at the same path, so the
    /// shrunk-file signal is the one that matters in practice.
    #[test]
    fn log_follow_reopens_on_truncation_or_a_new_file_only() {
        // Same file, grown: keep reading.
        assert!(!log_was_replaced(Some(7), Some(7), 100, Some(150)));
        // Same file, same length: nothing happened.
        assert!(!log_was_replaced(Some(7), Some(7), 100, Some(100)));
        // Truncated in place (inode reused): reopen.
        assert!(log_was_replaced(Some(7), Some(7), 100, Some(0)));
        // Recreated as a different file: reopen even if it already grew past us.
        assert!(log_was_replaced(Some(7), Some(8), 100, Some(500)));
        // Momentarily missing (mid-recreate): keep the handle for now.
        assert!(!log_was_replaced(Some(7), None, 100, None));
        // No identity available on this platform at all: length still decides.
        assert!(log_was_replaced(None, None, 100, Some(10)));
    }

    /// The same decision driven by a real file: read to the end, truncate the
    /// file in place the way `daemons::create_log` does, and the loop's
    /// observations say "reopen".
    #[test]
    fn log_follow_notices_an_in_place_truncation() {
        use std::io::{Read, Seek, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        std::fs::write(&path, "first daemon's output\n").unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        let mut sink = Vec::new();
        f.read_to_end(&mut sink).unwrap();
        let id = file_identity(&path);
        let pos = f.stream_position().unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(!log_was_replaced(id, file_identity(&path), pos, Some(len)));

        // create_log's truncate-on-open, then a shorter new log.
        let mut fresh = std::fs::File::create(&path).unwrap();
        fresh.write_all(b"new\n").unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(
            log_was_replaced(id, file_identity(&path), pos, Some(len)),
            "a truncated log must be reopened even when the inode is reused"
        );
    }

    /// `paniolo daemons` prints other processes' raw argv (the stray-helper
    /// listing); a control character in it — an ESC-led terminal escape
    /// sequence, in particular — must not reach the terminal unescaped
    /// (Review low #12).
    #[test]
    fn escape_control_chars_neutralizes_terminal_escapes_and_leaves_text_alone() {
        assert_eq!(
            escape_control_chars("\x1b[2J\x07bell\r\n"),
            "\\x1b[2J\\x07bell\\x0d\\x0a"
        );
        assert_eq!(
            escape_control_chars("zigplug -d /dev/x on 1"),
            "zigplug -d /dev/x on 1"
        );
        // Non-ASCII text is untouched — only control characters are escaped.
        assert_eq!(escape_control_chars("café"), "café");
    }

    /// The gate in front of every `daemons stop` signal, run against a real
    /// process: this one is alive and its command line names the test
    /// binary — and does not name `netbootd`.
    #[test]
    fn pid_runs_one_of_checks_the_live_command_line() {
        let me = std::process::id() as i32;
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(pid_runs_one_of(
            me,
            &["definitely-not-this".to_string(), exe_name]
        ));
        assert!(!pid_runs_one_of(me, &["netbootd".to_string()]));
    }

    /// `daemons stop 'hid[t]'` refused a live jetkvm daemon ("no longer
    /// running hidrig/ch9329") because the hid needles named only the two
    /// older helpers. Run a real process whose command line names `jetkvm`
    /// and check the gate lets the signal through. A shell script, run as
    /// `sh <dir>/jetkvm`, rather than a renamed copy of `sleep`: on hosts
    /// where `sleep` is a multi-call coreutils binary it dispatches on its
    /// own name, so a copy called `jetkvm` exits at once (CI, Linux).
    #[cfg(unix)]
    #[test]
    fn hid_needles_match_a_running_jetkvm() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("jetkvm");
        std::fs::write(&fake, "while :; do sleep 1; done\n").unwrap();
        let mut child = std::process::Command::new("sh").arg(&fake).spawn().unwrap();
        let pid = child.id() as i32;
        let needles = daemon_process_needles(HID_DAEMON);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut ok = false;
        while !ok && std::time::Instant::now() < deadline {
            ok = pid_runs_one_of(pid, &needles);
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        child.kill().unwrap();
        let _ = child.wait();
        assert!(ok, "a running jetkvm must pass the hid identity check");
    }

    // Regression (#203): the config verbs took `-t/--target` and the runtime
    // verbs took the target positionally, so `netboot stop -t pi5` failed with
    // "unexpected argument '-t'" — while `resolve_single_target` told the user
    // to "specify one with -t". Every runtime verb now flattens `TargetArg`,
    // which accepts either spelling and refuses both at once.

    /// Every runtime verb that takes an optional target, as the argv prefix
    /// that reaches it. The list is the test's own; a verb added without a
    /// line here is simply uncovered, which is why the parse assertions below
    /// are cheap enough to keep the list complete.
    const RUNTIME_VERBS: &[&[&str]] = &[
        &["console"],
        &["power-cycle"],
        &["power-state"],
        &["serial", "connect"],
        &["serial", "watch"],
        &["serial", "stop"],
        &["serial", "show"],
        &["serial", "status"],
        &["serial", "evidence"],
        &["serial", "wait", "--epoch", "capture-a", "--since", "0"],
        &["serial", "log"],
        &["serial", "dtr"],
        &["serial", "reset"],
        &["video", "watch"],
        &["video", "stop"],
        &["video", "shot"],
        &["video", "read"],
        &["video", "preview"],
        &["video", "show"],
        &["netboot", "start"],
        &["netboot", "stop"],
        &["netboot", "status"],
        &["netboot", "logs"],
        &["netboot", "tftp-root"],
        &["netif", "mode", "link"],
        &["netif", "down-hard"],
        &["netif", "status"],
        &["power", "on"],
        &["power", "off"],
        &["hid", "serve"],
        &["hid", "stop"],
        &["adb", "shell"],
        &["adb", "screencap"],
    ];

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec!["paniolo"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full)
    }

    /// The merge every runtime verb goes through. Four combinations, one
    /// place: there is no per-verb merge left to get wrong.
    #[test]
    fn target_arg_takes_either_spelling_and_prefers_neither() {
        let pos = TargetArg {
            target_pos: Some("pi5".into()),
            target: None,
        };
        let flag = TargetArg {
            target_pos: None,
            target: Some("pi5".into()),
        };
        assert_eq!(pos.name(), Some("pi5"));
        assert_eq!(flag.name(), Some("pi5"));
        assert_eq!(TargetArg::default().name(), None);
        // Both at once is refused by clap before this is reached (see
        // `every_runtime_verb_refuses_both_spellings_at_once`), so the merge
        // never has to arbitrate between two different names.
    }

    /// The bug, on every verb that had it: `-t` and `--target` must parse
    /// wherever the positional does.
    #[test]
    fn every_runtime_verb_accepts_both_target_spellings() {
        for verb in RUNTIME_VERBS {
            for spelling in [vec!["pi5"], vec!["-t", "pi5"], vec!["--target", "pi5"]] {
                let mut argv = verb.to_vec();
                argv.extend(spelling.iter().copied());
                parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            }
            // Naming no target stays legal: a lab with one target needs none.
            parse(verb).unwrap_or_else(|e| panic!("{verb:?} (no target): {e}"));
        }
    }

    /// Both spellings at once is a clean clap error, not a silent precedence
    /// rule the reader has to know about.
    #[test]
    fn every_runtime_verb_refuses_both_spellings_at_once() {
        for verb in RUNTIME_VERBS {
            let mut argv = verb.to_vec();
            argv.extend(["pi5", "-t", "pi5"]);
            let err = parse(&argv)
                .err()
                .unwrap_or_else(|| panic!("{argv:?} must be refused"));
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?}: {err}"
            );
        }
    }

    /// `resolve_single_target` tells the user to "specify one with -t" for
    /// every runtime verb. That advice was false for most of them; these are
    /// the verbs that print it, so `-t` has to work on each.
    #[test]
    fn the_multiple_targets_message_names_a_spelling_that_works() {
        let lab = model::parse(
            "[targets.a]\n[targets.a.power]\ncycle_cmd = \"true\"\n\
             [targets.b]\n[targets.b.power]\ncycle_cmd = \"true\"\n",
        )
        .expect("two-target lab");
        let err = resolve_single_target(&lab, None).unwrap_err().to_string();
        assert!(
            err.contains("-t"),
            "the message should name a spelling: {err}"
        );
        // ...and that spelling parses on the verbs that can print it.
        for verb in RUNTIME_VERBS {
            let mut argv = verb.to_vec();
            argv.extend(["-t", "a"]);
            parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
    }

    /// The merged value is what the handler acts on, for both spellings.
    #[test]
    fn netboot_and_netif_handlers_see_the_merged_target() {
        let target_of = |argv: &[&str]| -> Option<String> {
            let cli = parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.command {
                Command::Netboot { cmd } => match cmd {
                    NetbootCmd::Start { target }
                    | NetbootCmd::Stop { target }
                    | NetbootCmd::Status { target }
                    | NetbootCmd::TftpRoot { target }
                    | NetbootCmd::Logs { target, .. } => target.name().map(str::to_string),
                    _ => panic!("{argv:?}: not a netboot runtime verb"),
                },
                Command::Netif { cmd } => match cmd {
                    NetifCmd::Mode { target, .. }
                    | NetifCmd::DownHard { target }
                    | NetifCmd::Status { target } => target.name().map(str::to_string),
                },
                _ => panic!("{argv:?}: not netboot or netif"),
            }
        };
        for verb in [
            vec!["netboot", "start"],
            vec!["netboot", "stop"],
            vec!["netboot", "status"],
            vec!["netboot", "logs"],
            vec!["netboot", "tftp-root"],
            vec!["netif", "mode", "link"],
            vec!["netif", "down-hard"],
            vec!["netif", "status"],
        ] {
            let mut pos = verb.clone();
            pos.push("pi5");
            let mut flag = verb.clone();
            flag.extend(["-t", "pi5"]);
            assert_eq!(
                target_of(&pos).as_deref(),
                Some("pi5"),
                "{verb:?} positional"
            );
            assert_eq!(target_of(&flag).as_deref(), Some("pi5"), "{verb:?} -t");
            assert_eq!(target_of(&verb), None, "{verb:?} with no target");
        }
    }

    /// `netboot logs` keeps its own short flags next to the shared `-t`.
    #[test]
    fn netboot_logs_keeps_tail_and_follow_alongside_target_flag() {
        let cli = parse(&["netboot", "logs", "-t", "pi5", "-n", "7", "-f"]).unwrap();
        let Command::Netboot {
            cmd:
                NetbootCmd::Logs {
                    target,
                    tail,
                    follow,
                },
        } = cli.command
        else {
            panic!("expected netboot logs");
        };
        assert_eq!(target.target_pos, None);
        assert_eq!(target.name(), Some("pi5"));
        assert_eq!(tail, 7);
        assert!(follow);
    }

    /// The config verbs are deliberately NOT part of this: they still require
    /// `-t` and still refuse a positional, because a command that creates or
    /// destroys configuration should not act on an implicit target.
    #[test]
    fn config_verbs_still_require_the_target_flag() {
        // Verbs whose only required argument is the target, so the positional
        // and missing-target assertions below are unambiguous.
        for verb in [
            vec!["netboot", "rm"],
            vec!["power", "rm"],
            vec!["hid", "rm"],
        ] {
            let mut flag = verb.clone();
            flag.extend(["-t", "pi5"]);
            parse(&flag).unwrap_or_else(|e| panic!("{flag:?}: {e}"));

            let mut pos = verb.clone();
            pos.push("pi5");
            assert!(
                parse(&pos).is_err(),
                "{pos:?} must not take the target positionally"
            );
            assert!(parse(&verb).is_err(), "{verb:?} must require --target");
        }
    }

    /// `video set --change-threshold` takes a byte: 0 and 255 parse, 256 and
    /// a negative number are refused by clap before any lab edit happens.
    #[test]
    fn video_set_change_threshold_must_fit_a_byte() {
        fn argv(n: &str) -> Vec<&str> {
            vec![
                "video",
                "set",
                "-t",
                "t",
                "-d",
                "x",
                "--change-threshold",
                n,
            ]
        }
        for ok in ["0", "16", "255"] {
            assert!(parse(&argv(ok)).is_ok(), "{ok} should parse");
        }
        for bad in ["256", "-1", "abc"] {
            assert!(parse(&argv(bad)).is_err(), "{bad} should be refused");
        }
    }
}
