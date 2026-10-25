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

//! The dashboard's way to the target's serial and hid daemons (#293).
//!
//! The page talks only to this daemon, authenticated by its session cookie
//! (`session.rs`), and never sees another daemon's token. Each proxied route
//! reads the other daemon's discovery file (named by an env var the CLI sets)
//! on every request, so a restarted daemon on a new port with a new token is
//! found again, and connects to it over loopback with `Authorization: Bearer`.
//!
//! | Route | Upstream |
//! |---|---|
//! | `GET /proxy/serial/stream[?interface=NAME]` (WebSocket) | serialcap `/stream` |
//! | `GET /proxy/serial/interfaces` | serialcap `/interfaces` |
//! | `GET /proxy/hid` (WebSocket) | the hid daemon's `/hid` |
//!
//! Only text and binary messages cross; each side answers its own pings. When
//! either side closes, both do, and the page reconnects as it would to the
//! daemon itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{
        ws::{Message as AxMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use crate::server::AppState;

/// Env var naming the target's serialcap discovery file (`daemon.json`).
pub const ENV_SERIAL_DISCOVERY: &str = "HDMICAP_SERIAL_DISCOVERY";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the other daemons publish their discovery files. `None`: the CLI did
/// not name one, and that leg of the dashboard is absent.
#[derive(Clone, Default)]
pub struct Upstreams {
    pub serial: Option<PathBuf>,
    pub hid: Option<PathBuf>,
}

impl Upstreams {
    /// From [`ENV_SERIAL_DISCOVERY`] and `HDMICAP_HID_DISCOVERY`.
    pub fn from_env() -> Upstreams {
        let path = |var: &str| {
            std::env::var_os(var)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        };
        Upstreams {
            serial: path(ENV_SERIAL_DISCOVERY),
            hid: path(crate::hid_link::ENV_HID_DISCOVERY),
        }
    }

    pub fn proxied(&self) -> crate::session::Proxied {
        crate::session::Proxied {
            serial: self.serial.is_some(),
            hid: self.hid.is_some(),
        }
    }
}

pub type Ws = WebSocketStream<TcpStream>;

/// Open `path` (with its query) as a WebSocket on the daemon whose discovery
/// file is `discovery`, with its token as a bearer header.
pub async fn upstream_ws(discovery: &Path, path: &str) -> Result<Ws> {
    let (port, token) = crate::rfb::read_discovery(discovery)?;
    let url = format!("ws://127.0.0.1:{port}{path}");
    let mut req = url.as_str().into_client_request()?;
    if let Some(t) = token {
        req.headers_mut()
            .insert("Authorization", format!("Bearer {t}").parse()?);
    }
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("connecting to the daemon on port {port}"))?;
    let _ = tcp.set_nodelay(true);
    let (ws, _) = tokio_tungstenite::client_async(req, tcp)
        .await
        .map_err(|e| anyhow!("daemon websocket handshake: {e}"))?;
    Ok(ws)
}

fn not_configured(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("this daemon was not started with a {what} daemon to proxy\n"),
    )
        .into_response()
}

fn bad_gateway(e: anyhow::Error) -> Response {
    debug!("proxy: {e:#}");
    (StatusCode::BAD_GATEWAY, format!("{e:#}\n")).into_response()
}

/// Connect upstream first, so a daemon that is down is a 502 the page can see
/// (its WebSocket just fails and retries), then accept the browser's upgrade.
async fn proxy_ws(ws: WebSocketUpgrade, discovery: &Path, path: &str) -> Response {
    match tokio::time::timeout(CONNECT_TIMEOUT, upstream_ws(discovery, path)).await {
        Ok(Ok(up)) => ws.on_upgrade(move |client| pump(client, up)),
        Ok(Err(e)) => bad_gateway(e),
        Err(_) => bad_gateway(anyhow!("timed out connecting to the daemon")),
    }
}

async fn pump(client: WebSocket, upstream: Ws) {
    let (mut c_tx, mut c_rx) = client.split();
    let (mut u_tx, mut u_rx) = upstream.split();
    let up = async {
        while let Some(Ok(m)) = c_rx.next().await {
            let m = match m {
                AxMessage::Text(t) => TMessage::Text(t),
                AxMessage::Binary(b) => TMessage::Binary(b),
                AxMessage::Close(_) => break,
                _ => continue,
            };
            if u_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = u_tx.close().await;
    };
    let down = async {
        while let Some(Ok(m)) = u_rx.next().await {
            let m = match m {
                TMessage::Text(t) => AxMessage::Text(t),
                TMessage::Binary(b) => AxMessage::Binary(b),
                TMessage::Close(_) => break,
                _ => continue,
            };
            if c_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = c_tx.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

/// `GET /proxy/serial/stream`: serialcap's `/stream`, `interface` passed on.
/// Nothing else in the query crosses — in particular not this daemon's
/// `token=`.
pub async fn serial_stream(
    State(s): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(discovery) = s.upstreams.serial.clone() else {
        return not_configured("serial");
    };
    let path = match q.get("interface") {
        Some(i) => format!("/stream?interface={}", query_escape(i)),
        None => "/stream".to_string(),
    };
    proxy_ws(ws, &discovery, &path).await
}

/// `GET /proxy/hid`: the hid daemon's `/hid`.
pub async fn hid(State(s): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(discovery) = s.upstreams.hid.clone() else {
        return not_configured("hid");
    };
    proxy_ws(ws, &discovery, "/hid").await
}

/// `GET /proxy/serial/interfaces`: serialcap's interface list, as JSON.
pub async fn serial_interfaces(State(s): State<AppState>) -> Response {
    let Some(discovery) = s.upstreams.serial.clone() else {
        return not_configured("serial");
    };
    let fetched = tokio::task::spawn_blocking(move || -> Result<String> {
        let (port, token) = crate::rfb::read_discovery(&discovery)?;
        let mut req =
            ureq::get(&format!("http://127.0.0.1:{port}/interfaces")).timeout(CONNECT_TIMEOUT);
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        Ok(req.call()?.into_string()?)
    })
    .await;
    match fetched {
        Ok(Ok(body)) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Ok(Err(e)) => bad_gateway(e),
        Err(e) => bad_gateway(anyhow!("{e}")),
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::ws::WebSocketUpgrade, routing::get, Router};
    use std::sync::{Arc, Mutex};

    const UP_TOKEN: &str = "upstream-token";

    /// A stand-in for serialcap/hid: `/stream` and `/hid` echo with a prefix
    /// that records the query, `/interfaces` returns a list, all behind the
    /// real auth layer with its own token.
    async fn fake_upstream(seen: Arc<Mutex<Vec<String>>>) -> u16 {
        async fn echo(ws: WebSocketUpgrade, q: Option<String>) -> Response {
            ws.on_upgrade(move |mut sock| async move {
                while let Some(Ok(AxMessage::Text(t))) = sock.recv().await {
                    let reply = format!("{}:{t}", q.clone().unwrap_or_default());
                    if sock.send(AxMessage::Text(reply)).await.is_err() {
                        break;
                    }
                }
            })
        }
        let app = Router::new()
            .route(
                "/stream",
                get(move |ws: WebSocketUpgrade, uri: axum::http::Uri| {
                    seen.lock().unwrap().push(uri.to_string());
                    echo(ws, uri.query().map(str::to_string))
                }),
            )
            .route(
                "/hid",
                get(|ws: WebSocketUpgrade| echo(ws, Some("hid".into()))),
            )
            .route("/interfaces", get(|| async { r#"[{"name":"console"}]"# }))
            .layer(axum::middleware::from_fn_with_state(
                crate::auth::Auth::new(UP_TOKEN.into(), &[]),
                crate::auth::require,
            ));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        port
    }

    fn discovery_file(name: &str, port: u16) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("panio-proxy-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        let body = serde_json::json!({
            "pid": std::process::id(), "port": port, "token": UP_TOKEN,
        });
        std::fs::write(&path, body.to_string()).unwrap();
        path
    }

    /// hdmicap's real router, served on a real port, with the page's
    /// session cookie.
    async fn hdmicap(upstreams: Upstreams) -> (u16, String) {
        let (_tx, rx) =
            tokio::sync::watch::channel(Arc::new(crate::frame::FrameState::no_device()));
        let sessions = crate::session::Sessions::new("hdmi-token".into());
        let state = AppState::new(rx)
            .with_sessions(sessions.clone())
            .with_upstreams(upstreams);
        let app = crate::server::router(
            state,
            crate::auth::Auth::new("hdmi-token".into(), crate::server::PUBLIC_ASSETS),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let ticket = sessions.mint().unwrap();
        let resp = ureq::AgentBuilder::new()
            .redirects(0)
            .build()
            .get(&format!("http://127.0.0.1:{port}/?ticket={ticket}"))
            .call()
            .unwrap();
        assert_eq!(resp.status(), 303);
        let cookie = resp.header("set-cookie").unwrap();
        (port, cookie.split(';').next().unwrap().to_string())
    }

    async fn page_ws(port: u16, path: &str, cookie: &str) -> Ws {
        let mut req = format!("ws://127.0.0.1:{port}{path}")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert("Cookie", cookie.parse().unwrap());
        req.headers_mut().insert(
            "Origin",
            format!("http://127.0.0.1:{port}").parse().unwrap(),
        );
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tokio_tungstenite::client_async(req, tcp).await.unwrap().0
    }

    async fn roundtrip(ws: &mut Ws, text: &str) -> String {
        ws.send(TMessage::Text(text.into())).await.unwrap();
        match ws.next().await.unwrap().unwrap() {
            TMessage::Text(t) => t,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serial_and_hid_websockets_cross_with_the_upstream_token() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let up = fake_upstream(seen.clone()).await;
        let (port, cookie) = hdmicap(Upstreams {
            serial: Some(discovery_file("serial", up)),
            hid: Some(discovery_file("hid", up)),
        })
        .await;

        let mut ws = page_ws(
            port,
            "/proxy/serial/stream?interface=a%20b&token=x",
            &cookie,
        )
        .await;
        assert_eq!(roundtrip(&mut ws, "ls").await, "interface=a%20b:ls");
        // Only the interface crossed, not the page's query token.
        assert_eq!(seen.lock().unwrap().as_slice(), ["/stream?interface=a%20b"]);

        let mut ws = page_ws(port, "/proxy/hid", &cookie).await;
        assert_eq!(roundtrip(&mut ws, "down A").await, "hid:down A");

        let body = tokio::task::spawn_blocking(move || {
            ureq::get(&format!("http://127.0.0.1:{port}/proxy/serial/interfaces"))
                .set("Cookie", &cookie)
                .call()
                .unwrap()
                .into_string()
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(body, r#"[{"name":"console"}]"#);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_needs_the_page_session() {
        let up = fake_upstream(Arc::default()).await;
        let (port, _cookie) = hdmicap(Upstreams {
            serial: Some(discovery_file("nosession", up)),
            hid: None,
        })
        .await;
        let status = tokio::task::spawn_blocking(move || {
            match ureq::get(&format!("http://127.0.0.1:{port}/proxy/serial/interfaces")).call() {
                Err(ureq::Error::Status(code, _)) => code,
                other => panic!("expected an error status, got {other:?}"),
            }
        })
        .await
        .unwrap();
        assert_eq!(status, 401);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_upstream_is_reported() {
        let (port, cookie) = hdmicap(Upstreams {
            serial: None,
            hid: Some(PathBuf::from("/nonexistent/daemon.json")),
        })
        .await;
        let statuses = tokio::task::spawn_blocking(move || {
            ["/proxy/serial/interfaces", "/proxy/hid"].map(|p| {
                match ureq::get(&format!("http://127.0.0.1:{port}{p}"))
                    .set("Cookie", &cookie)
                    .set("Connection", "Upgrade")
                    .set("Upgrade", "websocket")
                    .set("Sec-WebSocket-Version", "13")
                    .set("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .call()
                {
                    Err(ureq::Error::Status(code, _)) => code,
                    other => panic!("{p}: expected an error status, got {other:?}"),
                }
            })
        })
        .await
        .unwrap();
        // No serial daemon configured; hid configured but not running.
        assert_eq!(statuses, [404, 502]);
    }

    #[test]
    fn escape_keeps_unreserved_only() {
        assert_eq!(query_escape("con-sole_1.~"), "con-sole_1.~");
        assert_eq!(query_escape("a b&c"), "a%20b%26c");
    }
}
