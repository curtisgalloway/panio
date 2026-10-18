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

//! The POSIX primitives this daemon needs, behind a portable surface.
//!
//! Trimmed copy of `cli/src/platform.rs` — identity, the private runtime dir,
//! and pid liveness/termination. It is duplicated rather than shared for the
//! same reason `daemon.rs` is: these are standalone cargo projects with no
//! common crate between them. Keep the copies in sync (cli, hdmicap, serialcap,
//! ch9329, hidrig).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// A stable per-user id used to namespace the runtime base (`panio-<id>`).
///
/// On Unix this is the real uid. Windows has no uid, so we hash the username
/// (FNV-1a) — meaningless on its own, but stable for a user across sessions,
/// which is all the runtime path needs. Never used for authorization.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // Safe: getuid() has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(windows)]
pub fn current_uid() -> u32 {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string());
    let mut h: u32 = 0x811c_9dc5;
    for b in user.to_ascii_lowercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Root beneath which the per-user runtime base lives.
///
/// Unix hardcodes `/tmp` rather than `$TMPDIR`, because macOS hands each
/// environment a different TMPDIR and a daemon started in one would be
/// invisible from the others. On Windows `std::env::temp_dir()` is already
/// per-user (`%LOCALAPPDATA%\Temp`), stable across sessions, and ACL'd to that
/// user — the right location and the safe one. `$PANIO_RUNTIME_BASE`
/// overrides it, matching the CLI.
pub fn runtime_root() -> PathBuf {
    if let Some(base) = std::env::var_os("PANIO_RUNTIME_BASE") {
        return PathBuf::from(base);
    }
    #[cfg(unix)]
    {
        PathBuf::from("/tmp")
    }
    #[cfg(windows)]
    {
        std::env::temp_dir()
    }
}

/// Create `base` as a private, user-owned directory, or validate an existing
/// one.
///
/// Unix does a 0700 create plus an ownership *and mode* check, guarding the
/// well-known `/tmp` path against a squatter. An existing directory that is
/// ours but group/world-accessible (left by an older panio's plain
/// `create_dir_all`, or made by hand) is tightened to 0700 and accepted — it
/// is our own directory, so there is nothing to refuse or report; a symlink,
/// a non-directory, or another owner is an error. On Windows the path sits
/// inside the user's own profile, whose inherited ACL already excludes other
/// non-administrative users, so the check reduces to "exists and is a
/// directory".
#[cfg(unix)]
pub fn ensure_private_dir(base: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let uid = current_uid();
            let md = std::fs::symlink_metadata(base)?;
            if !md.is_dir() || md.uid() != uid {
                return Err(anyhow!(
                    "{} exists but is not a directory owned by uid {uid}",
                    base.display()
                ));
            }
            if md.mode() & 0o077 != 0 {
                std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(windows)]
pub fn ensure_private_dir(base: &Path) -> Result<()> {
    match std::fs::create_dir(base) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let md = std::fs::symlink_metadata(base)?;
            if !md.is_dir() {
                return Err(anyhow!("{} exists but is not a directory", base.display()));
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// A pid that could not name a real process.
///
/// This guard is not defensive padding. On Unix `kill()` reads non-positive
/// pids as *broadcasts*: 0 means "every process in my group", -1 means "every
/// process I am allowed to signal", and any other negative is a process group.
/// So a zero or corrupt pid in a discovery file would make `pid_alive` answer
/// "yes, running" and `signal_pid(.., Kill)` take down panio itself and the
/// shell that launched it. Only positive pids are ever real daemons.
fn is_real_pid(pid: i32) -> bool {
    pid > 0
}

/// True if a process with this pid exists.
///
/// Unix uses the signal-0 probe, treating `EPERM` as alive. Windows opens the
/// process for a limited-information query and asks whether it still has
/// `STILL_ACTIVE` as its exit code; access-denied likewise means live-but-not-
/// ours.
#[cfg(unix)]
pub fn pid_alive(pid: i32) -> bool {
    if !is_real_pid(pid) {
        return false;
    }
    // Safe: kill(pid, 0) only probes for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if !is_real_pid(pid) {
        return false;
    }
    // Safe: OpenProcess validates the pid and returns null on failure.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the broadcast-pid hazard: on Unix `kill()` reads 0 as "my whole
    /// process group" and -1 as "everything I may signal", so a corrupt pid in
    /// a discovery file must never reach it. Kept in every copy of this module
    /// so the guard cannot be dropped from one of them unnoticed.
    #[test]
    fn non_positive_pids_are_never_real() {
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn runtime_root_is_absolute() {
        assert!(runtime_root().is_absolute());
    }

    #[test]
    fn ensure_private_dir_creates_then_revalidates() {
        let tmp = std::env::temp_dir().join(format!("panio-hp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        ensure_private_dir(&tmp).expect("first call creates");
        assert!(tmp.is_dir());
        ensure_private_dir(&tmp).expect("second call revalidates");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A base left group/world-accessible by an earlier creator (an older
    /// panio's plain `create_dir_all`, or the user by hand) is ours to
    /// tighten: after the call it must be 0700, so the discovery file and
    /// logs beneath it are not reachable by other users.
    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_tightens_an_open_dir_we_own() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = std::env::temp_dir().join(format!("panio-hp-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir(&tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::metadata(&tmp).unwrap().mode() & 0o777, 0o755);

        ensure_private_dir(&tmp).expect("an open dir we own is tightened, not refused");
        assert_eq!(std::fs::metadata(&tmp).unwrap().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A symlink at the base — even one pointing at a private directory we
    /// own — is refused: a squatter's link would redirect the discovery file
    /// and logs to a place of their choosing.
    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_a_symlink() {
        let real = std::env::temp_dir().join(format!("panio-hp-real-{}", std::process::id()));
        let link = std::env::temp_dir().join(format!("panio-hp-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        ensure_private_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(
            ensure_private_dir(&link).is_err(),
            "a symlinked base must be refused"
        );
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&real);
    }
}
