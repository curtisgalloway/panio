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

//! Serial console runtime: `tio` for an interactive terminal, and the
//! `serialcap` daemon (which owns the port, captures a timestamped log, and
//! accepts input over localhost HTTP so input coexists with capture).
//!
//! Ported from the Python `_serial.py`. serialcap's discovery file is
//! `<runtime-base>/serialcap/<target>/daemon.json` (per-target, so multiple
//! targets' daemons coexist on one host — see daemons.rs), holding
//! `{pid, port, token, …}`; every request carries the token (see
//! `daemons::Endpoint`). An interface is passed to the daemon as
//! `NAME=DEVICE@BAUD[:SENSE]`.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::daemons;
use crate::model::SerialChannel;

pub const DAEMON: &str = "serialcap";
pub const DEFAULT_EVIDENCE_BYTES: u64 = 65536;
pub const MAX_EVIDENCE_BYTES: u64 = 1048576;

/// Read an offline receive journal through the helper. Never touches discovery
/// files, opens a device, or starts/restarts a daemon.
pub fn read_evidence(
    target: &str,
    interface: &str,
    epoch: Option<&str>,
    since: Option<u64>,
    max_bytes: u64,
) -> Result<serde_json::Value> {
    if since.is_some() && epoch.is_none() {
        bail!("--since requires --epoch");
    }
    if max_bytes == 0 || max_bytes > MAX_EVIDENCE_BYTES {
        bail!("--max-bytes must be 1..={MAX_EVIDENCE_BYTES}");
    }
    let binary = daemons::find_binary(DAEMON).ok_or_else(|| {
        crate::error::PanioloError::not_configured("serialcap not found — run `paniolo setup`")
    })?;
    let mut cmd = Command::new(binary);
    cmd.args([
        "evidence",
        "--interface",
        interface,
        "--max-bytes",
        &max_bytes.to_string(),
    ]);
    if let Some(epoch) = epoch {
        cmd.args(["--epoch", epoch]);
    }
    if let Some(since) = since {
        cmd.args(["--since", &since.to_string()]);
    }
    cmd.envs(daemons::helper_env(DAEMON, Some(target)));
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(crate::error::hook_failed(
            output.status,
            format!(
                "serialcap evidence failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        )
        .target(target)
        .channel("serial")
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Bounds and receive boundary for one read-only wait.
pub struct WaitOptions<'a> {
    pub epoch: &'a str,
    pub since: u64,
    pub timeout_ms: u64,
    pub max_bytes: u64,
    pub max_output_bytes: u64,
    pub pattern: Option<&'a str>,
    pub include_pending: bool,
    pub follow: bool,
}

pub fn wait_for_evidence(target: &str, interface: &str, options: WaitOptions<'_>) -> Result<()> {
    let binary =
        daemons::find_binary(DAEMON).ok_or_else(|| crate::error::helper_missing(DAEMON))?;
    let mut cmd = Command::new(binary);
    cmd.args([
        "wait",
        "--interface",
        interface,
        "--epoch",
        options.epoch,
        "--since",
        &options.since.to_string(),
        "--timeout-ms",
        &options.timeout_ms.to_string(),
        "--max-bytes",
        &options.max_bytes.to_string(),
        "--max-output-bytes",
        &options.max_output_bytes.to_string(),
    ]);
    if let Some(pattern) = options.pattern {
        cmd.args(["--pattern", pattern]);
    }
    if options.include_pending {
        cmd.arg("--include-pending");
    }
    if options.follow {
        cmd.arg("--follow");
    }
    let status = cmd
        .envs(daemons::helper_env(DAEMON, Some(target)))
        .stdin(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(
            crate::error::hook_failed(status, "serialcap wait failed".to_string())
                .target(target)
                .channel("serial")
                .into(),
        )
    }
}

/// Default daemon port: 0 = OS-assigned. The discovery file carries the actual
/// port and every consumer reads it, so a fixed default buys nothing and
/// collides with stale `ssh -L` dashboard tunnels squatting the old 8724.
pub const DEFAULT_PORT: u16 = 0;

/// The target's running serialcap daemon — port and token — or None if it
/// isn't running.
pub fn daemon(target: &str) -> Option<daemons::Endpoint> {
    daemons::daemon_endpoint(DAEMON, Some(target))
}

/// Base URL of the target's running serialcap daemon, or None if it isn't
/// running. For printing; API calls go through [`daemon`].
pub fn daemon_url(target: &str) -> Option<String> {
    daemons::daemon_url(DAEMON, Some(target))
}

// ── daemon control ──────────────────────────────────────────────────────────

/// The serialcap daemon holding any of `ifaces`' ports that no discovery file
/// accounts for, if there is one — an orphan left behind when the runtime file
/// that named it was deleted out from under it (see the "untracked daemons"
/// note in daemons.rs). A serial port cannot be opened twice, so it has to be
/// reaped before a replacement can start.
pub fn untracked(ifaces: &[SerialChannel]) -> Option<daemons::Untracked> {
    let devices: Vec<String> = ifaces.iter().map(|ch| ch.device.clone()).collect();
    daemons::untracked_on_devices(DAEMON, &devices)
        .into_iter()
        .next()
}

/// Format one interface for the daemon's repeatable `--interface` flag:
/// `NAME=DEVICE@BAUD[:SENSE]`.
pub fn interface_arg(ch: &SerialChannel) -> String {
    let mut arg = format!("{}={}@{}", ch.name, ch.device, ch.baud);
    if let Some(sense) = &ch.power_sense_signal {
        arg.push(':');
        arg.push_str(sense);
    }
    arg
}

/// Start the target's serialcap daemon (owning every given interface),
/// detached. The caller polls [`daemon_url`] for readiness.
pub fn start_daemon(
    ifaces: &[SerialChannel],
    port: u16,
    target: &str,
) -> Result<std::process::Child> {
    let binary = daemons::find_binary(DAEMON).ok_or_else(|| {
        crate::error::PanioloError::not_configured(
            "serialcap not found (libexec or PATH) — run `paniolo setup`".to_string(),
        )
    })?;
    let mut cmd = Command::new(&binary);
    cmd.arg("daemon").arg("--port").arg(port.to_string());
    cmd.envs(daemons::helper_env(DAEMON, Some(target)));
    for ch in ifaces {
        cmd.arg("--interface").arg(interface_arg(ch));
    }
    // Capture stderr (tracing output) so a startup failure is diagnosable;
    // daemons::start_failure() reads the tail on timeout.
    let log = daemons::create_log(DAEMON, Some(target))?;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    // Detach into its own process group so it survives this CLI exiting.
    crate::platform::detach(&mut cmd);
    let child = cmd.spawn()?;
    // Stamp actual running content after successful exec, not just its path.
    daemons::record_binmeta(&binary, DAEMON, Some(target), child.id());
    Ok(child)
}

/// Stop the target's running daemon via `serialcap stop` (it owns the clean
/// shutdown). The per-target `helper_env` points `serialcap stop` at the right
/// instance's discovery file.
pub fn stop_daemon(target: &str) -> Result<std::process::ExitStatus> {
    let binary =
        daemons::find_binary(DAEMON).ok_or_else(|| crate::error::helper_missing("serialcap"))?;
    let status = Command::new(binary)
        .arg("stop")
        .envs(daemons::helper_env(DAEMON, Some(target)))
        .status()?;
    Ok(status)
}

// ── input ───────────────────────────────────────────────────────────────────

/// POST raw bytes to the serial port the daemon owns; input coexists with
/// capture. `pace_ms > 0` drips bytes one at a time (the substitute for flow
/// control on slow polled consoles), so the timeout is scaled to match.
pub fn send_input(
    daemon: &daemons::Endpoint,
    interface: &str,
    data: &[u8],
    pace_ms: u32,
) -> Result<()> {
    // `interface` is a name from the lab file — percent-encode it so one
    // containing `&` or `=` can't reshape the query string it's spliced into
    // (Review low #3).
    let mut path = format!("/input?interface={}", daemons::query_escape(interface));
    if pace_ms > 0 {
        path.push_str(&format!("&pace_ms={pace_ms}"));
    }
    let timeout_ms = std::cmp::max(15_000, data.len() as u64 * pace_ms as u64 + 10_000);
    daemon
        .post(&path)
        .timeout(Duration::from_millis(timeout_ms))
        .send_bytes(data)
        .map(|_| ())
        .map_err(|e| crate::error::daemon_request_failed(DAEMON, "serialcap /input", e).into())
}

// ── interactive console ─────────────────────────────────────────────────────

/// Replace this process with `tio` on the given device (never returns on
/// success).
pub fn exec_tio(device: &str, baud: i64) -> Result<()> {
    let tio = daemons::find_binary("tio").ok_or_else(|| {
        crate::error::PanioloError::not_configured(
            "tio not found in PATH — install it (e.g. brew install tio)".to_string(),
        )
    })?;
    let err = crate::platform::exec_replace(
        Command::new(tio)
            .arg("--baudrate")
            .arg(baud.to_string())
            .arg(device),
    );
    bail!("exec tio failed: {err}")
}

// ── device listing ──────────────────────────────────────────────────────────

/// Available serial device paths on this platform. On Linux, one entry per
/// physical port, named by its stable /dev/serial symlink — by-id preferred
/// (names the adapter; what lab files typically use), by-path as the fallback
/// (port-derived; the only stable name for adapters without a serial number).
/// On Windows the OS assigns `COM<n>` names and there is no stable by-path
/// analogue, so the enumeration comes from the `serialport` crate as-is.
pub fn list_devices() -> Vec<String> {
    let mut out = enumerate_devices();
    out.sort();
    out
}

/// Windows enumeration: whatever the OS reports as a serial port.
///
/// COM numbers are assigned by the OS and can move when an adapter is
/// re-enumerated, so a lab file pinned to `COM7` is not as stable as a Linux
/// by-id path. There is no better handle to offer.
#[cfg(windows)]
fn enumerate_devices() -> Vec<String> {
    serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn enumerate_devices() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if cfg!(target_os = "macos") {
        if let Ok(rd) = std::fs::read_dir("/dev") {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("tty.usbserial-") || n.starts_with("tty.usbmodem") {
                    out.push(format!("/dev/{n}"));
                }
            }
        }
    } else {
        // Group the /dev/serial symlinks by the tty they resolve to, so each
        // physical port lists once even though udev usually gives it several
        // aliases (a by-id name, plus by-path `usb`/`usbv2` twins).
        let mut by_tty: std::collections::BTreeMap<std::path::PathBuf, Vec<String>> =
            Default::default();
        for dir in ["/dev/serial/by-id", "/dev/serial/by-path"] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let path = e.path();
                    let tty = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    by_tty
                        .entry(tty)
                        .or_default()
                        .push(path.display().to_string());
                }
            }
        }
        out.extend(by_tty.values().filter_map(|a| preferred_alias(a).cloned()));
        if out.is_empty() {
            if let Ok(rd) = std::fs::read_dir("/dev") {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("ttyUSB") || n.starts_with("ttyACM") {
                        out.push(format!("/dev/{n}"));
                    }
                }
            }
        }
    }
    out
}

/// The display name for one physical port's symlink aliases: the by-id name
/// when the adapter has one, else the first (sorted) alias — which keeps the
/// plain `usb` by-path variant ahead of its `usbv2` twin.
#[cfg(not(windows))]
fn preferred_alias(aliases: &[String]) -> Option<&String> {
    aliases
        .iter()
        .find(|a| a.contains("/by-id/"))
        .or_else(|| aliases.iter().min())
}

/// Read-only health collection. Never starts, reclaims, or retries a daemon.
pub fn health_snapshot(
    target: &str,
    channels: &[SerialChannel],
    interface: Option<&str>,
) -> Result<serde_json::Value> {
    let configured: Vec<_> = channels
        .iter()
        .filter(|c| interface.is_none_or(|i| c.name == i))
        .map(|c| serde_json::json!({"name": c.name, "device": c.device, "baud": c.baud, "host": c.host}))
        .collect();
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut snapshot = serde_json::json!({
        "target": target, "observed_at_ms": observed_at_ms,
        "configured": configured, "process_live": false,
        "daemon_state": "stopped", "pid": null, "active": null,
        "command_execution": "not_assessed"
    });
    if let Some(endpoint) = daemon(target) {
        snapshot["process_live"] = true.into();
        snapshot["pid"] = endpoint.pid.into();
        let path = interface.map_or_else(
            || "/status".into(),
            |i| format!("/status?interface={}", daemons::query_escape(i)),
        );
        match endpoint.get(&path).timeout(Duration::from_secs(5)).call() {
            Ok(response) => {
                let active: serde_json::Value = response
                    .into_json()
                    .map_err(|e| anyhow::anyhow!("invalid serial health response: {e}"))?;
                snapshot["active"] = active;
                snapshot["daemon_state"] = "reachable".into();
            }
            Err(e) => {
                snapshot["daemon_state"] = "unreachable".into();
                snapshot["diagnostic"] = e.to_string().into();
            }
        }
    } else if let Some(process) = untracked(channels) {
        snapshot["process_live"] = true.into();
        snapshot["pid"] = process.pid.into();
        snapshot["daemon_state"] = "untracked".into();
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(name: &str, sense: Option<&str>) -> SerialChannel {
        SerialChannel {
            name: name.into(),
            device: "/dev/ttyUSB0".into(),
            baud: 115200,
            power_sense_signal: sense.map(String::from),
            power_button: false,
            host: None,
        }
    }

    #[test]
    fn interface_arg_formats_name_device_baud() {
        assert_eq!(
            interface_arg(&ch("console", None)),
            "console=/dev/ttyUSB0@115200"
        );
    }

    #[test]
    fn interface_arg_appends_sense() {
        assert_eq!(
            interface_arg(&ch("console", Some("cts"))),
            "console=/dev/ttyUSB0@115200:cts"
        );
    }

    /// The interface name reaches `/input` percent-encoded — checked against
    /// a real loopback listener, not by inspecting the formatted string
    /// (Review low #3): unescaped, `a b&c` would reshape the query string
    /// into extra, wrong parameters.
    #[test]
    fn send_input_percent_encodes_the_interface() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = crate::stubhttp::serve_one(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let ep = daemons::Endpoint {
            pid: 1,
            port,
            token: None,
        };
        send_input(&ep, "a b&c", b"hi", 0).unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("POST /input?interface=a%20b%26c HTTP/1.1"),
            "{req}"
        );
    }

    // `preferred_alias` serves the /dev/serial enumeration, which does not
    // exist on Windows.
    #[cfg(not(windows))]
    #[test]
    fn preferred_alias_picks_by_id_over_by_path() {
        let aliases = vec![
            "/dev/serial/by-path/platform-xhci-hcd.1-usb-0:1.2:1.0-port0".to_string(),
            "/dev/serial/by-id/usb-1a86_USB2.0-Serial-if00-port0".to_string(),
            "/dev/serial/by-path/platform-xhci-hcd.1-usbv2-0:1.2:1.0-port0".to_string(),
        ];
        assert_eq!(
            preferred_alias(&aliases).unwrap(),
            "/dev/serial/by-id/usb-1a86_USB2.0-Serial-if00-port0"
        );
    }

    // `preferred_alias` serves the /dev/serial enumeration, which does not
    // exist on Windows.
    #[cfg(not(windows))]
    #[test]
    fn preferred_alias_falls_back_to_first_sorted_by_path() {
        // No by-id (adapter without a serial number): the plain `usb` by-path
        // variant sorts ahead of its `usbv2` twin and wins deterministically.
        let aliases = vec![
            "/dev/serial/by-path/platform-xhci-hcd.1-usbv2-0:1.2:1.0-port0".to_string(),
            "/dev/serial/by-path/platform-xhci-hcd.1-usb-0:1.2:1.0-port0".to_string(),
        ];
        assert_eq!(
            preferred_alias(&aliases).unwrap(),
            "/dev/serial/by-path/platform-xhci-hcd.1-usb-0:1.2:1.0-port0"
        );
        assert_eq!(preferred_alias(&[]), None);
    }
}
