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

//! hdmicap — warm-stream HDMI capture daemon + thin client CLI.
//!
//! Subcommands:
//!   daemon   run the capture daemon (the controller starts this)
//!   devices  list capture devices
//!   shot     fetch one PNG from a running daemon (--stable, --out)
//!   watch    block until the screen changes, then print the new hash
//!   stop     ask the running daemon to exit (authenticated HTTP, never a PID signal)
//!   preview  print the URL to open in a browser

mod auth;
mod capture;
mod capture_thread;
mod daemon;
mod demand;
mod frame;
mod hid_link;
mod keysym;
// Pixel formats and conversions exist to serve a capture backend. On a
// platform with none compiled in (Windows), nothing constructs them — that is
// the design, not dead weight.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
mod pixel;
// The file is kept byte-identical across the crates that copy it, so a crate
// may not use every primitive in it.
#[allow(dead_code)]
mod platform;
mod rfb;
mod rfb_serve;
mod server;
mod zrle;

use std::io::{Read, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use capture::DeviceSpec;

#[derive(Parser)]
#[command(name = "hdmicap", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the capture daemon (foreground; controller manages the process).
    Daemon {
        /// Device: "auto" (default), an index, a name substring, or a network
        /// RFB source: `rfb+ws://127.0.0.1:PORT/rfb` (RFB over WebSocket; the
        /// bearer token comes from env HDMICAP_RFB_TOKEN, never argv),
        /// `rfb://HOST:PORT` (plain TCP, security None only), or
        /// `rfb+discovery:` (a hid daemon's /rfb, located through the
        /// discovery file named by env HDMICAP_RFB_DISCOVERY and re-read on
        /// every reconnect).
        #[arg(long, default_value = "auto")]
        device: String,
        /// Port to bind on localhost. 0 = OS-assigned.
        #[arg(long, default_value_t = 8723)]
        port: u16,
        /// How far (0-255) any one pixel's luma must move from the last
        /// accepted frame before the frame hash changes. Smaller movements
        /// are treated as capture noise (lossy H.264 sources re-render an
        /// unchanged screen slightly differently each frame; measured noise
        /// was at most 8 levels). 0 = exact: any differing pixel changes
        /// the hash.
        #[arg(long, default_value_t = frame::DEFAULT_CHANGE_THRESHOLD)]
        change_threshold: u8,
    },
    /// List available capture devices and exit (no daemon needed).
    Devices {
        /// Emit machine-readable JSON instead of the human table.
        #[arg(long)]
        json: bool,
        /// Include internal platform video nodes (SoC pipeline stages, codecs)
        /// normally hidden from the listing.
        #[arg(long)]
        all: bool,
    },
    /// Fetch one screenshot from the running daemon.
    Shot {
        /// Wait until the signal is Stable before capturing.
        #[arg(long)]
        stable: bool,
        /// Only return once the frame differs from this hex hash.
        #[arg(long)]
        changed_since: Option<String>,
        /// Timeout in ms.
        #[arg(long, default_value_t = 2000)]
        timeout: u64,
        /// Output path; "-" for stdout.
        #[arg(long, default_value = "-")]
        out: String,
    },
    /// Block until the screen changes vs the current frame; print the new hash.
    Watch {
        #[arg(long, default_value_t = 30000)]
        timeout: u64,
    },
    /// Print the preview URL (open in a browser).
    Preview,
    /// Ask the running daemon to shut down using its authentication token.
    ///
    /// Older daemons without the shutdown endpoint must be stopped with
    /// `panio daemons stop hdmicap` before starting the updated daemon.
    Stop,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hdmicap=info".into()),
        )
        // Log to stderr: the panio CLI redirects a daemon's stderr into its
        // daemon.log and discards stdout, so logging to stdout left every managed
        // daemon's log file empty.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon {
            device,
            port,
            change_threshold,
        } => daemon::run(DeviceSpec::parse(&device), port, change_threshold),
        Cmd::Devices { json, all } => cmd_devices(json, all),
        Cmd::Shot {
            stable,
            changed_since,
            timeout,
            out,
        } => cmd_shot(stable, changed_since, timeout, &out),
        Cmd::Watch { timeout } => cmd_watch(timeout),
        Cmd::Preview => cmd_preview(),
        Cmd::Stop => cmd_stop(),
    }
}

/// The running daemon as a client sees it: its base URL and the bearer token
/// its API requires (absent for a daemon older than the token).
struct Client {
    base: String,
    token: Option<String>,
}

impl Client {
    fn discover() -> Result<Client> {
        let d = daemon::discover()?;
        Ok(Client {
            base: format!("http://127.0.0.1:{}", d.port),
            token: d.token,
        })
    }

    /// A GET of `path` (which may carry a query) with the token attached.
    fn get(&self, path: &str) -> ureq::Request {
        let req = ureq::get(&format!("{}{path}", self.base));
        match &self.token {
            Some(t) => req.set("Authorization", &format!("Bearer {t}")),
            None => req,
        }
    }

    /// `path` as a URL a browser can open: the token rides as `?token=`.
    fn browser_url(&self, path: &str) -> String {
        match &self.token {
            Some(t) => format!("{}{path}?token={t}", self.base),
            None => format!("{}{path}", self.base),
        }
    }
}

fn cmd_devices(json: bool, all: bool) -> Result<()> {
    // The default listing hides nodes that can't capture (Linux: SoC pipeline
    // stages, codecs, UVC metadata nodes — filtered by V4L2 device caps in
    // capture::enumerate_capture); --all shows the raw enumeration.
    let devices = if all {
        capture::enumerate()?
    } else {
        capture::enumerate_capture()?
    };
    if json {
        let list: Vec<serde_json::Value> = devices
            .iter()
            .map(|d| {
                serde_json::json!({"index": d.index, "name": d.name, "misc": d.misc, "id": d.id})
            })
            .collect();
        println!("{}", serde_json::to_string(&list)?);
        return Ok(());
    }
    for d in devices {
        println!("{:>3}  {}  [{}]  id={}", d.index, d.name, d.misc, d.id);
    }
    Ok(())
}

fn cmd_shot(stable: bool, changed_since: Option<String>, timeout: u64, out: &str) -> Result<()> {
    let client = Client::discover().context("is the daemon running? try `hdmicap daemon`")?;
    let mut snap_path = format!("/snapshot?timeout={timeout}");
    if stable {
        snap_path.push_str("&wait=stable");
    }
    if let Some(ref hash) = changed_since {
        snap_path.push_str(&format!("&changed_since={hash}"));
    }

    let resp = client
        .get(&snap_path)
        .call()
        .context("GET /snapshot failed")?;

    let timed_out = resp.header("x-timeout") == Some("1");
    let signal = resp.header("x-signal").unwrap_or("unknown").to_string();
    let hash = resp.header("x-frame-hash").unwrap_or("").to_string();
    eprintln!(
        "signal={}  hash={}{}",
        signal,
        hash,
        if timed_out { "  (timeout)" } else { "" }
    );

    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body)?;

    if out == "-" {
        std::io::stdout().write_all(&body)?;
    } else {
        std::fs::write(out, &body).with_context(|| format!("writing {out}"))?;
        eprintln!("wrote {} bytes to {out}", body.len());
    }
    Ok(())
}

fn cmd_watch(timeout: u64) -> Result<()> {
    let client = Client::discover().context("is the daemon running? try `hdmicap daemon`")?;

    // Read the current hash so we can long-poll for a change.
    let body = client
        .get("/status")
        .call()
        .context("GET /status failed")?
        .into_string()
        .context("reading /status body")?;
    let status: serde_json::Value = serde_json::from_str(&body).context("parsing /status JSON")?;
    let hash = status["hash"]
        .as_str()
        .unwrap_or("0000000000000000")
        .to_string();

    // Block until the frame changes or we time out.
    let resp = client
        .get(&format!("/snapshot?changed_since={hash}&timeout={timeout}"))
        .call()
        .context("GET /snapshot (changed_since) failed")?;

    let new_hash = resp.header("x-frame-hash").unwrap_or("").to_string();
    let timed_out = resp.header("x-timeout") == Some("1");
    if timed_out {
        anyhow::bail!("timed out waiting for screen change after {timeout}ms");
    }
    println!("{new_hash}");
    Ok(())
}

fn cmd_preview() -> Result<()> {
    let client = Client::discover().context("is the daemon running? try `hdmicap daemon`")?;
    println!(
        "Open {} in a browser to watch the screen.",
        client.browser_url("/preview")
    );
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let d = daemon::discover().context("is the daemon running?")?;
    request_stop(&d)?;
    println!("daemon (pid {}) stopping", d.pid);
    Ok(())
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
    let token = d.token.as_deref().filter(|t| !t.is_empty()).context(
        "daemon has no token; use `panio daemons stop hdmicap` to stop the older daemon",
    )?;
    ureq::post(&format!("http://127.0.0.1:{}/stop", d.port))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(std::time::Duration::from_secs(5))
        .send_bytes(&[])
        .context(
            "authenticated shutdown failed; for an older daemon use `panio daemons stop hdmicap`",
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
        };
        assert!(request_stop(&record).is_err());
        worker.join().unwrap();
        // No token at all (a pre-token daemon): refuse rather than signal.
        record.token = None;
        assert!(request_stop(&record).is_err());
    }
}
