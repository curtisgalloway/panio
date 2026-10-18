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

//! Where a helper's secret comes from: an environment variable, a file, or a
//! command — the restic/borg pattern (issue #249).
//!
//! A helper that needs a secret `X` accepts it from any one of three sources,
//! so any secret manager plugs in without a wrapper script:
//!
//! | Source            | Covers                                                  |
//! |-------------------|---------------------------------------------------------|
//! | env `X`           | CI, `op run`, a one-off export                          |
//! | `--x-file <path>` | systemd `LoadCredential`, Docker/Kubernetes secret mounts, a 0600 file |
//! | `--x-command <cmd>` | `op read …`, `pass show …`, macOS Keychain, sops      |
//!
//! When more than one is set, the first in that order wins (env, then file,
//! then command), as restic does. The secret itself never appears in a flag:
//! a path or a command does not reveal it, the same way ssh's `IdentityFile`
//! does not.
//!
//! Every failure is a [`NotConfigured`] error. Retrying will not fix a missing
//! file or a failing command, so a helper maps it to exit status 3
//! (`not_configured`) rather than 1; see [`is_not_configured`].
//!
//! This is a library crate shared by path dependency (`secret = { path =
//! "../secret" }`), not a byte-identical copy like `platform.rs`: it has no
//! platform glue a crate would want to trim, and one copy cannot drift.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long a `--…-command` may run before it is killed and treated as a
/// failure. Long enough for a cold `op read` or a Vault agent round trip;
/// short enough that a power hook stuck on a prompt nobody will answer fails
/// instead of hanging the runner.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling interval while waiting for a secret command to exit.
const POLL: Duration = Duration::from_millis(20);

/// The names one secret goes by: its environment variable and its two flags.
/// Used only to write error messages that name every source.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// What the secret is, for messages: `"AMT password"`.
    pub what: &'static str,
    /// The environment variable: `"AMT_PASSWORD"`.
    pub env: &'static str,
    /// The file flag, with dashes: `"--password-file"`.
    pub file_flag: &'static str,
    /// The command flag, with dashes: `"--password-command"`.
    pub command_flag: &'static str,
}

impl Spec {
    /// One line naming all three sources, for a "not set" message.
    pub fn sources_hint(&self) -> String {
        format!(
            "set {} in the environment, or pass {} <path> or {} <command>",
            self.env, self.file_flag, self.command_flag
        )
    }
}

/// The file and command sources a caller parsed from its flags.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    pub file: Option<PathBuf>,
    pub command: Option<String>,
}

/// Which source a [`Secret`] came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Env(&'static str),
    File(PathBuf),
    Command(&'static str),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Env(name) => write!(f, "{name}"),
            Origin::File(path) => write!(f, "{}", path.display()),
            Origin::Command(flag) => write!(f, "{flag}"),
        }
    }
}

/// A secret value and where it came from. `Debug` never shows the value.
#[derive(Clone)]
pub struct Secret {
    value: String,
    origin: Origin,
}

impl Secret {
    pub fn expose(&self) -> &str {
        &self.value
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("value", &"<redacted>")
            .field("origin", &self.origin)
            .finish()
    }
}

/// A secret that could not be obtained. The message never contains the
/// secret, nor anything a secret command printed on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotConfigured(pub String);

impl fmt::Display for NotConfigured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotConfigured {}

/// Whether `err`, or anything in its source chain, is a [`NotConfigured`].
/// A helper's `main` uses this to pick exit status 3 over 1.
pub fn is_not_configured(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<NotConfigured>() {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Resolve an optional secret: `Ok(None)` when no source is set.
pub fn resolve(spec: &Spec, sources: &Sources) -> Result<Option<Secret>, NotConfigured> {
    resolve_with(spec, std::env::var(spec.env).ok(), sources, COMMAND_TIMEOUT)
}

/// Resolve a required secret: no source set is an error naming all three.
pub fn require(spec: &Spec, sources: &Sources) -> Result<Secret, NotConfigured> {
    resolve(spec, sources)?.ok_or_else(|| not_set(spec))
}

/// The error for a required secret with no source set.
pub fn not_set(spec: &Spec) -> NotConfigured {
    NotConfigured(format!(
        "the {} is not set — {}. It is never taken from a flag's value or a \
         config file, so it cannot leak into a lab file, shell history, or `ps`",
        spec.what,
        spec.sources_hint()
    ))
}

/// [`resolve`] with the environment value and the command timeout injected,
/// so tests need not touch the process environment or wait 30 s.
///
/// An empty environment variable counts as unset, since an empty secret is
/// never what anyone meant.
pub fn resolve_with(
    spec: &Spec,
    env_value: Option<String>,
    sources: &Sources,
    timeout: Duration,
) -> Result<Option<Secret>, NotConfigured> {
    if let Some(value) = env_value.filter(|v| !v.is_empty()) {
        return Ok(Some(Secret {
            value,
            origin: Origin::Env(spec.env),
        }));
    }
    if let Some(path) = &sources.file {
        return read_file(spec, path).map(Some);
    }
    if let Some(cmd) = &sources.command {
        return run_command(spec, cmd, timeout).map(Some);
    }
    Ok(None)
}

/// Drop one trailing line ending (`\n` or `\r\n`), as restic does. Anything
/// else — leading or inner whitespace, a second newline — is kept, because it
/// may be part of the secret.
fn trim_one_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

fn read_file(spec: &Spec, path: &Path) -> Result<Secret, NotConfigured> {
    let bytes = std::fs::read(path).map_err(|e| {
        NotConfigured(format!(
            "{}: cannot read {} {}: {e}",
            spec.file_flag,
            spec.what,
            path.display()
        ))
    })?;
    warn_if_exposed(spec, path);
    let text = String::from_utf8(bytes).map_err(|_| {
        NotConfigured(format!(
            "{}: {} is not valid UTF-8",
            spec.file_flag,
            path.display()
        ))
    })?;
    let value = trim_one_newline(text);
    if value.is_empty() {
        return Err(NotConfigured(format!(
            "{}: {} is empty",
            spec.file_flag,
            path.display()
        )));
    }
    Ok(Secret {
        value,
        origin: Origin::File(path.to_path_buf()),
    })
}

/// Warn, without refusing, when a secret file is readable by group or others.
/// ssh refuses such a key; this does not, because container secret mounts
/// (Docker, Kubernetes) are commonly 0444 and cannot be changed from inside.
#[cfg(unix)]
fn warn_if_exposed(spec: &Spec, path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o044 != 0 {
            eprintln!(
                "warning: {} file {} is readable by group or others (mode {mode:03o}); \
                 consider chmod 600",
                spec.what,
                path.display()
            );
        }
    }
}

#[cfg(windows)]
fn warn_if_exposed(_spec: &Spec, _path: &Path) {}

fn run_command(spec: &Spec, cmd: &str, timeout: Duration) -> Result<Secret, NotConfigured> {
    let fail = |why: String| NotConfigured(format!("{} `{cmd}` {why}", spec.command_flag));
    // No stdin: a command that prompts must fail, not hang the hook. Its stderr
    // passes through so its own diagnostics reach the user; its stdout is the
    // secret and is never logged.
    let mut child = shell_command(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| fail(format!("could not be started: {e}")))?;

    // Drain stdout on a thread so a command that prints more than a pipe
    // buffer cannot block on write while we wait for it to exit.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                // The reader is not joined: a grandchild the shell started
                // may still hold the pipe open.
                return Err(fail(format!(
                    "did not finish within {} s and was killed",
                    timeout.as_secs_f32()
                )));
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => return Err(fail(format!("could not be waited for: {e}"))),
        }
    };
    if !status.success() {
        return Err(fail(match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "was killed by a signal".to_string(),
        }));
    }
    let bytes = reader
        .join()
        .map_err(|_| fail("output could not be read".to_string()))?
        .map_err(|e| fail(format!("output could not be read: {e}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| fail("printed something that is not valid UTF-8".to_string()))?;
    let value = trim_one_newline(text);
    if value.is_empty() {
        return Err(fail(format!("printed no {}", spec.what)));
    }
    Ok(Secret {
        value,
        origin: Origin::Command(spec.command_flag),
    })
}

/// A shell invocation for the command string, the same way panio runs a
/// lab file's hooks (`cli/src/platform.rs`, `shell_command`): `sh -c` on
/// Unix, `cmd /C` on Windows with the string passed verbatim.
#[cfg(unix)]
fn shell_command(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    c
}

#[cfg(windows)]
fn shell_command(script: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("cmd");
    c.arg("/C");
    // cmd.exe does not follow the argument-quoting rules std applies, so the
    // script is handed over verbatim; std's escaping would mangle any quoting
    // the user wrote.
    c.raw_arg(script);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    // A made-up value; no test prints it, and failures assert it is absent.
    const FAKE: &str = "not-a-real-secret";

    const SPEC: Spec = Spec {
        what: "test password",
        env: "SECRET_TEST_PASSWORD",
        file_flag: "--password-file",
        command_flag: "--password-command",
    };

    const SHORT: Duration = Duration::from_millis(500);

    fn tmp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("secret-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn file(path: PathBuf) -> Sources {
        Sources {
            file: Some(path),
            command: None,
        }
    }

    fn command(cmd: &str) -> Sources {
        Sources {
            file: None,
            command: Some(cmd.to_string()),
        }
    }

    /// A command that prints FAKE followed by the platform's line ending.
    fn print_fake() -> String {
        if cfg!(windows) {
            format!("echo {FAKE}")
        } else {
            format!("printf '%s\\n' {FAKE}")
        }
    }

    fn err_of(r: Result<Option<Secret>, NotConfigured>) -> String {
        let e = r.expect_err("expected an error").0;
        assert!(!e.contains(FAKE), "error leaks the secret: {e}");
        e
    }

    #[test]
    fn env_alone() {
        let s = resolve_with(&SPEC, Some(FAKE.into()), &Sources::default(), SHORT)
            .unwrap()
            .unwrap();
        assert_eq!(s.expose(), FAKE);
        assert_eq!(s.origin(), &Origin::Env("SECRET_TEST_PASSWORD"));
    }

    #[test]
    fn empty_env_counts_as_unset() {
        let r = resolve_with(&SPEC, Some(String::new()), &Sources::default(), SHORT);
        assert!(r.unwrap().is_none());
    }

    #[test]
    fn nothing_set_is_none_and_require_names_all_three() {
        assert!(resolve_with(&SPEC, None, &Sources::default(), SHORT)
            .unwrap()
            .is_none());
        let msg = not_set(&SPEC).0;
        for name in [
            "SECRET_TEST_PASSWORD",
            "--password-file",
            "--password-command",
        ] {
            assert!(msg.contains(name), "{msg}");
        }
    }

    #[test]
    fn file_alone_trims_exactly_one_newline() {
        let p = tmp("one-newline", &format!("{FAKE}\n"));
        let s = resolve_with(&SPEC, None, &file(p.clone()), SHORT)
            .unwrap()
            .unwrap();
        assert_eq!(s.expose(), FAKE);
        assert_eq!(s.origin(), &Origin::File(p));

        let p = tmp("crlf", &format!("{FAKE}\r\n"));
        let s = resolve_with(&SPEC, None, &file(p), SHORT).unwrap().unwrap();
        assert_eq!(s.expose(), FAKE);

        // Only one: a second newline, or surrounding spaces, may be part of it.
        let p = tmp("two-newlines", &format!(" {FAKE}\n\n"));
        let s = resolve_with(&SPEC, None, &file(p), SHORT).unwrap().unwrap();
        assert_eq!(s.expose(), format!(" {FAKE}\n"));

        let p = tmp("no-newline", FAKE);
        let s = resolve_with(&SPEC, None, &file(p), SHORT).unwrap().unwrap();
        assert_eq!(s.expose(), FAKE);
    }

    #[test]
    fn missing_file_is_not_configured() {
        let p = std::env::temp_dir().join("secret-test-does-not-exist");
        let e = err_of(resolve_with(&SPEC, None, &file(p), SHORT));
        assert!(e.contains("--password-file"), "{e}");
        assert!(e.contains("cannot read"), "{e}");
    }

    #[test]
    fn empty_file_is_an_error() {
        let p = tmp("empty", "\n");
        let e = err_of(resolve_with(&SPEC, None, &file(p), SHORT));
        assert!(e.contains("is empty"), "{e}");
    }

    #[test]
    fn command_alone() {
        let s = resolve_with(&SPEC, None, &command(&print_fake()), SHORT * 10)
            .unwrap()
            .unwrap();
        assert_eq!(s.expose(), FAKE);
        assert_eq!(s.origin(), &Origin::Command("--password-command"));
    }

    #[test]
    fn failing_command_is_an_error_and_hides_its_stdout() {
        // The command prints `stdoutmarker`, a string its own text does not
        // contain (the error quotes the command, which is not secret).
        let cmd = if cfg!(windows) {
            "echo stdout^marker& exit 3"
        } else {
            "printf '%s%s' stdout marker; exit 3"
        };
        let e = err_of(resolve_with(&SPEC, None, &command(cmd), SHORT * 10));
        assert!(e.contains("exited with status 3"), "{e}");
        assert!(!e.contains("stdoutmarker"), "{e}");
    }

    #[test]
    fn command_that_prints_nothing_is_an_error() {
        let cmd = if cfg!(windows) { "rem" } else { "true" };
        let e = err_of(resolve_with(&SPEC, None, &command(cmd), SHORT * 10));
        assert!(e.contains("printed no test password"), "{e}");
    }

    #[test]
    fn slow_command_is_killed_at_the_timeout() {
        let cmd = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let start = Instant::now();
        let e = err_of(resolve_with(&SPEC, None, &command(cmd), SHORT));
        assert!(e.contains("did not finish"), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn precedence_is_env_then_file_then_command() {
        let p = tmp("precedence", "from-file\n");
        let both = Sources {
            file: Some(p),
            command: Some("echo from-command".into()),
        };
        let s = resolve_with(&SPEC, Some("from-env".into()), &both, SHORT * 10)
            .unwrap()
            .unwrap();
        assert_eq!(s.expose(), "from-env");
        let s = resolve_with(&SPEC, None, &both, SHORT * 10)
            .unwrap()
            .unwrap();
        assert_eq!(s.expose(), "from-file");
        let cmd_only = Sources {
            file: None,
            command: both.command.clone(),
        };
        let s = resolve_with(&SPEC, None, &cmd_only, SHORT * 10)
            .unwrap()
            .unwrap();
        assert_eq!(s.expose(), "from-command");
    }

    #[test]
    fn debug_redacts_the_value() {
        let s = resolve_with(&SPEC, Some(FAKE.into()), &Sources::default(), SHORT)
            .unwrap()
            .unwrap();
        assert!(!format!("{s:?}").contains(FAKE));
    }

    #[test]
    fn is_not_configured_walks_the_chain() {
        #[derive(Debug)]
        struct Wrap(NotConfigured);
        impl fmt::Display for Wrap {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("wrapped")
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        assert!(is_not_configured(&Wrap(not_set(&SPEC))));
        assert!(!is_not_configured(&std::io::Error::other("x")));
    }
}
