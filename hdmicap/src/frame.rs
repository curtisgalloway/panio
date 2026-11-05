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

//! The single value that flows from the capture thread to every HTTP handler.
//!
//! Design rule: the capture thread does the cheap classification (dims, hash,
//! signal) inline, but NEVER converts or encodes full images here. RGB/PNG/JPEG
//! materialize lazily in the handler that needs bytes, so the hot loop cost is
//! bounded and independent of resolution.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::pixel::PixelData;

/// How old the newest frame may be before it stops describing the screen.
///
/// The capture loop publishes only on success: a frame error does `continue`,
/// leaving the previous `FrameState` in the watch channel with its previous
/// label. So a dead capture path reads as `Stable` forever, and callers acted
/// on it — a machine whose mains had been cut kept reporting `stable` on its
/// pre-cut desktop. Age is the only thing that distinguishes "the screen has
/// not changed" from "we have stopped being told what the screen is".
///
/// Generous next to the 30 fps capture cap, so an occasional slow frame is not
/// reported as a fault, and far below the minutes-long staleness observed.
pub const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

/// How often a frame is meant to reach a viewer: the Linux capture loop's rate
/// cap and the `/preview` stream's tick, which must be the same number.
///
/// They were separate constants until 2026-09-14 — a 100 ms capture cap against
/// a 67 ms preview tick — and the preview was the slower of the two whenever
/// capture got faster. Raising the capture cap to 30 fps therefore changed
/// nothing a browser could see: the stream still ticked at 67 ms and delivered
/// 14.9 fps, measured on lab-optiplex-1. Neither constant was wrong on its own,
/// which is why the drift went unnoticed; sharing one is what stops it
/// recurring.
///
/// The Linux loop runs at this rate only while something wants it: an open
/// `/preview`, a `/snapshot` waiting for a change, or a pull in the last
/// 300 ms. Otherwise it idles at 5 fps (#221, `crate::demand`), and a pull
/// that finds the warm frame too old waits for a fresh one.
pub const TARGET_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// How many consecutive same-resolution, non-black frames we require before
/// trusting the signal as `Stable`. A booting machine renegotiates HDMI at
/// firmware -> bootloader -> OS handoffs; the dongle emits black/torn frames
/// across each switch. This debounce stops an agent reading a black rectangle.
pub const STABLE_FRAMES: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
// `NoSignal` is the correct domain term (HDMI "no signal"); the shared `Signal`
// suffix is intentional, not the accidental redundancy this lint guards against.
#[allow(clippy::enum_variant_names)]
pub enum Signal {
    /// No capture device present / handle lost.
    NoDevice,
    /// Device is streaming but the frame is (near-)black: HDMI source off,
    /// unplugged, or mid-blank. Distinct from a stale-but-valid frame.
    NoSignal,
    /// Resolution just changed or we haven't seen enough stable frames yet.
    ModeSwitching,
    /// Frame is trustworthy for OCR / agent reading.
    Stable,
    /// The last frame is too old to describe what is on screen now. The
    /// capture loop has stopped delivering — the device errored, the source
    /// lost power, or a mode change is in flight — and what remains in the
    /// watch channel is whatever was last seen.
    Stale,
}

/// Immutable snapshot of "what's on screen right now", shared via `watch`.
#[derive(Clone)]
pub struct FrameState {
    /// Raw JPEG/MJPEG bytes as delivered by the capture device (Linux MJPEG
    /// tee). The preview endpoint serves these directly — no server-side
    /// decode/re-encode.
    pub jpeg: Option<Arc<[u8]>>,
    /// Native pixel data (NV12 on macOS, RGB on decode paths, Empty when
    /// `jpeg` carries the image). Handlers convert lazily via `crate::pixel`.
    pub pixels: PixelData,
    pub width: u32,
    pub height: u32,
    /// The settled hash (see [`Settler`]): the exact digest of the last
    /// frame that differed from its predecessor by more than the change
    /// threshold; frames within the threshold of that reference carry the
    /// reference's hash. Powers change-detection (`changed_since`).
    pub hash: u64,
    pub signal: Signal,
    /// Bumps every time the capture resolution changes. Lets a consumer notice
    /// "the machine switched video modes" even if pixel hashes happen to match.
    pub resolution_epoch: u64,
    pub captured_at: Instant,
}

impl FrameState {
    /// The signal as it applies *now*, downgrading a frame that has aged out.
    ///
    /// Read this rather than the `signal` field. The stored value describes the
    /// frame when it was captured; this describes whether it still describes
    /// the screen. `NoDevice` and `NoSignal` are already terminal answers and
    /// pass through unchanged.
    pub fn effective_signal(&self) -> Signal {
        match self.signal {
            Signal::Stable | Signal::ModeSwitching if self.captured_at.elapsed() > STALE_AFTER => {
                Signal::Stale
            }
            other => other,
        }
    }

    pub fn no_device() -> Self {
        FrameState {
            jpeg: None,
            pixels: PixelData::Empty,
            width: 0,
            height: 0,
            hash: 0,
            signal: Signal::NoDevice,
            resolution_epoch: 0,
            captured_at: Instant::now(),
        }
    }
}

/// JSON shape returned by `GET /status`. Cheap for the agent to poll.
#[derive(Serialize)]
pub struct StatusDto {
    pub signal: Signal,
    /// The daemon's `--change-threshold`: how far a pixel's luma must move
    /// from the reference frame before `hash` changes (0 = any change).
    pub change_threshold: u8,
    pub width: u32,
    pub height: u32,
    pub hash: String, // hex, so it round-trips cleanly into ?changed_since=
    pub resolution_epoch: u64,
    pub captured_at_ms_ago: u128,
}

impl StatusDto {
    pub fn new(f: &FrameState, change_threshold: u8) -> Self {
        StatusDto {
            change_threshold,
            signal: f.effective_signal(),
            width: f.width,
            height: f.height,
            hash: format!("{:016x}", f.hash),
            resolution_epoch: f.resolution_epoch,
            captured_at_ms_ago: f.captured_at.elapsed().as_millis(),
        }
    }
}

/// Edge of the luma sample lattice the no-signal test reads: 64x64, 4096
/// reads total — resolution-independent cost.
///
/// This was 32 (1024 reads) and that was too coarse to *see* console text. On
/// a 1280x720 Gigaboot screen — 1.35% of pixels at luma 255 — a 32x32 lattice
/// landed on no glyph at all: max luma seen 20, so the frame classified as
/// blank and hdmicap reported `no_signal` on a perfectly readable screen. At
/// 64x64 the same frame yields max luma 255 across 42 samples. See
/// evals/ocr/dataset/README.md for the measurements.
const GRID: u32 = 64;

/// A luma level that cannot come from an unlit panel. Well above the noise of a
/// black frame from a capture dongle, well below any legible text.
const BRIGHT: u32 = 64;

/// (Near-)black no-signal detection from a strided 64x64 lattice of luma
/// samples. `luma_at(x, y)` must return FULL-RANGE luma (0-255); callers
/// normalize video-range sources.
///
/// Replaces the old full-image no-signal scan, whose cost scaled with
/// resolution (~hundreds of ms at 8 MP — the capture loop ran at 1.4 fps
/// against the IPEVO V4K before this).
fn is_no_signal<F: FnMut(u32, u32) -> u8>(w: u32, h: u32, mut luma_at: F) -> bool {
    if w == 0 || h == 0 {
        return true;
    }

    let mut sum = 0u64;
    let mut sum_sq = 0u64;
    let mut max = 0u32;

    for gy in 0..GRID {
        // Center each sample within its lattice slot.
        let y = (gy * h + h / 2) / GRID;
        for gx in 0..GRID {
            let x = (gx * w + w / 2) / GRID;
            let l = luma_at(x.min(w - 1), y.min(h - 1)) as u32;
            sum += l as u64;
            sum_sq += (l * l) as u64;
            max = max.max(l);
        }
    }

    let n = (GRID * GRID) as u64;
    let mean = sum / n;
    let var = (sum_sq / n).saturating_sub(mean * mean);
    // Low mean + low variance => HDMI blank / lens-capped. The max-luma term is
    // the guard that mean and variance cannot provide: a screen showing a
    // handful of bright glyphs on black has a near-zero mean and, if the
    // lattice happens to miss them, a near-zero variance too. One sample above
    // BRIGHT is enough to prove something is lit, whatever the average says.
    // Conservative on purpose — this decides whether an agent is allowed to
    // read the screen at all.
    mean < 10 && var < 64 && max < BRIGHT
}

/// The frame hash: an exact 64-bit digest of every byte of the frame's pixel
/// buffer (the first `len` bytes of `bytes`), plus its dimensions. It is only
/// a "has the screen changed?" token — `/snapshot?changed_since=`, the
/// `x-frame-hash` header, `/status` — so any pixel that changes changes it.
///
/// This was an 8x8 aHash (one bit per cell: above or below the mean), which
/// could not see a few lines of console text on a 1080p screen: a command's
/// output did not flip a single cell, so `changed_since` waited out its whole
/// timeout (#258). The cost of exactness is that a blinking cursor or a
/// ticking clock is a change too. Consecutive frames of a static screen were
/// measured bit-identical on an MS2109-class MJPEG dongle, so capture noise
/// does not trip it there.
///
/// Word-at-a-time multiply-rotate (the FxHash step) with a murmur3 finalizer:
/// about a millisecond for an 8 MP luma plane. Not stable across hdmicap
/// versions and not collision-resistant against an adversary; neither is
/// needed for a token that lives as long as the daemon.
fn digest(bytes: &[u8], len: usize, w: u32, h: u32) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let bytes = &bytes[..len.min(bytes.len())];
    let mut acc = ((u64::from(w) << 32) | u64::from(h)).wrapping_mul(K);
    let (words, rest) = bytes.as_chunks::<8>();
    for &word in words {
        acc = (acc.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(K);
    }
    for &b in rest {
        acc = (acc.rotate_left(5) ^ u64::from(b)).wrapping_mul(K);
    }
    acc = (acc.rotate_left(5) ^ bytes.len() as u64).wrapping_mul(K);
    acc ^= acc >> 33;
    acc = acc.wrapping_mul(0xff51_afd7_ed55_8ccd);
    acc ^= acc >> 33;
    acc = acc.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    acc ^ (acc >> 33)
}

/// Default `--change-threshold`: a pixel's luma must move by more than this
/// (0-255 scale) before the frame counts as changed. Measured on an H.264
/// source (JetKVM) showing an unchanged screen: 316 of 2,073,600 pixels
/// differed, by at most 8 levels and never above 16, while typed text and a
/// clock tick moved over a thousand pixels by more than 16.
pub const DEFAULT_CHANGE_THRESHOLD: u8 = 16;

/// The pixels a frame offers for the threshold comparison.
///
/// Comparison is on luma everywhere: NV12 and grayscale-decoded MJPEG hand
/// over their Y plane as is (NV12's is video-range, so a level there is 255/219
/// of a full-range level; immaterial at this granularity), and RGB is reduced
/// with the same integer Rec.601 weights `classify_rgb` uses. The buffers are
/// `Arc`s so keeping one as the reference costs a refcount, not a copy.
pub enum Samples {
    Luma(Arc<[u8]>),
    Rgb(Arc<[u8]>),
}

fn rgb_luma(p: &[u8; 3]) -> u8 {
    ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8) as u8
}

impl Samples {
    fn into_luma(self) -> Arc<[u8]> {
        match self {
            Samples::Luma(l) => l,
            Samples::Rgb(rgb) => rgb.as_chunks::<3>().0.iter().map(rgb_luma).collect(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Samples::Luma(l) => l.len(),
            Samples::Rgb(rgb) => rgb.len() / 3,
        }
    }

    /// Does any pixel's luma differ from `reference` by more than `threshold`?
    /// Early-exits at the first 64-pixel block holding one.
    fn exceeds(&self, reference: &[u8], threshold: u8) -> bool {
        const BLOCK: usize = 64;
        match self {
            Samples::Luma(l) => l.chunks(BLOCK).zip(reference.chunks(BLOCK)).any(|(a, b)| {
                // No early exit inside the block, so the loop vectorizes.
                a.iter()
                    .zip(b)
                    .map(|(x, y)| x.abs_diff(*y))
                    .max()
                    .unwrap_or(0)
                    > threshold
            }),
            Samples::Rgb(rgb) => {
                rgb.chunks(BLOCK * 3)
                    .zip(reference.chunks(BLOCK))
                    .any(|(a, b)| {
                        a.as_chunks::<3>()
                            .0
                            .iter()
                            .zip(b)
                            .map(|(p, y)| rgb_luma(p).abs_diff(*y))
                            .max()
                            .unwrap_or(0)
                            > threshold
                    })
            }
        }
    }
}

struct Reference {
    /// The exact digest of the accepted frame; also the hash published for it.
    hash: u64,
    width: u32,
    height: u32,
    luma: Arc<[u8]>,
}

/// Turns the per-frame exact digest into the published hash, ignoring noise.
///
/// A lossy source (H.264) re-renders an unchanged screen slightly differently
/// each frame, so the exact digest flips on noise and `changed_since` /
/// `--stable` callers see changes that are not there. The settler keeps the
/// last *accepted* frame as a reference and publishes the reference's hash
/// for every frame whose pixels all stay within `threshold` luma levels of it.
/// Frames are compared to the REFERENCE, never to their predecessor, so slow
/// drift accumulates and is eventually accepted. A frame that does move some
/// pixel by more than `threshold` becomes the new reference and publishes its
/// own exact digest. Any single pixel counts; there is no minimum area.
///
/// Cost: a frame whose exact digest equals the reference's, or the last
/// noise frame's, is settled from the digest alone. Only a genuinely new
/// digest runs the luma compare, which exits at the first block over the
/// threshold. `threshold == 0` bypasses all of it: the exact digest is the hash.
pub struct Settler {
    threshold: u8,
    reference: Option<Reference>,
    /// Digest of the previous frame that was judged noise, so a source that
    /// repeats one noisy frame is not compared again.
    last_noise: Option<u64>,
    /// Luma compares performed; lets a test prove a static source costs none.
    compares: u64,
}

impl Settler {
    pub fn new(threshold: u8) -> Self {
        Settler {
            threshold,
            reference: None,
            last_noise: None,
            compares: 0,
        }
    }

    #[cfg(test)]
    pub fn compares(&self) -> u64 {
        self.compares
    }

    /// The hash to publish for a frame with exact digest `digest` and
    /// dimensions `w` x `h` (of the plane, for a scaled luma plane).
    pub fn settle(&mut self, digest: u64, w: u32, h: u32, samples: Option<Samples>) -> u64 {
        if self.threshold == 0 {
            return digest;
        }
        let Some(samples) = samples else {
            return digest;
        };
        if let Some(r) = &self.reference {
            if r.hash == digest || self.last_noise == Some(digest) {
                return r.hash;
            }
            if (r.width, r.height) == (w, h) && r.luma.len() == samples.len() {
                self.compares += 1;
                if !samples.exceeds(&r.luma, self.threshold) {
                    self.last_noise = Some(digest);
                    return r.hash;
                }
            }
        }
        self.reference = Some(Reference {
            hash: digest,
            width: w,
            height: h,
            luma: samples.into_luma(),
        });
        self.last_noise = None;
        digest
    }
}

/// Classify an NV12 luma plane ('420v', video-range: black=16): the frame
/// hash over the Y plane, and no-signal. Normalizes to full range so the
/// no-signal thresholds keep their meaning.
pub fn classify_nv12(y: &[u8], w: u32, h: u32) -> (u64, bool) {
    if w == 0 || h == 0 {
        return (0, true);
    }
    let no_signal = is_no_signal(w, h, |x, yy| {
        let raw = *y
            .get((yy as usize) * (w as usize) + x as usize)
            .unwrap_or(&16);
        ((raw.saturating_sub(16) as u32 * 255) / 219).min(255) as u8
    });
    (digest(y, w as usize * h as usize, w, h), no_signal)
}

/// Classify a full-range luma plane, as JPEG grayscale decoding delivers it.
///
/// No normalization, unlike [`classify_nv12`]: a JPEG's Y is already 0-255, and
/// stretching it again would push a dim screen toward `Stable` and a black one
/// toward lit.
pub fn classify_gray(y: &[u8], w: u32, h: u32) -> (u64, bool) {
    if w == 0 || h == 0 {
        return (0, true);
    }
    let no_signal = is_no_signal(w, h, |x, yy| {
        *y.get((yy as usize) * (w as usize) + x as usize)
            .unwrap_or(&0)
    });
    (digest(y, w as usize * h as usize, w, h), no_signal)
}

/// Classify a packed RGB8 buffer (Rec.601 luma, integer). The hash covers
/// all three channels, so a change of color alone counts.
pub fn classify_rgb(rgb: &[u8], w: u32, h: u32) -> (u64, bool) {
    if w == 0 || h == 0 {
        return (0, true);
    }
    let no_signal = is_no_signal(w, h, |x, y| {
        let i = ((y as usize) * (w as usize) + x as usize) * 3;
        match rgb.get(i..i + 3) {
            Some(p) => ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8) as u8,
            None => 0,
        }
    });
    (digest(rgb, w as usize * h as usize * 3, w, h), no_signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn solid_rgb(r: u8, g: u8, b: u8, w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|_| [r, g, b]).collect()
    }

    /// A scaled luma plane must be classified against ITS dimensions, not the
    /// frame's. The Linux MJPEG path decodes grayscale at half scale (#211),
    /// so passing the frame's `w`/`h` walks a plane a quarter the size.
    ///
    /// The failure is quiet rather than loud, which is why `LumaPlane` carries
    /// its own dimensions: `classify_gray` reads an out-of-range sample as 0,
    /// so roughly the first quarter of sampled rows land in real pixels and the
    /// rest come back black. The result is not an error and not "no signal" —
    /// it is a plausible-looking hash computed mostly from absent data, and the
    /// hash is what `/snapshot?changed_since=` uses to decide a screen changed.
    #[test]
    fn a_scaled_plane_classified_at_frame_dimensions_hashes_differently() {
        // A uniformly lit half-scale plane: 960x540 of mid-grey.
        let (pw, ph) = (960u32, 540u32);
        let plane = vec![200u8; (pw * ph) as usize];

        let (right_hash, no_signal) = classify_gray(&plane, pw, ph);
        assert!(!no_signal, "a lit plane at its own dimensions is lit");

        // The same bytes, classified as if they were the 1920x1080 frame.
        let (wrong_hash, wrong_no_signal) = classify_gray(&plane, pw * 2, ph * 2);
        assert!(
            !wrong_no_signal,
            "it still reads as lit, which is exactly what makes this quiet — \
             a loud failure would be easier to catch"
        );
        assert_ne!(
            right_hash, wrong_hash,
            "the wrong dimensions must at least produce a different hash; \
             equal hashes would mean this mistake is undetectable"
        );
    }

    /// JPEG grayscale is full-range, so `classify_gray` must not apply the
    /// video-range stretch `classify_nv12` does. A plane just above the
    /// `BRIGHT` guard must stay lit, not be pushed around by normalization.
    #[test]
    fn classify_gray_does_not_normalize_like_video_range() {
        let (w, h) = (128u32, 128u32);
        let mut plane = vec![0u8; (w * h) as usize];
        // One bright cell, enough for the max-luma guard and nothing else.
        for y in 0..h {
            for x in 0..16 {
                plane[(y * w + x) as usize] = 70;
            }
        }
        let (_, no_signal) = classify_gray(&plane, w, h);
        assert!(
            !no_signal,
            "70 is above BRIGHT (64), so this is lit without any stretching"
        );
    }

    #[test]
    fn black_is_no_signal() {
        let buf = solid_rgb(0, 0, 0, 320, 240);
        let (_, no_sig) = classify_rgb(&buf, 320, 240);
        assert!(no_sig);
    }

    #[test]
    fn near_black_is_no_signal() {
        // Dark grey (luma ~7) should still register as no-signal.
        let buf = solid_rgb(8, 8, 8, 320, 240);
        let (_, no_sig) = classify_rgb(&buf, 320, 240);
        assert!(no_sig);
    }

    #[test]
    fn content_frame_is_not_no_signal() {
        // Mid-grey has enough luma.
        let buf = solid_rgb(128, 128, 128, 320, 240);
        let (_, no_sig) = classify_rgb(&buf, 320, 240);
        assert!(!no_sig);
    }

    /// A boot screen is mostly black, and that must not read as "no signal".
    ///
    /// This is the regression for a real failure: hdmicap reported `no_signal`
    /// for minutes at a time across a NUC's PXE/Gigaboot phase, and labelled a
    /// clean, fully readable capture `no_signal` too. The screen was 1.35%
    /// bright pixels on black, and the sampling lattice was coarse enough to
    /// land on none of them — so mean, variance and max all said "blank".
    ///
    /// The frame below mimics that: thin bright text rows on black, at roughly
    /// the same bright fraction, positioned off the old lattice. It fails with
    /// the pre-fix `CELL_SAMPLES = 4`.
    #[test]
    fn sparse_bright_text_on_black_is_not_no_signal() {
        let (w, h) = (1280u32, 720u32);
        let mut buf = vec![0u8; (w * h * 3) as usize];
        // Nine 2px text rows, at y positions the PRE-FIX 32x32 lattice misses
        // entirely and the 64x64 lattice hits. Calibrated deliberately: a row
        // spacing chosen by eye happens to collide with the old lattice, the
        // test passes against the old code, and the regression it is supposed
        // to guard goes unguarded. If the lattice geometry changes, recompute
        // these rather than assuming they still straddle it.
        const ROWS: [u32; 9] = [4, 83, 162, 240, 319, 398, 477, 555, 634];
        let mut lit = 0u32;
        for y0 in ROWS {
            for y in y0..(y0 + 2).min(h) {
                // Dashes with gaps, like glyphs rather than a solid rule.
                for x in (0..w).filter(|x| (x / 4) % 3 != 0) {
                    let i = ((y * w + x) * 3) as usize;
                    buf[i] = 255;
                    buf[i + 1] = 255;
                    buf[i + 2] = 255;
                    lit += 1;
                }
            }
        }
        // Sanity: this is a sparse screen, not a bright one.
        let frac = lit as f64 / (w * h) as f64;
        assert!(frac < 0.03, "test frame should be sparse, got {frac:.4}");

        let (_, no_sig) = classify_rgb(&buf, w, h);
        assert!(
            !no_sig,
            "a readable boot screen classified as no_signal ({:.2}% of pixels lit)",
            frac * 100.0
        );
    }

    /// A frame that has aged out must stop claiming to be the screen.
    ///
    /// The capture loop publishes only on success, so a stalled capture leaves
    /// the last good frame in place with its old label. Observed twice on real
    /// hardware: across a mode change, and on a machine whose mains had been
    /// cut — which kept reporting `stable` on its pre-cut desktop. `--stable`
    /// and `/ocr` both gate on this, so an agent trusted a dead screen.
    #[test]
    fn an_aged_out_frame_is_stale_not_stable() {
        let fresh = FrameState {
            jpeg: None,
            pixels: PixelData::Empty,
            width: 1280,
            height: 720,
            hash: 1,
            signal: Signal::Stable,
            resolution_epoch: 0,
            captured_at: Instant::now(),
        };
        assert_eq!(fresh.effective_signal(), Signal::Stable);

        let stalled = FrameState {
            captured_at: Instant::now() - (STALE_AFTER + Duration::from_millis(1)),
            ..fresh.clone()
        };
        assert_eq!(
            stalled.effective_signal(),
            Signal::Stale,
            "a frame older than STALE_AFTER must not read as Stable"
        );

        // A device that is genuinely gone keeps saying so; staleness does not
        // overwrite a terminal answer with a vaguer one.
        let gone = FrameState {
            signal: Signal::NoDevice,
            captured_at: Instant::now() - (STALE_AFTER * 10),
            ..fresh.clone()
        };
        assert_eq!(gone.effective_signal(), Signal::NoDevice);
    }

    #[test]
    fn nv12_video_range_black_is_no_signal() {
        // '420v' black is luma 16, NOT 0 — normalization must map it under
        // the threshold or HDMI blank detection breaks on the macOS path.
        let y = vec![16u8; 320 * 240];
        let (_, no_sig) = classify_nv12(&y, 320, 240);
        assert!(no_sig);
    }

    #[test]
    fn nv12_content_is_not_no_signal() {
        let y = vec![126u8; 320 * 240];
        let (_, no_sig) = classify_nv12(&y, 320, 240);
        assert!(!no_sig);
    }

    #[test]
    fn hash_same_image_stable() {
        let buf = solid_rgb(100, 150, 200, 320, 240);
        assert_eq!(
            classify_rgb(&buf, 320, 240).0,
            classify_rgb(&buf, 320, 240).0
        );
    }

    #[test]
    fn hash_different_images_differ() {
        // Opposing horizontal gradients: same histogram, different pixels.
        let w = 320u32;
        let h = 240u32;
        let gradient = |left_dark: bool| -> Vec<u8> {
            (0..w * h)
                .flat_map(|i| {
                    let x = i % w;
                    let v: u8 = if (x < w / 2) == left_dark { 50 } else { 200 };
                    [v, v, v]
                })
                .collect()
        };
        let a = classify_rgb(&gradient(true), w, h).0;
        let b = classify_rgb(&gradient(false), w, h).0;
        assert_ne!(a, b);
    }

    /// Two lines of console output change the hash (#258). The scene is the
    /// one the bug was found on: a 1080p virtual console whose tab bar
    /// already lights the top-left cells, then a command's output below it.
    /// The old 8x8 aHash reported the same value with and without the
    /// output — those cells were already "above the mean" — so
    /// `changed_since` slept through it.
    #[test]
    fn hash_sees_a_few_lines_of_text() {
        let (w, h) = (1920u32, 1080u32);
        // 16-pixel-high "glyphs": 2-pixel strokes every 8 pixels.
        let write_line = |buf: &mut [u8], top: u32, width: u32| {
            for y in top + 2..top + 14 {
                for x in (0..width).filter(|x| x % 8 < 2) {
                    buf[(y * w + x) as usize] = 255;
                }
            }
        };
        let mut prompt = vec![0u8; (w * h) as usize];
        write_line(&mut prompt, 0, 470); // tab bar
        write_line(&mut prompt, 16, 8); // "$"
        let mut output = prompt.clone();
        write_line(&mut output, 16, 120); // "$ echo mcp-hid-ok"
        write_line(&mut output, 32, 80); // "mcp-hid-ok"
        write_line(&mut output, 48, 8); // "$"
        assert_ne!(
            classify_gray(&prompt, w, h).0,
            classify_gray(&output, w, h).0
        );
    }

    /// One changed pixel anywhere, including the very last byte, which the
    /// word loop leaves to the remainder when the length is not a multiple
    /// of 8.
    #[test]
    fn hash_sees_one_pixel() {
        let (w, h) = (321u32, 3u32); // 963 bytes: a 3-byte remainder
        let base = vec![40u8; (w * h) as usize];
        let base_hash = classify_gray(&base, w, h).0;
        for i in [0, 500, (w * h - 1) as usize] {
            let mut changed = base.clone();
            changed[i] = 41;
            assert_ne!(classify_gray(&changed, w, h).0, base_hash, "pixel {i}");
        }
    }

    #[test]
    fn zero_dims_is_no_signal() {
        let (hash, no_sig) = classify_rgb(&[], 0, 0);
        assert_eq!(hash, 0);
        assert!(no_sig);
    }

    // ── Settler (change threshold) ─────────────────────────────────────────

    const SW: u32 = 64;
    const SH: u32 = 48;

    fn base_plane() -> Vec<u8> {
        (0..SW * SH).map(|i| ((i * 7) % 200) as u8 + 20).collect()
    }

    /// Run one luma plane through the real digest + settler.
    fn settle_luma(s: &mut Settler, plane: &[u8]) -> u64 {
        let (d, _) = classify_gray(plane, SW, SH);
        s.settle(d, SW, SH, Some(Samples::Luma(Arc::from(plane))))
    }

    fn to_rgb(plane: &[u8]) -> Vec<u8> {
        plane.iter().flat_map(|&l| [l, l, l]).collect()
    }

    fn settle_rgb(s: &mut Settler, rgb: &[u8]) -> u64 {
        let (d, _) = classify_rgb(rgb, SW, SH);
        s.settle(d, SW, SH, Some(Samples::Rgb(Arc::from(rgb))))
    }

    /// Deterministic pseudo-random noise of at most +-`amp` on a few pixels.
    fn noisy(plane: &[u8], amp: i16, seed: u32) -> Vec<u8> {
        let mut out = plane.to_vec();
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(12345);
        for _ in 0..40 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let i = (x >> 8) as usize % out.len();
            let d = ((x >> 3) % (2 * amp as u32 + 1)) as i16 - amp;
            out[i] = (out[i] as i16 + d).clamp(0, 255) as u8;
        }
        out
    }

    #[test]
    fn noise_within_the_threshold_keeps_the_hash() {
        let mut s = Settler::new(16);
        let base = base_plane();
        let h0 = settle_luma(&mut s, &base);
        for seed in 0..50 {
            let f = noisy(&base, 8, seed);
            assert_eq!(settle_luma(&mut s, &f), h0, "noise frame {seed}");
        }
        assert!(s.compares() > 0, "the noise frames must have been compared");
    }

    #[test]
    fn noise_within_the_threshold_keeps_the_hash_in_rgb() {
        let mut s = Settler::new(16);
        let base = base_plane();
        let h0 = settle_rgb(&mut s, &to_rgb(&base));
        for seed in 0..50 {
            let f = to_rgb(&noisy(&base, 8, seed));
            assert_eq!(settle_rgb(&mut s, &f), h0, "noise frame {seed}");
        }
    }

    #[test]
    fn one_pixel_beyond_the_threshold_is_a_change() {
        let mut s = Settler::new(16);
        let base = base_plane();
        let h0 = settle_luma(&mut s, &base);
        let mut f = base.clone();
        f[500] = f[500].saturating_add(40);
        let h1 = settle_luma(&mut s, &f);
        assert_ne!(h1, h0);
        assert_eq!(
            h1,
            classify_gray(&f, SW, SH).0,
            "the accepted hash is exact"
        );
        // ...and it is the new reference.
        assert_eq!(settle_luma(&mut s, &f), h1);
        let mut g = to_rgb(&base);
        let mut s2 = Settler::new(16);
        let r0 = settle_rgb(&mut s2, &g);
        g[500 * 3 + 1] = g[500 * 3 + 1].saturating_add(80);
        assert_ne!(settle_rgb(&mut s2, &g), r0);
    }

    /// Exactly the threshold is noise; one level more is a change.
    #[test]
    fn the_threshold_itself_is_still_noise() {
        let mut s = Settler::new(16);
        let mut base = base_plane();
        base[10] = 100;
        let h0 = settle_luma(&mut s, &base);
        base[10] = 116;
        assert_eq!(settle_luma(&mut s, &base), h0);
        base[10] = 117;
        assert_ne!(settle_luma(&mut s, &base), h0);
    }

    /// Slow drift is measured against the reference, not the previous frame,
    /// so it is caught once it adds up.
    #[test]
    fn slow_drift_is_caught_once_it_exceeds_the_threshold() {
        let mut s = Settler::new(16);
        let base = vec![100u8; (SW * SH) as usize];
        let h0 = settle_luma(&mut s, &base);
        let mut first_change = None;
        for step in 1..=20u8 {
            let f = vec![100 + step; (SW * SH) as usize];
            if settle_luma(&mut s, &f) != h0 {
                first_change = Some(step);
                break;
            }
        }
        assert_eq!(first_change, Some(17), "drift of 17 levels > 16");
    }

    /// Threshold 0 is the old behavior: the exact digest, every time.
    #[test]
    fn threshold_zero_is_the_exact_digest() {
        let mut s = Settler::new(0);
        let base = base_plane();
        for seed in 0..10 {
            let f = noisy(&base, 1, seed);
            assert_eq!(settle_luma(&mut s, &f), classify_gray(&f, SW, SH).0);
        }
        let mut f = base.clone();
        f[3] = f[3].wrapping_add(1);
        assert_ne!(settle_luma(&mut s, &f), settle_luma(&mut s, &base));
        assert_eq!(s.compares(), 0, "no luma compare at threshold 0");
    }

    #[test]
    fn a_resolution_change_is_a_change() {
        let mut s = Settler::new(16);
        let a = vec![100u8; (SW * SH) as usize];
        let ha = settle_luma(&mut s, &a);
        let b = vec![100u8; (SW * SH / 2) as usize];
        let (d, _) = classify_gray(&b, SW, SH / 2);
        let hb = s.settle(d, SW, SH / 2, Some(Samples::Luma(Arc::from(b))));
        assert_ne!(hb, ha);
        assert_eq!(hb, d);
    }

    /// A bit-identical (deterministic) source costs no luma compares at all:
    /// the digest equals the reference's.
    #[test]
    fn a_static_source_is_never_compared() {
        let mut s = Settler::new(16);
        let base = base_plane();
        let h0 = settle_luma(&mut s, &base);
        for _ in 0..100 {
            assert_eq!(settle_luma(&mut s, &base), h0);
        }
        assert_eq!(s.compares(), 0);
    }

    /// A source that repeats one noisy frame is compared once.
    #[test]
    fn a_repeated_noise_frame_is_compared_once() {
        let mut s = Settler::new(16);
        let base = base_plane();
        let h0 = settle_luma(&mut s, &base);
        let n = noisy(&base, 8, 3);
        for _ in 0..10 {
            assert_eq!(settle_luma(&mut s, &n), h0);
        }
        assert_eq!(s.compares(), 1);
    }

    /// Frames without samples (nothing to compare) publish their digest.
    #[test]
    fn a_frame_without_samples_publishes_its_digest() {
        let mut s = Settler::new(16);
        assert_eq!(s.settle(42, SW, SH, None), 42);
    }
}
