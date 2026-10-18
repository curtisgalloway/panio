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

//! Netboot lifecycle — start/stop/status of the `netbootd` daemon (DHCP + TFTP
//! over the dedicated USB-Ethernet link).
//!
//! Ported from `_netboot.py`, rust engine only (the legacy pure-Python
//! DHCP/TFTP engine stays behind in the Python tree). On macOS netbootd runs
//! unprivileged (its raw-frame send path uses the setuid bpf-helper installed
//! beside it); on Linux ports 67/69 need root so the spawn gets a sudo prefix
//! (netbootd drops that root itself once its sockets are bound).
//!
//! `start` does not trust the spawn alone: netbootd validates its
//! configuration and binds every listener before it serves, and any of that
//! failing (a port in use, an interface it cannot pin, a bad client IP) is an
//! early exit — so `start` watches the child for [`STARTUP_GRACE`] and, if it
//! dies, reports the tail of its log instead of writing a state file for a
//! daemon that is not there. It also keeps one netboot per interface: a
//! second target starting on an interface another target's netbootd already
//! serves would only fight it for the port. The complementary rule — one
//! /24 per interface, so a second *link* never shares the first's subnet —
//! is enforced where the address is assigned (`netif::configure_interface`).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::daemons;
use crate::netif;
use crate::state::{self, NetbootState};

/// How long `start` watches a freshly spawned netbootd for an early exit
/// before declaring it up. Binding, pinning and the primary-NIC lookup take
/// well under a second; the margin covers a loaded host.
const STARTUP_GRACE: Duration = Duration::from_secs(2);
/// Poll interval while watching the child during [`STARTUP_GRACE`].
const STARTUP_POLL: Duration = Duration::from_millis(100);
/// How much of the daemon's log a startup failure quotes.
const LOG_TAIL_LINES: usize = 20;

fn resolve_netbootd() -> Result<std::path::PathBuf> {
    Ok(daemons::find_binary("netbootd").ok_or_else(|| {
        crate::error::PanioError::not_configured(
            "netbootd not found — build and install it with `panio setup`",
        )
    })?)
}

/// The other target (name and state) whose live netbootd already owns
/// `interface`, if any. One netbootd per interface: the second would just
/// fail to bind (or, worse, share) the DHCP/TFTP ports on the same link.
fn interface_conflict<'a>(
    target: &str,
    interface: &str,
    running: &'a [(String, NetbootState)],
) -> Option<&'a (String, NetbootState)> {
    running
        .iter()
        .find(|(other, st)| other != target && st.interface == interface)
}

/// The last `lines` lines of the file at `path` (all of it if shorter; empty
/// if unreadable), for quoting a failed daemon's log in an error.
fn log_tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Optional UEFI boot parameters forwarded to `netbootd` as flags. All default
/// inside `netbootd` when unset (boot_file → `kernel_2712.img`, http_port → 80,
/// content_type → `application/octet-stream`).
#[derive(Default, Clone)]
pub struct BootOptions {
    pub boot_file: Option<String>,
    pub http_port: Option<String>,
    pub content_type: Option<String>,
}

/// Kill any lingering netbootd from a previous crashed session for `target`.
fn cleanup_stale(target: &str) {
    if let Some(s) = state::load_netboot_state(target) {
        if s.engine == "rust" && state::is_named_child_alive(s.dhcp_pid, "netbootd") {
            crate::platform::signal_pid(s.dhcp_pid, crate::platform::Signal::Term);
        }
    }
    state::remove_netboot_state(target);
}

/// Start netbootd for `target` on `interface`, serving `tftp_root` at `host_ip`.
pub fn start(
    target: &str,
    interface: &str,
    host_ip: &str,
    tftp_root: &str,
    opts: &BootOptions,
) -> Result<()> {
    if state::is_netboot_running(target) {
        bail!("netboot already running for '{target}'");
    }
    if tftp_root.is_empty() {
        bail!("no tftp_root configured (panio netboot set -t {target} --tftp-root <path>)");
    }
    if !Path::new(tftp_root).exists() {
        bail!("TFTP root does not exist: {tftp_root}");
    }
    if netif::is_primary_interface(interface) {
        bail!(
            "refusing to start netboot on '{interface}': it carries the system default \
             route (your primary network interface). netboot reconfigures it to \
             {host_ip} and would break host networking. Use a dedicated USB-Ethernet \
             adapter for the netboot link."
        );
    }
    if let Some((other, st)) = interface_conflict(target, interface, &state::running_netboots()) {
        bail!(
            "netboot for '{other}' is already running on {interface} (pid {}); one netboot \
             per interface — stop it first (panio netboot stop {other}) or give \
             '{target}' its own adapter",
            st.dhcp_pid
        );
    }

    cleanup_stale(target);
    netif::configure_interface(interface, host_ip)?;
    netif::tune_arp_for_silent_client();

    state::ensure_target_dir(target)?;
    let log_path = state::netboot_log_path(target);
    let _ = std::fs::remove_file(&log_path);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let netbootd = resolve_netbootd()?;
    // Linux needs root for ports 67/69; sudo resets the env, so NO_COLOR rides
    // through `env` in the prefix. macOS runs unprivileged (bpf-helper).
    let needs_sudo = cfg!(target_os = "linux") && !crate::platform::is_superuser();
    let mut cmd = if !needs_sudo {
        let mut c = Command::new(&netbootd);
        c.env("NO_COLOR", "1");
        c
    } else {
        let mut c = Command::new("sudo");
        c.arg("env").arg("NO_COLOR=1").arg(&netbootd);
        c
    };
    cmd.arg("--host-ip")
        .arg(host_ip)
        .arg("--tftp-root")
        .arg(tftp_root)
        .arg("--interface")
        .arg(interface);
    // Optional UEFI boot params; netbootd defaults each when the flag is absent.
    if let Some(bf) = &opts.boot_file {
        cmd.arg("--boot-file").arg(bf);
    }
    if let Some(p) = &opts.http_port {
        cmd.arg("--http-port").arg(p);
    }
    if let Some(ct) = &opts.content_type {
        cmd.arg("--content-type").arg(ct);
    }
    cmd.stdin(Stdio::null()).stdout(log).stderr(log_err);
    crate::platform::detach(&mut cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;

    // Watch for an early exit before recording the daemon as running. Two
    // probes: `try_wait` reaps a child that has exited (a zombie still answers
    // `kill -0`, so a liveness probe alone could miss a crash on macOS), and
    // the named-process check catches the Linux case where the pid we hold is
    // `sudo`'s while netbootd underneath it has already gone.
    let deadline = Instant::now() + STARTUP_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(STARTUP_POLL);
        let exited = match child.try_wait() {
            Ok(Some(status)) => Some(format!("exited with {status}")),
            Ok(None) if !state::is_named_child_alive(pid, "netbootd") => {
                Some("is no longer running".to_string())
            }
            _ => None,
        };
        if let Some(how) = exited {
            let tail = log_tail(&log_path, LOG_TAIL_LINES);
            bail!(
                "netbootd {how} during startup; last lines of {}:\n{tail}",
                log_path.display()
            );
        }
    }

    state::save_netboot_state(&NetbootState {
        target: target.to_string(),
        // Single process; both pid fields hold the netbootd PID (state-file compat).
        dhcp_pid: pid,
        tftp_pid: pid,
        started_at: state::now_epoch(),
        interface: interface.to_string(),
        tftp_root: tftp_root.to_string(),
        engine: "rust".to_string(),
    })?;
    Ok(())
}

/// Whether `stop` may signal a pid it read from the state file. Only a pid
/// that is both alive *and* still running netbootd is ours to touch: a dead
/// one needs nothing, and a live one whose command line no longer mentions
/// netbootd has been recycled by the kernel for some unrelated process since
/// the file was written — signalling it (let alone `sudo kill`ing it) would
/// hit a stranger. Pure, so the rule is unit-testable without a process.
fn should_signal(alive: bool, cmdline_mentions_netbootd: bool) -> bool {
    alive && cmdline_mentions_netbootd
}

/// [`should_signal`] evaluated against the live process table, right now.
fn is_verified_netbootd(pid: i32) -> bool {
    should_signal(
        state::is_pid_alive(pid),
        state::pid_cmdline(pid).contains("netbootd"),
    )
}

/// Deliver `signal` to a verified netbootd, escalating through `sudo kill`
/// when the direct send is refused: on Linux the daemon runs as root, so an
/// unprivileged CLI gets `EPERM` from `kill(2)`.
fn signal_netbootd(pid: i32, signal: crate::platform::Signal) {
    if crate::platform::try_signal_pid(pid, signal).is_err() {
        escalate_via_sudo(pid, signal);
    }
}

#[cfg(unix)]
fn escalate_via_sudo(pid: i32, signal: crate::platform::Signal) {
    let flag = match signal {
        crate::platform::Signal::Term => "-TERM",
        crate::platform::Signal::Kill => "-KILL",
    };
    let _ = Command::new("sudo")
        .args(["kill", flag, &pid.to_string()])
        .status();
}

/// No `sudo` and no root-owned daemon on Windows: a refused signal stays refused.
#[cfg(not(unix))]
fn escalate_via_sudo(_pid: i32, _signal: crate::platform::Signal) {}

/// Block until every pid in `pids` has exited, or `timeout` passes.
fn wait_for_exit(pids: &[i32], timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if pids.iter().all(|&pid| !state::is_pid_alive(pid)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Stop the netboot session for `target` and restore its interface.
///
/// Every signal is gated on the pid still being a live netbootd (see
/// [`should_signal`]); a recorded pid that is dead or recycled is simply
/// forgotten — state file removed, interface restored — without being
/// signalled. A netbootd that ignores SIGTERM through the 3 s grace is
/// SIGKILLed (again name-verified, again escalating through `sudo` on
/// `EPERM`); one that survives even that is an error, and the state file is
/// kept so nothing reports a stop that did not happen.
pub fn stop(target: &str) -> Result<()> {
    let s = state::load_netboot_state(target)
        .ok_or_else(|| anyhow!("no netboot state for '{target}'"))?;
    // Both fields hold the one netbootd pid for the rust engine; signal each
    // distinct pid once.
    let mut recorded = vec![s.dhcp_pid];
    if s.tftp_pid != s.dhcp_pid {
        recorded.push(s.tftp_pid);
    }
    let live: Vec<i32> = recorded
        .into_iter()
        .filter(|&pid| is_verified_netbootd(pid))
        .collect();
    for &pid in &live {
        signal_netbootd(pid, crate::platform::Signal::Term);
    }
    wait_for_exit(&live, std::time::Duration::from_secs(3));

    // Re-verify before escalating: the grace period is long enough for a
    // pid to have been recycled.
    let stubborn: Vec<i32> = live
        .into_iter()
        .filter(|&pid| is_verified_netbootd(pid))
        .collect();
    for &pid in &stubborn {
        signal_netbootd(pid, crate::platform::Signal::Kill);
    }
    wait_for_exit(&stubborn, std::time::Duration::from_secs(2));
    let survivors: Vec<String> = stubborn
        .into_iter()
        .filter(|&pid| is_verified_netbootd(pid))
        .map(|pid| pid.to_string())
        .collect();
    if !survivors.is_empty() {
        bail!(
            "netbootd (pid {}) is still running after SIGKILL; leaving its state \
             file and interface {} in place",
            survivors.join(", "),
            s.interface
        );
    }

    state::remove_netboot_state(target);
    netif::restore_interface(&s.interface);
    Ok(())
}

pub struct Status {
    pub running: bool,
    pub state: Option<NetbootState>,
    pub uptime_seconds: Option<f64>,
}

pub fn status(target: &str) -> Status {
    let Some(s) = state::load_netboot_state(target) else {
        return Status {
            running: false,
            state: None,
            uptime_seconds: None,
        };
    };
    let alive = state::is_named_child_alive(s.dhcp_pid, "netbootd");
    let uptime = alive.then(|| state::now_epoch() - s.started_at);
    Status {
        running: alive,
        state: Some(s),
        uptime_seconds: uptime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(target: &str, interface: &str, pid: i32) -> (String, NetbootState) {
        (
            target.to_string(),
            NetbootState {
                target: target.to_string(),
                dhcp_pid: pid,
                tftp_pid: pid,
                started_at: 0.0,
                interface: interface.to_string(),
                tftp_root: "/srv/tftp".to_string(),
                engine: "rust".to_string(),
            },
        )
    }

    #[test]
    fn another_target_on_the_same_interface_conflicts() {
        let live = vec![running("pi5", "en7", 4242), running("nova", "en9", 4343)];
        let hit = interface_conflict("nuc", "en7", &live).expect("en7 is taken");
        assert_eq!(hit.0, "pi5");
        assert_eq!(hit.1.dhcp_pid, 4242);
    }

    #[test]
    fn a_free_interface_or_our_own_entry_does_not_conflict() {
        let live = vec![running("pi5", "en7", 4242)];
        assert!(
            interface_conflict("nuc", "en8", &live).is_none(),
            "other adapter"
        );
        // A stale-but-alive entry for the same target is `start`'s own business
        // (cleanup_stale), not a conflict with another target.
        assert!(interface_conflict("pi5", "en7", &live).is_none());
        assert!(
            interface_conflict("nuc", "en7", &[]).is_none(),
            "nothing running"
        );
    }

    #[test]
    fn log_tail_quotes_the_last_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netboot.log");
        let all: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        std::fs::write(&path, all.join("\n") + "\n").unwrap();

        let tail = log_tail(&path, 20);
        let got: Vec<&str> = tail.lines().collect();
        assert_eq!(got.len(), 20);
        assert_eq!(got[0], "line 11");
        assert_eq!(got[19], "line 30");

        // Shorter than the window: everything, in order.
        assert_eq!(log_tail(&path, 100).lines().count(), 30);
        // Missing file: empty, not an error — the spawn failure is the story.
        assert_eq!(log_tail(&dir.path().join("nope.log"), 20), "");
    }

    /// The one rule behind every signal `stop` sends: a pid from the state
    /// file is touched only while it is alive *and* still a netbootd. A live
    /// pid running something else has been recycled and must be left alone —
    /// that path used to get a SIGTERM and then a `sudo kill`.
    #[test]
    fn stop_signals_only_a_live_netbootd() {
        assert!(should_signal(true, true));
        assert!(
            !should_signal(true, false),
            "a recycled pid running another program must not be signalled"
        );
        assert!(!should_signal(false, true), "a dead pid needs nothing");
        assert!(!should_signal(false, false));
    }
}
