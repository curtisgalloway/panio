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

//! Localhost HTTP API. Handlers never touch the device — they only read the
//! latest FrameState from their `watch::Receiver`. PNG encoding is lazy, here.
//!
//! Every route but the vendored xterm.js assets sits behind the auth layer
//! (`auth.rs`): loopback Host and Origin, and the daemon token. The dashboard
//! page reads the token from its own URL and appends it to every request it
//! makes back here.

use std::io::Cursor;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use bytes::Bytes;
use image::{ImageBuffer, Rgb};
use serde::Deserialize;
use tokio::sync::{watch, Semaphore};

use crate::capture_thread::FrameRx;
use crate::demand::{Demand, FRESH_ENOUGH, FRESH_WAIT};
use crate::frame::{FrameState, Signal, StatusDto};
use crate::pixel::{nv12_to_rgb, nv12_to_rgb_half, PixelData};

/// Concurrent permits for the genuinely expensive work a dashboard click can
/// trigger: PNG encode/decode and the OCR subprocess (Review M21). Also
/// bounds /preview's own JPEG-encode fallback (Issue #142) — every open
/// preview connection used to spawn its own unbounded `spawn_blocking`
/// encode, so a handful of browser tabs could starve /snapshot and /ocr's
/// share of this same semaphore. Small on purpose — enough that one slow
/// request doesn't serialize behind another unrelated one, not so large
/// that a burst of clicks piles up unbounded CPU work or unbounded
/// `visionocr` helper processes.
const EXPENSIVE_PERMITS: usize = 2;

/// The most recently fallback-encoded /preview JPEG, keyed by the source
/// frame's `captured_at` (Issue #142): `None` until the first fallback
/// encode, then `Some((that frame's captured_at, its encoded bytes))`.
type PreviewCache = Arc<Mutex<Option<(Instant, Arc<[u8]>)>>>;

#[derive(Clone)]
pub struct AppState {
    pub frames: FrameRx,
    /// Bounds concurrent PNG encode/decode, OCR subprocess work, and
    /// /preview's fallback JPEG encode — see [`EXPENSIVE_PERMITS`].
    pub expensive: Arc<Semaphore>,
    /// Every preview client observing the same frame checks this after
    /// taking an `expensive` permit — a cache hit reuses the bytes instead
    /// of re-encoding, so N clients watching one frame cost one encode, not
    /// N. Plain `std::sync::Mutex` is fine: it is only ever held for a
    /// synchronous lookup or store, never across an `.await`.
    preview_cache: PreviewCache,
    /// What clients want from the capture loop right now (#221): streams
    /// hold a guard, pulls record themselves. See [`crate::demand`].
    pub(crate) demand: Arc<Demand>,
    /// The noVNC view's shared state (`rfb_serve`): client count, decode
    /// cache, optional hid link for input.
    pub(crate) rfb: crate::rfb_serve::RfbShared,
    /// The daemon's `--change-threshold`, reported by `/status`.
    change_threshold: u8,
}

impl AppState {
    pub fn new(frames: FrameRx) -> Self {
        AppState {
            frames,
            expensive: Arc::new(Semaphore::new(EXPENSIVE_PERMITS)),
            preview_cache: Arc::new(Mutex::new(None)),
            demand: Demand::new(),
            rfb: crate::rfb_serve::RfbShared::new(),
            change_threshold: crate::frame::DEFAULT_CHANGE_THRESHOLD,
        }
    }

    /// Record the threshold the capture thread was started with.
    pub fn with_change_threshold(mut self, threshold: u8) -> Self {
        self.change_threshold = threshold;
        self
    }

    /// Name the target in the RFB ServerInit, and give the noVNC view its
    /// keyboard and mouse through `hid`, when there is one.
    pub fn with_rfb(mut self, name: &str, hid: Option<Arc<crate::hid_link::HidLink>>) -> Self {
        self.rfb = self.rfb.with_name(name);
        if let Some(h) = hid {
            self.rfb = self.rfb.with_hid(h);
        }
        self
    }

    /// Share the capture thread's demand record, so requests pace it.
    pub fn with_demand(mut self, demand: Arc<Demand>) -> Self {
        self.demand = demand;
        self
    }
}

/// A pull's promise: the frame it serves was captured after the pull arrived,
/// or at least within [`FRESH_ENOUGH`] of it (#221).
///
/// An idle capture loop runs slowly, so the warm frame can be most of a
/// second old. Recording the pull wakes the loop at full rate; if the warm
/// frame is too old, wait (up to [`FRESH_WAIT`]) for the next one. A capture
/// thread that is gone or slow just means serving what there is, as before.
async fn fresh_frame(rx: &mut FrameRx, demand: &Demand) {
    demand.pulled();
    if rx.borrow_and_update().captured_at.elapsed() <= FRESH_ENOUGH {
        return;
    }
    let _ = tokio::time::timeout(FRESH_WAIT, rx.changed()).await;
}

/// Paths served without the daemon token: the vendored xterm.js and noVNC
/// library files the dashboard page loads by bare `<script>`/`<link>` path.
/// They are public code, not data, and a bare asset tag cannot carry a header.
pub const PUBLIC_ASSETS: &[&str] = &[
    "/xterm.js",
    "/xterm.css",
    "/xterm-addon-fit.js",
    "/novnc.js",
];

pub fn router(state: AppState, auth: crate::auth::Auth) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/stop", post(stop))
        .route("/status", get(status))
        .route("/snapshot", get(snapshot))
        .route("/preview", get(preview))
        .route("/rfb", get(crate::rfb_serve::handler))
        .route("/ocr", get(ocr))
        .route("/power", get(power_state))
        .route("/power-on", post(power_on))
        .route("/power-off", post(power_off))
        .route("/power-cycle", post(power_cycle))
        .route("/devices", get(devices))
        // Vendored xterm.js assets for the serial terminal pane.
        .route("/xterm.js", get(xterm_js))
        .route("/xterm.css", get(xterm_css))
        .route("/xterm-addon-fit.js", get(xterm_fit_js))
        // Vendored noVNC (one bundled ES module) for the video pane.
        .route("/novnc.js", get(novnc_js))
        .layer(middleware::from_fn_with_state(auth, crate::auth::require))
        .with_state(state)
}

/// Authenticated shutdown. `hdmicap stop` calls this instead of signaling the
/// PID in the discovery file: a record left behind by a crash can name a PID
/// the kernel has since handed to an unrelated process, and the token proves
/// the request reached the daemon that wrote the record. The daemon's serve
/// loop owns the `Notify` (see `daemon::run`).
async fn stop(Extension(shutdown): Extension<Arc<tokio::sync::Notify>>) -> &'static str {
    shutdown.notify_one();
    "daemon stopping\n"
}

/// The dashboard. It must never render inside another page's frame: its power
/// buttons act on the target, and a framed page is how a click gets stolen.
async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        include_str!("../assets/index.html"),
    )
}

async fn xterm_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../assets/xterm.js"),
    )
}

async fn xterm_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/xterm.css"),
    )
}

async fn xterm_fit_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../assets/xterm-addon-fit.js"),
    )
}

async fn novnc_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/novnc.js"),
    )
}

/// `GET /status`: the frame's state plus what the noVNC view is doing.
#[derive(serde::Serialize)]
struct StatusOut {
    #[serde(flatten)]
    frame: StatusDto,
    /// Open `/rfb` sessions.
    rfb_clients: usize,
    /// Whether the noVNC view can type and point (a hid daemon is configured).
    rfb_input: bool,
}

async fn status(State(s): State<AppState>) -> Json<StatusOut> {
    let f = s.frames.borrow().clone();
    Json(StatusOut {
        frame: StatusDto::new(f.as_ref(), s.change_threshold),
        rfb_clients: s.rfb.clients(),
        rfb_input: s.rfb.input_enabled(),
    })
}

#[derive(Deserialize)]
struct SnapReq {
    /// "stable" -> wait until signal == Stable.
    wait: Option<String>,
    /// Hex hash from a prior /status; wait until the published hash differs.
    changed_since: Option<String>,
    /// Milliseconds; default applied below.
    timeout: Option<u64>,
}

const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// Whether a frame satisfies the /snapshot wait conditions.
///
/// The subtle case is when the caller passes *both* `wait=stable` and
/// `changed_since` (Issue #170): they want the next frame that is stable AND
/// differs from the hash they already have, so both must hold. The earlier
/// code answered on stability alone in that case, handing back the very frame
/// the caller said they already had (`changed_since` silently ignored). Each
/// single condition is still applied on its own; with neither, any frame is
/// ready.
fn snapshot_ready(f: &FrameState, want_stable: bool, changed_since: Option<u64>) -> bool {
    let stable = f.effective_signal() == Signal::Stable;
    match (want_stable, changed_since) {
        (true, Some(h)) => stable && f.hash != h,
        (true, None) => stable,
        (false, Some(h)) => f.hash != h,
        (false, None) => true,
    }
}

async fn snapshot(State(s): State<AppState>, Query(q): Query<SnapReq>) -> Response {
    let mut rx = s.frames.clone();
    fresh_frame(&mut rx, &s.demand).await;
    let timeout_ms = q.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms).min(Duration::from_secs(60));
    let want_stable = q.wait.as_deref() == Some("stable");
    let changed_since = q
        .changed_since
        .as_ref()
        .and_then(|h| u64::from_str_radix(h, 16).ok());

    // A snapshot that waits for a change or a stable signal is watching the
    // screen until it answers, so it holds the capture loop at full rate:
    // at an idle rate it would notice the change up to a second late.
    let _stream = (want_stable || changed_since.is_some()).then(|| s.demand.stream());

    loop {
        let ready = {
            let f = rx.borrow_and_update();
            snapshot_ready(&f, want_stable, changed_since)
        };

        if ready {
            let f = rx.borrow().clone();
            return png_response(&f, false, &s.expensive).await;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let f = rx.borrow().clone();
            return png_response(&f, true, &s.expensive).await;
        }
        match tokio::time::timeout(remaining, rx.changed()).await {
            // Changed: loop back around and re-check readiness.
            Ok(Ok(())) => {}
            // Review M20. The capture thread's Sender dropped. Without this
            // arm, `rx.changed()` keeps resolving immediately with Err (there
            // is nothing left to wait for) and the outer `.is_err()` check —
            // which only sees the *outer* timeout, never this inner result —
            // let the loop spin at 100% CPU until the deadline.
            Ok(Err(_)) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "capture thread gone").into_response();
            }
            Err(_) => {
                let f = rx.borrow().clone();
                return png_response(&f, true, &s.expensive).await;
            }
        }
    }
}

/// Decode the frame to a full-resolution RGB image. NV12 (macOS) converts
/// here, lazily; on the Linux MJPEG path we decode `jpeg` with turbojpeg.
pub(crate) fn decode_rgb(f: &FrameState) -> Option<ImageBuffer<Rgb<u8>, Vec<u8>>> {
    match &f.pixels {
        PixelData::Rgb(buf) => ImageBuffer::from_raw(f.width, f.height, buf.to_vec()),
        PixelData::Nv12 { y, cbcr } => Some(nv12_to_rgb(y, cbcr, f.width, f.height)),
        PixelData::Empty => {
            #[cfg(target_os = "linux")]
            if let Some(ref jpeg) = f.jpeg {
                return turbojpeg::decompress_image::<Rgb<u8>>(jpeg).ok();
            }
            None
        }
    }
}

/// Encode a preview JPEG from decoded pixels (the non-MJPEG fallback path).
/// Large NV12 frames are halved first — the human preview doesn't need 8 MP,
/// and 4:2:0 makes halving nearly free — then encoded with the fast
/// `jpeg-encoder` crate.
fn encode_preview_jpeg(f: &FrameState) -> Option<Vec<u8>> {
    const PREVIEW_MAX_WIDTH: u32 = 1920;
    let img = match &f.pixels {
        PixelData::Nv12 { y, cbcr } if f.width > PREVIEW_MAX_WIDTH => {
            nv12_to_rgb_half(y, cbcr, f.width, f.height)
        }
        PixelData::Nv12 { y, cbcr } => nv12_to_rgb(y, cbcr, f.width, f.height),
        PixelData::Rgb(buf) => ImageBuffer::from_raw(f.width, f.height, buf.to_vec())?,
        PixelData::Empty => return None,
    };
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, 80);
    encoder
        .encode(
            img.as_raw(),
            img.width() as u16,
            img.height() as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .ok()?;
    Some(out)
}

/// Encode the frame to PNG bytes. Shared by /snapshot and /ocr. Decode
/// (`decode_rgb`, which on Linux runs `turbojpeg::decompress_image`) and PNG
/// encoding both happen here, inline — callers reach it only through
/// [`encode_png_guarded`], which moves this off the async runtime.
fn encode_png(f: &FrameState) -> Option<Vec<u8>> {
    let img = decode_rgb(f)?;
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .ok()?;
    Some(bytes)
}

/// [`encode_png`], run on the blocking thread pool and gated by `expensive`
/// (Review M21). Decode + PNG encode is real CPU work — turbojpeg on Linux,
/// a full-resolution PNG encode everywhere — and running it inline on a
/// tokio worker blocks every other request that worker would otherwise
/// service. The semaphore bounds how many such encodes (plus OCR's own use
/// of it) can run at once, so repeated /snapshot or /ocr clicks queue rather
/// than piling up unbounded work.
async fn encode_png_guarded(f: Arc<FrameState>, expensive: &Semaphore) -> Option<Vec<u8>> {
    let _permit = expensive.acquire().await.ok()?;
    tokio::task::spawn_blocking(move || encode_png(&f))
        .await
        .unwrap_or(None)
}

/// Lazily encode the current RGB buffer to PNG. PNG for agent snapshots: text
/// edges matter for OCR and the dongle already adds MJPEG artifacts.
async fn png_response(f: &Arc<FrameState>, timed_out: bool, expensive: &Semaphore) -> Response {
    if f.effective_signal() == Signal::Stale {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::HeaderName::from_static("x-signal"), "stale")],
            "capture stalled; the last frame is too old to be the screen",
        )
            .into_response();
    }
    if f.signal == Signal::NoDevice || f.width == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::HeaderName::from_static("x-signal"), "no_device")],
            "no capture device",
        )
            .into_response();
    }

    let bytes = match encode_png_guarded(Arc::clone(f), expensive).await {
        Some(b) => b,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "frame buffer size mismatch",
            )
                .into_response()
        }
    };

    let signal_str = signal_name(f.signal);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (
                header::HeaderName::from_static("x-signal"),
                signal_str.to_string(),
            ),
            (
                header::HeaderName::from_static("x-resolution-epoch"),
                f.resolution_epoch.to_string(),
            ),
            (
                header::HeaderName::from_static("x-frame-hash"),
                format!("{:016x}", f.hash),
            ),
            (
                header::HeaderName::from_static("x-timeout"),
                (timed_out as u8).to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Dimensions for the "not a screen" placeholder (Issue #143) when this
/// connection has never yet seen a live frame — e.g. `/preview` opened before
/// any device is detected.
const PLACEHOLDER_DEFAULT_DIMS: (u32, u32) = (640, 360);

/// Half the stroke width, in pixels, of the placeholder's diagonal X.
const PLACEHOLDER_HALF_STROKE: f64 = 6.0;

/// A clearly-not-a-screen placeholder JPEG: a dark gray field with a thick
/// red diagonal X. `/preview` sends this in place of a live frame whenever
/// `FrameState::effective_signal()` says the last frame no longer describes
/// the screen (Issue #143) — a frozen `<img>` on an open MJPEG stream reads
/// as "still live" to a human watching it, so the picture itself has to
/// visibly change, not just an HTTP header nobody but a script reads.
///
/// `dims` is the last live frame's resolution, so the placeholder keeps that
/// aspect ratio instead of jumping to the default; `None` (never seen a live
/// frame on this connection) falls back to [`PLACEHOLDER_DEFAULT_DIMS`].
fn placeholder_jpeg(dims: Option<(u32, u32)>) -> Option<Vec<u8>> {
    let (w, h) = dims
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or(PLACEHOLDER_DEFAULT_DIMS);
    let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgb([40, 40, 40]));

    // Perpendicular (Euclidean) distance from (x, y) to each corner-to-corner
    // diagonal of the w*h box; painting every pixel within half a stroke
    // width of either draws a thick X regardless of aspect ratio.
    let (wf, hf) = (f64::from(w), f64::from(h));
    let norm = wf.hypot(hf).max(1.0);
    for y in 0..h {
        for x in 0..w {
            let (xf, yf) = (f64::from(x), f64::from(y));
            let d1 = (xf * hf - yf * wf).abs() / norm;
            let d2 = (xf * hf + yf * wf - wf * hf).abs() / norm;
            if d1 <= PLACEHOLDER_HALF_STROKE || d2 <= PLACEHOLDER_HALF_STROKE {
                img.put_pixel(x, y, Rgb([200, 20, 20]));
            }
        }
    }

    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, 80);
    encoder
        .encode(
            img.as_raw(),
            w as u16,
            h as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .ok()?;
    Some(out)
}

/// Build one multipart/x-mixed-replace part: boundary, headers — including
/// `X-Signal` (Issue #143; browsers ignore unknown part headers, but a test
/// or a future client can read it) — then the JPEG bytes.
fn multipart_chunk(jpeg_bytes: &[u8], signal: Signal) -> Bytes {
    let part_header = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nX-Signal: {}\r\n\r\n",
        jpeg_bytes.len(),
        signal_name(signal),
    );
    let mut chunk = Vec::with_capacity(part_header.len() + jpeg_bytes.len() + 2);
    chunk.extend_from_slice(part_header.as_bytes());
    chunk.extend_from_slice(jpeg_bytes);
    chunk.extend_from_slice(b"\r\n");
    Bytes::from(chunk)
}

/// What this /preview connection last put on the wire — so a signal that
/// hasn't changed (the common case: a real frame across several preview ticks,
/// or a placeholder while nothing has recovered) is neither re-encoded nor
/// re-sent every tick.
enum Served {
    /// The last live frame served, keyed by its `captured_at`.
    Frame(Instant),
    /// A live frame whose fallback JPEG encode failed, keyed by its
    /// `captured_at` (Issue #169). `encode_preview_jpeg` returns `None` only
    /// for a malformed `PixelData::Rgb` buffer (length != w*h*3), and a
    /// `spawn_blocking` join error means the encode task panicked; either way
    /// this exact frame cannot be encoded. Recording the attempt stops the
    /// stream from re-acquiring an `expensive` permit and re-spawning the same
    /// doomed encode on every preview tick. A new frame (different `captured_at`)
    /// resumes normal service.
    Failed(Instant),
    /// The last placeholder served, keyed by the effective signal that caused
    /// it.
    Placeholder(Signal),
}

/// Whether `preview` should attempt to encode and serve a live frame given
/// what it last put on the wire. A frame already served (`Frame`) or already
/// tried and found un-encodable (`Failed`) — same `captured_at` — is skipped,
/// so an un-encodable frame is attempted once, not re-attempted on every
/// tick (Issue #169). Any frame with a new `captured_at` is always attempted,
/// whatever the previous outcome, so service resumes as soon as an encodable
/// frame arrives.
fn should_attempt_live(last_served: &Option<Served>, captured_at: Instant) -> bool {
    !matches!(
        last_served,
        Some(Served::Frame(at) | Served::Failed(at)) if *at == captured_at
    )
}

/// multipart/x-mixed-replace MJPEG stream for the human browser preview.
/// Reads the same warm buffer as /snapshot — zero device contention.
/// When raw JPEG bytes are available (Linux MJPEG path), they are served
/// directly with zero server-side decode or re-encode. Otherwise we re-encode
/// from the decoded RGB buffer at quality 80 — bounded by the same
/// `AppState.expensive` semaphore /snapshot and /ocr share, and coalesced
/// across every client watching the same frame via `AppState.preview_cache`
/// (Issue #142): N clients on one frame cost one encode, not N.
///
/// Once `FrameState::effective_signal()` says the last frame is no longer
/// live (`Stale`, `NoSignal`, or `NoDevice`), the stream stops serving that
/// frame's bytes and instead sends [`placeholder_jpeg`] once per transition,
/// so a browser tab left open visibly shows "not the screen" instead of
/// quietly freezing on the last real frame (Issue #143). Every part carries
/// an `X-Signal` header naming the effective signal that produced it.
async fn preview(State(s): State<AppState>) -> Response {
    let mut frames = s.frames.clone();
    let expensive = s.expensive.clone();
    let preview_cache = s.preview_cache.clone();
    // Held by the stream itself, so it drops when the client disconnects and
    // the body stream is dropped: an open preview wants every frame (#221).
    let demand = s.demand.clone();

    let stream = async_stream::stream! {
        let _watching = demand.stream();
        let mut interval = tokio::time::interval(crate::frame::TARGET_FRAME_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_served: Option<Served> = None;
        // The last live frame's dimensions, so a placeholder shown after a
        // real frame keeps that frame's aspect ratio rather than jumping to
        // the startup default.
        let mut last_dims: Option<(u32, u32)> = None;

        loop {
            interval.tick().await;
            let f = frames.borrow_and_update().clone();
            let eff = f.effective_signal();
            let live = matches!(eff, Signal::Stable | Signal::ModeSwitching)
                && f.width > 0
                && f.height > 0;

            if live {
                last_dims = Some((f.width, f.height));
                if !should_attempt_live(&last_served, f.captured_at) {
                    continue;
                }

                // Fast path: raw JPEG bytes from the device — no decode/re-encode.
                let jpeg_bytes: Vec<u8> = if let Some(ref raw) = f.jpeg {
                    raw.to_vec()
                } else {
                    // Fallback: encode from native pixels (macOS NV12 / YUYV).
                    // Review M21 / Issue #142: real CPU work, so it runs off
                    // the async runtime, bounded by `expensive`, and
                    // coalesced across every client on this frame via
                    // `preview_cache`. `_permit` (leading underscore: a real
                    // binding kept alive by RAII, not `let _ = ...`, which
                    // would drop it immediately) is released at the end of
                    // this `else` block — before its value is even assigned
                    // to `jpeg_bytes`, well before the chunk below is
                    // yielded.
                    let _permit = match expensive.acquire().await {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let cached = {
                        let guard = preview_cache.lock().ok();
                        guard.and_then(|g| match &*g {
                            Some((at, bytes)) if *at == f.captured_at => Some(Arc::clone(bytes)),
                            _ => None,
                        })
                    };
                    let bytes = match cached {
                        Some(b) => b,
                        None => {
                            let owned = f.clone();
                            let encoded =
                                tokio::task::spawn_blocking(move || encode_preview_jpeg(&owned))
                                    .await;
                            let encoded: Arc<[u8]> = match encoded {
                                Ok(Some(b)) => Arc::from(b),
                                // This exact frame cannot be encoded. Advance
                                // the cursor to `Failed` so the next tick skips
                                // it (Issue #169) instead of re-taking a permit
                                // and re-spawning the same doomed encode ~15
                                // times a second. Warn once per frame — the
                                // `Failed` cursor rate-limits us to one log per
                                // stuck frame, not one per tick.
                                other => {
                                    tracing::warn!(
                                        "/preview: dropping un-encodable frame \
                                         ({}x{}, hash {:016x}): {}",
                                        f.width,
                                        f.height,
                                        f.hash,
                                        match other {
                                            Ok(None) =>
                                                "encode produced no bytes \
                                                 (malformed pixel buffer)",
                                            Err(_) => "encode task panicked",
                                            Ok(Some(_)) => unreachable!(),
                                        },
                                    );
                                    last_served = Some(Served::Failed(f.captured_at));
                                    continue;
                                }
                            };
                            if let Ok(mut c) = preview_cache.lock() {
                                *c = Some((f.captured_at, Arc::clone(&encoded)));
                            }
                            encoded
                        }
                    };
                    bytes.to_vec()
                };
                last_served = Some(Served::Frame(f.captured_at));
                yield Ok::<Bytes, std::io::Error>(multipart_chunk(&jpeg_bytes, eff));
            } else {
                if matches!(last_served, Some(Served::Placeholder(sig)) if sig == eff) {
                    continue;
                }
                let dims = last_dims;
                let jpeg_bytes =
                    match tokio::task::spawn_blocking(move || placeholder_jpeg(dims)).await {
                        Ok(Some(b)) => b,
                        // Same rule as the live path above: record the attempt
                        // before skipping, or a placeholder that cannot be
                        // encoded is retried on every tick for as long as
                        // the signal stays put. Synthetic bytes make this far
                        // less reachable than the live case, but the dedup is
                        // what keeps it bounded either way.
                        other => {
                            tracing::warn!(
                                "/preview: dropping un-encodable {} placeholder: {}",
                                signal_name(eff),
                                match other {
                                    Ok(None) => "encode produced no bytes",
                                    Err(_) => "encode task panicked",
                                    Ok(Some(_)) => unreachable!(),
                                },
                            );
                            last_served = Some(Served::Placeholder(eff));
                            continue;
                        }
                    };
                last_served = Some(Served::Placeholder(eff));
                yield Ok::<Bytes, std::io::Error>(multipart_chunk(&jpeg_bytes, eff));
            }
        }
    };

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace;boundary=frame",
        )
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Locate the OCR tool: `PANIO_VISIONOCR` (panio sets this) wins, then a
/// `visionocr` installed next to our own executable (`panio setup` puts both
/// in the libexec dir), then a bare name resolved via PATH.
fn visionocr_bin() -> std::ffi::OsString {
    if let Some(bin) = std::env::var_os("PANIO_VISIONOCR") {
        return bin;
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            for name in ["visionocr", "linuxocr", "winocr", "winocr.exe"] {
                let sibling = dir.join(name);
                if sibling.is_file() {
                    return sibling.into();
                }
            }
        }
    }
    "visionocr".into()
}

/// Wrap plain text from a pre-v1 helper in a v1 envelope.
///
/// A new daemon against an old installed helper should still read screens —
/// just without confidences — rather than failing in a way that looks like a
/// broken capture. The synthesized envelope names the binary so the cause is
/// visible, and carries no `lines`, because inventing boxes would be worse than
/// omitting them. See docs/dev/ocr.md.
fn legacy_envelope(bin: &str, text: &str, width: u32, height: u32) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "engine": "unknown",
        "engine_detail": format!("pre-v1 helper ({bin}): plain text only, no confidences"),
        "width": width,
        "height": height,
        "text": text.trim_end_matches('\n'),
        "lines": [],
    })
}

/// Ceiling on one `visionocr`/`winocr`/`linuxocr` invocation. It wraps model
/// inference; a wedged model must not hold the request — and the `expensive`
/// permit it acquires below — forever. See [`wait_with_timeout`].
const OCR_TIMEOUT: Duration = Duration::from_secs(30);

/// Why [`wait_with_timeout`] did not return a completed `Output`.
enum WaitError {
    Io(std::io::Error),
    TimedOut,
}

/// Wait for `child` to finish, killing it if `timeout` elapses first (Review
/// M21). Relies on the caller having set `Command::kill_on_drop(true)`:
/// dropping `wait_with_output()`'s future on timeout drops the `Child`,
/// which tokio then kills.
async fn wait_with_timeout(
    child: tokio::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, WaitError> {
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(WaitError::Io(e)),
        Err(_) => Err(WaitError::TimedOut),
    }
}

/// OCR the current warm frame by shelling out to the platform's OCR helper
/// (`visionocr` / `winocr` / `linuxocr`). The daemon links no OCR engine
/// itself — it pipes a PNG to the tool located by [`visionocr_bin`] and returns
/// the v1 envelope the helper emits under `--json` (see docs/dev/ocr.md).
async fn ocr(State(s): State<AppState>) -> Response {
    let mut rx = s.frames.clone();
    fresh_frame(&mut rx, &s.demand).await;
    let f = rx.borrow().clone();
    if f.effective_signal() == Signal::Stale {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "capture stalled; the last frame is too old to be the screen",
        )
            .into_response();
    }
    if f.signal == Signal::NoDevice || f.width == 0 {
        return (StatusCode::SERVICE_UNAVAILABLE, "no capture device").into_response();
    }
    // A dark/off display OCRs to empty text, which a caller can't tell apart
    // from a genuinely blank screen — report the missing signal instead.
    if f.signal == Signal::NoSignal {
        return (StatusCode::SERVICE_UNAVAILABLE, "no video signal").into_response();
    }
    let (fw, fh) = (f.width, f.height);
    let png = match encode_png_guarded(f, &s.expensive).await {
        Some(p) => p,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "png encode failed").into_response(),
    };

    let bin = visionocr_bin();
    // Review M21: the same gate as PNG encoding, held across the subprocess
    // too — "visionocr" is the "unbounded helpers" a burst of /ocr clicks
    // used to be able to spawn.
    let _permit = match s.expensive.acquire().await {
        Ok(p) => p,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "ocr gate closed").into_response(),
    };
    let mut child = match tokio::process::Command::new(&bin)
        .arg("--json")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                format!("visionocr unavailable ({}): {e}", bin.to_string_lossy()),
            )
                .into_response()
        }
    };

    // Write the PNG to stdin on a task while we collect stdout, so a large
    // frame can't deadlock the pipe.
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&png).await;
            // stdin dropped here -> EOF, so visionocr stops reading.
        });
    }

    let out = match wait_with_timeout(child, OCR_TIMEOUT).await {
        Ok(out) => out,
        Err(WaitError::TimedOut) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!("visionocr timed out after {OCR_TIMEOUT:?}\n"),
            )
                .into_response()
        }
        Err(WaitError::Io(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("visionocr wait: {e}"),
            )
                .into_response()
        }
    };

    if out.status.success() {
        let name = bin.to_string_lossy().into_owned();
        // A v1 helper answers with the envelope. Anything else is treated
        // as a pre-v1 helper's plain text rather than an error — see
        // legacy_envelope.
        let body = match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            Ok(v) if v.get("version").is_some() => v,
            _ => {
                let text = String::from_utf8_lossy(&out.stdout);
                tracing::warn!(
                    "OCR helper {name} did not emit a v1 envelope; \
                     treating its output as plain text (upgrade it with `panio setup`)"
                );
                legacy_envelope(&name, &text, fw, fh)
            }
        };
        (
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response()
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("visionocr failed: {}", String::from_utf8_lossy(&out.stderr)),
        )
            .into_response()
    }
}

/// PANIO_TARGET (set by `panio video watch`/`console <target>`), or None
/// when unset/empty.
fn power_target() -> Option<String> {
    std::env::var("PANIO_TARGET").ok().filter(|t| !t.is_empty())
}

/// The 501 power endpoints return when no target is configured.
fn no_target_response() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        "PANIO_TARGET not set — start the daemon with: panio video watch <target>",
    )
        .into_response()
}

/// Ceiling on one power-hook subprocess (`panio power on|off`, `power-cycle`,
/// `power-state`). Deliberately generous next to OCR's 30s: a real hook may
/// toggle a relay, wait on a smart plug's HTTP RPC, or power-cycle hardware and
/// poll it back up, none of which is instant. But a wedged hook must not pin
/// the request — nor, for the action endpoints, leave the dashboard unsure
/// whether the target ever moved — indefinitely. 60s clears a slow-but-working
/// cycle with room to spare while still bounding the hang.
const POWER_TIMEOUT: Duration = Duration::from_secs(60);

/// Spawn `panio <args…>`, wait up to `timeout`, and map the outcome to a
/// Response for the power *action* endpoints (on/off/cycle). `label` names the
/// action for error messages. `kill_on_drop(true)` plus [`wait_with_timeout`]
/// means a hook that outruns `timeout` is killed (its `Child` is dropped) and
/// answered with 504 rather than holding the request open forever (Issue #171).
async fn run_panio_action(panio: &str, args: &[&str], label: &str, timeout: Duration) -> Response {
    let child = match tokio::process::Command::new(panio)
        .args(args)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to run {panio}: {e}"),
            )
                .into_response()
        }
    };
    match wait_with_timeout(child, timeout).await {
        Ok(out) if out.status.success() => (StatusCode::OK, "ok").into_response(),
        Ok(out) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("panio {label} exited with {}", out.status),
        )
            .into_response(),
        Err(WaitError::TimedOut) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!("panio {label} timed out after {timeout:?}\n"),
        )
            .into_response(),
        Err(WaitError::Io(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to run {panio}: {e}"),
        )
            .into_response(),
    }
}

/// Run `panio <action…> <target>` and map its exit status to a Response. The
/// action endpoints (on/off/cycle) all funnel through here, so a request is the
/// only thing that ever changes the target's power.
async fn run_power_action(action: &[&str]) -> Response {
    let target = match power_target() {
        Some(t) => t,
        None => return no_target_response(),
    };
    let panio = std::env::var("PANIO_BIN").unwrap_or_else(|_| "panio".to_string());
    let mut args: Vec<&str> = action.to_vec();
    args.push(&target);
    run_panio_action(&panio, &args, &action.join(" "), POWER_TIMEOUT).await
}

/// `POST /power-cycle` — `panio power-cycle <target>`.
async fn power_cycle() -> Response {
    run_power_action(&["power-cycle"]).await
}

/// `POST /power-on` — `panio power on <target>`.
async fn power_on() -> Response {
    run_power_action(&["power", "on"]).await
}

/// `POST /power-off` — `panio power off <target>`.
async fn power_off() -> Response {
    run_power_action(&["power", "off"]).await
}

/// `GET /power` — capability + current state WITHOUT acting, so the dashboard
/// can probe availability and drive the on/off toggle on a timer without ever
/// toggling the target. 501 if no target; otherwise runs `panio power-state
/// <target>` and returns "on", "off", or "unknown".
async fn power_state() -> Response {
    let target = match power_target() {
        Some(t) => t,
        None => return no_target_response(),
    };
    let panio = std::env::var("PANIO_BIN").unwrap_or_else(|_| "panio".to_string());
    // Unlike the action hooks, this one needs stdout, so spawn with a piped
    // stdout and read it back through `wait_with_timeout` (Issue #171): same
    // `kill_on_drop(true)` + timeout treatment as `run_panio_action` and
    // `ocr`, so a wedged `power-state` hook is killed rather than holding the
    // request forever.
    let child = match tokio::process::Command::new(&panio)
        .args(["power-state", &target])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to run {panio}: {e}"),
            )
                .into_response()
        }
    };
    match wait_with_timeout(child, POWER_TIMEOUT).await {
        Ok(o) if o.status.success() => {
            let out = String::from_utf8_lossy(&o.stdout);
            (StatusCode::OK, power_state_verdict(&out)).into_response()
        }
        Ok(_) => (StatusCode::OK, "unknown").into_response(),
        Err(WaitError::TimedOut) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!("panio power-state timed out after {POWER_TIMEOUT:?}\n"),
        )
            .into_response(),
        Err(WaitError::Io(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to run {panio}: {e}"),
        )
            .into_response(),
    }
}

async fn devices() -> Response {
    // `enumerate()` is synchronous and can block (V4L ioctls on Linux, an
    // AVFoundation discovery-session query on macOS), so run it off the async
    // runtime rather than on a tokio worker (Issue #171 — same class of fix as
    // M21 for PNG encoding). It returns an owned `Vec`, so nothing needs to be
    // borrowed across the hop.
    match tokio::task::spawn_blocking(crate::capture::enumerate).await {
        Ok(Ok(list)) => Json(
            list.into_iter()
                .map(|d| {
                    serde_json::json!({"index": d.index, "name": d.name, "misc": d.misc, "id": d.id})
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("device enumeration task failed: {e}"),
        )
            .into_response(),
    }
}

fn signal_name(s: Signal) -> &'static str {
    match s {
        Signal::Stable => "stable",
        Signal::Stale => "stale",
        Signal::ModeSwitching => "mode_switching",
        Signal::NoSignal => "no_signal",
        Signal::NoDevice => "no_device",
    }
}

#[allow(unused_imports)]
use watch as _watch;

/// The dashboard's verdict from `panio power-state` output: `on`, `off` or
/// `unknown`. It prints a human line like "Power ON  (pi5)"; the on/off token
/// is pulled out of the *first* line only (case-insensitive,
/// position-independent). A target with named power levels gets one line per
/// level with the default level first, and the dashboard's controls drive
/// the default level, so a later level's line must not answer for it.
fn power_state_verdict(out: &str) -> &'static str {
    let first = out.lines().next().unwrap_or("");
    first
        .split_whitespace()
        .find_map(|t| match t.to_ascii_lowercase().as_str() {
            "on" => Some("on"),
            "off" => Some("off"),
            _ => None,
        })
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_state_verdict_reads_the_default_level_line_only() {
        assert_eq!(power_state_verdict("Power ON  (pi5)\n"), "on");
        assert_eq!(power_state_verdict("Power OFF  (pi5)\n"), "off");
        assert_eq!(
            power_state_verdict("Power OFF  (pi5)\nPower ON  (pi5, level hard)\n"),
            "off"
        );
        assert_eq!(
            power_state_verdict(
                "Power UNKNOWN  (pi5: no state_cmd)\nPower ON  (pi5, level hard)\n"
            ),
            "unknown"
        );
        assert_eq!(power_state_verdict(""), "unknown");
    }
    use crate::frame::STALE_AFTER;
    use axum::http::Request as HttpRequest;
    use futures_util::StreamExt;
    use tower::ServiceExt;

    /// A pre-v1 helper's plain text must still reach the caller as an
    /// envelope. Failing instead would look to an agent like a broken capture
    /// rather than an out-of-date helper.
    #[test]
    fn legacy_envelope_wraps_plain_text() {
        let v = legacy_envelope("linuxocr", "login:\nPassword:\n", 1280, 720);
        assert_eq!(v["version"], 1);
        assert_eq!(v["text"], "login:\nPassword:");
        assert_eq!(v["width"], 1280);
        assert_eq!(v["height"], 720);
        // No invented lines: omitting them is honest, fabricating boxes is not.
        assert_eq!(v["lines"].as_array().map(|a| a.len()), Some(0));
        // The binary is named so the cause is visible in the response itself.
        assert!(v["engine_detail"].as_str().unwrap().contains("linuxocr"));
    }

    /// `/stop` sits behind the same bearer-token layer as every other route,
    /// and only a valid token may wake the daemon's shutdown `Notify`.
    #[tokio::test]
    async fn shutdown_requires_the_daemon_token() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let (_tx, rx) = watch::channel(Arc::new(FrameState::no_device()));
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let app = router(
            AppState::new(rx),
            crate::auth::Auth::new("test-token".into(), PUBLIC_ASSETS),
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
                tokio::time::timeout(Duration::from_millis(10), shutdown.notified())
                    .await
                    .is_ok(),
                valid,
                "token {token:?} must {}wake the shutdown",
                if valid { "" } else { "not " }
            );
        }
    }

    // ── Review M20: /snapshot must not spin when the capture thread is gone ──

    /// Before the fix, dropping the capture thread's `Sender` made
    /// `rx.changed()` resolve immediately with `Err` on every poll, and only
    /// the *outer* `tokio::time::timeout` was checked — so the loop spun at
    /// 100% CPU until the deadline, then answered with whatever `png_response`
    /// made of the last frame (here, "no capture device", since the receiver
    /// never saw anything else). The fix matches on the inner `Err` directly
    /// and returns "capture thread gone" without waiting out the deadline.
    /// A short deadline (200ms) keeps this test fast either way; the
    /// `tokio::time::timeout` wrapper around the whole call fails the test
    /// outright if the handler doesn't return promptly.
    #[tokio::test]
    async fn snapshot_reports_the_capture_thread_gone_instead_of_spinning() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let (tx, rx) = watch::channel(Arc::new(FrameState::no_device()));
        drop(tx); // the capture thread's Sender is gone

        let state = AppState::new(rx);
        let app = router(state, crate::auth::Auth::new("tok".into(), PUBLIC_ASSETS));

        // `wait=stable` forces `ready == false` on the first check (a
        // `no_device` frame is never `Stable`), so the handler must actually
        // reach `rx.changed()` rather than returning before ever calling it.
        let req = HttpRequest::builder()
            .uri("/snapshot?wait=stable&timeout=200")
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, "Bearer tok")
            .body(Body::empty())
            .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(2), app.oneshot(req))
            .await
            .expect("handler did not return promptly")
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        // Distinguishes this path from the *other* 503s `png_response` can
        // give (stale / no_device), which set an `x-signal` header this path
        // does not.
        assert!(
            resp.headers()
                .get(header::HeaderName::from_static("x-signal"))
                .is_none(),
            "this 503 is the M20 path, not a png_response one"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"capture thread gone");
    }

    // ── Review M21: the OCR child is killed on timeout ───────────────────────

    /// Without a timeout, a wedged (or merely slow) `visionocr` process holds
    /// the request — and the `expensive` semaphore permit it took — forever.
    /// `sleep 5` stands in for a wedged helper: with a much shorter timeout,
    /// `wait_with_timeout` must return `TimedOut` promptly, and
    /// `kill_on_drop(true)` (set on the `Command` here, as `ocr()` sets it)
    /// must actually have killed the process by the time we check.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_with_timeout_kills_a_child_that_outlives_the_deadline() {
        let child = tokio::process::Command::new("sleep")
            .arg("5")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("pid") as i32;

        let start = Instant::now();
        let result = wait_with_timeout(child, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(WaitError::TimedOut)));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "did not return promptly: {:?}",
            start.elapsed()
        );

        // Give the kill a moment to land, then confirm the process is gone
        // (signal 0 only probes existence/permission, it sends nothing).
        tokio::time::sleep(Duration::from_millis(300)).await;
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(!alive, "child pid {pid} should have been killed on timeout");
    }

    /// A child that finishes on its own, well inside the deadline, must be
    /// reported normally rather than as a timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_with_timeout_returns_the_output_of_a_child_that_finishes_in_time() {
        let child = tokio::process::Command::new("echo")
            .arg("hi")
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn echo");
        let out = wait_with_timeout(child, Duration::from_secs(5))
            .await
            .unwrap_or_else(|_| panic!("should not time out"));
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    // ── /preview test helpers ─────────────────────────────────────────────

    /// A frame carrying real NV12 pixels, so the fallback encode path
    /// (`encode_preview_jpeg`) actually runs rather than short-circuiting on
    /// `PixelData::Empty`/`jpeg`.
    fn nv12_frame(w: u32, h: u32, signal: Signal, captured_at: Instant) -> FrameState {
        let y = vec![126u8; (w * h) as usize];
        let cbcr = vec![128u8; (w * (h / 2)) as usize];
        FrameState {
            jpeg: None,
            pixels: PixelData::Nv12 {
                y: Arc::from(y),
                cbcr: Arc::from(cbcr),
            },
            width: w,
            height: h,
            hash: 1,
            signal,
            resolution_epoch: 1,
            captured_at,
        }
    }

    fn preview_request() -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri("/preview")
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, "Bearer tok")
            .body(Body::empty())
            .unwrap()
    }

    fn preview_auth() -> crate::auth::Auth {
        crate::auth::Auth::new("tok".into(), PUBLIC_ASSETS)
    }

    /// Pull one chunk (one multipart part: headers + JPEG bytes + trailing
    /// CRLF) off a /preview response body stream, failing the test if none
    /// arrives within 2s.
    async fn next_chunk<S>(stream: &mut S) -> Bytes
    where
        S: futures_util::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
    {
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("no chunk within 2s")
            .expect("stream ended")
            .expect("chunk error")
    }

    /// Slice out just the JPEG bytes of one chunk: between the blank line
    /// that ends the part headers and the trailing "\r\n" this server always
    /// appends after each part's body.
    fn chunk_jpeg_payload(chunk: &[u8]) -> &[u8] {
        let sep = chunk
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("no header/body separator in chunk")
            + 4;
        &chunk[sep..chunk.len() - 2]
    }

    /// Pull one header's value out of a chunk's part headers (before the
    /// blank line). Not a real HTTP parser — good enough for a test.
    fn header_value(chunk: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(chunk);
        let needle = format!("{name}: ");
        let start = text.find(&needle)? + needle.len();
        let end = text[start..].find("\r\n")?;
        Some(text[start..start + end].to_string())
    }

    // ── Issue #142: /preview's fallback encode is bounded + coalesced ───────

    /// Before the fix, every open /preview connection ran its own unbounded
    /// `spawn_blocking` encode with no relationship to `AppState.expensive` —
    /// so a burst of preview clients could pile up encode work indefinitely
    /// while /snapshot and /ocr starved for a share of the same CPU. This
    /// test holds every `expensive` permit (standing in for busy
    /// /snapshot or /ocr requests) and checks that /preview's fallback
    /// encode does not even start; only after releasing the permits does a
    /// chunk arrive. On the old code (no `expensive.acquire()` anywhere in
    /// the fallback path) the first assertion fails outright: a chunk
    /// arrives immediately no matter who else holds permits.
    #[tokio::test]
    async fn preview_fallback_encode_is_bounded_by_the_expensive_semaphore() {
        let frame = Arc::new(nv12_frame(16, 8, Signal::Stable, Instant::now()));
        let (_tx, rx) = watch::channel(frame);
        let state = AppState::new(rx);

        // Clone the Arc first: a permit borrowed straight off `state.expensive`
        // would tie its lifetime to a borrow of `state`, and `state` needs to
        // move into `router` below while the permit is still held.
        let expensive = state.expensive.clone();
        let held = expensive
            .acquire_many(EXPENSIVE_PERMITS as u32)
            .await
            .expect("acquire every expensive permit");

        let app = router(state, preview_auth());
        let resp = app.oneshot(preview_request()).await.unwrap();
        let mut stream = resp.into_body().into_data_stream();

        let starved = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
        assert!(
            starved.is_err(),
            "a preview chunk arrived while every expensive permit was held"
        );

        drop(held);

        let chunk = next_chunk(&mut stream).await;
        assert!(chunk.starts_with(b"--frame\r\n"));
    }

    /// Two /preview connections watching the same (unchanged) frame must
    /// share one encode (Issue #142) rather than each running their own.
    /// Proven here not just by reading the cache after the first
    /// connection's chunk arrives, but by *replacing* the cached bytes with
    /// a sentinel a real encode could never produce, then checking that a
    /// second, independent connection serves that sentinel back. On the old
    /// code (no cache at all) this fails: the second connection re-encodes
    /// the real frame and never sees the sentinel.
    #[tokio::test]
    async fn preview_shares_one_encode_across_clients_on_the_same_frame() {
        const SENTINEL: &[u8] = b"SENTINEL-NOT-A-REAL-JPEG-0142";

        let frame = Arc::new(nv12_frame(16, 8, Signal::Stable, Instant::now()));
        let (_tx, rx) = watch::channel(frame);
        let state = AppState::new(rx);

        let app1 = router(state.clone(), preview_auth());
        let resp1 = app1.oneshot(preview_request()).await.unwrap();
        let mut stream1 = resp1.into_body().into_data_stream();
        let _chunk1 = next_chunk(&mut stream1).await;

        {
            let mut cache = state.preview_cache.lock().unwrap();
            let (captured_at, _) = cache.take().expect("first client populated the cache");
            *cache = Some((captured_at, Arc::from(SENTINEL)));
        }

        let app2 = router(state, preview_auth());
        let resp2 = app2.oneshot(preview_request()).await.unwrap();
        let mut stream2 = resp2.into_body().into_data_stream();
        let chunk2 = next_chunk(&mut stream2).await;

        assert_eq!(
            chunk_jpeg_payload(&chunk2),
            SENTINEL,
            "second client did not reuse the cached bytes for the same frame"
        );
    }

    // ── Issue #143: /preview must not serve a frame that has gone stale ─────

    /// A frame older than `STALE_AFTER` must not be served as-is: the first
    /// part /preview sends for it must carry `X-Signal: stale` and must NOT
    /// be the real encode of that (stale) frame. On the old code — which
    /// only checked `f.signal == NoDevice || f.width == 0`, never
    /// `effective_signal()` — this fails: the real, stale-frame encode goes
    /// out with no `X-Signal` header at all.
    #[tokio::test]
    async fn preview_serves_a_placeholder_for_a_stale_frame() {
        let stale_at = Instant::now() - (STALE_AFTER + Duration::from_millis(1));
        let frame = Arc::new(nv12_frame(16, 8, Signal::Stable, stale_at));
        let real_encode = encode_preview_jpeg(&frame).expect("reference encode");
        let (_tx, rx) = watch::channel(frame);
        let state = AppState::new(rx);

        let app = router(state, preview_auth());
        let resp = app.oneshot(preview_request()).await.unwrap();
        let mut stream = resp.into_body().into_data_stream();
        let chunk = next_chunk(&mut stream).await;

        assert_eq!(header_value(&chunk, "X-Signal").as_deref(), Some("stale"));
        assert_ne!(
            chunk_jpeg_payload(&chunk),
            real_encode.as_slice(),
            "served the real (stale) frame instead of a placeholder"
        );
    }

    /// The mirror image of the previous test: a fresh `Stable` frame must
    /// still be served for real, byte-for-byte what `encode_preview_jpeg`
    /// produces, carrying `X-Signal: stable`. Guards against an
    /// over-eager staleness check swallowing live frames too.
    #[tokio::test]
    async fn preview_serves_the_real_encode_for_a_fresh_stable_frame() {
        let frame = Arc::new(nv12_frame(16, 8, Signal::Stable, Instant::now()));
        let real_encode = encode_preview_jpeg(&frame).expect("reference encode");
        let (_tx, rx) = watch::channel(frame);
        let state = AppState::new(rx);

        let app = router(state, preview_auth());
        let resp = app.oneshot(preview_request()).await.unwrap();
        let mut stream = resp.into_body().into_data_stream();
        let chunk = next_chunk(&mut stream).await;

        assert_eq!(header_value(&chunk, "X-Signal").as_deref(), Some("stable"));
        assert_eq!(chunk_jpeg_payload(&chunk), real_encode.as_slice());
    }

    /// A stale placeholder must be sent once per transition, not every
    /// tick: after the first placeholder part, no further chunk should
    /// arrive while the signal stays `Stale`. This particular frame's fixed
    /// `captured_at` means the *old* code's `last_served == Some(captured_at)`
    /// dedup would also have suppressed a repeat, for the wrong reason —
    /// this test guards the new `Served::Placeholder` dedup against a
    /// regression where every tick re-encodes and re-sends the placeholder,
    /// rather than discriminating old vs. new code on its own.
    #[tokio::test]
    async fn preview_does_not_repeat_the_stale_placeholder_every_tick() {
        let stale_at = Instant::now() - (STALE_AFTER + Duration::from_millis(1));
        let frame = Arc::new(nv12_frame(16, 8, Signal::Stable, stale_at));
        let (_tx, rx) = watch::channel(frame);
        let state = AppState::new(rx);

        let app = router(state, preview_auth());
        let resp = app.oneshot(preview_request()).await.unwrap();
        let mut stream = resp.into_body().into_data_stream();
        let _first = next_chunk(&mut stream).await;

        let second = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
        assert!(
            second.is_err(),
            "the stale placeholder was resent on a later tick"
        );
    }

    // ── Issue #169: an un-encodable frame is attempted once, not every tick ──

    /// A `PixelData::Rgb` frame whose buffer is the wrong length for its
    /// dimensions: `ImageBuffer::from_raw` rejects it, so `encode_preview_jpeg`
    /// returns `None` and /preview's fallback encode fails for this frame.
    fn bad_rgb_frame(w: u32, h: u32, signal: Signal, captured_at: Instant) -> FrameState {
        FrameState {
            jpeg: None,
            // Far shorter than w*h*3 — a malformed buffer.
            pixels: PixelData::Rgb(Arc::from(vec![0u8; 3])),
            width: w,
            height: h,
            hash: 7,
            signal,
            resolution_epoch: 1,
            captured_at,
        }
    }

    /// The failing encode really does fail, so the tests below drive the
    /// Issue #169 path rather than a frame that quietly encodes.
    #[test]
    fn encode_preview_jpeg_rejects_a_malformed_rgb_frame() {
        let bad = bad_rgb_frame(16, 8, Signal::Stable, Instant::now());
        assert!(encode_preview_jpeg(&bad).is_none());
    }

    /// The cursor bookkeeping that fixes Issue #169, exercised the way
    /// `preview()` uses it. A single un-encodable frame stays in the watch
    /// channel with a fixed `captured_at`; across many ticks the loop must run
    /// the expensive encode for it exactly once — the failed attempt advances
    /// the cursor to `Served::Failed`, and every later tick skips it.
    ///
    /// On the old code there was no `Served::Failed`: the failed-encode arm did
    /// a bare `continue` that left `last_served` untouched, so the equivalent
    /// of `should_attempt_live` returned `true` on every tick and this loop
    /// would count 10 attempts, not 1.
    #[test]
    fn an_unencodable_frame_is_attempted_once_across_many_ticks() {
        let stuck = Instant::now();
        let mut last_served: Option<Served> = None;
        let mut attempts = 0usize;
        for _ in 0..10 {
            if !should_attempt_live(&last_served, stuck) {
                continue;
            }
            // Where preview() takes an `expensive` permit and spawns the
            // encode. It fails, so the cursor advances to `Failed`.
            attempts += 1;
            last_served = Some(Served::Failed(stuck));
        }
        assert_eq!(
            attempts, 1,
            "an un-encodable frame must be attempted once, not once per tick"
        );

        // A new, encodable frame resumes normal service whatever the last
        // outcome was.
        let fresh = stuck + crate::frame::TARGET_FRAME_INTERVAL;
        assert!(should_attempt_live(&last_served, fresh));
        // And a frame already served for real is likewise not re-attempted.
        assert!(!should_attempt_live(&Some(Served::Frame(fresh)), fresh));
    }

    /// End-to-end through `preview()`: an un-encodable frame yields no chunk
    /// and, crucially, does not wedge the stream — a later encodable frame is
    /// served normally. (The once-vs-every-tick attempt count is asserted by
    /// the cursor test above; this proves the real handler drives that cursor
    /// and recovers.)
    #[tokio::test]
    async fn preview_recovers_after_an_unencodable_frame() {
        let bad = Arc::new(bad_rgb_frame(16, 8, Signal::Stable, Instant::now()));
        let (tx, rx) = watch::channel(bad);
        let state = AppState::new(rx);

        let app = router(state, preview_auth());
        let resp = app.oneshot(preview_request()).await.unwrap();
        let mut stream = resp.into_body().into_data_stream();

        // The malformed frame is never put on the wire.
        let none = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
        assert!(
            none.is_err(),
            "an un-encodable frame must not yield a chunk"
        );

        // A subsequent good frame is served for real: the failed cursor did not
        // wedge the loop.
        let good = nv12_frame(16, 8, Signal::Stable, Instant::now());
        let good_encode = encode_preview_jpeg(&good).expect("reference encode");
        tx.send(Arc::new(good)).unwrap();

        let chunk = next_chunk(&mut stream).await;
        assert_eq!(chunk_jpeg_payload(&chunk), good_encode.as_slice());
    }

    // ── Issue #170: /snapshot wait=stable&changed_since needs BOTH ──────────

    /// A `Stable` NV12 frame with a chosen hash, for the readiness predicate.
    fn stable_frame_with_hash(hash: u64) -> FrameState {
        FrameState {
            hash,
            ..nv12_frame(16, 8, Signal::Stable, Instant::now())
        }
    }

    /// The readiness predicate: with both `wait=stable` and `changed_since`
    /// given, a stable-but-unchanged frame is NOT ready (Issue #170); each
    /// single condition still stands on its own. On the old code the
    /// both-given case answered on stability alone, so the marked assertion
    /// (`!ready` for a stable frame whose hash equals the caller's) failed.
    #[test]
    fn snapshot_ready_requires_both_when_stable_and_changed_since_are_given() {
        let h = 0xABCDu64;
        let stable = stable_frame_with_hash(h);

        // wait=stable alone: a stable frame is ready regardless of hash.
        assert!(snapshot_ready(&stable, true, None));
        assert!(snapshot_ready(&stable, true, Some(0x1234)));

        // Both given, hash EQUALS the caller's -> not ready (must also differ).
        assert!(
            !snapshot_ready(&stable, true, Some(h)),
            "a stable frame the caller already has must not satisfy the wait"
        );
        // Both given, hash differs -> ready.
        assert!(snapshot_ready(&stable, true, Some(h + 1)));

        // changed_since alone keys purely on hash, stability aside.
        let switching = FrameState {
            signal: Signal::ModeSwitching,
            ..stable_frame_with_hash(h)
        };
        assert!(snapshot_ready(&switching, false, Some(0x1234))); // differs
        assert!(!snapshot_ready(&switching, false, Some(h))); // same
                                                              // Neither condition -> any frame is ready.
        assert!(snapshot_ready(&switching, false, None));
    }

    /// End-to-end through the `/snapshot` handler. `nv12_frame` publishes a
    /// stable frame with `hash == 1`. Asking for `wait=stable&changed_since=1`
    /// names the frame the caller already has, so the handler must WAIT out the
    /// (short) deadline — the timed-out PNG carries `x-timeout: 1`. On the old
    /// code the both-given case answered on stability alone and returned this
    /// very frame at once, with `x-timeout: 0`. `wait=stable` alone still
    /// returns the stable frame promptly (`x-timeout: 0`).
    #[tokio::test]
    async fn snapshot_stable_and_changed_since_waits_for_a_different_frame() {
        let frame = Arc::new(nv12_frame(16, 8, Signal::Stable, Instant::now()));
        // Keep the Sender alive so `rx.changed()` never fires: the loop must
        // reach its deadline rather than being handed a new frame.
        let (_tx, rx) = watch::channel(frame);
        let state = AppState::new(rx);
        let app = router(state, preview_auth());

        let waited = app
            .clone()
            .oneshot(snapshot_request(
                "/snapshot?wait=stable&changed_since=1&timeout=200",
            ))
            .await
            .unwrap();
        assert_eq!(waited.status(), StatusCode::OK);
        assert_eq!(
            header_of(&waited, "x-timeout").as_deref(),
            Some("1"),
            "stable+changed_since with the caller's own hash must wait, not return at once"
        );

        // The mirror: `wait=stable` alone is satisfied immediately.
        let prompt = app
            .oneshot(snapshot_request("/snapshot?wait=stable&timeout=200"))
            .await
            .unwrap();
        assert_eq!(prompt.status(), StatusCode::OK);
        assert_eq!(
            header_of(&prompt, "x-timeout").as_deref(),
            Some("0"),
            "wait=stable alone must return the stable frame promptly"
        );
    }

    fn snapshot_request(uri: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri(uri)
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, "Bearer tok")
            .body(Body::empty())
            .unwrap()
    }

    fn header_of(resp: &Response, name: &'static str) -> Option<String> {
        resp.headers()
            .get(header::HeaderName::from_static(name))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    // ── Issue #171: power hooks time out; devices() runs off-thread ─────────

    /// A power-action hook that outlives its timeout is killed and answered
    /// promptly with 504, rather than pinning the request (and, before the
    /// fix, blocking with no timeout at all). Modeled on
    /// `wait_with_timeout_kills_a_child_that_outlives_the_deadline`, but driven
    /// through the action mapper the endpoints use. `sh -c 'sleep 30' <target>`
    /// stands in for a wedged hook; a 200ms timeout must fire well inside 2s.
    #[cfg(unix)]
    #[tokio::test]
    async fn power_action_times_out_and_returns_promptly() {
        let start = Instant::now();
        // Args as run_power_action builds them: the resolved target is the
        // trailing arg (here it becomes $0 for `sh -c`, harmlessly).
        let resp = run_panio_action(
            "sh",
            &["-c", "sleep 30", "target"],
            "power on",
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "power action did not return promptly on timeout: {:?}",
            start.elapsed()
        );
    }

    /// A hook that finishes inside the timeout is reported on its exit status,
    /// not as a timeout: `true` -> 200 "ok", `false` -> 500.
    #[cfg(unix)]
    #[tokio::test]
    async fn power_action_maps_exit_status_within_the_timeout() {
        let ok = run_panio_action("true", &[], "power on", Duration::from_secs(5)).await;
        assert_eq!(ok.status(), StatusCode::OK);

        let bad = run_panio_action("false", &[], "power off", Duration::from_secs(5)).await;
        assert_eq!(bad.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// `devices()` now runs `enumerate()` on `spawn_blocking`; the off-thread
    /// hop must not change what the endpoint returns. Compare the handler's
    /// JSON against a direct, inline `enumerate()` on this same machine
    /// (behavior unchanged — this is a preservation test, not a discriminating
    /// one). If `enumerate()` itself errors here, the handler must surface a
    /// 500, not hang or panic.
    #[tokio::test]
    async fn devices_enumerates_off_thread_with_unchanged_result() {
        let direct = crate::capture::enumerate();
        let (_tx, rx) = watch::channel(Arc::new(FrameState::no_device()));
        let app = router(AppState::new(rx), preview_auth());
        let req = HttpRequest::builder()
            .uri("/devices")
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, "Bearer tok")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        match direct {
            Ok(list) => {
                assert_eq!(resp.status(), StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap();
                let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let arr = got.as_array().expect("devices returns a JSON array");
                assert_eq!(arr.len(), list.len(), "device count changed off-thread");
                for (v, d) in arr.iter().zip(list.iter()) {
                    assert_eq!(v["index"], d.index);
                    assert_eq!(v["name"], d.name);
                    assert_eq!(v["id"], d.id);
                }
            }
            Err(_) => {
                assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    // ── #221: demand-paced capture ───────────────────────────────────────

    /// A pull that finds the warm frame too old waits for the next one, so
    /// an idle capture loop never hands an agent a screen from before its
    /// last action.
    #[tokio::test]
    async fn a_pull_on_an_old_frame_waits_for_a_fresh_one() {
        let old = Instant::now() - Duration::from_secs(1);
        let (tx, mut rx) = watch::channel(Arc::new(nv12_frame(16, 8, Signal::Stable, old)));
        let demand = Demand::new();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = tx.send(Arc::new(nv12_frame(16, 8, Signal::Stable, Instant::now())));
            // Keep the sender alive past the wait, as the capture thread would.
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        fresh_frame(&mut rx, &demand).await;
        assert!(
            rx.borrow().captured_at.elapsed() < Duration::from_millis(500),
            "the pull must come back with the new frame, not the one from a second ago"
        );
    }

    /// A frame that is already fresh is served at once.
    #[tokio::test]
    async fn a_pull_on_a_fresh_frame_does_not_wait() {
        let (_tx, mut rx) =
            watch::channel(Arc::new(nv12_frame(16, 8, Signal::Stable, Instant::now())));
        let start = Instant::now();
        fresh_frame(&mut rx, &Demand::new()).await;
        assert!(start.elapsed() < FRESH_WAIT / 2);
    }

    /// A snapshot waiting for a change holds the capture loop at full rate
    /// until it answers; a plain snapshot does not.
    #[tokio::test]
    async fn a_waiting_snapshot_holds_a_stream_until_it_answers() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let (tx, rx) = watch::channel(Arc::new(stable_frame_with_hash(0x1)));
        let demand = Demand::new();
        let state = AppState::new(rx).with_demand(demand.clone());
        let app = router(state, crate::auth::Auth::new("tok".into(), PUBLIC_ASSETS));

        let req = HttpRequest::builder()
            .uri("/snapshot?changed_since=1&timeout=2000")
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, "Bearer tok")
            .body(Body::empty())
            .unwrap();
        let pending = tokio::spawn(app.oneshot(req));

        let deadline = Instant::now() + Duration::from_secs(1);
        while demand.stream_count() == 0 {
            assert!(
                Instant::now() < deadline,
                "the waiting snapshot never held a stream"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tx.send(Arc::new(stable_frame_with_hash(0x2))).unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("snapshot did not answer after the change")
            .unwrap()
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            demand.stream_count(),
            0,
            "the guard must drop with the answer"
        );
    }

    /// End to end through the real router: a frame that is only noise
    /// relative to the reference carries the reference's hash, so a
    /// `changed_since=<reference hash>` caller keeps waiting; a real change
    /// answers it.
    #[tokio::test]
    async fn changed_since_ignores_noise_and_sees_a_real_change() {
        use crate::frame::{classify_nv12, Samples, Settler};
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let (w, h) = (16u32, 8u32);
        let mut settler = Settler::new(16);
        let mut settled = |y: Vec<u8>| -> FrameState {
            let (digest, _) = classify_nv12(&y, w, h);
            let hash = settler.settle(digest, w, h, Some(Samples::Luma(Arc::from(y.clone()))));
            FrameState {
                hash,
                pixels: PixelData::Nv12 {
                    y: Arc::from(y),
                    cbcr: Arc::from(vec![128u8; (w * (h / 2)) as usize]),
                },
                ..nv12_frame(w, h, Signal::Stable, Instant::now())
            }
        };

        let base = vec![100u8; (w * h) as usize];
        let first = settled(base.clone());
        let reference = first.hash;
        let (tx, rx) = watch::channel(Arc::new(first));
        let app = router(
            AppState::new(rx),
            crate::auth::Auth::new("tok".into(), PUBLIC_ASSETS),
        );
        let req = |q: &str| {
            HttpRequest::builder()
                .uri(q.to_string())
                .header(header::HOST, "127.0.0.1:1")
                .header(header::AUTHORIZATION, "Bearer tok")
                .body(Body::empty())
                .unwrap()
        };
        let uri = format!("/snapshot?changed_since={reference:016x}&timeout=3000");
        let pending = tokio::spawn(app.clone().oneshot(req(&uri)));

        let mut noise = base.clone();
        noise[5] = 108;
        let noisy_frame = settled(noise);
        assert_eq!(noisy_frame.hash, reference, "noise keeps the hash");
        tx.send(Arc::new(noisy_frame)).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !pending.is_finished(),
            "noise must not satisfy changed_since"
        );

        let mut real = base.clone();
        real[5] = 200;
        let real_frame = settled(real);
        let new_hash = real_frame.hash;
        assert_ne!(new_hash, reference);
        tx.send(Arc::new(real_frame)).unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("a real change must answer changed_since")
            .unwrap()
            .unwrap();
        assert_eq!(resp.headers()["x-timeout"], "0");
        assert_eq!(
            resp.headers()["x-frame-hash"],
            format!("{new_hash:016x}").as_str()
        );

        let status = app.oneshot(req("/status")).await.unwrap();
        let body = axum::body::to_bytes(status.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["change_threshold"], 16);
    }

    /// The vendored noVNC bundle is public code: served without a token, as
    /// JavaScript, so the dashboard's `<script type="module">` can import it.
    #[tokio::test]
    async fn novnc_bundle_is_a_public_javascript_asset() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let (_tx, rx) = watch::channel(Arc::new(FrameState::no_device()));
        let app = router(
            AppState::new(rx),
            crate::auth::Auth::new("tok".into(), PUBLIC_ASSETS),
        );
        let req = HttpRequest::builder()
            .uri("/novnc.js")
            .header(header::HOST, "127.0.0.1:1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp.headers()[header::CONTENT_TYPE].to_str().unwrap();
        assert!(ctype.starts_with("text/javascript"), "{ctype}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.len() > 100_000, "bundle looks truncated");
    }

    /// Cheap guard that the dashboard still wires up the noVNC video pane:
    /// it must import the vendored bundle and dial the `/rfb` endpoint.
    #[test]
    fn dashboard_references_novnc_and_rfb() {
        let html = include_str!("../assets/index.html");
        assert!(html.contains("/novnc.js"));
        assert!(html.contains("/rfb"));
        assert!(html.contains("panio-video-mode"));
    }
}
