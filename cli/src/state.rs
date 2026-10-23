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

//! Persistent runtime state — netboot PID records and liveness checks.
//!
//! The JSON shape matches the Python `_state.py` exactly so both CLIs can read
//! each other's state during the migration: for the rust engine both `*_pid`
//! fields hold the single netbootd PID.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetbootState {
    pub target: String,
    pub dhcp_pid: i32,
    pub tftp_pid: i32,
    pub started_at: f64,
    pub interface: String,
    pub tftp_root: String,
    #[serde(default = "default_engine")]
    pub engine: String,
}

fn default_engine() -> String {
    "python".to_string()
}

pub fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local/share/panio")
}

/// `target` becomes a single path component under `state_dir()`. It reaches
/// here from the lab file, which may predate name validation
/// (`model::validate_name` only runs at `add`/`rename`/`set` time — see its
/// doc comment) or have been hand-edited, so it is sanitized the same way
/// every other lab-derived path component is (`daemons::sanitize_component`)
/// rather than trusted to already be safe.
pub(crate) fn target_dir(target: &str) -> PathBuf {
    state_dir().join(crate::daemons::sanitize_component(target))
}

pub fn netboot_state_path(target: &str) -> PathBuf {
    target_dir(target).join("netboot.json")
}

pub fn netboot_log_path(target: &str) -> PathBuf {
    target_dir(target).join("netboot.log")
}

pub fn ensure_target_dir(target: &str) -> std::io::Result<PathBuf> {
    let d = target_dir(target);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

pub fn save_netboot_state(state: &NetbootState) -> anyhow::Result<()> {
    ensure_target_dir(&state.target)?;
    let json = serde_json::to_string_pretty(state)?;
    // Atomic (sibling temp file + rename): a reader of this file — `doctor`,
    // another `panio netboot` invocation — must never see a truncated or
    // half-written state file, and a crash mid-write must leave the previous
    // state intact rather than corrupt.
    crate::platform::write_atomic(&netboot_state_path(&state.target), json.as_bytes())?;
    Ok(())
}

pub fn load_netboot_state(target: &str) -> Option<NetbootState> {
    let text = std::fs::read_to_string(netboot_state_path(target)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn remove_netboot_state(target: &str) {
    let _ = std::fs::remove_file(netboot_state_path(target));
}

/// True if any process with this PID exists (signal-0 probe; EPERM = alive).
pub fn is_pid_alive(pid: i32) -> bool {
    crate::platform::pid_alive(pid)
}

/// The full command line of `pid`, or empty on failure. On Windows it is the
/// image name only (`hdmicap.exe`, what `tasklist` reports), which is enough
/// for every caller: they all match on a binary name.
pub fn pid_cmdline(pid: i32) -> String {
    if cfg!(target_os = "macos") {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    } else if cfg!(windows) {
        // `"hdmicap.exe","1234","Console","1","12,345 K"` — or an INFO line
        // when nothing matches.
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()
            .and_then(|o| {
                let text = String::from_utf8_lossy(&o.stdout);
                let row = text.lines().find(|l| l.starts_with('"'))?;
                Some(row.trim_start_matches('"').split('"').next()?.to_string())
            })
            .unwrap_or_default()
    } else {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .replace('\0', " ")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default()
    }
}

/// True only if `pid` is alive AND its command line mentions `needle` — guards
/// against PID reuse by unrelated processes after a crash.
pub fn is_named_child_alive(pid: i32, needle: &str) -> bool {
    is_pid_alive(pid) && pid_cmdline(pid).contains(needle)
}

/// The classification behind [`is_named_process_pending`], over the two facts
/// it reads: pure, so the teardown case is testable without a process to kill.
///
/// `cmdline` empty while the pid is alive is the case that matters. On Linux
/// `/proc/<pid>/cmdline` reads empty for a process between its death and its
/// reaping, and `ps -o args=` on macOS does the same for a `<defunct>` entry —
/// so a process that has just been SIGKILLed reports *alive* to `kill(pid, 0)`
/// and *not ours* to a name match, at the same instant. Reading that as "the
/// daemon is gone" releases the device before the kernel has, which is how a
/// replacement ends up losing the race for an exclusive device to a corpse
/// (GitHub #193). A non-empty command line that is not ours is different: that
/// pid has genuinely been recycled for something else, and waiting on it would
/// never end.
fn named_process_pending(alive: bool, cmdline: &str, needle: &str) -> bool {
    alive && (cmdline.is_empty() || cmdline.contains(needle))
}

/// True while `pid` must still be treated as the named daemon — it is alive
/// and either still running that program or showing no command line at all
/// (see [`named_process_pending`]). The test to wait on before taking over a
/// daemon's exclusive device; [`is_named_child_alive`] answers the different
/// question of whether a pid is *identifiably* still that daemon, which is the
/// right test before signalling it.
pub fn is_named_process_pending(pid: i32, needle: &str) -> bool {
    named_process_pending(is_pid_alive(pid), &pid_cmdline(pid), needle)
}

/// True only if the netboot process for `target` is alive (rust engine: the
/// single netbootd; legacy python engine: both children).
pub fn is_netboot_running(target: &str) -> bool {
    let Some(state) = load_netboot_state(target) else {
        return false;
    };
    if state.engine == "rust" {
        is_named_child_alive(state.dhcp_pid, "netbootd")
    } else {
        is_named_child_alive(state.dhcp_pid, "panio._dhcp")
            && is_named_child_alive(state.tftp_pid, "panio._tftp")
    }
}

/// Every target whose netboot engine is alive right now, found via the
/// per-target state files under [`state_dir`], sorted by target name.
pub fn running_netboots() -> Vec<(String, NetbootState)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(state_dir()) {
        for e in entries.flatten() {
            let target = e.file_name().to_string_lossy().into_owned();
            if let Some(st) = load_netboot_state(&target) {
                if is_netboot_running(&target) {
                    out.push((target, st));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target name reaching `target_dir` may predate `model::validate_name`
    /// (it only runs at `add`/`rename`/`set` time) or come from a hand-edited
    /// lab file. Whatever it is, it must land as one path component directly
    /// under `state_dir()` — never able to escape it or address a sibling
    /// directory (Review low #3). This doesn't touch the real filesystem:
    /// `target_dir` only builds a path.
    #[test]
    fn target_dir_is_always_a_direct_child_of_state_dir() {
        for bad in [
            "../../etc",
            "/etc/passwd",
            "a/b",
            "..",
            ".",
            "",
            "  ",
            "a b",
        ] {
            let d = target_dir(bad);
            assert_eq!(
                d.parent(),
                Some(state_dir().as_path()),
                "{bad:?} -> {}",
                d.display()
            );
        }
    }

    /// A plain, already-safe name passes through unchanged, so existing
    /// per-target state paths (`~/.local/share/panio/<target>/…`) don't
    /// move under this change.
    #[test]
    fn target_dir_leaves_a_plain_name_unchanged() {
        assert_eq!(target_dir("pi5"), state_dir().join("pi5"));
    }

    /// The case that cost a capture daemon (GitHub #193): a process between
    /// SIGKILL and reaping is alive to `kill(pid, 0)` and nameless to a
    /// command-line match, at the same instant. Reading that as "gone" hands
    /// the device to a replacement while the corpse still holds it. The two
    /// predicates must disagree here, and agree everywhere else.
    #[test]
    fn a_process_in_teardown_is_pending_but_not_identifiably_ours() {
        // alive, no command line — the teardown window. The superseded
        // predicate was `alive && cmdline.contains(name)`, which is false
        // here: that is what read a corpse still holding the device as
        // finished. It is described rather than spelled out, because written
        // inline with these literals it is a constant expression and clippy's
        // `nonminimal_bool` fails the build on it (GitHub #205).
        assert!(named_process_pending(true, "", "hdmicap"));

        // Alive and still running the daemon: pending, and identifiable.
        assert!(named_process_pending(
            true,
            "/usr/libexec/panio/bin/hdmicap daemon --device /dev/video0",
            "hdmicap"
        ));
        // A pid the kernel handed to something else: not ours, and waiting on
        // it would never end.
        assert!(!named_process_pending(
            true,
            "/usr/bin/vim notes.txt",
            "hdmicap"
        ));
        // Dead is dead, whatever the command line said.
        assert!(!named_process_pending(false, "", "hdmicap"));
        assert!(!named_process_pending(
            false,
            "/usr/libexec/panio/bin/hdmicap daemon",
            "hdmicap"
        ));
    }
}
