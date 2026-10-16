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

//! Named target actions (#299) and the per-target hook lock they share with
//! the power hooks.
//!
//! An action is an opaque hook command for a control that is not power: a
//! reset line, a boot strap, a recovery button combination. paniolo runs it
//! under the power-hook contract (`sh -c`, helper dirs on PATH, the helper
//! state/runtime dirs) and defines no semantics for it.
//!
//! Action and power hooks for one target often talk to the same exclusive
//! device (a board-control MCU's serial port), so each one runs holding a
//! per-target lock file. A hook that itself calls back into paniolo for the
//! same target (an action script running `paniolo power-state`) must not wait
//! on the lock its parent holds: the holder exports [`LOCK_HELD_ENV`], and a
//! process that sees its own target there skips the lock.

use std::fs::File;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{daemons, error, model, platform};

/// Set, in a hook's environment, to the target whose hook lock the parent
/// holds.
pub const LOCK_HELD_ENV: &str = "PANIOLO_HOOK_LOCK";

/// How long a hook waits for another hook on the same target to finish. A
/// power cycle can legitimately take tens of seconds.
pub const LOCK_WAIT: Duration = Duration::from_secs(120);

const LOCK_POLL: Duration = Duration::from_millis(100);

/// A held per-target hook lock; released when dropped (the OS drops the lock
/// with the file). `file` is `None` when the parent process already holds it.
pub struct HookLock {
    target: String,
    _file: Option<File>,
}

impl HookLock {
    /// The env pair a hook run under this lock gets, so a nested paniolo
    /// call for the same target does not wait on it.
    pub fn env(&self) -> (&'static str, &str) {
        (LOCK_HELD_ENV, &self.target)
    }
}

/// Take `target`'s hook lock, waiting up to `wait` for a hook already running.
/// A timeout is `timeout` (exit 22): the other hook may still be using the
/// device, so the caller must not assume anything about its state.
pub fn hook_lock(target: &str, wait: Duration) -> Result<HookLock> {
    let held = std::env::var(LOCK_HELD_ENV).ok();
    if lock_inherited(held.as_deref(), target) {
        return Ok(HookLock {
            target: target.to_string(),
            _file: None,
        });
    }
    let dir = daemons::ensure_runtime_dir("hooks", Some(target))?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("hook.lock"))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(HookLock {
                    target: target.to_string(),
                    _file: Some(file),
                })
            }
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < wait => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(error::PanioloError::new(
                    error::Kind::Timeout,
                    format!(
                        "another power or action hook for '{target}' is still running after {} s",
                        wait.as_secs()
                    ),
                )
                .target(target)
                .into())
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Whether the lock for `target` is already held by an ancestor, according
/// to the inherited [`LOCK_HELD_ENV`] value.
fn lock_inherited(held: Option<&str>, target: &str) -> bool {
    held == Some(target)
}

/// Why `requires_power` refuses to run, or `None` when the state allows it.
/// `usage` (exit 2): as with an ambiguous target, only the caller can fix it,
/// by changing the power state first.
pub fn power_refusal(
    target: &str,
    action: &str,
    requires: &str,
    is_on: bool,
) -> Option<error::PanioloError> {
    let wanted_on = requires == "on";
    if wanted_on == is_on {
        return None;
    }
    let now = if is_on { "on" } else { "off" };
    Some(
        error::PanioloError::new(
            error::Kind::Usage,
            format!(
                "action '{action}' requires power {requires}, but '{target}' is {now} \
                 (paniolo power {requires} {target}, then retry)"
            ),
        )
        .target(target)
        .channel("actions"),
    )
}

/// The named action of `target`, as configured, or `not_configured` naming
/// the ones it does have.
pub fn find<'a>(lab: &'a model::Lab, target: &str, name: &str) -> Result<&'a model::Action> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let acts = t.actions.as_ref().ok_or_else(|| {
        error::channel_missing(
            target,
            "actions",
            format!(
                "target '{target}' has no actions (paniolo action set -t {target} <name> --cmd ...)"
            ),
        )
    })?;
    acts.actions.get(name).ok_or_else(|| {
        let have: Vec<&str> = acts.actions.keys().map(String::as_str).collect();
        error::channel_missing(
            target,
            "actions",
            format!(
                "target '{target}' has no action '{name}' (have: {})",
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

/// Run an action's hook command on this host under `lock`, propagating a
/// failure as `hook_failed` (its exit code kept in the error).
pub fn run_hook(lock: &HookLock, target: &str, name: &str, cmd: &str) -> Result<()> {
    eprintln!("Running action '{name}' for '{target}' via {cmd}");
    let envs = daemons::hook_helper_name(cmd)
        .map(|n| daemons::helper_env(&n, None))
        .unwrap_or_default();
    let (k, v) = lock.env();
    let status = platform::shell_command(cmd)
        .env("PATH", daemons::hook_path())
        .envs(envs)
        .env(k, v)
        .status()?;
    if status.success() {
        return Ok(());
    }
    Err(error::hook_failed(
        status,
        format!(
            "action '{name}' exited with code {}",
            error::shell_code(status)
        ),
    )
    .target(target)
    .channel("actions")
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_power_refuses_only_the_wrong_state() {
        assert!(power_refusal("pi5", "boot-usb", "off", false).is_none());
        assert!(power_refusal("pi5", "reset", "on", true).is_none());
        let e = power_refusal("pi5", "boot-usb", "off", true).unwrap();
        assert_eq!(e.kind, error::Kind::Usage);
        assert!(e.to_string().contains("requires power off"), "{e}");
        assert!(e.to_string().contains("'pi5' is on"), "{e}");
        assert!(power_refusal("pi5", "reset", "on", false).is_some());
    }

    #[test]
    fn only_the_same_targets_lock_is_inherited() {
        assert!(lock_inherited(Some("pi5"), "pi5"));
        assert!(!lock_inherited(Some("nuc"), "pi5"));
        assert!(!lock_inherited(None, "pi5"));
    }

    /// A second taker waits and then times out while the first holds the
    /// lock, and gets it once the first is dropped. Uses a private runtime
    /// base so it never touches a real one.
    #[test]
    fn hook_lock_excludes_and_times_out() {
        let _guard = daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PANIOLO_RUNTIME_BASE");
        // Safe: serialized by RUNTIME_BASE_LOCK; restored below.
        unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", base.path()) };
        let target = "hook-lock-test";
        let first = hook_lock(target, Duration::ZERO).unwrap();
        let t0 = Instant::now();
        let second = hook_lock(target, Duration::from_millis(300));
        let e = second.err().expect("second taker must not get the lock");
        assert!(t0.elapsed() >= Duration::from_millis(300));
        let pe = e.downcast_ref::<error::PanioloError>().unwrap();
        assert_eq!(pe.kind, error::Kind::Timeout);
        drop(first);
        let third = hook_lock(target, Duration::ZERO);
        match prev {
            Some(p) => unsafe { std::env::set_var("PANIOLO_RUNTIME_BASE", p) },
            None => unsafe { std::env::remove_var("PANIOLO_RUNTIME_BASE") },
        }
        assert!(third.is_ok());
    }
}
