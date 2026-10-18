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

//! Drop root after the privileged setup is done (Linux).
//!
//! On Linux ports 67/69 (and 80) need root, and so does `SO_BINDTODEVICE` on
//! older kernels, so `panio netboot start` spawns netbootd under `sudo`.
//! Nothing after the listen sockets are bound and pinned needs root, though:
//! the ARP pin and the interface-IP monitor already go through their own
//! `sudo` (passwordless sudo is a documented requirement on Linux control
//! hosts), and everything else is parsing packets from an untrusted link and
//! reading files out of the root directory. So once the sockets are bound the
//! daemon drops to the user who ran `sudo` — the `SUDO_UID` / `SUDO_GID` sudo
//! leaves in the environment — with `setgroups(empty)`, `setgid`, `setuid`, in
//! that order (the reverse would leave no privilege to finish the job).
//! Any of the three failing is fatal; a daemon that thinks it dropped root and
//! did not is worse than one that refused to start.
//!
//! macOS is unchanged: netbootd already runs unprivileged there (the setuid
//! `netbootd-bpf-helper` holds the only root), so [`drop_privileges`] is a
//! no-op off Linux.
//!
//! The decision — whether to drop and to whom — is the pure
//! [`drop_target`], which is unit-tested. The syscalls themselves need a root
//! process to exercise and are not covered by the test suite.

use anyhow::{bail, Context, Result};

/// The uid/gid a root netbootd drops to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub struct DropTarget {
    pub uid: u32,
    pub gid: u32,
}

/// Decide whether to drop privileges, from the current real uid and the raw
/// `SUDO_UID` / `SUDO_GID` environment values.
///
/// * Not root → nothing to drop (`None`), whatever the environment says.
/// * Root with neither variable → `None`: netbootd was started as root
///   directly, not through `sudo`, so there is no unprivileged identity to
///   return to. The caller logs this.
/// * Root with both → the parsed target, except that a `SUDO_UID` of 0 (root
///   ran `sudo`) is `None` again.
/// * Root with exactly one, or an unparsable value → an error. sudo always
///   sets both; anything else is a broken environment, and guessing an
///   identity is not an option.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn drop_target(
    current_uid: u32,
    sudo_uid: Option<&str>,
    sudo_gid: Option<&str>,
) -> Result<Option<DropTarget>> {
    if current_uid != 0 {
        return Ok(None);
    }
    match (sudo_uid, sudo_gid) {
        (None, None) => Ok(None),
        (Some(u), Some(g)) => {
            let uid: u32 = u
                .trim()
                .parse()
                .with_context(|| format!("SUDO_UID={u:?} is not a uid"))?;
            let gid: u32 = g
                .trim()
                .parse()
                .with_context(|| format!("SUDO_GID={g:?} is not a gid"))?;
            if uid == 0 {
                return Ok(None);
            }
            Ok(Some(DropTarget { uid, gid }))
        }
        (Some(_), None) => bail!("SUDO_UID is set but SUDO_GID is not; refusing to guess a gid"),
        (None, Some(_)) => bail!("SUDO_GID is set but SUDO_UID is not; refusing to guess a uid"),
    }
}

/// Drop to the `sudo` invoker if this is a root process on Linux. Call it only
/// after every socket that needs root has been bound and pinned.
#[cfg(target_os = "linux")]
pub fn drop_privileges() -> Result<()> {
    let current_uid = unsafe { libc::getuid() };
    let sudo_uid = std::env::var("SUDO_UID").ok();
    let sudo_gid = std::env::var("SUDO_GID").ok();
    let target = drop_target(current_uid, sudo_uid.as_deref(), sudo_gid.as_deref())
        .context("privilege drop")?;
    let Some(DropTarget { uid, gid }) = target else {
        if current_uid == 0 {
            tracing::warn!(
                "running as root with no SUDO_UID/SUDO_GID in the environment; \
                 cannot drop privileges (start netbootd through sudo, as \
                 `panio netboot start` does)"
            );
        }
        return Ok(());
    };

    // Supplementary groups first (needs root), then the gid (needs root), then
    // the uid — after which none of the earlier steps would be possible.
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        bail!("setgroups(empty): {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::setgid(gid) } != 0 {
        bail!("setgid({gid}): {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::setuid(uid) } != 0 {
        bail!("setuid({uid}): {}", std::io::Error::last_os_error());
    }
    // The drop must be real and irreversible: both uids changed, and root
    // cannot be re-acquired.
    let (ruid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if ruid != uid || euid != uid {
        bail!("privilege drop did not take: uid {ruid}/{euid} after setuid({uid})");
    }
    if unsafe { libc::setuid(0) } == 0 {
        bail!("privilege drop is reversible: setuid(0) succeeded after dropping to {uid}");
    }
    tracing::info!("dropped privileges to uid {uid} gid {gid} (the sudo invoker)");
    Ok(())
}

/// macOS (and everything else) runs netbootd unprivileged already; nothing to
/// drop.
#[cfg(not(target_os = "linux"))]
pub fn drop_privileges() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unprivileged_process_never_drops() {
        assert_eq!(drop_target(501, Some("501"), Some("20")).unwrap(), None);
        assert_eq!(drop_target(1000, None, None).unwrap(), None);
        // Even a broken environment is irrelevant when there is no root to drop.
        assert_eq!(drop_target(1000, Some("x"), None).unwrap(), None);
    }

    #[test]
    fn root_with_sudo_identity_drops_to_it() {
        assert_eq!(
            drop_target(0, Some("1000"), Some("1000")).unwrap(),
            Some(DropTarget {
                uid: 1000,
                gid: 1000
            })
        );
        // Whitespace from a sloppy wrapper is tolerated.
        assert_eq!(
            drop_target(0, Some(" 1001\n"), Some("27")).unwrap(),
            Some(DropTarget { uid: 1001, gid: 27 })
        );
    }

    #[test]
    fn root_without_sudo_stays_root() {
        assert_eq!(drop_target(0, None, None).unwrap(), None);
        // root ran sudo: SUDO_UID=0 is not an identity to drop to.
        assert_eq!(drop_target(0, Some("0"), Some("0")).unwrap(), None);
    }

    #[test]
    fn half_set_or_garbage_environment_is_an_error() {
        assert!(drop_target(0, Some("1000"), None).is_err());
        assert!(drop_target(0, None, Some("1000")).is_err());
        assert!(drop_target(0, Some("bob"), Some("1000")).is_err());
        assert!(drop_target(0, Some("1000"), Some("-1")).is_err());
        assert!(drop_target(0, Some(""), Some("1000")).is_err());
    }
}
