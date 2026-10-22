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

//! ch9329 — drive a WCH CH9329 USB-HID bridge (as in the Openterface Mini-KVM)
//! over its control UART, speaking panio's HID serial protocol.
//!
//! This is a sibling of `hidrig` (the KB2040 injector client): same CLI surface
//! — `type`, `key`, `combo`, `down`, `up`, `releaseall`, `move`, `moveabs`,
//! `click`, `mdown`, `mup`, `scroll`, `ping`, `version`, `run`, plus `serve`/
//! `stop` for the KVM daemon — so it drops into a panio `hid` channel
//! (`panio hid set --cmd "ch9329 -d <uart>"`). Unlike hidrig there is no
//! microcontroller running firmware: the CH9329 chip is itself the USB HID
//! device, so this client speaks the chip's binary frame protocol directly (see
//! `session.rs`). `serve` runs a daemon that owns the UART and re-exposes the
//! protocol over a WebSocket so the web console can stream events that intermix
//! with CLI injections; when a daemon is running for the same device, one-shots
//! route through it automatically.

mod auth;
mod daemon;
mod keys;
// The file is kept byte-identical across the crates that copy it, so a crate
// may not use every primitive in it.
#[allow(dead_code)]
mod platform;
mod proto;
mod server;
mod session;
mod uart;

use std::io::Read;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use proto::{budget, execute_line, parse_sequence, Step, CLIENT_MARGIN};
use session::Session;

#[derive(Parser)]
#[command(
    name = "ch9329",
    version,
    about = "WCH CH9329 USB-HID bridge (Openterface Mini-KVM): keyboard/mouse injection over a control UART",
    after_help = "Exit status: 0 success; 1 error; 2 usage; 40 a send through the daemon timed out and \
the daemon cancelled the command and confirmed it stopped; 41 timed out and the daemon could not be \
confirmed stopped (it may still be typing)."
)]
struct Cli {
    /// CH9329 control UART (the CH340 USB-serial adapter, e.g.
    /// /dev/cu.usbserial-XXXX). Required.
    #[arg(short = 'd', long = "device", value_name = "DEVICE", global = true)]
    device: Option<String>,

    /// Force the serial baud rate (default: autodetect 115200, 57600, then 9600).
    #[arg(short = 'b', long = "baud", global = true)]
    baud: Option<u32>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Type a string of text (US layout), at most 4096 characters.
    ///
    /// Through a running daemon the wait grows with the text (10 s plus
    /// 100 ms per character). If the send times out or this process is
    /// killed, the daemon cancels the typing and releases the keys. After a
    /// timeout the exit status is 40 (cancelled and confirmed stopped) or 41
    /// (could not confirm; the target may still be receiving keystrokes).
    Type {
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Tap (press then release) a key, e.g. `key ENTER`.
    Key {
        /// adafruit_hid Keycode name (A-Z, ENTER, TAB, LEFT_CONTROL, F1..F12, ...).
        name: String,
    },
    /// Chord: press all named keys, then release all, e.g. `combo LEFT_CONTROL C`.
    Combo {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Press and hold a key.
    Down { name: String },
    /// Release a held key.
    Up { name: String },
    /// Release all held keys.
    Releaseall,
    /// Relative mouse move (negative values allowed).
    Move {
        #[arg(allow_hyphen_values = true)]
        dx: i32,
        #[arg(allow_hyphen_values = true)]
        dy: i32,
    },
    /// Absolute mouse move in a 0..32767 logical space (the host OS maps the
    /// range across the screen).
    Moveabs { x: i32, y: i32 },
    /// Click a mouse button.
    Click {
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
    /// No-op health check: confirms the chip is powered and replying (GET_INFO).
    Ping,
    /// Print protocol version, implementation id, and capabilities.
    Version,
    /// Print CH9329 chip status (GET_INFO): device-reported chip_version, whether the
    /// target has enumerated the emulated HID, lock-LED state, and link baud.
    /// On Openterface emulators, chip_version may identify the board type;
    /// it is not a reliable firmware release number. CH9329-specific
    /// (not part of the HID serial protocol).
    Info,
    /// Run a command file: one protocol command per line; blank lines and
    /// `# comments` are skipped; `delay <ms>` / `sleep <seconds>` pause.
    Run {
        /// Path to the command file, or `-` for stdin.
        file: String,
        /// Extra delay in milliseconds after every command.
        #[arg(long, default_value_t = 0)]
        delay_ms: u64,
    },
    /// Run the KVM daemon: own the UART and re-expose the protocol over a
    /// localhost WebSocket (the `panio console` path). Blocks until stopped.
    /// One-shots for the same device route through this daemon automatically.
    Serve {
        /// TCP port to listen on (0 = OS-assigned; the port is published in the
        /// discovery file panio reads).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Ask a running hid daemon to shut down using its authentication token.
    ///
    /// Older daemons without the shutdown endpoint must be stopped with
    /// `panio daemons stop hid` before starting the updated daemon.
    Stop,
    /// Persistently set the CH9329's serial baud (SET_PARA_CFG flash + reset),
    /// then reconnect at the new rate. The datasheet range is 1200..=115200
    /// (Openterface default 115200; NanoKVM-USB 57600; factory chips 9600).
    /// Unlike the protocol's
    /// transient `baud`, this is stored in flash and survives a power-cycle.
    Baud { rate: u32 },
    /// Switch or query the Openterface USB mux, which shares one onboard
    /// microSD reader between the host and the target (never both at once).
    /// Present on the KVM-Go; a real CH9329 does not implement it and answers
    /// with silence, which surfaces here as a timeout meaning "unsupported".
    Usb {
        /// `host` and `target` drive the mux and verify it landed there;
        /// `state` reports the current side without changing it.
        #[arg(value_parser = ["host", "target", "state"])]
        action: String,
    },
}

/// Exit status: a command sent through the daemon timed out, the daemon was
/// then asked to prove it had stopped (a `version` queued behind the command
/// is answered), and it did. The keys the command pressed are released; typing
/// has stopped.
const EXIT_TIMEOUT_STOPPED: u8 = 40;
/// Exit status: a command sent through the daemon timed out and the daemon
/// could not be shown to have stopped within [`CONFIRM_WAIT`]. It may still be
/// typing; do not assume otherwise.
const EXIT_TIMEOUT_UNCONFIRMED: u8 = 41;
/// How long the client waits for the daemon's proof of stopping after a
/// timeout of its own.
const CONFIRM_WAIT: Duration = Duration::from_secs(5);

/// A `/send` that timed out, and whether the daemon was then shown to have
/// stopped the command (#328). `main` turns it into [`EXIT_TIMEOUT_STOPPED`]
/// or [`EXIT_TIMEOUT_UNCONFIRMED`] instead of the generic exit status 1.
#[derive(Debug, PartialEq, Eq)]
enum SendTimeout {
    Stopped,
    Unconfirmed,
}

impl SendTimeout {
    fn exit_status(&self) -> u8 {
        match self {
            SendTimeout::Stopped => EXIT_TIMEOUT_STOPPED,
            SendTimeout::Unconfirmed => EXIT_TIMEOUT_UNCONFIRMED,
        }
    }
}

impl std::fmt::Display for SendTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendTimeout::Stopped => write!(
                f,
                "timed out; the daemon cancelled the command and confirmed it stopped"
            ),
            SendTimeout::Unconfirmed => write!(
                f,
                "timed out; could not confirm the daemon stopped the command \
                 (it may still be typing)"
            ),
        }
    }
}

impl std::error::Error for SendTimeout {}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            if let Some(t) = e.downcast_ref::<SendTimeout>() {
                eprintln!("Error: {t}");
                return std::process::ExitCode::from(t.exit_status());
            }
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Commands that don't take the one-shot UART path.
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
    let mut tx = Sender::open(device, cli.baud)?;

    match cli.cmd {
        Cmd::Type { text } => one(&mut tx, &format!("type {}", text.join(" "))),
        Cmd::Key { name } => one(&mut tx, &format!("key {name}")),
        Cmd::Combo { names } => one(&mut tx, &format!("combo {}", names.join(" "))),
        Cmd::Down { name } => one(&mut tx, &format!("down {name}")),
        Cmd::Up { name } => one(&mut tx, &format!("up {name}")),
        Cmd::Releaseall => one(&mut tx, "releaseall"),
        Cmd::Move { dx, dy } => one(&mut tx, &format!("move {dx} {dy}")),
        Cmd::Moveabs { x, y } => one(
            &mut tx,
            &format!("moveabs {} {}", proto::clamp_abs(x), proto::clamp_abs(y)),
        ),
        Cmd::Click { button } => one(&mut tx, &format!("click {button}")),
        Cmd::Mdown { button } => one(&mut tx, &format!("mdown {button}")),
        Cmd::Mup { button } => one(&mut tx, &format!("mup {button}")),
        Cmd::Scroll { amount } => one(&mut tx, &format!("scroll {amount}")),
        Cmd::Ping => one(&mut tx, "ping"),
        Cmd::Version => {
            println!("{}", tx.run_line("version")?);
            Ok(())
        }
        Cmd::Info => {
            println!("{}", tx.run_line("info")?);
            Ok(())
        }
        Cmd::Run { file, delay_ms } => cmd_run(&mut tx, &file, delay_ms),
        Cmd::Baud { rate } => {
            println!("{}", tx.run_line(&format!("baud {rate}"))?);
            Ok(())
        }
        Cmd::Usb { action } => {
            println!("{}", tx.run_line(&format!("usb {action}"))?);
            Ok(())
        }
        Cmd::Serve { .. } | Cmd::Stop => unreachable!("handled above"),
    }
}

fn require_device(cli: &Cli) -> Result<&str> {
    cli.device
        .as_deref()
        .ok_or_else(|| anyhow!("required argument '--device <DEVICE>' (-d) was not provided"))
}

/// One command line, executed either through a running daemon or directly on
/// the UART, depending on what owns the device.
enum Sender {
    /// A hid daemon owns this device; route commands through its HTTP API,
    /// presenting the token from its discovery file (absent for a daemon
    /// older than the token, which accepts anything).
    Daemon { base: String, token: Option<String> },
    /// No daemon for this device; we hold the UART ourselves.
    Direct { session: Session },
}

impl Sender {
    /// Choose the transport: if a hid daemon is running for `device`, route
    /// through it (it holds the port, so a direct open would fail anyway);
    /// otherwise open the UART directly.
    fn open(device: &str, baud: Option<u32>) -> Result<Sender> {
        if let Some(d) = daemon::discover() {
            if d.device == device {
                return Ok(Sender::Daemon {
                    base: format!("http://127.0.0.1:{}", d.port),
                    token: d.token,
                });
            }
        }
        Ok(Sender::Direct {
            session: Session::open(device, baud)?,
        })
    }

    /// Execute one command line, returning the `OK` reply data (empty for a
    /// bare `OK`, the capability/info string for `version`/`info`).
    fn run_line(&mut self, line: &str) -> Result<String> {
        match self {
            Sender::Daemon { base, token } => post_send(base, token.as_deref(), line),
            Sender::Direct { session } => execute_line(session, line),
        }
    }
}

/// POST one command line to a running daemon's `/send`; return the reply body.
///
/// The client waits the command's [`budget`] plus [`CLIENT_MARGIN`], so the
/// daemon normally answers (with its own timeout error) first. A timeout on
/// either side cancels the command in the daemon; see [`post_send_within`].
fn post_send(base: &str, token: Option<&str>, line: &str) -> Result<String> {
    post_send_within(
        base,
        token,
        line,
        budget(line) + CLIENT_MARGIN,
        CONFIRM_WAIT,
    )
}

/// Why one `/send` request failed.
enum PostFailure {
    /// The daemon answered with an `ERR`/transport message (HTTP 503).
    Reply(String),
    /// The request never reached the daemon (connect refused, bad address).
    NotSent(String),
    /// The request was sent but no answer came: our own timer ran out or the
    /// connection broke mid-request.
    Lost(String),
}

fn post_once(
    base: &str,
    token: Option<&str>,
    line: &str,
    timeout: Duration,
) -> std::result::Result<String, PostFailure> {
    let mut req = ureq::post(&format!("{base}/send")).timeout(timeout);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.send_string(line) {
        Ok(resp) => Ok(resp.into_string().unwrap_or_default().trim().to_string()),
        // A 503 carries the board's ERR / transport message in the body.
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(PostFailure::Reply(body.trim().to_string()))
        }
        Err(ureq::Error::Transport(t)) => {
            let msg = format!("hid daemon /send failed: {t}");
            match t.kind() {
                ureq::ErrorKind::Dns
                | ureq::ErrorKind::ConnectionFailed
                | ureq::ErrorKind::InvalidUrl
                | ureq::ErrorKind::UnknownScheme => Err(PostFailure::NotSent(msg)),
                _ => Err(PostFailure::Lost(msg)),
            }
        }
    }
}

/// [`post_send`] with explicit waits. When the daemon reports its own timeout,
/// or our `timeout` runs out, or the connection fails mid-request, the daemon
/// has dropped the command's reply channel and is cancelling it between HID
/// reports. A no-op `version` is then sent through the same queue: it never
/// touches the wire, but it is answered only after the earlier command has
/// finished or stopped, so its answer within `confirm` proves typing has
/// stopped. The outcome is a [`SendTimeout`] error (exit 40 or 41).
fn post_send_within(
    base: &str,
    token: Option<&str>,
    line: &str,
    timeout: Duration,
    confirm: Duration,
) -> Result<String> {
    let lost = match post_once(base, token, line, timeout) {
        Ok(body) => return Ok(body),
        Err(PostFailure::NotSent(msg)) => return Err(anyhow!(msg)),
        Err(PostFailure::Reply(msg)) if !msg.starts_with(uart::DAEMON_TIMEOUT_PREFIX) => {
            return Err(anyhow!("{msg}"))
        }
        Err(PostFailure::Reply(msg)) | Err(PostFailure::Lost(msg)) => msg,
    };
    eprintln!("{lost}; asking the daemon to confirm the command stopped");
    match post_once(base, token, "version", confirm) {
        Ok(_) => Err(SendTimeout::Stopped.into()),
        Err(_) => Err(SendTimeout::Unconfirmed.into()),
    }
}

/// Execute a single command and acknowledge with `OK` on stdout.
fn one(tx: &mut Sender, line: &str) -> Result<()> {
    tx.run_line(line)?;
    println!("OK");
    Ok(())
}

fn cmd_run(tx: &mut Sender, file: &str, delay_ms: u64) -> Result<()> {
    let text = if file == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow!("reading stdin: {e}"))?;
        buf
    } else {
        std::fs::read_to_string(file).map_err(|e| anyhow!("{file}: {e}"))?
    };
    let steps = parse_sequence(&text)?;
    let mut sent = 0usize;
    for step in steps {
        match step {
            Step::Delay(secs) => sleep(Duration::from_secs_f64(secs)),
            Step::Cmd(cmd) => {
                tx.run_line(&cmd)?;
                sent += 1;
                if delay_ms > 0 {
                    sleep(Duration::from_millis(delay_ms));
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
        };
        assert!(request_stop(&record).is_err());
        worker.join().unwrap();
        // No token at all (a pre-token daemon): refuse rather than signal.
        record.token = None;
        assert!(request_stop(&record).is_err());
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Read one HTTP request (headers and body) and return its body.
    fn read_request(sock: &mut TcpStream) -> String {
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        let mut byte = [0u8];
        while !buf.ends_with(b"\r\n\r\n") {
            sock.read_exact(&mut byte).unwrap();
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).unwrap();
        String::from_utf8(body).unwrap()
    }

    /// A fake daemon. The first `/send` (the long command) is read and then
    /// left unanswered. Later requests (the `version` confirmation) are
    /// answered `200 OK` when `confirms` is set and left unanswered otherwise.
    /// Returns the port and the bodies seen.
    fn fake_daemon(confirms: bool) -> (u16, Arc<std::sync::Mutex<Vec<String>>>, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (seen2, done2) = (seen.clone(), done.clone());
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let mut sock = sock.unwrap();
                let (seen, done) = (seen2.clone(), done2.clone());
                std::thread::spawn(move || {
                    let body = read_request(&mut sock);
                    let first = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(body);
                        seen.len() == 1
                    };
                    if !first && confirms {
                        let _ = sock.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\
                              Connection: close\r\n\r\nOK",
                        );
                    } else {
                        while !done.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                });
            }
        });
        (port, seen, done)
    }

    fn timeout_kind(r: Result<String>) -> SendTimeout {
        match r.unwrap_err().downcast::<SendTimeout>() {
            Ok(t) => t,
            Err(e) => panic!("not a SendTimeout: {e:?}"),
        }
    }

    /// #328: a timed-out `/send` asks the daemon to prove the command stopped;
    /// an answer to the queued `version` means it did (exit 40).
    #[test]
    fn a_timeout_the_daemon_confirms_is_reported_as_stopped() {
        let (port, seen, done) = fake_daemon(true);
        let base = format!("http://127.0.0.1:{port}");
        let r = post_send_within(
            &base,
            Some("t"),
            "type hello",
            Duration::from_millis(300),
            Duration::from_secs(2),
        );
        done.store(true, Ordering::Relaxed);
        let t = timeout_kind(r);
        assert_eq!(t, SendTimeout::Stopped);
        assert_eq!(t.exit_status(), 40);
        assert_eq!(*seen.lock().unwrap(), vec!["type hello", "version"]);
    }

    /// No answer to the confirmation means the daemon may still be typing
    /// (exit 41), which is a different status from a confirmed stop.
    #[test]
    fn a_timeout_the_daemon_cannot_confirm_is_reported_as_unconfirmed() {
        let (port, _seen, done) = fake_daemon(false);
        let base = format!("http://127.0.0.1:{port}");
        let r = post_send_within(
            &base,
            Some("t"),
            "type hello",
            Duration::from_millis(300),
            Duration::from_millis(300),
        );
        done.store(true, Ordering::Relaxed);
        let t = timeout_kind(r);
        assert_eq!(t, SendTimeout::Unconfirmed);
        assert_eq!(t.exit_status(), 41);
    }

    /// A daemon that is not there at all never saw the command: that is a
    /// plain error, not one of the timeout statuses.
    #[test]
    fn an_unreachable_daemon_is_not_a_timeout() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let r = post_send_within(
            &format!("http://127.0.0.1:{port}"),
            None,
            "key A",
            Duration::from_millis(300),
            Duration::from_millis(300),
        );
        assert!(r.unwrap_err().downcast_ref::<SendTimeout>().is_none());
    }
}
