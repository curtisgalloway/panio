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

//! hidrig — drive the dual-board KB2040 USB-HID injector.
//!
//! hidrig owns HID *composition*: each subcommand (type, key, combo, down, up,
//! releaseall, move, moveabs, click, mdown, mup, scroll, ping, version) is
//! turned into HID report bytes (see [`compose`]) and wrapped in the binary
//! frames the control board relays over I2C to the target board, which injects
//! them as USB-HID into the DUT — the "dumb pipe" rig whose firmware lives in the
//! paniolo-hardware repo
//! (<https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware>).
//! The control board is a USB-CDC device; hidrig writes frames to its data endpoint.
//!
//! `run` executes a command file. `serve` runs a daemon that owns the control
//! link, holds the composition state (held keys, virtual cursor), and re-exposes
//! the command protocol over a localhost WebSocket so the web console intermixes
//! with CLI injections; when a daemon is running for the same device, one-shots
//! route through it automatically.

mod auth;
mod compose;
// Not every helper needs every primitive in here (hdmicap and serialcap have
// no pid-liveness probe of their own), and the file is kept byte-identical
// across the crates that copy it, so unused items are expected rather than a
// smell.
mod daemon;
#[allow(dead_code)]
mod platform;
mod proto;
#[cfg(unix)]
mod pty;
mod server;
mod uart;

use std::io::Read;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use compose::Composer;
use proto::{open_port, parse_sequence, run_command, Step};

#[derive(Parser)]
#[command(
    name = "hidrig",
    version,
    about = "Dual-board KB2040 USB-HID injector: keyboard/mouse via the control board's CDC link"
)]
struct Cli {
    /// Control board data CDC endpoint (e.g. /dev/cu.usbmodemXXXX). Required.
    // Option<String> because clap does not permit global required args; main()
    // validates presence before dispatch.
    #[arg(short = 'd', long = "device", value_name = "DEVICE", global = true)]
    device: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Type a string of text.
    Type {
        /// Text to type (multiple words are joined with single spaces).
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Tap (press then release) a key, e.g. `key ENTER`.
    Key {
        /// adafruit_hid Keycode name (A-Z, ENTER, TAB, LEFT_CONTROL, F1..F12, ...).
        name: String,
    },
    /// Chord: press all the named keys, then release all, e.g. `combo LEFT_CONTROL C`.
    Combo {
        /// Keycode names, pressed together.
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Press and hold a key.
    Down { name: String },
    /// Release a held key.
    Up { name: String },
    /// Release all held keys.
    Releaseall,
    /// Relative mouse move in pixels (negative values allowed).
    Move {
        #[arg(allow_hyphen_values = true)]
        dx: i32,
        #[arg(allow_hyphen_values = true)]
        dy: i32,
    },
    /// Absolute mouse move in a 0..32767 logical space (requires the
    /// `moveabs` capability; the host OS maps the range across the screen).
    Moveabs {
        /// X in 0..32767 (clamped).
        x: i32,
        /// Y in 0..32767 (clamped).
        y: i32,
    },
    /// Click a mouse button.
    Click {
        /// left | right | middle.
        #[arg(default_value = "left")]
        button: String,
    },
    /// Press and hold a mouse button.
    Mdown {
        #[arg(default_value = "left")]
        button: String,
    },
    /// Release a held mouse button.
    Mup {
        #[arg(default_value = "left")]
        button: String,
    },
    /// Scroll the wheel (positive = up, negative = down).
    Scroll {
        #[arg(allow_hyphen_values = true)]
        amount: i32,
    },
    /// No-op health check: confirms the board is powered and replying.
    Ping,
    /// Print the board's protocol version, implementation id, and capabilities.
    Version,
    /// Power the DUT off/on, or power-cycle it, via the control board's relay.
    Power {
        /// off | on | cycle
        action: String,
        /// For `cycle`: seconds to stay off before powering back on (the
        /// firmware default is used if omitted).
        secs: Option<u8>,
    },
    /// Run a command file: one protocol command per line; blank lines and
    /// `# comments` are skipped; `delay <ms>` / `sleep <seconds>` pause.
    Run {
        /// Path to the command file, or `-` for stdin.
        file: String,
        /// Extra delay in milliseconds after every command.
        #[arg(long, default_value_t = 0)]
        delay_ms: u64,
    },
    /// Run the daemon: own the UART and re-expose the protocol over a localhost
    /// WebSocket (the KVM path). Blocks until stopped. One-shot invocations for
    /// the same device route through this daemon automatically.
    Serve {
        /// TCP port to listen on (0 = OS-assigned; the port is published in the
        /// discovery file the panio console reads).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Ask a running hid daemon to shut down using its authentication token.
    ///
    /// Older daemons without the shutdown endpoint must be stopped with
    /// `panio daemons stop hid` before starting the updated daemon.
    Stop,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Commands that don't open the UART directly.
    match &cli.cmd {
        Cmd::Serve { port } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                // Log to stderr: the panio CLI redirects a daemon's stderr into its
                // daemon.log and discards stdout, so logging to stdout left every managed
                // daemon's log file empty.
                .with_writer(std::io::stderr)
                .init();
            let device = require_device(&cli)?;
            return daemon::run(device.to_string(), *port);
        }
        Cmd::Stop => return cmd_stop(),
        _ => {}
    }

    let device = require_device(&cli)?;
    let mut tx = Sender::open(device)?;

    match cli.cmd {
        Cmd::Type { text } => one(&mut tx, &format!("type {}", text.join(" "))),
        Cmd::Key { name } => one(&mut tx, &format!("key {name}")),
        Cmd::Combo { names } => one(&mut tx, &format!("combo {}", names.join(" "))),
        Cmd::Down { name } => one(&mut tx, &format!("down {name}")),
        Cmd::Up { name } => one(&mut tx, &format!("up {name}")),
        Cmd::Releaseall => one(&mut tx, "releaseall"),
        Cmd::Move { dx, dy } => one(&mut tx, &format!("move {dx} {dy}")),
        Cmd::Moveabs { x, y } => one(&mut tx, &format!("moveabs {x} {y}")),
        Cmd::Click { button } => one(&mut tx, &format!("click {button}")),
        Cmd::Mdown { button } => one(&mut tx, &format!("mdown {button}")),
        Cmd::Mup { button } => one(&mut tx, &format!("mup {button}")),
        Cmd::Scroll { amount } => one(&mut tx, &format!("scroll {amount}")),
        Cmd::Ping => one(&mut tx, "ping"),
        Cmd::Version => {
            let reply = tx.send("version")?;
            println!("{reply}");
            Ok(())
        }
        Cmd::Power { action, secs } => {
            let line = match secs {
                Some(s) => format!("power {action} {s}"),
                None => format!("power {action}"),
            };
            one(&mut tx, &line)
        }
        Cmd::Run { file, delay_ms } => cmd_run(&mut tx, &file, delay_ms),
        Cmd::Serve { .. } | Cmd::Stop => unreachable!("handled above"),
    }
}

fn require_device(cli: &Cli) -> Result<&str> {
    cli.device
        .as_deref()
        .ok_or_else(|| anyhow!("required argument '--device <DEVICE>' (-d) was not provided"))
}

/// One command line, sent either through a running daemon or directly to the
/// UART, depending on what owns the device.
enum Sender {
    /// A hid daemon owns this device; route commands through its HTTP API,
    /// presenting the token from its discovery file (absent for a daemon
    /// older than the token, which accepts anything).
    Daemon { base: String, token: Option<String> },
    /// No daemon for this device; we hold the control link ourselves and
    /// compose frames in-process (held-key / cursor state lasts one process).
    Direct {
        composer: Composer,
        port: Box<dyn serialport::SerialPort>,
    },
}

impl Sender {
    /// Choose the transport: if a hid daemon is running for `device`, route
    /// through it (it holds the port, so a direct open would fail anyway);
    /// otherwise open the UART directly.
    fn open(device: &str) -> Result<Sender> {
        if let Some(d) = daemon::discover() {
            if d.device == device {
                return Ok(Sender::Daemon {
                    base: format!("http://127.0.0.1:{}", d.port),
                    token: d.token,
                });
            }
        }
        Ok(Sender::Direct {
            composer: Composer::new(),
            port: open_port(device)?,
        })
    }

    /// Send one command line, returning the `OK` reply data (empty for a bare
    /// `OK`). Errors carry the board's `ERR` message or a transport failure.
    fn send(&mut self, line: &str) -> Result<String> {
        match self {
            Sender::Daemon { base, token } => post_send(base, token.as_deref(), line),
            Sender::Direct { composer, port } => run_command(composer, port, line),
        }
    }
}

/// POST one command line to a running daemon's `/send`; return the reply body.
fn post_send(base: &str, token: Option<&str>, line: &str) -> Result<String> {
    let mut req = ureq::post(&format!("{base}/send")).timeout(Duration::from_secs(15));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.send_string(line) {
        Ok(resp) => Ok(resp.into_string().unwrap_or_default().trim().to_string()),
        // A 503 carries the board's ERR / transport message in the body.
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(anyhow!("{}", body.trim()))
        }
        Err(e) => Err(anyhow!("hid daemon /send failed: {e}")),
    }
}

/// Send a single command and acknowledge with `OK` on stdout.
fn one(tx: &mut Sender, cmd: &str) -> Result<()> {
    tx.send(cmd)?;
    println!("OK");
    Ok(())
}

fn cmd_run(tx: &mut Sender, file: &str, delay_ms: u64) -> Result<()> {
    let text = if file == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| anyhow!("reading stdin: {e}"))?;
        s
    } else {
        std::fs::read_to_string(file).map_err(|e| anyhow!("{file}: {e}"))?
    };
    let steps = parse_sequence(&text)?;
    let mut sent = 0usize;
    for step in steps {
        match step {
            Step::Delay(secs) => thread::sleep(Duration::from_secs_f64(secs)),
            Step::Cmd(cmd) => {
                tx.send(&cmd)?;
                sent += 1;
                if delay_ms > 0 {
                    thread::sleep(Duration::from_millis(delay_ms));
                }
            }
        }
    }
    println!("OK ({sent} commands)");
    Ok(())
}

/// Stop a running hid daemon through its token-protected `POST /stop`.
fn cmd_stop() -> Result<()> {
    match daemon::discover() {
        Some(d) => {
            request_stop(&d)?;
            println!("hid daemon (pid {}) stopping", d.pid);
            Ok(())
        }
        None => {
            println!("no hid daemon running");
            Ok(())
        }
    }
}

/// Shut the daemon down through its token-protected `POST /stop`.
///
/// Never falls back to signaling `d.pid`. A discovery file outlives the daemon
/// whenever it did not exit cleanly, and once the kernel reuses that PID the
/// record names an unrelated process; only the token proves the request
/// reached the daemon that wrote the file. A daemon too old to have the
/// endpoint answers 404 and must be stopped by the panio CLI, which checks
/// the process identity before it signals anything.
fn request_stop(d: &daemon::Discovery) -> Result<()> {
    let token =
        d.token.as_deref().filter(|t| !t.is_empty()).context(
            "daemon has no token; use `panio daemons stop hid` to stop the older daemon",
        )?;
    ureq::post(&format!("http://127.0.0.1:{}/stop", d.port))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(5))
        .send_bytes(&[])
        .context(
            "authenticated shutdown failed; for an older daemon use `panio daemons stop hid`",
        )?;
    Ok(())
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    /// A stale record naming a live PID (here: our own) must be answered by
    /// the daemon behind the token or fail — never by a signal to that PID.
    /// The fake daemon rejects the token so the request fails; the process
    /// running this test is still alive afterwards, which is the point.
    #[test]
    fn stale_record_never_signals_its_live_pid() {
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
            assert!(request.starts_with("POST /stop "), "{request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer old-token"),
                "{request}"
            );
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
            device: "/dev/test".into(),
            console: None,
            console_device: None,
        };
        assert!(request_stop(&record).is_err());
        worker.join().unwrap();
        // No token at all (a pre-token daemon): refuse rather than signal.
        record.token = None;
        assert!(request_stop(&record).is_err());
    }
}
