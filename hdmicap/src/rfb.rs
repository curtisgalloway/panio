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

//! A network capture backend: an RFB (VNC) client.
//!
//! RFB is panio's common format for network video sources. The first
//! consumer is a hid helper daemon that serves RFB over a WebSocket at
//! `GET /rfb`; plain TCP servers (AMT KVM, VMs, BMCs) come next, which is why
//! the transport is abstracted away from the protocol.
//!
//! # Device strings
//!
//! | `--device`                    | transport                          | secrets |
//! |-------------------------------|------------------------------------|---------|
//! | `rfb+ws://127.0.0.1:PORT/rfb` | RFB over WebSocket (loopback only) | bearer token from env `HDMICAP_RFB_TOKEN` |
//! | `rfb://HOST:PORT`             | plain TCP, security type None only | none (VNC authentication is not implemented) |
//! | `rfb+discovery:`              | WebSocket to a hid daemon, found through the discovery file named by env `HDMICAP_RFB_DISCOVERY` | token read from that file |
//!
//! Secrets never ride argv, which `ps` shows to every user. `rfb+discovery:`
//! exists because the hid daemon is a separate process that can restart on a
//! new port with a new token: the file is re-read on every (re)connect, so a
//! fixed port in argv would go stale.
//!
//! # Threading
//!
//! The capture thread is a plain `std::thread` and [`CaptureBackend::frame`]
//! blocks, so the session runs on a worker thread with its own current-thread
//! tokio runtime and hands the composed framebuffer over through a
//! `Mutex` + `Condvar`.
//!
//! # Liveness (the stall watchdog)
//!
//! The watchdog in `capture_thread` flags a stall after 4 s without a new
//! frame, and `frame::STALE_AFTER` calls a frame older than 3 s stale. A
//! healthy RFB server with an unchanging screen sends nothing at all, so a
//! backend that only returned frames on updates would be reopened forever.
//! Two mechanisms together:
//!
//! 1. [`RfbBackend::frame`] waits at most [`REEMIT_AFTER`] for a new update and
//!    then returns the composed framebuffer again (an `Arc` clone, no copy).
//!    The pipeline sees a steady cadence and the unchanged hash keeps the
//!    signal `Stable`.
//! 2. The session sends a non-incremental update request every
//!    [`KEEPALIVE`] and declares the link dead if nothing at all arrives for
//!    [`DEAD_AFTER`]. A server that is gone is detected by *that*, not by the
//!    watchdog, so a quiet screen and a dead peer are told apart.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::{Buf, BytesMut};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;

use crate::capture::{CaptureBackend, CapturedFrame};
use crate::pixel::PixelData;

/// Env var carrying the bearer token for `rfb+ws://`.
pub const ENV_TOKEN: &str = "HDMICAP_RFB_TOKEN";
/// Env var naming the hid daemon's discovery file for `rfb+discovery:`.
pub const ENV_DISCOVERY: &str = "HDMICAP_RFB_DISCOVERY";

/// How long the backend waits for a new update before re-serving the last
/// frame. Well under the 3 s stale threshold and the 4 s watchdog poll, and
/// fast enough that the signal settles to `Stable` (8 frames) within ~2 s.
const REEMIT_AFTER: Duration = Duration::from_millis(250);
/// A non-incremental request this often proves the server is still answering.
const KEEPALIVE: Duration = Duration::from_secs(5);
/// No bytes at all for this long: the link is dead. Three missed keepalives.
const DEAD_AFTER: Duration = Duration::from_secs(15);
/// Connect + handshake + first frame.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Once a message has started, the rest must follow within this.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Pixels in the largest framebuffer accepted (a hostile server's cap).
const MAX_PIXELS: u64 = 1 << 26;
/// ServerCutText bodies are skipped; refuse absurd ones.
const MAX_CUT_TEXT: usize = 16 << 20;

const ENC_RAW: i32 = 0;
const ENC_COPYRECT: i32 = 1;
const ENC_DESKTOP_SIZE: i32 = -223;

/// Whether a `--device` string names a network source rather than a USB one.
pub fn is_rfb_spec(s: &str) -> bool {
    s.starts_with("rfb+ws://") || s.starts_with("rfb://") || s == "rfb+discovery:"
}

/// A parsed network source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfbTarget {
    /// WebSocket on loopback; `token` goes out as `Authorization: Bearer`.
    Ws { url: String, token: Option<String> },
    /// Plain TCP `host:port`.
    Tcp { addr: String },
    /// A hid daemon's discovery file (re-read per connection).
    Discovery { path: PathBuf },
}

#[derive(Deserialize)]
struct DiscoveryFile {
    pid: u32,
    port: u16,
    #[serde(default)]
    token: Option<String>,
}

impl RfbTarget {
    /// Parse a device string, taking secrets from the process environment.
    pub fn parse(spec: &str) -> Result<RfbTarget> {
        Self::parse_with(spec, |k| std::env::var(k).ok())
    }

    pub fn parse_with(spec: &str, env: impl Fn(&str) -> Option<String>) -> Result<RfbTarget> {
        let nonempty = |k: &str| env(k).filter(|v| !v.is_empty());
        if spec == "rfb+discovery:" {
            let path = nonempty(ENV_DISCOVERY)
                .ok_or_else(|| anyhow!("rfb+discovery: needs env {ENV_DISCOVERY}"))?;
            return Ok(RfbTarget::Discovery { path: path.into() });
        }
        if let Some(rest) = spec.strip_prefix("rfb+ws://") {
            let (authority, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, "/rfb"),
            };
            let (host, port) = split_host_port(authority).with_context(|| {
                format!("bad device {spec:?}: want rfb+ws://127.0.0.1:PORT/rfb")
            })?;
            // The token travels in a header over plain ws://, so the peer must
            // be this machine; a remote RFB server uses rfb://.
            if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
                bail!("rfb+ws:// is for a loopback daemon only (got host {host:?}); use rfb://HOST:PORT for a remote RFB server");
            }
            return Ok(RfbTarget::Ws {
                url: format!("ws://{host}:{port}{path}"),
                token: nonempty(ENV_TOKEN),
            });
        }
        if let Some(rest) = spec.strip_prefix("rfb://") {
            let authority = rest.trim_end_matches('/');
            split_host_port(authority)
                .with_context(|| format!("bad device {spec:?}: want rfb://HOST:PORT"))?;
            return Ok(RfbTarget::Tcp {
                addr: authority.to_string(),
            });
        }
        bail!("not a network source: {spec:?}")
    }

    /// Resolve `Discovery` into a concrete `Ws`; other forms are unchanged.
    /// Called on every connection attempt so a restarted daemon is found.
    pub fn resolve(&self) -> Result<RfbTarget> {
        let RfbTarget::Discovery { path } = self else {
            return Ok(self.clone());
        };
        let (port, token) = read_discovery(path)?;
        Ok(RfbTarget::Ws {
            url: format!("ws://127.0.0.1:{port}/rfb"),
            token,
        })
    }
}

/// Read a hid daemon's discovery file: its port and token. Fails when the file
/// is missing or names a process that is gone. Shared with the input link in
/// `hid_link.rs`.
pub(crate) fn read_discovery(path: &std::path::Path) -> Result<(u16, Option<String>)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("hid daemon not running? cannot read {path:?}"))?;
    let d: DiscoveryFile =
        serde_json::from_str(&text).with_context(|| format!("parsing discovery file {path:?}"))?;
    if !crate::platform::pid_alive(d.pid as i32) {
        bail!("hid daemon not running (stale discovery file {path:?})");
    }
    Ok((d.port, d.token.filter(|t| !t.is_empty())))
}

fn split_host_port(authority: &str) -> Result<(&str, u16)> {
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("missing :PORT"))?;
    if host.is_empty() {
        bail!("missing host");
    }
    let port: u16 = port.parse().map_err(|_| anyhow!("bad port {port:?}"))?;
    if port == 0 {
        bail!("port 0");
    }
    Ok((host, port))
}

// ── Transport ────────────────────────────────────────────────────────────────

enum Transport {
    Tcp(TcpStream),
    Ws(Box<WebSocketStream<TcpStream>>),
}

/// A byte stream with a read-ahead buffer. WebSocket messages are arbitrary
/// chunks of the stream, so both transports are read the same way: fill the
/// buffer until it holds enough, then consume. Nothing is consumed until
/// enough has arrived, which makes [`Conn::ensure`] safe to cancel (the
/// session's `select!` relies on that).
struct Conn {
    t: Transport,
    buf: BytesMut,
    last_rx: Instant,
}

impl Conn {
    fn new(t: Transport) -> Conn {
        Conn {
            t,
            buf: BytesMut::new(),
            last_rx: Instant::now(),
        }
    }

    async fn fill(&mut self) -> Result<()> {
        match &mut self.t {
            Transport::Tcp(s) => {
                if s.read_buf(&mut self.buf).await? == 0 {
                    bail!("server closed the connection");
                }
            }
            Transport::Ws(ws) => loop {
                match ws.next().await {
                    Some(Ok(Message::Binary(b))) => {
                        self.buf.extend_from_slice(&b);
                        break;
                    }
                    Some(Ok(Message::Close(_))) | None => bail!("server closed the connection"),
                    Some(Ok(_)) => continue, // ping/pong/text: not RFB bytes
                    Some(Err(e)) => return Err(anyhow!(e).context("websocket read")),
                }
            },
        }
        self.last_rx = Instant::now();
        Ok(())
    }

    /// Wait until at least `n` bytes are buffered. Cancel-safe.
    async fn ensure(&mut self, n: usize) -> Result<()> {
        while self.buf.len() < n {
            self.fill().await?;
        }
        Ok(())
    }

    /// `ensure` bounded by [`MESSAGE_TIMEOUT`], for the middle of a message.
    async fn need(&mut self, n: usize) -> Result<()> {
        tokio::time::timeout(MESSAGE_TIMEOUT, self.ensure(n))
            .await
            .map_err(|_| anyhow!("server stalled in the middle of a message"))?
    }

    async fn take(&mut self, n: usize) -> Result<BytesMut> {
        self.need(n).await?;
        Ok(self.buf.split_to(n))
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        match &mut self.t {
            Transport::Tcp(s) => {
                s.write_all(bytes).await?;
                s.flush().await?;
            }
            Transport::Ws(ws) => ws
                .send(Message::Binary(bytes.to_vec()))
                .await
                .map_err(|e| anyhow!(e).context("websocket write"))?,
        }
        Ok(())
    }
}

async fn connect(target: &RfbTarget) -> Result<Conn> {
    match target {
        RfbTarget::Tcp { addr } => {
            let s = TcpStream::connect(addr.as_str())
                .await
                .with_context(|| format!("connecting to {addr}"))?;
            let _ = s.set_nodelay(true);
            Ok(Conn::new(Transport::Tcp(s)))
        }
        RfbTarget::Ws { url, token } => {
            let mut req = url.as_str().into_client_request()?;
            let h = req.headers_mut();
            h.insert("Sec-WebSocket-Protocol", "binary".parse()?);
            if let Some(t) = token {
                h.insert("Authorization", format!("Bearer {t}").parse()?);
            }
            let authority = req
                .uri()
                .authority()
                .map(|a| a.to_string())
                .ok_or_else(|| anyhow!("no host in {url}"))?;
            let tcp = TcpStream::connect(authority.as_str())
                .await
                .with_context(|| format!("connecting to {authority}"))?;
            let _ = tcp.set_nodelay(true);
            match tokio_tungstenite::client_async(req, tcp).await {
                Ok((ws, _)) => Ok(Conn::new(Transport::Ws(Box::new(ws)))),
                Err(tungstenite::Error::Http(resp)) => {
                    let code = resp.status();
                    if code == 401 || code == 403 {
                        bail!("the RFB endpoint refused the token (HTTP {code})")
                    }
                    bail!("the RFB endpoint refused the WebSocket upgrade (HTTP {code})")
                }
                Err(e) => Err(anyhow!(e).context("websocket handshake")),
            }
        }
        RfbTarget::Discovery { .. } => unreachable!("resolve() before connect()"),
    }
}

// ── Protocol ─────────────────────────────────────────────────────────────────

/// RFB 3.3 / 3.7 / 3.8 handshake offering security type None only; returns the
/// framebuffer size from ServerInit.
async fn handshake(c: &mut Conn) -> Result<(u16, u16)> {
    let v = c.take(12).await?;
    let ver = std::str::from_utf8(&v).unwrap_or("");
    if !ver.starts_with("RFB ") || !ver.ends_with('\n') {
        bail!(
            "not an RFB server (greeting {:?})",
            String::from_utf8_lossy(&v)
        );
    }
    let minor: u32 = ver[8..11].parse().unwrap_or(0);
    let (reply, minor) = match minor {
        8.. => (&b"RFB 003.008\n"[..], 8),
        7 => (&b"RFB 003.007\n"[..], 7),
        _ => (&b"RFB 003.003\n"[..], 3),
    };
    c.send(reply).await?;

    if minor == 3 {
        let t = u32::from_be_bytes(c.take(4).await?[..].try_into().unwrap());
        match t {
            0 => bail!("server refused the connection: {}", read_reason(c).await?),
            1 => {}
            n => bail!(unsupported_security(&[n as u8])),
        }
    } else {
        let n = c.take(1).await?[0] as usize;
        if n == 0 {
            bail!("server refused the connection: {}", read_reason(c).await?);
        }
        let types = c.take(n).await?.to_vec();
        if !types.contains(&1) {
            bail!(unsupported_security(&types));
        }
        c.send(&[1]).await?;
    }
    // 3.8 sends a SecurityResult even for None; 3.3 and 3.7 do not.
    if minor == 8 {
        let r = u32::from_be_bytes(c.take(4).await?[..].try_into().unwrap());
        if r != 0 {
            bail!("security handshake failed: {}", read_reason(c).await?);
        }
    }
    c.send(&[1]).await?; // ClientInit: shared
    let init = c.take(24).await?;
    let w = u16::from_be_bytes([init[0], init[1]]);
    let h = u16::from_be_bytes([init[2], init[3]]);
    let name_len = u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize;
    if name_len > 1 << 16 {
        bail!("implausible desktop name length {name_len}");
    }
    c.take(name_len).await?;
    Ok((w, h))
}

fn unsupported_security(types: &[u8]) -> String {
    format!(
        "unsupported RFB security type(s) {types:?}: only 1 (None) is implemented; \
         VNC authentication (2) is not supported yet"
    )
}

async fn read_reason(c: &mut Conn) -> Result<String> {
    let n = u32::from_be_bytes(c.take(4).await?[..].try_into().unwrap()) as usize;
    if n > 1 << 16 {
        bail!("(reason too long)");
    }
    Ok(String::from_utf8_lossy(&c.take(n).await?).into_owned())
}

/// SetPixelFormat (32 bpp, depth 24, little-endian true colour, shifts
/// r16 g8 b0 => bytes B,G,R,pad) then SetEncodings [Raw, CopyRect, DesktopSize].
fn setup_messages() -> Vec<u8> {
    let mut m = vec![0, 0, 0, 0, 32, 24, 0, 1];
    m.extend_from_slice(&255u16.to_be_bytes());
    m.extend_from_slice(&255u16.to_be_bytes());
    m.extend_from_slice(&255u16.to_be_bytes());
    m.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
    m.extend_from_slice(&[2, 0, 0, 3]);
    for e in [ENC_RAW, ENC_COPYRECT, ENC_DESKTOP_SIZE] {
        m.extend_from_slice(&e.to_be_bytes());
    }
    m
}

fn update_request(incremental: bool, w: u16, h: u16) -> [u8; 10] {
    let mut m = [0u8; 10];
    m[0] = 3;
    m[1] = incremental as u8;
    m[6..8].copy_from_slice(&w.to_be_bytes());
    m[8..10].copy_from_slice(&h.to_be_bytes());
    m
}

/// The persistent framebuffer rectangles are composed into (packed RGB).
struct Fb {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Fb {
    fn new(w: u16, h: u16) -> Result<Fb> {
        if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS {
            bail!("unusable framebuffer size {w}x{h}");
        }
        Ok(Fb {
            w: w as usize,
            h: h as usize,
            rgb: vec![0; w as usize * h as usize * 3],
        })
    }

    fn in_bounds(&self, x: usize, y: usize, w: usize, h: usize) -> bool {
        x + w <= self.w && y + h <= self.h
    }

    /// Blit a Raw rectangle: 4 bytes per pixel, B,G,R,pad.
    fn blit_raw(&mut self, x: usize, y: usize, w: usize, h: usize, src: &[u8]) -> Result<()> {
        if !self.in_bounds(x, y, w, h) {
            bail!(
                "rectangle {w}x{h}+{x}+{y} outside the {}x{} framebuffer",
                self.w,
                self.h
            );
        }
        for row in 0..h {
            let d = ((y + row) * self.w + x) * 3;
            let s = row * w * 4;
            for (px, out) in src[s..s + w * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .zip(self.rgb[d..d + w * 3].as_chunks_mut::<3>().0.iter_mut())
            {
                out[0] = px[2];
                out[1] = px[1];
                out[2] = px[0];
            }
        }
        Ok(())
    }

    /// CopyRect: source and destination may overlap, so go through a copy.
    fn copy_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        sx: usize,
        sy: usize,
    ) -> Result<()> {
        if !self.in_bounds(x, y, w, h) || !self.in_bounds(sx, sy, w, h) {
            bail!("CopyRect {w}x{h} from +{sx}+{sy} to +{x}+{y} outside the framebuffer");
        }
        let mut tmp = Vec::with_capacity(w * h * 3);
        for row in 0..h {
            let s = ((sy + row) * self.w + sx) * 3;
            tmp.extend_from_slice(&self.rgb[s..s + w * 3]);
        }
        for row in 0..h {
            let d = ((y + row) * self.w + x) * 3;
            self.rgb[d..d + w * 3].copy_from_slice(&tmp[row * w * 3..(row + 1) * w * 3]);
        }
        Ok(())
    }
}

/// State shared between the worker thread and the capture thread.
struct Link {
    state: Mutex<Shared>,
    cv: Condvar,
    stop: AtomicBool,
}

#[derive(Default)]
struct Shared {
    version: u64,
    frame: Option<(u32, u32, Arc<[u8]>)>,
    err: Option<String>,
}

impl Link {
    fn publish(&self, fb: &Fb) {
        let rgb: Arc<[u8]> = Arc::from(fb.rgb.as_slice());
        let mut s = self.state.lock().unwrap();
        s.version += 1;
        s.frame = Some((fb.w as u32, fb.h as u32, rgb));
        self.cv.notify_all();
    }

    fn fail(&self, e: &anyhow::Error) {
        let mut s = self.state.lock().unwrap();
        s.err = Some(format!("{e:#}"));
        self.cv.notify_all();
    }
}

#[derive(Clone, Copy)]
struct Timing {
    keepalive: Duration,
    dead_after: Duration,
}

const TIMING: Timing = Timing {
    keepalive: KEEPALIVE,
    dead_after: DEAD_AFTER,
};

/// Connect, handshake and run the session until it ends. Returns `Ok` only
/// when asked to stop.
async fn session(target: &RfbTarget, link: &Link, timing: Timing) -> Result<()> {
    let target = target.resolve()?;
    let (mut c, w, h) = tokio::time::timeout(OPEN_TIMEOUT, async {
        let mut c = connect(&target).await?;
        let (w, h) = handshake(&mut c).await?;
        Ok::<_, anyhow::Error>((c, w, h))
    })
    .await
    .map_err(|_| anyhow!("timed out connecting to the RFB server"))??;

    let mut fb = Fb::new(w, h)?;
    c.send(&setup_messages()).await?;
    c.send(&update_request(false, w, h)).await?;
    let mut last_req = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        tokio::select! {
            r = c.ensure(1) => r?,
            _ = tick.tick() => {
                if link.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                if c.last_rx.elapsed() > timing.dead_after {
                    bail!("no data from the RFB server for {:?}", timing.dead_after);
                }
                if last_req.elapsed() >= timing.keepalive {
                    c.send(&update_request(false, fb.w as u16, fb.h as u16)).await?;
                    last_req = Instant::now();
                }
                continue;
            }
        }
        match c.buf[0] {
            0 => {
                if read_update(&mut c, &mut fb).await? {
                    link.publish(&fb);
                }
                c.send(&update_request(true, fb.w as u16, fb.h as u16))
                    .await?;
                last_req = Instant::now();
            }
            1 => bail!("server sent SetColourMapEntries; only true-colour is supported"),
            2 => c.buf.advance(1), // Bell
            3 => {
                // ServerCutText: skip it.
                let head = c.take(8).await?;
                let n = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;
                if n > MAX_CUT_TEXT {
                    bail!("ServerCutText of {n} bytes refused");
                }
                c.take(n).await?;
            }
            t => bail!("unknown server message type {t}"),
        }
    }
}

/// Parse one FramebufferUpdate into `fb`; true when the picture may differ.
async fn read_update(c: &mut Conn, fb: &mut Fb) -> Result<bool> {
    let head = c.take(4).await?;
    let n = u16::from_be_bytes([head[2], head[3]]);
    let mut changed = false;
    for _ in 0..n {
        let r = c.take(12).await?;
        let (x, y) = (be16(&r[0..2]), be16(&r[2..4]));
        let (w, h) = (be16(&r[4..6]), be16(&r[6..8]));
        let enc = i32::from_be_bytes(r[8..12].try_into().unwrap());
        match enc {
            ENC_RAW => {
                let len = w * h * 4;
                if (len as u64) > MAX_PIXELS * 4 {
                    bail!("Raw rectangle of {len} bytes refused");
                }
                // Bounds first, so a lying server cannot make us buffer 256 MiB.
                if !fb.in_bounds(x, y, w, h) {
                    bail!(
                        "rectangle {w}x{h}+{x}+{y} outside the {}x{} framebuffer",
                        fb.w,
                        fb.h
                    );
                }
                let data = c.take(len).await?;
                fb.blit_raw(x, y, w, h, &data)?;
                changed = true;
            }
            ENC_COPYRECT => {
                let s = c.take(4).await?;
                fb.copy_rect(x, y, w, h, be16(&s[0..2]), be16(&s[2..4]))?;
                changed = true;
            }
            ENC_DESKTOP_SIZE => {
                *fb = Fb::new(w as u16, h as u16)?;
                changed = true;
            }
            e => bail!("server sent unadvertised encoding {e}"),
        }
    }
    Ok(changed)
}

fn be16(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct RfbBackend {
    link: Arc<Link>,
    seen: u64,
    worker: Option<JoinHandle<()>>,
}

impl RfbBackend {
    /// Connect and wait for the first frame, so a failure surfaces as an open
    /// error (the capture thread publishes no_device and retries).
    pub fn open(spec: &str) -> Result<RfbBackend> {
        Self::open_with(RfbTarget::parse(spec)?, TIMING)
    }

    fn open_with(target: RfbTarget, timing: Timing) -> Result<RfbBackend> {
        let link = Arc::new(Link {
            state: Mutex::new(Shared::default()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let l = link.clone();
        let worker = std::thread::Builder::new()
            .name("rfb".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => return l.fail(&anyhow!(e)),
                };
                match rt.block_on(session(&target, &l, timing)) {
                    Ok(()) => {}
                    Err(e) => l.fail(&e),
                }
            })?;
        let backend = RfbBackend {
            link,
            seen: 0,
            worker: Some(worker),
        };
        let guard = backend.link.state.lock().unwrap();
        let (guard, _) = backend
            .link
            .cv
            .wait_timeout_while(guard, OPEN_TIMEOUT + Duration::from_secs(1), |s| {
                s.frame.is_none() && s.err.is_none()
            })
            .unwrap();
        if let Some(e) = &guard.err {
            return Err(anyhow!("{e}"));
        }
        if guard.frame.is_none() {
            return Err(anyhow!(
                "no frame from the RFB server within {OPEN_TIMEOUT:?}"
            ));
        }
        drop(guard);
        Ok(backend)
    }
}

impl CaptureBackend for RfbBackend {
    fn frame(&mut self) -> Result<CapturedFrame> {
        let seen = self.seen;
        let guard = self.link.state.lock().unwrap();
        let (guard, _) = self
            .link
            .cv
            .wait_timeout_while(guard, REEMIT_AFTER, |s| {
                s.version == seen && s.err.is_none()
            })
            .unwrap();
        if let Some(e) = &guard.err {
            let e = e.clone();
            drop(guard);
            // The caller retries without pausing; a dead session would spin it.
            std::thread::sleep(Duration::from_millis(250));
            return Err(anyhow!("{e}"));
        }
        let (w, h, rgb) = guard.frame.clone().ok_or_else(|| anyhow!("no frame yet"))?;
        self.seen = guard.version;
        Ok(CapturedFrame {
            jpeg: None,
            luma: None,
            pixels: PixelData::Rgb(rgb),
            width: w,
            height: h,
        })
    }
}

impl Drop for RfbBackend {
    fn drop(&mut self) {
        self.link.stop.store(true, Ordering::Relaxed);
        // The worker notices within a tick; do not block the capture thread
        // on a server that is mid-write.
        drop(self.worker.take());
    }
}

#[cfg(test)]
mod tests;
