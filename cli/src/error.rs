// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! The error contract: every paniolo failure has a [`Kind`], and the kind
//! decides the exit status (and, when asked for, a one-line JSON object on
//! stderr), so a program driving paniolo can branch on the failure without
//! parsing its message. See `docs/dev/error-contract/design.md`.

use std::fmt;

use serde_json::json;

/// Set to `1` to request the JSON error object (or pass `--json-errors`).
/// Read once at startup by [`take_json_request`] and then removed from this
/// process's environment, so hooks, helpers and daemons never inherit it: a
/// hook that runs paniolo would otherwise print its own object ahead of this
/// process's, and the contract is exactly one. Dispatch passes
/// `--json-errors` to a remote paniolo instead.
pub const JSON_ERRORS_ENV: &str = "PANIOLO_JSON_ERRORS";

/// The exit-status section of `paniolo --help`: every code paniolo emits, and
/// no others (a test checks it against [`Kind::ALL`]). The full contract is in
/// `docs/errors.md`.
pub const EXIT_STATUS_HELP: &str = "\
Exit status:
  0    success
  1    negative answer only (doctor found problems); no error exits 1
  2    usage             bad flags or arguments
  3    not_configured    no lab, unknown target/channel/interface/host or missing
                         required channel configuration;
                         a hook or helper missing or not executable
  4    unreachable       control host not reachable over SSH
  22   timeout           no answer within a deadline; outcome unknown
  100  daemon_down       the channel's daemon is not running
  101  helper_failed     a hook, helper or daemon ran and failed
  109  internal          not yet classified
Passthroughs (helper, config edit, video shot/devices, adb run/input/devices,
adb shell, serial connect, setup --host) exit with their program's own status.
PANIOLO_JSON_ERRORS=1, or --json-errors before any trailing arguments, adds a
one-line JSON object as the last line of stderr:
{\"error\": {kind, code, message, target, channel, host, child_exit}}.";

/// What went wrong, in the house exit-code bands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Bad flags or arguments. Clap reports most of these itself.
    Usage,
    /// No lab file, an invalid one, or an unknown target, channel, interface
    /// or host; missing required channel configuration; a hook or helper
    /// that is missing or not executable.
    NotConfigured,
    /// A control host that cannot be reached (paniolo does not probe device
    /// nodes itself; the daemons report those).
    Unreachable,
    /// No answer within a deadline; the outcome is unknown.
    Timeout,
    /// The channel's daemon is not running and the command needs it.
    DaemonDown,
    /// A hook or helper ran and exited non-zero.
    HelperFailed,
    /// Any failure not yet classified.
    Internal,
}

impl Kind {
    /// Every kind, for the tests that pin the help text and the bands.
    #[cfg(test)]
    pub const ALL: [Kind; 7] = [
        Kind::Usage,
        Kind::NotConfigured,
        Kind::Unreachable,
        Kind::Timeout,
        Kind::DaemonDown,
        Kind::HelperFailed,
        Kind::Internal,
    ];

    /// Never called. A new variant stops the build at this match, which is
    /// the reminder to add it to [`Kind::ALL`] above too; the help test then
    /// fails until `EXIT_STATUS_HELP` lists it.
    #[cfg(test)]
    #[allow(dead_code)]
    fn listed_in_all(self) {
        match self {
            Kind::Usage
            | Kind::NotConfigured
            | Kind::Unreachable
            | Kind::Timeout
            | Kind::DaemonDown
            | Kind::HelperFailed
            | Kind::Internal => {}
        }
    }

    /// The process exit status for this kind.
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::Usage => 2,
            Kind::NotConfigured => 3,
            Kind::Unreachable => 4,
            Kind::Timeout => 22,
            Kind::DaemonDown => 100,
            Kind::HelperFailed => 101,
            Kind::Internal => 109,
        }
    }

    /// The `kind` string in the JSON object.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Usage => "usage",
            Kind::NotConfigured => "not_configured",
            Kind::Unreachable => "unreachable",
            Kind::Timeout => "timeout",
            Kind::DaemonDown => "daemon_down",
            Kind::HelperFailed => "helper_failed",
            Kind::Internal => "internal",
        }
    }
}

/// A classified failure. Its `Display` is just `message`, so wrapping an
/// existing `anyhow!` site in one leaves the printed text unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanioloError {
    pub kind: Kind,
    pub message: String,
    pub target: Option<String>,
    pub channel: Option<String>,
    pub host: Option<String>,
    pub child_exit: Option<i32>,
}

impl PanioloError {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        PanioloError {
            kind,
            message: message.into(),
            target: None,
            channel: None,
            host: None,
            child_exit: None,
        }
    }

    pub fn not_configured(message: impl Into<String>) -> Self {
        Self::new(Kind::NotConfigured, message)
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn child_exit(mut self, code: Option<i32>) -> Self {
        self.child_exit = code;
        self
    }
}

/// Fill in the `target` of a classified error that was raised without one
/// (a lower layer that does not know the target name); any other error is
/// returned unchanged. Call it on the error as the lower layer returned it:
/// the by-value downcast would drop `.context()` added on top.
pub fn with_target(err: anyhow::Error, target: &str) -> anyhow::Error {
    match err.downcast::<PanioloError>() {
        Ok(mut e) => {
            if e.target.is_none() {
                e.target = Some(target.to_string());
            }
            e.into()
        }
        Err(other) => other,
    }
}

/// `target '<t>' not found in lab`, classified.
pub fn target_not_found(target: &str) -> PanioloError {
    PanioloError::not_configured(format!("target '{target}' not found in lab")).target(target)
}

/// A target lacking a channel, a channel lacking a required field, a named
/// interface the target does not have, or a channel configured on another
/// host than the one running the command: `not_configured`, carrying the
/// target and the channel kind.
pub fn channel_missing(target: &str, channel: &str, message: String) -> PanioloError {
    PanioloError::not_configured(message)
        .target(target)
        .channel(channel)
}

/// A child's exit status the way a shell reports it: its code, or 128+N when
/// signal N killed it. Never the -1 (seen as 255, ssh's own failure code) that
/// `status.code().unwrap_or(-1)` produced for a killed child. Passthroughs
/// (design D3) exit with this unchanged.
pub fn shell_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// A hook or helper that ran and failed, from its exit status. 126 (not
/// executable) and 127 (not found) mean the lab points at something that is
/// not there: `not_configured`. On Windows hooks run under `cmd.exe /C`,
/// which exits 1 for an unknown command or a missing path alike, the same as
/// a script's own failure, so there a missing hook is `helper_failed`.
/// Anything else is `helper_failed`, with the child's code in `child_exit`
/// (null when a signal killed it).
pub fn hook_failed(status: std::process::ExitStatus, message: String) -> PanioloError {
    let kind = match status.code() {
        Some(126 | 127) => Kind::NotConfigured,
        _ => Kind::HelperFailed,
    };
    PanioloError::new(kind, message).child_exit(status.code())
}

/// A bundled helper binary (serialcap, hdmicap, …) that is not installed.
pub fn helper_missing(name: &str) -> PanioloError {
    PanioloError::not_configured(format!("{name} not found"))
}

/// The channel a paniolo daemon serves, for the JSON `channel` field.
pub fn daemon_channel(daemon: &str) -> &str {
    match daemon {
        "serialcap" => "serial",
        "hdmicap" => "video",
        "netbootd" => "netboot",
        other => other,
    }
}

/// `daemon` (serialcap, hdmicap, …) is not running and the command needs it.
pub fn daemon_down(daemon: &str, message: impl Into<String>) -> PanioloError {
    PanioloError::new(Kind::DaemonDown, message).channel(daemon_channel(daemon))
}

/// A daemon did not become ready within its start deadline (the caller puts
/// the deadline and the daemon's last stderr in `message`) and is still
/// running: `timeout`, since it may yet come up.
pub fn daemon_start_timeout(daemon: &str, message: String) -> PanioloError {
    PanioloError::new(Kind::Timeout, message).channel(daemon_channel(daemon))
}

/// A failed HTTP request to a running daemon's discovery endpoint, classified:
/// a refused connection means the discovery record outlived the daemon
/// (`daemon_down`); a connect or read that timed out leaves the outcome
/// unknown (`timeout`); an error status is the daemon refusing the request
/// (`helper_failed`, with the daemon's own explanation, stripped of control
/// characters and capped, since whatever answers on a stale port is not
/// authenticated). `what` names the request (`"serialcap /input"`).
pub fn daemon_request_failed(daemon: &str, what: &str, e: ureq::Error) -> PanioloError {
    let kind = match &e {
        ureq::Error::Status(..) => Kind::HelperFailed,
        ureq::Error::Transport(t) => match t.kind() {
            // ureq reports a connect that hit its deadline as ConnectionFailed
            // too; the io error underneath tells the two apart.
            ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io if is_timeout(t) => {
                Kind::Timeout
            }
            ureq::ErrorKind::ConnectionFailed => Kind::DaemonDown,
            _ => Kind::Internal,
        },
    };
    let message = match e {
        ureq::Error::Status(code, resp) => {
            let body = printable(resp.into_string().unwrap_or_default().trim());
            match body.trim() {
                "" => format!("{what} failed: daemon returned status {code}"),
                msg => format!("{what} failed: {msg}"),
            }
        }
        e => format!("{what} failed: {e}"),
    };
    PanioloError::new(kind, message).channel(daemon_channel(daemon))
}

/// `s` with control characters (ANSI escapes included) replaced by spaces
/// and cut to [`BODY_LIMIT`] characters, for text from a daemon that is going
/// to a terminal.
fn printable(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(BODY_LIMIT)
        .collect();
    if s.chars().count() > BODY_LIMIT {
        out.push('…');
    }
    out
}

const BODY_LIMIT: usize = 300;

fn is_timeout(t: &ureq::Transport) -> bool {
    let mut src = std::error::Error::source(t);
    while let Some(e) = src {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            );
        }
        src = e.source();
    }
    false
}

/// ssh's own failure code, as opposed to the remote command's.
pub const SSH_TRANSPORT_FAILURE: i32 = 255;

/// The message for ssh's 255 from `host`: OpenSSH exits 255 when the
/// connection fails and also when the remote command is killed by a signal,
/// so the outcome is unknown.
pub fn ssh_255_message(host: &str) -> String {
    format!(
        "ssh to control host '{host}' exited 255: the host is unreachable or the \
         remote command was killed; its outcome is unknown"
    )
}

/// The control host `host` could not be reached (ssh exited 255, or the lab
/// slice could not be copied to it).
pub fn unreachable_host(host: &str, message: String) -> PanioloError {
    PanioloError::new(Kind::Unreachable, message).host(host)
}

impl fmt::Display for PanioloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PanioloError {}

/// Classify an error for `main()`: the first [`PanioloError`] in the chain
/// wins (its fields are kept, with the full chain as the message); a
/// [`crate::model::LabError`] is `not_configured`; anything else is
/// `internal`.
pub fn classify(err: &anyhow::Error) -> PanioloError {
    let message = format!("{err:#}");
    for cause in err.chain() {
        if let Some(pe) = cause.downcast_ref::<PanioloError>() {
            return PanioloError {
                message,
                ..pe.clone()
            };
        }
        if cause.downcast_ref::<crate::model::LabError>().is_some() {
            return PanioloError::not_configured(message);
        }
    }
    PanioloError::new(Kind::Internal, message)
}

static JSON_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn env_requests_json() -> bool {
    std::env::var_os(JSON_ERRORS_ENV).is_some_and(|v| v == "1")
}

/// Record whether this process was asked for the JSON object (`flag`, or
/// [`JSON_ERRORS_ENV`]) and remove the variable from the environment so no
/// child inherits it. Call once, from `main`, before anything is spawned.
pub fn take_json_request(flag: bool) {
    let requested = flag || env_requests_json();
    JSON_REQUESTED.store(requested, std::sync::atomic::Ordering::Relaxed);
    std::env::remove_var(JSON_ERRORS_ENV);
}

/// Whether the JSON error object was requested for this process. Before
/// [`take_json_request`] (a command-line parse failure) the variable is
/// still in the environment and is read directly.
pub fn json_requested() -> bool {
    JSON_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) || env_requests_json()
}

/// A command-line parse failure (clap's error, exit 2). Clap's own text goes
/// to stderr as always; the JSON object follows when it was requested, by the
/// variable or by `--json-errors` anywhere before a `--` in `args` (the flag
/// itself was never parsed, since parsing is what failed).
pub fn report_parse_error(err: &clap::Error, args: &[std::ffi::OsString]) -> i32 {
    let code = err.exit_code();
    let _ = err.print();
    let flagged = args
        .iter()
        .skip(1)
        .take_while(|a| a.as_os_str() != "--")
        .any(|a| a.as_os_str() == "--json-errors");
    // `--help` and `--version` are clap "errors" that exit 0: not failures.
    if code != 0 && (flagged || json_requested()) {
        // Clap's first line, without its `error: ` prefix: the same sentence
        // a person sees at the top of the usage text.
        // A command group given without its subcommand makes clap print the
        // group's help instead of an error line, so name the problem.
        let message =
            if err.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
                "a subcommand is required".to_string()
            } else {
                let text = err.to_string();
                let mut lines = text.lines().map(str::trim);
                let first = lines.next().unwrap_or_default();
                let first = first.strip_prefix("error: ").unwrap_or(first);
                // "the following required arguments were not provided:"
                // names them, one per line, up to a blank line; keep them.
                if first.ends_with(':') {
                    let named: Vec<&str> = lines
                        .take_while(|l| !l.is_empty() && !l.starts_with("Usage:"))
                        .collect();
                    format!("{first} {}", named.join(", "))
                } else {
                    first.to_string()
                }
            };
        eprintln!("{}", to_json(&PanioloError::new(Kind::Usage, message)));
    }
    code
}

/// The one-line JSON error object.
pub fn to_json(e: &PanioloError) -> String {
    json!({
        "error": {
            "kind": e.kind.as_str(),
            "code": e.kind.exit_code(),
            "message": e.message,
            "target": e.target,
            "channel": e.channel,
            "host": e.host,
            "child_exit": e.child_exit,
        }
    })
    .to_string()
}

/// Report `err` on stderr per the contract and return the exit status.
pub fn report(err: &anyhow::Error) -> i32 {
    let e = classify(err);
    eprintln!("{}", e.message);
    if json_requested() {
        eprintln!("{}", to_json(&e));
    }
    e.kind.exit_code()
}

/// Make a closed stdout a quiet success instead of a panic.
///
/// Rust ignores SIGPIPE, so when the reader goes away early (`paniolo skill |
/// head -1`, `| grep -q`) the next `println!` panics with "failed printing to
/// stdout: Broken pipe" and the process exits 101, a status outside the
/// contract. The reader has already taken what it wanted, so exit 0 silently,
/// as ripgrep does. Restoring SIGPIPE's default action instead would also kill
/// the process on a write to a dead ssh child or socket, which the code
/// handles as an ordinary error today.
pub fn exit_quietly_on_closed_stdout() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_closed_stdout_panic(info.payload()) {
            std::process::exit(0);
        }
        default(info);
    }));
}

/// Whether a panic payload is std's `println!` failure on a closed pipe.
/// It is matched exactly against the message std builds from each OS error
/// that means "the reader is gone", so any other stdout failure (a full disk)
/// still panics loudly. The closed-pipe test in `tests/error_contract.rs`
/// fails if a future std words this differently.
fn is_closed_stdout_panic(payload: &(dyn std::any::Any + Send)) -> bool {
    let Some(msg) = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
    else {
        return false;
    };
    #[cfg(unix)]
    let codes = [libc::EPIPE];
    // ERROR_NO_DATA ("the pipe is being closed") and ERROR_BROKEN_PIPE.
    #[cfg(windows)]
    let codes = [232, 109];
    codes.iter().any(|&code| {
        msg == format!(
            "failed printing to stdout: {}",
            std::io::Error::from_raw_os_error(code)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn only_a_closed_stdout_panic_is_quiet() {
        #[cfg(unix)]
        let closed = std::io::Error::from_raw_os_error(libc::EPIPE);
        #[cfg(windows)]
        let closed = std::io::Error::from_raw_os_error(232);
        let quiet = format!("failed printing to stdout: {closed}");
        assert!(is_closed_stdout_panic(&quiet));
        let full = format!(
            "failed printing to stdout: {}",
            std::io::Error::from(std::io::ErrorKind::StorageFull)
        );
        assert!(!is_closed_stdout_panic(&full));
        let stderr = quiet.replace("stdout", "stderr");
        assert!(!is_closed_stdout_panic(&stderr));
        assert!(!is_closed_stdout_panic(&"some other panic"));
        assert!(!is_closed_stdout_panic(&42_u32));
    }

    #[test]
    fn codes_follow_the_bands() {
        for k in Kind::ALL {
            let c = k.exit_code();
            assert!((2..125).contains(&c), "{k:?} -> {c}");
        }
    }

    #[test]
    fn help_lists_exactly_the_emitted_codes() {
        let listed: Vec<(i32, Option<&str>)> = EXIT_STATUS_HELP
            .lines()
            .filter_map(|l| {
                let mut words = l.split_whitespace();
                let code = words.next()?.parse().ok()?;
                Some((code, words.next()))
            })
            .collect();
        let mut expected: Vec<(i32, Option<&str>)> =
            vec![(0, Some("success")), (1, Some("negative"))];
        expected.extend(Kind::ALL.iter().map(|k| (k.exit_code(), Some(k.as_str()))));
        assert_eq!(listed, expected);
    }

    #[test]
    fn classify_finds_a_wrapped_error_and_keeps_the_chain() {
        let inner: anyhow::Result<()> =
            Err(PanioloError::not_configured("target 'x' not found in lab")
                .target("x")
                .into());
        let err = inner.context("power cycle").unwrap_err();
        let e = classify(&err);
        assert_eq!(e.kind, Kind::NotConfigured);
        assert_eq!(e.target.as_deref(), Some("x"));
        assert_eq!(e.message, "power cycle: target 'x' not found in lab");
    }

    #[test]
    fn lab_error_is_not_configured_and_anything_else_internal() {
        let lab = anyhow::Error::new(crate::model::LabError("bad".into()));
        assert_eq!(classify(&lab).kind, Kind::NotConfigured);
        assert_eq!(classify(&anyhow::anyhow!("boom")).kind, Kind::Internal);
    }

    #[cfg(unix)]
    fn status(raw: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(raw)
    }

    #[cfg(unix)]
    #[test]
    fn shell_code_reports_signals_as_128_plus_n() {
        assert_eq!(shell_code(status(7 << 8)), 7);
        assert_eq!(shell_code(status(15)), 143, "SIGTERM");
    }

    #[cfg(unix)]
    #[test]
    fn hook_failed_classifies_by_status() {
        let e = hook_failed(status(7 << 8), "m".into());
        assert_eq!((e.kind, e.child_exit), (Kind::HelperFailed, Some(7)));
        let e = hook_failed(status(127 << 8), "m".into());
        assert_eq!((e.kind, e.child_exit), (Kind::NotConfigured, Some(127)));
        let e = hook_failed(status(9), "m".into());
        assert_eq!((e.kind, e.child_exit), (Kind::HelperFailed, None));
        let e = hook_failed(status(200 << 8), "m".into());
        assert_eq!(
            (e.kind, e.child_exit),
            (Kind::HelperFailed, Some(200)),
            "a real exit code of 128 or more is kept, not read as a signal"
        );
    }

    #[test]
    fn a_refused_daemon_request_is_daemon_down() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = ureq::get(&format!("http://127.0.0.1:{port}/x"))
            .call()
            .unwrap_err();
        let pe = daemon_request_failed("serialcap", "serialcap /input", e);
        assert_eq!(pe.kind, Kind::DaemonDown, "{}", pe.message);
        assert_eq!(pe.channel.as_deref(), Some("serial"));
    }

    #[test]
    fn a_daemon_that_never_answers_is_a_timeout() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let e = ureq::get(&format!("http://127.0.0.1:{port}/x"))
            .timeout(std::time::Duration::from_millis(200))
            .call()
            .unwrap_err();
        drop(l);
        let pe = daemon_request_failed("hdmicap", "OCR", e);
        assert_eq!(pe.kind, Kind::Timeout, "{}", pe.message);
        assert_eq!(pe.channel.as_deref(), Some("video"));
    }

    #[test]
    fn a_daemon_error_status_is_helper_failed_with_its_reason() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = crate::stubhttp::serve_one(
            l,
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 15\r\nConnection: close\r\n\r\nno video signal",
        );
        let e = ureq::get(&format!("http://127.0.0.1:{port}/ocr"))
            .call()
            .unwrap_err();
        server.join().unwrap();
        let pe = daemon_request_failed("hdmicap", "OCR", e);
        assert_eq!(pe.kind, Kind::HelperFailed);
        assert_eq!(pe.message, "OCR failed: no video signal");
    }

    #[test]
    fn daemon_text_loses_control_characters_and_is_capped() {
        assert_eq!(printable("no \x1b[31msignal\r\n"), "no  [31msignal  ");
        let long = "x".repeat(BODY_LIMIT + 10);
        assert_eq!(printable(&long).chars().count(), BODY_LIMIT + 1);
    }

    #[test]
    fn json_has_every_field() {
        let e = PanioloError::not_configured("m").target("t");
        let v: serde_json::Value = serde_json::from_str(&to_json(&e)).unwrap();
        let o = &v["error"];
        assert_eq!(o["kind"], "not_configured");
        assert_eq!(o["code"], 3);
        assert_eq!(o["target"], "t");
        for f in ["channel", "host", "child_exit"] {
            assert!(o[f].is_null(), "{f}");
        }
    }
}
