// Copyright 2026 The Panio Authors
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

#![allow(clippy::result_large_err)]

//! Tests drive the real client against an in-process fake RFB server over
//! both transports, so they execute the handshake, the reassembly of a byte
//! stream split at arbitrary points, and the reconnect path.

use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

use super::*;
use crate::capture::DeviceSpec;

// ── message builders (server -> client) ──────────────────────────────────────

fn rect_head(x: u16, y: u16, w: u16, h: u16, enc: i32) -> Vec<u8> {
    let mut v = Vec::new();
    for n in [x, y, w, h] {
        v.extend_from_slice(&n.to_be_bytes());
    }
    v.extend_from_slice(&enc.to_be_bytes());
    v
}

/// A Raw rectangle of one solid RGB colour.
fn solid(x: u16, y: u16, w: u16, h: u16, rgb: [u8; 3]) -> Vec<u8> {
    let mut v = rect_head(x, y, w, h, 0);
    for _ in 0..(w as usize * h as usize) {
        v.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
    }
    v
}

fn copy(x: u16, y: u16, w: u16, h: u16, sx: u16, sy: u16) -> Vec<u8> {
    let mut v = rect_head(x, y, w, h, 1);
    v.extend_from_slice(&sx.to_be_bytes());
    v.extend_from_slice(&sy.to_be_bytes());
    v
}

fn desktop_size(w: u16, h: u16) -> Vec<u8> {
    rect_head(0, 0, w, h, -223)
}

fn update(rects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut v = vec![0, 0];
    v.extend_from_slice(&(rects.len() as u16).to_be_bytes());
    for r in rects {
        v.extend(r);
    }
    v
}

// ── the fake server ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum After {
    /// Drop the connection once the script is exhausted.
    Close,
    /// Keep answering nothing but keep reading (logs keepalives).
    Idle,
    /// Stop reading and writing, but stay connected.
    Silent,
}

#[derive(Clone)]
struct Script {
    w: u16,
    h: u16,
    security: Vec<u8>,
    /// One entry is sent in answer to each update request.
    updates: Vec<Vec<u8>>,
    after: After,
    /// Split every write into pieces this big (0 = whole).
    chunk: usize,
}

impl Script {
    fn new(w: u16, h: u16, updates: Vec<Vec<u8>>) -> Script {
        Script {
            w,
            h,
            security: vec![1],
            updates,
            after: After::Idle,
            chunk: 0,
        }
    }
}

#[derive(Default)]
struct Log {
    connections: usize,
    pixel_formats: Vec<Vec<u8>>,
    encodings: Vec<Vec<i32>>,
    /// (incremental, width, height) of every update request.
    requests: Vec<(bool, u16, u16)>,
    auth: Vec<Option<String>>,
    subprotocols: Vec<Option<String>>,
}

struct Fake {
    addr: SocketAddr,
    log: Arc<Mutex<Log>>,
}

impl Fake {
    /// `token`: Some(t) makes the WebSocket upgrade demand `Bearer t`.
    fn start(ws: bool, token: Option<&str>, scripts: Vec<Script>) -> Fake {
        let log = Arc::new(Mutex::new(Log::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let l = log.clone();
        let token = token.map(str::to_string);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let mut n = 0;
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let script = scripts.get(n).cloned();
                    n += 1;
                    l.lock().unwrap().connections = n;
                    let (l, token) = (l.clone(), token.clone());
                    tokio::spawn(async move {
                        let Some(script) = script else { return };
                        let t = if ws {
                            match accept_ws(tcp, token, l.clone()).await {
                                Some(t) => t,
                                None => return,
                            }
                        } else {
                            Transport::Tcp(tcp)
                        };
                        let _ = serve(Conn::new(t), script, l).await;
                    });
                }
            });
        });
        Fake {
            addr: rx.recv().unwrap(),
            log,
        }
    }

    fn tcp_target(&self) -> RfbTarget {
        RfbTarget::Tcp {
            addr: self.addr.to_string(),
        }
    }

    fn ws_target(&self, token: Option<&str>) -> RfbTarget {
        RfbTarget::Ws {
            url: format!("ws://{}/rfb", self.addr),
            token: token.map(str::to_string),
        }
    }
}

async fn accept_ws(
    tcp: TcpStream,
    token: Option<String>,
    log: Arc<Mutex<Log>>,
) -> Option<Transport> {
    let cb = move |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        let get = |n: &str| {
            req.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let auth = get("authorization");
        {
            let mut l = log.lock().unwrap();
            l.auth.push(auth.clone());
            l.subprotocols.push(get("sec-websocket-protocol"));
        }
        if let Some(t) = &token {
            if auth.as_deref() != Some(&format!("Bearer {t}")) {
                return Err(tungstenite::http::Response::builder()
                    .status(401)
                    .body(Some("unauthorized".to_string()))
                    .unwrap());
            }
        }
        resp.headers_mut()
            .insert("Sec-WebSocket-Protocol", "binary".parse().unwrap());
        Ok(resp)
    };
    let ws = tokio_tungstenite::accept_hdr_async(tcp, cb).await.ok()?;
    Some(Transport::Ws(Box::new(ws)))
}

async fn send_chunked(c: &mut Conn, bytes: &[u8], chunk: usize) -> Result<()> {
    if chunk == 0 {
        return c.send(bytes).await;
    }
    for piece in bytes.chunks(chunk) {
        c.send(piece).await?;
    }
    Ok(())
}

async fn serve(mut c: Conn, s: Script, log: Arc<Mutex<Log>>) -> Result<()> {
    c.send(b"RFB 003.008\n").await?;
    c.take(12).await?;
    let mut sec = vec![s.security.len() as u8];
    sec.extend_from_slice(&s.security);
    c.send(&sec).await?;
    if !s.security.contains(&1) {
        // Hold the line until the client gives up and hangs up.
        loop {
            c.fill().await?;
        }
    }
    c.take(1).await?; // chosen type
    c.send(&0u32.to_be_bytes()).await?; // SecurityResult OK
    c.take(1).await?; // ClientInit
    let mut init = Vec::new();
    init.extend_from_slice(&s.w.to_be_bytes());
    init.extend_from_slice(&s.h.to_be_bytes());
    init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    init.extend_from_slice(&4u32.to_be_bytes());
    init.extend_from_slice(b"fake");
    c.send(&init).await?;

    let mut next = 0;
    loop {
        let t = c.take(1).await?[0];
        match t {
            0 => {
                let m = c.take(19).await?;
                log.lock().unwrap().pixel_formats.push(m[3..19].to_vec());
            }
            2 => {
                let m = c.take(3).await?;
                let n = u16::from_be_bytes([m[1], m[2]]) as usize;
                let e = c.take(4 * n).await?;
                let encs = e
                    .chunks(4)
                    .map(|b| i32::from_be_bytes(b.try_into().unwrap()))
                    .collect();
                log.lock().unwrap().encodings.push(encs);
            }
            3 => {
                let m = c.take(9).await?;
                log.lock().unwrap().requests.push((
                    m[0] == 1,
                    u16::from_be_bytes([m[5], m[6]]),
                    u16::from_be_bytes([m[7], m[8]]),
                ));
                if s.after == After::Silent && next >= s.updates.len() {
                    std::future::pending::<()>().await;
                }
                if next < s.updates.len() {
                    send_chunked(&mut c, &s.updates[next], s.chunk).await?;
                    next += 1;
                } else if s.after == After::Close {
                    return Ok(());
                } else if m[0] == 0 {
                    // A real server answers a non-incremental request.
                    c.send(&update(vec![])).await?;
                }
            }
            t => bail!("fake server: unexpected client message {t}"),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

const RED: [u8; 3] = [250, 30, 30];
const GREEN: [u8; 3] = [30, 250, 30];
const BLUE: [u8; 3] = [30, 30, 250];

fn open(t: RfbTarget) -> Result<RfbBackend> {
    RfbBackend::open_with(t, TIMING)
}

fn px(f: &CapturedFrame, x: usize, y: usize) -> [u8; 3] {
    let PixelData::Rgb(b) = &f.pixels else {
        panic!("not RGB")
    };
    let i = (y * f.width as usize + x) * 3;
    [b[i], b[i + 1], b[i + 2]]
}

/// Pull frames until `pred` holds (the backend re-serves the last frame, so
/// "the update has not landed yet" is a legitimate answer for a while).
fn frame_where(b: &mut RfbBackend, pred: impl Fn(&CapturedFrame) -> bool) -> CapturedFrame {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let f = b.frame().expect("frame");
        if pred(&f) {
            return f;
        }
        assert!(Instant::now() < end, "frame never matched");
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[test]
fn tcp_raw_full_frame_and_client_setup_messages() {
    let fake = Fake::start(
        false,
        None,
        vec![Script::new(
            4,
            2,
            vec![update(vec![solid(0, 0, 4, 2, RED)])],
        )],
    );
    let mut b = open(fake.tcp_target()).unwrap();
    let f = b.frame().unwrap();
    assert_eq!((f.width, f.height), (4, 2));
    for (x, y) in [(0, 0), (3, 1), (2, 0)] {
        assert_eq!(px(&f, x, y), RED);
    }
    // The client asked for a known pixel format and encoding list, then a
    // full non-incremental update followed by an incremental one.
    let end = Instant::now() + Duration::from_secs(5);
    while fake.log.lock().unwrap().requests.len() < 2 {
        assert!(Instant::now() < end, "no incremental request followed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let l = fake.log.lock().unwrap();
    assert_eq!(
        l.pixel_formats[0],
        [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
    );
    assert_eq!(l.encodings[0], [0, 1, -223]);
    assert_eq!(l.requests[0], (false, 4, 2));
    assert_eq!(l.requests[1], (true, 4, 2));
}

#[test]
fn ws_carries_the_bearer_token_and_subprotocol() {
    let fake = Fake::start(
        true,
        Some("s3cret"),
        vec![Script::new(
            2,
            2,
            vec![update(vec![solid(0, 0, 2, 2, GREEN)])],
        )],
    );
    let mut b = open(fake.ws_target(Some("s3cret"))).unwrap();
    assert_eq!(px(&b.frame().unwrap(), 1, 1), GREEN);
    let l = fake.log.lock().unwrap();
    assert_eq!(l.auth[0].as_deref(), Some("Bearer s3cret"));
    assert_eq!(l.subprotocols[0].as_deref(), Some("binary"));
}

#[test]
fn ws_wrong_token_is_a_clear_auth_error() {
    let fake = Fake::start(true, Some("right"), vec![Script::new(2, 2, vec![])]);
    let err = open(fake.ws_target(Some("wrong")))
        .err()
        .expect("must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refused the token") && msg.contains("401"),
        "{msg}"
    );
}

#[test]
fn subrects_copyrect_and_odd_chunking_compose_one_framebuffer() {
    // 6x2: left half red, right half blue; then green over (2,0) 2x1 and a
    // CopyRect of that green onto (4,1). Delivered in 5-byte pieces over both
    // transports, so every field straddles a read boundary.
    let first = update(vec![solid(0, 0, 3, 2, RED), solid(3, 0, 3, 2, BLUE)]);
    let second = update(vec![solid(2, 0, 2, 1, GREEN), copy(4, 1, 2, 1, 2, 0)]);
    for ws in [false, true] {
        let mut s = Script::new(6, 2, vec![first.clone(), second.clone()]);
        s.chunk = 5;
        let fake = Fake::start(ws, None, vec![s]);
        let target = if ws {
            fake.ws_target(None)
        } else {
            fake.tcp_target()
        };
        let mut b = open(target).unwrap();
        let f = frame_where(&mut b, |f| px(f, 4, 1) == GREEN);
        assert_eq!(px(&f, 0, 0), RED, "ws={ws}");
        assert_eq!(px(&f, 1, 1), RED, "ws={ws}");
        assert_eq!(px(&f, 2, 0), GREEN, "ws={ws}");
        assert_eq!(px(&f, 3, 0), GREEN, "ws={ws}");
        assert_eq!(px(&f, 5, 1), GREEN, "ws={ws}");
        assert_eq!(
            px(&f, 4, 0),
            BLUE,
            "ws={ws}: row 0 right of the green is untouched"
        );
        assert_eq!(px(&f, 3, 1), BLUE, "ws={ws}");
    }
}

#[test]
fn copyrect_with_overlap_reads_the_source_before_writing() {
    // Shift a gradient row right by one: only correct if the source is
    // snapshotted first.
    let mut row = rect_head(0, 0, 4, 1, 0);
    for v in [10u8, 20, 30, 40] {
        row.extend_from_slice(&[v, v, v, 0]);
    }
    let fake = Fake::start(
        false,
        None,
        vec![Script::new(
            4,
            1,
            vec![update(vec![row]), update(vec![copy(1, 0, 3, 1, 0, 0)])],
        )],
    );
    let mut b = open(fake.tcp_target()).unwrap();
    let f = frame_where(&mut b, |f| px(f, 3, 0)[0] != 40);
    let got: Vec<u8> = (0..4).map(|x| px(&f, x, 0)[0]).collect();
    assert_eq!(got, [10, 10, 20, 30]);
}

#[test]
fn desktop_size_resizes_the_frame() {
    let fake = Fake::start(
        false,
        None,
        vec![Script::new(
            2,
            2,
            vec![
                update(vec![solid(0, 0, 2, 2, RED)]),
                update(vec![desktop_size(3, 1), solid(0, 0, 3, 1, BLUE)]),
            ],
        )],
    );
    let mut b = open(fake.tcp_target()).unwrap();
    let f = frame_where(&mut b, |f| f.width == 3);
    assert_eq!(f.height, 1);
    assert_eq!(px(&f, 2, 0), BLUE);
    let end = Instant::now() + Duration::from_secs(5);
    while !fake
        .log
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|r| (r.1, r.2) == (3, 1))
    {
        assert!(Instant::now() < end, "requests must use the new size");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn rect_outside_the_framebuffer_is_refused() {
    let fake = Fake::start(
        false,
        None,
        vec![Script::new(
            2,
            2,
            vec![
                update(vec![solid(0, 0, 2, 2, RED)]),
                update(vec![solid(1, 1, 2, 2, BLUE)]),
            ],
        )],
    );
    let mut b = open(fake.tcp_target()).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let err = loop {
        match b.frame() {
            Err(e) => break e,
            Ok(_) => assert!(Instant::now() < end, "never failed"),
        }
    };
    assert!(format!("{err:#}").contains("outside"), "{err:#}");
}

#[test]
fn server_drop_errors_the_backend_and_reopen_reconnects() {
    let mut first = Script::new(2, 2, vec![update(vec![solid(0, 0, 2, 2, RED)])]);
    first.after = After::Close;
    let second = Script::new(2, 2, vec![update(vec![solid(0, 0, 2, 2, GREEN)])]);
    let fake = Fake::start(false, None, vec![first, second]);
    let mut b = open(fake.tcp_target()).unwrap();
    assert_eq!(px(&b.frame().unwrap(), 0, 0), RED);
    let end = Instant::now() + Duration::from_secs(5);
    let err = loop {
        match b.frame() {
            Err(e) => break e,
            Ok(_) => assert!(Instant::now() < end, "drop never surfaced"),
        }
    };
    assert!(format!("{err:#}").contains("closed"), "{err:#}");
    drop(b);
    let mut b = open(fake.tcp_target()).unwrap();
    assert_eq!(px(&b.frame().unwrap(), 0, 0), GREEN);
    assert_eq!(fake.log.lock().unwrap().connections, 2);
}

#[test]
fn unsupported_security_type_is_named() {
    let mut s = Script::new(2, 2, vec![]);
    s.security = vec![2];
    let fake = Fake::start(false, None, vec![s]);
    let err = open(fake.tcp_target()).err().expect("must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("security") && msg.contains("VNC authentication"),
        "{msg}"
    );
}

#[test]
fn a_quiet_screen_is_kept_alive_and_re_served() {
    // The server answers the first request and then says nothing. The backend
    // must keep returning the frame (watchdog), and the session must ask again
    // non-incrementally (keepalive).
    let fake = Fake::start(
        false,
        None,
        vec![Script::new(
            2,
            2,
            vec![update(vec![solid(0, 0, 2, 2, RED)])],
        )],
    );
    let timing = Timing {
        keepalive: Duration::from_millis(300),
        dead_after: Duration::from_secs(15),
    };
    let mut b = RfbBackend::open_with(fake.tcp_target(), timing).unwrap();
    let t0 = Instant::now();
    for _ in 0..2 {
        assert_eq!(px(&b.frame().unwrap(), 0, 0), RED);
    }
    // Two frames with no new update take ~REEMIT_AFTER, not forever.
    assert!(t0.elapsed() < Duration::from_secs(5));
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let n = fake
            .log
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| !r.0)
            .count();
        if n >= 2 {
            break;
        }
        assert!(Instant::now() < end, "no keepalive request seen");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_silent_server_is_declared_dead() {
    let mut s = Script::new(2, 2, vec![update(vec![solid(0, 0, 2, 2, RED)])]);
    s.after = After::Silent;
    let fake = Fake::start(false, None, vec![s]);
    let timing = Timing {
        keepalive: Duration::from_millis(200),
        dead_after: Duration::from_millis(800),
    };
    let mut b = RfbBackend::open_with(fake.tcp_target(), timing).unwrap();
    let end = Instant::now() + Duration::from_secs(8);
    let err = loop {
        match b.frame() {
            Err(e) => break e,
            Ok(_) => assert!(Instant::now() < end, "never declared dead"),
        }
    };
    assert!(format!("{err:#}").contains("no data"), "{err:#}");
}

/// The whole path: capture thread -> frame pipeline -> the real router's
/// /snapshot, as a PNG of the pixels the server sent.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_serves_the_rfb_framebuffer_as_png() {
    use axum::body::Body;
    use axum::http::{header, Request};
    use tower::ServiceExt;

    let fake = Fake::start(
        false,
        None,
        vec![Script::new(
            4,
            2,
            vec![update(vec![
                solid(0, 0, 2, 2, RED),
                solid(2, 0, 2, 2, BLUE),
            ])],
        )],
    );
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let spec = DeviceSpec::parse(&format!("rfb://{}", fake.addr));
    assert!(matches!(spec, DeviceSpec::Rfb(_)));
    let (rx, _h) = crate::capture_thread::spawn(
        spec,
        crate::demand::Demand::new(),
        crate::frame::DEFAULT_CHANGE_THRESHOLD,
    );
    let app = crate::server::router(
        crate::server::AppState::new(rx),
        crate::auth::Auth::new("tok".into(), crate::server::PUBLIC_ASSETS),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/snapshot?wait=stable&timeout=5000")
                    .header(header::HOST, "127.0.0.1:1")
                    .header(header::AUTHORIZATION, "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if resp.status() == 200
            && resp
                .headers()
                .get("x-timeout")
                .map(|v| v == "0")
                .unwrap_or(true)
        {
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap();
            let img = image::load_from_memory_with_format(&body, image::ImageFormat::Png)
                .unwrap()
                .to_rgb8();
            assert_eq!(img.dimensions(), (4, 2));
            assert_eq!(img.get_pixel(0, 0).0, RED);
            assert_eq!(img.get_pixel(3, 1).0, BLUE);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no snapshot: {} {:?}",
            resp.status(),
            resp.headers()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── device strings ───────────────────────────────────────────────────────────

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn device_strings_parse_and_usb_forms_are_unchanged() {
    assert!(matches!(
        DeviceSpec::parse("rfb+ws://127.0.0.1:9/rfb"),
        DeviceSpec::Rfb(_)
    ));
    assert!(matches!(
        DeviceSpec::parse("rfb://h:5900"),
        DeviceSpec::Rfb(_)
    ));
    assert!(matches!(
        DeviceSpec::parse("rfb+discovery:"),
        DeviceSpec::Rfb(_)
    ));
    // Pre-existing forms keep their meaning, including names that merely
    // start with the letters.
    assert!(matches!(DeviceSpec::parse("auto"), DeviceSpec::Auto));
    assert!(matches!(DeviceSpec::parse(""), DeviceSpec::Auto));
    assert!(matches!(DeviceSpec::parse("2"), DeviceSpec::Index(2)));
    assert!(matches!(
        DeviceSpec::parse("/dev/video4"),
        DeviceSpec::Index(4)
    ));
    assert!(matches!(DeviceSpec::parse("rfbcam"), DeviceSpec::Name(_)));
    assert!(matches!(DeviceSpec::parse("rfb+ws"), DeviceSpec::Name(_)));
    assert!(matches!(
        DeviceSpec::parse("USB Video"),
        DeviceSpec::Name(_)
    ));
}

#[test]
fn ws_target_takes_the_token_from_env_only() {
    let t =
        RfbTarget::parse_with("rfb+ws://127.0.0.1:8080/rfb", env(&[(ENV_TOKEN, "abc")])).unwrap();
    assert_eq!(
        t,
        RfbTarget::Ws {
            url: "ws://127.0.0.1:8080/rfb".into(),
            token: Some("abc".into())
        }
    );
    let t = RfbTarget::parse_with("rfb+ws://localhost:8080", env(&[])).unwrap();
    assert_eq!(
        t,
        RfbTarget::Ws {
            url: "ws://localhost:8080/rfb".into(),
            token: None
        }
    );
}

#[test]
fn bad_device_strings_are_refused() {
    let none = env(&[]);
    for bad in [
        "rfb+ws://192.0.2.10:8080/rfb",
        "rfb+ws://127.0.0.1/rfb",
        "rfb+ws://127.0.0.1:0/rfb",
        "rfb://host",
        "rfb://:5900",
        "rfb://host:notaport",
        "rfb+discovery:",
    ] {
        assert!(RfbTarget::parse_with(bad, &none).is_err(), "{bad}");
    }
    assert_eq!(
        RfbTarget::parse_with("rfb://192.0.2.10:5900/", &none).unwrap(),
        RfbTarget::Tcp {
            addr: "192.0.2.10:5900".into()
        }
    );
}

#[test]
fn discovery_is_resolved_on_every_connect_and_rejects_a_dead_daemon() {
    let dir = std::env::temp_dir().join(format!("hdmicap-rfb-disc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("daemon.json");
    let t = RfbTarget::parse_with(
        "rfb+discovery:",
        env(&[(ENV_DISCOVERY, path.to_str().unwrap())]),
    )
    .unwrap();

    assert!(t.resolve().is_err(), "no file yet");
    let me = std::process::id();
    std::fs::write(
        &path,
        format!(r#"{{"pid":{me},"port":4001,"token":"one"}}"#),
    )
    .unwrap();
    assert_eq!(
        t.resolve().unwrap(),
        RfbTarget::Ws {
            url: "ws://127.0.0.1:4001/rfb".into(),
            token: Some("one".into())
        }
    );
    // The daemon restarts on a new port with a new token: the same target
    // follows it.
    std::fs::write(
        &path,
        format!(r#"{{"pid":{me},"port":4002,"token":"two"}}"#),
    )
    .unwrap();
    assert_eq!(
        t.resolve().unwrap(),
        RfbTarget::Ws {
            url: "ws://127.0.0.1:4002/rfb".into(),
            token: Some("two".into())
        }
    );
    std::fs::write(&path, r#"{"pid":0,"port":4003,"token":"x"}"#).unwrap();
    assert!(t.resolve().unwrap_err().to_string().contains("not running"));
    let _ = std::fs::remove_dir_all(&dir);
}
