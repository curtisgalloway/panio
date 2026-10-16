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

//! `paniolo doctor` — probe configured channels against reality.
//!
//! Read-only health check: for each channel it SSHes to the control host the
//! channel lives on (or runs locally) and tests that the configured device or
//! interface actually exists. This is the probing that used to be baked into the
//! Python `setup` commands, separated so config edits stay local and offline.

use std::process::Command;

use crate::daemons;
use crate::model::{ChannelKind, Lab, ResolvedChannel, ResolvedTarget};
use crate::ssh;

#[derive(PartialEq, Eq)]
enum Status {
    Ok,
    Missing,
    Unreachable,
    Incomplete,
    /// Present, but configured against another channel: two netboot links on
    /// one host sharing a /24 (`model::netboot_subnet_clash`).
    Conflict,
    /// Present, but not running as configured: a netboot interface holding
    /// an IPv4 address that is not the channel's effective host IP
    /// ([`netboot_ip_drift`]).
    Mismatch,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Missing => "MISSING",
            Status::Unreachable => "unreachable",
            Status::Incomplete => "incomplete",
            Status::Conflict => "CONFLICT",
            Status::Mismatch => "MISMATCH",
        }
    }
    fn is_problem(&self) -> bool {
        matches!(
            self,
            Status::Missing | Status::Unreachable | Status::Conflict | Status::Mismatch
        )
    }
}

/// One health check, expressed structurally rather than as shell text.
///
/// Locally a probe runs natively, so `paniolo doctor` needs no shell at all.
/// It used to build a POSIX script and run it through `sh -c`, which does not
/// exist on Windows — and nothing caught that, because the tests asserted on
/// the *text* of the generated script instead of running it. Anything that
/// only inspects a command string is blind to whether the command can run.
///
/// Remotely a probe still renders to POSIX sh via [`Probe::to_posix`], because
/// the far side of the SSH hop is a Unix control host. Keeping both views of
/// each probe in one type is what stops them drifting apart.
enum Probe {
    /// The path exists.
    Exists(String),
    /// The program resolves the way a hook would: libexec dirs, then PATH.
    OnHookPath(String),
    /// A capture device: either a path that exists, or a name `hdmicap
    /// devices` lists. Exit 3 means hdmicap itself is missing — a different
    /// failure from a missing device.
    Video(String),
    /// The network interface is present on this host.
    NetInterface(String),
    /// The interface holds no IPv4 address, or holds `host_ip`. Exit 1 — and
    /// the offending address on stdout — when it holds only other addresses:
    /// a link that drifted from its configured host IP
    /// ([`netboot_ip_drift`]).
    NetInterfaceHolds { iface: String, host_ip: String },
    /// adb reaches the device: the binary resolves (else exit 3), then
    /// `get-state` succeeds.
    Adb { bin: String, serial: Option<String> },
}

impl Probe {
    /// Render as a POSIX-sh script, for probing a remote Unix control host.
    fn to_posix(&self) -> String {
        match self {
            Probe::Exists(p) => format!("test -e {}", ssh::shell_quote(p)),
            Probe::OnHookPath(prog) => format!(
                "{HOOK_PATH_PREFIX} command -v {} >/dev/null",
                ssh::shell_quote(prog)
            ),
            Probe::Video(dev) => video_probe_script(dev),
            Probe::NetInterface(iface) => {
                let q = ssh::shell_quote(iface);
                format!("test -e /sys/class/net/{q} || ifconfig {q} >/dev/null 2>&1")
            }
            Probe::NetInterfaceHolds { iface, host_ip } => {
                net_interface_holds_script(iface, host_ip)
            }
            Probe::Adb { bin, serial } => {
                let q_bin = ssh::shell_quote(bin);
                let find = if bin.starts_with('/') {
                    format!("test -x {q_bin}")
                } else {
                    format!("command -v {q_bin} >/dev/null")
                };
                let sel = match serial {
                    Some(s) => format!("-s {} ", ssh::shell_quote(s)),
                    None => String::new(),
                };
                format!("{find} || exit 3; {q_bin} {sel}get-state >/dev/null 2>&1")
            }
        }
    }

    /// Run the probe on this host, natively. Exit codes match [`to_posix`], so
    /// a local and a remote answer mean the same thing.
    fn run_local(&self) -> Option<i32> {
        let ok = |b: bool| Some(if b { 0 } else { 1 });
        match self {
            Probe::Exists(p) => ok(std::path::Path::new(p).exists()),
            Probe::OnHookPath(prog) => ok(crate::daemons::find_binary(prog).is_some()),
            Probe::Video(dev) => {
                if std::path::Path::new(dev).exists() {
                    return Some(0);
                }
                let Some(hdmicap) = crate::daemons::find_binary("hdmicap") else {
                    return Some(3);
                };
                let out = Command::new(hdmicap).arg("devices").output().ok()?;
                ok(String::from_utf8_lossy(&out.stdout).contains(dev.as_str()))
            }
            Probe::NetInterface(iface) => local_interface_exists(iface),
            Probe::NetInterfaceHolds { iface, host_ip } => {
                ok(local_netboot_ip_drift(iface, host_ip).is_none())
            }
            Probe::Adb { bin, serial } => {
                let found = if bin.starts_with('/') {
                    is_executable(std::path::Path::new(bin)).then(|| bin.into())
                } else {
                    crate::daemons::find_binary(bin)
                };
                let Some(adb) = found else {
                    return Some(3);
                };
                let mut cmd = Command::new(adb);
                if let Some(s) = serial {
                    cmd.arg("-s").arg(s);
                }
                let status = cmd
                    .arg("get-state")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .ok()?;
                ok(status.success())
            }
        }
    }
}

/// Is this a file we could execute? On Unix that is an execute bit; Windows
/// has no such bit, so existing and being a file is the whole test.
fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Is `iface` present on this host?
///
/// Linux exposes every interface under sysfs. macOS has no such tree, so ask
/// `ifconfig`. Windows has neither, and `netif` has no Windows implementation
/// to configure one with — so the honest answer there is "cannot tell", which
/// [`interpret`] renders as unreachable rather than a confident "missing".
fn local_interface_exists(iface: &str) -> Option<i32> {
    if cfg!(target_os = "linux") {
        return Some(i32::from(
            !std::path::Path::new(&format!("/sys/class/net/{iface}")).exists(),
        ));
    }
    if cfg!(target_os = "macos") {
        return Command::new("ifconfig")
            .arg(iface)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|s| s.code().unwrap_or(-1));
    }
    None
}

/// The address a netboot interface holds that is not its effective host IP,
/// if it holds any address at all. `held` is what `netif::iface_addresses`
/// listed: bare on macOS, `A.B.C.D/NN` on Linux, so a prefix is stripped
/// before comparing. No IPv4 address at all (`netif mode off`) is not drift;
/// neither is holding `host_ip` alongside others, since that is the address
/// netbootd serves at. Pure, so the decision is unit-testable.
fn netboot_ip_drift(held: &[String], host_ip: &str) -> Option<String> {
    let bare: Vec<&str> = held
        .iter()
        .map(|a| a.split('/').next().unwrap_or(a))
        .collect();
    if bare.contains(&host_ip) {
        return None;
    }
    bare.first().map(|a| a.to_string())
}

/// [`netboot_ip_drift`] against this host's live addresses.
fn local_netboot_ip_drift(iface: &str, host_ip: &str) -> Option<String> {
    netboot_ip_drift(&crate::netif::iface_addresses(iface).0, host_ip)
}

/// POSIX rendering of [`Probe::NetInterfaceHolds`]: list the interface's
/// IPv4 addresses (`ip -4 -o addr show` on Linux, `ifconfig` on macOS —
/// both put the address right after an `inet` token, Linux with a `/NN`
/// prefix), exit 0 when there are none or `host_ip` is among them, else
/// print the first other address and exit 1.
fn net_interface_holds_script(iface: &str, host_ip: &str) -> String {
    let q_iface = ssh::shell_quote(iface);
    let q_ip = ssh::shell_quote(host_ip);
    format!(
        // `ip` lives in /usr/sbin and `ifconfig` in /sbin, and a
        // non-interactive SSH shell often has neither on PATH. Without this
        // both legs exit 127, the address list comes back empty, and a
        // drifted link reads as fine — the one direction this check must not
        // fail in. Same reason `HOOK_PATH_PREFIX` exists.
        "PATH=\"$PATH:/usr/sbin:/sbin\"; \
         out=$({{ ip -4 -o addr show dev {q_iface} || ifconfig {q_iface}; }} 2>/dev/null); \
         test -n \"$out\" || exit 3; \
         addrs=$(printf '%s\\n' \"$out\" | \
         awk '{{ for (i = 1; i < NF; i++) if ($i == \"inet\") {{ \
         sub(/^addr:/, \"\", $(i + 1)); split($(i + 1), a, \"/\"); print a[1] }} }}'); \
         test -z \"$addrs\" && exit 0; \
         for a in $addrs; do test \"$a\" = {q_ip} && exit 0; done; \
         set -- $addrs; echo \"$1\"; exit 1"
    )
}

/// What a netboot interface holds instead of its host IP: `None` when the
/// host is unreachable, `Some(None)` when the link is fine, `Some(Some(addr))`
/// when it drifted. Local hosts answer through [`local_netboot_ip_drift`];
/// remote ones run [`Probe::NetInterfaceHolds`], whose stdout carries the
/// address (the exit status alone would only say "some other address").
fn probe_netboot_ip_drift(
    lab: &Lab,
    host_name: &str,
    iface: &str,
    host_ip: &str,
) -> Option<Option<String>> {
    let host = lab.host(host_name);
    if host.is_local(host_name) {
        return Some(local_netboot_ip_drift(iface, host_ip));
    }
    let p = Probe::NetInterfaceHolds {
        iface: iface.to_string(),
        host_ip: host_ip.to_string(),
    };
    let out = ssh::run(
        &host,
        &["sh".to_string(), "-c".to_string(), p.to_posix()],
        None,
        &[],
    )
    .ok()?;
    drift_from_probe(out.status, &out.stdout)
}

/// [`probe_netboot_ip_drift`]'s verdict from what the remote probe returned.
///
/// Pure, so every arm is testable without a second machine: the remote half of
/// this check had no test of its own, and each arm is a claim about hardware
/// state that a reader will act on.
///
/// 255 is ssh's own failure and 3 is the script's "neither `ip` nor `ifconfig`
/// told me anything". Both mean the question was not answered, which must not
/// be rendered as a hardware-state claim. Nor must a non-zero exit with
/// nothing on stdout — that is a probe that died (a signal, a non-POSIX login
/// shell), not a link that moved, and reporting `MISMATCH` there would be a
/// confident claim about an address never read.
fn drift_from_probe(status: i32, stdout: &str) -> Option<Option<String>> {
    match status {
        0 => Some(None),
        255 | 3 => None,
        _ => {
            let addr = stdout.lines().next().unwrap_or("").trim();
            if addr.is_empty() {
                None
            } else {
                Some(Some(addr.to_string()))
            }
        }
    }
}

/// The `MISMATCH` detail for [`netboot_ip_drift`]: what the interface holds,
/// what the lab file says (marked when that is the default), and the remedy.
///
/// The remedy is `netif mode link` (or `netboot start`) on its own.
/// `configure_interface` assigns the configured address and then removes every
/// other IPv4 on the interface, so it repairs drift by itself. Telling the
/// operator to run `mode off` first would be worse than redundant: `mode off`
/// deletes the *configured* address — the one that is not assigned when a link
/// has drifted — so it cannot release the stale one, and on the way it stops a
/// running netbootd and, on macOS, hands the service back to DHCP.
fn netboot_drift_detail(
    target: &str,
    iface: &str,
    held: &str,
    host_ip: &str,
    defaulted: bool,
) -> String {
    format!(
        "{iface} holds {held} but host_ip is {host_ip}{}; run `paniolo netif mode link \
         {target}` (or `netboot start {target}`) to re-apply it",
        if defaulted { " (default)" } else { "" }
    )
}

/// Run a probe on a host: natively when it is this machine, over SSH as a
/// POSIX script otherwise. None == unreachable.
fn probe(lab: &Lab, host_name: &str, p: &Probe) -> Option<i32> {
    let host = lab.host(host_name);
    if host.is_local(host_name) {
        return p.run_local();
    }
    match ssh::run(
        &host,
        &["sh".to_string(), "-c".to_string(), p.to_posix()],
        None,
        &[],
    ) {
        Ok(o) => Some(o.status),
        Err(_) => None,
    }
}

fn interpret(rc: Option<i32>, what: &str) -> (Status, String) {
    match rc {
        None | Some(255) => (Status::Unreachable, "host unreachable".to_string()),
        Some(0) => (Status::Ok, what.to_string()),
        Some(_) => (Status::Missing, what.to_string()),
    }
}

fn field<'a>(ch: &'a ResolvedChannel, key: &str) -> Option<&'a str> {
    ch.fields
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

/// Shell statement giving hook commands the same resolution the hooks get at
/// runtime (`daemons::hook_path()`): the per-user libexec dir, then the system
/// package dir, then PATH. The literal paths must match
/// `daemons::libexec_dir()` / `daemons::system_libexec_dir()`; `$HOME` is
/// expanded by the probed host's own shell so that host's home applies. A
/// standalone assignment (not a `VAR=… cmd` prefix) so the `command -v`
/// lookup that follows honors it in every /bin/sh, dash included. Exe-relative
/// helper dirs (a Homebrew keg) can't be derived for a remote host, so a
/// keg-only install on a *remote* control host may still probe MISSING.
const HOOK_PATH_PREFIX: &str =
    "PATH=\"$HOME/.local/libexec/paniolo/bin:/usr/libexec/paniolo/bin:$PATH\";";

/// Probe script for a video channel. The device is usually a capture-device
/// NAME (e.g. "USB Video" on macOS), not a path, so `test -e` alone is wrong:
/// ask `hdmicap devices` on the channel host whether it enumerates. Path-style
/// devices (`/dev/video0`) still short-circuit via `test -e`. hdmicap resolves
/// like `daemons::find_binary()`: the helper dirs (via [`HOOK_PATH_PREFIX`]),
/// then PATH, then the legacy ~/.cargo/bin. Exit 3 = hdmicap itself is
/// missing, a distinct failure from a missing device.
fn video_probe_script(device: &str) -> String {
    let q = ssh::shell_quote(device);
    format!(
        "test -e {q} && exit 0; \
         {HOOK_PATH_PREFIX} PATH=\"$PATH:$HOME/.cargo/bin\"; \
         command -v hdmicap >/dev/null || exit 3; \
         hdmicap devices 2>/dev/null | grep -F -q -- {q}"
    )
}

/// How a hook program (a power hook, or the hid `cmd`) is probed: an absolute
/// path just has to exist; a bare name has to resolve the way the hook itself
/// will resolve it — libexec dirs, then PATH.
fn hook_probe(prog: &str) -> Probe {
    if prog.starts_with('/') {
        Probe::Exists(prog.to_string())
    } else {
        Probe::OnHookPath(prog.to_string())
    }
}

fn check_channel(lab: &Lab, ch: &ResolvedChannel, rt: &ResolvedTarget) -> (Status, String) {
    match ch.kind {
        ChannelKind::Serial => match field(ch, "device") {
            None => (Status::Incomplete, "no device set".to_string()),
            Some(dev) => interpret(probe(lab, &ch.host, &Probe::Exists(dev.to_string())), dev),
        },
        ChannelKind::Video => match field(ch, "device") {
            None => (Status::Incomplete, "no device set".to_string()),
            // A network source (RFB) is not an enumerable capture device:
            // `hdmicap devices` would never list it, so only hdmicap itself
            // can be missing.
            Some(dev) if crate::model::is_network_device(dev) => {
                match probe(lab, &ch.host, &Probe::OnHookPath("hdmicap".to_string())) {
                    Some(0) => (Status::Ok, format!("{dev} (network source)")),
                    None | Some(255) => (Status::Unreachable, "host unreachable".to_string()),
                    Some(_) => (Status::Missing, format!("{dev} (hdmicap not installed)")),
                }
            }
            Some(dev) => match probe(lab, &ch.host, &Probe::Video(dev.to_string())) {
                Some(3) => (Status::Missing, format!("{dev} (hdmicap not installed)")),
                rc => interpret(rc, dev),
            },
        },
        ChannelKind::Netboot if field(ch, "tftp_root").is_none_or(str::is_empty) => (
            Status::Missing,
            format!(
                "no tftp_root configured (paniolo netboot set -t {} --tftp-root <path>)",
                rt.name
            ),
        ),
        ChannelKind::Netboot => match field(ch, "interface") {
            None => (Status::Incomplete, "no interface set".to_string()),
            Some(iface) => {
                let (status, detail) = interpret(
                    probe(lab, &ch.host, &Probe::NetInterface(iface.to_string())),
                    iface,
                );
                if status.is_problem() {
                    return (status, detail);
                }
                // A lab that already has two links in one /24 still loads
                // (validate_for_save refuses to *create* one); this is where
                // the existing clash gets reported.
                if let Some(c) = crate::model::netboot_subnet_clash(lab, &rt.name) {
                    return (
                        Status::Conflict,
                        format!(
                            "{iface} at {}{} shares {} with '{}' on {}; give one link its \
                             own /24 (paniolo netboot set --host-ip)",
                            c.host_ip,
                            if c.defaulted { " (default)" } else { "" },
                            c.subnet,
                            c.other_target,
                            c.other_interface
                        ),
                    );
                }
                // The lab file's host IP is only what the link *should* run
                // at. A stale `mode link`, a hand-assigned address, or an
                // edit made while the daemon was down leaves the interface
                // holding something else, and only a running netbootd
                // re-applies it — so ask the interface. The effective IP
                // comes from the channel, not from the rendered field, which
                // reads `192.168.99.1 (default)` when unset.
                let Some(nb) = lab.targets.get(&rt.name).and_then(|t| t.netboot.as_ref()) else {
                    return (status, detail);
                };
                let host_ip = nb.effective_host_ip();
                match probe_netboot_ip_drift(lab, &ch.host, iface, host_ip) {
                    None => (Status::Unreachable, "host unreachable".to_string()),
                    Some(Some(held)) => (
                        Status::Mismatch,
                        netboot_drift_detail(&rt.name, iface, &held, host_ip, nb.host_ip.is_none()),
                    ),
                    Some(None) => (status, detail),
                }
            }
        },
        ChannelKind::Actions => {
            let Some(acts) = lab.targets.get(&rt.name).and_then(|t| t.actions.as_ref()) else {
                return (Status::Incomplete, "no actions".to_string());
            };
            if acts.actions.is_empty() {
                return (Status::Incomplete, "no actions defined".to_string());
            }
            // Like the power hooks: the program after any `VAR=value`
            // prefix, resolved the way the hook will resolve it.
            for (name, a) in &acts.actions {
                let prog = daemons::first_program_token(&a.cmd).unwrap_or("");
                let rc = probe(lab, &ch.host, &hook_probe(prog));
                if rc != Some(0) {
                    let (status, detail) = interpret(rc, prog);
                    return (status, format!("action '{name}': {detail}"));
                }
            }
            let names: Vec<&str> = acts.actions.keys().map(String::as_str).collect();
            (Status::Ok, names.join(", "))
        }
        ChannelKind::Power => {
            if let Some(si) = field(ch, "serial_interface") {
                let have = rt
                    .channels
                    .iter()
                    .any(|c| c.kind == ChannelKind::Serial && c.name == si);
                if !have {
                    return (
                        Status::Missing,
                        format!("serial_interface '{si}' has no matching serial"),
                    );
                }
            }
            // Probe all four hook fields; report the first missing program.
            // Bare names resolve like the hooks themselves do: libexec first,
            // then PATH (see daemons::hook_path()). A hook may be prefixed
            // with `VAR=value` credential assignments (docs/power.md
            // "Credentials"); the program to probe is the token after them.
            let hook_keys = ["cycle_cmd", "on_cmd", "off_cmd", "state_cmd"];
            let mut configured: Vec<&str> = Vec::new();
            for key in hook_keys {
                if let Some(cmd) = field(ch, key) {
                    configured.push(key);
                    let prog = daemons::first_program_token(cmd).unwrap_or("");
                    let rc = probe(lab, &ch.host, &hook_probe(prog));
                    if rc != Some(0) {
                        return interpret(rc, prog);
                    }
                }
            }
            if configured.is_empty() {
                (Status::Ok, "configured".to_string())
            } else {
                (Status::Ok, configured.join(","))
            }
        }
        ChannelKind::Hid | ChannelKind::Usb => match field(ch, "cmd") {
            None => (Status::Incomplete, "no cmd set".to_string()),
            // Like the power hooks: absolute-path helpers are probed for
            // existence; bare names are probed under libexec-then-PATH; a
            // leading `VAR=value` is skipped the same way.
            Some(cmd) => {
                let prog = daemons::first_program_token(cmd).unwrap_or("");
                interpret(probe(lab, &ch.host, &hook_probe(prog)), prog)
            }
        },
        ChannelKind::Adb => {
            // adb is a system tool (PATH), not a paniolo libexec helper. Exit 3
            // distinguishes "adb not installed" from "device not reachable".
            let adb_bin = field(ch, "adb").unwrap_or("adb");
            let serial = field(ch, "serial");
            let p = Probe::Adb {
                bin: adb_bin.to_string(),
                serial: serial.map(str::to_string),
            };
            match probe(lab, &ch.host, &p) {
                Some(3) => (Status::Missing, format!("{adb_bin} (adb not installed)")),
                rc => interpret(rc, serial.unwrap_or("device")),
            }
        }
    }
}

fn channel_name(ch: &ResolvedChannel) -> String {
    if ch.name == ch.kind.as_str() {
        ch.kind.as_str().to_string()
    } else {
        format!("{} {}", ch.kind.as_str(), ch.name)
    }
}

/// Probe `target` (or all targets), optionally limited to `host_filter`.
/// Prints a report and returns the number of problems found.
pub fn run(lab: &Lab, target: Option<&str>, host_filter: Option<&str>) -> i32 {
    let names: Vec<String> = match target {
        Some(n) => vec![n.to_string()],
        None => lab.targets.keys().cloned().collect(),
    };
    if names.is_empty() {
        println!("No targets configured.");
        return 0;
    }
    let mut problems = 0;
    for tname in names {
        let Some(rt) = lab.resolved_target(&tname) else {
            eprintln!("Target '{tname}' not found in lab.");
            problems += 1;
            continue;
        };
        for ch in &rt.channels {
            if let Some(h) = host_filter {
                if ch.host != h {
                    continue;
                }
            }
            let (status, detail) = check_channel(lab, ch, &rt);
            if status.is_problem() {
                problems += 1;
            }
            println!(
                "{tname}\t{}\t@{}\t{}\t{}",
                channel_name(ch),
                ch.host,
                status.label(),
                detail
            );
        }
    }
    if problems == 0 {
        println!("All configured channels present.");
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_path_prefix_covers_user_and_system_libexec() {
        // The probe must search everywhere the hooks themselves resolve
        // helpers (daemons::helper_dirs()): a .deb install's helpers live only
        // in /usr/libexec/paniolo/bin, off PATH.
        assert!(HOOK_PATH_PREFIX.contains("$HOME/.local/libexec/paniolo/bin"));
        assert!(HOOK_PATH_PREFIX.contains("/usr/libexec/paniolo/bin"));
        // A standalone statement, so a following `command -v` honors it in
        // dash as well as bash.
        assert!(HOOK_PATH_PREFIX.ends_with(';'));
    }

    // ── behavioural tests ───────────────────────────────────────────────
    //
    // These RUN the probes. The string-shape tests below cover the POSIX
    // rendering used over SSH, but they pass identically on every platform —
    // which is exactly how `paniolo doctor` shipped depending on `sh -c`, a
    // binary that does not exist on Windows. A probe that cannot execute has
    // to fail a test somewhere, so it fails here.

    #[test]
    fn exists_probe_runs_natively() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("device");
        std::fs::write(&present, b"").unwrap();

        assert_eq!(
            Probe::Exists(present.to_string_lossy().into_owned()).run_local(),
            Some(0),
            "a file that exists must probe ok"
        );
        assert_eq!(
            Probe::Exists(
                dir.path()
                    .join("no-such-device")
                    .to_string_lossy()
                    .into_owned()
            )
            .run_local(),
            Some(1),
            "a missing file must probe missing, not error"
        );
    }

    #[test]
    fn hook_path_probe_runs_natively() {
        // A name nothing could resolve. The point is that the lookup runs to
        // completion and reports missing, rather than failing to launch.
        assert_eq!(
            Probe::OnHookPath("paniolo-no-such-helper-xyzzy".to_string()).run_local(),
            Some(1)
        );
    }

    // ── netboot host_ip drift (#202) ────────────────────────────────────
    //
    // The lab file's host_ip is only what the link *should* run at. netbootd's
    // IP monitor re-applies it while a daemon runs, so a stopped link drifts
    // indefinitely: a target sat at the default 192.168.99.1 for weeks while
    // its lab comment claimed another /24, colliding with a second link, and
    // nothing reported it.

    fn held(addrs: &[&str]) -> Vec<String> {
        addrs.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn an_interface_holding_its_host_ip_has_not_drifted() {
        assert_eq!(
            netboot_ip_drift(&held(&["192.168.99.1"]), "192.168.99.1"),
            None
        );
        // Linux prints a prefix; macOS does not. Both must compare equal.
        assert_eq!(
            netboot_ip_drift(&held(&["192.168.99.1/24"]), "192.168.99.1"),
            None,
            "the /NN prefix must be stripped before comparing"
        );
        // Serving at the right address alongside another one is still serving.
        assert_eq!(
            netboot_ip_drift(&held(&["10.0.0.5/8", "192.168.99.1/24"]), "192.168.99.1"),
            None
        );
    }

    #[test]
    fn an_interface_holding_no_address_has_not_drifted() {
        // `netif mode off` releases the address. That is a link that is down,
        // not a link running at the wrong place, and doctor already reports
        // the channel's state elsewhere.
        assert_eq!(netboot_ip_drift(&[], "192.168.99.1"), None);
    }

    #[test]
    fn an_interface_holding_only_another_address_has_drifted() {
        // The case from the report: configured for one /24, running in another.
        assert_eq!(
            netboot_ip_drift(&held(&["192.168.99.1/24"]), "192.168.100.1"),
            Some("192.168.99.1".to_string())
        );
        assert_eq!(
            netboot_ip_drift(&held(&["192.0.2.10"]), "192.168.99.1"),
            Some("192.0.2.10".to_string()),
            "a hand-assigned address counts too"
        );
    }

    /// The remedy has to be actionable *and* safe: `configure_interface`
    /// assigns the configured address and removes every other IPv4, so
    /// `netif mode link` repairs drift on its own.
    #[test]
    fn the_drift_detail_names_both_addresses_and_the_remedy() {
        let d = netboot_drift_detail("pi5", "eth4", "192.168.99.1", "192.168.100.1", false);
        assert!(d.contains("192.168.99.1"), "{d}");
        assert!(d.contains("192.168.100.1"), "{d}");
        assert!(d.contains("netif mode link pi5"), "{d}");
        assert!(!d.contains("(default)"), "{d}");
        // NOT `mode off` first: it deletes the configured address, which is
        // the one that is not assigned when a link has drifted, and it stops
        // a running netbootd on the way. `mode link` repairs drift alone.
        assert!(
            !d.contains("mode off"),
            "`mode off` cannot release a drifted address: {d}"
        );
        // An unset host_ip is marked, so the reader knows where it came from.
        let d = netboot_drift_detail("pi5", "eth4", "192.0.2.10", "192.168.99.1", true);
        assert!(d.contains("192.168.99.1 (default)"), "{d}");
    }

    /// The POSIX rendering, for a remote control host. Exit 0 covers both
    /// "no address" and "holds host_ip"; anything else prints the offending
    /// address, because the exit status alone would only say "some other one".
    #[test]
    fn the_holds_probe_renders_a_posix_script() {
        let script = net_interface_holds_script("eth4", "192.168.99.1");
        assert!(script.contains("ip -4 -o addr show dev eth4"), "{script}");
        assert!(script.contains("ifconfig eth4"), "macOS fallback: {script}");
        assert!(script.contains("192.168.99.1"), "{script}");
        assert!(script.contains("exit 1"), "{script}");
        // A non-interactive SSH shell often has neither sbin dir on PATH; if
        // both legs exit 127 the address list is empty and a drifted link
        // reads as fine, which is the one direction this must not fail in.
        assert!(script.contains("/usr/sbin"), "{script}");
        assert!(script.contains("/sbin"), "{script}");
        // "no output at all" is a probe that could not answer (exit 3), not a
        // link holding nothing.
        assert!(script.contains("exit 3"), "{script}");
        // Legacy net-tools and BusyBox print `inet addr:A.B.C.D`.
        assert!(script.contains("sub(/^addr:/"), "{script}");
        // An interface name cannot break out of the script: `shell_quote`
        // leaves a plain name bare and quotes anything that needs it. (Names
        // are validated on the way into the lab file too — this is the second
        // line of defense, for a file edited by hand.)
        let nasty = net_interface_holds_script("eth4; rm -rf /", "192.168.99.1");
        assert!(
            nasty.contains("'eth4; rm -rf /'"),
            "a name with a metacharacter must be quoted: {nasty}"
        );
    }

    /// The probe RUNS, rather than only rendering — on both Unix platforms,
    /// because `netif::iface_addresses` parses `ip -4 -o addr show` on Linux
    /// and `ifconfig` on macOS, two entirely different parsers. Asserting on
    /// the script text alone is how `doctor` once shipped depending on a
    /// binary that does not exist on Windows (AGENTS.md, "Testing the platform
    /// split"). Loopback is the one interface both platforms always have, with
    /// an address known in advance.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_holds_probe_runs_natively_against_loopback() {
        let lo = if cfg!(target_os = "macos") {
            "lo0"
        } else {
            "lo"
        };
        // A host whose loopback carries no address at all (a bare netns, a
        // container with an unconfigured lo) has nothing to compare, and
        // "holds nothing" is deliberately not drift — so there would be no
        // signal either way. Skip rather than fail on an unrelated condition.
        if crate::netif::iface_addresses(lo).0.is_empty() {
            return;
        }
        let holds = |host_ip: &str| {
            Probe::NetInterfaceHolds {
                iface: lo.to_string(),
                host_ip: host_ip.to_string(),
            }
            .run_local()
        };
        assert_eq!(holds("127.0.0.1"), Some(0), "{lo} holds 127.0.0.1");
        assert_eq!(
            holds("192.0.2.10"),
            Some(1),
            "{lo} does not hold 192.0.2.10, so this is drift"
        );
    }

    /// The remote half of the drift check: what the probe's exit status and
    /// stdout are allowed to claim (#202).
    ///
    /// Only the local branch was pinned, and every arm here is a statement
    /// about hardware the reader will act on. Two of them are refusals to
    /// speak, which is the part that rots quietly: collapsing "I could not
    /// ask" into "the link is fine" leaves a drifted netboot link reported as
    /// healthy, and collapsing it into MISMATCH invents an address nobody read.
    ///
    /// `None` is "unknown", `Some(None)` is "the link holds its host_ip",
    /// `Some(Some(addr))` is "it holds `addr` instead".
    #[test]
    fn the_remote_probe_claims_drift_only_when_it_read_an_address() {
        // Exit 0: the script found host_ip among the interface's addresses,
        // or the interface holds none at all. Either way, not drift.
        assert_eq!(drift_from_probe(0, ""), Some(None));

        // 255 is ssh's own failure and 3 is "neither `ip` nor `ifconfig` told
        // me anything". The question was not answered.
        assert_eq!(drift_from_probe(255, ""), None, "ssh failed; nothing known");
        assert_eq!(
            drift_from_probe(3, ""),
            None,
            "no tool answered; nothing known"
        );

        // Exit 1 with the address the script printed: the one case that is
        // drift, reported with the address, since the status alone would only
        // say "some other address".
        assert_eq!(
            drift_from_probe(1, "10.66.28.9\n"),
            Some(Some("10.66.28.9".to_string()))
        );
        assert_eq!(
            drift_from_probe(1, "  10.66.28.9  \nnoise\n"),
            Some(Some("10.66.28.9".to_string())),
            "the first line, trimmed, is the address"
        );

        // A non-zero exit with nothing on stdout is a probe that died — a
        // signal, a non-POSIX login shell — not a link that moved. Claiming
        // MISMATCH here would be a confident report about an address that was
        // never read.
        assert_eq!(drift_from_probe(1, ""), None);
        assert_eq!(drift_from_probe(1, "\n  \n"), None);
        assert_eq!(drift_from_probe(137, ""), None, "SIGKILL is not drift");
    }

    /// MISMATCH counts as a problem, so `paniolo doctor` exits non-zero and a
    /// script gating on it notices. A status that reported nothing would make
    /// the whole check decorative.
    #[test]
    fn a_mismatch_is_a_problem() {
        assert!(Status::Mismatch.is_problem());
        assert_eq!(Status::Mismatch.label(), "MISMATCH");
    }

    #[test]
    fn hook_probe_picks_path_vs_name_lookup() {
        assert!(matches!(hook_probe("/opt/bin/zigplug"), Probe::Exists(_)));
        assert!(matches!(hook_probe("zigplug"), Probe::OnHookPath(_)));
    }

    /// `check_channel`'s power and hid/usb arms both build their probe from
    /// `daemons::first_program_token(cmd)`, exactly as here — a hook written
    /// `AMT_PASSWORD=... amt-tool cycle` (docs/power.md "Credentials") must
    /// probe `amt-tool`, not the literal string `AMT_PASSWORD=...` (which the
    /// old `cmd.split_whitespace().next()` treated as the program, and
    /// `hook_probe` would then send down the `OnHookPath` branch as a bare
    /// name doctor could never find).
    #[test]
    fn hook_probe_skips_a_leading_env_assignment() {
        let cmd = "AMT_PASSWORD=hunter2 /opt/bin/amt-tool cycle";
        let prog = daemons::first_program_token(cmd).unwrap_or("");
        assert_eq!(prog, "/opt/bin/amt-tool");
        assert!(matches!(hook_probe(prog), Probe::Exists(p) if p == "/opt/bin/amt-tool"));
    }

    #[test]
    fn adb_probe_reports_missing_binary_distinctly() {
        // Exit 3 is what separates "adb is not installed" from "the device is
        // not reachable"; callers branch on it.
        let p = Probe::Adb {
            bin: "paniolo-no-such-adb-xyzzy".to_string(),
            serial: None,
        };
        assert_eq!(p.run_local(), Some(3));
    }

    #[test]
    fn video_probe_reports_missing_hdmicap_distinctly() {
        // A device path that cannot exist, so the probe falls through to the
        // hdmicap lookup. If hdmicap IS installed on this machine the fallback
        // runs it instead, which is also a valid answer — assert on the pair.
        let p = Probe::Video("/paniolo-no-such-capture-device-xyzzy".to_string());
        let rc = p.run_local();
        if crate::daemons::find_binary("hdmicap").is_some() {
            assert_eq!(rc, Some(1), "hdmicap present: device simply not listed");
        } else {
            assert_eq!(rc, Some(3), "hdmicap absent: exit 3, not a device failure");
        }
    }

    /// A network RFB device is never in `hdmicap devices`, so the capture-device
    /// probe would always call it MISSING. It must be judged on hdmicap alone:
    /// the detail names a network source (hdmicap present) or the missing
    /// hdmicap, never the device as absent.
    #[test]
    fn network_video_devices_are_not_reported_as_missing_capture_devices() {
        let lab = crate::model::parse(
            "[targets.t]\n[targets.t.hid]\ncmd = \"x\"\n[targets.t.video]\ndevice = \"rfb+hid:\"\n",
        )
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Video)
            .unwrap();
        let (status, detail) = check_channel(&lab, ch, &rt);
        if crate::daemons::find_binary("hdmicap").is_some() {
            assert!(matches!(status, Status::Ok), "{detail}");
            assert!(detail.contains("network source"), "{detail}");
        } else {
            assert!(matches!(status, Status::Missing), "{detail}");
            assert!(detail.contains("hdmicap not installed"), "{detail}");
        }
    }

    #[test]
    fn every_probe_renders_posix_for_the_remote_hop() {
        // The SSH path still ships shell text, so each variant must render.
        for p in [
            Probe::Exists("/dev/ttyUSB0".to_string()),
            Probe::OnHookPath("zigplug".to_string()),
            Probe::Video("/dev/video0".to_string()),
            Probe::NetInterface("eth0".to_string()),
            Probe::NetInterfaceHolds {
                iface: "eth0".to_string(),
                host_ip: "192.168.99.1".to_string(),
            },
            Probe::Adb {
                bin: "adb".to_string(),
                serial: Some("XYZ".to_string()),
            },
        ] {
            assert!(!p.to_posix().is_empty());
        }
    }

    #[test]
    fn video_probe_searches_helper_dirs_then_path_then_cargo() {
        let s = video_probe_script("/dev/video0");
        assert!(s.contains("/usr/libexec/paniolo/bin"), "{s}");
        assert!(s.contains("$HOME/.cargo/bin"), "{s}");
        assert!(s.contains("command -v hdmicap"), "{s}");
        assert!(s.starts_with("test -e /dev/video0 && exit 0"), "{s}");
    }
}
