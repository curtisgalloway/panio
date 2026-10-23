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

//! The POSIX facilities panio depends on, behind a portable surface.
//!
//! Panio grew up on macOS and Linux, so process and filesystem primitives
//! were reached for directly (`libc::getuid`, `libc::kill`, `CommandExt::exec`,
//! `DirBuilderExt::mode`). Windows has none of those. Rather than sprinkle
//! `#[cfg]` through the call sites, every such primitive lives here with one
//! implementation per platform and a single documented contract.
//!
//! The same module is duplicated (deliberately, matching how `daemon.rs` is
//! duplicated) in the helper crates that run daemons — hdmicap, serialcap,
//! ch9329, hidrig. Keep the four in sync.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Result};

/// Which signal [`signal_pid`] should deliver.
///
/// Windows has no signals: both variants land on `TerminateProcess`, which is
/// unconditional. A Windows daemon therefore never runs its graceful-shutdown
/// path (discovery-file removal, the 300 ms grace) — it is killed outright, and
/// a stale discovery file may be left behind for the next `pid_alive` probe to
/// reap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Polite stop: SIGTERM on Unix.
    Term,
    /// Unconditional kill: SIGKILL on Unix.
    Kill,
}

// ── identity ────────────────────────────────────────────────────────────────

/// A stable per-user id used to namespace the runtime base (`panio-<id>`).
///
/// On Unix this is the real uid. Windows has no uid, so we hash the username
/// (FNV-1a, truncated) — the value is meaningless on its own but is stable for
/// a given user across sessions and reboots, which is all the runtime path
/// needs. It is never used for an authorization decision.
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

// ── runtime directories ─────────────────────────────────────────────────────

/// Default root beneath which the per-user runtime base is created, when
/// `$PANIO_RUNTIME_BASE` is unset.
///
/// Unix uses a hardcoded `/tmp` rather than `$TMPDIR`, because macOS hands each
/// environment a different TMPDIR (GUI terminal vs SSH vs a sandboxed agent
/// shell) and a daemon started in one would be invisible from the others.
/// Windows has no such problem: `std::env::temp_dir()` resolves to
/// `%LOCALAPPDATA%\Temp`, which is per-user, stable across sessions, and
/// already ACL'd to that user — so it is both the right location and the
/// safe one.
#[cfg(unix)]
pub fn default_runtime_root() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp")
}

#[cfg(windows)]
pub fn default_runtime_root() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// Create `base` as a private, user-owned directory, or validate it if it
/// already exists.
///
/// On Unix this is a 0700 create plus an ownership *and mode* check, guarding
/// against a squatter pre-creating the well-known `/tmp` path. An existing
/// directory that is ours but group/world-accessible — left by a plain
/// `create_dir_all` in an older panio, or made by hand — is tightened to
/// 0700 and accepted: it is our own directory, so there is nothing to refuse
/// and nothing to report. One that is a symlink, not a directory, or owned
/// by someone else is an error. On Windows the parent is inside the user's
/// own profile, whose inherited ACL already denies other non-administrative
/// users, so the check reduces to "exists and is a directory". (An
/// administrator can write anywhere on either platform; that is out of scope
/// for both.)
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

/// True if `path` is a directory this user owns that nobody else can enter:
/// a real directory (not a symlink), owned by [`current_uid`], with no
/// group/other permission bits. The read-side twin of [`ensure_private_dir`]
/// — a reader of the runtime base applies it before trusting a discovery
/// file found beneath — and, unlike the writer, it never repairs, only
/// refuses. On Windows the profile ACL stands in for the owner and mode
/// checks, so it reduces to "a directory, not a link".
#[cfg(unix)]
pub fn is_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(md) => md.is_dir() && md.uid() == current_uid() && md.mode() & 0o077 == 0,
        Err(_) => false,
    }
}

/// True if `md` (from `symlink_metadata`) is a regular file owned by
/// [`current_uid`] — the test for "a leftover of ours, safe to delete". On
/// Windows there is no uid to compare, so it reduces to "a regular file".
#[cfg(unix)]
pub fn is_own_file(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    md.is_file() && md.uid() == current_uid()
}

#[cfg(windows)]
pub fn is_own_file(md: &std::fs::Metadata) -> bool {
    md.is_file()
}

#[cfg(windows)]
pub fn is_private_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|md| md.is_dir())
        .unwrap_or(false)
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

/// Mark `path` executable. A no-op on Windows, where executability comes from
/// the file extension rather than a permission bit.
#[cfg(unix)]
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(windows)]
pub fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Write `data` to `path` through a sibling temp file and a rename, so a
/// reader never sees a truncated or half-written file and a crash mid-write
/// leaves the old contents intact.
///
/// `path` is resolved first: the lab file is commonly a symlink into a git
/// checkout, and a rename onto the *link* would replace it with a plain file
/// and silently detach the lab from version control. The temp file is
/// created beside the resolved target (rename needs one filesystem), and an
/// existing file's permissions are carried over — the temp file starts
/// owner-only, which is not what a lab in a shared checkout was.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let resolved = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let name = path.file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} has no file name", path.display()),
                )
            })?;
            std::fs::canonicalize(parent)?.join(name)
        }
        Err(e) => return Err(e),
    };
    let dir = resolved.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", resolved.display()),
        )
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    if let Ok(md) = std::fs::metadata(&resolved) {
        std::fs::set_permissions(tmp.path(), md.permissions())?;
    }
    tmp.persist(&resolved).map_err(|e| e.error)?;
    Ok(())
}

// ── process lifecycle ───────────────────────────────────────────────────────

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
/// Unix uses the signal-0 probe, treating `EPERM` as alive (the process exists
/// but belongs to another user). Windows opens the process for a
/// limited-information query and asks whether it has an exit code yet; a
/// successful open that reports `STILL_ACTIVE` is the equivalent answer, and
/// `ERROR_ACCESS_DENIED` likewise means the pid is live but not ours.
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
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if !is_real_pid(pid) {
        return false;
    }
    // Safe: OpenProcess validates the pid itself and returns null on failure.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            // Access-denied means the process exists, we just can't open it.
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code: u32 = 0;
        let ok = windows_sys::Win32::System::Threading::GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// An opaque start-time stamp for `pid`: equal across calls for the same
/// process, different for a recycled pid. `None` when the process does not
/// exist or cannot be queried. Units differ per platform and are not
/// comparable across machines or boots.
// Used by lease.rs (later task); nothing calls it yet.
#[allow(dead_code)]
#[cfg(target_os = "linux")]
pub fn process_start_time(pid: i32) -> Option<u64> {
    if !is_real_pid(pid) {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may contain spaces and parentheses, so
    // parse from the last `)`. Fields after it start at field 3 (state);
    // starttime is field 22, i.e. index 19 of the remainder.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[allow(dead_code)]
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: i32) -> Option<u64> {
    if !is_real_pid(pid) {
        return None;
    }
    // Safe: proc_bsdinfo is plain data, and proc_pidinfo writes at most the
    // buffer size we pass.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[allow(dead_code)]
#[cfg(windows)]
pub fn process_start_time(pid: i32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if !is_real_pid(pid) {
        return None;
    }
    // Safe: OpenProcess validates the pid; the FILETIMEs are plain data.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        Some(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
    }
}

/// 32 lowercase hex characters from the operating system's random source,
/// for lease tokens.
// Used by lease.rs (later task); nothing calls it yet.
#[allow(dead_code)]
pub fn random_token() -> std::io::Result<String> {
    let mut buf = [0u8; 16];
    fill_random(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    // Safe: the pointer and length describe `buf`; no algorithm handle is
    // needed with the system-preferred RNG flag.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "BCryptGenRandom failed: {status:#x}"
        )))
    }
}

/// True if this process runs with superuser rights.
///
/// Used only to decide whether a privileged-port helper needs a `sudo` prefix.
/// Windows has no `sudo` and no caller that needs this, so it answers `false`
/// and the caller takes the unprivileged path.
#[cfg(unix)]
pub fn is_superuser() -> bool {
    current_uid() == 0
}

#[cfg(windows)]
pub fn is_superuser() -> bool {
    false
}

/// Like [`signal_pid`], but reports whether the signal was delivered and why
/// not, so the caller can escalate (re-send under `sudo` on `EPERM`) or tell
/// the user rather than print a `TERM` line for a signal that never landed.
/// The error is the OS's own (`PermissionDenied` for a process that is not
/// ours, `NotFound`-class for one that already exited).
#[cfg(unix)]
pub fn try_signal_pid(pid: i32, signal: Signal) -> std::io::Result<()> {
    if !is_real_pid(pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid pid {pid}"),
        ));
    }
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // Safe: kill() on an arbitrary pid is defined; we only read the result.
    if unsafe { libc::kill(pid, sig) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub fn try_signal_pid(pid: i32, _signal: Signal) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if !is_real_pid(pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid pid {pid}"),
        ));
    }
    // Safe: a null handle is checked before use.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let result = if TerminateProcess(h, 1) != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        };
        CloseHandle(h);
        result
    }
}

/// Deliver `signal` to `pid`, best-effort. Failure (already exited, not ours)
/// is silently ignored on both platforms.
#[cfg(unix)]
pub fn signal_pid(pid: i32, signal: Signal) {
    if !is_real_pid(pid) {
        return;
    }
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // Safe: sending a signal to a pid we just enumerated; failure is fine.
    unsafe {
        libc::kill(pid, sig);
    }
}

#[cfg(windows)]
pub fn signal_pid(pid: i32, _signal: Signal) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if !is_real_pid(pid) {
        return;
    }
    // Safe: a null handle is checked; TerminateProcess on a live handle is
    // defined, and failure is ignored by contract.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if h.is_null() {
            return;
        }
        TerminateProcess(h, 1);
        CloseHandle(h);
    }
}

/// A shell invocation for an opaque command string from the lab file.
///
/// Power hooks (`on_cmd`, `off_cmd`, …) and the hid `cmd` are written by the
/// user, may contain pipelines or redirection, and so have to go through a
/// shell rather than being split into an argv. That shell is `sh` on Unix and
/// `cmd.exe` on Windows.
///
/// A lab file's command strings are already platform-specific — one naming
/// `/dev/cu.usbmodem…` is macOS-only however it is run — so this does not make
/// a Unix lab file portable. It makes a *Windows* lab file (`COM4`,
/// `ch9329.exe`) runnable at all, which it was not: `Command::new("sh")`
/// compiles everywhere and fails to launch on Windows, which is how this
/// shipped broken.
#[cfg(unix)]
pub fn shell_command(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    c
}

#[cfg(windows)]
pub fn shell_command(script: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("cmd");
    c.arg("/C");
    // cmd.exe does not follow the argument-quoting rules std applies, so the
    // script is handed over verbatim; std's escaping would mangle any quoting
    // the user wrote.
    c.raw_arg(script);
    c
}

/// Detach `cmd` so the spawned daemon outlives this CLI process.
///
/// Unix puts it in its own process group, so a Ctrl-C to the CLI's group is not
/// forwarded. Windows has no process groups in that sense; `DETACHED_PROCESS`
/// plus `CREATE_NEW_PROCESS_GROUP` is the closest equivalent — the child gets
/// no console and does not receive the parent's Ctrl-C/Ctrl-Break.
#[cfg(unix)]
pub fn detach(cmd: &mut Command) {
    std::os::unix::process::CommandExt::process_group(cmd, 0);
}

#[cfg(windows)]
pub fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Start `cmd` as the leader of its own process group, so
/// [`kill_process_tree`] can take down everything it spawned — the shell a
/// hook runs under *and* the helper the shell started — rather than only the
/// shell. Windows has no Unix process groups and needs nothing here: the
/// caller falls back to killing the one process it holds.
#[cfg(unix)]
pub fn own_process_group(cmd: &mut Command) {
    std::os::unix::process::CommandExt::process_group(cmd, 0);
}

#[cfg(windows)]
pub fn own_process_group(_cmd: &mut Command) {}

/// SIGKILL the process group led by `pid` (one started with
/// [`own_process_group`]). Best-effort, like [`signal_pid`]; a non-positive
/// pid is refused, since `kill(-0)` and `kill(-(-1))` would reach far more
/// than one group. On Windows this is [`signal_pid`] on the one process.
#[cfg(unix)]
pub fn kill_process_tree(pid: i32) {
    if !is_real_pid(pid) {
        return;
    }
    // Safe: kill() on a negative pid signals that group; failure is fine.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn kill_process_tree(pid: i32) {
    signal_pid(pid, Signal::Kill);
}

/// Hand this process over to `cmd`, never returning on success.
///
/// Unix `exec`s, so the child inherits the pid and the terminal outright —
/// which is what an interactive console (`tio`, `adb shell`) wants. Windows has
/// no `exec`, so we spawn, wait, and exit with the child's status. The
/// observable difference is that this process stays alive as a parent while the
/// child runs; stdio is inherited either way, so an interactive session behaves
/// the same.
#[cfg(unix)]
pub fn exec_replace(cmd: &mut Command) -> std::io::Error {
    std::os::unix::process::CommandExt::exec(cmd)
}

#[cfg(windows)]
pub fn exec_replace(cmd: &mut Command) -> std::io::Error {
    match cmd.status() {
        // A passthrough (design D3): the console's own status.
        Ok(status) => std::process::exit(crate::error::shell_code(status)),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child process that will sit still until we kill it.
    ///
    /// `sleep`/`timeout` are the one command with a long-running form on both
    /// platforms; the point is only to have a live pid to probe.
    fn spawn_sleeper() -> std::process::Child {
        if cfg!(windows) {
            // `timeout` needs a console; ping to loopback is the portable idle.
            let mut c = Command::new("cmd");
            c.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        }
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sleeper")
    }

    /// The daemon machinery lives or dies on these two: every `discover()`
    /// asks whether a recorded pid is alive, and every `stop` terminates one.
    /// Both are hand-written per platform — on Windows they are raw Win32
    /// calls — so they get an actual process rather than a mocked one.
    #[test]
    fn pid_alive_tracks_a_real_process() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        assert!(pid_alive(pid), "a just-spawned child must read as alive");

        try_signal_pid(pid, Signal::Kill).expect("terminating our own child must succeed");
        let _ = child.wait();

        // Windows can take a moment to tear the process down, and a reaped
        // Unix pid is gone immediately; poll briefly rather than race.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!pid_alive(pid), "a terminated child must read as dead");
    }

    #[test]
    fn pid_alive_rejects_impossible_pids() {
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn signal_pid_on_a_dead_pid_is_survivable() {
        // Best-effort by contract: reaping an already-dead daemon must not
        // panic or hang, on either platform.
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        let _ = try_signal_pid(pid, Signal::Kill);
        let _ = child.wait();
        signal_pid(pid, Signal::Term);
        signal_pid(pid, Signal::Kill);
    }

    /// `Command::new("sh")` compiles on Windows and fails to launch, which is
    /// how `doctor` and the power hooks shipped broken. Run a real command
    /// through the shell wrapper and read its exit code back.
    #[test]
    fn shell_command_actually_runs_a_command() {
        let status = shell_command("exit 7").status().expect("shell must launch");
        assert_eq!(status.code(), Some(7));

        let ok = shell_command("exit 0").status().expect("shell must launch");
        assert!(ok.success());
    }

    #[test]
    fn runtime_root_is_absolute_and_usable() {
        // The daemon runtime dir hangs off this; a relative or empty root
        // would put daemon state somewhere unpredictable.
        let root = default_runtime_root();
        assert!(root.is_absolute(), "{root:?} must be absolute");
    }

    #[test]
    fn ensure_private_dir_creates_then_revalidates() {
        let tmp = std::env::temp_dir().join(format!("panio-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        ensure_private_dir(&tmp).expect("first call creates");
        assert!(tmp.is_dir());
        // Second call takes the already-exists path, including the Unix
        // ownership check — the branch a running daemon hits every time.
        ensure_private_dir(&tmp).expect("second call revalidates");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_private_dir_rejects_a_non_directory() {
        let tmp = std::env::temp_dir().join(format!("panio-test-file-{}", std::process::id()));
        std::fs::write(&tmp, b"not a directory").unwrap();
        assert!(
            ensure_private_dir(&tmp).is_err(),
            "a squatted path must be refused, not silently used"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// A base left group/world-accessible by an earlier creator (an older
    /// panio's plain `create_dir_all`, or the user by hand) is ours to
    /// tighten: after the call it must be 0700, so nothing beneath it — a
    /// discovery file, a capture log — is reachable by other users.
    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_tightens_an_open_dir_we_own() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = std::env::temp_dir().join(format!("panio-test-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir(&tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::metadata(&tmp).unwrap().mode() & 0o777, 0o755);
        assert!(!is_private_dir(&tmp), "a 0755 dir is not private");

        ensure_private_dir(&tmp).expect("an open dir we own is tightened, not refused");
        assert_eq!(std::fs::metadata(&tmp).unwrap().mode() & 0o777, 0o700);
        assert!(is_private_dir(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A symlink at the base — even one pointing at a private directory we
    /// own — is refused by both the writer and the reader: a squatter's link
    /// would redirect every discovery file and capture log to a place of
    /// their choosing.
    #[cfg(unix)]
    #[test]
    fn private_dir_checks_reject_a_symlink() {
        let real = std::env::temp_dir().join(format!("panio-test-real-{}", std::process::id()));
        let link = std::env::temp_dir().join(format!("panio-test-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        ensure_private_dir(&real).unwrap();
        assert!(is_private_dir(&real));
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(
            ensure_private_dir(&link).is_err(),
            "a symlinked base must be refused"
        );
        assert!(!is_private_dir(&link));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&real);
    }

    /// `write_atomic` replaces the destination by rename rather than
    /// truncating it in place — a concurrent reader (`state::load_netboot_state`,
    /// a lab load) sees either the old content or the new content whole, never
    /// a partial write, and a crash mid-write leaves the previous file intact.
    #[cfg(unix)]
    #[test]
    fn write_atomic_replaces_the_file_by_rename() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"first").unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();
        write_atomic(&path, b"second, and longer than the first payload").unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            before,
            "the file must be replaced whole, not rewritten in place"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "second, and longer than the first payload"
        );
        // No temp file left beside it.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["state.json"], "{names:?}");
    }

    /// A relative, nonexistent destination in the current directory is still
    /// resolved and written — `write_atomic` must not assume `path` already
    /// exists to find where to put the temp file.
    #[test]
    fn write_atomic_creates_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        assert!(!path.exists());
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn start_time_of_self_is_stable() {
        let pid = std::process::id() as i32;
        let a = process_start_time(pid);
        assert!(a.is_some());
        assert_eq!(a, process_start_time(pid));
    }

    #[test]
    fn start_time_of_child_differs_from_self() {
        let mut child = spawn_sleeper();
        let theirs = process_start_time(child.id() as i32);
        let ours = process_start_time(std::process::id() as i32);
        let _ = child.kill();
        let _ = child.wait();
        assert!(theirs.is_some());
        assert_ne!(theirs, ours);
    }

    #[test]
    fn start_time_of_dead_pid_is_none() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(process_start_time(pid), None);
    }

    #[test]
    fn random_token_shape() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(a, b);
    }
}
