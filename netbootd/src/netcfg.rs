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

//! Host network glue: ARP pinning and interface-IP enforcement.
//!
//! Like the Python version, this shells out to the platform tools rather than
//! going native (netlink / route sockets). For the PoC that keeps the surface
//! small; going native is a follow-up if we want to drop the `sudo` shell-outs.
//!
//! The shell-outs that run while the daemon is serving (`set_arp` from the
//! DHCP task, the `ifconfig`/`ip` probes and re-apply from the IP monitor) go
//! through `tokio::process` so a slow or prompting `sudo` parks one task
//! instead of a whole runtime worker; only the one-shot default-route lookup
//! at startup blocks. On Linux these `sudo` calls run *after* the daemon has
//! dropped root (see `privdrop`), which is why passwordless sudo is a
//! documented requirement there.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::process::Command;
use tracing::warn;

/// The interface carrying the system default route, if any.
///
/// macOS: the library's [`netbootd::route::default_route_interface`]
/// (`/sbin/route -n get default`), shared with the setuid helper so the
/// daemon's guard and the helper's refusal can never disagree.
#[cfg(target_os = "macos")]
fn default_route_interface() -> Option<String> {
    netbootd::route::default_route_interface()
}

/// The interface carrying the system default route, if any.
///
/// Linux: `ip route show default` emits `default via <gw> dev en0 ...`.
#[cfg(not(target_os = "macos"))]
fn default_route_interface() -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut toks = s.split_whitespace();
    while let Some(tok) = toks.next() {
        if tok == "dev" {
            return toks.next().map(str::to_string);
        }
    }
    None
}

/// Whether `interface` is a primary NIC — i.e. it carries the system default
/// route. The netboot link must be a *dedicated secondary* interface: forcing
/// our static `192.168.99.1` onto a primary NIC (via [`apply_interface_ip`])
/// clobbers the host's real networking. Used to refuse that case loudly rather
/// than silently break connectivity.
pub fn is_primary_interface(interface: &str) -> bool {
    default_route_interface().as_deref() == Some(interface)
}

/// Pin a static ARP/neighbor entry mapping `ip` → `mac`.
///
/// The Pi netboot firmware sends DHCP/TFTP but never answers ARP, so we install
/// the MAC we just saw in the DHCP frame directly. Needs root.
///
/// - macOS: `arp -s <ip> <mac>`
/// - Linux: `ip neigh replace <ip> lladdr <mac> nud permanent dev <iface>`
pub async fn set_arp(ip: Ipv4Addr, mac: &str, interface: &str) {
    let status = if cfg!(target_os = "macos") {
        Command::new("sudo")
            .args(["arp", "-s", &ip.to_string(), mac])
            .status()
            .await
    } else {
        Command::new("sudo")
            .args([
                "ip",
                "neigh",
                "replace",
                &ip.to_string(),
                "lladdr",
                mac,
                "nud",
                "permanent",
                "dev",
                interface,
            ])
            .status()
            .await
    };
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => warn!("ARP pin {ip} -> {mac} exited {s}"),
        Err(e) => warn!("ARP pin {ip} -> {mac} failed to spawn: {e}"),
    }
}

/// Whether `host_ip` is currently assigned to `interface`.
async fn has_interface_ip(interface: &str, host_ip: Ipv4Addr) -> bool {
    if cfg!(target_os = "macos") {
        Command::new("ifconfig")
            .arg(interface)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("inet {host_ip} ")))
            .unwrap_or(false)
    } else {
        Command::new("ip")
            .args(["addr", "show", "dev", interface])
            .output()
            .await
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains(&format!("inet {host_ip}/")) || s.contains(&format!("inet {host_ip} "))
            })
            .unwrap_or(false)
    }
}

async fn is_link_up(interface: &str) -> bool {
    if cfg!(target_os = "macos") {
        Command::new("ifconfig")
            .arg(interface)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("status: active"))
            .unwrap_or(false)
    } else {
        tokio::fs::read_to_string(format!("/sys/class/net/{interface}/carrier"))
            .await
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }
}

/// (Re-)assign `host_ip` to the interface. The netmask is the fixed /24 the
/// whole netboot link is built on: DHCP advertises the same
/// `255.255.255.0` and derives the client's lease inside it
/// (`dhcp::derive_client_ip`), and `panio netboot start` configures the
/// interface with the same mask before spawning us.
async fn apply_interface_ip(interface: &str, host_ip: Ipv4Addr) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("sudo")
            .args([
                "ifconfig",
                interface,
                &host_ip.to_string(),
                "netmask",
                "255.255.255.0",
                "up",
            ])
            .status()
            .await
    } else {
        Command::new("sudo")
            .args([
                "ip",
                "addr",
                "add",
                &format!("{host_ip}/24"),
                "dev",
                interface,
            ])
            .status()
            .await
    };
}

/// Continuously enforce the static IP on `interface`.
///
/// The netboot client flaps the link on every power-cycle and at several points
/// during its own boot; macOS drops a manually-set IPv4 on flap and Linux's
/// NetworkManager may reset it. Poll and re-apply so the client's next retry
/// always finds the host reachable. Runs until the task is cancelled.
pub async fn monitor_interface(interface: String, host_ip: Ipv4Addr) {
    // Defense in depth: never enforce a static IP on a primary NIC. main()
    // already refuses to start in that case; this guards direct callers.
    if is_primary_interface(&interface) {
        warn!(
            "{interface} carries the default route — refusing to enforce {host_ip} \
             on a primary NIC; IP monitor disabled"
        );
        return;
    }
    let mut had_ip = true;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let has_ip = has_interface_ip(&interface, host_ip).await;
        let active = is_link_up(&interface).await;
        if !has_ip && active {
            apply_interface_ip(&interface, host_ip).await;
            if had_ip {
                warn!("interface {interface} lost IP {host_ip} — restoring");
            }
        } else if has_ip && !had_ip {
            tracing::info!("interface {interface} restored with IP {host_ip}");
        }
        had_ip = has_ip;
    }
}
