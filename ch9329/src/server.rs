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
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            let line = match msg {
                // Only a line terminator is stripped: a `type` line's trailing
                // spaces are part of its text.
                Message::Text(t) => t.trim_end_matches(['\r', '\n']).to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            if line.trim().is_empty() {
                continue;
            }
            let _ = hid.send(line).await; // result is broadcast as an `evt` frame
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
}
