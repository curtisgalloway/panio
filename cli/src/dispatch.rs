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

//! Per-channel transparent dispatch (see docs/config-redesign.md "Dispatch
//! design"). A location-transparent command resolves the host of the channel it
//! touches; if that's the dev machine it runs locally, otherwise paniolo re-execs
//! the same command on the control host over SSH against a shipped single-host
//! **slice** of the lab. Because the slice's channels carry no `host`, the remote
//! resolves them as local and never re-dispatches.

use std::path::Path;

use crate::labfile::LabFile;
use crate::model::{ChannelKind, Lab, LabError};
use crate::ssh;

/// Re-exec transport mode.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Non-interactive: stdio passed through.
    Reexec,
    /// PTY over `ssh -t` (e.g. tio serial console).
    Interactive,
}

/// Build a one-target lab containing only `target`'s channels that live on
/// `host`, with their `host` fields stripped so the remote treats them as local.
/// This is the slice shipped for remote re-exec (and the shape a host sees).
pub fn build_slice(lab: &Lab, target: &str, host: &str) -> Result<String, LabError> {
    let t = lab
        .targets
        .get(target)
        .ok_or_else(|| LabError(format!("no target '{target}'")))?;
    let default_host = t.default_host();
    let on = |h: &Option<String>| h.as_deref().unwrap_or(default_host) == host;

    let mut lf = LabFile::create(Path::new("slice.toml"));
    lf.add_target(target, None, t.description.as_deref())?;
    if let Some(nb) = &t.netboot {
        if on(&nb.host) {
            lf.set_netboot(
                target,
                nb.interface.as_deref(),
                nb.host_ip.as_deref(),
                nb.tftp_root.as_deref(),
                nb.boot_file.as_deref(),
                nb.http_port.as_deref(),
                nb.content_type.as_deref(),
                None,
            )?;
        }
    }
    for s in &t.serial {
        if on(&s.host) {
            lf.add_serial(
                target,
                &s.name,
                &s.device,
                s.baud,
                s.power_sense_signal.as_deref(),
                s.power_button,
                None,
                s.port,
            )?;
        }
    }
    if let Some(p) = &t.power {
        if on(&p.host) {
            lf.set_power(
                target,
                p.cycle_cmd.as_deref(),
                p.on_cmd.as_deref(),
                p.off_cmd.as_deref(),
                p.state_cmd.as_deref(),
                p.serial_interface.as_deref(),
                None,
            )?;
            for (name, l) in &p.levels {
                lf.set_power_level(
                    target,
                    name,
                    l.cycle_cmd.as_deref(),
                    l.on_cmd.as_deref(),
                    l.off_cmd.as_deref(),
                    l.state_cmd.as_deref(),
                )?;
            }
            if p.doctor {
                lf.set_doctor(target, "power", true)?;
            }
        }
    }
    if let Some(v) = &t.video {
        if on(&v.host) {
            lf.set_video(
                target,
                v.device.as_deref(),
                v.ocr_mode.as_deref(),
                v.change_threshold,
                None,
                v.port,
            )?;
        }
    }
    if let Some(h) = &t.hid {
        if on(&h.host) {
            lf.set_hid(target, h.cmd.as_deref(), None, h.port)?;
            if h.doctor {
                lf.set_doctor(target, "hid", true)?;
            }
        }
    }
    if let Some(a) = &t.adb {
        if on(&a.host) {
            lf.set_adb(target, a.serial.as_deref(), a.adb.as_deref(), None)?;
        }
    }
    if let Some(u) = &t.usb {
        if on(&u.host) {
            lf.set_usb(target, u.cmd.as_deref(), None)?;
            if u.doctor {
                lf.set_doctor(target, "usb", true)?;
            }
        }
    }
    if let Some(acts) = &t.actions {
        if on(&acts.host) {
            for (name, a) in &acts.actions {
                lf.set_action(
                    target,
                    name,
                    Some(&a.cmd),
                    a.description.as_deref(),
                    a.requires_power.as_deref(),
                    None,
                )?;
            }
            // The table only exists in the slice once an action is in it.
            if acts.doctor && !acts.actions.is_empty() {
                lf.set_doctor(target, "actions", true)?;
            }
        }
    }
    Ok(lf.doc.to_string())
}

/// Drop the global `--lab PATH` / `--lab=PATH` option from an argv tail; the
/// dev machine's lab path is meaningless on the control host (it gets a slice).
pub fn strip_lab_option(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--lab" {
            skip = true;
            continue;
        }
        if a.starts_with("--lab=") {
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// The subcommand argv to re-exec on the remote: this process's args minus the
/// program name and the global `--lab` option.
///
/// `args_os` rather than `args`: a non-UTF-8 argument (a device path or file
/// name from the shell's locale, say) must not panic the whole dispatch —
/// [`std::env::args`] does exactly that on invalid Unicode. Losing fidelity
/// on the rare non-UTF-8 byte is an acceptable trade for not crashing.
pub fn subcommand_args() -> Vec<String> {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    strip_lab_option(&args)
}

/// Write `slice_toml` to a file on `host` over SFTP and return its remote name.
///
/// This used to run a POSIX shell script (`f=$(mktemp …) && cat > "$f"`) to
/// create the file and echo its path. That works on a Unix control host and is
/// not a command at all on a Windows one, whose OpenSSH answers with PowerShell
/// — which is what stopped paniolo dispatching to a Windows host. SFTP is a
/// protocol rather than a shell, so it behaves the same on both.
///
/// The returned name is **relative**, and the remote `--lab` depends on that:
/// see [`ssh::sftp_put`] for why an absolute path is not usable on Windows.
///
/// Errors are classified here, where their source is still known: the SFTP
/// put is the first network contact of a dispatch, so its failure is the
/// usual sign of an unreachable control host (`unreachable`, exit 4), and an
/// `sftp` that is not installed is `not_configured` (3). A failure writing the
/// local temp copy is neither and stays unclassified. Exit 4 here strictly
/// means "the copy to the control host failed": a host that answers but
/// refuses the write, or a local ssh control directory that cannot be made
/// private, reports the same way; the message carries sftp's own reason.
pub fn ship_slice(
    host: &crate::model::Host,
    host_name: &str,
    target: &str,
    slice_toml: &str,
) -> anyhow::Result<String> {
    // Unique per invocation: several dispatches to one host can overlap, and a
    // shared name would let one clobber another's lab slice mid-run.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let remote = format!(".paniolo-lab-{}-{stamp}.toml", std::process::id());

    // The local copy only has to survive long enough for the SFTP put below;
    // `tempfile` picks a private, collision-free name in the system temp dir
    // and removes it on drop, so a failed put (or this process being killed)
    // never leaves a stray lab slice behind the way a hand-rolled name could.
    use anyhow::Context;
    let mut local = tempfile::NamedTempFile::new().context("creating the local lab slice")?;
    use std::io::Write;
    local
        .write_all(slice_toml.as_bytes())
        .context("writing the local lab slice")?;
    ssh::sftp_put(host, local.path(), &remote).map_err(|e| {
        let err = if e.kind() == std::io::ErrorKind::NotFound {
            crate::error::PanioloError::not_configured(format!(
                "sftp not found — cannot ship the lab slice to '{host_name}': {e}"
            ))
        } else {
            crate::error::unreachable_host(
                host_name,
                format!(
                    "cannot reach control host '{host_name}': failed to ship lab slice to {}: {e}",
                    host.ssh
                ),
            )
        };
        err.target(target)
    })?;
    Ok(remote)
}

/// Re-exec `sub_argv` on `host` against a shipped slice; return the remote exit
/// code, except that ssh's own 255 becomes an `unreachable` error (see
/// [`transport_checked`]). Cleans up the slice file afterward either way.
pub fn dispatch(
    lab: &Lab,
    target: &str,
    host_name: &str,
    mode: Mode,
    sub_argv: &[String],
) -> anyhow::Result<i32> {
    let host = lab.host(host_name);
    let slice = build_slice(lab, target, host_name)?;
    let remote_path = ship_slice(&host, host_name, target, &slice)?;

    let argv = remote_argv(&host, &remote_path, sub_argv);

    let code = match mode {
        // No environment crosses here: the remote's stdin *is* the terminal
        // the user is typing into (see `ssh::run_interactive`'s doc comment
        // for why that rules out a forwarding channel).
        Mode::Interactive => ssh::run_interactive(&host, &argv),
        Mode::Reexec => ssh::run_passthrough(&host, &argv, &ssh::forwarded_env()?),
    }?;

    // Best-effort cleanup of the shipped slice, before judging the code: a
    // 255 does not always mean the host is gone (see `transport_checked`),
    // and when it is, the removal just fails.
    let _ = ssh::sftp_rm(&host, &remote_path);
    transport_checked(code, host_name, target)
}

/// The remote exit code, unless it is ssh's own 255, which is reported as
/// `unreachable` (exit 4) instead of being passed on. OpenSSH exits 255 when
/// the connection fails (it has already printed why) but also when the remote
/// command is killed by a signal, and a passthrough child on the far side may
/// exit 255 itself; a remote paniolo never does under the error contract. So
/// the message says the outcome is unknown: check state before retrying a
/// mutation.
fn transport_checked(code: i32, host_name: &str, target: &str) -> anyhow::Result<i32> {
    if code == crate::error::SSH_TRANSPORT_FAILURE {
        return Err(crate::error::unreachable_host(
            host_name,
            crate::error::ssh_255_message(host_name),
        )
        .target(target)
        .into());
    }
    Ok(code)
}

/// The remote paniolo's argv for a user-facing dispatch: `paniolo
/// [--plugin-path=…] --lab <slice>` (see [`crate::model::Host::paniolo_argv`]),
/// then `--json-errors` when requested here, then `sub_argv`.
fn remote_argv(host: &crate::model::Host, remote_path: &str, sub_argv: &[String]) -> Vec<String> {
    let mut argv = host.paniolo_argv();
    argv.extend(["--lab".to_string(), remote_path.to_string()]);
    argv.extend(json_errors_arg(crate::error::json_requested()));
    argv.extend(sub_argv.iter().cloned());
    argv
}

/// `--json-errors` for the remote argv when the error contract's JSON object
/// was requested here (by the flag or `PANIOLO_JSON_ERRORS`). Added even when
/// `sub_argv` already has one: the flag accepts repeats, and scanning argv
/// for it cannot tell an option from a payload token of the same spelling.
/// An argument rather than a forwarded variable: the variable prelude needs a
/// POSIX shell on the control host and is skipped for interactive dispatch;
/// an argument crosses both. The cost is version skew: a 0.4.x remote
/// rejects the flag with a usage error (design D5).
fn json_errors_arg(requested: bool) -> Option<String> {
    requested.then(|| "--json-errors".to_string())
}

/// Run a paniolo subcommand on `host_name` against a shipped slice, captured.
/// Used by composite commands (e.g. `console`) to drive helper commands on the
/// host before tunnelling to its daemons. Never passes `--json-errors`: the
/// output is read by this process, which reports any failure itself.
pub fn run_subcommand(
    lab: &Lab,
    target: &str,
    host_name: &str,
    subargs: &[&str],
) -> anyhow::Result<ssh::Output> {
    let host = lab.host(host_name);
    let slice = build_slice(lab, target, host_name)?;
    let remote_path = ship_slice(&host, host_name, target, &slice)?;
    let mut argv = host.paniolo_argv();
    argv.extend(["--lab".to_string(), remote_path.clone()]);
    argv.extend(subargs.iter().map(|s| s.to_string()));
    let out = ssh::run(&host, &argv, None, &ssh::forwarded_env()?);
    let _ = ssh::sftp_rm(&host, &remote_path);
    let out = out?;
    if out.status == crate::error::SSH_TRANSPORT_FAILURE {
        // Captured, so ssh's own reason is in `out.stderr`, not on the
        // terminal: carry it in the message.
        return Err(crate::error::unreachable_host(
            host_name,
            format!(
                "{}: {}",
                crate::error::ssh_255_message(host_name),
                out.stderr.trim()
            ),
        )
        .target(target)
        .into());
    }
    Ok(out)
}

const CAPTURE_TMP_PREFIX: &str = ".paniolo-capture-";
const CAPTURE_TMP_SUFFIX: &str = ".tmp";

/// How old a capture temp file must be before [`sweep_stale_captures`]
/// deletes it. Far longer than any capture runs, so a concurrent capture into
/// the same directory never loses its temp file mid-transfer; a leftover that
/// survives an extra hour costs nothing.
const STALE_CAPTURE_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Delete capture temp files in `dir` left by an earlier capture that was
/// interrupted (#255). A SIGINT, SIGTERM or SIGKILL ends the process without
/// running `NamedTempFile`'s destructor, so the next capture cleans up
/// instead: only regular files with our prefix and suffix, owned by this
/// user, and older than [`STALE_CAPTURE_AGE`]. Best effort — any error just
/// leaves the file for a later sweep.
fn sweep_stale_captures(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(CAPTURE_TMP_PREFIX) || !name.ends_with(CAPTURE_TMP_SUFFIX) {
            continue;
        }
        let path = entry.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let stale = md
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_CAPTURE_AGE);
        if stale && crate::platform::is_own_file(&md) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Run `write_body` with a writable sink prepared for `out_path` — a sibling
/// temp file in the same directory, never `out_path` itself — and persist
/// that sink onto `out_path` only when `write_body` returns exit code 0.
///
/// Shared by every "stream a remote binary payload into a local file" path
/// (`video shot`, `adb screencap` — see [`dispatch_stdout_to_file`]) so the
/// atomicity guarantee lives in one place, testable without SSH: a failed or
/// partial capture leaves whatever was already at `out_path` untouched
/// instead of destroying it (the old code truncated `out_path` up front,
/// before the transfer even started), and a reader of `out_path` never sees
/// a partially-written file mid-transfer. On a non-zero exit the temp file
/// is simply dropped, which removes it; one orphaned by a killed process is
/// removed by a later capture's [`sweep_stale_captures`].
///
/// The exception is an `out_path` that already exists and is not a regular
/// file — `/dev/null`, a FIFO, `/dev/stdout` on a terminal. Rename cannot
/// replace those (and their parent, e.g. `/dev`, is usually not writable), and
/// there is no earlier copy to protect, so the body streams straight into it.
fn capture_to_file(
    out_path: &str,
    write_body: impl FnOnce(std::fs::File) -> anyhow::Result<i32>,
) -> anyhow::Result<i32> {
    if std::fs::metadata(out_path).is_ok_and(|m| !m.is_file()) {
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .open(out_path)
            .map_err(|e| anyhow::anyhow!("opening {out_path}: {e}"))?;
        return write_body(sink);
    }
    let dir = Path::new(out_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sweep_stale_captures(dir);
    let tmp = tempfile::Builder::new()
        .prefix(CAPTURE_TMP_PREFIX)
        .suffix(CAPTURE_TMP_SUFFIX)
        .tempfile_in(dir)
        .map_err(|e| anyhow::anyhow!("creating a temp file next to {out_path}: {e}"))?;
    let sink = tmp
        .as_file()
        .try_clone()
        .map_err(|e| anyhow::anyhow!("preparing {out_path}: {e}"))?;
    let code = write_body(sink)?;
    if code == 0 {
        tmp.persist(out_path)
            .map_err(|e| anyhow::anyhow!("saving {out_path}: {}", e.error))?;
    }
    Ok(code)
}

/// Like [`dispatch`], but the remote command's stdout streams into the local
/// file at `out_path` instead of the terminal — for remote commands producing
/// a binary payload (`video shot`, `adb screencap`), where `--out <path>` must
/// mean the invoking machine's filesystem, not the control host's. See
/// [`capture_to_file`] for the write-then-rename guarantee.
pub fn dispatch_stdout_to_file(
    lab: &Lab,
    target: &str,
    host_name: &str,
    sub_argv: &[String],
    out_path: &str,
) -> anyhow::Result<i32> {
    let host = lab.host(host_name);
    let slice = build_slice(lab, target, host_name)?;
    let remote_path = ship_slice(&host, host_name, target, &slice)?;

    let argv = remote_argv(&host, &remote_path, sub_argv);

    let env = ssh::forwarded_env()?;
    let code = capture_to_file(out_path, |sink| {
        Ok(ssh::run_stdout_to(&host, &argv, &env, sink)?)
    });

    // Best-effort cleanup first, as in `dispatch`.
    let _ = ssh::sftp_rm(&host, &remote_path);
    transport_checked(code?, host_name, target)
}

/// Read a daemon's discovery record — port and token — from its `daemon.json`
/// on `host`, or None. The path is resolved by a remote shell so the host's
/// own uid applies; must match `runtime_base()` in daemons.rs (and the daemon
/// crates). `subdir` is `<name>` for a host-singleton daemon or
/// `<name>/<target>` for a per-target one — build it with
/// [`crate::daemons::runtime_rel`]. The token crosses only the SSH session.
pub fn remote_daemon_endpoint(
    host: &crate::model::Host,
    subdir: &str,
) -> Option<crate::daemons::Endpoint> {
    let script = format!(
        "cat \"${{PANIOLO_RUNTIME_BASE:-/tmp}}/paniolo-$(id -u)/{subdir}/daemon.json\" 2>/dev/null"
    );
    let out = ssh::run(
        host,
        &["sh".to_string(), "-c".to_string(), script],
        None,
        &[],
    )
    .ok()?;
    if out.status != 0 || out.stdout.trim().is_empty() {
        return None;
    }
    crate::daemons::Endpoint::from_json(out.stdout.trim())
}

/// Where `paniolo helper` should run (#300): the lab host named by `--host`,
/// the host of `target`'s `channel` (`-t T --channel K`, with `interface`
/// picking a serial interface), or, with `-t` alone, the one host all of the
/// target's channels share. Neither flag means local, unchanged.
///
/// Unlike [`channel_host`](crate::model::channel_host), a channel the target
/// does not have is an error rather than a fall back to the default host:
/// the caller named that channel to say where the hardware is.
pub fn helper_host(
    lab: &Lab,
    host: Option<&str>,
    target: Option<&str>,
    channel: Option<&str>,
    interface: Option<&str>,
) -> Result<String, crate::error::PanioloError> {
    use crate::error::{channel_missing, Kind, PanioloError};
    if let Some(h) = host {
        if h == crate::model::LOCAL || lab.hosts.contains_key(h) {
            return Ok(h.to_string());
        }
        let declared: Vec<&str> = lab.hosts.keys().map(String::as_str).collect();
        return Err(PanioloError::not_configured(format!(
            "no host '{h}' in lab (declared: {})",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.join(", ")
            }
        ))
        .host(h));
    }
    let Some(target) = target else {
        return Ok(crate::model::LOCAL.to_string());
    };
    let rt = lab
        .resolved_target(target)
        .ok_or_else(|| crate::error::target_not_found(target))?;
    let Some(channel) = channel else {
        let hosts = rt.hosts();
        if hosts.len() == 1 {
            return Ok(hosts[0].clone());
        }
        return Err(PanioloError::new(
            Kind::Usage,
            format!(
                "target '{target}' has channels on several hosts ({}); \
                 pick one with --channel",
                hosts.join(", ")
            ),
        )
        .target(target));
    };
    let kind = ChannelKind::from_name(channel).ok_or_else(|| {
        let valid: Vec<&str> = ChannelKind::ALL.iter().map(|k| k.as_str()).collect();
        PanioloError::new(
            Kind::Usage,
            format!("unknown channel '{channel}' (valid: {})", valid.join(", ")),
        )
    })?;
    if !rt.channels.iter().any(|c| c.kind == kind) {
        return Err(channel_missing(
            target,
            channel,
            format!("target '{target}' has no {channel} channel"),
        ));
    }
    crate::model::channel_host(&rt, kind, interface)
}

/// Run `sub_argv` (a paniolo subcommand that needs no lab, such as `helper`)
/// on `host_name` with stdio passed through, and return its exit code. No lab
/// slice is shipped. ssh's own 255 becomes `unreachable`, as in [`dispatch`].
pub fn run_on_host(lab: &Lab, host_name: &str, sub_argv: &[String]) -> anyhow::Result<i32> {
    let host = lab.host(host_name);
    let mut argv = host.paniolo_argv();
    argv.extend(json_errors_arg(crate::error::json_requested()));
    argv.extend(sub_argv.iter().cloned());
    let code = ssh::run_passthrough(&host, &argv, &ssh::forwarded_env()?)?;
    if code == crate::error::SSH_TRANSPORT_FAILURE {
        return Err(crate::error::unreachable_host(
            host_name,
            crate::error::ssh_255_message(host_name),
        )
        .into());
    }
    Ok(code)
}

/// Resolve where a command should run and dispatch if remote.
///
/// Returns `Some(exit_code)` when the command was dispatched to a control host
/// (the caller should exit with it), or `None` when it should run locally.
pub fn maybe_dispatch(
    lab: &Lab,
    target: &str,
    kind: ChannelKind,
    serial_name: Option<&str>,
    mode: Mode,
) -> anyhow::Result<Option<i32>> {
    let rt = lab
        .resolved_target(target)
        .ok_or_else(|| crate::error::target_not_found(target))?;
    let host_name = crate::model::channel_host(&rt, kind, serial_name)?;
    let host = lab.host(&host_name);
    if host.is_local(&host_name) {
        return Ok(None);
    }
    let code = dispatch(lab, target, &host_name, mode, &subcommand_args())?;
    Ok(Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model;

    /// A zero exit persists the sink's bytes onto `out_path`, replacing
    /// whatever was already there. Proves the rename actually happens — not
    /// just that the function returns without error.
    #[test]
    fn capture_to_file_persists_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("shot.png");
        std::fs::write(&out, b"stale png").unwrap();

        let code = capture_to_file(out.to_str().unwrap(), |mut sink| {
            use std::io::Write;
            sink.write_all(b"fresh png bytes")?;
            Ok(0)
        })
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"fresh png bytes");
        // No leftover temp file beside it.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["shot.png"], "{names:?}");
    }

    /// A temp file orphaned by an interrupted capture (#255) is swept by the
    /// next capture into that directory once it is older than
    /// `STALE_CAPTURE_AGE`; a fresh one, which may belong to a capture still
    /// running, is left alone, as is an unrelated file.
    #[test]
    fn capture_to_file_sweeps_stale_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join(".paniolo-capture-old.tmp");
        let fresh = dir.path().join(".paniolo-capture-fresh.tmp");
        let other = dir.path().join("notes.tmp");
        for p in [&old, &fresh, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let backdated =
            std::time::SystemTime::now() - STALE_CAPTURE_AGE - std::time::Duration::from_secs(60);
        for p in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(backdated)
                .unwrap();
        }

        let out = dir.path().join("shot.png");
        let code = capture_to_file(out.to_str().unwrap(), |_| Ok(0)).unwrap();

        assert_eq!(code, 0);
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![".paniolo-capture-fresh.tmp", "notes.tmp", "shot.png"],
            "{names:?}"
        );
    }

    /// A special file (here `/dev/null`) is written in place: no temp file in
    /// its parent directory, which for `/dev` is not writable, and no rename.
    /// Regressed as `creating a temp file next to /dev/null: Permission denied`.
    #[cfg(unix)]
    #[test]
    fn capture_to_file_streams_into_a_special_file() {
        let code = capture_to_file("/dev/null", |mut sink| {
            use std::io::Write;
            sink.write_all(b"discarded png bytes")?;
            Ok(3)
        })
        .unwrap();

        assert_eq!(code, 3);
    }

    /// A non-zero exit must not touch `out_path` at all — the old code
    /// (`File::create(out_path)` before the transfer even started) truncated
    /// it regardless of the outcome, destroying whatever was there on a
    /// failed capture. This is the case that regresses without the
    /// sibling-temp-file rewrite (Review "adb screencap on a remote channel").
    #[test]
    fn capture_to_file_leaves_the_original_untouched_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("shot.png");
        std::fs::write(&out, b"the only good copy").unwrap();

        let code = capture_to_file(out.to_str().unwrap(), |mut sink| {
            use std::io::Write;
            // The remote wrote a partial payload before failing.
            sink.write_all(b"partial garbage")?;
            Ok(1)
        })
        .unwrap();

        assert_eq!(code, 1);
        assert_eq!(std::fs::read(&out).unwrap(), b"the only good copy");
        // The temp file was cleaned up too, not left beside it.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["shot.png"], "{names:?}");
    }

    /// `paniolo helper`'s host resolution (#300) against the fixture lab:
    /// fortune's video and hid live on bench2, everything else on bench1.
    #[test]
    fn helper_host_follows_the_named_channel() {
        let lab = lab();
        let at = |t, c, i| helper_host(&lab, None, Some(t), Some(c), i);
        assert_eq!(at("fortune", "hid", None).unwrap(), "bench2");
        assert_eq!(at("fortune", "video", None).unwrap(), "bench2");
        assert_eq!(at("fortune", "usb", None).unwrap(), "bench1");
        assert_eq!(at("fortune", "serial", Some("console")).unwrap(), "bench1");
        assert_eq!(at("fortune", "serial", None).unwrap(), "bench1");
    }

    /// A channel the target lacks is refused, not silently replaced by the
    /// default host: naming it was the caller's way of saying where to run.
    #[test]
    fn helper_host_refuses_a_missing_or_unknown_channel() {
        let lab = lab();
        let missing = helper_host(&lab, None, Some("fortune"), Some("power"), None).unwrap_err();
        assert_eq!(missing.kind, crate::error::Kind::NotConfigured);
        assert!(
            missing.to_string().contains("no power channel"),
            "{missing}"
        );
        let unknown = helper_host(&lab, None, Some("fortune"), Some("relay"), None).unwrap_err();
        assert_eq!(unknown.kind, crate::error::Kind::Usage);
        assert!(unknown.to_string().contains("valid: netboot"), "{unknown}");
        let iface =
            helper_host(&lab, None, Some("fortune"), Some("serial"), Some("dbg")).unwrap_err();
        assert!(
            iface.to_string().contains("no serial interface 'dbg'"),
            "{iface}"
        );
    }

    /// `-t` alone works only when every channel shares one host.
    #[test]
    fn helper_host_with_target_alone_needs_one_host() {
        let split = helper_host(&lab(), None, Some("fortune"), None, None).unwrap_err();
        assert_eq!(split.kind, crate::error::Kind::Usage);
        assert!(split.to_string().contains("bench1, bench2"), "{split}");
        let one = model::parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [targets.pi5]
            host = "bench1"
            [targets.pi5.power]
            cycle_cmd = "shellyplug -d 192.0.2.10 cycle"
            "#,
        )
        .unwrap();
        assert_eq!(
            helper_host(&one, None, Some("pi5"), None, None).unwrap(),
            "bench1"
        );
        assert!(helper_host(&one, None, Some("nope"), None, None).is_err());
    }

    /// `--host` takes a declared host or `local`; anything else is refused
    /// rather than quietly run here, which is what `Lab::host` would do.
    /// Neither flag is the old local behavior.
    #[test]
    fn helper_host_by_name() {
        let lab = lab();
        assert_eq!(
            helper_host(&lab, Some("bench2"), None, None, None).unwrap(),
            "bench2"
        );
        assert_eq!(
            helper_host(&lab, Some("local"), None, None, None).unwrap(),
            "local"
        );
        let e = helper_host(&lab, Some("ghost"), None, None, None).unwrap_err();
        assert_eq!(e.kind, crate::error::Kind::NotConfigured);
        assert!(e.to_string().contains("declared: bench1, bench2"), "{e}");
        assert_eq!(helper_host(&lab, None, None, None, None).unwrap(), "local");
    }

    /// Power levels (#302) ride in the slice with the power channel, to its
    /// host only, so `power-cycle --level` re-exec'd there finds them.
    #[test]
    fn slice_carries_power_levels_to_the_power_host() {
        let lab = model::parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [hosts.bench2]
            ssh = "u@bench2"
            [targets.pi5]
            host = "bench1"
            [targets.pi5.power]
            host = "bench2"
            cycle_cmd = "boardctl cycle"
            [targets.pi5.power.levels.hard]
            cycle_cmd = "shellyplug -d 192.0.2.10 cycle"
            state_cmd = "shellyplug -d 192.0.2.10 state"
            [targets.pi5.power.levels.pmic]
            off_cmd = "pmic off"
            "#,
        )
        .unwrap();
        let on2 = model::parse(&build_slice(&lab, "pi5", "bench2").unwrap()).unwrap();
        let p = on2.targets["pi5"].power.as_ref().unwrap();
        assert_eq!(p.host, None);
        assert_eq!(p.cycle_cmd.as_deref(), Some("boardctl cycle"));
        assert_eq!(
            p.levels,
            lab.targets["pi5"].power.as_ref().unwrap().levels,
            "every level crosses unchanged"
        );
        let on1 = model::parse(&build_slice(&lab, "pi5", "bench1").unwrap()).unwrap();
        assert!(on1.targets["pi5"].power.is_none());
    }

    /// The actions table rides in the slice for the host it is bound to, and
    /// only that one.
    #[test]
    fn slice_carries_actions_to_their_host() {
        let lab = model::parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [hosts.bench2]
            ssh = "u@bench2"
            [targets.pi5]
            host = "bench1"
            [targets.pi5.actions]
            host = "bench2"
            reset = { cmd = "boardctl reset", description = "Cold reset", requires_power = "on" }
            "#,
        )
        .unwrap();
        let on2 = model::parse(&build_slice(&lab, "pi5", "bench2").unwrap()).unwrap();
        let acts = on2.targets["pi5"].actions.as_ref().unwrap();
        assert_eq!(acts.host, None);
        let reset = &acts.actions["reset"];
        assert_eq!(reset.cmd, "boardctl reset");
        assert_eq!(reset.description.as_deref(), Some("Cold reset"));
        assert_eq!(reset.requires_power.as_deref(), Some("on"));
        let on1 = model::parse(&build_slice(&lab, "pi5", "bench1").unwrap()).unwrap();
        assert!(on1.targets["pi5"].actions.is_none());
        assert_eq!(
            helper_host(&lab, None, Some("pi5"), Some("actions"), None).unwrap(),
            "bench2"
        );
    }

    fn lab() -> Lab {
        model::parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [hosts.bench2]
            ssh = "u@bench2"
            [targets.fortune]
            host = "bench1"
            [targets.fortune.netboot]
            interface = "en0"
            [[targets.fortune.serial]]
            name = "console"
            device = "/dev/ttyUSB0"
            port = 8724
            [targets.fortune.video]
            device = "/dev/video0"
            ocr_mode = "gui"
            change_threshold = 4
            port = 8723
            host = "bench2"
            [targets.fortune.usb]
            cmd = "ch9329 -d /dev/ttyUSB1"
            [targets.fortune.hid]
            cmd = "ch9329 -d /dev/ttyUSB1"
            host = "bench2"
            port = 8725
            "#,
        )
        .unwrap()
    }

    #[test]
    fn slice_keeps_only_that_hosts_channels_host_stripped() {
        let s = build_slice(&lab(), "fortune", "bench1").unwrap();
        let reparsed = model::parse(&s).unwrap();
        let t = &reparsed.targets["fortune"];
        // bench1 has netboot + the console serial + usb (inherited default
        // host); video and hid (bench2) are excluded.
        assert!(t.netboot.is_some());
        assert_eq!(t.serial.len(), 1);
        assert!(t.video.is_none());
        assert!(t.hid.is_none());
        let usb = t.usb.as_ref().expect("usb channel shipped in the slice");
        assert_eq!(usb.cmd.as_deref(), Some("ch9329 -d /dev/ttyUSB1"));
        // Host fields are stripped so the remote resolves them as local.
        assert!(t.host.is_none());
        assert!(t.serial[0].host.is_none());
        assert!(usb.host.is_none());
    }

    #[test]
    fn slice_for_the_other_host_carries_only_its_channels() {
        let s = build_slice(&lab(), "fortune", "bench2").unwrap();
        let reparsed = model::parse(&s).unwrap();
        let t = &reparsed.targets["fortune"];
        assert!(t.video.is_some());
        assert!(t.hid.is_some());
        assert!(t.usb.is_none());
        assert!(t.netboot.is_none());
        assert!(t.serial.is_empty());
    }

    /// `video.ocr_mode` used to be dropped by `build_slice` (it called
    /// `set_video` with a hardcoded `None`), so a remote target's GUI OCR
    /// mode silently reverted to the default engine after re-exec (Review
    /// M14a).
    #[test]
    fn slice_carries_the_videos_ocr_mode() {
        let s = build_slice(&lab(), "fortune", "bench2").unwrap();
        let reparsed = model::parse(&s).unwrap();
        let v = reparsed.targets["fortune"].video.as_ref().unwrap();
        assert_eq!(v.device.as_deref(), Some("/dev/video0"));
        assert_eq!(v.ocr_mode.as_deref(), Some("gui"));
        assert!(v.host.is_none());
    }

    /// Same trap as `ocr_mode`: a field `build_slice` does not copy silently
    /// reverts to the default on the control host.
    #[test]
    fn slice_carries_the_videos_change_threshold() {
        let s = build_slice(&lab(), "fortune", "bench2").unwrap();
        let reparsed = model::parse(&s).unwrap();
        let v = reparsed.targets["fortune"].video.as_ref().unwrap();
        assert_eq!(v.change_threshold, Some(4));
    }

    /// Fixed daemon ports have to reach the control host, which is where the
    /// daemons start; a dropped one silently reverts to OS-assigned.
    #[test]
    fn slice_carries_fixed_daemon_ports() {
        let s = build_slice(&lab(), "fortune", "bench2").unwrap();
        let t = &model::parse(&s).unwrap().targets["fortune"];
        assert_eq!(t.video.as_ref().unwrap().port, Some(8723));
        assert_eq!(t.hid.as_ref().unwrap().port, Some(8725));
        let s = build_slice(&lab(), "fortune", "bench1").unwrap();
        assert_eq!(
            model::parse(&s).unwrap().targets["fortune"].serial_port(),
            8724
        );
    }

    /// The helper doctor opt-in (#304) on every channel that has one. A
    /// dropped flag would leave a doctor run *on* the control host — e.g.
    /// `paniolo doctor` dispatched there by a future verb — silently skipping
    /// the check the lab asked for.
    #[test]
    fn slice_carries_the_doctor_opt_in() {
        let lab = model::parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [targets.pi5]
            host = "bench1"
            [targets.pi5.power]
            cycle_cmd = "shellyplug -d 192.0.2.10 cycle"
            doctor = true
            [targets.pi5.power.levels.hard]
            cycle_cmd = "shellyplug -d 192.0.2.11 cycle"
            [targets.pi5.hid]
            cmd = "ch9329 -d /dev/ttyUSB1"
            doctor = true
            [targets.pi5.usb]
            cmd = "ch9329 -d /dev/ttyUSB1"
            doctor = true
            [targets.pi5.actions]
            doctor = true
            reset = { cmd = "boardctl reset" }
            [targets.pi6]
            host = "bench1"
            [targets.pi6.power]
            cycle_cmd = "shellyplug -d 192.0.2.11 cycle"
            "#,
        )
        .unwrap();
        let s = build_slice(&lab, "pi5", "bench1").unwrap();
        let t = &model::parse(&s).unwrap().targets["pi5"];
        let p = t.power.as_ref().unwrap();
        assert!(p.doctor, "{s}");
        // Levels (#302) and the flag travel together.
        assert_eq!(
            p.levels["hard"].cycle_cmd.as_deref(),
            Some("shellyplug -d 192.0.2.11 cycle"),
            "{s}"
        );
        assert!(t.hid.as_ref().unwrap().doctor, "{s}");
        assert!(t.usb.as_ref().unwrap().doctor, "{s}");
        let acts = t.actions.as_ref().unwrap();
        assert!(acts.doctor, "{s}");
        assert_eq!(acts.actions.keys().collect::<Vec<_>>(), ["reset"]);
        // Off stays off: nothing is written for the default.
        let s = build_slice(&lab, "pi6", "bench1").unwrap();
        assert!(!s.contains("doctor"), "{s}");
    }

    #[test]
    fn json_errors_crosses_the_hop_when_requested() {
        assert_eq!(json_errors_arg(true).as_deref(), Some("--json-errors"));
        assert_eq!(json_errors_arg(false), None);
    }

    /// A host's lab `plugin_path` rides on the re-exec as `--plugin-path=…`
    /// ahead of the subcommand, where the remote paniolo's real parser takes
    /// it; a host without one adds nothing.
    #[test]
    fn the_reexec_carries_the_hosts_plugin_path() {
        use clap::Parser;
        let mut host = model::Host {
            ssh: "u@bench1".to_string(),
            plugin_path: Some("/opt/lab-plugins:/srv/more".to_string()),
            ..Default::default()
        };
        let sub = vec!["helper".to_string()];
        let argv = remote_argv(&host, ".slice.toml", &sub);
        assert_eq!(argv[0], "paniolo");
        assert_eq!(argv[1], "--plugin-path=/opt/lab-plugins:/srv/more");
        assert_eq!(&argv[2..4], ["--lab", ".slice.toml"]);
        let cli = crate::Cli::try_parse_from(&argv).expect("the remote parser accepts it");
        assert_eq!(
            cli.plugin_path.as_deref(),
            Some("/opt/lab-plugins:/srv/more")
        );

        host.plugin_path = None;
        let argv = remote_argv(&host, ".slice.toml", &sub);
        assert!(
            !argv.iter().any(|a| a.starts_with("--plugin-path")),
            "{argv:?}"
        );
    }

    #[test]
    fn strip_lab_handles_both_forms() {
        let a: Vec<String> = ["--lab", "/x", "serial", "connect", "fortune"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(strip_lab_option(&a), vec!["serial", "connect", "fortune"]);
        let b: Vec<String> = ["--lab=/x", "netboot", "start"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(strip_lab_option(&b), vec!["netboot", "start"]);
    }
}
