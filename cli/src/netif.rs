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

//! USB-Ethernet link plumbing for the netboot/ffx modes.
//!
//! Ported from `_netboot.py` (interface helpers) + `_netif.py` (modes). The two
//! link modes share one physical point-to-point link and are mutually
//! exclusive: **netboot** = IPv4 host_ip/24 + DHCP/TFTP; **ffx** = host IPv6
//! link-local `fe80::1/64`, no daemons. The active mode is probed from the
//! running daemon + the interface's addresses, never stored.

use std::process::Command;

use anyhow::{bail, Result};

/// The host-side IPv6 link-local that ffx talks through.
pub const FFX_HOST_LL: &str = "fe80::1";
pub const FFX_PREFIX: u8 = 64;

const SUDO_HINT: &str =
    "Ensure passwordless sudo is configured (NOPASSWD) for the control machine.";

fn run(cmd: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(cmd[0]).args(&cmd[1..]).output()
}

fn macos() -> bool {
    cfg!(target_os = "macos")
}

// ── interface discovery / probing ───────────────────────────────────────────

pub struct EthInterface {
    pub port: String,
    pub device: String,
    pub active: bool,
}

pub fn is_interface_active(device: &str) -> bool {
    if macos() {
        run(&["ifconfig", device])
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("status: active"))
            .unwrap_or(false)
    } else {
        std::fs::read_to_string(format!("/sys/class/net/{device}/carrier"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }
}

const MAC_EXCLUDE_PORTS: [&str; 6] = [
    "Wi-Fi",
    "Thunderbolt",
    "Bluetooth",
    "FireWire",
    "iPhone",
    "iPad",
];
const LINUX_SKIP_PREFIXES: [&str; 8] = [
    "lo", "docker", "veth", "br", "virbr", "vlan", "bond", "dummy",
];

/// External (non-built-in) Ethernet interfaces, active ones first. The
/// interface carrying the system default route is excluded — it can never be a
/// netboot link (the start guard refuses to reconfigure the primary NIC), so
/// surfacing it in discovery only invites a misconfiguration.
pub fn list_usb_ethernet_interfaces() -> Vec<EthInterface> {
    let primary = default_route_interface();
    let mut out: Vec<EthInterface> = Vec::new();
    if macos() {
        let Ok(o) = run(&["networksetup", "-listallhardwareports"]) else {
            return out;
        };
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let mut port: Option<String> = None;
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("Hardware Port:") {
                port = Some(p.trim().to_string());
            } else if let Some(d) = line.strip_prefix("Device:") {
                if let Some(p) = port.take() {
                    let device = d.trim().to_string();
                    let excluded = device == "bridge0"
                        || device == "lo0"
                        || primary.as_deref() == Some(device.as_str())
                        || MAC_EXCLUDE_PORTS.iter().any(|x| p.starts_with(x));
                    if !excluded {
                        let active = is_interface_active(&device);
                        out.push(EthInterface {
                            port: p,
                            device,
                            active,
                        });
                    }
                }
            }
        }
    } else if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if LINUX_SKIP_PREFIXES.iter().any(|p| name.starts_with(p))
                || primary.as_deref() == Some(name.as_str())
            {
                continue;
            }
            // Type 1 = Ethernet (ARPHRD_ETHER).
            let is_ether = std::fs::read_to_string(e.path().join("type"))
                .map(|t| t.trim() == "1")
                .unwrap_or(false);
            if !is_ether {
                continue;
            }
            // Wi-Fi also reports ARPHRD_ETHER, but a wireless NIC can never be
            // the point-to-point netboot link; the `wireless` sysfs dir marks it.
            if e.path().join("wireless").exists() {
                continue;
            }
            let active = is_interface_active(&name);
            out.push(EthInterface {
                port: name.clone(),
                device: name,
                active,
            });
        }
    }
    out.sort_by(|a, b| (!a.active, &a.device).cmp(&(!b.active, &b.device)));
    out
}

/// The networksetup service name for a device (macOS only).
fn find_network_service(interface: &str) -> Option<String> {
    if !macos() {
        return None;
    }
    let o = run(&["networksetup", "-listallhardwareports"]).ok()?;
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut service: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("Hardware Port:") {
            service = Some(p.trim().to_string());
        } else if let Some(d) = line.strip_prefix("Device:") {
            if d.trim() == interface {
                return service;
            }
        }
    }
    None
}

/// The interface carrying the system default route, or None.
fn default_route_interface() -> Option<String> {
    if macos() {
        let o = run(&["route", "-n", "get", "default"]).ok()?;
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        for line in text.lines() {
            if let Some(i) = line.trim().strip_prefix("interface:") {
                return Some(i.trim().to_string());
            }
        }
        None
    } else {
        let o = run(&["ip", "route", "show", "default"]).ok()?;
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let toks: Vec<&str> = text.split_whitespace().collect();
        toks.iter()
            .position(|&t| t == "dev")
            .and_then(|i| toks.get(i + 1))
            .map(|s| s.to_string())
    }
}

/// The pure decision behind [`is_primary_interface`]: `default_route` is the
/// interface carrying the system default route (if any), and `interface` is
/// primary when it is that one. Free of probing so it can be unit-tested.
fn is_default_route_interface(interface: &str, default_route: Option<&str>) -> bool {
    default_route == Some(interface)
}

/// True if `interface` carries the system default route — netboot and every
/// netif mode change must never reconfigure the primary NIC.
pub fn is_primary_interface(interface: &str) -> bool {
    is_default_route_interface(interface, default_route_interface().as_deref())
}

/// The refusal every mode change shares with `netboot::start`: never touch the
/// interface carrying the system default route. A lab file naming the host's
/// real NIC would otherwise let `mode link` replace the control host's only
/// address, or `down-hard` admin-down it — an SSH lockout on a remote control
/// host. Pure: `default_route` is what [`default_route_interface`] returned, so
/// both the decision and the message are unit-testable; `what` names the verb.
fn refuse_primary_interface(
    what: &str,
    interface: &str,
    default_route: Option<&str>,
) -> Result<()> {
    if is_default_route_interface(interface, default_route) {
        bail!(
            "refusing netif {what} on '{interface}': it carries the system default \
             route (your primary network interface). netif would reconfigure it and \
             break host networking. Use a dedicated USB-Ethernet adapter for the \
             netboot link."
        );
    }
    Ok(())
}

/// [`refuse_primary_interface`] against the live routing table.
fn guard_primary_interface(what: &str, interface: &str) -> Result<()> {
    refuse_primary_interface(what, interface, default_route_interface().as_deref())
}

/// Every IPv4 address on every interface of this host, as `(interface,
/// address)`. Empty when the listing tool is missing (Windows) or fails, in
/// which case [`guard_shared_subnet`] has nothing to refuse on.
pub fn all_ipv4_addrs() -> Vec<(String, String)> {
    let out = if macos() {
        run(&["ifconfig"])
    } else {
        run(&["ip", "-4", "-o", "addr", "show"])
    };
    let Ok(o) = out else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&o.stdout);
    if macos() {
        parse_ifconfig_inet(&text)
    } else {
        parse_ip_addr_show(&text)
    }
}

/// `(interface, address)` pairs from `ip -4 -o addr show` (all interfaces):
/// one address per line, the interface in the second column, the address
/// (with its prefix stripped) after `inet`. Pure, pinned by a test.
fn parse_ip_addr_show(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let mut toks = line.split_whitespace();
            let _index = toks.next()?;
            let iface = toks.next()?;
            toks.find(|t| *t == "inet")?;
            let addr = toks.next()?.split('/').next()?;
            Some((iface.to_string(), addr.to_string()))
        })
        .collect()
}

/// `(interface, address)` pairs from macOS `ifconfig` (all interfaces): an
/// unindented `en3: flags=…` line opens each interface's block and its
/// indented `inet A.B.C.D netmask …` lines carry the addresses. Pure.
fn parse_ifconfig_inet(out: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        if !line.starts_with([' ', '\t']) {
            current = line.split(':').next().map(str::to_string);
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("inet ") {
            if let (Some(iface), Some(addr)) = (&current, rest.split_whitespace().next()) {
                pairs.push((iface.clone(), addr.to_string()));
            }
        }
    }
    pairs
}

/// The first *other* interface holding an address in `host_ip`'s /24, if any.
/// Pure: `addrs` is what [`all_ipv4_addrs`] listed.
fn shared_subnet_holder<'a>(
    interface: &str,
    host_ip: &str,
    addrs: &'a [(String, String)],
) -> Option<&'a (String, String)> {
    let subnet = crate::model::netboot_subnet(host_ip)?;
    addrs.iter().find(|(iface, addr)| {
        iface != interface && crate::model::netboot_subnet(addr) == Some(subnet)
    })
}

/// The refusal behind [`guard_shared_subnet`], free of probing so the decision
/// and its message are unit-testable.
fn refuse_shared_subnet(interface: &str, host_ip: &str, addrs: &[(String, String)]) -> Result<()> {
    if let Some((other, addr)) = shared_subnet_holder(interface, host_ip, addrs) {
        let subnet = crate::model::netboot_subnet(host_ip)
            .map(crate::model::subnet_str)
            .unwrap_or_default();
        bail!(
            "refusing to put '{interface}' in {subnet}: '{other}' already holds {addr} on this \
             host. Two links in one /24 leave every route to a target ambiguous — ssh, AMT, \
             ffx and fastboot all follow whichever link the kernel lists first. Give each \
             link its own /24 (panio netboot set -t <target> --host-ip <addr>), or release \
             the other link first (panio netif mode off <its target>)."
        );
    }
    Ok(())
}

/// Never put a second interface on this host into a /24 another one already
/// holds — whether panio assigned that address (a target left in `mode
/// link`, a live netboot) or somebody did by hand. The lab-file check in
/// `model::validate_for_save` catches the configured case at edit time; this
/// is the live one, run wherever an address is about to be assigned.
pub fn guard_shared_subnet(interface: &str, host_ip: &str) -> Result<()> {
    refuse_shared_subnet(interface, host_ip, &all_ipv4_addrs())
}

/// The interface's addresses: (inet, inet6) with IPv6 scope suffixes stripped.
pub fn iface_addresses(interface: &str) -> (Vec<String>, Vec<String>) {
    let mut inet = Vec::new();
    let mut inet6 = Vec::new();
    if macos() {
        if let Ok(o) = run(&["ifconfig", interface]) {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let s = line.trim();
                if let Some(rest) = s.strip_prefix("inet ") {
                    if let Some(a) = rest.split_whitespace().next() {
                        inet.push(a.to_string());
                    }
                } else if let Some(rest) = s.strip_prefix("inet6 ") {
                    if let Some(a) = rest.split_whitespace().next() {
                        inet6.push(a.split('%').next().unwrap_or(a).to_string());
                    }
                }
            }
        }
    } else if let Ok(o) = run(&["ip", "-brief", "addr", "show", "dev", interface]) {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            for addr in text.split_whitespace().skip(2) {
                if addr.contains(':') {
                    inet6.push(addr.to_string());
                } else {
                    inet.push(addr.to_string());
                }
            }
        }
    }
    (inet, inet6)
}

fn has_host_ll(inet6: &[String]) -> bool {
    inet6
        .iter()
        .any(|a| a.split('/').next().unwrap_or(a) == FFX_HOST_LL)
}

/// Link-local IPv6 neighbours on the interface (Linux only; excludes our LL).
pub fn ipv6_peers(interface: &str) -> Vec<String> {
    if macos() {
        return Vec::new();
    }
    let Ok(o) = run(&["ip", "-6", "neigh", "show", "dev", interface]) else {
        return Vec::new();
    };
    if !o.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|t| t.starts_with("fe80:") && *t != FFX_HOST_LL)
        .map(String::from)
        .collect()
}

// ── privileged interface mutation ───────────────────────────────────────────

/// What a failed command said: stderr, or stdout when stderr is empty
/// (`networksetup` reports its `** Error:` lines on stdout).
fn failure_text(o: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
    if err.is_empty() {
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    } else {
        err
    }
}

/// The IPv4 addresses (`addr/prefix`, as printed) in `ip -4 -o addr show dev
/// <iface>` output: one address per line, the token after `inet`. Pure, so the
/// format is pinned by a test.
fn ipv4_addrs(show_output: &str) -> Vec<String> {
    show_output
        .lines()
        .filter_map(|line| {
            let mut toks = line.split_whitespace();
            toks.find(|t| *t == "inet")?;
            toks.next().map(str::to_string)
        })
        .collect()
}

/// Linux: `ip addr replace` — idempotent, and it lands the address before
/// anything else is touched.
fn ip_addr_replace(interface: &str, cidr: &str) -> Result<()> {
    let o = run(&["sudo", "ip", "addr", "replace", cidr, "dev", interface])?;
    if !o.status.success() {
        bail!(
            "ip addr replace {cidr} dev {interface} failed: {}\n{SUDO_HINT}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(())
}

/// Assign the static netboot host IP to the interface (sudo).
pub fn configure_interface(interface: &str, host_ip: &str) -> Result<()> {
    guard_shared_subnet(interface, host_ip)?;
    if macos() {
        if let Some(service) = find_network_service(interface) {
            // Tell configd the service is manual, so a carrier flap (every
            // target power-cycle) doesn't hand the link back to DHCP.
            // `-setmanual` takes four arguments — service, ip, subnet, router —
            // and rejects the call without the router; the host IP itself is
            // the customary router for a point-to-point link with no gateway.
            // `ifconfig` below is what makes the address live, so a failure
            // here is a warning, not a failure of the call.
            let manual = run(&[
                "sudo",
                "networksetup",
                "-setmanual",
                &service,
                host_ip,
                "255.255.255.0",
                host_ip,
            ]);
            match manual {
                Ok(o) if o.status.success() => {}
                Ok(o) => eprintln!(
                    "warning: networksetup -setmanual '{service}' failed: {}",
                    failure_text(&o)
                ),
                Err(e) => eprintln!("warning: networksetup -setmanual '{service}' failed: {e}"),
            }
        }
        let o = run(&[
            "sudo",
            "ifconfig",
            interface,
            host_ip,
            "netmask",
            "255.255.255.0",
            "up",
        ])?;
        if !o.status.success() {
            bail!(
                "ifconfig {interface} failed: {}\n{SUDO_HINT}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
    } else {
        // Replace first, remove the rest after: a failed assignment leaves the
        // interface as it was, never addressless (flush-then-add did).
        let cidr = format!("{host_ip}/24");
        ip_addr_replace(interface, &cidr)?;
        let others: Vec<String> = match run(&["ip", "-4", "-o", "addr", "show", "dev", interface]) {
            Ok(o) => ipv4_addrs(&String::from_utf8_lossy(&o.stdout))
                .into_iter()
                .filter(|a| *a != cidr)
                .collect(),
            Err(_) => Vec::new(),
        };
        for other in &others {
            let _ = run(&["sudo", "ip", "addr", "del", other, "dev", interface]);
        }
        if !others.is_empty() {
            // Deleting a primary address takes its secondaries with it unless
            // promote_secondaries is on; re-assert ours either way.
            ip_addr_replace(interface, &cidr)?;
        }
        let _ = run(&["sudo", "ip", "link", "set", interface, "up"]);
    }
    Ok(())
}

/// Release the static IP and return the interface to OS-managed networking.
pub fn restore_interface(interface: &str) {
    if macos() {
        if let Some(service) = find_network_service(interface) {
            let _ = run(&["sudo", "networksetup", "-setdhcp", &service]);
        }
    } else {
        let _ = run(&["sudo", "ip", "addr", "flush", "dev", interface]);
    }
}

/// macOS: stop neighbour-unreachability detection from blocking sends to the
/// Pi's bootloader (it never answers ARP probes). No-op on Linux.
pub fn tune_arp_for_silent_client() {
    if !macos() {
        return;
    }
    for kv in [
        "net.link.ether.inet.arp_llreach_base=0",
        "net.link.ether.inet.host_down_time=0",
    ] {
        let _ = run(&["sudo", "sysctl", "-w", kv]);
    }
}

fn add_host_ll(interface: &str) -> Result<()> {
    if macos() {
        let o = run(&[
            "sudo",
            "ifconfig",
            interface,
            "inet6",
            FFX_HOST_LL,
            "prefixlen",
            "64",
            "up",
        ])?;
        let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
        // Re-adding an address that is already there is the idempotent re-run.
        if !o.status.success() && !stderr.to_lowercase().contains("exists") {
            bail!(
                "ifconfig {interface} inet6 {FFX_HOST_LL} failed: {}\n{SUDO_HINT}",
                stderr.trim()
            );
        }
        return Ok(());
    }
    let sysctl_key = format!("net/ipv6/conf/{interface}/disable_ipv6=0");
    let _ = run(&["sudo", "sysctl", "-w", &sysctl_key]);
    let _ = run(&["sudo", "ip", "link", "set", interface, "up"]);
    let cidr = format!("{FFX_HOST_LL}/{FFX_PREFIX}");
    let o = run(&["sudo", "ip", "-6", "addr", "add", &cidr, "dev", interface])?;
    let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
    if !o.status.success() && !stderr.to_lowercase().contains("exists") {
        bail!(
            "ip -6 addr add {cidr} dev {interface} failed: {}\n{SUDO_HINT}",
            stderr.trim()
        );
    }
    Ok(())
}

fn del_host_ll(interface: &str) {
    if macos() {
        let _ = run(&[
            "sudo",
            "ifconfig",
            interface,
            "inet6",
            FFX_HOST_LL,
            "-alias",
        ]);
    } else {
        let cidr = format!("{FFX_HOST_LL}/{FFX_PREFIX}");
        let _ = run(&["sudo", "ip", "-6", "addr", "del", &cidr, "dev", interface]);
    }
}

fn del_host_ip(interface: &str, host_ip: &str) {
    if macos() {
        // Release the static IP by returning the service to DHCP — an
        // `ifconfig` delete won't unset a `networksetup -setmanual` IP, so
        // `mode off` from `link` mode (no netboot stop to restore it) would
        // otherwise leave the host IP assigned.
        if let Some(service) = find_network_service(interface) {
            let _ = run(&["sudo", "networksetup", "-setdhcp", &service]);
        }
        // `-setdhcp` reconfigures the service, but the address `ifconfig` put
        // on the interface can outlive it as an alias; release it explicitly so
        // a stale host IP cannot survive `mode off`.
        let _ = run(&["sudo", "ifconfig", interface, "inet", host_ip, "-alias"]);
        return;
    }
    let cidr = format!("{host_ip}/24");
    let _ = run(&["sudo", "ip", "addr", "del", &cidr, "dev", interface]);
}

/// Best-effort: disable Wake-on-LAN so the NIC doesn't keep the PHY (and the
/// peer's carrier) alive after the interface is taken down. Per-NIC via ethtool
/// on Linux; macOS WoL is a system-wide pref (`pmset womp`), so there we rely on
/// admin-down alone (USB-Ethernet adapters drop link on `ifconfig down`).
fn disable_wol(interface: &str) {
    if macos() {
        return;
    }
    let _ = run(&["sudo", "ethtool", "-s", interface, "wol", "d"]);
}

/// Administratively bring the interface down — drops carrier on the peer.
fn admin_down(interface: &str) {
    if macos() {
        let _ = run(&["sudo", "ifconfig", interface, "down"]);
    } else {
        let _ = run(&["sudo", "ip", "link", "set", interface, "down"]);
    }
}

// ── modes ───────────────────────────────────────────────────────────────────

pub const MODES: [&str; 4] = ["netboot", "link", "ffx", "off"];

/// The netboot channel's link hooks: shell commands that drive something
/// outside the host (typically a managed-switch port) so the carrier really
/// drops (`down_cmd`, run by `down-hard`) and comes back (`up_cmd`, run before
/// `mode link|netboot|ffx`). Empty strings count as unset.
#[derive(Debug, Default, Clone)]
pub struct LinkHooks {
    pub down_cmd: Option<String>,
    pub up_cmd: Option<String>,
}

impl LinkHooks {
    fn set(cmd: &Option<String>) -> Option<&str> {
        cmd.as_deref().filter(|c| !c.trim().is_empty())
    }

    /// `none`, `down`, `up` or `down+up`, for `netif status`.
    pub fn summary(&self) -> &'static str {
        match (Self::set(&self.down_cmd), Self::set(&self.up_cmd)) {
            (None, None) => "none",
            (Some(_), None) => "down",
            (None, Some(_)) => "up",
            (Some(_), Some(_)) => "down+up",
        }
    }
}

/// Run one optional link hook on this host the way a power hook runs: under
/// the target's hook lock, `sh -c`, helper dir first on PATH. `which` names
/// the hook (`link_down_cmd`) and `note` is appended to a failure message so
/// the caller can say what had already happened. No hook is a no-op. A failure
/// is `hook_failed` (exit-status contract).
fn run_link_hook(target: &str, which: &str, cmd: Option<&str>, note: &str) -> Result<()> {
    let Some(cmd) = cmd.filter(|c| !c.trim().is_empty()) else {
        return Ok(());
    };
    let lock = crate::actions::hook_lock(target, crate::actions::LOCK_WAIT)?;
    eprintln!("Running {which} for '{target}' via {cmd}");
    let envs = crate::daemons::hook_helper_name(cmd)
        .map(|n| crate::daemons::helper_env(&n, None))
        .unwrap_or_default();
    let (k, v) = lock.env();
    let status = crate::platform::shell_command(cmd)
        .env("PATH", crate::daemons::hook_path())
        .envs(envs)
        .env(k, v)
        .status()?;
    if status.success() {
        return Ok(());
    }
    Err(crate::error::hook_failed(
        status,
        format!(
            "{which} exited with code {}{note}",
            crate::error::shell_code(status)
        ),
    )
    .target(target)
    .channel("netboot")
    .into())
}

/// netboot mode: tear down ffx, start DHCP+TFTP (idempotent).
pub fn mode_netboot(
    target: &str,
    interface: &str,
    host_ip: &str,
    tftp_root: &str,
    opts: &crate::netboot::BootOptions,
    hooks: &LinkHooks,
) -> Result<()> {
    guard_primary_interface("mode netboot", interface)?;
    run_link_hook(target, "link_up_cmd", hooks.up_cmd.as_deref(), "")?;
    del_host_ll(interface);
    if crate::state::is_netboot_running(target) {
        return Ok(());
    }
    crate::netboot::start(target, interface, host_ip, tftp_root, opts)
}

/// link mode: assign just the IPv4 host IP to the interface — no DHCP/TFTP
/// daemon and no ffx link-local. This is the bare host side of the link, for
/// testing it up/down (`mode link` brings the host IP up, `mode off` releases
/// it). Idempotent. Note `mode off` only releases the IP; it does not force the
/// physical carrier down (the NIC can hold the link up for Wake-on-LAN — see
/// docs/netif.md).
pub fn mode_link(target: &str, interface: &str, host_ip: &str, hooks: &LinkHooks) -> Result<()> {
    guard_primary_interface("mode link", interface)?;
    run_link_hook(target, "link_up_cmd", hooks.up_cmd.as_deref(), "")?;
    if crate::state::is_netboot_running(target) {
        crate::netboot::stop(target)?;
    }
    del_host_ll(interface);
    configure_interface(interface, host_ip)
}

/// ffx mode: stop netboot first, then add the host IPv6 link-local. Like the
/// other link-up modes it runs `link_up_cmd` first, since ffx needs the link
/// as much as they do (and may follow a `down-hard`).
pub fn mode_ffx(target: &str, interface: &str, hooks: &LinkHooks) -> Result<()> {
    guard_primary_interface("mode ffx", interface)?;
    run_link_hook(target, "link_up_cmd", hooks.up_cmd.as_deref(), "")?;
    if crate::state::is_netboot_running(target) {
        crate::netboot::stop(target)?;
    }
    add_host_ll(interface)
}

/// off: stop netboot, remove the ffx LL and any stale IPv4.
pub fn mode_off(target: &str, interface: &str, host_ip: &str) -> Result<()> {
    guard_primary_interface("mode off", interface)?;
    if crate::state::is_netboot_running(target) {
        crate::netboot::stop(target)?;
    }
    del_host_ll(interface);
    del_host_ip(interface, host_ip);
    Ok(())
}

/// Force the link down *hard*: do everything `mode off` does (stop netboot,
/// release the host IP + ffx LL), then disable Wake-on-LAN and admin-down the
/// interface so the peer sees carrier loss. `mode off` alone only releases the
/// host IP and can leave the carrier up (a WoL-capable NIC keeps the PHY
/// energized) — use this when you need the target to actually *detect* link
/// loss. Bring the link back with `mode link`/`mode netboot`; WoL stays disabled
/// until re-enabled (`ethtool -s <iface> wol g`) or the adapter is replugged.
///
/// When the channel has a `link_down_cmd` it runs last, after the host-side
/// steps: that is the guaranteed drop (e.g. a managed-switch port disable). If
/// it fails the host-side steps stay done and the error says so.
pub fn down_hard(target: &str, interface: &str, host_ip: &str, hooks: &LinkHooks) -> Result<()> {
    guard_primary_interface("down-hard", interface)?;
    mode_off(target, interface, host_ip)?;
    disable_wol(interface);
    admin_down(interface);
    run_link_hook(
        target,
        "link_down_cmd",
        hooks.down_cmd.as_deref(),
        " (the host-side steps already ran: netboot stopped, addresses released, \
         Wake-on-LAN disabled, interface admin-down; the carrier may still be up)",
    )?;
    Ok(())
}

pub struct NetifStatus {
    pub mode: &'static str,
    pub carrier: bool,
    pub inet: Vec<String>,
    pub inet6: Vec<String>,
    pub peers: Vec<String>,
}

/// Probe the active mode (never stored): daemons running → netboot; host LL
/// present → ffx; the static host IP present (but no daemon/LL) → link; else
/// off. `carrier` is the physical link state (independent of the mode — it can
/// read up in `off` if the NIC keeps the PHY alive for Wake-on-LAN).
pub fn get_status(target: &str, interface: &str, host_ip: &str) -> NetifStatus {
    let netboot_running = crate::state::is_netboot_running(target);
    let (inet, inet6) = iface_addresses(interface);
    let has_ll = has_host_ll(&inet6);
    let has_host_ip = inet
        .iter()
        .any(|a| a.split('/').next().unwrap_or(a) == host_ip);
    let mode = if netboot_running {
        "netboot"
    } else if has_ll {
        "ffx"
    } else if has_host_ip {
        "link"
    } else {
        "off"
    };
    let peers = if mode == "ffx" {
        ipv6_peers(interface)
    } else {
        Vec::new()
    };
    NetifStatus {
        mode,
        carrier: is_interface_active(interface),
        inet,
        inet6,
        peers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `cmd` through `run_link_hook` under a private runtime base.
    fn run_hook_isolated(cmd: Option<&str>) -> Result<()> {
        let _guard = crate::daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PANIO_RUNTIME_BASE");
        // Safe: serialized by RUNTIME_BASE_LOCK; restored below.
        unsafe { std::env::set_var("PANIO_RUNTIME_BASE", base.path()) };
        let r = run_link_hook("link-hook-test", "link_down_cmd", cmd, " [note]");
        match prev {
            Some(p) => unsafe { std::env::set_var("PANIO_RUNTIME_BASE", p) },
            None => unsafe { std::env::remove_var("PANIO_RUNTIME_BASE") },
        }
        r
    }

    #[cfg(unix)]
    #[test]
    fn a_link_hook_actually_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let cmd = format!("touch '{}'", marker.display());
        run_hook_isolated(Some(&cmd)).unwrap();
        assert!(marker.exists(), "the hook command must have executed");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_link_hook_is_hook_failed_and_carries_the_note() {
        let err = run_hook_isolated(Some("exit 7")).unwrap_err();
        let pe = err.downcast_ref::<crate::error::PanioError>().unwrap();
        assert_eq!(pe.kind, crate::error::Kind::HelperFailed);
        assert_eq!(pe.child_exit, Some(7));
        let msg = err.to_string();
        assert!(msg.contains("link_down_cmd exited with code 7"), "{msg}");
        assert!(msg.contains("[note]"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_link_hook_program_is_not_configured() {
        let err = run_hook_isolated(Some("panio-no-such-link-helper-xyz")).unwrap_err();
        let pe = err.downcast_ref::<crate::error::PanioError>().unwrap();
        assert_eq!(pe.kind, crate::error::Kind::NotConfigured);
    }

    #[test]
    fn unset_or_blank_link_hooks_do_nothing() {
        run_hook_isolated(None).unwrap();
        run_hook_isolated(Some("  ")).unwrap();
    }

    #[test]
    fn link_hook_summary_names_what_is_configured() {
        let mut h = LinkHooks::default();
        assert_eq!(h.summary(), "none");
        h.down_cmd = Some("x".into());
        assert_eq!(h.summary(), "down");
        h.up_cmd = Some("y".into());
        assert_eq!(h.summary(), "down+up");
        h.down_cmd = Some(String::new());
        assert_eq!(h.summary(), "up");
    }

    #[test]
    fn the_default_route_interface_is_primary() {
        assert!(is_default_route_interface("en0", Some("en0")));
        assert!(!is_default_route_interface("en14", Some("en0")));
        // No default route at all (an offline control host): nothing is primary.
        assert!(!is_default_route_interface("en0", None));
    }

    #[test]
    fn every_mode_change_refuses_the_primary_interface() {
        for what in [
            "mode netboot",
            "mode link",
            "mode ffx",
            "mode off",
            "down-hard",
        ] {
            let err = refuse_primary_interface(what, "en0", Some("en0"))
                .expect_err("the default-route interface must be refused");
            let msg = err.to_string();
            assert!(
                msg.starts_with(&format!("refusing netif {what} on 'en0'")),
                "{msg}"
            );
            assert!(msg.contains("system default route"), "{msg}");
        }
    }

    #[test]
    fn a_dedicated_adapter_passes_the_guard() {
        assert!(refuse_primary_interface("mode link", "en14", Some("en0")).is_ok());
        assert!(refuse_primary_interface("down-hard", "en14", None).is_ok());
    }

    #[test]
    fn all_interface_listings_parse_on_both_platforms() {
        let linux = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.0.2.10/24 brd 192.0.2.255 scope global dynamic eth0\\       valid_lft 3242sec preferred_lft 3242sec
5: eth3    inet 198.51.100.1/24 brd 198.51.100.255 scope global eth3\\       valid_lft forever preferred_lft forever
6: eth4    inet 198.51.100.1/24 brd 198.51.100.255 scope global eth4\\       valid_lft forever preferred_lft forever
";
        assert_eq!(
            parse_ip_addr_show(linux),
            vec![
                ("lo".to_string(), "127.0.0.1".to_string()),
                ("eth0".to_string(), "192.0.2.10".to_string()),
                ("eth3".to_string(), "198.51.100.1".to_string()),
                ("eth4".to_string(), "198.51.100.1".to_string()),
            ]
        );
        let mac = "\
lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
\tinet 127.0.0.1 netmask 0xff000000
\tinet6 ::1 prefixlen 128
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether 00:11:22:33:44:55
\tinet 192.0.2.10 netmask 0xffffff00 broadcast 192.0.2.255
en14: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 198.51.100.1 netmask 0xffffff00 broadcast 198.51.100.255
en16: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tstatus: active
";
        assert_eq!(
            parse_ifconfig_inet(mac),
            vec![
                ("lo0".to_string(), "127.0.0.1".to_string()),
                ("en0".to_string(), "192.0.2.10".to_string()),
                ("en14".to_string(), "198.51.100.1".to_string()),
            ]
        );
        assert!(parse_ip_addr_show("").is_empty());
        assert!(parse_ifconfig_inet("").is_empty());
    }

    #[test]
    fn a_second_interface_in_the_same_slash_24_is_refused() {
        let addrs = vec![
            ("eth0".to_string(), "192.0.2.10".to_string()),
            ("eth3".to_string(), "198.51.100.1".to_string()),
        ];
        // eth4 wants eth3's /24: refused, naming eth3 and its address.
        let e = refuse_shared_subnet("eth4", "198.51.100.1", &addrs).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("refusing to put 'eth4' in 198.51.100.0/24"),
            "{msg}"
        );
        assert!(msg.contains("'eth3' already holds 198.51.100.1"), "{msg}");
        assert!(msg.contains("--host-ip"), "{msg}");
        // A different host address in the same /24 is still the same link.
        assert!(refuse_shared_subnet("eth4", "198.51.100.7", &addrs).is_err());
        // Re-applying to the interface that already holds it is fine (that is
        // every `mode link` re-run and every netbootd IP restore).
        refuse_shared_subnet("eth3", "198.51.100.1", &addrs).unwrap();
        // A /24 nobody holds is fine, and so is an empty listing (no tool).
        refuse_shared_subnet("eth4", "198.51.101.1", &addrs).unwrap();
        refuse_shared_subnet("eth4", "198.51.100.1", &[]).unwrap();
        assert_eq!(
            shared_subnet_holder("eth4", "198.51.100.1", &addrs),
            Some(&("eth3".to_string(), "198.51.100.1".to_string()))
        );
    }

    #[test]
    fn ipv4_addrs_parses_the_one_line_ip_listing() {
        // Captured shape of `ip -4 -o addr show dev eth1`: `-o` joins the
        // lifetime line onto the address line with a backslash, and a second
        // (secondary) address is a second line.
        let sample = "\
3: eth1    inet 192.0.2.10/24 brd 192.0.2.255 scope global eth1\\       valid_lft forever preferred_lft forever
3: eth1    inet 198.51.100.7/16 brd 198.51.255.255 scope global secondary dynamic eth1\\       valid_lft 3242sec preferred_lft 3242sec
";
        assert_eq!(ipv4_addrs(sample), vec!["192.0.2.10/24", "198.51.100.7/16"]);
        assert!(ipv4_addrs("").is_empty());
        // Only `inet` entries count.
        assert!(ipv4_addrs("3: eth1    <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500\n").is_empty());
    }
}
