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

//! The UART owner: a single dedicated thread that owns the CH9329 control UART
//! (one long-lived [`Session`]) and serializes every command — CLI-injected and
//! WebSocket-injected alike — onto the one wire, one in flight. That single
//! queue is what makes events from the web console and the CLI intermix
//! correctly, and it is what makes held state (`down`/`mdown`/drag) work: the
//! one persistent `Session` carries the modifier/button report across commands,
//! which a one-shot CLI invocation cannot.
//!
//! It uses the **blocking** `serialport` path (the same one the one-shot CLI
//! uses), not async I/O: tokio-serial's async reads do not get reliable
//! read-readiness on a macOS tty. The thread bridges to the async axum server
//! via tokio channels — `blocking_recv` for requests, `oneshot`/`broadcast`
//! sends for replies.
//!
//! The `Session` is opened lazily and dropped on transport error, so the daemon
//! recovers across adapter replug and target power cycles without a restart.

use std::thread;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::proto::{budget, execute_line_cancellable};
use crate::session::{is_cancelled, Cancel, Session};

const REQ_CAP: usize = 256;
const TRANSCRIPT_CAP: usize = 256;
/// Start of the error a client gets when the owner does not answer within the
/// command's [`budget`]. The CLI client recognizes it (the daemon answered
/// first, before the client's own timer) and then confirms the command
/// stopped, exactly as for a timeout of its own (#328).
pub const DAEMON_TIMEOUT_PREFIX: &str = "hid daemon did not answer within";

/// One item on the owner's queue.
enum Request {
    /// A command line, answered on `reply` with the `OK` data or the
    /// `ERR`/transport message.
    Line {
        line: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Shutdown: release every held key, modifier and button, so the target
    /// is not left with one down after the daemon — the only thing that
    /// remembers what it pressed — exits. Answered on `done` once the report
    /// is written or there was no open link to write it to. Never touches the
    /// USB mux.
    Release { done: oneshot::Sender<()> },
}

/// A transcript event broadcast to every WebSocket observer: the command that
/// ran and its one-line outcome. Lets a passive viewer see what the CLI (or
/// another browser) just injected.
#[derive(Clone, Debug)]
pub struct Event {
    pub line: String,
    pub ok: bool,
    pub reply: String,
}

/// Cloneable handle to the UART owner thread.
#[derive(Clone)]
pub struct HidHandle {
    req_tx: mpsc::Sender<Request>,
    transcript: broadcast::Sender<Event>,
    pub device: String,
}

impl HidHandle {
    /// Spawn the owner thread for `device` and return a handle. The port itself
    /// is opened lazily on the first command (so the daemon starts even with
    /// the target — and therefore the CH9329 — currently powered off).
    pub fn spawn(device: String) -> HidHandle {
        Self::spawn_inner(device, None)
    }

    /// [`spawn`](Self::spawn) with an already-open session, so a test can run
    /// the real owner loop over a fake serial port.
    #[cfg(test)]
    pub(crate) fn spawn_with_session(device: String, session: Session) -> HidHandle {
        Self::spawn_inner(device, Some(session))
    }

    fn spawn_inner(device: String, session: Option<Session>) -> HidHandle {
        let (req_tx, req_rx) = mpsc::channel(REQ_CAP);
        let (transcript, _) = broadcast::channel(TRANSCRIPT_CAP);
        let handle = HidHandle {
            req_tx,
            transcript: transcript.clone(),
            device: device.clone(),
        };
        thread::spawn(move || run(device, session, req_rx, transcript));
        handle
    }

    /// Submit one command line and await the reply (the `OK` data, or the
    /// `ERR`/transport message). The line must not contain a newline. The
    /// wait is bounded by the line's [`budget`] (it grows with the text of a
    /// `type`), so a request the owner never answers cannot hang every later
    /// client queued behind it.
    ///
    /// Dropping the returned future (a WebSocket client that closed, an HTTP
    /// client that disconnected) or letting the bound expire closes the
    /// request's reply channel, which cancels the command if it is already
    /// running on the wire (#328).
    pub async fn send(&self, line: String) -> Result<String, String> {
        let limit = budget(&line);
        self.send_within(line, limit).await
    }

    /// [`send`](Self::send) with an explicit bound on the wait.
    async fn send_within(&self, line: String, limit: Duration) -> Result<String, String> {
        if line.contains('\n') || line.contains('\r') {
            return Err(format!("command contains a newline: {line:?}"));
        }
        let (tx, rx) = oneshot::channel();
        let round_trip = async {
            self.req_tx
                .send(Request::Line { line, reply: tx })
                .await
                .map_err(|_| "hid daemon stopped".to_string())?;
            rx.await
                .map_err(|_| "hid daemon dropped the request".to_string())?
        };
        match tokio::time::timeout(limit, round_trip).await {
            Ok(result) => result,
            Err(_) => Err(format!("{DAEMON_TIMEOUT_PREFIX} {} s", limit.as_secs())),
        }
    }

    /// Shutdown hook: release every held key, modifier and button. Bounded by
    /// `limit`; the error is informational — the daemon exits either way.
    pub async fn release_for_shutdown(&self, limit: Duration) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let round_trip = async {
            self.req_tx
                .send(Request::Release { done: tx })
                .await
                .map_err(|_| "hid control link owner is gone".to_string())?;
            rx.await
                .map_err(|_| "hid control link owner dropped the release".to_string())
        };
        match tokio::time::timeout(limit, round_trip).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "hid control link did not release keys within {} ms",
                limit.as_millis()
            )),
        }
    }

    /// Subscribe to the command transcript (for WebSocket observers).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.transcript.subscribe()
    }
}

/// True for errors that mean the port/session itself is gone (vs. a board-level
/// `ERR`), so the next request reopens it (adapter replug, target power cycle).
fn is_transport_error(msg: &str) -> bool {
    msg.starts_with("cannot open")
        || msg.starts_with("serial write failed")
        || msg.starts_with("serial read failed")
        || msg.starts_with("timed out")
        || msg.starts_with("serial port closed")
        || msg.starts_with("CH9329 did not respond")
}

/// True for the specific "no reply within the timeout" shape, the subset of
/// [`is_transport_error`] worth one retry before it is believed.
fn is_timeout(msg: &str) -> bool {
    msg.starts_with("timed out")
}

/// Whether a command line is a pure query with no side effect on the target —
/// no HID edge, no pointer motion, no mux change, no flash write — and so is
/// safe to run a second time after a lost reply. Only these are retried on a
/// timeout.
///
/// An input-injecting command (`key`, `type`, `move`, `click`, `down`, ...) is
/// **never** retried at this level: `execute_line` runs a whole command, and
/// re-running it after the first attempt already reached the wire would inject
/// a *duplicate* keystroke, click or motion on the target — a `key ENTER`
/// whose release report was written but whose ACK read timed out would press
/// Enter a second time. A duplicated injection is worse than a reported
/// timeout the caller can decide about (issue #172). `usb host`/`usb target`
/// and `baud` change device state, so `usb`/`baud` are excluded too; `version`
/// never reaches the wire, but including it is harmless.
fn is_idempotent_query(line: &str) -> bool {
    let head = line.split_whitespace().next().unwrap_or("");
    matches!(
        head.to_ascii_lowercase().as_str(),
        "ping" | "info" | "version"
    )
}

/// Try `attempt`; if it fails with what looks like a timeout **and** the
/// command is idempotent, try it once more before accepting the failure. A
/// slow target or a momentarily busy host can lose a single round trip without
/// the link itself being gone — reopening for that is worse than one retry:
/// each reopen briefly toggles DTR/RTS, which on a KVM-Go is a hardware reset
/// of its MCU. A non-idempotent command (any HID injection) is passed through
/// unretried so a lost ACK never duplicates the injection (issue #172).
fn retry_once_on_timeout<F: FnMut() -> Result<String, String>>(
    idempotent: bool,
    mut attempt: F,
) -> Result<String, String> {
    let first = attempt();
    match &first {
        Err(msg) if idempotent && is_timeout(msg) => attempt(),
        _ => first,
    }
}

/// Execute one command line against `s`, retrying once on a lost reply only
/// when the command is an idempotent query (see [`is_idempotent_query`]). This
/// is the policy the owner applies to every dequeued line; it is a free
/// function so the retry decision can be exercised against a fake serial
/// transport in tests.
fn execute_line_with_retry(
    s: &mut Session,
    line: &str,
    cancel: Cancel<'_>,
) -> Result<String, String> {
    let idempotent = is_idempotent_query(line);
    retry_once_on_timeout(idempotent, || {
        execute_line_cancellable(s, line, cancel).map_err(|e| e.to_string())
    })
}

/// Handle a shutdown `Release`: clear every held key, modifier and button on
/// the target. If the session was dropped after a transport error, reopen it
/// first — `Session::open_preferring`'s success path runs `push_all_clear`, so
/// the reopen alone clears the chip — because the CH9329 holds its last report
/// independent of this process, and a bare "done" would otherwise leave a
/// `down shift` (say) stuck on the target after the daemon exits (issue #173).
/// If it cannot be reopened, warn that keys may still be held; shutdown
/// proceeds either way. The happy path (an already-open session) is unchanged.
fn handle_release(session: &mut Option<Session>, reopen: impl FnOnce() -> Result<Session, String>) {
    if session.is_none() {
        match reopen() {
            Ok(s) => *session = Some(s),
            Err(e) => {
                warn!(
                    "ch9329 shutdown release: cannot reopen to clear held keys \
                     (keys may remain held on the target): {e}"
                );
                return;
            }
        }
    }
    if let Some(s) = session.as_mut() {
        if let Err(e) = s.release_everything() {
            warn!("ch9329 shutdown release failed: {e}");
        }
    }
}

/// Serve one dequeued command line. Skips it — touching neither the session nor
/// the wire — when the client has already given up (its reply channel is
/// closed), so a request the caller was told timed out is not injected on the
/// target after the fact (issue #174). Otherwise it opens the session lazily,
/// executes under the retry policy, drops the session on a transport error so
/// the next request reopens it, broadcasts the outcome, and answers the caller.
///
/// A command already running is cancelled the same way a queued one is
/// skipped: the session polls `reply.is_closed()` between HID reports (#328).
/// A cancelled command has released what it pressed and its outcome is
/// broadcast as an error event; it is not a transport error, so the link stays
/// open.
fn serve_line(
    session: &mut Option<Session>,
    last_baud: &mut Option<u32>,
    device: &str,
    transcript: &broadcast::Sender<Event>,
    line: String,
    reply: oneshot::Sender<Result<String, String>>,
) {
    if reply.is_closed() {
        debug!("ch9329 owner dropping a request whose client gave up: {line:?}");
        return;
    }

    if session.is_none() {
        match Session::open_preferring(device, *last_baud) {
            Ok(s) => {
                info!("ch9329 UART open at {} baud for {device}", s.baud());
                *session = Some(s);
            }
            Err(e) => {
                let msg = e.to_string();
                broadcast_event(transcript, &line, &Err(msg.clone()));
                let _ = reply.send(Err(msg));
                return;
            }
        }
    }

    let s = session.as_mut().unwrap();
    let result = execute_line_with_retry(s, &line, &|| reply.is_closed());
    // Recorded after every command: a `baud` command moves the chip, and the
    // next reopen must probe where it went.
    *last_baud = Some(s.baud());
    if let Err(ref msg) = result {
        if is_cancelled(msg) {
            info!("ch9329 cancelled {line:?}: its client went away");
        } else if is_transport_error(msg) {
            warn!("ch9329 UART transport error, will reopen: {msg}");
            *session = None;
        }
    }
    broadcast_event(transcript, &line, &result);
    let _ = reply.send(result);
}

/// The owner loop (blocking thread): drain requests, execute each against the
/// one persistent [`Session`], broadcast the outcome.
fn run(
    device: String,
    mut session: Option<Session>,
    mut req_rx: mpsc::Receiver<Request>,
    transcript: broadcast::Sender<Event>,
) {
    // The rate the last session ran at. After a transport error (adapter
    // replug, target power cycle) the chip is almost always still there, so a
    // reopen probes it first instead of paying the default candidates' failed
    // probes — and a `baud` command that moved the chip is remembered too.
    let mut last_baud: Option<u32> = None;
    info!("ch9329 UART owner started for {device}");

    while let Some(req) = req_rx.blocking_recv() {
        match req {
            Request::Line { line, reply } => {
                serve_line(
                    &mut session,
                    &mut last_baud,
                    &device,
                    &transcript,
                    line,
                    reply,
                );
            }
            Request::Release { done } => {
                let baud = last_baud;
                handle_release(&mut session, || {
                    Session::open_preferring(&device, baud).map_err(|e| e.to_string())
                });
                let _ = done.send(());
            }
        }
    }
    info!("ch9329 UART owner stopped for {device}");
}

fn broadcast_event(tx: &broadcast::Sender<Event>, line: &str, result: &Result<String, String>) {
    let ev = match result {
        Ok(data) => Event {
            line: line.to_string(),
            ok: true,
            reply: if data.is_empty() {
                "OK".to_string()
            } else {
                format!("OK {data}")
            },
        },
        Err(e) => Event {
            line: line.to_string(),
            ok: false,
            reply: e.clone(),
        },
    };
    let _ = tx.send(ev);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request the owner never answers must come back as an error, not park
    /// the caller — and every client behind it — forever.
    #[tokio::test]
    async fn send_gives_up_when_the_owner_never_answers() {
        let (req_tx, _req_rx) = mpsc::channel(REQ_CAP);
        let (transcript, _) = broadcast::channel(TRANSCRIPT_CAP);
        let hid = HidHandle {
            req_tx,
            transcript,
            device: "none".into(),
        };
        let err = hid
            .send_within("ping".into(), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
    }

    #[test]
    fn transport_error_classification() {
        assert!(is_transport_error("cannot open /dev/x: busy"));
        assert!(is_transport_error("serial write failed: x"));
        assert!(is_transport_error("timed out waiting for CH9329 reply"));
        assert!(is_transport_error(
            "CH9329 did not respond on /dev/x at 115200/57600/9600 baud"
        ));
        assert!(!is_transport_error(
            "CH9329 rejected cmd 0x02: bad parameter (0xe5)"
        ));
        assert!(!is_transport_error("unknown command: foo"));
    }

    /// A lost reply to an idempotent query gets one retry before the caller
    /// sees a failure at all.
    #[test]
    fn a_timeout_gets_one_retry_then_the_result_stands() {
        let mut calls = 0;
        let result = retry_once_on_timeout(true, || {
            calls += 1;
            if calls == 1 {
                Err("timed out waiting for CH9329 reply".to_string())
            } else {
                Ok("ok".to_string())
            }
        });
        assert_eq!(result, Ok("ok".to_string()));
        assert_eq!(calls, 2);
    }

    /// A second consecutive timeout is accepted as failure, not retried again
    /// — one retry, not an unbounded loop.
    #[test]
    fn a_second_timeout_is_not_retried_again() {
        let mut calls = 0;
        let result = retry_once_on_timeout(true, || {
            calls += 1;
            Err("timed out waiting for CH9329 reply".to_string())
        });
        assert_eq!(calls, 2);
        assert!(result.is_err());
    }

    /// A non-timeout failure (a genuine NAK, say) is not retried at all —
    /// retrying would just repeat a board-level rejection.
    #[test]
    fn a_non_timeout_error_is_not_retried() {
        let mut calls = 0;
        let result = retry_once_on_timeout(true, || {
            calls += 1;
            Err("CH9329 rejected cmd 0x02: bad parameter (0xe5)".to_string())
        });
        assert_eq!(calls, 1);
        assert!(result.is_err());
    }

    /// A non-idempotent command (any HID injection) is never retried, even on a
    /// timeout: re-running the whole command would duplicate the keystroke or
    /// click on the target (issue #172).
    #[test]
    fn a_non_idempotent_timeout_is_not_retried() {
        let mut calls = 0;
        let result = retry_once_on_timeout(false, || {
            calls += 1;
            Err("timed out waiting for CH9329 reply".to_string())
        });
        assert_eq!(calls, 1, "a timed-out injection must not run a second time");
        assert!(result.is_err());
    }

    /// Only pure queries are treated as idempotent; every input-injecting or
    /// state-changing verb is not, so none of them is retried on a lost reply.
    #[test]
    fn only_pure_queries_are_idempotent() {
        for line in ["ping", "info", "version", "  info  ", "PING"] {
            assert!(is_idempotent_query(line), "{line:?} should be idempotent");
        }
        for line in [
            "key ENTER",
            "type hello",
            "move 10 10",
            "moveabs 1 1",
            "click",
            "down shift",
            "up shift",
            "combo LEFT_CONTROL c",
            "scroll 1",
            "usb host",
            "usb target",
            "usb state",
            "baud 9600",
            "releaseall",
        ] {
            assert!(!is_idempotent_query(line), "{line:?} must NOT be retried");
        }
    }

    #[test]
    fn timeout_classification() {
        assert!(is_timeout("timed out waiting for CH9329 reply"));
        assert!(!is_timeout(
            "CH9329 rejected cmd 0x02: bad parameter (0xe5)"
        ));
        assert!(!is_timeout("cannot open /dev/x: busy"));
    }

    /// The shutdown release is answered even when the board is absent (the
    /// port cannot open): "nothing held on a closed link", not a request left
    /// queued behind the reopen loop until the daemon's grace runs out.
    #[tokio::test]
    async fn release_for_shutdown_is_answered_without_a_board() {
        let hid = HidHandle::spawn("/nonexistent/ch9329-release-test".into());
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            hid.release_for_shutdown(Duration::from_secs(3)),
        )
        .await
        .expect("bounded by its own limit");
        assert_eq!(result, Ok(()));
    }

    // -- fake-transport integration tests for #172, #173, #174 ---------------

    use crate::session::test_support::{ack, session_on, FakePort, WriteLog};

    /// Wire opcodes, private to `session`; asserted here by value.
    const CMD_KB_GENERAL: u8 = 0x02;
    const CMD_MS_REL: u8 = 0x05;
    /// Index of the first key slot in a keyboard write buffer
    /// (`[HEAD0 HEAD1 ADDR cmd len mods 0x00 k0..k5 sum]`).
    const KEY_SLOTS_AT: usize = 7;

    /// #172: a `key` whose release-report ACK is lost must reach the target
    /// exactly once. The old code re-ran the whole `execute_line` on the
    /// timeout, pressing the key a second time; the policy now refuses to
    /// retry a non-idempotent command, so only one press ever reaches the chip.
    #[test]
    fn a_key_with_a_lost_release_ack_presses_the_target_only_once() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log.clone());
        // Only the press gets an ACK; every read after that times out.
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00]));
        let mut s = session_on(port);

        let result = execute_line_with_retry(&mut s, "key ENTER", &crate::session::never_cancelled);
        assert!(
            result.as_ref().unwrap_err().contains("timed out"),
            "the lost ACK is surfaced, not hidden by a retry: {result:?}"
        );

        let writes = log.snapshot();
        assert!(
            writes.iter().all(|w| w[3] == CMD_KB_GENERAL),
            "only keyboard reports were written: {writes:?}"
        );
        // ENTER is usage 0x28. Exactly one write may carry a key (the single
        // press); the old code produced a second press on the retry.
        let with_key = writes
            .iter()
            .filter(|w| w[KEY_SLOTS_AT..KEY_SLOTS_AT + 6].iter().any(|&b| b != 0))
            .count();
        assert_eq!(
            with_key, 1,
            "exactly one press reaches the target: {writes:?}"
        );
        assert_eq!(
            writes[0][KEY_SLOTS_AT], 0x28,
            "the one press is ENTER: {writes:?}"
        );
    }

    /// #173: a shutdown `Release` on a session that was dropped after a
    /// transport error reopens and clears the chip, rather than reporting done
    /// while a key is left held on the target.
    #[test]
    fn release_reopens_and_clears_a_dropped_session() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log.clone());
        // release_everything() writes a zero keyboard report then a zero mouse
        // report; ACK both.
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00]));
        port.queue_reply(&ack(CMD_MS_REL, &[0x00]));

        let mut reopened = false;
        let mut session: Option<Session> = None;
        handle_release(&mut session, || {
            reopened = true;
            Ok(session_on(port))
        });

        assert!(reopened, "a dropped session must be reopened to clear it");
        let writes = log.snapshot();
        assert_eq!(writes.len(), 2, "the reopened chip is cleared: {writes:?}");
        assert_eq!(writes[0][3], CMD_KB_GENERAL);
        assert_eq!(&writes[0][5..13], &[0u8; 8], "zero keyboard report");
        assert_eq!(writes[1][3], CMD_MS_REL);
    }

    /// #173: if the session cannot be reopened, the release still completes
    /// (shutdown proceeds) but nothing is written and the session stays absent.
    #[test]
    fn release_with_an_unreachable_board_does_not_panic() {
        let mut session: Option<Session> = None;
        handle_release(&mut session, || Err("cannot open /dev/x: busy".to_string()));
        assert!(session.is_none());
    }

    /// #174: a dequeued command whose client already gave up (its reply
    /// receiver dropped) must not be executed — nothing reaches the wire. The
    /// old code executed it, injecting on the target after the caller was told
    /// it timed out.
    #[test]
    fn a_request_whose_client_gave_up_is_not_executed() {
        let log = WriteLog::default();
        let port = FakePort::new(log.clone()); // no ACKs queued
        let mut session = Some(session_on(port));
        let mut last_baud = None;
        let (transcript, _rx) = broadcast::channel(16);

        let (reply_tx, reply_rx) = oneshot::channel::<Result<String, String>>();
        drop(reply_rx); // the client gave up waiting

        serve_line(
            &mut session,
            &mut last_baud,
            "fake",
            &transcript,
            "key ENTER".into(),
            reply_tx,
        );

        assert!(
            log.snapshot().is_empty(),
            "a request the client abandoned must not reach the target: {:?}",
            log.snapshot()
        );
    }

    // -- #328: the wait scales with the command ------------------------------

    /// A handle whose owner answers after `delay` (virtual time).
    fn slow_owner(delay: Duration) -> HidHandle {
        let (req_tx, mut req_rx) = mpsc::channel(REQ_CAP);
        let (transcript, _) = broadcast::channel(TRANSCRIPT_CAP);
        tokio::spawn(async move {
            while let Some(Request::Line { reply, .. }) = req_rx.recv().await {
                tokio::time::sleep(delay).await;
                let _ = reply.send(Ok(String::new()));
            }
        });
        HidHandle {
            req_tx,
            transcript,
            device: "none".into(),
        }
    }

    /// A long `type` whose budget exceeds the old fixed 30 s is not given up
    /// on at 30 s; a short command still is.
    #[tokio::test(start_paused = true)]
    async fn the_daemon_wait_scales_with_a_type() {
        let hid = slow_owner(Duration::from_secs(45));
        let long = format!("type {}", "x".repeat(500));
        assert!(budget(&long) > Duration::from_secs(45));
        assert_eq!(hid.send(long).await, Ok(String::new()));
        let err = hid.send("ping".into()).await.unwrap_err();
        assert!(err.starts_with(DAEMON_TIMEOUT_PREFIX), "{err}");
        assert!(err.ends_with("30 s"), "{err}");
    }

    /// A cancelled command is not a transport error: it must not make the
    /// owner drop and reopen the link.
    #[test]
    fn a_cancellation_is_not_a_transport_error() {
        assert!(!is_transport_error(crate::session::CANCELLED));
        assert!(is_cancelled(crate::session::CANCELLED));
    }
}
