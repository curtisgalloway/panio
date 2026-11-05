// Copyright 2026 The Panio Authors
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

//! macOS raw-frame sender — the port of `_tftp.py`'s `BpfSender`.
//!
//! Why this exists: on macOS 15+ the kernel will not deliver unicast UDP to the
//! Pi bootloader even with a permanent static ARP entry, because the bootloader
//! sends from a different ephemeral source MAC than its DHCP MAC and the kernel
//! caches that wrong MAC. The only reliable fix is to stop trusting the kernel's
//! L2 resolution and inject a complete Ethernet frame addressed to the Pi's real
//! DHCP MAC (learned in-process from our own DHCP handler).
//!
//! Privilege model: opening `/dev/bpf` needs root, but netbootd runs
//! unprivileged. So the bound, `BIOCSHDRCMPLT`-set descriptor is opened by the
//! setuid-root `netbootd-bpf-helper` and handed over via `SCM_RIGHTS` (see
//! `netbootd::handoff` in the lib). The descriptor arrives **write-only** with
//! a reject-all filter installed — it can inject, never capture — which is all
//! this `BpfSender` needs: it only ever *writes* complete frames to that
//! descriptor with `libc::write` — the raw "libc BPF path" the previous
//! `pnet_datalink`-based version left unimplemented. Because `BIOCSHDRCMPLT` is
//! set once on the fd by the helper, we avoid the macOS bug where toggling it
//! per-write breaks injection.
//!
//! Portability: this module compiles on every platform so the TFTP call sites
//! stay type-checked, but netbootd only constructs a live sender on macOS via
//! [`BpfSender::from_handoff`] — elsewhere [`BpfSender::unavailable`] is used and
//! TFTP falls back to ordinary `send_to`, matching the Python behavior.

#[cfg(unix)]
use std::net::Ipv4Addr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};

#[cfg(unix)]
use netbootd::frame::build_udp_frame;
#[cfg(unix)]
use tracing::warn;

/// Sends UDP datagrams as raw Ethernet frames, bypassing the kernel ARP table.
/// The descriptor is a `/dev/bpf` fd received from the privileged helper.
pub struct BpfSender {
    /// Source MAC for the frames we build. Only the raw-frame path reads it,
    /// so off Unix it exists purely to keep `unavailable()` uniform.
    #[cfg_attr(not(unix), allow(dead_code))]
    src_mac: Option<[u8; 6]>,
    /// The `/dev/bpf` descriptor. Windows has no such device and no file
    /// descriptors to hold, so the field simply does not exist there and the
    /// sender is permanently inert.
    #[cfg(unix)]
    fd: Option<OwnedFd>,
}

impl BpfSender {
    /// An inert sender. `available()` is always false; `send_udp()` is a no-op.
    pub fn unavailable() -> Self {
        Self {
            src_mac: None,
            #[cfg(unix)]
            fd: None,
        }
    }

    /// Build a sender from a `/dev/bpf` descriptor handed over by the privileged
    /// helper (already bound to the interface with `BIOCSHDRCMPLT` set) and the
    /// interface's own MAC (read unprivileged by the caller).
    // Constructed only on the macOS send path; dead on a Linux build by design.
    #[cfg(unix)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn from_handoff(fd: OwnedFd, src_mac: [u8; 6]) -> Self {
        tracing::info!(
            "BPF sender ready (handoff fd {}, src {})",
            fd.as_raw_fd(),
            mac_hex(&src_mac)
        );
        Self {
            src_mac: Some(src_mac),
            fd: Some(fd),
        }
    }

    #[cfg(unix)]
    pub fn available(&self) -> bool {
        self.fd.is_some() && self.src_mac.is_some()
    }

    /// Always false off Unix: there is no raw-frame path to be available.
    #[cfg(not(unix))]
    pub fn available(&self) -> bool {
        false
    }

    /// Inject `payload` as a UDP datagram in a raw frame to `dst_mac`. Returns
    /// false (caller should fall back to `send_to`) if unavailable or the write
    /// fails.
    // Called only from the macOS send path; dead on a Linux build by design.
    #[cfg(unix)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn send_udp(
        &self,
        dst_mac: [u8; 6],
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) -> bool {
        let (Some(src_mac), Some(fd)) = (self.src_mac, self.fd.as_ref()) else {
            return false;
        };
        let Some(frame) = build_udp_frame(
            src_mac, dst_mac, src_ip, dst_ip, src_port, dst_port, payload,
        ) else {
            warn!(
                "BPF: {}-byte payload does not fit one UDP datagram; not sent",
                payload.len()
            );
            return false;
        };
        // A single write(2) to a BPF descriptor transmits exactly one frame and
        // is atomic, so concurrent senders need no extra locking.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
            )
        };
        if n < 0 {
            warn!("BPF write failed: {}", std::io::Error::last_os_error());
            false
        } else if n as usize != frame.len() {
            warn!("BPF short write: {n} of {} bytes", frame.len());
            false
        } else {
            true
        }
    }
}

// Used only by `from_handoff` (macOS); dead on a Linux build.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_hex(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
