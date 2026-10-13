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

//! Localhost HTTP API. The daemon can own several named serial interfaces; every
//! per-interface endpoint takes `?interface=NAME` and falls back to the default
//! (first-configured) interface when it's omitted, so single-interface clients
//! (and the existing dashboard) keep working unchanged.
//!
//! `/stream` is a bidirectional WebSocket: the daemon sends serial output (binary
//! frames) and accepts client keystrokes (binary or text) to write back to the
//! port. The hdmicap preview page connects here cross-port; the auth layer
//! (`auth.rs`) admits only loopback origins that present the daemon's token and
//! echoes that one origin in the CORS header — never `*`.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Extension, Query, State,
    },
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use crate::serial_io::{NamedSerial, SerialHandle, Serials};

#[derive(Clone)]
pub struct AppState {
    pub serials: Serials,
}

#[derive(Deserialize)]
pub struct IfaceParam {
    interface: Option<String>,
}

#[derive(Deserialize)]
pub struct ButtonParam {
    interface: Option<String>,
    ms: u64,
}

/// Ceiling on a single button press. A DTR press takes the port out of the
/// supervisor's select loop for its whole duration — no reads, no writes — so
/// an unbounded `ms` is a way to disable the interface until the daemon
/// restarts. Real presses are milliseconds to a few seconds (<=500 ms is a
/// power-button event, >=3 s a hard PMIC cut), so a minute is generous.
const MAX_BUTTON_MS: u64 = 60_000;

/// Ceiling on a `POST /input` body. A line of console input is bytes to a few
/// KiB; anything larger is a mistake or a flood, and with pacing every byte
/// costs wall-clock time on the port.
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Ceiling on one `/stream` WebSocket message (client keystrokes), for the
/// same reason.
const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;

/// Ceiling on per-byte pacing. 8 ms/byte is the known-good value for a polled
/// 115200-baud console; at ten seconds per byte a full body would already hold
/// the port for a week, so nothing real lies beyond it.
const MAX_PACE_MS: u64 = 10_000;

#[derive(Deserialize)]
pub struct InputParam {
    interface: Option<String>,
    /// Per-byte pacing in milliseconds for a slow polled console with no flow
    /// control. 0 (default) sends at full line rate.
    #[serde(default)]
    pace_ms: u64,
}

/// The API router. Every route sits behind the auth layer: loopback Host and
/// Origin, and the daemon token (see `auth.rs`).
pub fn router(state: AppState, auth: crate::auth::Auth) -> Router {
    Router::new()
        .route("/stop", post(stop))
        .route("/stream", get(stream))
        .route("/status", get(status))
        .route("/interfaces", get(interfaces))
        .route("/devices", get(devices))
        .route("/button", post(button))
        .route(
            "/input",
            post(input).layer(DefaultBodyLimit::max(MAX_INPUT_BYTES)),
        )
        .layer(middleware::from_fn_with_state(auth, crate::auth::require))
        .with_state(state)
}

async fn stop(Extension(shutdown): Extension<std::sync::Arc<tokio::sync::Notify>>) -> &'static str {
    shutdown.notify_one();
    "daemon stopping\n"
}

/// The per-byte pacing for `pace_ms`, or the refusal for one past the ceiling.
fn pace_of(pace_ms: u64) -> Result<std::time::Duration, String> {
    if pace_ms > MAX_PACE_MS {
        return Err(format!(
            "pace_ms={pace_ms} exceeds the {MAX_PACE_MS} ms ceiling\n"
        ));
    }
    Ok(std::time::Duration::from_millis(pace_ms))
}

/// Resolve the requested interface, or the default (first) when none is named.
fn resolve<'a>(serials: &'a Serials, name: &Option<String>) -> Option<&'a SerialHandle> {
    match name {
        Some(n) => serials.get(n),
        None => serials.default().map(|ns| &ns.handle),
    }
}

fn status_json(ns: &NamedSerial) -> serde_json::Value {
    let mut st = ns.handle.status();
    let observed_at_ms = crate::capture::now_ms();
    let last_rx_age_ms = st
        .health
        .last_rx_at_ms
        .map(|at| observed_at_ms.saturating_sub(at));
    if !st.connected {
        st.health.active_device = None;
        st.health.active_baud = None;
    }
    serde_json::json!({
        "name": ns.name,
        "device": st.device,
        "baud": st.baud,
        "connected": st.connected,
        "health": st.health,
        "observed_at_ms": observed_at_ms,
        "last_rx_age_ms": last_rx_age_ms,
        "power_on": st.power_on,   // null when no sense signal is configured
    })
}

/// Status of one interface (`?interface=NAME`) or, by default, all of them.
async fn status(State(s): State<AppState>, Query(q): Query<IfaceParam>) -> Response {
    match &q.interface {
        Some(name) => match s.serials.all().iter().find(|ns| &ns.name == name) {
            Some(ns) => (Json(status_json(ns))).into_response(),
            None => (StatusCode::NOT_FOUND, format!("no interface '{name}'")).into_response(),
        },
        None => {
            let all: Vec<_> = s.serials.all().iter().map(status_json).collect();
            (Json(all)).into_response()
        }
    }
}

/// All interfaces this daemon owns (name, device, baud, connected).
async fn interfaces(State(s): State<AppState>) -> Response {
    let all: Vec<_> = s.serials.all().iter().map(status_json).collect();
    (Json(all)).into_response()
}

async fn devices() -> Response {
    match crate::serial_io::list_ports() {
        Ok(list) => (Json(
            list.into_iter()
                .map(|(path, desc)| serde_json::json!({"path": path, "misc": desc}))
                .collect::<Vec<_>>(),
        ),)
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e:#}"),
        )
            .into_response(),
    }
}

/// Press the J2 power button on the attached target for `ms` milliseconds.
///
/// `POST /button?ms=200[&interface=NAME]`
///
/// Short presses (≤500 ms) deliver a power-button event to the OS (graceful
/// reboot/halt, target-OS-defined).  Long presses (≥3000 ms) trigger a PMIC
/// hard power-off.  The call blocks until the press completes.
/// Returns 200 on success, 503 if the supervisor is unavailable or a DTR ioctl fails.
async fn button(State(s): State<AppState>, Query(q): Query<ButtonParam>) -> Response {
    let handle = match resolve(&s.serials, &q.interface) {
        Some(h) => h.clone(),
        None => {
            let what = q.interface.as_deref().unwrap_or("(default)");
            return (StatusCode::NOT_FOUND, format!("no interface '{what}'")).into_response();
        }
    };
    if q.ms > MAX_BUTTON_MS {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "ms={} exceeds the {MAX_BUTTON_MS} ms ceiling — the port is out of the \
                 read/write loop for the whole press\n",
                q.ms
            ),
        )
            .into_response();
    }
    match handle.dtr_press(q.ms).await {
        Ok(()) => (format!("button pressed for {} ms\n", q.ms)).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}\n")).into_response(),
    }
}

/// Write the request body to the serial port the daemon already owns, so scripted
/// input coexists with live capture (no stop/restart, no exclusive re-open).
///
/// `POST /input?[interface=NAME][&pace_ms=N]`, body = raw bytes to send.
///
/// With `pace_ms > 0` the bytes are dripped one at a time that many ms apart —
/// the substitute for hardware flow control on a slow polled console. The call
/// blocks until the whole body has been written, so a paced send of N bytes
/// takes at least `(N - 1) * pace_ms` ms. Completion means driver acceptance,
/// not target execution. Disconnects fail pending writes without replay. The body is capped at [`MAX_INPUT_BYTES`] and
/// the pacing at [`MAX_PACE_MS`]. Returns 200 on success, 400 for a pace past
/// the ceiling, 404 for an unknown interface, 413 for an oversized body, 503
/// if the interface is disconnected or a driver write fails.
async fn input(State(s): State<AppState>, Query(q): Query<InputParam>, body: Bytes) -> Response {
    let handle = match resolve(&s.serials, &q.interface) {
        Some(h) => h.clone(),
        None => {
            let what = q.interface.as_deref().unwrap_or("(default)");
            return (StatusCode::NOT_FOUND, format!("no interface '{what}'")).into_response();
        }
    };
    let n = body.len();
    let pace = match pace_of(q.pace_ms) {
        Ok(p) => p,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    match handle.write_paced(body, pace).await {
        Ok(()) => (format!("wrote {n} bytes\n")).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}\n")).into_response(),
    }
}

async fn stream(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Query(q): Query<IfaceParam>,
) -> Response {
    let handle = match resolve(&s.serials, &q.interface) {
        Some(h) => h.clone(),
        None => {
            let what = q.interface.as_deref().unwrap_or("(default)");
            return (StatusCode::NOT_FOUND, format!("no interface '{what}'")).into_response();
        }
    };
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_ws(socket, handle))
}

async fn handle_ws(socket: WebSocket, serial: SerialHandle) {
    let (mut sender, mut receiver) = socket.split();
    // Scrollback and subscription together, under one lock, so no chunk is
    // ever delivered twice or missed (see `SerialHandle::attach`).
    let (snapshot, mut rx) = serial.attach();

    if !snapshot.is_empty() && sender.send(Message::Binary(snapshot)).await.is_err() {
        return;
    }

    // serial -> client
    let mut send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if sender.send(Message::Binary(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    debug!("ws client lagged, dropped {n} messages");
                    // The debug log alone is invisible to whoever is reading
                    // the terminal; put the loss in the stream itself, in the
                    // style of the connect/disconnect/button markers.
                    let marker = crate::serial_io::marker_line(
                        &format!("client lagged, dropped {n} chunks"),
                        33, // yellow
                    );
                    if sender.send(Message::Binary(marker.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    // client -> serial
    let input = serial.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            let bytes = match msg {
                Message::Binary(b) => Bytes::from(b),
                Message::Text(t) => Bytes::from(t.into_bytes()),
                Message::Close(_) => break,
                _ => continue,
            };
            // Wait for the supervisor's driver acknowledgement. A write
            // failure closes this connection; never silently replay input.
            if input
                .write_paced(bytes, std::time::Duration::ZERO)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_requires_the_daemon_token() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let app = router(
            AppState {
                serials: Serials::spawn_all(&[], std::path::Path::new("."), 100),
            },
            crate::auth::Auth::new("test-token".into(), &[]),
        )
        .layer(Extension(shutdown.clone()));
        for token in [None, Some("wrong"), Some("test-token")] {
            let mut req = Request::builder()
                .method("POST")
                .uri("/stop")
                .header("Host", "127.0.0.1");
            if let Some(token) = token {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let valid = token == Some("test-token");
            assert_eq!(
                response.status(),
                if valid {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_millis(10), shutdown.notified())
                    .await
                    .is_ok(),
                valid
            );
        }
    }

    /// Pacing multiplies the time a body holds the port; the ceiling keeps one
    /// request from parking the interface for days.
    #[test]
    fn pace_has_a_ceiling() {
        assert_eq!(pace_of(0).unwrap(), std::time::Duration::ZERO);
        assert_eq!(
            pace_of(MAX_PACE_MS).unwrap(),
            std::time::Duration::from_millis(MAX_PACE_MS)
        );
        assert!(pace_of(MAX_PACE_MS + 1).is_err());
        assert!(pace_of(u64::MAX).is_err());
    }

    #[tokio::test]
    async fn input_returns_503_for_a_disconnected_interface() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let dir = std::env::temp_dir().join(format!(
            "serialcap-http-disconnected-{}",
            std::process::id()
        ));
        let app = router(
            AppState {
                serials: Serials::spawn_all(
                    &[crate::serial_io::InterfaceSpec {
                        name: "console".into(),
                        device: dir.join("missing-device").to_string_lossy().into_owned(),
                        baud: 115200,
                        power_sense_signal: None,
                    }],
                    &dir,
                    100,
                ),
            },
            crate::auth::Auth::new("test-token".into(), &[]),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/input")
                    .header("Host", "127.0.0.1")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::from("reboot\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        std::fs::remove_dir_all(dir).ok();
    }
}
