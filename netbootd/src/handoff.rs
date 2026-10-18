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

//! Privilege-separated acquisition of a `/dev/bpf` send descriptor.
//!
//! On macOS, opening `/dev/bpf` requires root (or membership in the `access_bpf`
//! group). To keep `netbootd` itself unprivileged, a tiny **setuid-root** helper
//! — `netbootd-bpf-helper` — does the only privileged work: it opens `/dev/bpfN`
//! **write-only**, binds it to the netboot interface (`BIOCSETIF`), sets
//! `BIOCSHDRCMPLT` so the caller supplies the full L2 header, installs a
//! reject-all filter (`BIOCSETF`) so the descriptor can never capture, and
//! passes the open descriptor back over a `socketpair` via `SCM_RIGHTS`, then
//! exits.
//!
//! Why this works: on BSD/macOS, `/dev/bpf` access is checked at `open()` time.
//! Once the descriptor is open for writing and bound, its send capability
//! travels with the fd — the unprivileged `netbootd` that receives it can
//! `write()` raw frames to it regardless of its own uid. `BIOCSHDRCMPLT` is a
//! per-descriptor flag, so setting it once in the helper persists on the passed
//! fd (and sidesteps the macOS bug where toggling it per-write can break
//! injection).
//!
//! What the descriptor can *not* do: it is opened `O_WRONLY`, so `read(2)` on
//! it fails at the VFS layer (no `FREAD`), and the reject-all filter means the
//! kernel never queues a captured packet to it in the first place. Whoever
//! holds it has a send-only handle on one interface, nothing more.
//!
//! Who may ask for one: the helper is world-executable (mode 4755) but refuses
//! every caller except the user who installed it — the owner of the directory
//! the helper lives in ([`caller_allowed`], [`helper_owner_uid`]) — and refuses
//! to bind the interface carrying the host's default route
//! ([`interface_refused`]). Both checks run before `/dev/bpf` is touched.
//!
//! This module is shared by both the helper (the *send* side: [`open_bpf`] +
//! [`send_fd`]) and the daemon (the *receive* side: [`request_bpf_fd`], which
//! spawns the helper and calls [`recv_fd`]).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// macOS BPF ioctls (64-bit), identical to the constants `_tftp.py` hardcodes.
#[cfg(target_os = "macos")]
const BIOCSETIF: libc::c_ulong = 0x8020_426C; // bind fd to an interface (struct ifreq)
#[cfg(target_os = "macos")]
const BIOCSHDRCMPLT: libc::c_ulong = 0x8004_4275; // we write complete L2 headers
/// `_IOW('B', 103, struct bpf_program)`; `bpf_program` is 16 bytes on 64-bit
/// macOS (a `u32` length padded out to the pointer that follows it).
#[cfg(target_os = "macos")]
const BIOCSETF: libc::c_ulong = 0x8010_4267; // install a filter program (ours rejects everything)

/// `BPF_RET` / `BPF_K` from `<net/bpf.h>`: "return the constant `k`". The libc
/// crate does not expose them for macOS.
#[cfg(target_os = "macos")]
const BPF_RET: u16 = 0x06;
#[cfg(target_os = "macos")]
const BPF_K: u16 = 0x00;

/// `struct bpf_insn` from `<net/bpf.h>`: 8 bytes on 64-bit macOS.
#[cfg(target_os = "macos")]
#[repr(C)]
struct BpfInsn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// `struct bpf_program` from `<net/bpf.h>`: 16 bytes on 64-bit macOS. Its
/// size is baked into [`BIOCSETF`], so a layout mismatch here would make the
/// ioctl fail with `ENOTTY` rather than silently misbehave.
#[cfg(target_os = "macos")]
#[repr(C)]
struct BpfProgram {
    bf_len: u32,
    bf_insns: *const BpfInsn,
}

/// The one-instruction program `ret #0`: accept zero bytes of every packet, so
/// nothing is ever captured into the descriptor's buffer. Defense in depth
/// behind the `O_WRONLY` open — even a descriptor that could somehow be read
/// would have nothing to read.
#[cfg(target_os = "macos")]
const REJECT_ALL: [BpfInsn; 1] = [BpfInsn {
    code: BPF_RET | BPF_K,
    jt: 0,
    jf: 0,
    k: 0,
}];

/// Whether a caller whose *real* uid is `caller_uid` may use the setuid
/// helper, given that the directory holding the helper is owned by
/// `owner_uid`.
///
/// The helper is installed into the installing user's private libexec dir
/// (`~/.local/libexec/panio/bin`) or into a Homebrew keg owned by the brew
/// user, so the directory owner *is* the one user authorised to use it:
/// everyone else is refused before `/dev/bpf` is touched, even though the
/// binary itself is world-executable (mode 4755). Root is always allowed — it
/// could open `/dev/bpf` itself anyway.
pub fn caller_allowed(caller_uid: u32, owner_uid: u32) -> bool {
    caller_uid == 0 || caller_uid == owner_uid
}

/// Whether the helper must refuse to bind `requested`: true exactly when it is
/// the interface carrying the host's default route (the primary NIC), which
/// netboot must never touch. `None` — no default route, or it could not be
/// determined — refuses nothing: an offline bench has no primary NIC to
/// protect, and the daemon's own guard (`netcfg::is_primary_interface`) fails
/// open the same way.
pub fn interface_refused(requested: &str, default_route: Option<&str>) -> bool {
    default_route == Some(requested)
}

/// The uid that owns the directory holding the running helper executable —
/// the "installing user" that [`caller_allowed`] compares against.
///
/// The executable path is canonicalised first, so a symlink to the helper
/// placed in a directory the caller owns does not count; and a helper with
/// more than one hard link is refused outright, because a hard link in a
/// directory of the caller's choosing shares the real file's inode and would
/// pass every other check. When the process is actually privileged
/// (`geteuid() == 0`), the file it resolved to must also be the root-owned
/// setuid binary `panio setup` installed — anything else means the path we
/// were exec'd through was swapped underneath us.
#[cfg(target_os = "macos")]
pub fn helper_owner_uid() -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;

    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let meta = std::fs::metadata(&exe)?;
    if meta.nlink() != 1 {
        return Err(io::Error::other(format!(
            "{} has {} hard links; refusing to trust its directory",
            exe.display(),
            meta.nlink()
        )));
    }
    let privileged = unsafe { libc::geteuid() } == 0;
    if privileged && (meta.uid() != 0 || meta.mode() & libc::S_ISUID as u32 == 0) {
        return Err(io::Error::other(format!(
            "{} is not the root-owned setuid helper installed by `panio setup`",
            exe.display()
        )));
    }
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent directory", exe.display())))?;
    Ok(std::fs::metadata(dir)?.uid())
}

/// Reject interface names that are empty, too long for `struct ifreq`, or
/// contain anything but ASCII alphanumerics. The helper runs setuid-root, so it
/// must not trust this string even though netbootd is the expected caller.
#[cfg(target_os = "macos")]
fn validate_iface(iface: &str) -> io::Result<()> {
    if iface.is_empty() || iface.len() >= 16 || !iface.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid interface name {iface:?}"),
        ));
    }
    Ok(())
}

/// Open a **write-only** `/dev/bpf` descriptor bound to `iface`, with
/// `BIOCSHDRCMPLT` set and the reject-all filter installed. The privileged
/// operation — only callable by the setuid helper. Tries `/dev/bpf0..256`,
/// skipping devices already in use (`EBUSY`).
///
/// `O_WRONLY` is the primary containment: the ioctls do not need read
/// permission, and without `FREAD` on the file a `read(2)` fails at the VFS
/// layer before the bpf driver ever sees it. The filter is the backstop.
#[cfg(target_os = "macos")]
pub fn open_bpf(iface: &str) -> io::Result<OwnedFd> {
    validate_iface(iface)?;

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no free /dev/bpf device available");
    for n in 0..256 {
        let path = std::ffi::CString::new(format!("/dev/bpf{n}")).unwrap();
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY) };
        if raw < 0 {
            last_err = io::Error::last_os_error();
            match last_err.raw_os_error() {
                // Device busy — try the next one.
                Some(libc::EBUSY) => continue,
                // No such device — we have run off the end of the cloning range.
                Some(libc::ENOENT) | Some(libc::ENXIO) => break,
                // Permission denied etc. — surface immediately.
                _ => return Err(last_err),
            }
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        // BIOCSETIF: bind to the interface. struct ifreq is 32 bytes; the name
        // goes in the first field, NUL-padded.
        let mut ifreq = [0u8; 32];
        ifreq[..iface.len()].copy_from_slice(iface.as_bytes());
        if unsafe { libc::ioctl(fd.as_raw_fd(), BIOCSETIF, ifreq.as_ptr()) } != 0 {
            // Interface-level failure (e.g. no such interface) — not per-device,
            // so retrying other bpf nodes will not help.
            return Err(io::Error::last_os_error());
        }

        // BIOCSHDRCMPLT(1): we supply the source MAC ourselves.
        let one: u32 = 1;
        if unsafe { libc::ioctl(fd.as_raw_fd(), BIOCSHDRCMPLT, &one as *const u32) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // BIOCSETF(ret #0): capture nothing, ever.
        let prog = BpfProgram {
            bf_len: REJECT_ALL.len() as u32,
            bf_insns: REJECT_ALL.as_ptr(),
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), BIOCSETF, &prog as *const BpfProgram) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(fd);
    }
    Err(last_err)
}

/// Send `fd` to the peer of the connected unix socket `sock` via `SCM_RIGHTS`,
/// alongside a single dummy data byte (some platforms drop ancillary data on a
/// zero-length payload).
pub fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    let mut dummy = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: dummy.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const RawFd as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<RawFd>(),
        );
    }

    if unsafe { libc::sendmsg(sock, &msg, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Mark `fd` close-on-exec. A descriptor that arrives over `SCM_RIGHTS` (or
/// out of `socketpair`) does not have the flag, and netbootd goes on to spawn
/// `sudo arp` / `ifconfig` children that must not inherit a raw send handle.
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive a single descriptor sent with [`send_fd`] from the connected unix
/// socket `sock`. The returned descriptor is close-on-exec.
pub fn recv_fd(sock: RawFd) -> io::Result<OwnedFd> {
    let mut dummy = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: dummy.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    let fd = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no SCM_RIGHTS control message received",
            ));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg),
            &mut fd as *mut RawFd as *mut u8,
            std::mem::size_of::<RawFd>(),
        );
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "received invalid fd",
            ));
        }
        OwnedFd::from_raw_fd(fd)
    };
    set_cloexec(fd.as_raw_fd())?;
    Ok(fd)
}

/// Locate the `netbootd-bpf-helper` binary: prefer the copy installed next to
/// the running `netbootd` executable, otherwise fall back to `PATH`.
#[cfg(target_os = "macos")]
fn locate_helper() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("netbootd-bpf-helper");
            if cand.exists() {
                return cand;
            }
        }
    }
    std::path::PathBuf::from("netbootd-bpf-helper")
}

/// Spawn the setuid helper and receive a bound, write-only `/dev/bpf`
/// descriptor over a `socketpair`. The daemon side of the handoff.
///
/// The helper inherits the child end of the socketpair as fd 3, writes the bpf
/// fd back via `SCM_RIGHTS`, and exits. Failure here is non-fatal to the caller:
/// netbootd logs it and falls back to the kernel `send_to` path. When the
/// helper exits non-zero — not setuid, caller refused, primary NIC refused —
/// the error carries its exit status and stderr, so the reason is in the log
/// rather than a bare "no SCM_RIGHTS control message received".
#[cfg(target_os = "macos")]
pub fn request_bpf_fd(iface: &str) -> io::Result<OwnedFd> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut sv = [0 as RawFd; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let parent = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let child = unsafe { OwnedFd::from_raw_fd(sv[1]) };
    // Neither end may leak into any other child this process spawns. The
    // helper still gets its end: dup2 onto fd 3 below clears FD_CLOEXEC on the
    // copy.
    set_cloexec(parent.as_raw_fd())?;
    set_cloexec(child.as_raw_fd())?;

    let helper = locate_helper();
    let child_raw = child.as_raw_fd();

    let mut cmd = Command::new(&helper);
    cmd.arg("--interface")
        .arg(iface)
        .arg("--handoff-fd")
        .arg("3")
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(move || {
            // Move the child socketpair end to fd 3 (dup2 clears FD_CLOEXEC).
            if libc::dup2(child_raw, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let proc = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("spawn {}: {e}", helper.display())))?;
    // Parent no longer needs the child end.
    drop(child);

    // recv_fd returns once the helper has sent the fd or exited (closing its
    // end). Only then drain stderr and reap it — the helper writes at most a
    // line, so it cannot block on a full pipe before it gets that far.
    let fd = recv_fd(parent.as_raw_fd());
    let output = proc.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "netbootd-bpf-helper exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    fd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_allowed_only_for_the_directory_owner_or_root() {
        assert!(caller_allowed(501, 501));
        assert!(caller_allowed(0, 501));
        assert!(caller_allowed(0, 0));
        assert!(!caller_allowed(502, 501));
        assert!(!caller_allowed(501, 0));
        // macOS `nobody` (uid -2) trying a helper in a user's libexec dir.
        assert!(!caller_allowed(4_294_967_294, 501));
    }

    #[test]
    fn interface_refused_only_for_the_default_route_interface() {
        assert!(interface_refused("en0", Some("en0")));
        assert!(!interface_refused("en11", Some("en0")));
        assert!(!interface_refused("en0", None));
        assert!(!interface_refused("", Some("en0")));
    }

    /// The ioctl numbers bake in the byte size of their argument, so the Rust
    /// mirrors of `bpf_insn` / `bpf_program` must match the C layout exactly.
    /// Re-derive every constant from `_IOW` and cross-check `BIOCSETF` against
    /// the libc crate's own value.
    #[cfg(target_os = "macos")]
    #[test]
    fn bpf_ioctls_match_their_argument_layouts() {
        use std::mem::size_of;

        // <sys/ioccom.h>: IOC_IN | ((len & IOCPARM_MASK) << 16) | (group << 8) | num
        fn iow(group: u8, num: u8, len: usize) -> libc::c_ulong {
            0x8000_0000
                | (((len & 0x1fff) as libc::c_ulong) << 16)
                | ((group as libc::c_ulong) << 8)
                | num as libc::c_ulong
        }

        assert_eq!(size_of::<BpfInsn>(), 8);
        assert_eq!(size_of::<BpfProgram>(), 16);
        assert_eq!(BIOCSETF, iow(b'B', 103, size_of::<BpfProgram>()));
        assert_eq!(BIOCSETF, libc::BIOCSETF);
        assert_eq!(BIOCSETIF, iow(b'B', 108, 32));
        assert_eq!(BIOCSHDRCMPLT, iow(b'B', 117, size_of::<u32>()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reject_all_is_a_single_return_of_zero_bytes() {
        assert_eq!(REJECT_ALL.len(), 1);
        assert_eq!(REJECT_ALL[0].code, 0x06);
        assert_eq!(REJECT_ALL[0].k, 0);
        assert_eq!((REJECT_ALL[0].jt, REJECT_ALL[0].jf), (0, 0));
    }

    /// A descriptor that comes out of `recv_fd` must be close-on-exec so the
    /// daemon's later `sudo arp` / `ifconfig` children cannot inherit it.
    #[test]
    fn received_fd_is_close_on_exec() {
        let mut sv = [0 as RawFd; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) },
            0
        );
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
        // Something to pass: a pipe end, deliberately without FD_CLOEXEC.
        let mut p = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        let (pr, pw) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
        assert_eq!(
            unsafe { libc::fcntl(pw.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        send_fd(a.as_raw_fd(), pw.as_raw_fd()).unwrap();
        let got = recv_fd(b.as_raw_fd()).unwrap();
        let flags = unsafe { libc::fcntl(got.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "received fd is not FD_CLOEXEC");

        // And it really is the pipe's write end: a byte written through it
        // comes out of the read end.
        assert_eq!(
            unsafe { libc::write(got.as_raw_fd(), b"x".as_ptr() as *const libc::c_void, 1) },
            1
        );
        let mut buf = [0u8; 1];
        assert_eq!(
            unsafe { libc::read(pr.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, 1) },
            1
        );
        assert_eq!(&buf, b"x");
    }
}
