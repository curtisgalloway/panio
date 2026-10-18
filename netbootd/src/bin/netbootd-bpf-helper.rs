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

//! `netbootd-bpf-helper` — the *only* privileged component of netboot.
//!
//! Installed setuid-root by `panio setup` (the one-time `sudo`). Its entire
//! job: open `/dev/bpf` **write-only**, bind it to the requested interface, set
//! `BIOCSHDRCMPLT`, install a reject-all filter, and hand the descriptor back
//! to its (unprivileged) caller over the inherited socketpair fd via
//! `SCM_RIGHTS`, then exit. It never reads from the network, never writes
//! frames, and holds root for only microseconds.
//!
//! Because it is setuid-root and world-executable, every input is treated as
//! hostile and two gates run before `/dev/bpf` is touched:
//!
//! 1. **Who is asking.** The caller's *real* uid must be root or the owner of
//!    the directory this executable lives in — the user who installed it
//!    ([`netbootd::handoff::caller_allowed`]). Anyone else gets
//!    `refused: caller uid N is not the installing user (uid M)`.
//! 2. **Which interface.** The interface carrying the host's default route is
//!    refused ([`netbootd::handoff::interface_refused`]) so the helper can
//!    never hand out a send handle on the primary NIC; the lookup runs
//!    `/sbin/route` by absolute path with an empty environment
//!    ([`netbootd::route`]). Refusal reads `refused: <iface> carries the
//!    default route`.
//!
//! The interface name itself is validated in [`netbootd::handoff::open_bpf`],
//! and the handoff fd is the only descriptor the helper writes to.
//!
//! Usage: `netbootd-bpf-helper --interface <name> --handoff-fd <n>`

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::os::fd::AsRawFd;
    use std::process::ExitCode;

    use netbootd::handoff;

    let mut iface: Option<String> = None;
    let mut handoff_fd: Option<i32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interface" => iface = args.next(),
            "--handoff-fd" => handoff_fd = args.next().and_then(|s| s.parse().ok()),
            other => {
                eprintln!("netbootd-bpf-helper: unexpected argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    let (Some(iface), Some(handoff_fd)) = (iface, handoff_fd) else {
        eprintln!("usage: netbootd-bpf-helper --interface <name> --handoff-fd <n>");
        return ExitCode::from(2);
    };

    // Gate 1: only the installing user (or root) may use this helper. Decided
    // on the *real* uid — the setuid bit changes only the effective one.
    let caller = unsafe { libc::getuid() };
    let owner = if caller == 0 {
        0
    } else {
        match handoff::helper_owner_uid() {
            Ok(uid) => uid,
            Err(e) => {
                eprintln!(
                    "netbootd-bpf-helper: refused: cannot establish the installing user: {e}"
                );
                return ExitCode::from(1);
            }
        }
    };
    if !handoff::caller_allowed(caller, owner) {
        eprintln!(
            "netbootd-bpf-helper: refused: caller uid {caller} is not the installing user (uid {owner})"
        );
        return ExitCode::from(1);
    }

    // The route lookup below spawns a child; keep the handoff socket out of it.
    // (Ignore failure: a bad fd number surfaces at send_fd with a clear error.)
    unsafe { libc::fcntl(handoff_fd, libc::F_SETFD, libc::FD_CLOEXEC) };

    // Gate 2: never the primary NIC.
    let default_route = netbootd::route::default_route_interface();
    if handoff::interface_refused(&iface, default_route.as_deref()) {
        eprintln!("netbootd-bpf-helper: refused: {iface} carries the default route");
        return ExitCode::from(1);
    }

    let bpf = match handoff::open_bpf(&iface) {
        Ok(fd) => fd,
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied
                && unsafe { libc::geteuid() } != 0
            {
                " (not running as root — is the helper installed setuid? run `panio setup`)"
            } else {
                ""
            };
            eprintln!("netbootd-bpf-helper: open_bpf({iface}): {e}{hint}");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = handoff::send_fd(handoff_fd, bpf.as_raw_fd()) {
        eprintln!("netbootd-bpf-helper: send_fd: {e}");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("netbootd-bpf-helper is macOS-only (Linux uses the kernel send path)");
    std::process::ExitCode::from(1)
}
