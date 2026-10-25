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

//! Dashboard sessions: a single-use ticket in the URL, a cookie after it.
//!
//! `panio console` used to put the daemon token itself in the dashboard URL,
//! so the URL was a long-lived credential that could never be printed (#196,
//! #293). Now the CLI mints a **ticket** with `POST /ticket` (bearer token
//! only), and the URL carries just that. `GET /?ticket=…` consumes it, sets an
//! HttpOnly, SameSite=Strict session cookie and redirects to the same page
//! without the ticket. A used or expired ticket is worth nothing, so the URL
//! can be printed.
//!
//! The cookie stands in for the token only on a **same-origin** request: an
//! `Origin`, when there is one, must name this daemon's own `Host`, and a
//! request that is not a GET or HEAD must carry one. That matters because a
//! browser treats every port on 127.0.0.1 as the same site, so SameSite alone
//! would let a page served from another loopback port send the cookie, and the
//! auth layer accepts any loopback `Origin`.
//!
//! This sits *outside* `auth.rs` (which is byte-identical across four crates):
//! a good cookie or ticket becomes an `Authorization: Bearer` header on the
//! request, and the auth layer then runs its Host, Origin and token checks as
//! usual.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};

/// How long a minted ticket stays redeemable.
pub const TICKET_TTL: Duration = Duration::from_secs(120);

/// Unredeemed tickets kept at once; minting past this drops the oldest.
const MAX_TICKETS: usize = 16;

/// Open sessions kept at once; a new one past this ends the oldest, whose
/// page then gets 401s until `panio console` is run again.
const MAX_SESSIONS: usize = 32;

/// Cookie names start with this; the rest is the port the browser used, so
/// two daemons on 127.0.0.1 (one per target) do not overwrite each other's
/// cookie: browsers scope cookies by host, not port.
const COOKIE_PREFIX: &str = "panio_hdmicap_";

/// Tickets and sessions for one daemon, plus the token a good one stands for.
pub struct Sessions {
    token: String,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Ticket → when it expires.
    tickets: HashMap<String, Instant>,
    /// Session ids, oldest first.
    sessions: VecDeque<String>,
}

/// Marks a request authenticated by the session cookie rather than a bearer
/// token, so routes that hand out credentials can refuse it.
#[derive(Clone, Copy)]
pub struct CookieAuth;

impl Sessions {
    /// Sessions that authorize requests as `token` (the daemon token).
    pub fn new(token: String) -> Arc<Sessions> {
        Arc::new(Sessions {
            token,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// A fresh single-use ticket, valid for [`TICKET_TTL`].
    pub fn mint(&self) -> anyhow::Result<String> {
        let ticket = crate::auth::generate_token()?;
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        g.tickets.retain(|_, exp| *exp > now);
        while g.tickets.len() >= MAX_TICKETS {
            let oldest = g
                .tickets
                .iter()
                .min_by_key(|(_, exp)| **exp)
                .map(|(t, _)| t.clone());
            match oldest {
                Some(t) => g.tickets.remove(&t),
                None => break,
            };
        }
        g.tickets.insert(ticket.clone(), now + TICKET_TTL);
        Ok(ticket)
    }

    /// Consume `ticket`. True only the first time, and only before it expires.
    fn redeem(&self, ticket: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        // Look up by constant-time comparison: the map key would compare
        // byte by byte.
        let found = g
            .tickets
            .keys()
            .find(|t| crate::auth::ct_eq(t.as_bytes(), ticket.as_bytes()))
            .cloned();
        match found.and_then(|t| g.tickets.remove(&t)) {
            Some(exp) => exp > Instant::now(),
            None => false,
        }
    }

    fn open_session(&self) -> anyhow::Result<String> {
        let id = crate::auth::generate_token()?;
        let mut g = self.inner.lock().unwrap();
        while g.sessions.len() >= MAX_SESSIONS {
            g.sessions.pop_front();
        }
        g.sessions.push_back(id.clone());
        Ok(id)
    }

    fn is_session(&self, id: &str) -> bool {
        let g = self.inner.lock().unwrap();
        g.sessions
            .iter()
            .any(|s| crate::auth::ct_eq(s.as_bytes(), id.as_bytes()))
    }
}

/// `POST /ticket`: mint a dashboard ticket. Bearer token only — a page
/// authenticated by its cookie may not mint more credentials.
pub async fn mint_handler(
    Extension(sessions): Extension<Arc<Sessions>>,
    cookie: Option<Extension<CookieAuth>>,
    Extension(proxied): Extension<Proxied>,
) -> Response {
    if cookie.is_some() {
        return (
            StatusCode::FORBIDDEN,
            "forbidden: tickets are minted with the daemon token only\n",
        )
            .into_response();
    }
    match sessions.mint() {
        Ok(ticket) => Json(serde_json::json!({
            "ticket": ticket,
            "expires_in": TICKET_TTL.as_secs(),
            "serial": proxied.serial,
            "hid": proxied.hid,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
    }
}

/// Which other daemons this one proxies for the dashboard, as reported to the
/// CLI with each ticket so it can tell the page what to expect.
#[derive(Clone, Copy, Default)]
pub struct Proxied {
    pub serial: bool,
    pub hid: bool,
}

/// The middleware. Apply it *outside* the auth layer (added after it), so it
/// runs first.
pub async fn layer(
    State(sessions): State<Arc<Sessions>>,
    mut req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/" {
        if let Some(ticket) = query_param(req.uri().query(), "ticket") {
            return redeem(&sessions, ticket.to_string(), req, next).await;
        }
    }
    if req.headers().get(header::AUTHORIZATION).is_none()
        && same_origin(&req)
        && cookie_session(&req).any(|id| sessions.is_session(id))
    {
        authorize(&mut req, &sessions.token);
        req.extensions_mut().insert(CookieAuth);
    }
    next.run(req).await
}

async fn redeem(sessions: &Sessions, ticket: String, mut req: Request, next: Next) -> Response {
    if !sessions.redeem(&ticket) {
        return (
            StatusCode::UNAUTHORIZED,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "This dashboard link has expired or was already used.\n\
             Run `panio console <target>` again for a fresh one.\n",
        )
            .into_response();
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let location = without_param(req.uri().query(), "ticket");
    // Let the auth layer judge Host and Origin as for any request; the
    // session is opened only if it lets this one through.
    authorize(&mut req, &sessions.token);
    let resp = next.run(req).await;
    if !resp.status().is_success() {
        return resp;
    }
    let id = match sessions.open_session() {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
    };
    let cookie = format!(
        "{}={id}; Path=/; HttpOnly; SameSite=Strict",
        cookie_name(&host)
    );
    let mut out = StatusCode::SEE_OTHER.into_response();
    let h = out.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&location) {
        h.insert(header::LOCATION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        h.insert(header::SET_COOKIE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    out
}

fn authorize(req: &mut Request, token: &str) {
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        req.headers_mut().insert(header::AUTHORIZATION, v);
    }
}

/// `panio_hdmicap_<port>`, the port taken from the `Host` the browser sent
/// (the tunnel's local port, through an SSH forward).
fn cookie_name(host: &str) -> String {
    let port = host
        .rsplit_once(':')
        .map(|(_, p)| p)
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or("0");
    format!("{COOKIE_PREFIX}{port}")
}

/// The values of every `panio_hdmicap_*` cookie on the request. Any of them
/// may be this daemon's: the browser sends all of 127.0.0.1's cookies to
/// every port.
fn cookie_session(req: &Request) -> impl Iterator<Item = &str> {
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| name.starts_with(COOKIE_PREFIX))
        .map(|(_, value)| value)
}

/// Whether the request comes from this daemon's own origin: an `Origin`, when
/// present, must be `http(s)://<Host>`, and anything but GET/HEAD must send
/// one. Without an Origin, a GET is a navigation or a plain resource load,
/// whose response a page on another origin cannot read.
fn same_origin(req: &Request) -> bool {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    match req.headers().get(header::ORIGIN) {
        Some(origin) => {
            let (Some(host), Ok(origin)) = (host, origin.to_str()) else {
                return false;
            };
            let authority = origin
                .strip_prefix("http://")
                .or_else(|| origin.strip_prefix("https://"));
            authority.is_some_and(|a| a.eq_ignore_ascii_case(host))
        }
        None => matches!(*req.method(), Method::GET | Method::HEAD),
    }
}

fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(k, _)| *k == name)
            .map(|(_, v)| v)
    })
}

/// `/` plus `query` with every `name=` pair removed.
fn without_param(query: Option<&str>, name: &str) -> String {
    let rest: Vec<&str> = query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty() && p.split_once('=').map_or(*p, |(k, _)| k) != name)
        .collect();
    if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", rest.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::Request as HttpRequest,
        routing::{get, post},
        Router,
    };
    use tower::ServiceExt;

    const TOKEN: &str = "tok";

    /// The real layering: session outside auth, a page at `/`, a data route,
    /// a POST, and the ticket route.
    fn app(sessions: Arc<Sessions>) -> Router {
        Router::new()
            .route("/", get(|| async { "page" }))
            .route("/status", get(|| async { "status" }))
            .route("/power-cycle", post(|| async { "cycled" }))
            .route("/ticket", post(mint_handler))
            .layer(axum::middleware::from_fn_with_state(
                crate::auth::Auth::new(TOKEN.into(), &[]),
                crate::auth::require,
            ))
            .layer(axum::middleware::from_fn_with_state(
                sessions.clone(),
                layer,
            ))
            .layer(Extension(sessions))
            .layer(Extension(Proxied {
                serial: true,
                hid: false,
            }))
    }

    fn get_req(uri: &str) -> axum::http::request::Builder {
        HttpRequest::builder()
            .uri(uri)
            .header(header::HOST, "127.0.0.1:4000")
    }

    async fn call(s: &Arc<Sessions>, req: HttpRequest<Body>) -> Response {
        app(s.clone()).oneshot(req).await.unwrap()
    }

    /// Redeem a fresh ticket and return the `name=value` part of the cookie.
    async fn login(s: &Arc<Sessions>) -> String {
        let t = s.mint().unwrap();
        let resp = call(
            s,
            get_req(&format!("/?ticket={t}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let c = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        c.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn ticket_sets_cookie_and_redirects_without_it() {
        let s = Sessions::new(TOKEN.into());
        let t = s.mint().unwrap();
        let resp = call(
            &s,
            get_req(&format!("/?ticket={t}&serial=proxy&interface=console"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers()[header::LOCATION],
            "/?serial=proxy&interface=console"
        );
        let cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with("panio_hdmicap_4000="), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        // The cookie is a session id, never the daemon token.
        assert!(!cookie.contains(TOKEN), "{cookie}");
    }

    #[tokio::test]
    async fn ticket_is_single_use() {
        let s = Sessions::new(TOKEN.into());
        let t = s.mint().unwrap();
        let first = call(
            &s,
            get_req(&format!("/?ticket={t}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        let again = call(
            &s,
            get_req(&format!("/?ticket={t}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
        assert!(again.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn expired_or_unknown_ticket_is_refused() {
        let s = Sessions::new(TOKEN.into());
        let t = s.mint().unwrap();
        s.inner
            .lock()
            .unwrap()
            .tickets
            .insert(t.clone(), Instant::now());
        let resp = call(
            &s,
            get_req(&format!("/?ticket={t}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = call(&s, get_req("/?ticket=nope").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ticket_on_a_foreign_host_opens_no_session() {
        let s = Sessions::new(TOKEN.into());
        let t = s.mint().unwrap();
        let resp = call(
            &s,
            HttpRequest::builder()
                .uri(format!("/?ticket={t}"))
                .header(header::HOST, "evil.example:4000")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
        assert!(s.inner.lock().unwrap().sessions.is_empty());
    }

    #[tokio::test]
    async fn cookie_authorizes_same_origin_requests() {
        let s = Sessions::new(TOKEN.into());
        let cookie = login(&s).await;
        // A plain GET (an <img>, a navigation): no Origin.
        let resp = call(
            &s,
            get_req("/status")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        // The page's own POST, with its own Origin.
        let resp = call(
            &s,
            get_req("/power-cycle")
                .method("POST")
                .header(header::ORIGIN, "http://127.0.0.1:4000")
                .header(header::COOKIE, format!("other=1; {cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cookie_is_refused_from_another_loopback_origin() {
        let s = Sessions::new(TOKEN.into());
        let cookie = login(&s).await;
        // Same site as far as SameSite goes (another port on 127.0.0.1).
        let resp = call(
            &s,
            get_req("/power-cycle")
                .method("POST")
                .header(header::ORIGIN, "http://127.0.0.1:5555")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // A POST without any Origin does not ride on the cookie either.
        let resp = call(
            &s,
            get_req("/power-cycle")
                .method("POST")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_cookie_is_unauthorized() {
        let s = Sessions::new(TOKEN.into());
        let resp = call(
            &s,
            get_req("/status")
                .header(header::COOKIE, "panio_hdmicap_4000=forged")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ticket_route_needs_the_bearer_token() {
        let s = Sessions::new(TOKEN.into());
        let resp = call(
            &s,
            get_req("/ticket")
                .method("POST")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["expires_in"], 120);
        assert_eq!(v["serial"], true);
        assert_eq!(v["hid"], false);
        let ticket = v["ticket"].as_str().unwrap();
        assert!(s.redeem(ticket));

        // The page's cookie cannot mint more.
        let cookie = login(&s).await;
        let resp = call(
            &s,
            get_req("/ticket")
                .method("POST")
                .header(header::ORIGIN, "http://127.0.0.1:4000")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn oldest_session_is_dropped_past_the_cap() {
        let s = Sessions::new(TOKEN.into());
        let first = s.open_session().unwrap();
        for _ in 0..MAX_SESSIONS {
            s.open_session().unwrap();
        }
        assert!(!s.is_session(&first));
        assert_eq!(s.inner.lock().unwrap().sessions.len(), MAX_SESSIONS);
    }

    #[test]
    fn query_helpers() {
        assert_eq!(query_param(Some("a=1&ticket=xyz"), "ticket"), Some("xyz"));
        assert_eq!(query_param(Some("noticket=1"), "ticket"), None);
        assert_eq!(without_param(Some("ticket=x"), "ticket"), "/");
        assert_eq!(
            without_param(Some("ticket=x&hid=proxy"), "ticket"),
            "/?hid=proxy"
        );
        assert_eq!(cookie_name("127.0.0.1:8080"), "panio_hdmicap_8080");
        assert_eq!(cookie_name("localhost"), "panio_hdmicap_0");
        assert_eq!(cookie_name("[::1]:9"), "panio_hdmicap_9");
    }
}
