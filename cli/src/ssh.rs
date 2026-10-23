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

//! SSH transport for driving a target on a remote control host.
//!
//! Ported from the Python `_ssh.py`. The dev machine is the hub; every command
//! against a remote target reaches its control host over SSH. There is no agent
//! or RPC server — `ssh` is the whole transport. A per-host **ControlMaster**
//! connection means only the first call to a host pays the handshake.
//!
//! A [`Host`] whose `ssh` destination is `"local"` is the dev machine itself;
//! the SSH functions must not be called for it (callers run those commands
//! directly).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::model::{expand_tilde, Host, LOCAL};

const CONTROL_PERSIST: &str = "300";
const CONNECT_TIMEOUT: &str = "10";
/// Probe an idle connection every this many seconds...
const SERVER_ALIVE_INTERVAL: &str = "5";
/// ...and give up after this many unanswered probes (~15 s in all).
const SERVER_ALIVE_COUNT_MAX: &str = "3";

/// Options that bound how long a dead link can hold a call: `ConnectTimeout`
/// for the handshake, and `ServerAlive*` once connected. Without the latter, a
/// connection that goes silently half-open (laptop sleep, a Wi-Fi change)
/// never errors — TCP has nothing to send, so a read just waits — and the
/// call hangs indefinitely instead of failing as an unreachable host. The
/// ControlMaster takes these from the invocation that starts it, so every
/// path that can start one must carry them.
fn liveness_args() -> Vec<String> {
    vec![
        "-o".into(),
        format!("ConnectTimeout={CONNECT_TIMEOUT}"),
        "-o".into(),
        format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL}"),
        "-o".into(),
        format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
    ]
}

fn uid() -> u32 {
    crate::platform::current_uid()
}

/// Short directory holding panio's default ControlMaster sockets. Kept short
/// because a Unix-domain socket path is length-limited and ssh appends a 40-char
/// `%C` hash; `$XDG_RUNTIME_DIR` is short on Linux, else the platform's
/// runtime root (`/tmp` on Unix).
///
/// Created and validated as a private directory (0700, owned by us) through
/// the same check as the daemon runtime base — on macOS it *is* the same
/// `/tmp/panio-<uid>` path, and a plain `create_dir_all` here once left
/// that base 0755 with every capture log beneath it world-readable. A path
/// that cannot be made private is an error, never a silent fallback: a
/// ControlMaster socket in a squatter's directory is an open SSH session in
/// their hands.
fn control_dir() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::platform::default_runtime_root);
    let d = base.join(format!("panio-{}", uid()));
    crate::platform::ensure_private_dir(&d).map_err(std::io::Error::other)?;
    Ok(d)
}

fn control_args(host: &Host) -> std::io::Result<Vec<String>> {
    let cp = match &host.control_path {
        Some(p) => expand_tilde(p).to_string_lossy().into_owned(),
        None => control_dir()?.join("cm-%C").to_string_lossy().into_owned(),
    };
    Ok(vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={cp}"),
        "-o".into(),
        format!("ControlPersist={CONTROL_PERSIST}"),
    ])
}

/// Base `ssh` argv (program + options) for a non-local host. `multiplex=false`
/// gives a standalone connection — a port forward must own its channel: an
/// `ssh -N -L` attached to a ControlMaster hands the forward to the master and
/// exits, so the process no longer represents (or can tear down) the tunnel.
/// Errors only when the default ControlMaster directory cannot be made
/// private (see [`control_dir`]).
fn base_args(host: &Host, interactive: bool, multiplex: bool) -> std::io::Result<Vec<String>> {
    debug_assert!(host.ssh != LOCAL, "ssh called for the local host");
    let mut a = vec!["ssh".to_string()];
    if !interactive {
        // Fail rather than block on a password prompt for non-interactive use.
        a.push("-o".into());
        a.push("BatchMode=yes".into());
    }
    a.extend(liveness_args());
    if let Some(id) = &host.identity {
        a.push("-i".into());
        a.push(expand_tilde(id).to_string_lossy().into_owned());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    if multiplex {
        a.extend(control_args(host)?);
    } else {
        a.extend(
            ["-o", "ControlMaster=no", "-o", "ControlPath=none"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    Ok(a)
}

/// Quote a single token for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Environment variables panio carries to a remote control host when it
/// dispatches a command there: the secrets a helper reads from its
/// environment rather than from the lab file (docs/power.md "Credentials").
/// This one constant is the whole policy — nothing else in the local
/// environment crosses, so a control host never sees a variable the lab did
/// not ask for. The values travel on the remote command's **stdin** (see
/// [`remote_command`]), never on its command line, where `ps` on the control
/// host would show them to every local user.
///
/// `PANIO_LEASE` is the caller's target-lease token. Interactive dispatch
/// forwards nothing, so there the token travels in a file instead (see
/// `dispatch::ship_token`).
pub const FORWARDED_ENV: &[&str] = &["AMT_PASSWORD", "PANIO_LEASE"];

/// The [`FORWARDED_ENV`] variables set in this process, in list order — the
/// `env` argument to [`run`], [`run_passthrough`] and [`run_stdout_to`].
/// Unset variables are simply absent. A value containing a newline is an
/// error: the remote reads one line per variable, so it could only arrive
/// truncated, with the remainder fed to the command as input.
pub fn forwarded_env() -> std::io::Result<Vec<(String, String)>> {
    collect_forwarded(FORWARDED_ENV, |name| std::env::var_os(name))
}

/// [`forwarded_env`] over an arbitrary name list and lookup, so the rules can
/// be tested without mutating this process's environment.
fn collect_forwarded(
    names: &[&str],
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> std::io::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for name in names {
        let Some(raw) = lookup(name) else {
            continue;
        };
        let invalid = |why: &str| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} {why}, so it cannot be forwarded to a control host"),
            )
        };
        let value = raw
            .into_string()
            .map_err(|_| invalid("is not valid UTF-8"))?;
        if value.contains('\n') {
            return Err(invalid("contains a newline"));
        }
        out.push((name.to_string(), value));
    }
    Ok(out)
}

/// True for a POSIX shell variable name (`[A-Za-z_][A-Za-z0-9_]*`) — the only
/// shape the prelude will splice into a `read`.
fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The remote command that reads `vars` from its stdin — one line each, in
/// the given order — exports them, and then execs `argv` with the rest of
/// stdin still attached:
///
/// ```text
/// sh -c 'IFS= read -r AMT_PASSWORD && export AMT_PASSWORD && exec "$@"' sh panio …
/// ```
///
/// `IFS=` and `-r` keep the value byte-for-byte (leading blanks, backslashes);
/// the `&&` chain means a pipe closed before every line arrived runs nothing.
/// The values themselves are not in this string — the caller writes them to
/// the child's stdin ([`launch`]).
pub fn stdin_prelude(argv: &[String], vars: &[&str]) -> String {
    let mut script = String::new();
    for v in vars {
        assert!(is_env_name(v), "not a shell variable name: {v:?}");
        script.push_str(&format!("IFS= read -r {v} && export {v} && "));
    }
    script.push_str("exec \"$@\"");
    let mut parts = vec![
        "sh".to_string(),
        "-c".to_string(),
        shell_quote(&script),
        "sh".to_string(),
    ];
    parts.extend(argv.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

/// Quote argv into one remote command, preserving argument boundaries through
/// the remote shell. With a non-empty `env` the command is wrapped in the
/// [`stdin_prelude`] for those variables' *names*; their values never appear
/// here (see [`FORWARDED_ENV`] for why), the caller writes them to stdin.
pub fn remote_command(argv: &[String], env: &[(String, String)]) -> String {
    if env.is_empty() {
        return argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
    }
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    stdin_prelude(argv, &names)
}

/// What the remote command's stdin carries once the forwarded values (if
/// any) have been written.
enum StdinSource<'a> {
    /// Nothing more; a captured command that takes no input.
    Null,
    /// A fixed payload, then EOF.
    Data(&'a str),
    /// This process's own stdin, copied through until it reaches EOF — a
    /// passthrough command (`serial send` reading a heredoc, say) keeps
    /// working with the prelude in front of it.
    Inherit,
}

/// Spawn `cmd` — an `ssh … -- <destination>` argv, or a local `sh -c` in the
/// tests — with the quoted remote command appended as its last argument,
/// `env` delivered through the child's stdin, and `stdin` following it.
///
/// Every non-interactive run function comes through here, so the one code
/// path that writes secrets is exercised end to end by the tests against a
/// shell instead of an ssh session. With no `env` and no payload the child's
/// stdin is inherited or null exactly as before; only a forwarded value or a
/// payload makes it a pipe. A pipe the child closed early (an ssh that could
/// not connect) is not an error here — the exit status the caller collects
/// says what happened.
fn launch(
    mut cmd: Command,
    argv: &[String],
    env: &[(String, String)],
    stdin: StdinSource<'_>,
) -> std::io::Result<std::process::Child> {
    cmd.arg(remote_command(argv, env));
    let piped = !env.is_empty() || matches!(stdin, StdinSource::Data(_));
    cmd.stdin(match (piped, &stdin) {
        (true, _) => Stdio::piped(),
        (false, StdinSource::Inherit) => Stdio::inherit(),
        (false, _) => Stdio::null(),
    });
    let mut child = cmd.spawn()?;
    if let Some(mut pipe) = child.stdin.take() {
        let tolerate_closed = |r: std::io::Result<()>| match r {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other,
        };
        for (_, value) in env {
            tolerate_closed(pipe.write_all(value.as_bytes()))?;
            tolerate_closed(pipe.write_all(b"\n"))?;
        }
        match stdin {
            StdinSource::Null => {}
            StdinSource::Data(d) => tolerate_closed(pipe.write_all(d.as_bytes()))?,
            StdinSource::Inherit => {
                // Copy until local EOF; the pipe closes when the thread drops
                // it, and a write into a child that has already exited
                // returns EPIPE, which ends the copy.
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut pipe);
                });
            }
        }
    }
    Ok(child)
}

/// SSH options shared with [`base_args`], for `sftp` rather than `ssh`.
///
/// Same identity, timeout and multiplexing — an sftp that reuses the session's
/// ControlMaster costs no extra handshake.
fn transfer_args(host: &Host) -> std::io::Result<Vec<String>> {
    let mut a = vec!["-o".to_string(), "BatchMode=yes".to_string()];
    a.extend(liveness_args());
    if let Some(id) = &host.identity {
        a.push("-i".into());
        a.push(expand_tilde(id).to_string_lossy().into_owned());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    a.extend(control_args(host)?);
    Ok(a)
}

/// Quote a path for an sftp batch line.
///
/// sftp splits its command lines on whitespace and honours double quotes. It is
/// not a shell, so only the quote and the escape character need care — and
/// notably a Windows path's backslashes must survive, which is why this is not
/// [`shell_quote`].
fn sftp_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn sftp_batch(host: &Host, script: &str) -> std::io::Result<()> {
    let mut cmd = Command::new("sftp");
    cmd.args(transfer_args(host)?);
    cmd.arg("-b").arg("-").arg("--").arg(&host.ssh);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "sftp to {} failed: {}",
            host.ssh,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Upload `local` to `remote_rel` on `host` over SFTP.
///
/// SFTP rather than a shell command on purpose. The remote's login shell is not
/// ours to choose: a Windows control host answers with PowerShell, where
/// `f=$(mktemp …) && cat > "$f"` is not a command at all. SFTP is a protocol, so
/// it behaves identically whatever shell the far side runs.
///
/// `remote_rel` is deliberately **relative**. SFTP reports a Windows home as
/// `/C:/Users/name` — an SFTP-protocol path no native Windows program can open —
/// so an absolute path taken from `pwd` would be unusable as a `--lab` argument.
/// Both platforms start an SSH session in the user's home, which is also SFTP's
/// default directory, so a relative name resolves consistently on both.
pub fn sftp_put(host: &Host, local: &std::path::Path, remote_rel: &str) -> std::io::Result<()> {
    sftp_batch(
        host,
        &format!(
            "put {} {}\n",
            sftp_quote(&local.to_string_lossy()),
            sftp_quote(remote_rel)
        ),
    )
}

/// Remove `remote_rel` on `host` over SFTP.
///
/// Also not a shell command: `rm -f` on a PowerShell host fails with "parameter
/// 'f' is ambiguous. Possible matches include: -Filter -Force."
pub fn sftp_rm(host: &Host, remote_rel: &str) -> std::io::Result<()> {
    sftp_batch(host, &format!("rm {}\n", sftp_quote(remote_rel)))
}

/// Result of a captured remote command.
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The `ssh` command for `host` up to and including the destination, ready
/// for the remote command to be appended. `--` ends option parsing so a
/// destination beginning with `-` can never be read as an ssh flag.
fn ssh_command(host: &Host, interactive: bool) -> std::io::Result<Command> {
    let mut cmd = Command::new("ssh");
    cmd.args(&base_args(host, interactive, true)?[1..]);
    if interactive {
        cmd.arg("-t");
    }
    cmd.arg("--").arg(&host.ssh);
    Ok(cmd)
}

/// Run `argv` on `host` and capture its output (never errors on non-zero
/// exit). `env` is forwarded over the remote's stdin ahead of `stdin`.
pub fn run(
    host: &Host,
    argv: &[String],
    stdin: Option<&str>,
    env: &[(String, String)],
) -> std::io::Result<Output> {
    let mut cmd = ssh_command(host, false)?;
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let source = match stdin {
        Some(data) => StdinSource::Data(data),
        None => StdinSource::Null,
    };
    let out = launch(cmd, argv, env, source)?.wait_with_output()?;
    Ok(Output {
        status: crate::error::shell_code(out.status),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run `argv` on `host` with the local terminal's stdio passed through (no
/// PTY). This is the transparent re-exec path. Returns the exit code. `env`
/// is forwarded over the remote's stdin; the local stdin is copied through
/// behind it.
pub fn run_passthrough(
    host: &Host,
    argv: &[String],
    env: &[(String, String)],
) -> std::io::Result<i32> {
    let cmd = ssh_command(host, false)?;
    let status = launch(cmd, argv, env, StdinSource::Inherit)?.wait()?;
    Ok(crate::error::shell_code(status))
}

/// Run `argv` on `host` with stdout redirected into `sink` (stderr and stdin
/// pass through, `env` forwarded as in [`run_passthrough`]). For remote
/// commands that stream a binary payload on stdout (e.g. `video shot --out
/// -`), where capturing into a lossy String would corrupt it.
pub fn run_stdout_to(
    host: &Host,
    argv: &[String],
    env: &[(String, String)],
    sink: std::fs::File,
) -> std::io::Result<i32> {
    let mut cmd = ssh_command(host, false)?;
    cmd.stdout(Stdio::from(sink));
    let status = launch(cmd, argv, env, StdinSource::Inherit)?.wait()?;
    Ok(crate::error::shell_code(status))
}

/// Run `argv` on `host` over an `ssh -t` PTY (for interactive tools like tio).
///
/// Nothing is forwarded from the environment here, by construction: the
/// remote's stdin *is* the terminal the user is typing into, so there is no
/// channel to put a secret on ahead of the command that the user's
/// keystrokes would not also share. The interactive commands (`serial
/// connect`, `adb shell`, `setup --host`) need none of the [`FORWARDED_ENV`]
/// credentials; the one variable they do need, `PANIO_LEASE`, goes in a
/// token file named by `--lease-file` instead.
pub fn run_interactive(host: &Host, argv: &[String]) -> std::io::Result<i32> {
    let status = ssh_command(host, true)?
        .arg(remote_command(argv, &[]))
        .status()?;
    Ok(crate::error::shell_code(status))
}

/// A held `ssh -L` tunnel to a port on `host`; killed on drop.
pub struct Forward {
    pub local_port: u16,
    child: std::process::Child,
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_local_port() -> std::io::Result<u16> {
    // Small TOCTOU window is acceptable.
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

/// Open an `ssh -L` tunnel to `127.0.0.1:remote_port` on `host`, returning once
/// the local end accepts connections. The forwarder is standalone (not
/// multiplexed) so killing it reliably tears the tunnel down.
pub fn forward(host: &Host, remote_port: u16) -> anyhow::Result<Forward> {
    use anyhow::bail;
    let local_port = free_local_port()?;
    let spec = format!("{local_port}:127.0.0.1:{remote_port}");
    let mut child = Command::new("ssh")
        .args(&base_args(host, false, false)?[1..])
        .arg("-N")
        .arg("-L")
        .arg(&spec)
        .arg("--")
        .arg(&host.ssh)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "ssh forward to {}:{remote_port} exited early ({status})",
                host.ssh
            );
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], local_port));
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
            .is_ok()
        {
            return Ok(Forward { local_port, child });
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            return Err(crate::error::PanioError::new(
                crate::error::Kind::Timeout,
                format!("timed out waiting for forwarded port {local_port}"),
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_safe_tokens_bare() {
        assert_eq!(shell_quote("/dev/ttyUSB0"), "/dev/ttyUSB0");
        assert_eq!(shell_quote("user@host"), "user@host");
    }

    #[test]
    fn shell_quote_wraps_specials() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn remote_command_without_env_is_the_quoted_argv() {
        assert_eq!(
            remote_command(&argv(&["panio", "serial", "send", "a b"]), &[]),
            "panio serial send 'a b'"
        );
    }

    /// A forwarded variable's *name* goes into the prelude; its value must
    /// not be anywhere in the command line, which `ps` on the control host
    /// shows to every local user. That was the whole reason the old
    /// `KEY=val` prefix could not be used for secrets (Review M14).
    #[test]
    fn remote_command_with_env_wraps_in_the_stdin_prelude_without_the_value() {
        let env = vec![("AMT_PASSWORD".to_string(), "hunter2".to_string())];
        let cmd = remote_command(&argv(&["panio", "power-state", "nuc"]), &env);
        assert_eq!(
            cmd,
            "sh -c 'IFS= read -r AMT_PASSWORD && export AMT_PASSWORD && exec \"$@\"' \
             sh panio power-state nuc"
        );
        assert!(!cmd.contains("hunter2"), "{cmd}");
    }

    /// Several variables are read in list order, one `read` each, so the
    /// writer and the remote agree on which line is which.
    #[test]
    fn stdin_prelude_reads_variables_in_order() {
        let p = stdin_prelude(&argv(&["x"]), &["FIRST", "SECOND"]);
        assert_eq!(
            p,
            "sh -c 'IFS= read -r FIRST && export FIRST && IFS= read -r SECOND && \
             export SECOND && exec \"$@\"' sh x"
        );
    }

    #[test]
    #[should_panic(expected = "not a shell variable name")]
    fn stdin_prelude_refuses_a_name_that_is_not_a_variable() {
        stdin_prelude(&argv(&["x"]), &["A-B"]);
    }

    #[test]
    fn forwarded_env_includes_panio_lease() {
        let lookup = |n: &str| (n == "PANIO_LEASE").then(|| std::ffi::OsString::from("tok"));
        let got = collect_forwarded(FORWARDED_ENV, lookup).unwrap();
        assert_eq!(got, vec![("PANIO_LEASE".to_string(), "tok".to_string())]);
    }

    /// Only the listed variables cross, in list order; an unset one is
    /// simply absent, and a value the remote's line-oriented `read` could
    /// not take back intact is refused rather than truncated.
    #[test]
    fn collect_forwarded_takes_set_names_in_order_and_rejects_newlines() {
        let lookup = |name: &str| -> Option<std::ffi::OsString> {
            match name {
                "B" => Some("second".into()),
                "A" => Some("first".into()),
                "NL" => Some("two\nlines".into()),
                _ => None,
            }
        };
        let got = collect_forwarded(&["A", "UNSET", "B"], lookup).unwrap();
        assert_eq!(
            got,
            vec![
                ("A".to_string(), "first".to_string()),
                ("B".to_string(), "second".to_string())
            ]
        );
        let err = collect_forwarded(&["NL"], lookup).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("NL contains a newline"), "{err}");
        assert!(collect_forwarded(&["UNSET"], lookup).unwrap().is_empty());
    }

    /// The transport path itself, run through a local `sh` in place of
    /// `ssh`: the child sees each forwarded variable, in order, and the
    /// payload that follows arrives on its stdin untouched. The same
    /// [`launch`] backs `run`, `run_passthrough` and `run_stdout_to`.
    #[cfg(unix)]
    #[test]
    fn launch_delivers_env_over_stdin_ahead_of_the_payload() {
        let mut sh = Command::new("sh");
        sh.arg("-c");
        sh.stdout(Stdio::piped()).stderr(Stdio::piped());
        let inner = "printf '%s|%s|' \"$PANIO_TEST_A\" \"$PANIO_TEST_B\"; cat";
        let remote = argv(&["sh", "-c", inner]);
        let env = vec![
            ("PANIO_TEST_A".to_string(), "  first secret".to_string()),
            ("PANIO_TEST_B".to_string(), r"it's \2#".to_string()),
        ];
        let out = launch(sh, &remote, &env, StdinSource::Data("the rest\n"))
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "  first secret|it's \\2#|the rest\n"
        );
    }

    /// With nothing to forward the child gets exactly the payload (the
    /// pre-prelude `run` contract), and no prelude wraps the command.
    #[cfg(unix)]
    #[test]
    fn launch_without_env_passes_the_payload_straight_through() {
        let mut sh = Command::new("sh");
        sh.arg("-c");
        sh.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = launch(sh, &argv(&["cat"]), &[], StdinSource::Data("plain"))
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "plain");
    }

    /// A child that exits without draining its stdin (an ssh that failed to
    /// connect, here a command that never reads) must surface as its exit
    /// status, not as a broken-pipe error from the writer. The payload is
    /// larger than a pipe buffer so the write cannot complete before the
    /// child is gone.
    #[cfg(unix)]
    #[test]
    fn launch_reports_an_early_exit_as_status_not_epipe() {
        let mut sh = Command::new("sh");
        sh.arg("-c");
        sh.stdout(Stdio::null()).stderr(Stdio::null());
        let payload = "x".repeat(1 << 20);
        let out = launch(sh, &argv(&["exit", "7"]), &[], StdinSource::Data(&payload))
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7));
    }

    /// Every ssh/sftp invocation puts `--` between the options and the
    /// destination, so a destination that starts with `-` reaches ssh as a
    /// host, not as a flag it would act on.
    #[test]
    fn ssh_command_terminates_options_before_the_destination() {
        let host = Host {
            ssh: "-oProxyCommand=evil".into(),
            ..Default::default()
        };
        let cmd = ssh_command(&host, false).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let dashdash = args.iter().position(|a| a == "--").expect("a `--`");
        assert_eq!(args[dashdash + 1], "-oProxyCommand=evil");
        assert_eq!(args.len(), dashdash + 2, "destination is last: {args:?}");
        let interactive = ssh_command(&host, true).unwrap();
        let args: Vec<String> = interactive
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let t = args.iter().position(|a| a == "-t").expect("-t");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        assert!(t < dashdash, "{args:?}");
    }

    #[test]
    fn base_args_include_batch_and_control_master() {
        let host = Host {
            ssh: "u@bench1".into(),
            identity: Some("~/.ssh/id".into()),
            ..Default::default()
        };
        let a = base_args(&host, false, true).unwrap().join(" ");
        assert!(a.contains("BatchMode=yes"), "{a}");
        assert!(a.contains("ControlMaster=auto"), "{a}");
        assert!(a.contains("IdentitiesOnly=yes"), "{a}");
        assert!(a.contains("ServerAliveInterval=5"), "{a}");
        assert!(a.contains("ServerAliveCountMax=3"), "{a}");
        // A standalone connection (a port forward) needs them as much as a
        // multiplexed one: a tunnel over a dead link should close, not hang.
        assert!(base_args(&host, false, false)
            .unwrap()
            .join(" ")
            .contains("ServerAliveInterval=5"));
        // Interactive variant drops BatchMode (so a PTY/password can work).
        assert!(!base_args(&host, true, true)
            .unwrap()
            .join(" ")
            .contains("BatchMode"));
    }

    /// The default ControlMaster directory must come back private. On macOS
    /// it is the same path as the daemon runtime base, and the plain
    /// `create_dir_all` this replaced is what once left that base 0755 with
    /// every capture log beneath it world-readable.
    #[test]
    fn control_dir_is_private() {
        let d = control_dir().expect("control dir");
        assert!(
            crate::platform::is_private_dir(&d),
            "{} must be a private directory owned by us",
            d.display()
        );
    }

    /// A Windows path is mostly backslashes, and the sftp batch parser treats a
    /// backslash as an escape — so the quoting that works for a remote shell
    /// (`shell_quote`) is the wrong tool here. Getting this wrong silently
    /// uploads to a mangled filename.
    #[test]
    fn sftp_quote_preserves_windows_paths() {
        assert_eq!(
            sftp_quote(r"C:\Users\curti\.panio-lab-1.toml"),
            r#""C:\\Users\\curti\\.panio-lab-1.toml""#
        );
    }

    #[test]
    fn sftp_quote_escapes_quotes_and_spaces() {
        assert_eq!(sftp_quote("a b.toml"), r#""a b.toml""#);
        assert_eq!(sftp_quote(r#"od"d.toml"#), r#""od\"d.toml""#);
    }

    /// sftp must not inherit `ssh`'s argv shape — it takes no `-o BatchMode`
    /// positionally out of order, and it must keep the multiplexing options so
    /// a dispatch reuses the session's existing master rather than
    /// re-authenticating per file transfer.
    #[test]
    fn transfer_args_carry_identity_and_multiplexing() {
        let host = Host {
            ssh: "u@bench1".into(),
            identity: Some("~/.ssh/id".into()),
            ..Default::default()
        };
        let a = transfer_args(&host).unwrap().join(" ");
        assert!(a.contains("BatchMode=yes"), "{a}");
        assert!(a.contains("IdentitiesOnly=yes"), "{a}");
        assert!(a.contains("ControlMaster=auto"), "{a}");
        // An sftp can be what starts the ControlMaster (dispatch ships the
        // lab slice first), and the master keeps the options it started with.
        assert!(a.contains("ServerAliveInterval=5"), "{a}");
        assert!(a.contains("ServerAliveCountMax=3"), "{a}");
    }
}
