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

//! Localhost HTTP/WebSocket API for the hid daemon (identical to hidrig's, so
//! `panio console` drives a CH9329 daemon the same way).
//!
//! `GET /hid` is a bidirectional WebSocket carrying the HID serial protocol:
//! each client text frame is one command line; the daemon replies one text
//! frame per command and also pushes a transcript of commands injected by
//! *other* clients (and the CLI), so the web console sees the full intermixed
//! stream. `POST /send` is the one-shot equivalent used by the CLI when a
//! daemon is already running. The hdmicap dashboard connects cross-port; the
//! auth layer (`auth.rs`) admits only loopback origins that present the
//! daemon's token and echoes that one origin in the CORS header — never `*`.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, State,
    },
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use crate::uart::HidHandle;

#[derive(Clone)]
pub struct AppState {
    pub hid: HidHandle,
}

/// Ceiling on a `POST /send` body: one command line. The longest legitimate
/// line is a full-length `type`: [`crate::session::MAX_TYPE_CHARS`] characters
/// of text plus the `"type "` verb and separator. Sizing the limit to that
/// keeps the documented character cap actually reachable — a flat 4096-byte
/// limit rejected a 4096-character `type` at ~4091 characters (issue #174).
/// The session enforces the exact character count; every other command is a
/// few tokens.
const MAX_SEND_BYTES: usize = crate::session::MAX_TYPE_CHARS + "type ".len();

/// Ceiling on one `/hid` WebSocket message, for the same reason.
const MAX_WS_MESSAGE_BYTES: usize = MAX_SEND_BYTES;

/// The API router. Every route sits behind the auth layer: loopback Host and
/// Origin, and the daemon token (see `auth.rs`).
pub fn router(state: AppState, auth: crate::auth::Auth) -> Router {
    Router::new()
        .route("/hid", get(hid_ws))
        .route(
            "/send",
            post(send).layer(DefaultBodyLimit::max(MAX_SEND_BYTES)),
        )
        .route("/stop", post(stop))
        .route("/status", get(status))
        .route("/version", get(version))
        .layer(middleware::from_fn_with_state(auth, crate::auth::require))
        .with_state(state)
}

/// Authenticated shutdown. `ch9329 stop` calls this instead of signaling the
/// PID in the discovery file: a record left behind by a crash can name a PID
/// the kernel has since handed to an unrelated process, and the token proves
/// the request reached the daemon that wrote the record. The daemon's serve
/// loop owns the `Notify` (see `daemon::run`).
async fn stop(Extension(shutdown): Extension<Arc<tokio::sync::Notify>>) -> &'static str {
    shutdown.notify_one();
    "hid daemon stopping\n"
}

/// `GET /status` — daemon liveness + the device it owns.
async fn status(State(s): State<AppState>) -> Response {
    Json(serde_json::json!({
        "device": s.hid.device,
        "pid": std::process::id(),
    }))
    .into_response()
}

/// `GET /version` — forwards a `version` command to the injector.
async fn version(State(s): State<AppState>) -> Response {
    match s.hid.send("version".to_string()).await {
        Ok(data) => data.into_response(),
        Err(e) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    }
}

/// `POST /send`, body = one command line. Returns the `OK <data>` payload, or
/// 503 with the `ERR`/transport message. Used by the CLI one-shot path.
async fn send(State(s): State<AppState>, body: String) -> Response {
    let line = body.trim_end_matches(['\r', '\n']).to_string();
    // A blank body is a no-op, matching the `/hid` WebSocket loop, which skips
    // blank frames rather than forwarding them: without this, `execute_line("")`
    // is reached and answers `ERR unknown command:` (issue #174).
    if line.trim().is_empty() {
        return axum::http::StatusCode::OK.into_response();
    }
    match s.hid.send(line).await {
        Ok(data) => data.into_response(),
        Err(e) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!("{e}\n"),
        )
            .into_response(),
    }
}

async fn hid_ws(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_ws(socket, s.hid))
}

/// What one inbound WebSocket item means for [`handle_ws`].
enum Inbound {
    /// A command line to run.
    Line(String),
    /// Nothing to do (blank line, ping, binary).
    Skip,
    /// The client closed the socket or the connection failed.
    Gone,
}

fn next_line(msg: Option<Result<Message, axum::Error>>) -> Inbound {
    match msg {
        Some(Ok(Message::Text(t))) => {
            // Only a line terminator is stripped: a `type` line's trailing
            // spaces are part of its text.
            let line = t.trim_end_matches(['\r', '\n']).to_string();
            if line.trim().is_empty() {
                Inbound::Skip
            } else {
                Inbound::Line(line)
            }
        }
        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => Inbound::Gone,
        Some(Ok(_)) => Inbound::Skip,
    }
}

/// Per-client WebSocket loop. Inbound text frames are command lines executed
/// against the shared UART owner; each gets a one-frame reply. Concurrently we
/// push transcript events for commands injected by everyone else.
async fn handle_ws(socket: WebSocket, hid: HidHandle) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = hid.subscribe();

    // transcript -> client (commands run by other clients / the CLI)
    let mut feed_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    let tag = if ev.ok { "evt ok" } else { "evt err" };
                    let frame = format!("{tag} {} :: {}", ev.line, ev.reply);
                    if sender.send(Message::Text(frame)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => debug!("hid ws observer lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });

    // client -> UART. Every command (from any client or the CLI) produces a
    // single broadcast `evt ok/err …` frame, so the issuer sees its own result
    // there too — no separate per-issuer reply channel is needed, and all
    // clients observe one consistent intermixed transcript.
    //
    // While a command is in flight the socket is still read: a close (or a
    // dead connection) drops the `send` future, which closes the request's
    // reply channel and cancels the command on the wire (#328). Text frames
    // that arrive meanwhile are queued and run in order afterwards.
    let mut recv_task = tokio::spawn(async move {
        let mut pending: std::collections::VecDeque<String> = Default::default();
        loop {
            let line = match pending.pop_front() {
                Some(line) => line,
                None => match next_line(receiver.next().await) {
                    Inbound::Line(line) => line,
                    Inbound::Skip => continue,
                    Inbound::Gone => return,
                },
            };
            let send = hid.send(line); // result is broadcast as an `evt` frame
            tokio::pin!(send);
            loop {
                tokio::select! {
                    _ = &mut send => break,
                    msg = receiver.next() => match next_line(msg) {
                        Inbound::Line(line) => pending.push_back(line),
                        Inbound::Skip => {}
                        Inbound::Gone => return,
                    },
                }
            }
        }
    });

    tokio::select! {
        _ = &mut feed_task => recv_task.abort(),
        _ = &mut recv_task => feed_task.abort(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    /// `/stop` sits behind the same bearer-token layer as every other route,
    /// and only a valid token may wake the daemon's shutdown `Notify`.
    #[tokio::test]
    async fn shutdown_requires_the_daemon_token() {
        let hid = HidHandle::spawn("test-device".into());
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let app = router(
            AppState { hid },
            crate::auth::Auth::new("test-token".into(), &[]),
        )
        .layer(Extension(shutdown.clone()));
        for token in [None, Some("wrong"), Some("test-token")] {
            let mut req = HttpRequest::builder()
                .method("POST")
                .uri("/stop")
                .header(header::HOST, "127.0.0.1:1");
            if let Some(token) = token {
                req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let resp = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let valid = token == Some("test-token");
            assert_eq!(
                resp.status(),
                if valid {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                },
                "token {token:?}"
            );
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_millis(10), shutdown.notified())
                    .await
                    .is_ok(),
                valid,
                "token {token:?} must {}wake the shutdown",
                if valid { "" } else { "not " }
            );
        }
    }

    /// #174(b): `POST /send` with an empty body is a no-op — it must not be
    /// forwarded to the injector as a blank command line. The daemon here owns
    /// a device that cannot open, so a forwarded blank line would surface a 503
    /// transport error; the guard returns 200 without ever reaching the owner.
    #[tokio::test]
    async fn empty_send_body_is_not_forwarded() {
        let hid = HidHandle::spawn("/nonexistent/ch9329-empty-send".into());
        let app = router(
            AppState { hid },
            crate::auth::Auth::new("test-token".into(), &[]),
        )
        .layer(Extension(Arc::new(tokio::sync::Notify::new())));
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/send")
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a blank /send body is a no-op, not a forwarded command"
        );
    }

    // -- #328: an in-flight command is cancelled when its requester goes away --

    use crate::session::test_support::{session_on, FakePort, WriteLog};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// A 300-character `type` on a fake port: ~15 ms per character, so about
    /// five seconds if nothing stops it.
    fn long_type() -> String {
        format!("type {}", "abcdefghij".repeat(30))
    }

    /// The real router on a loopback port, backed by the real owner thread
    /// running over an auto-acknowledging fake serial port.
    async fn serve() -> (u16, HidHandle, WriteLog) {
        let log = WriteLog::default();
        let hid =
            HidHandle::spawn_with_session("fake".into(), session_on(FakePort::acking(log.clone())));
        let app = router(
            AppState { hid: hid.clone() },
            crate::auth::Auth::new("test-token".into(), &[]),
        )
        .layer(Extension(Arc::new(tokio::sync::Notify::new())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, hid, log)
    }

    /// Open a connection and write a raw `POST /send` for `line`, without
    /// reading the response.
    async fn post_raw(port: u16, line: &str) -> TcpStream {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "POST /send HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Authorization: Bearer test-token\r\nContent-Length: {}\r\n\r\n{line}",
            line.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        sock
    }

    /// Wait for the transcript event that reports the command cancelled.
    async fn wait_cancelled(events: &mut tokio::sync::broadcast::Receiver<crate::uart::Event>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let ev = events.recv().await.unwrap();
                if !ev.ok && crate::session::is_cancelled(&ev.reply) {
                    return;
                }
            }
        })
        .await
        .expect("no cancellation event: the in-flight command was not cancelled");
    }

    /// Keyboard reports (`CMD_KB_GENERAL`) in the wire log.
    fn keyboard_frames(log: &WriteLog) -> Vec<Vec<u8>> {
        log.snapshot()
            .into_iter()
            .filter(|w| w[3] == 0x02)
            .collect()
    }

    /// Whether a keyboard frame holds no modifier and no key.
    fn keyboard_is_clear(frame: &[u8]) -> bool {
        frame[5..13].iter().all(|&b| b == 0)
    }

    /// The mechanism, and acceptance check 1: an HTTP client that drops its
    /// connection mid-`type` (what a SIGKILLed CLI does) makes the daemon
    /// stop writing keyboard reports and leave the keys released.
    #[tokio::test]
    async fn dropped_http_client_cancels_a_running_type() {
        let (port, hid, log) = serve().await;
        let mut events = hid.subscribe();
        let line = long_type();
        let sock = post_raw(port, &line).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let during = keyboard_frames(&log).len();
        assert!(during > 4, "the type should be under way, saw {during}");
        drop(sock);
        wait_cancelled(&mut events).await;
        let at_cancel = log.snapshot().len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            log.snapshot().len(),
            at_cancel,
            "reports kept flowing after the cancellation was acknowledged"
        );
        let frames = keyboard_frames(&log);
        assert!(
            frames.len() < 2 * 300,
            "the whole 300 characters were typed ({} reports)",
            frames.len()
        );
        assert!(
            keyboard_is_clear(frames.last().unwrap()),
            "the last keyboard report must release every key"
        );
    }

    /// Acceptance check 2: a `releaseall` queued behind the cancelled command
    /// reaches the wire only after the cancelled command stopped writing, and
    /// is the last frame.
    #[tokio::test]
    async fn release_after_a_cancel_runs_after_the_command_stopped() {
        let (port, hid, log) = serve().await;
        let mut events = hid.subscribe();
        let sock = post_raw(port, &long_type()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(sock);
        // Queued behind the in-flight type, as a harness's cleanup would be.
        let release = tokio::time::timeout(Duration::from_secs(10), hid.send("releaseall".into()));
        let ok = release.await.expect("releaseall answered").is_ok();
        assert!(ok);
        wait_cancelled(&mut events).await;
        let writes = log.snapshot();
        let last = writes.last().unwrap();
        assert_eq!(last[3], 0x02);
        assert!(keyboard_is_clear(last));
        // Nothing but the cancelled type's frames precede the releaseall, and
        // the type's own last frame was already a release: the releaseall is
        // the single frame after the cancellation point.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            log.snapshot().len(),
            writes.len(),
            "no frame after releaseall"
        );
        let typed = writes.len() - 1;
        assert!(
            typed < 2 * 300,
            "the type ran to completion ({typed} frames)"
        );
    }

    /// Keys held earlier with `down` stay held through a cancelled `type`.
    #[tokio::test]
    async fn a_cancel_keeps_keys_held_earlier() {
        let (port, hid, log) = serve().await;
        let mut events = hid.subscribe();
        hid.send("down LEFT_SHIFT".into()).await.unwrap();
        let sock = post_raw(port, &long_type()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(sock);
        wait_cancelled(&mut events).await;
        let frames = keyboard_frames(&log);
        let last = frames.last().unwrap();
        assert_eq!(last[5], crate::keys::MOD_LEFT_SHIFT, "shift still held");
        assert!(last[7..13].iter().all(|&b| b == 0), "no stray key");
    }

    const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

    /// A masked client text frame (the mask is all zero, which RFC 6455 allows).
    fn ws_text_frame(payload: &str) -> Vec<u8> {
        let mut f = vec![0x81];
        let n = payload.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(payload.as_bytes());
        f
    }

    /// Acceptance check 3: a WebSocket client that closes its socket while a
    /// `type` is running cancels it.
    #[tokio::test]
    async fn closed_websocket_cancels_a_running_type() {
        let (port, hid, log) = serve().await;
        let mut events = hid.subscribe();
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "GET /hid HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Authorization: Bearer test-token\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: {WS_KEY}\r\n\r\n"
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut head = [0u8; 12];
        sock.read_exact(&mut head).await.unwrap();
        assert!(head.starts_with(b"HTTP/1.1 101"), "{head:?}");
        sock.write_all(&ws_text_frame(&long_type())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            keyboard_frames(&log).len() > 4,
            "the type should be under way"
        );
        sock.write_all(&[0x88, 0x80, 0, 0, 0, 0]).await.unwrap(); // Close
        wait_cancelled(&mut events).await;
        let at_cancel = log.snapshot().len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(log.snapshot().len(), at_cancel);
        assert!(keyboard_is_clear(keyboard_frames(&log).last().unwrap()));
    }
}
