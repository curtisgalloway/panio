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

//! serialcap — serial console daemon + thin client CLI.
//!
//! Subcommands:
//!   daemon   own a serial port and serve it over a localhost WebSocket
//!   log      print captured serial output (timestamped, by line range)
//!   evidence read immutable exact-byte receive chunks and capture diagnostics
//!   devices  list serial devices
//!   stop     ask the running daemon to exit (authenticated HTTP)

mod auth;
mod capture;
mod daemon;
mod evidence;
// The file is kept byte-identical across the crates that copy it, so a crate
// may not use every primitive in it.
#[allow(dead_code)]
mod platform;
mod serial_io;
mod server;
mod transact;
mod wait;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::serial_io::InterfaceSpec;

#[derive(Parser)]
#[command(name = "serialcap", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Parse a `NAME=DEVICE[@BAUD][:SENSE]` interface spec.
///
/// - `BAUD` defaults to 115200 when omitted.
/// - `SENSE` is optional; valid values are `cts`, `dsr`, `dcd`, `ri`.
///   It denotes which FTDI modem-control input is wired to the target's 3.3 V
///   rail for power-state sensing.
///
/// Example: `console=/dev/ttyUSB0@115200:cts`
fn parse_interface(s: &str) -> Result<InterfaceSpec, String> {
    let (name, rest) = s
        .split_once('=')
        .ok_or("expected NAME=DEVICE[@BAUD][:SENSE], e.g. console=/dev/ttyUSB0@115200:cts")?;
    if name.is_empty() {
        return Err("interface name is empty".into());
    }

    // Peel off optional :SENSE suffix. Only recognised signal names count;
    // anything else (including colons embedded in /dev/serial/by-path/… paths)
    // is treated as part of the device path.
    let (dev_baud, power_sense_signal) = if let Some((prefix, maybe_sense)) = rest.rsplit_once(':')
    {
        match maybe_sense {
            "cts" | "dsr" | "dcd" | "ri" => (prefix, Some(maybe_sense.to_string())),
            _ => (rest, None),
        }
    } else {
        (rest, None)
    };

    let (device, baud) = match dev_baud.rsplit_once('@') {
        Some((dev, b)) => (
            dev,
            b.parse::<u32>()
                .map_err(|_| format!("invalid baud '{b}'"))?,
        ),
        None => (dev_baud, 115_200_u32),
    };
    if device.is_empty() {
        return Err("device path is empty".into());
    }
    Ok(InterfaceSpec {
        name: name.to_string(),
        device: device.to_string(),
        baud,
        power_sense_signal,
    })
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the serial console daemon (foreground; controller manages it).
    ///
    /// Owns one or more named interfaces; repeat --interface for each.
    Daemon {
        /// A serial interface as NAME=DEVICE[@BAUD] (repeatable), e.g.
        /// console=/dev/ttyUSB0@115200.
        #[arg(long = "interface", value_name = "NAME=DEVICE[@BAUD]", value_parser = parse_interface, required = true)]
        interfaces: Vec<InterfaceSpec>,
        /// Port to bind on localhost. 0 = OS-assigned.
        #[arg(long, default_value_t = 8724)]
        port: u16,
        /// Approximate number of recent lines retained on disk, per interface.
        #[arg(long, default_value_t = capture::DEFAULT_BUFFER_LINES)]
        buffer_lines: u64,
    },
    /// Print captured serial output. Reads the daemon's on-disk log directly, so
    /// it works whether or not the daemon is currently running.
    Log {
        /// Interface name. Optional when only one interface has been captured.
        #[arg(long, short = 'i')]
        interface: Option<String>,
        /// Show only the most recent N lines.
        #[arg(long, short = 'n')]
        tail: Option<u64>,
        /// Lowest line sequence number to include (inclusive).
        #[arg(long)]
        from: Option<u64>,
        /// Highest line sequence number to include (inclusive).
        #[arg(long)]
        to: Option<u64>,
        /// Show only lines newer than this sequence number (for polling).
        #[arg(long)]
        since: Option<u64>,
        /// Keep raw bytes (ANSI escapes, control chars) instead of cleaning them.
        #[arg(long)]
        raw: bool,
        /// Emit JSON Lines (seq, ts_ms, text) instead of formatted text.
        #[arg(long)]
        json: bool,
        /// Exclude the current unterminated line.
        #[arg(long)]
        no_pending: bool,
    },
    /// Read immutable received chunks as JSON without opening a device.
    /// Original RX times and exact byte arrays are separate from observation
    /// time. Defaults to the newest retained epoch; reports retention gaps.
    Evidence {
        /// Interface name (required when multiple interfaces were captured).
        #[arg(long, short = 'i')]
        interface: Option<String>,
        /// Capture epoch to read; required with --since.
        #[arg(long)]
        epoch: Option<String>,
        /// Return RX sequences strictly after this cursor within --epoch.
        #[arg(long, requires = "epoch")]
        since: Option<u64>,
        /// Maximum raw bytes returned (1..=1048576); JSON overhead is additional.
        /// Stops before an oversized chunk, returning its size and continuation.
        #[arg(long, default_value_t = evidence::DEFAULT_MAX_BYTES)]
        max_bytes: u64,
    },
    /// Acquire fresh raw output until a pattern, fault rule, deadline or byte cap.
    /// Returns JSON evidence and an explicit status even on timeout/disconnect;
    /// never sends input, resets hardware or restarts a daemon.
    Wait {
        /// Configured serial interface to observe.
        #[arg(long, short = 'i')]
        interface: String,

        /// Capture epoch from serial status/evidence; prevents stale data matching.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        epoch: String,
        /// Exclusive RX chunk sequence boundary within that epoch.
        #[arg(long)]
        since: u64,
        /// Finite acquisition deadline in ms, including journal catch-up
        /// (0 = one immediate observation; max 1 hour).
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
        /// Fault rule NAME=PATTERN (repeatable, split at the first `=`): end
        /// with status `fault` when this literal appears in fresh raw bytes.
        /// The earliest hit among --pattern and the rules wins; a tie goes to
        /// --pattern, then to the rules in the order given.
        #[arg(long = "fault", value_name = "NAME=PATTERN", value_parser = parse_fault)]
        faults: Vec<(String, String)>,
        /// Include the trailing unterminated bytes acquired after the boundary.
        #[arg(long)]
        include_pending: bool,
        /// Deliver newline-terminated rows as NDJSON before the final result.
        /// The first row may start mid-line and is marked prefix_unknown.
        #[arg(long)]
        follow: bool,
    },
    /// List available serial devices and exit (no daemon needed).
    Devices,
    /// Ask the running daemon to shut down using its authentication token.
    ///
    /// Older daemons without the shutdown endpoint must be stopped with
    /// `panio daemons stop serialcap` before starting the updated daemon.
    Stop,
}

/// `--fault NAME=PATTERN`: split at the first `=`; both halves nonempty.
fn parse_fault(arg: &str) -> Result<(String, String), String> {
    match arg.split_once('=') {
        Some((name, pattern)) if !name.is_empty() && !pattern.is_empty() => {
            Ok((name.to_string(), pattern.to_string()))
        }
        _ => Err(format!(
            "expected NAME=PATTERN with both parts nonempty, got '{arg}'"
        )),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "serialcap=info".into()),
        )
        // Log to stderr: the panio CLI redirects a daemon's stderr into its
        // daemon.log and discards stdout, so logging to stdout left every managed
        // daemon's log file empty.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon {
            interfaces,
            port,
            buffer_lines,
        } => daemon::run(interfaces, port, buffer_lines),
        Cmd::Log {
            interface,
            tail,
            from,
            to,
            since,
            raw,
            json,
            no_pending,
        } => capture::cmd_log(capture::LogArgs {
            interface,
            tail,
            from,
            to,
            since,
            raw,
            json,
            no_pending,
        }),
        Cmd::Evidence {
            interface,
            epoch,
            since,
            max_bytes,
        } => evidence::cmd_evidence(interface, epoch, since, max_bytes),
        Cmd::Wait {
            interface,
            epoch,
            since,
            timeout_ms,
            max_bytes,
            max_output_bytes,
            pattern,
            faults,
            include_pending,
            follow,
        } => wait::cmd_wait(
            interface,
            wait::Request {
                epoch,
                since,
                timeout_ms,
                max_bytes,
                max_output_bytes,
                pattern,
                faults,
                include_pending,
                follow,
            },
        ),
        Cmd::Devices => cmd_devices(),
        Cmd::Stop => cmd_stop(),
    }
}

fn cmd_devices() -> Result<()> {
    for (path, misc) in serial_io::list_ports()? {
        println!("{path}  [{misc}]");
    }
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let d = daemon::discover().context("is the daemon running?")?;
    request_stop(&d)?;
    println!("daemon (pid {}) stopping", d.pid);
    Ok(())
}

/// Never fall back to signaling the PID: it may have been recycled.
fn request_stop(d: &daemon::Discovery) -> Result<()> {
    let token = d.token.as_deref().filter(|t| !t.is_empty()).context(
        "daemon has no token; use `panio daemons stop serialcap` to stop the older daemon",
    )?;
    ureq::post(&format!("http://127.0.0.1:{}/stop", d.port))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(std::time::Duration::from_secs(5))
        .send_bytes(&[])
        .context(
            "authenticated shutdown failed; for an older daemon use `panio daemons stop serialcap`",
        )?;
    Ok(())
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    #[test]
    fn stale_record_never_signals_its_live_pid() {
        // Our own PID is alive, but a failed request must never signal it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("POST /stop "));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer old-token"));
            socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let mut record = daemon::Discovery {
            pid: std::process::id(),
            port,
            token: Some("old-token".into()),
            interfaces: vec![],
        };
        assert!(request_stop(&record).is_err());
        worker.join().unwrap();
        record.token = None;
        assert!(request_stop(&record).is_err());
    }
}
