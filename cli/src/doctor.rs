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

//! `panio doctor` — probe configured channels against reality.
//!
//! Read-only health check: for each channel it SSHes to the control host the
//! channel lives on (or runs locally) and tests that the configured device or
//! interface actually exists. This is the probing that used to be baked into the
//! Python `setup` commands, separated so config edits stay local and offline.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

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
    /// ([`netboot_ip_drift`]). The remote script exits 3 if neither address
    /// tool succeeds; no IPv4 in off/ffx modes is a successful check.
    Mismatch,
    /// A helper doctor check (#304) said `warn`: shown, not a problem.
    Warn,
    /// A helper doctor check said `fail`, timed out, or broke the contract.
    Fail,
    /// The helper has no `doctor` subcommand: reported once, not a problem.
    NoCheck,
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
            Status::Warn => "warn",
            Status::Fail => "FAIL",
            Status::NoCheck => "no-check",
        }
    }
    fn is_problem(&self) -> bool {
        matches!(
            self,
            Status::Missing
                | Status::Unreachable
                | Status::Conflict
                | Status::Mismatch
                | Status::Fail
        )
    }
}

/// One health check, expressed structurally rather than as shell text.
///
/// Locally a probe runs natively, so `panio doctor` needs no shell at all.
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
    /// A helper's own read-only health check (#304): `cmd` is the derived
    /// `<program> <transport options> doctor --json` ([`doctor_command_for_hook`],
    /// [`doctor_command_for_prefix`]), resolved like a hook (libexec dirs, then
    /// PATH). Killed after `timeout_secs`, which reads as exit [`TIMED_OUT`];
    /// stdout carries the JSON report ([`Probe::run_capture`]).
    HelperDoctor { cmd: String, timeout_secs: u64 },
}

impl Probe {
    /// Render as a POSIX-sh script, for probing a remote Unix control host.
    /// `plugin_path` is the host's lab `plugin_path`, which a dispatch there
    /// would pass on; `None` leaves the host's own `PANIO_PLUGIN_PATH` in
    /// charge (see [`hook_path_prefix`]).
    fn to_posix(&self, plugin_path: Option<&str>) -> String {
        match self {
            Probe::Exists(p) => format!("test -e {}", ssh::shell_quote(p)),
            Probe::OnHookPath(prog) => format!(
                "{} command -v {} >/dev/null",
                hook_path_prefix(plugin_path),
                ssh::shell_quote(prog)
            ),
            Probe::Video(dev) => video_probe_script(dev, plugin_path),
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
            Probe::HelperDoctor { cmd, timeout_secs } => {
                helper_doctor_script(cmd, *timeout_secs, plugin_path)
            }
        }
    }

    /// Run on this host and keep stdout too, for the probes whose answer is
    /// more than an exit status. Only [`Probe::HelperDoctor`] has one; every
    /// other probe returns its [`Probe::run_local`] code with empty output.
    fn run_capture(&self) -> Option<(i32, String)> {
        match self {
            Probe::HelperDoctor { cmd, timeout_secs } => {
                run_helper_doctor_local(cmd, Duration::from_secs(*timeout_secs))
            }
            other => other.run_local().map(|rc| (rc, String::new())),
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
            Probe::HelperDoctor { .. } => self.run_capture().map(|(rc, _)| rc),
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
/// before comparing. No IPv4 address (`netif mode off` or `ffx`) is not drift;
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
/// print the first other address and exit 1. Exit 3 only when both address
/// commands fail; a successful empty listing is valid in off/ffx modes.
fn net_interface_holds_script(iface: &str, host_ip: &str) -> String {
    let q_iface = ssh::shell_quote(iface);
    let q_ip = ssh::shell_quote(host_ip);
    format!(
        // `ip` lives in /usr/sbin and `ifconfig` in /sbin, and a
        // non-interactive SSH shell often has neither on PATH. Without this
        // both legs exit 127, the address list comes back empty, and a
        // drifted link reads as fine — the one direction this check must not
        // fail in. Same reason `hook_path_prefix` exists.
        "PATH=\"$PATH:/usr/sbin:/sbin\"; \
         out=$({{ ip -4 -o addr show dev {q_iface} || ifconfig {q_iface}; }} 2>/dev/null) || exit 3; \
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
        &["sh".to_string(), "-c".to_string(), p.to_posix(None)],
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
/// succeeded". Both mean the question was not answered, which must not
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
        "{iface} holds {held} but host_ip is {host_ip}{}; run `panio netif mode link \
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
        &[
            "sh".to_string(),
            "-c".to_string(),
            p.to_posix(host.plugin_path.as_deref()),
        ],
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

/// Shell statements giving hook commands the same resolution the hooks get at
/// runtime (`daemons::hook_path()`): the per-user libexec dir, then the system
/// package dir, then each plugin root's `libexec/panio/bin`, then PATH. The
/// literal paths must match `daemons::libexec_dir()` /
/// `daemons::system_libexec_dir()` / `daemons::plugin_helper_dir()`; `$HOME`
/// is expanded by the probed host's own shell so that host's home applies.
///
/// The plugin roots are the host's lab `plugin_path` when it has one (what a
/// dispatch there passes as `--plugin-path`), else the `PANIO_PLUGIN_PATH`
/// the host's own non-interactive SSH environment carries, which is what the
/// remote panio would read. Split on `:` in a subshell, so the caller's IFS
/// and globbing are untouched, with relative entries skipped as
/// `daemons::parse_plugin_path` skips them.
///
/// Ends in a standalone assignment (not a `VAR=… cmd` prefix) so the
/// `command -v` lookup that follows honors it in every /bin/sh, dash
/// included. Exe-relative helper dirs (a Homebrew keg) can't be derived for a
/// remote host, so a keg-only install on a *remote* control host may still
/// probe MISSING.
fn hook_path_prefix(plugin_path: Option<&str>) -> String {
    let roots = match plugin_path {
        Some(p) => ssh::shell_quote(p),
        None => "\"${PANIO_PLUGIN_PATH:-}\"".to_string(),
    };
    format!(
        "pp={roots}; \
         pd=$(IFS=:; set -f; for r in $pp; do case $r in /*) \
         printf '%s/libexec/panio/bin:' \"$r\";; esac; done); \
         PATH=\"$HOME/.local/libexec/panio/bin:/usr/libexec/panio/bin:$pd$PATH\";"
    )
}

/// Probe script for a video channel. The device is usually a capture-device
/// NAME (e.g. "USB Video" on macOS), not a path, so `test -e` alone is wrong:
/// ask `hdmicap devices` on the channel host whether it enumerates. Path-style
/// devices (`/dev/video0`) still short-circuit via `test -e`. hdmicap resolves
/// like `daemons::find_binary()`: the helper dirs (via [`hook_path_prefix`]),
/// then PATH, then the legacy ~/.cargo/bin. Exit 3 = hdmicap itself is
/// missing, a distinct failure from a missing device.
fn video_probe_script(device: &str, plugin_path: Option<&str>) -> String {
    let q = ssh::shell_quote(device);
    let prefix = hook_path_prefix(plugin_path);
    format!(
        "test -e {q} && exit 0; \
         {prefix} PATH=\"$PATH:$HOME/.cargo/bin\"; \
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

// ── helper doctor checks (#304) ─────────────────────────────────────────────
//
// A channel that sets `doctor = true` asks its hook program for its own
// read-only health report. The contract (docs/dev/adding-power-helpers.md,
// "Doctor check"): `<program> <transport options> doctor --json` exits 0 with
// `{"checks": [{"name", "status": "ok|warn|fail", "detail"}]}` on stdout,
// changes no hardware state, and is killed after HELPER_DOCTOR_TIMEOUT.

/// How long a helper's `doctor --json` may run before it is killed and
/// reported as a failed check.
const HELPER_DOCTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// The exit status a killed-at-the-timeout check reads as, on both the local
/// and the remote path (GNU `timeout`'s convention).
const TIMED_OUT: i32 = 124;

/// The most of a helper's report panio reads; the rest is discarded.
const MAX_REPORT_BYTES: usize = 64 * 1024;

/// The longest name or detail printed from a helper's report.
const MAX_FIELD_CHARS: usize = 300;

/// The next shell word of `s` at or after byte `from`, as a byte span, or
/// `None` at the end of the command (or at a `#` comment). Quotes, backslash
/// escapes, `$(…)` and backticks are kept inside the word they belong to; an
/// unquoted operator (`;`, `|`, `&`, `<`, `>`, `(`, `)`, a newline) is an
/// error, because then the command is no longer one simple command and
/// "everything before the verb" stops meaning anything.
fn next_word(s: &str, from: usize) -> Result<Option<(usize, usize)>, String> {
    let b = s.as_bytes();
    let mut i = from;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    if i >= b.len() || b[i] == b'#' {
        return Ok(None);
    }
    let start = i;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' => break,
            b'\'' => i = skip_single(b, i + 1)?,
            b'"' => i = skip_double(b, i + 1)?,
            b'`' => i = skip_backtick(b, i + 1)?,
            b'\\' => i += 2,
            b'$' if b.get(i + 1) == Some(&b'(') => i = skip_subst(b, i + 2)?,
            c @ (b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'\n' | b'\r') => {
                return Err(format!("it contains the shell operator `{}`", c as char))
            }
            _ => i += 1,
        }
    }
    Ok(Some((start, i.min(b.len()))))
}

/// Past the closing `'` of a single-quoted string starting at `i`.
fn skip_single(b: &[u8], i: usize) -> Result<usize, String> {
    b[i.min(b.len())..]
        .iter()
        .position(|&c| c == b'\'')
        .map(|j| i + j + 1)
        .ok_or_else(|| "it has an unterminated single quote".to_string())
}

/// Past the closing `"` of a double-quoted string starting at `i`.
fn skip_double(b: &[u8], mut i: usize) -> Result<usize, String> {
    while i < b.len() {
        match b[i] {
            b'"' => return Ok(i + 1),
            b'\\' => i += 2,
            b'`' => i = skip_backtick(b, i + 1)?,
            b'$' if b.get(i + 1) == Some(&b'(') => i = skip_subst(b, i + 2)?,
            _ => i += 1,
        }
    }
    Err("it has an unterminated double quote".to_string())
}

/// Past the closing backtick of a command substitution starting at `i`.
fn skip_backtick(b: &[u8], mut i: usize) -> Result<usize, String> {
    while i < b.len() {
        match b[i] {
            b'`' => return Ok(i + 1),
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    Err("it has an unterminated backtick".to_string())
}

/// Past the `)` closing a `$(` whose body starts at `i`.
fn skip_subst(b: &[u8], mut i: usize) -> Result<usize, String> {
    let mut depth = 1;
    while i < b.len() {
        match b[i] {
            b'\'' => i = skip_single(b, i + 1)?,
            b'"' => i = skip_double(b, i + 1)?,
            b'\\' => i += 2,
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => i += 1,
        }
    }
    Err("it has an unterminated $(".to_string())
}

/// Does option word `w` take the next word as its value? `--name=value` and
/// a short option with its value attached (`-dHOST`) do not; a bare `--name`
/// or `-d` does, except for AMT's boolean `--allow-insecure-http`. Other
/// boolean global switches must use `--name=value` for this derivation.
fn option_takes_value(w: &str) -> bool {
    if let Some(long) = w.strip_prefix("--") {
        long != "allow-insecure-http" && !long.contains('=')
    } else {
        w.len() == 2
    }
}

/// The helper doctor command for a *full* hook (`power` `*_cmd`, an action's
/// `cmd`): the hook text up to its verb, then ` doctor --json`.
///
/// Exactly what is kept, read left to right as shell words: any leading
/// `VAR=value` assignments; the program; then each option word (one starting
/// with `-`) together with its value ([`option_takes_value`]). The first word
/// that is not an option — the verb, and everything after it — is dropped,
/// as is a `--` end-of-options marker. So `AMT_PASSWORD=x amt -d 192.0.2.10
/// -u admin cycle --delay-ms 500` becomes `AMT_PASSWORD=x amt -d 192.0.2.10
/// -u admin doctor --json`. The kept text is copied verbatim, quoting
/// included, so it runs exactly the way the hook does.
pub(crate) fn doctor_command_for_hook(hook: &str) -> Result<String, String> {
    let mut end = 0;
    loop {
        let Some((a, z)) = next_word(hook, end)? else {
            return Err("it names no program".to_string());
        };
        end = z;
        if !daemons::is_env_assignment(&hook[a..z]) {
            break;
        }
    }
    while let Some((a, z)) = next_word(hook, end)? {
        let w = &hook[a..z];
        if w == "-" || w == "--" || !w.starts_with('-') {
            break;
        }
        end = z;
        if option_takes_value(w) {
            match next_word(hook, end)? {
                Some((_, v)) => end = v,
                None => break,
            }
        }
    }
    Ok(format!("{} doctor --json", hook[..end].trim_end()))
}

/// The helper doctor command for a *prefix* (`hid`/`usb` `cmd`, which
/// panio already appends verbs to): the whole command, then
/// ` doctor --json`. It must still be one simple command.
pub(crate) fn doctor_command_for_prefix(cmd: &str) -> Result<String, String> {
    let mut end = 0;
    let mut program = false;
    while let Some((a, z)) = next_word(cmd, end)? {
        program |= !daemons::is_env_assignment(&cmd[a..z]);
        end = z;
    }
    if !program {
        return Err("it names no program".to_string());
    }
    Ok(format!("{} doctor --json", cmd[..end].trim_end()))
}

/// POSIX rendering of [`Probe::HelperDoctor`], for a remote Unix control
/// host: run the command in the background with a watchdog that SIGKILLs it
/// after `secs`, and exit [`TIMED_OUT`] when it died that way. The watchdog's
/// fds point at /dev/null, so a leftover `sleep` never holds the SSH channel
/// open. `cmd` is one simple command ([`next_word`] refuses operators), so
/// the background job is the helper itself, not a subshell around it.
fn helper_doctor_script(cmd: &str, secs: u64, plugin_path: Option<&str>) -> String {
    let prefix = hook_path_prefix(plugin_path);
    format!(
        "{prefix} _pd_t0=$(date +%s); \
         {cmd} </dev/null 2>/dev/null & _pd_p=$!; \
         {{ sleep {secs}; kill -9 $_pd_p; }} </dev/null >/dev/null 2>&1 & _pd_w=$!; \
         wait $_pd_p; _pd_rc=$?; kill $_pd_w 2>/dev/null; \
         test $_pd_rc -ne 0 && test $(( $(date +%s) - _pd_t0 )) -ge {secs} && exit {TIMED_OUT}; \
         exit $_pd_rc"
    )
}

/// Run a helper doctor command on this host the way a hook runs (the
/// platform shell, helper dirs first on PATH), keeping at most
/// [`MAX_REPORT_BYTES`] of stdout. Past `timeout` the whole process group is
/// killed — the shell and the helper it started — and the answer is
/// `(TIMED_OUT, "")`, never a hang. `None` when it could not be started.
fn run_helper_doctor_local(cmd: &str, timeout: Duration) -> Option<(i32, String)> {
    let mut c = crate::platform::shell_command(cmd);
    c.env("PATH", daemons::hook_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::own_process_group(&mut c);
    let mut child = c.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    // A reader thread, so a chatty helper cannot fill the pipe and stall
    // while this thread waits on it; past the cap the rest is drained.
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out)
            .take(MAX_REPORT_BYTES as u64)
            .read_to_end(&mut buf);
        let _ = tx.send(buf);
        let _ = std::io::copy(&mut out, &mut std::io::sink());
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                crate::platform::kill_process_tree(child.id() as i32);
                let _ = child.kill();
                let _ = child.wait();
                return Some((TIMED_OUT, String::new()));
            }
        }
    };
    // The helper exited, so its stdout is closed unless something it left
    // running still holds it; don't wait on that for long.
    let buf = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    Some((
        crate::error::shell_code(status),
        String::from_utf8_lossy(&buf).into_owned(),
    ))
}

/// One entry of a helper's report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HelperCheck {
    name: String,
    status: CheckStatus,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

/// What a helper doctor run came to.
#[derive(Debug, PartialEq, Eq)]
enum HelperReport {
    /// A valid report.
    Checks(Vec<HelperCheck>),
    /// A non-zero exit with no valid report: the helper has no `doctor`
    /// subcommand (clap's "unrecognized subcommand", exit 2, is the usual
    /// shape). Reported once and not a problem.
    NoCheck,
    /// Exit 0 without a valid report, or a report that breaks the contract.
    Malformed(String),
    /// Killed at the timeout.
    TimedOut,
    /// The host could not be asked.
    Unreachable,
}

#[derive(Deserialize)]
struct RawReport {
    checks: Vec<RawCheck>,
}

#[derive(Deserialize)]
struct RawCheck {
    name: String,
    status: String,
    #[serde(default)]
    detail: String,
}

/// Parse a report, strictly: a `checks` array whose every entry has a name
/// and a status of exactly `ok`, `warn` or `fail`.
fn parse_report(stdout: &str) -> Result<Vec<HelperCheck>, String> {
    let raw: RawReport = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("its report is not valid JSON of the doctor contract: {e}"))?;
    raw.checks
        .into_iter()
        .map(|c| {
            let status = match c.status.as_str() {
                "ok" => CheckStatus::Ok,
                "warn" => CheckStatus::Warn,
                "fail" => CheckStatus::Fail,
                other => {
                    return Err(format!(
                        "check '{}' has status '{}', not ok, warn or fail",
                        printable(&c.name),
                        printable(other)
                    ))
                }
            };
            if c.name.trim().is_empty() {
                return Err("a check has an empty name".to_string());
            }
            Ok(HelperCheck {
                name: printable(&c.name),
                status,
                detail: printable(&c.detail),
            })
        })
        .collect()
}

/// A helper-supplied string made safe for one tab-separated report line:
/// control characters (tabs and newlines included) become spaces, and the
/// length is capped.
fn printable(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_FIELD_CHARS)
        .collect();
    if s.chars().count() > MAX_FIELD_CHARS {
        out.push('…');
    }
    out.trim().to_string()
}

/// The verdict from a helper doctor run's exit status (`None`: the host
/// could not be asked) and stdout. A valid report counts whatever the exit
/// status — the contract asks for 0, but a report is the better evidence.
fn interpret_helper_doctor(rc: Option<i32>, stdout: &str) -> HelperReport {
    let parsed = parse_report(stdout);
    if let Ok(checks) = parsed {
        return HelperReport::Checks(checks);
    }
    match rc {
        None => HelperReport::Unreachable,
        Some(TIMED_OUT) => HelperReport::TimedOut,
        Some(0) => HelperReport::Malformed(match parsed {
            Err(e) if !stdout.trim().is_empty() => e,
            _ => "it exited 0 without a report".to_string(),
        }),
        Some(_) => HelperReport::NoCheck,
    }
}

/// Run one helper doctor command on `host_name`: natively when local, as a
/// POSIX script over SSH otherwise. ssh's own failure (255) with no report is
/// "unreachable".
fn run_helper_doctor(lab: &Lab, host_name: &str, cmd: &str) -> HelperReport {
    let p = Probe::HelperDoctor {
        cmd: cmd.to_string(),
        timeout_secs: HELPER_DOCTOR_TIMEOUT.as_secs(),
    };
    let host = lab.host(host_name);
    let (rc, stdout) = if host.is_local(host_name) {
        match p.run_capture() {
            Some((rc, out)) => (Some(rc), out),
            None => (None, String::new()),
        }
    } else {
        match ssh::run(
            &host,
            &[
                "sh".to_string(),
                "-c".to_string(),
                p.to_posix(host.plugin_path.as_deref()),
            ],
            None,
            &[],
        ) {
            Ok(o) => ((o.status != 255).then_some(o.status), o.stdout),
            Err(_) => (None, String::new()),
        }
    };
    interpret_helper_doctor(rc, &stdout)
}

/// One line of the doctor report under a channel: `sub` extends the channel
/// column (`power doctor: reachable`), `None` is the channel's own line.
struct Row {
    sub: Option<String>,
    status: Status,
    detail: String,
}

/// The rows one helper doctor run contributes. `label` names it in the
/// channel column — `doctor`, or `doctor <program>` when a channel's hooks
/// name more than one helper.
fn helper_rows(label: &str, program: &str, report: HelperReport) -> Vec<Row> {
    let row = |sub: String, status, detail: String| Row {
        sub: Some(sub),
        status,
        detail,
    };
    match report {
        HelperReport::Checks(checks) if checks.is_empty() => vec![row(
            label.to_string(),
            Status::Ok,
            format!("{program} reported no checks"),
        )],
        HelperReport::Checks(checks) => checks
            .into_iter()
            .map(|c| {
                let status = match c.status {
                    CheckStatus::Ok => Status::Ok,
                    CheckStatus::Warn => Status::Warn,
                    CheckStatus::Fail => Status::Fail,
                };
                row(format!("{label}: {}", c.name), status, c.detail)
            })
            .collect(),
        HelperReport::NoCheck => vec![row(
            label.to_string(),
            Status::NoCheck,
            format!("helper has no doctor check ({program})"),
        )],
        HelperReport::Malformed(why) => vec![row(
            label.to_string(),
            Status::Fail,
            format!("{program} broke the doctor contract: {why}"),
        )],
        HelperReport::TimedOut => vec![row(
            label.to_string(),
            Status::Fail,
            format!(
                "{program} doctor timed out after {} s",
                HELPER_DOCTOR_TIMEOUT.as_secs()
            ),
        )],
        HelperReport::Unreachable => vec![row(
            label.to_string(),
            Status::Unreachable,
            "host unreachable".to_string(),
        )],
    }
}

/// One helper doctor command a channel asks for.
#[derive(Debug, PartialEq, Eq)]
struct DoctorSource {
    /// Which set of hooks it came from: `default` (a power channel's own
    /// hooks), `level <name>` (#302), `action <name>`, or `cmd` (hid/usb).
    /// Names the run in the report when a channel has more than one.
    group: String,
    /// The hook key it was derived from (`cycle_cmd`, `cmd`, …).
    hook: &'static str,
    /// The derived command, or why none could be derived.
    derived: Result<String, String>,
}

/// The helper doctor commands a channel asks for, in hook order, one per
/// distinct command (four power hooks on one plug run its check once, and a
/// power level on the same plug dedups to it). Empty unless the channel
/// opted in with `doctor = true`.
fn helper_doctor_commands(
    lab: &Lab,
    ch: &ResolvedChannel,
    rt: &ResolvedTarget,
) -> Vec<DoctorSource> {
    let mut found: Vec<DoctorSource> = Vec::new();
    let mut push = |group: String, hook: &'static str, derived: Result<String, String>| {
        if !found.iter().any(|s| s.derived == derived) {
            found.push(DoctorSource {
                group,
                hook,
                derived,
            });
        }
    };
    match ch.kind {
        ChannelKind::Power if field(ch, "doctor").is_some() => {
            for key in ["cycle_cmd", "on_cmd", "off_cmd", "state_cmd"] {
                if let Some(cmd) = field(ch, key) {
                    push("default".to_string(), key, doctor_command_for_hook(cmd));
                }
            }
            // Every named level (#302) too: a `hard` level is usually a
            // different helper (a plug) from the default or `soft` one (a
            // board MCU), and that is the hardware most worth asking about.
            if let Some(p) = lab.targets.get(&rt.name).and_then(|t| t.power.as_ref()) {
                for (name, level) in &p.levels {
                    for (key, cmd) in level.hooks() {
                        if let Some(cmd) = cmd {
                            push(format!("level {name}"), key, doctor_command_for_hook(cmd));
                        }
                    }
                }
            }
        }
        ChannelKind::Hid | ChannelKind::Usb if field(ch, "doctor").is_some() => {
            if let Some(cmd) = field(ch, "cmd") {
                push("cmd".to_string(), "cmd", doctor_command_for_prefix(cmd));
            }
        }
        ChannelKind::Actions => {
            if let Some(acts) = lab
                .targets
                .get(&rt.name)
                .and_then(|t| t.actions.as_ref())
                .filter(|a| a.doctor)
            {
                for (name, a) in &acts.actions {
                    push(
                        format!("action {name}"),
                        "cmd",
                        doctor_command_for_hook(&a.cmd),
                    );
                }
            }
        }
        _ => {}
    }
    found
}

/// Every helper doctor row for a channel whose own check passed. With more
/// than one run, each is labeled by the hooks it came from:
/// `power doctor (level hard): reachable`.
fn helper_doctor_rows(lab: &Lab, ch: &ResolvedChannel, rt: &ResolvedTarget) -> Vec<Row> {
    let cmds = helper_doctor_commands(lab, ch, rt);
    let several = cmds.len() > 1;
    let mut rows = Vec::new();
    for DoctorSource {
        group,
        hook,
        derived,
    } in cmds
    {
        let label = if several {
            format!("doctor ({group})")
        } else {
            "doctor".to_string()
        };
        match derived {
            Err(why) => rows.push(Row {
                sub: Some(label),
                status: Status::Fail,
                detail: format!("cannot derive a doctor command from {group} {hook}: {why}"),
            }),
            Ok(cmd) => {
                let program = daemons::first_program_token(&cmd).unwrap_or("helper");
                rows.extend(helper_rows(
                    &label,
                    program,
                    run_helper_doctor(lab, &ch.host, &cmd),
                ));
            }
        }
    }
    rows
}

/// A channel's report: its own check, then — when that passed and the
/// channel opted in — its helper's checks, merged under it.
fn channel_rows(lab: &Lab, ch: &ResolvedChannel, rt: &ResolvedTarget) -> Vec<Row> {
    let (status, detail) = check_channel(lab, ch, rt);
    let passed = status == Status::Ok;
    let mut rows = vec![Row {
        sub: None,
        status,
        detail,
    }];
    // A helper that is not even installed has nothing to ask.
    if passed {
        rows.extend(helper_doctor_rows(lab, ch, rt));
    }
    rows
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
                "no tftp_root configured (panio netboot set -t {} --tftp-root <path>)",
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
                             own /24 (panio netboot set --host-ip)",
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
            // Each named level (#302) the same way; a missing program names
            // the level it belongs to.
            let mut detail = if configured.is_empty() {
                "configured".to_string()
            } else {
                configured.join(",")
            };
            let levels = lab
                .targets
                .get(&rt.name)
                .and_then(|t| t.power.as_ref())
                .map(|p| p.levels.clone())
                .unwrap_or_default();
            for (name, level) in &levels {
                let mut keys: Vec<&str> = Vec::new();
                for (key, cmd) in level.hooks() {
                    let Some(cmd) = cmd else { continue };
                    keys.push(key);
                    let prog = daemons::first_program_token(cmd).unwrap_or("");
                    let rc = probe(lab, &ch.host, &hook_probe(prog));
                    if rc != Some(0) {
                        let (status, d) = interpret(rc, prog);
                        return (status, format!("level '{name}': {d}"));
                    }
                }
                detail.push_str(&format!("; level {name}: {}", keys.join(",")));
            }
            (Status::Ok, detail)
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
            // adb is a system tool (PATH), not a panio libexec helper. Exit 3
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

/// The hosts a doctor run looks at, for the plugin-root report: `local`
/// (unless `--host` names another), plus every host a checked channel lives
/// on, filtered like the channels are.
fn doctor_hosts(lab: &Lab, targets: &[String], host_filter: Option<&str>) -> Vec<String> {
    let mut hosts = std::collections::BTreeSet::new();
    if host_filter.is_none_or(|h| h == crate::model::LOCAL) {
        hosts.insert(crate::model::LOCAL.to_string());
    }
    for t in targets {
        if let Some(rt) = lab.resolved_target(t) {
            for ch in &rt.channels {
                if host_filter.is_none_or(|h| ch.host == h) {
                    hosts.insert(ch.host.clone());
                }
            }
        }
    }
    hosts.into_iter().collect()
}

/// One plugin root's verdict on this machine: missing, present, or present
/// but holding neither a helper nor a skills dir. Names the plugin helpers a
/// bundled helper of the same name shadows, since those never run.
fn local_root_status(
    root: &std::path::Path,
    shadowed: &[(String, std::path::PathBuf)],
) -> (Status, String) {
    if !root.is_dir() {
        return (Status::Missing, "no such directory".to_string());
    }
    let helpers = daemons::plugin_helper_dir(root);
    let skills = root.join("share/panio/skills");
    if !helpers.is_dir() && !skills.is_dir() {
        return (
            Status::Incomplete,
            "has neither libexec/panio/bin nor share/panio/skills".to_string(),
        );
    }
    let lost: Vec<&str> = shadowed
        .iter()
        .filter(|(_, r)| r == root)
        .map(|(n, _)| n.as_str())
        .collect();
    if lost.is_empty() {
        (Status::Ok, "present".to_string())
    } else {
        (
            Status::Ok,
            format!("present; shadowed by a bundled helper: {}", lost.join(", ")),
        )
    }
}

/// Report the plugin roots (`PANIO_PLUGIN_PATH`, GitHub #303) of each host
/// in `hosts`, one line per root, and return how many are missing. This
/// machine reports its own environment. A remote host reports its lab
/// `plugin_path`, the roots a dispatch there passes on; one without the field
/// is not reported, since its roots live in an environment this side does not
/// read. Prints nothing when no host has a root.
fn report_plugin_roots(lab: &Lab, hosts: &[String]) -> i32 {
    let mut problems = 0;
    let mut local_done = false;
    for host_name in hosts {
        let host = lab.host(host_name);
        let mut rows: Vec<(String, Status, String)> = Vec::new();
        if host.is_local(host_name) {
            if local_done {
                continue;
            }
            local_done = true;
            let raw = std::env::var_os(daemons::PLUGIN_PATH_ENV).unwrap_or_default();
            let (roots, ignored) = daemons::parse_plugin_path(&raw);
            let (_, shadowed) = daemons::helper_entries();
            for root in roots {
                let (status, detail) = local_root_status(&root, &shadowed);
                rows.push((root.display().to_string(), status, detail));
            }
            for rel in ignored {
                rows.push((
                    rel.display().to_string(),
                    Status::Incomplete,
                    "not an absolute path; ignored".to_string(),
                ));
            }
        } else if let Some(pp) = &host.plugin_path {
            // The host's own separator is not knowable from here; doctor's
            // remote probes assume a POSIX control host, so `:` it is.
            for entry in pp.split(':').filter(|e| !e.is_empty()) {
                if !entry.starts_with('/') {
                    rows.push((
                        entry.to_string(),
                        Status::Incomplete,
                        "not an absolute path; ignored".to_string(),
                    ));
                    continue;
                }
                let (status, detail) = interpret(
                    probe(lab, host_name, &Probe::Exists(entry.to_string())),
                    "present",
                );
                let detail = if status == Status::Missing {
                    "no such directory".to_string()
                } else {
                    detail
                };
                rows.push((entry.to_string(), status, detail));
            }
        }
        for (root, status, detail) in rows {
            if status.is_problem() {
                problems += 1;
            }
            println!(
                "plugins\t{root}\t@{host_name}\t{}\t{detail}",
                status.label()
            );
        }
    }
    problems
}

/// How many rows `doctor` counts toward its exit status: a helper's `fail`
/// does, its `warn` and a missing doctor subcommand do not.
fn count_problems(rows: &[Row]) -> i32 {
    rows.iter().filter(|r| r.status.is_problem()).count() as i32
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
    let mut problems = report_plugin_roots(lab, &doctor_hosts(lab, &names, host_filter));
    if names.is_empty() {
        println!("No targets configured.");
        return problems;
    }
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
            let rows = channel_rows(lab, ch, &rt);
            problems += count_problems(&rows);
            for row in rows {
                let name = match &row.sub {
                    Some(sub) => format!("{} {sub}", channel_name(ch)),
                    None => channel_name(ch),
                };
                println!(
                    "{tname}\t{name}\t@{}\t{}\t{}",
                    ch.host,
                    row.status.label(),
                    row.detail
                );
            }
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
        // in /usr/libexec/panio/bin, off PATH.
        for pp in [None, Some("/opt/lab-plugins")] {
            let prefix = hook_path_prefix(pp);
            assert!(prefix.contains("$HOME/.local/libexec/panio/bin"));
            assert!(prefix.contains("/usr/libexec/panio/bin"));
            // A standalone statement, so a following `command -v` honors it
            // in dash as well as bash.
            assert!(prefix.ends_with(';'));
        }
    }

    /// Runs the remote hook-path prefix in a real `sh`, with `HOME` pointed at
    /// a scratch dir: a plugin helper resolves (from the lab value, or from
    /// the host's own `PANIO_PLUGIN_PATH` when the lab has none), a
    /// relative entry is skipped, and a bundled helper of the same name wins.
    #[cfg(unix)]
    #[test]
    fn hook_path_prefix_resolves_plugin_helpers_after_bundled_ones() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let bundled = home.join(".local/libexec/panio/bin");
        let root = tmp.path().join("plugins");
        let pdir = daemons::plugin_helper_dir(&root);
        for (dir, name) in [(&bundled, "dup"), (&pdir, "dup"), (&pdir, "labctl")] {
            std::fs::create_dir_all(dir).unwrap();
            let f = dir.join(name);
            std::fs::write(&f, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let run = |pp: Option<&str>, env_pp: Option<&str>, prog: &str| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(format!("{} command -v {prog}", hook_path_prefix(pp)))
                .env("HOME", &home)
                .env("PATH", "/usr/bin:/bin")
                .env_remove(daemons::PLUGIN_PATH_ENV);
            if let Some(v) = env_pp {
                cmd.env(daemons::PLUGIN_PATH_ENV, v);
            }
            let out = cmd.output().unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )
        };
        let root_s = root.to_str().unwrap();
        let with_rel = format!("relative/dir:{root_s}");
        assert_eq!(
            run(Some(&with_rel), None, "labctl"),
            (Some(0), pdir.join("labctl").display().to_string())
        );
        assert_eq!(
            run(None, Some(root_s), "labctl"),
            (Some(0), pdir.join("labctl").display().to_string()),
            "without a lab value the host's own variable applies"
        );
        // Not found: dash says 127, bash 1. Either is a miss.
        assert_ne!(run(None, None, "labctl").0, Some(0));
        assert_eq!(
            run(Some(root_s), None, "dup"),
            (Some(0), bundled.join("dup").display().to_string()),
            "a bundled helper wins a name clash"
        );
    }

    #[test]
    fn local_plugin_root_status_flags_missing_and_empty_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let (status, _) = local_root_status(&tmp.path().join("nope"), &[]);
        assert!(status == Status::Missing);
        let (status, _) = local_root_status(tmp.path(), &[]);
        assert!(status == Status::Incomplete);
        std::fs::create_dir_all(daemons::plugin_helper_dir(tmp.path())).unwrap();
        let shadowed = vec![("hdmicap".to_string(), tmp.path().to_path_buf())];
        let (status, detail) = local_root_status(tmp.path(), &shadowed);
        assert!(status == Status::Ok);
        assert!(
            detail.contains("shadowed by a bundled helper: hdmicap"),
            "{detail}"
        );
    }

    // ── behavioural tests ───────────────────────────────────────────────
    //
    // These RUN the probes. The string-shape tests below cover the POSIX
    // rendering used over SSH, but they pass identically on every platform —
    // which is exactly how `panio doctor` shipped depending on `sh -c`, a
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
            Probe::OnHookPath("panio-no-such-helper-xyzzy".to_string()).run_local(),
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
        // `netif mode off` or IPv6-only `ffx` leaves no IPv4 address. Neither
        // is a link running at the wrong address.
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
        // Both tools failing is unknown (exit 3); successful empty output
        // means the interface holds no IPv4 address (#251).
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

    /// Execute the remote script with controlled tool results, without touching
    /// an interface. In particular, empty stdout is not command failure (#251).
    #[cfg(unix)]
    #[test]
    fn remote_holds_probe_distinguishes_empty_addresses_from_failed_tools() {
        let cases = [
            ("empty ip", 0, "", 0, "inet 192.0.2.10", 0, ""),
            (
                "matching ip",
                0,
                "2: eth4 inet 192.168.99.1/24 scope global eth4",
                99,
                "",
                0,
                "",
            ),
            (
                "matching secondary ip",
                0,
                "2: eth4 inet 192.0.2.10/24\n2: eth4 inet 192.168.99.1/24",
                99,
                "",
                0,
                "",
            ),
            (
                "multiple different ips",
                0,
                "2: eth4 inet 192.0.2.10/24\n2: eth4 inet 198.51.100.1/24",
                99,
                "",
                1,
                "192.0.2.10\n",
            ),
            (
                "different ip",
                0,
                "2: eth4 inet 192.0.2.10/24 scope global eth4",
                99,
                "",
                1,
                "192.0.2.10\n",
            ),
            ("empty fallback", 127, "", 0, "", 0, ""),
            (
                "IPv6-only fallback",
                1,
                "",
                0,
                "eth4: flags=UP\n    inet6 fe80::1 prefixlen 64",
                0,
                "",
            ),
            (
                "matching fallback",
                127,
                "",
                0,
                "    inet 192.168.99.1 netmask 0xffffff00",
                0,
                "",
            ),
            (
                "legacy fallback drift",
                1,
                "",
                0,
                "    inet addr:192.0.2.10 Mask:255.255.255.0",
                1,
                "192.0.2.10\n",
            ),
            ("missing tools", 127, "", 127, "", 3, ""),
            ("failed tools", 1, "", 1, "", 3, ""),
            (
                "failed tools with partial output",
                1,
                "inet 192.168.99.1/24",
                1,
                "inet 192.0.2.10",
                3,
                "",
            ),
        ];
        for (name, ip_status, ip_out, ifconfig_status, ifconfig_out, status, stdout) in cases {
            let script = format!(
                "ip() {{ printf '%s' {}; return {ip_status}; }}\n\
                 ifconfig() {{ printf '%s' {}; return {ifconfig_status}; }}\n{}",
                ssh::shell_quote(ip_out),
                ssh::shell_quote(ifconfig_out),
                net_interface_holds_script("eth4", "192.168.99.1"),
            );
            let output = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(status), "{name}");
            let actual_stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(actual_stdout, stdout, "{name}");
            let expected = match status {
                0 => Some(None),
                1 => Some(Some("192.0.2.10".to_string())),
                3 => None,
                _ => unreachable!(),
            };
            assert_eq!(drift_from_probe(status, &actual_stdout), expected, "{name}");
        }
    }

    /// The probe RUNS, rather than only rendering — on both Unix platforms,
    /// because `netif::iface_addresses` parses `ip -4 -o addr show` on Linux
    /// and `ifconfig` on macOS, two entirely different parsers. Asserting on
    /// the script text alone is how `doctor` once shipped depending on a
    /// binary that does not exist on Windows (docs/dev/platform-support.md,
    /// "Testing the platform split"). Loopback is the one interface both platforms always have, with
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

    /// MISMATCH counts as a problem, so `panio doctor` exits non-zero and a
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

    /// Power levels (#302): every level's hook programs are probed like the
    /// default hooks; a missing one is reported with the level's name, and
    /// an all-present channel lists each level's hooks. Unix only: the
    /// present program is `/bin/sh`.
    #[cfg(unix)]
    #[test]
    fn power_levels_are_probed_per_level() {
        let lab = crate::model::parse(
            "[targets.t.power]\ncycle_cmd = \"/bin/sh -c true\"\n\
             [targets.t.power.levels.hard]\ncycle_cmd = \"/bin/sh -c true\"\n\
             state_cmd = \"/bin/sh -c true\"\n",
        )
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Power)
            .unwrap();
        let (status, detail) = check_channel(&lab, ch, &rt);
        assert!(matches!(status, Status::Ok), "{detail}");
        assert_eq!(detail, "cycle_cmd; level hard: cycle_cmd,state_cmd");

        let lab = crate::model::parse(
            "[targets.t.power]\ncycle_cmd = \"/bin/sh -c true\"\n\
             [targets.t.power.levels.hard]\n\
             cycle_cmd = \"AMT_PASSWORD=x /panio-no-such-helper-xyzzy cycle\"\n",
        )
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Power)
            .unwrap();
        let (status, detail) = check_channel(&lab, ch, &rt);
        assert!(status.is_problem(), "{detail}");
        assert!(detail.starts_with("level 'hard': "), "{detail}");
        assert!(detail.contains("/panio-no-such-helper-xyzzy"), "{detail}");
    }

    #[test]
    fn adb_probe_reports_missing_binary_distinctly() {
        // Exit 3 is what separates "adb is not installed" from "the device is
        // not reachable"; callers branch on it.
        let p = Probe::Adb {
            bin: "panio-no-such-adb-xyzzy".to_string(),
            serial: None,
        };
        assert_eq!(p.run_local(), Some(3));
    }

    #[test]
    fn video_probe_reports_missing_hdmicap_distinctly() {
        // A device path that cannot exist, so the probe falls through to the
        // hdmicap lookup. If hdmicap IS installed on this machine the fallback
        // runs it instead, which is also a valid answer — assert on the pair.
        let p = Probe::Video("/panio-no-such-capture-device-xyzzy".to_string());
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
            Probe::HelperDoctor {
                cmd: "shellyplug -d 192.0.2.10 doctor --json".to_string(),
                timeout_secs: 10,
            },
        ] {
            assert!(!p.to_posix(None).is_empty());
        }
    }

    // ── helper doctor checks (#304) ─────────────────────────────────────

    #[test]
    fn a_hook_keeps_its_program_and_transport_options_and_drops_the_verb() {
        let d = |h: &str| doctor_command_for_hook(h).unwrap();
        assert_eq!(
            d("shellyplug -d 192.0.2.10 cycle 0"),
            "shellyplug -d 192.0.2.10 doctor --json"
        );
        // Credential assignments ride along, as the hook needs them too.
        assert_eq!(
            d("AMT_PASSWORD=x amt -d 192.0.2.10 -u admin cycle --delay-ms 500"),
            "AMT_PASSWORD=x amt -d 192.0.2.10 -u admin doctor --json"
        );
        // `--name=value` and an attached short value stand alone.
        assert_eq!(
            d("/opt/bin/zigplug --device=/dev/ttyUSB0 -b115200 on 00:11:22:33:44:55"),
            "/opt/bin/zigplug --device=/dev/ttyUSB0 -b115200 doctor --json"
        );
        // Quoting and substitutions are kept verbatim, so it runs as written.
        assert_eq!(
            d("AMT_PASSWORD=\"$(cat ~/.amt pw)\" amt -d 'host one' state"),
            "AMT_PASSWORD=\"$(cat ~/.amt pw)\" amt -d 'host one' doctor --json"
        );
        // `--` ends the options and is dropped with the verb.
        assert_eq!(d("boardctl -d x -- reset"), "boardctl -d x doctor --json");
        // A hook with no verb at all is the whole command.
        assert_eq!(d("boardctl"), "boardctl doctor --json");
        // What follows the verb is never scanned, so a trailing operator is fine.
        assert_eq!(
            d("cambrionix -d /dev/ttyUSB2 cycle 3 && echo done"),
            "cambrionix -d /dev/ttyUSB2 doctor --json"
        );
    }

    #[test]
    fn amt_insecure_http_switch_does_not_consume_the_verb_or_adjacent_options() {
        for (hook, expected) in [
            (
                "amt --allow-insecure-http state",
                "amt --allow-insecure-http doctor --json",
            ),
            (
                "amt -d bench1 --allow-insecure-http cycle --delay-ms 500",
                "amt -d bench1 --allow-insecure-http doctor --json",
            ),
            (
                "amt --allow-insecure-http -d bench1 --tls-ca-file /etc/amt/ca.pem state",
                "amt --allow-insecure-http -d bench1 --tls-ca-file /etc/amt/ca.pem doctor --json",
            ),
            (
                "amt --password-file --allow-insecure-http state",
                "amt --password-file --allow-insecure-http doctor --json",
            ),
        ] {
            assert_eq!(doctor_command_for_hook(hook).unwrap(), expected);
        }
    }

    #[test]
    fn a_hook_that_is_not_one_simple_command_has_no_doctor_command() {
        for bad in [
            "",
            "   ",
            "A=1 B=2",
            "foo -d x; reboot",
            "foo -d $(echo",
            "foo -d 'unterminated",
            "foo -d \"unterminated",
            "foo -d x|tee",
        ] {
            assert!(doctor_command_for_hook(bad).is_err(), "{bad:?}");
        }
        let e = doctor_command_for_hook("foo -d x; reboot").unwrap_err();
        assert!(e.contains("`;`"), "{e}");
    }

    #[test]
    fn a_prefix_command_is_kept_whole() {
        assert_eq!(
            doctor_command_for_prefix("ch9329 -d /dev/ttyUSB0").unwrap(),
            "ch9329 -d /dev/ttyUSB0 doctor --json"
        );
        assert_eq!(
            doctor_command_for_prefix("hidrig -d x  # bench rig").unwrap(),
            "hidrig -d x doctor --json"
        );
        assert!(doctor_command_for_prefix("hidrig -d x | cat").is_err());
        assert!(doctor_command_for_prefix("A=1").is_err());
    }

    fn check(name: &str, status: CheckStatus, detail: &str) -> HelperCheck {
        HelperCheck {
            name: name.to_string(),
            status,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn a_valid_report_parses_every_status() {
        let out = r#"{"checks": [
            {"name": "reachable", "status": "ok", "detail": "Plus Plug S, gen 2"},
            {"name": "firmware", "status": "warn", "detail": "update available"},
            {"name": "switch", "status": "fail", "detail": "no switch:0"},
            {"name": "bare", "status": "ok"}
        ], "extra": 1}"#;
        assert_eq!(
            interpret_helper_doctor(Some(0), out),
            HelperReport::Checks(vec![
                check("reachable", CheckStatus::Ok, "Plus Plug S, gen 2"),
                check("firmware", CheckStatus::Warn, "update available"),
                check("switch", CheckStatus::Fail, "no switch:0"),
                check("bare", CheckStatus::Ok, ""),
            ])
        );
        // A report is evidence whatever the exit status.
        assert!(matches!(
            interpret_helper_doctor(Some(1), out),
            HelperReport::Checks(c) if c.len() == 4
        ));
        assert_eq!(
            interpret_helper_doctor(Some(0), "{\"checks\": []}"),
            HelperReport::Checks(vec![])
        );
    }

    #[test]
    fn a_report_breaking_the_contract_is_malformed_not_skipped() {
        for out in [
            "",
            "all good",
            "{\"checks\": {}}",
            "{\"results\": []}",
            "{\"checks\": [{\"name\": \"x\", \"status\": \"OK\"}]}",
            "{\"checks\": [{\"name\": \"x\", \"status\": \"error\"}]}",
            "{\"checks\": [{\"status\": \"ok\"}]}",
            "{\"checks\": [{\"name\": \" \", \"status\": \"ok\"}]}",
        ] {
            assert!(
                matches!(
                    interpret_helper_doctor(Some(0), out),
                    HelperReport::Malformed(_)
                ),
                "{out:?}"
            );
        }
        let HelperReport::Malformed(why) = interpret_helper_doctor(
            Some(0),
            "{\"checks\": [{\"name\": \"x\", \"status\": \"error\"}]}",
        ) else {
            unreachable!()
        };
        assert!(why.contains("'error'"), "{why}");
    }

    #[test]
    fn a_helper_without_a_doctor_subcommand_is_not_a_failure() {
        // clap's "unrecognized subcommand" is exit 2 with a message on stderr.
        assert_eq!(interpret_helper_doctor(Some(2), ""), HelperReport::NoCheck);
        assert_eq!(
            interpret_helper_doctor(Some(1), "usage: foo on|off"),
            HelperReport::NoCheck
        );
        assert_eq!(
            interpret_helper_doctor(Some(TIMED_OUT), ""),
            HelperReport::TimedOut
        );
        assert_eq!(interpret_helper_doctor(None, ""), HelperReport::Unreachable);
    }

    #[test]
    fn helper_strings_cannot_break_the_report_line() {
        let HelperReport::Checks(c) = interpret_helper_doctor(
            Some(0),
            "{\"checks\": [{\"name\": \"a\\tb\", \"status\": \"ok\", \"detail\": \"x\\ny\"}]}",
        ) else {
            unreachable!()
        };
        assert_eq!(c[0].name, "a b");
        assert_eq!(c[0].detail, "x y");
        let long = "z".repeat(MAX_FIELD_CHARS + 50);
        assert_eq!(printable(&long).chars().count(), MAX_FIELD_CHARS + 1);
    }

    /// The checks land under the channel, and only `fail` (and a broken or
    /// hung helper) counts toward doctor's exit status.
    #[test]
    fn helper_checks_merge_under_the_channel_and_only_fail_is_a_problem() {
        let rows = helper_rows(
            "doctor",
            "shellyplug",
            HelperReport::Checks(vec![
                check("reachable", CheckStatus::Ok, "up"),
                check("firmware", CheckStatus::Warn, "old"),
                check("switch", CheckStatus::Fail, "absent"),
            ]),
        );
        let subs: Vec<_> = rows.iter().map(|r| r.sub.clone().unwrap()).collect();
        assert_eq!(
            subs,
            ["doctor: reachable", "doctor: firmware", "doctor: switch"]
        );
        let labels: Vec<_> = rows.iter().map(|r| r.status.label()).collect();
        assert_eq!(labels, ["ok", "warn", "FAIL"]);
        assert_eq!(count_problems(&rows), 1);

        let none = helper_rows("doctor", "hidrig", HelperReport::NoCheck);
        assert_eq!(none.len(), 1, "reported once");
        assert_eq!(none[0].status.label(), "no-check");
        assert!(none[0]
            .detail
            .contains("helper has no doctor check (hidrig)"));
        assert_eq!(count_problems(&none), 0);

        let hung = helper_rows("doctor", "x", HelperReport::TimedOut);
        assert!(
            hung[0].detail.contains("timed out after 10 s"),
            "{}",
            hung[0].detail
        );
        assert_eq!(count_problems(&hung), 1);
        let bad = helper_rows("doctor", "x", HelperReport::Malformed("no".into()));
        assert_eq!(count_problems(&bad), 1);
    }

    #[test]
    fn the_helper_doctor_probe_renders_a_bounded_posix_script() {
        let s = Probe::HelperDoctor {
            cmd: "shellyplug -d 192.0.2.10 doctor --json".to_string(),
            timeout_secs: 10,
        }
        .to_posix(None);
        assert!(s.starts_with(&hook_path_prefix(None)), "{s}");
        assert!(
            s.contains("shellyplug -d 192.0.2.10 doctor --json </dev/null"),
            "{s}"
        );
        assert!(s.contains("sleep 10; kill -9"), "{s}");
        assert!(s.contains("exit 124"), "{s}");
    }

    /// The POSIX rendering RUNS: a hung helper comes back as exit 124 within
    /// the bound instead of holding the SSH channel open, and a well-behaved
    /// one passes its report and status through.
    #[cfg(unix)]
    #[test]
    fn the_rendered_script_times_out_and_passes_a_report_through() {
        let run = |cmd: &str| {
            let script = Probe::HelperDoctor {
                cmd: cmd.to_string(),
                timeout_secs: 1,
            }
            .to_posix(None);
            let t0 = Instant::now();
            let out = Command::new("sh").arg("-c").arg(script).output().unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                t0.elapsed(),
            )
        };
        let (rc, _, took) = run("sleep 30");
        assert_eq!(rc, Some(TIMED_OUT));
        assert!(took < Duration::from_secs(8), "took {took:?}");
        let (rc, out, _) = run("printf '%s' '{\"checks\":[]}'");
        assert_eq!(rc, Some(0));
        assert_eq!(out, "{\"checks\":[]}");
        let (rc, _, _) = run("false");
        assert_eq!(rc, Some(1), "a plain failure is not a timeout");
    }

    /// The local path kills a helper that sleeps past the bound — the whole
    /// process group, not only the shell — and reports the timeout code.
    #[cfg(unix)]
    #[test]
    fn a_local_helper_that_hangs_is_killed_at_the_timeout() {
        let t0 = Instant::now();
        let got = run_helper_doctor_local("sleep 30", Duration::from_secs(1));
        assert_eq!(got, Some((TIMED_OUT, String::new())));
        assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
        // A shell that forks the sleeper (not exec) is killed with it.
        let t0 = Instant::now();
        let got = run_helper_doctor_local("sleep 30; true", Duration::from_secs(1));
        assert_eq!(got, Some((TIMED_OUT, String::new())));
        assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
        let got = run_helper_doctor_local("printf ok; exit 3", Duration::from_secs(5));
        assert_eq!(got, Some((3, "ok".to_string())));
    }

    /// End to end on this host: a lab whose power hook names a fake helper
    /// that implements `doctor --json` for the hook's own transport options.
    #[cfg(unix)]
    #[test]
    fn doctor_runs_an_opted_in_helper_and_counts_its_failures() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("fakeplug");
        std::fs::write(
            &helper,
            "#!/bin/sh\n\
             if [ \"$1 $2 $3 $4\" = \"-d 192.0.2.10 doctor --json\" ]; then\n\
             printf '%s' '{\"checks\":[{\"name\":\"reachable\",\"status\":\"ok\",\"detail\":\"up\"},\
             {\"name\":\"auth\",\"status\":\"warn\",\"detail\":\"on\"},\
             {\"name\":\"switch\",\"status\":\"fail\",\"detail\":\"absent\"}]}'\n\
             exit 0\nfi\n\
             echo \"unrecognized subcommand\" >&2; exit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let h = helper.display();
        let lab_text = |doctor: bool, args: &str| {
            format!(
                "[targets.t.power]\ncycle_cmd = \"{h} {args} cycle 0\"\n\
                 state_cmd = \"{h} {args} state 0\"\ndoctor = {doctor}\n"
            )
        };
        let rows_for = |text: String| {
            let lab = crate::model::parse(&text).unwrap();
            let rt = lab.resolved_target("t").unwrap();
            let ch = rt
                .channels
                .iter()
                .find(|c| c.kind == ChannelKind::Power)
                .unwrap();
            channel_rows(&lab, ch, &rt)
        };

        let rows = rows_for(lab_text(true, "-d 192.0.2.10"));
        // The hook line, then one run's checks (two hooks, one command).
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (r.sub.clone(), r.status.label()))
            .collect();
        assert_eq!(
            summary,
            [
                (None, "ok"),
                (Some("doctor: reachable".to_string()), "ok"),
                (Some("doctor: auth".to_string()), "warn"),
                (Some("doctor: switch".to_string()), "FAIL"),
            ]
        );
        assert_eq!(count_problems(&rows), 1);

        // Other transport options: the fake answers like a helper with no
        // doctor subcommand. Reported once, not a problem.
        let rows = rows_for(lab_text(true, "-d 192.0.2.99"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].status.label(), "no-check");
        assert_eq!(count_problems(&rows), 0);

        // Not opted in: the helper is never asked.
        let rows = rows_for(lab_text(false, "-d 192.0.2.10"));
        assert_eq!(rows.len(), 1);
    }

    /// Power levels (#302) with `doctor = true` (#304): every level's hooks
    /// are asked too, a level on the same transport as another hook runs
    /// once, and a level whose hook cannot be derived says which level.
    #[test]
    fn doctor_on_a_power_channel_covers_every_level() {
        let lab = crate::model::parse(
            "[targets.t.power]\ncycle_cmd = \"shellyplug -d 192.0.2.10 cycle 0\"\n\
             state_cmd = \"shellyplug -d 192.0.2.10 state 0\"\ndoctor = true\n\
             [targets.t.power.levels.hard]\ncycle_cmd = \"shellyplug -d 192.0.2.10 cycle 1\"\n\
             [targets.t.power.levels.soft]\non_cmd = \"boardctl -u 3 on\"\n\
             off_cmd = \"boardctl -u 3 off\"\n\
             [targets.t.power.levels.strap]\ncycle_cmd = \"strapctl -d x; reboot\"\n",
        )
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Power)
            .unwrap();
        let got: Vec<_> = helper_doctor_commands(&lab, ch, &rt)
            .into_iter()
            .map(|s| (s.group, s.hook, s.derived.is_ok()))
            .collect();
        assert_eq!(
            got,
            [
                ("default".to_string(), "cycle_cmd", true),
                ("level soft".to_string(), "on_cmd", true),
                ("level strap".to_string(), "cycle_cmd", false),
            ]
        );
        let cmds = helper_doctor_commands(&lab, ch, &rt);
        assert_eq!(
            cmds[1].derived.as_deref(),
            Ok("boardctl -u 3 doctor --json")
        );

        // Not opted in: no level is asked either.
        let lab = crate::model::parse(
            "[targets.t.power]\ncycle_cmd = \"a -d x cycle\"\n\
             [targets.t.power.levels.hard]\ncycle_cmd = \"b -d y cycle\"\n",
        )
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        assert!(helper_doctor_commands(&lab, &rt.channels[0], &rt).is_empty());
    }

    /// End to end with levels: the default hooks' helper answers, the `hard`
    /// level's (another transport) has no doctor check. Each run is labeled
    /// by its level, and only the `fail` counts.
    #[cfg(unix)]
    #[test]
    fn doctor_labels_each_levels_run_and_counts_only_failures() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("fakeplug");
        std::fs::write(
            &helper,
            "#!/bin/sh\n\
             if [ \"$1 $2 $3 $4\" = \"-d 192.0.2.10 doctor --json\" ]; then\n\
             printf '%s' '{\"checks\":[{\"name\":\"reachable\",\"status\":\"ok\"},\
             {\"name\":\"switch\",\"status\":\"fail\",\"detail\":\"absent\"}]}'\n\
             exit 0\nfi\n\
             echo \"unrecognized subcommand\" >&2; exit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let h = helper.display();
        let lab = crate::model::parse(&format!(
            "[targets.t.power]\ncycle_cmd = \"{h} -d 192.0.2.10 cycle 0\"\ndoctor = true\n\
             [targets.t.power.levels.hard]\ncycle_cmd = \"{h} -d 192.0.2.11 cycle\"\n"
        ))
        .unwrap();
        let rt = lab.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Power)
            .unwrap();
        let rows = channel_rows(&lab, ch, &rt);
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (r.sub.clone(), r.status.label()))
            .collect();
        assert_eq!(
            summary,
            [
                (None, "ok"),
                (Some("doctor (default): reachable".to_string()), "ok"),
                (Some("doctor (default): switch".to_string()), "FAIL"),
                (Some("doctor (level hard)".to_string()), "no-check"),
            ]
        );
        assert_eq!(count_problems(&rows), 1);
    }

    #[test]
    fn video_probe_searches_helper_dirs_then_path_then_cargo() {
        let s = video_probe_script("/dev/video0", None);
        assert!(s.contains("/usr/libexec/panio/bin"), "{s}");
        assert!(s.contains("$HOME/.cargo/bin"), "{s}");
        assert!(s.contains("command -v hdmicap"), "{s}");
        assert!(s.starts_with("test -e /dev/video0 && exit 0"), "{s}");
    }
}
