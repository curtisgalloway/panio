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

//! The capture thread. Owns the device, runs the warm decode loop, classifies
//! each frame, and publishes the latest FrameState into a `watch` channel.
//!
//! This is a plain std::thread, NOT a tokio task: both backends' frame waits
//! block (V4L2 DQBUF; the AVFoundation layer's condvar) and must not sit on
//! the async runtime. `watch::Sender::send` is sync, so the thread publishes
//! freely.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{info, warn};

use crate::capture::{open_backend, CapturedFrame, DeviceSpec};
use crate::demand::Demand;
use crate::frame::{
    classify_gray, classify_nv12, classify_rgb, FrameState, Samples, Settler, Signal, STABLE_FRAMES,
};
use crate::pixel::PixelData;

pub type FrameRx = watch::Receiver<Arc<FrameState>>;

/// Spawn the capture thread. Returns the receiver end and the JoinHandle.
/// `demand` sets the Linux loop's pace (#221); see [`crate::demand`].
pub fn spawn(
    spec: DeviceSpec,
    demand: Arc<Demand>,
    change_threshold: u8,
) -> (FrameRx, thread::JoinHandle<()>) {
    let (tx, rx) = watch::channel(Arc::new(FrameState::no_device()));

    let handle = thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            demand.set_capture_thread(thread::current());
            capture_loop(spec, tx, &demand, change_threshold)
        })
        .expect("failed to spawn capture thread");

    (rx, handle)
}

/// Consecutive times a flagged stall (see the watchdog in `capture_loop`) has
/// forced a backend reopen with no healthy frame in between. Review low (h):
/// the watchdog used to `process::exit(1)` on every stall, with nothing to
/// restart the daemon afterward. Reopening in place handles the common case
/// (the device recovers); this tracks how many times in a row that has
/// *not* worked, so a device that is genuinely gone still gets a bounded
/// number of tries before the daemon exits — the same last-resort escape
/// hatch the immediate-exit watchdog always was, just not the first resort.
const MAX_CONSECUTIVE_STALLS: u32 = 8;

struct StallTracker {
    consecutive: u32,
}

impl StallTracker {
    fn new() -> Self {
        StallTracker { consecutive: 0 }
    }

    /// A frame arrived: the device is healthy again.
    fn recovered(&mut self) {
        self.consecutive = 0;
    }

    /// A stall forced a reopen. `true` when this was the last straw and
    /// nothing is left to try but exiting for an external restart.
    fn stalled(&mut self) -> bool {
        self.consecutive += 1;
        self.consecutive >= MAX_CONSECUTIVE_STALLS
    }

    fn count(&self) -> u32 {
        self.consecutive
    }
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn capture_loop(
    spec: DeviceSpec,
    tx: watch::Sender<Arc<FrameState>>,
    demand: &Demand,
    change_threshold: u8,
) {
    let mut stalls = StallTracker::new();

    // Reconnect loop: if the device is absent or vanishes mid-run, publish
    // NoDevice and keep retrying so hot-plug just works.
    loop {
        let mut backend = match open_backend(&spec) {
            Ok(b) => {
                info!("capture device opened");
                b
            }
            Err(e) => {
                warn!("open failed: {e:#}");
                let _ = tx.send(Arc::new(FrameState::no_device()));
                if all_receivers_gone(&tx) {
                    return;
                }
                // Brief pause before retry. The MS2109 firmware needs time to
                // reset its isochronous endpoint state after a stream stop;
                // immediately reopening can catch it mid-reset and cause stalls.
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        // Watchdog: fallback for any stall the v4l/AVFoundation frame
        // timeout doesn't catch. The cancel flag is set when we exit the
        // inner loop normally so the watchdog doesn't fire across reconnect
        // iterations.
        //
        // `stalled` is how the watchdog hands a detected stall back to the
        // main thread rather than acting unilaterally: it sets the flag,
        // and `capture_loop`'s frame loop notices it the next time
        // `backend.frame()` returns (bounded by that call's own internal
        // timeout) and reopens the backend in place — cheaper than killing
        // the whole process, and the daemon needs no external supervisor to
        // come back from it. If the flag is *still* set a full POLL later,
        // `backend.frame()` itself never returned to let the main thread see
        // it — genuinely wedged, not just slow — and there is nothing left
        // to do in-process; that is the only remaining `process::exit`.
        let frame_count = Arc::new(AtomicU64::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        {
            let frame_count = frame_count.clone();
            let cancelled = cancelled.clone();
            let stalled = stalled.clone();
            thread::Builder::new()
                .name("stall-watchdog".into())
                .spawn(move || {
                    const GRACE: Duration = Duration::from_secs(12);
                    const POLL: Duration = Duration::from_secs(4);
                    thread::sleep(GRACE);
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut prev = frame_count.load(Ordering::Relaxed);
                    if prev == 0 {
                        warn!("no frames in {GRACE:?} after device open — flagging a stall");
                        stalled.store(true, Ordering::Relaxed);
                    }
                    loop {
                        thread::sleep(POLL);
                        if cancelled.load(Ordering::Relaxed) {
                            return;
                        }
                        let cur = frame_count.load(Ordering::Relaxed);
                        if cur == prev {
                            if stalled.swap(true, Ordering::Relaxed) {
                                warn!(
                                    "capture thread did not respond to a flagged stall \
                                     within {POLL:?} — backend.frame() appears wedged; \
                                     exiting for restart"
                                );
                                std::process::exit(1);
                            }
                            warn!(
                                "capture stalled ({POLL:?} with no new frames) — flagging a stall"
                            );
                        }
                        prev = cur;
                    }
                })
                .ok();
        }

        // Fresh per backend open: a reopened device may show anything.
        let mut settler = Settler::new(change_threshold);
        let mut last_dims = (0u32, 0u32);
        let mut epoch = 0u64;
        let mut stable_count = 0u32;
        // Linux only: the loop is rate-capped there (see below). On macOS the
        // backend blocks until the device delivers the next frame, so the
        // device's own cadence paces us.
        #[cfg(target_os = "linux")]
        let mut frame_start = Instant::now();
        // Consecutive decode errors. Transient errors (bad buffer after open,
        // UVC flush frames) are tolerated; only a sustained run triggers reconnect.
        let mut consecutive_errors = 0u32;
        const MAX_CONSECUTIVE_ERRORS: u32 = 8;

        loop {
            if all_receivers_gone(&tx) {
                info!("no receivers left; capture thread exiting");
                return;
            }

            // Review low (h). The watchdog flagged a stall: drop and reopen
            // the backend rather than waiting for `backend.frame()` to error
            // out on its own (which may never happen — that is exactly the
            // stall the v4l/AVFoundation timeout didn't catch).
            if stalled.swap(false, Ordering::Relaxed) {
                let exit = stalls.stalled();
                warn!(
                    "reopening the capture backend after a flagged stall ({}/{MAX_CONSECUTIVE_STALLS})",
                    stalls.count()
                );
                cancelled.store(true, Ordering::Relaxed);
                let _ = tx.send(Arc::new(FrameState::no_device()));
                if exit {
                    warn!(
                        "giving up after {MAX_CONSECUTIVE_STALLS} consecutive stalls \
                         — exiting for restart"
                    );
                    std::process::exit(1);
                }
                break;
            }

            let captured = match backend.frame() {
                Ok(f) => {
                    consecutive_errors = 0;
                    stalls.recovered();
                    f
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        warn!("frame error ({consecutive_errors} consecutive): {e:#}");
                        cancelled.store(true, Ordering::Relaxed);
                        let _ = tx.send(Arc::new(FrameState::no_device()));
                        break;
                    }
                    // Transient: skip this frame and try again.
                    tracing::debug!("transient frame error (#{consecutive_errors}): {e:#}");
                    continue;
                }
            };
            let (w, h) = (captured.width, captured.height);

            if (w, h) != last_dims {
                epoch += 1;
                stable_count = 0;
                last_dims = (w, h);
                info!("resolution -> {w}x{h} (epoch {epoch})");
            }

            let (digest, no_signal) = classify_captured(&captured);
            let (pw, ph, samples) = settle_input(&captured);
            let hash = settler.settle(digest, pw, ph, samples);

            let signal = if no_signal {
                stable_count = 0;
                Signal::NoSignal
            } else if stable_count < STABLE_FRAMES {
                stable_count += 1;
                Signal::ModeSwitching
            } else {
                Signal::Stable
            };

            frame_count.fetch_add(1, Ordering::Relaxed);

            // When raw JPEG bytes are available (Linux MJPEG path), the
            // preview serves them directly and snapshot/OCR re-decode on
            // demand — don't carry a redundant RGB copy in every FrameState.
            let pixels = if captured.jpeg.is_some() {
                PixelData::Empty
            } else {
                captured.pixels
            };

            let _ = tx.send(Arc::new(FrameState {
                jpeg: captured.jpeg,
                pixels,
                width: w,
                height: h,
                hash,
                signal,
                resolution_epoch: epoch,
                captured_at: Instant::now(),
            }));

            // Linux: pace the loop — the v4l device delivers as fast as we
            // dequeue, and every frame costs a decode. At most
            // TARGET_FRAME_INTERVAL (33 ms) while something is watching; the
            // dongle offers 50 fps at 1080p, so this cap, not the hardware,
            // bounds how stale the newest frame can be. Slower when nothing
            // is (#221): `demand` picks the interval and unparks this thread
            // the moment a stream opens or a pull arrives, so the new interval
            // takes effect mid-sleep. The interval is re-read on every wake.
            // macOS: no cap; the backend blocks until the next frame.
            #[cfg(target_os = "linux")]
            {
                loop {
                    let elapsed = frame_start.elapsed();
                    let interval = demand.interval();
                    if elapsed >= interval {
                        break;
                    }
                    thread::park_timeout(interval - elapsed);
                }
                frame_start = Instant::now();
            }
        }
    }
}

fn all_receivers_gone(tx: &watch::Sender<Arc<FrameState>>) -> bool {
    tx.receiver_count() == 0
}

/// One-pass strided classification of a captured frame: hash + no-signal from
/// ~1k luma samples, resolution-independent (the old full-image pass cost
/// hundreds of ms at 8 MP).
///
/// A pre-scaled luma plane wins over the pixel data, and is classified against
/// ITS dimensions rather than the frame's: the Linux MJPEG path decodes
/// grayscale at half scale (#211), so passing the frame's `width`/`height`
/// here would walk a plane a quarter the expected size.
///
/// Its own function so a test can drive that choice. Inline in the capture
/// loop it was reachable only by running a thread against real hardware, and
/// the mistake it guards against is silent: out-of-range samples read as 0, so
/// the wrong dimensions yield a plausible hash computed mostly from absent
/// data — and that hash is what `/snapshot?changed_since=` trusts.
fn classify_captured(captured: &CapturedFrame) -> (u64, bool) {
    let (w, h) = (captured.width, captured.height);
    match (&captured.luma, &captured.pixels) {
        (Some(l), _) => classify_gray(&l.data, l.width, l.height),
        (None, PixelData::Nv12 { y, .. }) => classify_nv12(y, w, h),
        (None, PixelData::Rgb(buf)) => classify_rgb(buf, w, h),
        (None, PixelData::Empty) => (0, true),
    }
}

/// What the settler compares for a frame: the dimensions of the buffer it
/// holds (the scaled plane's own, for MJPEG) and its luma samples. Cloning the
/// `Arc`s is a refcount bump.
fn settle_input(captured: &CapturedFrame) -> (u32, u32, Option<Samples>) {
    match (&captured.luma, &captured.pixels) {
        (Some(l), _) => (l.width, l.height, Some(Samples::Luma(l.data.clone()))),
        (None, PixelData::Nv12 { y, .. }) => (
            captured.width,
            captured.height,
            Some(Samples::Luma(y.clone())),
        ),
        (None, PixelData::Rgb(buf)) => (
            captured.width,
            captured.height,
            Some(Samples::Rgb(buf.clone())),
        ),
        (None, PixelData::Empty) => (captured.width, captured.height, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixel::LumaPlane;

    /// Review low (h): the escalation threshold behind "exit only after ~8
    /// consecutive reopen failures". The Nth stall in a row (not before) is
    /// the one that says "give up".
    #[test]
    fn stall_tracker_gives_up_only_on_the_nth_consecutive_stall() {
        let mut t = StallTracker::new();
        for n in 1..MAX_CONSECUTIVE_STALLS {
            assert!(!t.stalled(), "stall #{n} should not give up yet");
            assert_eq!(t.count(), n);
        }
        assert!(
            t.stalled(),
            "stall #{MAX_CONSECUTIVE_STALLS} should be the last straw"
        );
        assert_eq!(t.count(), MAX_CONSECUTIVE_STALLS);
    }

    /// A run of healthy frames between stalls resets the count: a device
    /// that mostly works and stalls only occasionally must not be exiled
    /// after enough *total* stalls across its whole uptime.
    #[test]
    fn stall_tracker_resets_on_recovery() {
        let mut t = StallTracker::new();
        for _ in 0..MAX_CONSECUTIVE_STALLS - 1 {
            assert!(!t.stalled());
        }
        t.recovered();
        assert_eq!(t.count(), 0);
        for n in 1..MAX_CONSECUTIVE_STALLS {
            assert!(
                !t.stalled(),
                "stall #{n} after recovery should not give up yet"
            );
        }
        assert!(
            t.stalled(),
            "a fresh run of stalls still gives up at the threshold"
        );
    }

    fn frame(luma: Option<LumaPlane>, pixels: PixelData, w: u32, h: u32) -> CapturedFrame {
        CapturedFrame {
            jpeg: None,
            luma,
            pixels,
            width: w,
            height: h,
        }
    }

    /// A pre-scaled luma plane is classified at its own dimensions, not the
    /// frame's (#211).
    ///
    /// The Linux MJPEG path decodes grayscale at half scale, so the plane is a
    /// quarter of the frame's pixel count. Classifying it at the frame's
    /// dimensions does not error and does not report no-signal — it reads
    /// past the plane, takes the missing samples as 0, and returns a
    /// plausible-looking hash computed mostly from absent data. That hash is
    /// what `/snapshot?changed_since=` uses to decide the screen changed, so
    /// the whole cost of getting this wrong is paid somewhere else.
    #[test]
    fn a_scaled_luma_plane_is_classified_at_the_planes_dimensions() {
        let (pw, ph) = (960u32, 540u32);
        let plane: Arc<[u8]> = vec![200u8; (pw * ph) as usize].into();
        let captured = frame(
            Some(LumaPlane {
                data: plane.clone(),
                width: pw,
                height: ph,
            }),
            PixelData::Empty,
            pw * 2,
            ph * 2,
        );

        assert_eq!(
            classify_captured(&captured),
            classify_gray(&plane, pw, ph),
            "the plane must be classified at 960x540, its own size"
        );
        assert_ne!(
            classify_captured(&captured),
            classify_gray(&plane, pw * 2, ph * 2),
            "classifying at the frame's 1920x1080 is the bug; if the two agree \
             this test cannot see it"
        );
    }

    /// The luma plane wins over the pixel data when both are present, so a
    /// backend that supplies one is never charged for the full-size pass it
    /// was built to avoid.
    #[test]
    fn a_luma_plane_takes_precedence_over_the_pixel_data() {
        let (pw, ph) = (64u32, 64u32);
        let plane: Arc<[u8]> = vec![200u8; (pw * ph) as usize].into();
        // Black RGB at the same dimensions: classified on its own it is
        // no-signal, so the two sources cannot be confused for one another.
        let rgb: Arc<[u8]> = vec![0u8; (pw * ph * 3) as usize].into();
        assert!(classify_rgb(&rgb, pw, ph).1, "the decoy must be black");

        let captured = frame(
            Some(LumaPlane {
                data: plane.clone(),
                width: pw,
                height: ph,
            }),
            PixelData::Rgb(rgb),
            pw,
            ph,
        );
        assert_eq!(classify_captured(&captured), classify_gray(&plane, pw, ph));
        assert!(
            !classify_captured(&captured).1,
            "a lit luma plane must not be reported as no-signal because the \
             pixel data happens to be black"
        );
    }

    /// No plane and no pixels is no signal, not a hash of nothing.
    #[test]
    fn a_frame_with_neither_plane_nor_pixels_is_no_signal() {
        assert_eq!(
            classify_captured(&frame(None, PixelData::Empty, 1920, 1080)),
            (0, true)
        );
    }

    /// The settler compares the scaled plane at the plane's own size, never the
    /// frame's (the same trap as #211): its dimensions and length must agree.
    #[test]
    fn settle_input_for_a_scaled_plane_uses_the_planes_dimensions() {
        let (pw, ph) = (960u32, 540u32);
        let plane: Arc<[u8]> = vec![200u8; (pw * ph) as usize].into();
        let captured = frame(
            Some(LumaPlane {
                data: plane,
                width: pw,
                height: ph,
            }),
            PixelData::Empty,
            pw * 2,
            ph * 2,
        );
        let (w, h, samples) = settle_input(&captured);
        assert_eq!((w, h), (pw, ph));
        assert!(matches!(samples, Some(Samples::Luma(l)) if l.len() == (pw * ph) as usize));
    }
}
