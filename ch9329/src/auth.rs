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

//! Request authentication for the daemon's localhost HTTP API.
//!
//! Binding 127.0.0.1 keeps other *machines* out, not other *origins*: any web
//! page open in the operator's browser can POST to a loopback port or open a
//! WebSocket to it, and a DNS-rebinding page can do so under a name that looks
//! like its own. Every request therefore passes three checks, in this order:
//!
//! 1. **Host** must name a loopback address (defeats DNS rebinding).
//! 2. **Origin**, when the browser sends one, must be a loopback origin too.
//!    Browsers attach it to every cross-origin fetch and every WebSocket
//!    upgrade, so this alone shuts a hostile page out.
//! 3. **Token**: the request carries the secret this daemon generated at
//!    start, either as `Authorization: Bearer <token>` (the CLI and helper
//!    one-shots) or as a `token=<token>` query parameter (the dashboard's
//!    WebSockets and image loads, which cannot set headers). The token is
//!    published in `daemon.json`, written owner-only under the per-uid runtime
//!    dir, so only the operator's own processes can read it.
//!
//! Only the vendored static assets the dashboard loads by bare path are exempt
//! from the token check — they are public library files. `Access-Control-
//! Allow-Origin` is never `*`: an allowed Origin is echoed back, with `Vary:
//! Origin`, so the dashboard's cross-port fetches still work.
//!
//! This file is kept byte-identical across serialcap, hdmicap, hidrig and
//! ch9329 (like `platform.rs`), so a crate may not use every item in it.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::debug;

/// The host names a loopback request may carry in `Host` and `Origin`.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// Bytes of entropy in a token; hex-encoded to twice as many characters.
const TOKEN_BYTES: usize = 32;

/// The shared state behind [`require`]: the token every request must present
/// and the paths that are served without one.
#[derive(Clone)]
pub struct Auth {
    token: Arc<String>,
    /// Request paths served without a token: vendored static assets only.
    public: &'static [&'static str],
}

impl Auth {
    pub fn new(token: String, public: &'static [&'static str]) -> Auth {
        Auth {
            token: Arc::new(token),
            public,
        }
    }
}

/// A fresh token from the OS entropy source, hex-encoded.
pub fn generate_token() -> anyhow::Result<String> {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buf)
        .map_err(|e| anyhow::anyhow!("generating the daemon token: {e}"))?;
    Ok(hex(&buf))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Constant-time byte comparison, so a wrong token cannot be sharpened one
/// byte at a time by timing the reply. The length is not secret.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Write `bytes` to `path` atomically — a temp file in the same directory,
/// then a rename — and, on Unix, readable by the owner only. For files that
/// hold a secret, such as the discovery file now that it carries the token.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    // The mode below applies only when the file is created, so never reuse a
    // leftover temp file whose mode is unknown.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// The middleware. Apply to the whole router with
/// `axum::middleware::from_fn_with_state(auth, auth::require)`.
pub async fn require(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    // 1. Host: present and loopback, or this is a rebinding attempt (or a
    //    proxy we never expected).
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(is_loopback_authority);
    if !host_ok {
        debug!("rejected request: Host is missing or not loopback");
        return (
            StatusCode::FORBIDDEN,
            "forbidden: this daemon answers only loopback Host names\n",
        )
            .into_response();
    }

    // 2. Origin: absent (CLI, same-origin page loads) or loopback.
    let origin = req.headers().get(header::ORIGIN).cloned();
    if let Some(o) = &origin {
        if !o.to_str().ok().is_some_and(is_loopback_origin) {
            debug!("rejected request: Origin is not a loopback origin");
            return (
                StatusCode::FORBIDDEN,
                "forbidden: cross-origin requests are not accepted\n",
            )
                .into_response();
        }
    }

    // 3. Token, unless the path is a public static asset.
    let public = auth.public.contains(&req.uri().path());
    if !public && !presents_token(&req, &auth.token) {
        debug!("rejected request: missing or wrong token for {}", req.uri());
        let resp = (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized: send the token from daemon.json as \
             `Authorization: Bearer <token>` or `?token=<token>`\n",
        )
            .into_response();
        return with_cors(resp, origin);
    }

    with_cors(next.run(req).await, origin)
}

/// Echo an allowed Origin back (never `*`), marking the response as varying on
/// it so a shared cache cannot hand one origin's reply to another.
fn with_cors(mut resp: Response, origin: Option<HeaderValue>) -> Response {
    if let Some(o) = origin {
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, o);
        resp.headers_mut()
            .append(header::VARY, HeaderValue::from_static("Origin"));
    }
    resp
}

/// Whether the request carries the token, as a bearer header or a `token=`
/// query parameter. Either form is accepted; both are compared in constant
/// time.
fn presents_token(req: &Request, token: &str) -> bool {
    let header_ok = bearer(req).is_some_and(|t| ct_eq(t.as_bytes(), token.as_bytes()));
    let query_ok =
        query_token(req.uri().query()).is_some_and(|t| ct_eq(t.as_bytes(), token.as_bytes()));
    header_ok || query_ok
}

fn bearer(req: &Request) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(rest.trim())
}

fn query_token(query: Option<&str>) -> Option<&str> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

/// The host part of a `host[:port]` authority. A bracketed IPv6 literal keeps
/// its brackets, which is how the loopback list spells `[::1]`.
fn host_part(authority: &str) -> &str {
    if authority.starts_with('[') {
        match authority.find(']') {
            Some(i) => &authority[..=i],
            None => authority,
        }
    } else {
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority)
    }
}

/// `Host` header check: the host part names a loopback address.
fn is_loopback_authority(authority: &str) -> bool {
    let host = host_part(authority.trim()).to_ascii_lowercase();
    LOOPBACK_HOSTS.contains(&host.as_str())
}

/// `Origin` header check: `http(s)://<loopback>[:port]` and nothing else —
/// `null` (a sandboxed or file: page) and every other scheme are rejected.
fn is_loopback_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    match rest {
        Some(authority) if !authority.is_empty() && !authority.contains('/') => {
            is_loopback_authority(authority)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, routing::get, Router};
    use tower::ServiceExt;

    const TOKEN: &str = "3f1c9b0e7a5d2c4f8e6b1a0d9c7e5f3a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e";

    /// A tiny router behind the layer: one protected route and one public
    /// asset, as the dashboard's page and its vendored xterm.js are.
    fn app() -> Router {
        Router::new()
            .route("/ping", get(|| async { "pong" }))
            .route("/xterm.js", get(|| async { "js" }))
            .layer(axum::middleware::from_fn_with_state(
                Auth::new(TOKEN.to_string(), &["/xterm.js"]),
                require,
            ))
    }

    async fn call(req: HttpRequest<Body>) -> Response {
        app().oneshot(req).await.unwrap()
    }

    /// A request as the CLI would send it: loopback Host, no Origin.
    fn local(uri: &str) -> axum::http::request::Builder {
        HttpRequest::builder()
            .uri(uri)
            .header(header::HOST, "127.0.0.1:1")
    }

    fn bearer_value() -> String {
        format!("Bearer {TOKEN}")
    }

    #[tokio::test]
    async fn no_token_is_unauthorized() {
        let resp = call(local("/ping").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }

    #[tokio::test]
    async fn bearer_header_is_accepted() {
        let resp = call(
            local("/ping")
                .header(header::AUTHORIZATION, bearer_value())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn query_token_is_accepted() {
        let resp = call(
            local(&format!("/ping?token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Also when it is not the first parameter.
        let resp = call(
            local(&format!("/ping?interface=console&token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_token_is_unauthorized() {
        let resp = call(
            local("/ping")
                .header(header::AUTHORIZATION, "Bearer deadbeef")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = call(local("/ping?token=deadbeef").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // A wrong header is not rescued by an absent query token, and a
        // Basic credential is not a bearer token.
        let resp = call(
            local("/ping")
                .header(header::AUTHORIZATION, format!("Basic {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_host_is_forbidden() {
        let resp = call(
            HttpRequest::builder()
                .uri("/ping")
                .header(header::AUTHORIZATION, bearer_value())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn foreign_host_is_forbidden() {
        // DNS rebinding: a name the attacker controls resolves to 127.0.0.1;
        // the token is even right, and it still must not get through.
        let resp = call(
            HttpRequest::builder()
                .uri("/ping")
                .header(header::HOST, "evil.example:1")
                .header(header::AUTHORIZATION, bearer_value())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn foreign_origin_is_forbidden_even_with_token() {
        let resp = call(
            local("/ping")
                .header(header::ORIGIN, "http://evil.example")
                .header(header::AUTHORIZATION, bearer_value())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn loopback_origin_is_echoed_exactly() {
        let resp = call(
            local("/ping")
                .header(header::ORIGIN, "http://127.0.0.1:5555")
                .header(header::AUTHORIZATION, bearer_value())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "http://127.0.0.1:5555"
        );
        let vary: Vec<_> = resp.headers().get_all(header::VARY).iter().collect();
        assert!(vary.iter().any(|v| *v == "Origin"), "Vary: {vary:?}");
    }

    #[tokio::test]
    async fn null_origin_is_forbidden() {
        let resp = call(
            local("/ping")
                .header(header::ORIGIN, "null")
                .header(header::AUTHORIZATION, bearer_value())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn public_asset_needs_no_token() {
        let resp = call(local("/xterm.js").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        // But the Host check still applies to it.
        let resp = call(
            HttpRequest::builder()
                .uri("/xterm.js")
                .header(header::HOST, "evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn ct_eq_compares_whole_values() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
    }

    #[test]
    fn host_and_origin_parsing() {
        assert!(is_loopback_authority("127.0.0.1"));
        assert!(is_loopback_authority("127.0.0.1:8724"));
        assert!(is_loopback_authority("localhost:1"));
        assert!(is_loopback_authority("LOCALHOST"));
        assert!(is_loopback_authority("[::1]:443"));
        assert!(!is_loopback_authority("127.0.0.1.evil.example:80"));
        assert!(!is_loopback_authority("localhost.evil.example"));
        assert!(!is_loopback_authority("::1"));
        assert!(!is_loopback_authority(""));

        assert!(is_loopback_origin("http://localhost:9999"));
        assert!(is_loopback_origin("https://[::1]"));
        assert!(is_loopback_origin("http://127.0.0.1"));
        assert!(!is_loopback_origin("null"));
        assert!(!is_loopback_origin("ws://127.0.0.1"));
        assert!(!is_loopback_origin("http://127.0.0.1/path"));
        assert!(!is_loopback_origin("http://"));
        assert!(!is_loopback_origin("127.0.0.1"));
    }

    #[test]
    fn generate_token_is_hex_and_fresh() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn write_private_file_is_owner_only_and_replaces_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "panio-auth-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t").len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        write_private_file(&path, b"{\"pid\":1}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"pid\":1}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        }
        // A rewrite replaces the file and leaves no temp file behind.
        write_private_file(&path, b"{\"pid\":2}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"pid\":2}");
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
