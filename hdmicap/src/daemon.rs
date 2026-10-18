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

//! Daemon lifecycle: advisory lock, discovery file, runtime wiring, shutdown.

use std::fs::{self, File};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::capture::DeviceSpec;
use crate::capture_thread;
use crate::server::{self, AppState};

#[derive(Serialize, Deserialize)]
pub struct Discovery {
    pub pid: u32,
    pub port: u16,
    /// The bearer token every request to this daemon must carry (see
    /// auth.rs). Optional on read so a file written by an older daemon still
    /// parses; always written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// The daemon's runtime dir. Panio passes the canonical location as
/// `PANIO_RUNTIME_DIR` (the helper state/runtime-dir API in the CLI's
/// daemons.rs — the single source of truth; it also adds a per-target segment
/// so multiple targets' daemons coexist). The fallback below is for standalone
/// invocations: `<base>/panio-<uid>/hdmicap`, where `<base>` honors
/// `$PANIO_RUNTIME_BASE` (default `/tmp`), identical in every environment of
/// the same user. Deliberately NOT `$TMPDIR`/`temp_dir()` (macOS hands each
/// environment a different TMPDIR — GUI terminal vs SSH vs sandboxed agent
/// shells — so a running daemon was invisible from the others) and NOT
/// `$XDG_RUNTIME_DIR` (systemd removes `/run/user/<uid>` when the user's
/// last session ends, breaking daemons that outlive the SSH session that
/// started them). Keep in sync with daemons.rs `runtime_root`/`runtime_base`.
fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PANIO_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir)?;
        return Ok(dir);
    }
    let uid = crate::platform::current_uid();
    let base = crate::platform::runtime_root().join(format!("panio-{uid}"));
    crate::platform::ensure_private_dir(&base)?;
    let dir = base.join("hdmicap");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn lock_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.lock"))
}

fn discovery_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.json"))
}

/// Read the discovery file so the CLI knows which port to hit.
pub fn discover() -> Result<Discovery> {
    let p = discovery_path()?;
    let s = fs::read_to_string(&p).with_context(|| format!("daemon not running? {p:?}"))?;
    let d: Discovery = serde_json::from_str(&s)?;
    // Liveness, as ch9329 and hidrig already do. A discovery file outlives the
    // process that wrote it whenever the daemon did not exit gracefully — a
    // crash or SIGKILL on Unix, and *every* stop on Windows, where
    // `TerminateProcess` gives the daemon no chance to clean up. Without this
    // check the next command dials a dead port and reports a connection
    // refusal instead of "the daemon is not running".
    if !crate::platform::pid_alive(d.pid as i32) {
        let _ = fs::remove_file(&p);
        return Err(anyhow!("daemon not running (stale {p:?} removed)"));
    }
    Ok(d)
}

/// Blocking entry point for `hdmicap daemon`. Builds the tokio runtime itself
/// so the capture thread can stay a plain std::thread alongside it.
pub fn run(device: DeviceSpec, port: u16, change_threshold: u8) -> Result<()> {
    // A malformed network source is an operator error: refuse now rather than
    // retry it forever as an unplugged device.
    if let DeviceSpec::Rfb(s) = &device {
        crate::rfb::RfbTarget::parse(s)?;
    }
    // 1. Acquire the advisory lock. Held for the lifetime of the process.
    let lock_file = File::create(lock_path()?)?;
    lock_file
        .try_lock()
        .map_err(|_| anyhow!("another hdmicap daemon is already running"))?;

    // 2. Spawn the capture thread BEFORE the runtime. It owns the device and
    //    publishes into the watch channel.
    let demand = crate::demand::Demand::new();
    let (frames, _capture_handle) = capture_thread::spawn(device, demand.clone(), change_threshold);

    // 3. Build a multi-thread runtime for axum and run the server.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;

        // 4. Publish discovery info now that we have the real port. Every
        //    request must present the token (auth.rs); it reaches clients only
        //    through the owner-only discovery file.
        let token = crate::auth::generate_token()?;
        let disc = Discovery {
            pid: std::process::id(),
            port: bound.port(),
            token: Some(token.clone()),
        };
        crate::auth::write_private_file(
            &discovery_path()?,
            serde_json::to_string(&disc)?.as_bytes(),
        )
        .context("writing discovery file")?;
        info!("hdmicap daemon listening on http://{bound}");

        // `POST /stop` wakes this; `hdmicap stop` never signals the PID in
        // the discovery file, which a crash can leave pointing at whatever
        // process the kernel next gave that number to.
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        // The noVNC view types and points through the target's hid daemon
        // when the CLI says where it keeps its discovery file.
        let hid = std::env::var_os(crate::hid_link::ENV_HID_DISCOVERY)
            .filter(|p| !p.is_empty())
            .map(|p| crate::hid_link::HidLink::spawn(p.into()));
        let name = std::env::var("PANIO_TARGET")
            .ok()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "hdmicap".into());
        let app = server::router(
            AppState::new(frames)
                .with_demand(demand)
                .with_rfb(&name, hid)
                .with_change_threshold(change_threshold),
            crate::auth::Auth::new(token, server::PUBLIC_ASSETS),
        )
        .layer(axum::Extension(shutdown.clone()));

        // 5. Serve until SIGTERM/SIGINT or an authenticated `POST /stop`. The
        //    /preview MJPEG stream is an infinite response, so a plain graceful
        //    shutdown would block on it forever. Remove the discovery file, give
        //    short in-flight requests a brief grace period, then hard-exit (the
        //    OS releases the device).
        //
        //    The lock file itself is deliberately NOT unlinked here: `lock_file`
        //    (above) holds an OS advisory lock (flock) on it, and this process
        //    exits before ever reaching `drop(lock_file)`. Unlinking the path
        //    while the lock is still held replaces the directory entry with a
        //    fresh inode the moment the next daemon starts — that daemon's
        //    `try_lock` succeeds against the NEW inode even while this
        //    process (and its lock on the OLD, now-unlinked inode) is still
        //    alive, so two daemons could hold the device at once. Leaving the
        //    file in place means the next daemon's `File::create` reopens the
        //    SAME inode, and its lock attempt correctly waits on this process's
        //    exit (which releases the OS-level lock).
        let disc = discovery_path()?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                tokio::select! {
                    _ = shutdown_signal() => {},
                    _ = shutdown.notified() => info!("stop requested over HTTP"),
                }
                let _ = fs::remove_file(&disc);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                info!("daemon shut down");
                std::process::exit(0);
            })
            .await?;

        Ok::<(), anyhow::Error>(())
    })?;

    drop(lock_file);
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let term = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A discovery file outlives its process whenever the daemon did not exit
    /// gracefully: a crash or SIGKILL on Unix, and every stop on Windows,
    /// where `TerminateProcess` runs no cleanup. Reading one back without
    /// checking liveness makes the next command dial a dead port and report a
    /// connection refusal instead of "not running" — which is exactly what
    /// happened on Windows.
    #[test]
    fn discover_rejects_and_reaps_a_stale_record() {
        let dir = std::env::temp_dir().join(format!("panio-disc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Safe: this test owns the variable; it is restored below.
        let prev = std::env::var_os("PANIO_RUNTIME_DIR");
        unsafe { std::env::set_var("PANIO_RUNTIME_DIR", &dir) };

        // A pid that cannot be running: 0 is never a real process, and the
        // liveness guard rejects non-positive pids outright.
        let path = dir.join("daemon.json");
        std::fs::write(&path, br#"{"pid":0,"port":8723}"#).unwrap();

        let got = discover();

        match prev {
            Some(v) => unsafe { std::env::set_var("PANIO_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("PANIO_RUNTIME_DIR") },
        }

        assert!(got.is_err(), "a dead pid must not look like a live daemon");
        assert!(
            !path.exists(),
            "the stale file must be reaped, not left to fail again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The discovery file now carries the token. One written by an older
    /// daemon (no token) must still parse, so the CLI can see it is running
    /// and tell the operator to restart it.
    #[test]
    fn discovery_token_is_optional_on_read_and_written_when_present() {
        let old: Discovery = serde_json::from_str(r#"{"pid":1,"port":2}"#).unwrap();
        assert_eq!(old.token, None);
        let new = Discovery {
            pid: 1,
            port: 2,
            token: Some("ab".into()),
        };
        let text = serde_json::to_string(&new).unwrap();
        assert!(text.contains(r#""token":"ab""#), "{text}");
        let back: Discovery = serde_json::from_str(&text).unwrap();
        assert_eq!(back.token.as_deref(), Some("ab"));
    }
}
