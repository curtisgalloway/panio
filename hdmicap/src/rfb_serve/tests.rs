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

//! A test RFB client over a real WebSocket against the real router, with a
//! fake hid daemon behind the input path.

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message as WsMsg};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

use super::*;
use crate::frame::Signal;
use crate::pixel::PixelData;
use crate::zrle::tests::Decoder;

const TOKEN: &str = "tok";

// ── Frames ───────────────────────────────────────────────────────────────────

static CLOCK: AtomicU64 = AtomicU64::new(1);

/// A pattern whose red and blue differ almost everywhere, so a swapped
/// channel cannot pass for a match.
fn pattern(w: usize, h: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[
                ((x / 5) as u8).wrapping_mul(7),
                ((y / 3) as u8).wrapping_mul(11),
                (((x + y) / 7) as u8).wrapping_mul(3).wrapping_add(1),
            ]);
        }
    }
    v
}

fn frame(w: usize, h: usize, rgb: &[u8], hash: u64) -> Arc<FrameState> {
    // Distinct capture times, or the decode cache could hand back an old frame.
    let n = CLOCK.fetch_add(1, Ordering::SeqCst);
    Arc::new(FrameState {
        jpeg: None,
        pixels: PixelData::Rgb(Arc::from(rgb.to_vec())),
        width: w as u32,
        height: h as u32,
        hash,
        signal: Signal::Stable,
        resolution_epoch: 1,
        captured_at: Instant::now() + Duration::from_millis(n),
    })
}

fn paint(rgb: &mut [u8], w: usize, x0: usize, y0: usize, size: usize, c: [u8; 3]) {
    for y in y0..y0 + size {
        for x in x0..x0 + size {
            rgb[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&c);
        }
    }
}

// ── Server harness ───────────────────────────────────────────────────────────

struct Harness {
    port: u16,
    tx: watch::Sender<Arc<FrameState>>,
    router: axum::Router,
}

async fn harness(w: usize, h: usize, rgb: &[u8], hid: Option<Arc<HidLink>>) -> Harness {
    let (tx, rx) = watch::channel(frame(w, h, rgb, 1));
    let state = AppState::new(rx).with_rfb("bench-target", hid);
    let router = crate::server::router(
        state,
        crate::auth::Auth::new(TOKEN.into(), crate::server::PUBLIC_ASSETS),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = router.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness { port, tx, router }
}

impl Harness {
    fn publish(&self, f: Arc<FrameState>) {
        self.tx.send(f).unwrap();
    }

    async fn status(&self) -> serde_json::Value {
        let resp = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header(header::HOST, "127.0.0.1:1")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn client(&self) -> Client {
        Client::connect(self.port, Some(TOKEN)).await.unwrap()
    }
}

// ── Test client ──────────────────────────────────────────────────────────────

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Client {
    ws: Ws,
    buf: Vec<u8>,
    fmt: PixelFormat,
    canvas: Vec<u8>,
    w: usize,
    h: usize,
    dec: Decoder,
}

#[derive(Debug)]
struct GotRect {
    r: Rect,
    enc: i32,
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .expect("timed out")
}

impl Client {
    // Boxed: tungstenite::Error is large enough that clippy's result_large_err
    // fires on CI's newer toolchain. `?` boxes it via From<T> for Box<T>.
    async fn connect(port: u16, token: Option<&str>) -> Result<Client, Box<tungstenite::Error>> {
        let q = token.map(|t| format!("?token={t}")).unwrap_or_default();
        let mut req = format!("ws://127.0.0.1:{port}/rfb{q}").into_client_request()?;
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", "binary".parse().unwrap());
        let (ws, resp) = tokio_tungstenite::connect_async(req).await?;
        assert_eq!(
            resp.headers().get("Sec-WebSocket-Protocol").unwrap(),
            "binary"
        );
        Ok(Client {
            ws,
            buf: Vec::new(),
            fmt: PixelFormat::DEFAULT,
            canvas: Vec::new(),
            w: 0,
            h: 0,
            dec: Decoder::new(),
        })
    }

    async fn send(&mut self, b: Vec<u8>) {
        self.ws.send(WsMsg::Binary(b)).await.unwrap();
    }

    /// Next chunk of the stream, or `None` once the server closed.
    async fn more(&mut self) -> Option<()> {
        loop {
            match self.ws.next().await? {
                Ok(WsMsg::Binary(b)) => {
                    self.buf.extend_from_slice(&b);
                    return Some(());
                }
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn take(&mut self, n: usize) -> Vec<u8> {
        within(async {
            while self.buf.len() < n {
                self.more().await.expect("server closed the connection");
            }
        })
        .await;
        self.buf.drain(..n).collect()
    }

    /// Run the handshake as RFB 3.`minor`; returns (width, height, name).
    async fn handshake(&mut self, minor: u32) -> (usize, usize, String) {
        assert_eq!(self.take(12).await, b"RFB 003.008\n");
        self.send(format!("RFB 003.{minor:03}\n").into_bytes())
            .await;
        match minor {
            8 | 7 => {
                assert_eq!(self.take(2).await, [1, 1]);
                self.send(vec![1]).await;
                if minor == 8 {
                    assert_eq!(self.take(4).await, [0, 0, 0, 0], "SecurityResult OK");
                }
            }
            _ => assert_eq!(self.take(4).await, [0, 0, 0, 1]),
        }
        self.send(vec![1]).await;
        let head = self.take(24).await;
        let (w, h) = (be16(&head[0..2]), be16(&head[2..4]));
        assert_eq!(head[4..20], PixelFormat::DEFAULT.to_bytes());
        let n = u32::from_be_bytes(head[20..24].try_into().unwrap()) as usize;
        let name = String::from_utf8(self.take(n).await).unwrap();
        self.w = w;
        self.h = h;
        self.canvas = vec![0; w * h * 3];
        (w, h, name)
    }

    async fn set_format(&mut self, fmt: PixelFormat) {
        let mut m = vec![0, 0, 0, 0];
        m.extend_from_slice(&fmt.to_bytes());
        self.send(m).await;
        self.fmt = fmt;
    }

    async fn set_encodings(&mut self, encs: &[i32]) {
        let mut m = vec![2, 0];
        m.extend_from_slice(&(encs.len() as u16).to_be_bytes());
        for e in encs {
            m.extend_from_slice(&e.to_be_bytes());
        }
        self.send(m).await;
    }

    async fn request(&mut self, incremental: bool, x: u16, y: u16, w: u16, h: u16) {
        let mut m = vec![3, incremental as u8];
        for n in [x, y, w, h] {
            m.extend_from_slice(&n.to_be_bytes());
        }
        self.send(m).await;
    }

    async fn request_all(&mut self, incremental: bool) {
        let (w, h) = (self.w as u16, self.h as u16);
        self.request(incremental, 0, 0, w, h).await;
    }

    async fn key(&mut self, down: bool, sym: u32) {
        let mut m = vec![4, down as u8, 0, 0];
        m.extend_from_slice(&sym.to_be_bytes());
        self.send(m).await;
    }

    async fn pointer(&mut self, mask: u8, x: u16, y: u16) {
        let mut m = vec![5, mask];
        m.extend_from_slice(&x.to_be_bytes());
        m.extend_from_slice(&y.to_be_bytes());
        self.send(m).await;
    }

    /// Read one FramebufferUpdate, painting it onto the client's canvas.
    async fn update(&mut self) -> Vec<GotRect> {
        let head = self.take(4).await;
        assert_eq!(head[0], 0, "FramebufferUpdate");
        let n = be16(&head[2..4]);
        let mut got = Vec::new();
        for _ in 0..n {
            let h = self.take(12).await;
            let r = Rect {
                x: be16(&h[0..2]),
                y: be16(&h[2..4]),
                w: be16(&h[4..6]),
                h: be16(&h[6..8]),
            };
            let enc = i32::from_be_bytes(h[8..12].try_into().unwrap());
            let rgb = match enc {
                ENC_DESKTOP_SIZE => {
                    self.w = r.w;
                    self.h = r.h;
                    self.canvas = vec![0; r.w * r.h * 3];
                    got.push(GotRect { r, enc });
                    continue;
                }
                ENC_RAW => {
                    let raw = self.take(r.w * r.h * 4).await;
                    raw.as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|p| {
                            let v = if self.fmt.big_endian {
                                u32::from_be_bytes(*p)
                            } else {
                                u32::from_le_bytes(*p)
                            };
                            self.fmt.shifts.map(|s| (v >> s) as u8)
                        })
                        .collect::<Vec<u8>>()
                }
                ENC_ZRLE => {
                    let len = self.take(4).await;
                    let len = u32::from_be_bytes(len.try_into().unwrap()) as usize;
                    let data = self.take(len).await;
                    self.dec.decode_rect(self.fmt, &data, r.w, r.h)
                }
                other => panic!("unexpected encoding {other}"),
            };
            for row in 0..r.h {
                let o = ((r.y + row) * self.w + r.x) * 3;
                self.canvas[o..o + r.w * 3]
                    .copy_from_slice(&rgb[row * r.w * 3..(row + 1) * r.w * 3]);
            }
            got.push(GotRect { r, enc });
        }
        got
    }

    /// Assert nothing arrives for `ms` milliseconds.
    async fn silent_for(&mut self, ms: u64) {
        assert!(self.buf.is_empty(), "unread bytes: {}", self.buf.len());
        let r = tokio::time::timeout(Duration::from_millis(ms), self.more()).await;
        assert!(r.is_err(), "the server sent something unprompted");
    }

    /// Whether the server ends the connection within a few seconds.
    async fn closed(&mut self) -> bool {
        within(async { while self.more().await.is_some() {} }).await;
        true
    }
}

fn tiles_covered(rects: &[GotRect]) -> std::collections::BTreeSet<(usize, usize)> {
    let mut set = std::collections::BTreeSet::new();
    for g in rects {
        for ty in g.r.y / TILE..=(g.r.y + g.r.h - 1) / TILE {
            for tx in g.r.x / TILE..=(g.r.x + g.r.w - 1) / TILE {
                set.insert((tx, ty));
            }
        }
    }
    set
}

/// noVNC's pixel format: 32bpp depth 24, little-endian, r0 g8 b16.
const NOVNC: PixelFormat = PixelFormat {
    big_endian: false,
    shifts: [0, 8, 16],
    depth: 24,
};

// ── Handshake ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn handshake_announces_size_format_and_target_name() {
    let img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    for minor in [8, 7, 3] {
        let mut c = h.client().await;
        let (w, hh, name) = c.handshake(minor).await;
        assert_eq!(
            (w, hh, name.as_str()),
            (200, 150, "bench-target"),
            "3.{minor}"
        );
    }
}

#[tokio::test]
async fn a_text_frame_is_refused() {
    let h = harness(64, 64, &pattern(64, 64), None).await;
    let mut c = h.client().await;
    c.ws.send(WsMsg::Text("hello".into())).await.unwrap();
    assert!(c.closed().await);
}

#[tokio::test]
async fn the_endpoint_needs_the_daemon_token() {
    let h = harness(64, 64, &pattern(64, 64), None).await;
    match Client::connect(h.port, None).await.map_err(|e| *e) {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 401),
        other => panic!("expected 401, got {:?}", other.map(|_| ())),
    }
    match Client::connect(h.port, Some("wrong")).await.map_err(|e| *e) {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 401),
        other => panic!("expected 401, got {:?}", other.map(|_| ())),
    }
}

// ── Pixels ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_novnc_style_client_gets_the_exact_frame_in_zrle() {
    let img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_format(NOVNC).await;
    c.set_encodings(&[1, 7, ENC_ZRLE, 5, ENC_RAW, ENC_DESKTOP_SIZE])
        .await;
    c.request_all(false).await;
    let rects = c.update().await;
    assert!(rects.iter().all(|g| g.enc == ENC_ZRLE));
    assert!(c.canvas == img, "decoded frame differs from the source");
}

#[tokio::test]
async fn every_supported_pixel_format_reproduces_the_frame() {
    let img = pattern(130, 70);
    let h = harness(130, 70, &img, None).await;
    for (big_endian, shifts) in [
        (false, [16, 8, 0]),
        (true, [16, 8, 0]),
        (true, [0, 8, 16]),
        (false, [24, 16, 8]),
    ] {
        for encs in [vec![ENC_ZRLE], vec![ENC_RAW]] {
            let mut c = h.client().await;
            c.handshake(8).await;
            c.set_format(PixelFormat {
                big_endian,
                shifts,
                depth: 24,
            })
            .await;
            c.set_encodings(&encs).await;
            c.request_all(false).await;
            c.update().await;
            assert!(c.canvas == img, "{big_endian} {shifts:?} {encs:?}");
        }
    }
}

#[tokio::test]
async fn raw_is_used_when_zrle_is_not_offered() {
    let img = pattern(100, 80);
    let h = harness(100, 80, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_RAW, 1]).await;
    c.request_all(false).await;
    let rects = c.update().await;
    assert!(rects.iter().all(|g| g.enc == ENC_RAW));
    assert!(c.canvas == img);
}

#[tokio::test]
async fn an_unsupported_pixel_format_is_refused_clearly() {
    let h = harness(64, 64, &pattern(64, 64), None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    let mut bad = PixelFormat::DEFAULT.to_bytes();
    bad[0] = 16; // 16 bits per pixel
    let mut m = vec![0, 0, 0, 0];
    m.extend_from_slice(&bad);
    c.send(m).await;
    let reason = within(async {
        loop {
            match c.ws.next().await {
                Some(Ok(WsMsg::Close(Some(f)))) => return f.reason.to_string(),
                Some(Ok(_)) => {}
                other => panic!("no close frame: {other:?}"),
            }
        }
    })
    .await;
    assert!(reason.contains("16 bits per pixel"), "{reason}");
}

#[tokio::test]
async fn an_incremental_update_sends_exactly_the_tiles_that_changed() {
    let mut img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_ZRLE]).await;
    c.request_all(false).await;
    c.update().await;

    // A 10x10 patch inside one tile: that tile and no other.
    paint(&mut img, 200, 70, 70, 10, [250, 1, 2]);
    h.publish(frame(200, 150, &img, 2));
    c.request_all(true).await;
    let rects = c.update().await;
    assert_eq!(rects.len(), 1);
    assert_eq!(
        rects[0].r,
        Rect {
            x: 64,
            y: 64,
            w: 64,
            h: 64
        }
    );
    assert!(c.canvas == img);

    // A patch across a tile corner: all four tiles around it.
    paint(&mut img, 200, 60, 60, 10, [3, 250, 4]);
    h.publish(frame(200, 150, &img, 3));
    c.request_all(true).await;
    let rects = c.update().await;
    let want: std::collections::BTreeSet<_> = [(0, 0), (1, 0), (0, 1), (1, 1)].into();
    assert_eq!(tiles_covered(&rects), want);
    assert!(c.canvas == img);
}

#[tokio::test]
async fn an_edge_tile_smaller_than_64_is_sent_whole() {
    let mut img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_ZRLE]).await;
    c.request_all(false).await;
    c.update().await;
    paint(&mut img, 200, 195, 145, 5, [9, 9, 250]);
    h.publish(frame(200, 150, &img, 2));
    c.request_all(true).await;
    let rects = c.update().await;
    assert_eq!(
        rects[0].r,
        Rect {
            x: 192,
            y: 128,
            w: 8,
            h: 22
        }
    );
    assert!(c.canvas == img);
}

#[tokio::test]
async fn with_nothing_new_a_request_waits_and_sends_nothing() {
    let mut img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_ZRLE]).await;
    c.request_all(false).await;
    c.update().await;

    c.request_all(true).await;
    c.silent_for(300).await;

    // A new capture of an unchanged screen (same settled hash): still quiet.
    h.publish(frame(200, 150, &img, 1));
    c.silent_for(200).await;
    // A new hash whose pixels equal what the client holds: the exact diff
    // finds nothing, still quiet.
    h.publish(frame(200, 150, &img, 2));
    c.silent_for(200).await;

    // A real change wakes the waiting request.
    paint(&mut img, 200, 0, 0, 4, [255, 255, 255]);
    h.publish(frame(200, 150, &img, 3));
    let rects = within(c.update()).await;
    assert_eq!(rects.len(), 1);
    assert!(c.canvas == img);
}

#[tokio::test]
async fn a_second_client_has_its_own_view_of_what_changed() {
    let mut img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut a = h.client().await;
    a.handshake(8).await;
    a.set_encodings(&[ENC_ZRLE]).await;
    a.request_all(false).await;
    a.update().await;

    paint(&mut img, 200, 0, 0, 8, [1, 2, 3]);
    h.publish(frame(200, 150, &img, 2));

    // B connects after the change and still gets the whole current frame.
    let mut b = h.client().await;
    b.handshake(8).await;
    b.set_encodings(&[ENC_ZRLE]).await;
    b.request_all(false).await;
    b.update().await;
    assert!(b.canvas == img);

    a.request_all(true).await;
    a.update().await;
    assert!(a.canvas == img);
}

#[tokio::test]
async fn a_resolution_change_sends_desktop_size_then_the_whole_frame() {
    let img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_ZRLE, ENC_DESKTOP_SIZE]).await;
    c.request_all(false).await;
    c.update().await;

    let img2 = pattern(100, 90);
    h.publish(frame(100, 90, &img2, 5));
    c.request_all(true).await;
    let rects = c.update().await;
    assert_eq!(rects[0].enc, ENC_DESKTOP_SIZE);
    assert_eq!((rects[0].r.w, rects[0].r.h), (100, 90));
    assert!(rects[1..].iter().all(|g| g.enc == ENC_ZRLE));
    assert_eq!((c.w, c.h), (100, 90));
    assert!(c.canvas == img2);
}

#[tokio::test]
async fn a_resolution_change_closes_a_client_without_desktop_size() {
    let h = harness(200, 150, &pattern(200, 150), None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_ZRLE]).await;
    c.request_all(false).await;
    c.update().await;
    h.publish(frame(100, 90, &pattern(100, 90), 5));
    c.request_all(true).await;
    assert!(c.closed().await);
}

#[tokio::test]
async fn clients_are_capped_and_counted() {
    let h = harness(64, 64, &pattern(64, 64), None).await;
    let mut held = Vec::new();
    for _ in 0..MAX_CLIENTS {
        let mut c = h.client().await;
        c.handshake(8).await;
        held.push(c);
    }
    assert_eq!(h.status().await["rfb_clients"], MAX_CLIENTS);
    match Client::connect(h.port, Some(TOKEN)).await.map_err(|e| *e) {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 503),
        other => panic!("expected 503, got {:?}", other.map(|_| ())),
    }
    drop(held);
    within(async {
        while h.status().await["rfb_clients"] != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

// ── Input ────────────────────────────────────────────────────────────────────

/// A stand-in for the hid daemon's `GET /hid`: records every text frame, and
/// insists on the bearer token.
struct FakeHid {
    lines: Arc<Mutex<Vec<String>>>,
    discovery: std::path::PathBuf,
}

async fn fake_hid() -> FakeHid {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use axum::http::HeaderMap;
    use axum::routing::get;

    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let app = axum::Router::new().route(
        "/hid",
        get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
            let sink = sink.clone();
            async move {
                let ok = headers
                    .get(header::AUTHORIZATION)
                    .is_some_and(|v| v == "Bearer hid-secret");
                if !ok {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                ws.on_upgrade(move |mut sock| async move {
                    while let Some(Ok(m)) = sock.recv().await {
                        if let Message::Text(t) = m {
                            sink.lock().unwrap().push(t.clone());
                            let _ = sock.send(Message::Text(format!("evt ok {t} :: OK"))).await;
                        }
                    }
                })
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = std::env::temp_dir().join(format!(
        "hdmicap-hid-{}-{}",
        std::process::id(),
        CLOCK.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let discovery = dir.join("daemon.json");
    std::fs::write(
        &discovery,
        format!(
            r#"{{"pid":{},"port":{port},"token":"hid-secret"}}"#,
            std::process::id()
        ),
    )
    .unwrap();
    FakeHid { lines, discovery }
}

impl FakeHid {
    async fn lines_len(&self, n: usize) -> Vec<String> {
        within(async {
            loop {
                let l = self.lines.lock().unwrap().clone();
                if l.len() >= n {
                    return l;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }
}

async fn input_rig() -> (Harness, FakeHid, Client) {
    let hid = fake_hid().await;
    let link = HidLink::spawn(hid.discovery.clone());
    let img = pattern(200, 150);
    let h = harness(200, 150, &img, Some(link)).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    (h, hid, c)
}

const SHIFT_L: u32 = 0xFFE1;

#[tokio::test]
async fn shift_then_a_types_a_capital_the_way_novnc_sends_it() {
    let (h, hid, mut c) = input_rig().await;
    // noVNC: Shift_L down, then the shifted keysym 'A', releases in order.
    c.key(true, SHIFT_L).await;
    c.key(true, 'A' as u32).await;
    c.key(false, 'A' as u32).await;
    c.key(false, SHIFT_L).await;
    assert_eq!(
        hid.lines_len(4).await,
        ["down LEFT_SHIFT", "down A", "up A", "up LEFT_SHIFT"]
    );
    assert_eq!(h.status().await["rfb_input"], true);
}

#[tokio::test]
async fn a_shifted_character_sent_alone_gets_a_shift_of_its_own() {
    let (_h, hid, mut c) = input_rig().await;
    c.key(true, '!' as u32).await;
    c.key(false, '!' as u32).await;
    assert_eq!(
        hid.lines_len(4).await,
        ["down LEFT_SHIFT", "down ONE", "up ONE", "up LEFT_SHIFT"]
    );
}

#[tokio::test]
async fn special_keys_and_repeats() {
    let (_h, hid, mut c) = input_rig().await;
    c.key(true, 0xFF0D).await; // Return
    c.key(true, 0xFF0D).await; // auto-repeat: not sent again
    c.key(false, 0xFF0D).await;
    c.key(true, 0xFFBE).await; // F1
    c.key(false, 0xFFBE).await;
    c.key(true, 0xFFCA).await; // F13: no such key, dropped
    c.key(false, 0xFFCA).await;
    c.key(true, 0xFF51).await; // Left
    c.key(false, 0xFF51).await;
    assert_eq!(
        hid.lines_len(6).await,
        [
            "down ENTER",
            "up ENTER",
            "down F1",
            "up F1",
            "down LEFT_ARROW",
            "up LEFT_ARROW"
        ]
    );
}

#[tokio::test]
async fn the_pointer_is_scaled_and_buttons_and_wheel_are_edges() {
    let (_h, hid, mut c) = input_rig().await;
    // The middle of a 200x150 screen: 100/200 and 75/150 of 32767, rounded.
    c.pointer(0, 100, 75).await;
    c.pointer(1, 100, 75).await; // left down, same place
    c.pointer(0, 100, 75).await; // left up
    c.pointer(8, 100, 75).await; // wheel up
    c.pointer(0, 100, 75).await;
    c.pointer(16, 100, 75).await; // wheel down
    c.pointer(0, 100, 75).await;
    c.pointer(0, 0, 0).await; // corner
    assert_eq!(
        hid.lines_len(6).await,
        [
            "moveabs 16384 16384",
            "mdown left",
            "mup left",
            "scroll 1",
            "scroll -1",
            "moveabs 0 0"
        ]
    );
}

#[tokio::test]
async fn a_click_lands_where_the_pointer_was_sent() {
    let (_h, hid, mut c) = input_rig().await;
    c.pointer(0, 20, 30).await;
    c.pointer(4, 40, 60).await; // right button down at a new place
    c.pointer(0, 40, 60).await;
    let l = hid.lines_len(3).await;
    let moves: Vec<_> = l.iter().filter(|s| s.starts_with("moveabs")).collect();
    let down = l.iter().position(|s| s == "mdown right").unwrap();
    // Whatever moves were coalesced away, the last move before the press is
    // the press position: 40/200 and 60/150 of 32767.
    let before = l[..down]
        .iter()
        .rfind(|s| s.starts_with("moveabs"))
        .unwrap();
    assert_eq!(before, "moveabs 6553 13107", "{l:?} {moves:?}");
    assert_eq!(l[down + 1], "mup right");
}

#[tokio::test]
async fn disconnecting_releases_every_key_and_button_the_client_held() {
    let (_h, hid, mut c) = input_rig().await;
    c.key(true, 'a' as u32).await;
    c.key(true, SHIFT_L).await;
    c.pointer(1, 10, 10).await;
    hid.lines_len(4).await;
    drop(c);
    let l = hid.lines_len(7).await;
    for want in ["up A", "up LEFT_SHIFT", "mup left"] {
        assert!(l.iter().any(|s| s == want), "missing {want}: {l:?}");
    }
}

#[tokio::test]
async fn without_a_hid_daemon_input_is_ignored_and_status_says_so() {
    let img = pattern(200, 150);
    let h = harness(200, 150, &img, None).await;
    let mut c = h.client().await;
    c.handshake(8).await;
    c.set_encodings(&[ENC_ZRLE]).await;
    c.key(true, 'a' as u32).await;
    c.pointer(1, 5, 5).await;
    c.request_all(false).await;
    c.update().await;
    assert!(c.canvas == img, "the view still works");
    assert_eq!(h.status().await["rfb_input"], false);
}
