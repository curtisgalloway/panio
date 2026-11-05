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

//! `GET /rfb`: the daemon's warm frame served as RFB over a WebSocket, for
//! noVNC in the dashboard. (`rfb.rs` is the opposite direction: hdmicap as an
//! RFB *client* of a network source.)
//!
//! One server for every target, USB capture and network source alike. The
//! protocol is RFB 3.8 with security type None (3.3 and 3.7 clients are
//! answered in kind); the endpoint sits behind the same auth layer as every
//! other route, so reaching it needs the daemon token and a loopback Host.
//!
//! **Pixels.** The session keeps, per client, the last pixels it sent. A
//! FramebufferUpdateRequest is answered by diffing the current frame against
//! that in 64x64 tiles and sending only what changed, ZRLE-compressed (Raw if
//! the client does not offer ZRLE). The frame's settled hash lets an unchanged
//! screen skip the diff altogether. A client with nothing new to see waits;
//! nothing here polls.
//!
//! **Input.** KeyEvent and PointerEvent become HID serial protocol lines for
//! the target's hid daemon (`hid_link.rs`) — or are ignored when no daemon is
//! configured (`/status` says `rfb_input: false`). Whatever a client still
//! holds when it goes away is released.
//!
//! **Bounds.** At most [`MAX_CLIENTS`] sessions; a client that cannot take an
//! update within [`SEND_TIMEOUT`] is closed rather than buffered for.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::{Buf, BytesMut};
use tracing::{debug, info};

use crate::capture_thread::FrameRx;
use crate::frame::FrameState;
use crate::hid_link::HidLink;
use crate::keysym;
use crate::server::AppState;
use crate::zrle::{PixelFormat, ZrleEncoder, TILE};

/// Simultaneous RFB sessions; the next one gets 503.
pub const MAX_CLIENTS: usize = 8;

/// How long one update may take to leave for a client before it is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on bytes received but not yet parsed.
const MAX_INBOUND: usize = 2 << 20;

/// Size announced before the first frame exists.
const PLACEHOLDER: (usize, usize) = (640, 480);

const ENC_RAW: i32 = 0;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;

type DecodeCache = Arc<Mutex<Option<(Instant, Arc<Rgb>)>>>;

/// Daemon-wide RFB state, held in [`AppState`].
#[derive(Clone)]
pub struct RfbShared {
    clients: Arc<AtomicUsize>,
    /// The last decoded frame, keyed by its capture time, so N clients on one
    /// frame cost one decode.
    cache: DecodeCache,
    hid: Option<Arc<HidLink>>,
    name: Arc<str>,
}

impl RfbShared {
    pub fn new() -> RfbShared {
        RfbShared {
            clients: Arc::new(AtomicUsize::new(0)),
            cache: Arc::new(Mutex::new(None)),
            hid: None,
            name: Arc::from("hdmicap"),
        }
    }

    pub fn with_hid(mut self, hid: Arc<HidLink>) -> RfbShared {
        self.hid = Some(hid);
        self
    }

    pub fn with_name(mut self, name: &str) -> RfbShared {
        self.name = Arc::from(name);
        self
    }

    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub fn input_enabled(&self) -> bool {
        self.hid.is_some()
    }
}

impl Default for RfbShared {
    fn default() -> Self {
        Self::new()
    }
}

struct ClientSlot(Arc<AtomicUsize>);

impl ClientSlot {
    fn acquire(counter: &Arc<AtomicUsize>) -> Option<ClientSlot> {
        if counter.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(ClientSlot(counter.clone()))
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A decoded frame: packed RGB, 3 bytes per pixel.
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Rgb {
    fn black(w: usize, h: usize) -> Rgb {
        Rgb {
            w,
            h,
            data: vec![0; w * h * 3],
        }
    }
}

/// `GET /rfb`.
pub async fn handler(State(s): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(slot) = ClientSlot::acquire(&s.rfb.clients) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many RFB clients\n").into_response();
    };
    ws.protocols(["binary"])
        .max_message_size(1 << 20)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            run_session(socket, s).await;
        })
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Why a session ends, and what the client is told.
struct Close {
    code: u16,
    reason: String,
}

impl Close {
    fn normal() -> Close {
        Close {
            code: 1000,
            reason: String::new(),
        }
    }

    fn protocol(reason: impl Into<String>) -> Close {
        Close {
            code: 1002,
            reason: reason.into(),
        }
    }

    fn unsupported(reason: impl Into<String>) -> Close {
        Close {
            code: 1003,
            reason: reason.into(),
        }
    }

    fn internal(reason: impl Into<String>) -> Close {
        Close {
            code: 1011,
            reason: reason.into(),
        }
    }
}

// ── Messages ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Req {
    incremental: bool,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

#[derive(Debug)]
enum Msg {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    Request(Req),
    Key { down: bool, sym: u32 },
    Pointer { mask: u8, x: usize, y: usize },
    CutText,
}

fn be16(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}

/// Take one complete client message off the front of `buf`, if there is one.
fn parse(buf: &mut BytesMut) -> Result<Option<Msg>, Close> {
    let Some(&ty) = buf.first() else {
        return Ok(None);
    };
    let (need, extra) = match ty {
        0 => (20, 0),
        2 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            let n = be16(&buf[2..4]);
            if n > 1024 {
                return Err(Close::protocol("too many encodings"));
            }
            (4 + 4 * n, 0)
        }
        3 => (10, 0),
        4 => (8, 0),
        5 => (6, 0),
        6 => {
            if buf.len() < 8 {
                return Ok(None);
            }
            let n = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            if n > 1 << 20 {
                return Err(Close::protocol("client cut text too long"));
            }
            (8 + n, 0)
        }
        other => return Err(Close::protocol(format!("unknown client message {other}"))),
    };
    let _ = extra;
    if buf.len() < need {
        return Ok(None);
    }
    let m = buf.split_to(need);
    Ok(Some(match ty {
        0 => {
            let pf: [u8; 16] = m[4..20].try_into().expect("16 bytes");
            Msg::SetPixelFormat(PixelFormat::parse(&pf).map_err(Close::unsupported)?)
        }
        2 => {
            let n = be16(&m[2..4]);
            Msg::SetEncodings(
                (0..n)
                    .map(|i| {
                        let o = 4 + 4 * i;
                        i32::from_be_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]])
                    })
                    .collect(),
            )
        }
        3 => Msg::Request(Req {
            incremental: m[1] != 0,
            x: be16(&m[2..4]),
            y: be16(&m[4..6]),
            w: be16(&m[6..8]),
            h: be16(&m[8..10]),
        }),
        4 => Msg::Key {
            down: m[1] != 0,
            sym: u32::from_be_bytes([m[4], m[5], m[6], m[7]]),
        },
        5 => Msg::Pointer {
            mask: m[1],
            x: be16(&m[2..4]),
            y: be16(&m[4..6]),
        },
        _ => Msg::CutText,
    }))
}

// ── Client framebuffer: what the client has been sent ────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

struct ClientFb {
    w: usize,
    h: usize,
    /// The pixels the client holds, packed RGB.
    sent: Vec<u8>,
    /// Per grid tile: has the client been sent this whole tile at least once?
    known: Vec<bool>,
    cols: usize,
}

impl ClientFb {
    fn new(w: usize, h: usize) -> ClientFb {
        let cols = w.div_ceil(TILE);
        let rows = h.div_ceil(TILE);
        ClientFb {
            w,
            h,
            sent: vec![0; w * h * 3],
            known: vec![false; cols * rows],
            cols,
        }
    }

    /// The part of the request inside the framebuffer.
    fn clip(&self, r: &Req) -> Rect {
        let x0 = r.x.min(self.w);
        let y0 = r.y.min(self.h);
        let x1 = (r.x + r.w).min(self.w);
        let y1 = (r.y + r.h).min(self.h);
        Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }

    fn region_differs(&self, fb: &Rgb, r: Rect) -> bool {
        (r.y..r.y + r.h).any(|row| {
            let o = (row * self.w + r.x) * 3;
            fb.data[o..o + r.w * 3] != self.sent[o..o + r.w * 3]
        })
    }

    /// The rectangles to send for `req`: every grid tile (clipped to the
    /// request) that differs from what the client has, or all of them when
    /// `everything`; horizontally adjacent ones merged into one rectangle.
    fn dirty_regions(&self, fb: &Rgb, req: Rect, everything: bool) -> Vec<Rect> {
        let mut out: Vec<Rect> = Vec::new();
        if req.w == 0 || req.h == 0 {
            return out;
        }
        for ty in req.y / TILE..=(req.y + req.h - 1) / TILE {
            let mut open: Option<Rect> = None;
            for tx in req.x / TILE..=(req.x + req.w - 1) / TILE {
                let x0 = (tx * TILE).max(req.x);
                let y0 = (ty * TILE).max(req.y);
                let x1 = ((tx + 1) * TILE).min(req.x + req.w);
                let y1 = ((ty + 1) * TILE).min(req.y + req.h);
                let r = Rect {
                    x: x0,
                    y: y0,
                    w: x1 - x0,
                    h: y1 - y0,
                };
                let dirty =
                    everything || !self.known[ty * self.cols + tx] || self.region_differs(fb, r);
                match (dirty, open.as_mut()) {
                    (true, Some(o)) => o.w += r.w,
                    (true, None) => open = Some(r),
                    (false, _) => out.extend(open.take()),
                }
            }
            out.extend(open.take());
        }
        out
    }

    /// Record that the client now holds `r` as it is in `fb`.
    fn commit(&mut self, fb: &Rgb, r: Rect) {
        for row in r.y..r.y + r.h {
            let o = (row * self.w + r.x) * 3;
            self.sent[o..o + r.w * 3].copy_from_slice(&fb.data[o..o + r.w * 3]);
        }
        for ty in r.y / TILE..=(r.y + r.h - 1) / TILE {
            for tx in r.x / TILE..=(r.x + r.w - 1) / TILE {
                let covers = r.x <= tx * TILE
                    && r.y <= ty * TILE
                    && r.x + r.w >= ((tx + 1) * TILE).min(self.w)
                    && r.y + r.h >= ((ty + 1) * TILE).min(self.h);
                if covers {
                    self.known[ty * self.cols + tx] = true;
                }
            }
        }
    }
}

/// What the encode step needs, moved onto a blocking thread and back.
struct Work {
    cfb: ClientFb,
    zrle: ZrleEncoder,
}

struct UpdateOut {
    /// The whole FramebufferUpdate message, when there is anything to send.
    msg: Option<Vec<u8>>,
    /// The request covered the entire framebuffer, so "nothing changed" (or
    /// "sent it all") says something about the whole frame.
    covers_all: bool,
    /// The frame changed size and the client cannot be told.
    no_desktop_size: bool,
}

fn push_rect_head(out: &mut Vec<u8>, r: Rect, enc: i32) {
    for n in [r.x, r.y, r.w, r.h] {
        out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    out.extend_from_slice(&enc.to_be_bytes());
}

impl Work {
    fn update(
        &mut self,
        fb: &Rgb,
        req: Req,
        fmt: PixelFormat,
        use_zrle: bool,
        desktop_ok: bool,
    ) -> UpdateOut {
        let mut rects = 0u16;
        let mut body = Vec::new();
        let mut everything = !req.incremental;
        let mut req = req;
        if fb.w != self.cfb.w || fb.h != self.cfb.h {
            if !desktop_ok {
                return UpdateOut {
                    msg: None,
                    covers_all: false,
                    no_desktop_size: true,
                };
            }
            self.cfb = ClientFb::new(fb.w, fb.h);
            push_rect_head(
                &mut body,
                Rect {
                    x: 0,
                    y: 0,
                    w: fb.w,
                    h: fb.h,
                },
                ENC_DESKTOP_SIZE,
            );
            rects += 1;
            // The client's framebuffer was just reallocated: refill it all.
            req = Req {
                incremental: false,
                x: 0,
                y: 0,
                w: fb.w,
                h: fb.h,
            };
            everything = true;
        }
        let clip = self.cfb.clip(&req);
        let covers_all = clip.w == fb.w && clip.h == fb.h;
        let regions = self.cfb.dirty_regions(fb, clip, everything);
        for r in &regions {
            if use_zrle {
                push_rect_head(&mut body, *r, ENC_ZRLE);
                let z = self
                    .zrle
                    .encode_rect(fmt, &fb.data, fb.w, r.x, r.y, r.w, r.h);
                body.extend_from_slice(&(z.len() as u32).to_be_bytes());
                body.extend_from_slice(&z);
            } else {
                push_rect_head(&mut body, *r, ENC_RAW);
                for row in r.y..r.y + r.h {
                    let o = (row * fb.w + r.x) * 3;
                    let (px, _) = fb.data[o..o + r.w * 3].as_chunks::<3>();
                    for p in px {
                        fmt.push_pixel(&mut body, *p);
                    }
                }
            }
            self.cfb.commit(fb, *r);
            rects += 1;
        }
        let msg = (rects > 0).then(|| {
            let mut m = vec![0u8, 0];
            m.extend_from_slice(&rects.to_be_bytes());
            m.extend_from_slice(&body);
            m
        });
        UpdateOut {
            msg,
            covers_all,
            no_desktop_size: false,
        }
    }
}

// ── Input ────────────────────────────────────────────────────────────────────

struct Held {
    name: &'static str,
    /// This key got a Shift of the session's own pressed for it.
    synth_shift: bool,
    is_shift: bool,
}

/// One client's keyboard and pointer state, translated into HID lines.
struct Input {
    link: Option<Arc<HidLink>>,
    held: HashMap<u32, Held>,
    /// Shift keys the client itself holds.
    real_shifts: u32,
    /// Held keys that rely on the session's own Shift.
    synth_users: u32,
    /// Last pointer mask, for edge detection.
    mask: u8,
    last_pos: Option<(u16, u16)>,
    warned: HashSet<u32>,
}

const BUTTONS: [&str; 3] = ["left", "middle", "right"];

impl Input {
    fn new(link: Option<Arc<HidLink>>) -> Input {
        Input {
            link,
            held: HashMap::new(),
            real_shifts: 0,
            synth_users: 0,
            mask: 0,
            last_pos: None,
            warned: HashSet::new(),
        }
    }

    async fn key(&mut self, down: bool, sym: u32) {
        let Some(link) = self.link.clone() else {
            return;
        };
        if down {
            if self.held.contains_key(&sym) {
                return; // browser auto-repeat: the target repeats on its own
            }
            let Some(k) = keysym::lookup(sym) else {
                if self.warned.insert(sym) {
                    debug!("rfb: dropping keysym {sym:#x} (no HID key)");
                }
                return;
            };
            // noVNC sends Shift as its own event before a shifted keysym, so
            // normally Shift is already down. A client that sends '!' alone
            // gets a Shift for the length of the key.
            let synth = k.shifted && self.real_shifts == 0;
            if synth {
                if self.synth_users == 0 {
                    link.send("down LEFT_SHIFT".into()).await;
                }
                self.synth_users += 1;
            }
            if k.is_shift {
                self.real_shifts += 1;
            }
            link.send(format!("down {}", k.name)).await;
            self.held.insert(
                sym,
                Held {
                    name: k.name,
                    synth_shift: synth,
                    is_shift: k.is_shift,
                },
            );
        } else if let Some(h) = self.held.remove(&sym) {
            link.send(format!("up {}", h.name)).await;
            if h.is_shift {
                self.real_shifts -= 1;
            }
            if h.synth_shift {
                self.synth_users -= 1;
                if self.synth_users == 0 {
                    link.send("up LEFT_SHIFT".into()).await;
                }
            }
        }
    }

    async fn pointer(&mut self, mask: u8, x: usize, y: usize, dims: (usize, usize)) {
        let Some(link) = self.link.clone() else {
            return;
        };
        let (w, h) = dims;
        if w > 0 && h > 0 {
            // The protocol's own formula: round(pixel * 32767 / size).
            let scale = |p: usize, size: usize| {
                ((p.min(size - 1) as u64 * 32767 + size as u64 / 2) / size as u64) as u16
            };
            let pos = (scale(x, w), scale(y, h));
            let edge = (self.mask ^ mask) & 0x1F != 0;
            if edge {
                // A press or release happens where the pointer is now.
                link.move_to_ordered(pos.0, pos.1).await;
            } else if self.last_pos != Some(pos) {
                link.move_to(pos.0, pos.1);
            }
            self.last_pos = Some(pos);
        }
        let (old, new) = (self.mask, mask);
        for (bit, name) in BUTTONS.iter().enumerate() {
            let (was, is) = (old >> bit & 1, new >> bit & 1);
            if is > was {
                link.send(format!("mdown {name}")).await;
            } else if was > is {
                link.send(format!("mup {name}")).await;
            }
        }
        // Wheel: bits 3 (up) and 4 (down), one notch per press. Bits 5 and 6
        // (left, right) have no counterpart in the HID vocabulary.
        if new & 8 != 0 && old & 8 == 0 {
            link.send("scroll 1".into()).await;
        }
        if new & 16 != 0 && old & 16 == 0 {
            link.send("scroll -1".into()).await;
        }
        self.mask = mask;
    }
}

impl Drop for Input {
    /// Release whatever the client still holds. Not async, so it uses the
    /// non-waiting send; the queue has room for a keyboard's worth.
    fn drop(&mut self) {
        let Some(link) = &self.link else {
            return;
        };
        for h in self.held.values() {
            link.try_send(format!("up {}", h.name));
        }
        if self.synth_users > 0 {
            link.try_send("up LEFT_SHIFT".into());
        }
        for (bit, name) in BUTTONS.iter().enumerate() {
            if self.mask >> bit & 1 != 0 {
                link.try_send(format!("mup {name}"));
            }
        }
    }
}

// ── Session ──────────────────────────────────────────────────────────────────

struct Session<'a> {
    socket: &'a mut WebSocket,
    buf: BytesMut,
    s: AppState,
    rx: FrameRx,
    fmt: PixelFormat,
    zrle: bool,
    desktop_size: bool,
    pending: Option<Req>,
    work: Option<Work>,
    /// Settled hash and size of the frame last diffed against the whole
    /// framebuffer.
    last: Option<(u64, usize, usize)>,
    input: Input,
    dims: (usize, usize),
}

async fn run_session(socket: WebSocket, s: AppState) {
    let mut socket = socket;
    let result = session(&mut socket, s).await;
    let c = result.err().unwrap_or_else(Close::normal);
    if c.code != 1000 {
        debug!("rfb session closed: {} ({})", c.reason, c.code);
    }
    let mut reason = c.reason;
    if reason.len() > 120 {
        reason.truncate(120);
    }
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: c.code,
            reason: reason.into(),
        })))
        .await;
}

/// Read client messages into `buf` until at least one more byte is there.
async fn fill(socket: &mut WebSocket, buf: &mut BytesMut) -> Result<(), Close> {
    loop {
        match socket.recv().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Err(Close::normal()),
            Some(Ok(Message::Binary(b))) => {
                buf.extend_from_slice(&b);
                if buf.len() > MAX_INBOUND {
                    return Err(Close::protocol("client sent too much"));
                }
                return Ok(());
            }
            Some(Ok(Message::Text(_))) => {
                return Err(Close::protocol("RFB is binary"));
            }
            Some(Ok(_)) => {}
        }
    }
}

async fn need(socket: &mut WebSocket, buf: &mut BytesMut, n: usize) -> Result<(), Close> {
    while buf.len() < n {
        fill(socket, buf).await?;
    }
    Ok(())
}

async fn send(socket: &mut WebSocket, data: Vec<u8>) -> Result<(), Close> {
    match tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Binary(data))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(Close::normal()),
        Err(_) => Err(Close::internal("client too slow")),
    }
}

async fn session(socket: &mut WebSocket, s: AppState) -> Result<(), Close> {
    let mut buf = BytesMut::new();

    // ProtocolVersion.
    send(socket, b"RFB 003.008\n".to_vec()).await?;
    need(socket, &mut buf, 12).await?;
    let v = buf.split_to(12);
    let minor = match (&v[..8], &v[8..11], v[11]) {
        (b"RFB 003.", m, b'\n') => std::str::from_utf8(m)
            .ok()
            .and_then(|m| m.parse::<u32>().ok())
            .ok_or_else(|| Close::protocol("bad protocol version"))?,
        _ => return Err(Close::protocol("not an RFB client")),
    };

    // Security: None only.
    if minor >= 7 {
        send(socket, vec![1, 1]).await?;
        need(socket, &mut buf, 1).await?;
        if buf.get_u8() != 1 {
            return Err(Close::protocol("only security type None is offered"));
        }
        if minor >= 8 {
            send(socket, vec![0, 0, 0, 0]).await?;
        }
    } else {
        send(socket, vec![0, 0, 0, 1]).await?;
    }

    // ClientInit (shared flag): sessions are all shared.
    need(socket, &mut buf, 1).await?;
    buf.advance(1);

    // ServerInit.
    let rx = s.frames.clone();
    let (w, h) = {
        let f = rx.borrow();
        if f.width > 0 && f.height > 0 {
            (f.width as usize, f.height as usize)
        } else {
            PLACEHOLDER
        }
    };
    let mut init = Vec::new();
    init.extend_from_slice(&(w as u16).to_be_bytes());
    init.extend_from_slice(&(h as u16).to_be_bytes());
    init.extend_from_slice(&PixelFormat::DEFAULT.to_bytes());
    init.extend_from_slice(&(s.rfb.name.len() as u32).to_be_bytes());
    init.extend_from_slice(s.rfb.name.as_bytes());
    send(socket, init).await?;
    info!("rfb client connected ({} now)", s.rfb.clients());

    // An attached viewer wants every frame, like an open /preview.
    let _stream = s.demand.stream();

    let input = Input::new(s.rfb.hid.clone());
    let mut sess = Session {
        socket,
        buf,
        s,
        rx,
        fmt: PixelFormat::DEFAULT,
        zrle: false,
        desktop_size: false,
        pending: None,
        work: Some(Work {
            cfb: ClientFb::new(w, h),
            zrle: ZrleEncoder::new(),
        }),
        last: None,
        input,
        dims: (w, h),
    };
    sess.run().await
}

impl Session<'_> {
    async fn run(&mut self) -> Result<(), Close> {
        loop {
            while let Some(m) = parse(&mut self.buf)? {
                self.handle(m).await?;
            }
            tokio::select! {
                r = fill(self.socket, &mut self.buf) => r?,
                r = self.rx.changed(), if self.pending.is_some() => {
                    r.map_err(|_| Close::internal("capture thread gone"))?;
                    self.try_update().await?;
                }
            }
        }
    }

    async fn handle(&mut self, m: Msg) -> Result<(), Close> {
        match m {
            Msg::SetPixelFormat(f) => self.fmt = f,
            Msg::SetEncodings(e) => {
                self.zrle = e.contains(&ENC_ZRLE);
                self.desktop_size = e.contains(&ENC_DESKTOP_SIZE);
            }
            Msg::Request(r) => {
                self.pending = Some(r);
                self.try_update().await?;
            }
            Msg::Key { down, sym } => self.input.key(down, sym).await,
            Msg::Pointer { mask, x, y } => self.input.pointer(mask, x, y, self.dims).await,
            Msg::CutText => {}
        }
        Ok(())
    }

    /// The current frame as RGB, decoded once per frame however many clients
    /// are watching. `None` while the source has produced nothing decodable.
    async fn rgb(s: &AppState, f: &Arc<FrameState>) -> Option<Arc<Rgb>> {
        if let Some((at, rgb)) = &*s.rfb.cache.lock().unwrap() {
            if *at == f.captured_at {
                return Some(rgb.clone());
            }
        }
        let _permit = s.expensive.clone().acquire_owned().await.ok()?;
        let f2 = f.clone();
        let img = tokio::task::spawn_blocking(move || crate::server::decode_rgb(&f2))
            .await
            .ok()??;
        let rgb = Arc::new(Rgb {
            w: img.width() as usize,
            h: img.height() as usize,
            data: img.into_raw(),
        });
        *s.rfb.cache.lock().unwrap() = Some((f.captured_at, rgb.clone()));
        Some(rgb)
    }

    /// Answer the pending request if the screen has something to show for it.
    async fn try_update(&mut self) -> Result<(), Close> {
        let Some(req) = self.pending else {
            return Ok(());
        };
        let frame = self.rx.borrow_and_update().clone();
        let size = (frame.width as usize, frame.height as usize);
        if req.incremental && self.last == Some((frame.hash, size.0, size.1)) {
            return Ok(()); // the settled hash says nothing moved
        }
        let rgb = match Self::rgb(&self.s, &frame).await {
            Some(r) => r,
            None if !req.incremental => Arc::new(Rgb::black(self.dims.0, self.dims.1)),
            None => return Ok(()),
        };

        let mut work = self.work.take().expect("work is returned after each use");
        let (fmt, zrle, desktop) = (self.fmt, self.zrle, self.desktop_size);
        let permit = self.s.expensive.clone().acquire_owned().await.ok();
        let (work, out) = tokio::task::spawn_blocking(move || {
            let out = work.update(&rgb, req, fmt, zrle, desktop);
            (work, out)
        })
        .await
        .map_err(|_| Close::internal("encoder failed"))?;
        drop(permit);
        self.dims = (work.cfb.w, work.cfb.h);
        self.work = Some(work);
        if out.no_desktop_size {
            return Err(Close::unsupported(
                "the screen changed size and this client has no DesktopSize",
            ));
        }
        self.last = out.covers_all.then_some((frame.hash, size.0, size.1));
        if let Some(msg) = out.msg {
            self.pending = None;
            send(self.socket, msg).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
