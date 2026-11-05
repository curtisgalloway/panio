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

//! Capture backend abstraction.
//!
//! On Linux we use the `v4l` crate directly so we can:
//!   - Call `stream.set_timeout()` to avoid an indefinite VIDIOC_DQBUF block
//!   - Keep raw MJPEG bytes for zero-cost preview serving
//!   - Use turbojpeg (libjpeg-turbo) for fast RGB decode when signal detection
//!     or OCR needs pixel data
//!
//! On macOS we use our own ObjC AVFoundation layer (src/capture_avf.m). The
//! OS UVC stack decodes MJPEG before AVFoundation — only uncompressed formats
//! are reachable — so the layer requests '420v' (bi-planar 4:2:0) and the
//! frame loop classifies the luma plane directly; RGB materializes lazily.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::pixel::{LumaPlane, PixelData};

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: String,
    pub misc: String,
    /// Stable identifier, derived from USB topology — survives reboots and
    /// enumeration-order shifts, changes only if the device moves to another
    /// port. macOS: the AVFoundation `uniqueID` (location ID + VID + PID).
    /// Linux: the `/dev/v4l/by-path/...` symlink. Empty when unavailable.
    pub id: String,
}

/// How the user asked us to pick a device.
///
/// The payloads are read only when a capture backend resolves them, so a
/// platform with no backend compiled in never touches them.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", windows)),
    allow(dead_code)
)]
pub enum DeviceSpec {
    Auto,
    Index(u32),
    Name(String),
    /// A network RFB source (`rfb+ws://…`, `rfb://…`, `rfb+discovery:`);
    /// the raw string, parsed by [`crate::rfb::RfbTarget::parse`].
    Rfb(String),
}

impl DeviceSpec {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if crate::rfb::is_rfb_spec(s) {
            DeviceSpec::Rfb(s.to_string())
        } else if s.is_empty() || s.eq_ignore_ascii_case("auto") {
            DeviceSpec::Auto
        } else if let Ok(i) = s.parse::<u32>() {
            DeviceSpec::Index(i)
        } else if let Some(idx) = s
            .strip_prefix("/dev/video")
            .and_then(|n| n.parse::<u32>().ok())
        {
            DeviceSpec::Index(idx)
        } else {
            DeviceSpec::Name(s.to_string())
        }
    }
}

// Only a real capture backend resolves a device, so on a backend-less platform
// these are unreferenced by design rather than dead.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", windows)),
    allow(dead_code)
)]
const BUILTIN_HINTS: &[&str] = &["facetime", "built-in", "integrated", "isight"];

/// One captured frame in its native form. `jpeg` carries raw MJPEG bytes when
/// available (Linux MJPEG tee); `pixels` carries decoded data for
/// classification and lazy RGB conversion.
pub struct CapturedFrame {
    /// Raw MJPEG bytes from the device. Present on the Linux v4l path when
    /// the device is in MJPEG mode; None on macOS (the OS decodes upstream).
    pub jpeg: Option<Arc<[u8]>>,
    /// Native pixel data: RGB on decode paths, NV12 on the macOS path, and
    /// `Empty` on the Linux MJPEG path, where `jpeg` carries the image and
    /// `luma` carries what classification needs.
    pub pixels: PixelData,
    /// A pre-scaled luma plane for classification, when the backend produced
    /// one more cheaply than a full decode. Carries its own dimensions, which
    /// are NOT `width`/`height`.
    pub luma: Option<LumaPlane>,
    pub width: u32,
    pub height: u32,
}

pub trait CaptureBackend {
    fn frame(&mut self) -> Result<CapturedFrame>;
}

/// Map V4L2 device index → its stable `/dev/v4l/by-path` symlink (the Linux
/// analogue of AVFoundation's uniqueID: derived from USB port topology).
#[cfg(target_os = "linux")]
fn stable_ids_by_index() -> std::collections::HashMap<u32, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/dev/v4l/by-path") else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(target) = std::fs::canonicalize(&path) else {
            continue;
        };
        let target = target.to_string_lossy();
        let Some(idx) = target
            .strip_prefix("/dev/video")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        map.entry(idx)
            .or_insert_with(|| path.to_string_lossy().into_owned());
    }
    map
}

#[cfg(target_os = "linux")]
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    use anyhow::Context;
    use nokhwa::query;
    use nokhwa::utils::{ApiBackend, CameraIndex};

    let cams = query(ApiBackend::Auto).context("nokhwa device query failed")?;
    let by_path = stable_ids_by_index();
    Ok(cams
        .into_iter()
        .map(|c| {
            let index = match c.index() {
                CameraIndex::Index(i) => *i,
                CameraIndex::String(_) => u32::MAX,
            };
            let id = by_path.get(&index).cloned().unwrap_or_default();
            DeviceInfo {
                index,
                name: c.human_name(),
                misc: c.description().to_string(),
                id,
            }
        })
        .collect())
}

/// True when a node's V4L2 *device* capabilities mark it as a real capture
/// device: `VIDEO_CAPTURE` set, and neither `VIDEO_OUTPUT` nor `VIDEO_M2M`
/// (SoC pipeline stages and codecs — the Pi's `pispbe-*` nodes, HEVC
/// decoders — are M2M; UVC metadata nodes lack `VIDEO_CAPTURE` entirely).
#[cfg(target_os = "linux")]
fn is_capture_caps(flags: v4l::capability::Flags) -> bool {
    use v4l::capability::Flags as F;
    flags.contains(F::VIDEO_CAPTURE)
        && !flags.intersects(F::VIDEO_OUTPUT | F::VIDEO_M2M | F::VIDEO_M2M_MPLANE)
}

/// [`enumerate`], filtered to nodes that can actually capture (see
/// [`is_capture_caps`]). Nodes nokhwa reports without a numeric index can't
/// be opened as devices at all and are dropped too. A node whose caps can't
/// be read is kept — only a confidently-internal node is hidden. This is the
/// *listing* filter (`devices` without `--all`); `resolve` keeps matching the
/// unfiltered list so an explicitly configured device always resolves.
#[cfg(target_os = "linux")]
pub fn enumerate_capture() -> Result<Vec<DeviceInfo>> {
    let mut devices = enumerate()?;
    devices.retain(|d| {
        if d.index == u32::MAX {
            return false;
        }
        match v4l::Device::new(d.index as usize) {
            Ok(dev) => dev
                .query_caps()
                // The v4l crate populates `capabilities` from device_caps.
                .map(|c| is_capture_caps(c.capabilities))
                .unwrap_or(true),
            Err(_) => true,
        }
    });
    Ok(devices)
}

#[cfg(target_os = "macos")]
pub fn enumerate_capture() -> Result<Vec<DeviceInfo>> {
    enumerate()
}

#[cfg(target_os = "macos")]
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    use std::ffi::{c_char, c_void, CStr};

    unsafe extern "C" fn collect(
        ctx: *mut c_void,
        name: *const c_char,
        unique_id: *const c_char,
        misc: *const c_char,
    ) {
        let out = unsafe { &mut *(ctx as *mut Vec<DeviceInfo>) };
        let s = |p: *const c_char| {
            if p.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        };
        out.push(DeviceInfo {
            index: out.len() as u32,
            name: s(name),
            misc: s(misc),
            id: s(unique_id),
        });
    }

    let mut out: Vec<DeviceInfo> = Vec::new();
    unsafe {
        macos::avf_capture_enumerate(collect, &mut out as *mut Vec<DeviceInfo> as *mut c_void);
    }
    Ok(out)
}

/// Resolve a spec to a V4L2 device index (Linux; macOS resolves against the
/// enumerated device list directly in AvfBackend::open).
#[cfg(target_os = "linux")]
pub fn resolve(spec: &DeviceSpec) -> Result<u32> {
    if let DeviceSpec::Index(i) = spec {
        return Ok(*i);
    }
    // Any path-style spec (e.g. /dev/v4l/by-id/...) canonicalizes to
    // /dev/videoN; by-path specs also hit the exact-id match in resolve_in.
    if let DeviceSpec::Name(s) = spec {
        if s.starts_with('/') {
            if let Ok(target) = std::fs::canonicalize(s) {
                if let Some(idx) = target
                    .to_string_lossy()
                    .strip_prefix("/dev/video")
                    .and_then(|n| n.parse::<u32>().ok())
                {
                    return Ok(idx);
                }
            }
        }
    }
    resolve_in(&enumerate()?, spec)
}

/// Pick a device from `devices` per `spec`. An exact stable-id match wins;
/// otherwise case-insensitive name substring, which must match exactly one
/// device — a first-match-wins guess is how two identical dongles end up
/// silently swapped.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", windows)),
    allow(dead_code)
)]
fn resolve_in(devices: &[DeviceInfo], spec: &DeviceSpec) -> Result<u32> {
    if devices.is_empty() {
        return Err(anyhow!("no capture devices found"));
    }
    match spec {
        DeviceSpec::Index(i) => Ok(*i),
        DeviceSpec::Name(s) => {
            if let Some(d) = devices.iter().find(|d| !d.id.is_empty() && d.id == *s) {
                return Ok(d.index);
            }
            let sub = s.to_lowercase();
            let matches: Vec<&DeviceInfo> = devices
                .iter()
                .filter(|d| d.name.to_lowercase().contains(&sub))
                .collect();
            match matches.as_slice() {
                [] => Err(anyhow!("no device matching name or id {:?}", s)),
                [d] => Ok(d.index),
                many => Err(anyhow!(
                    "device {:?} is ambiguous ({} matches) — use a stable id:\n{}",
                    s,
                    many.len(),
                    many.iter()
                        .map(|d| format!("  {:>3}  {}  id={}", d.index, d.name, d.id))
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
            }
        }
        DeviceSpec::Rfb(s) => Err(anyhow!("{s:?} is a network source, not a capture device")),
        DeviceSpec::Auto => {
            let external = devices.iter().find(|d| {
                let n = d.name.to_lowercase();
                !BUILTIN_HINTS.iter().any(|h| n.contains(h))
            });
            Ok(external.unwrap_or(&devices[0]).index)
        }
    }
}

pub fn open_backend(spec: &DeviceSpec) -> Result<Box<dyn CaptureBackend>> {
    // A network source needs no platform backend: it is the same on all three.
    if let DeviceSpec::Rfb(s) = spec {
        return crate::rfb::RfbBackend::open(s).map(|b| Box::new(b) as Box<dyn CaptureBackend>);
    }
    #[cfg(target_os = "linux")]
    {
        linux::LinuxV4LBackend::open(spec).map(|b| Box::new(b) as Box<dyn CaptureBackend>)
    }
    #[cfg(target_os = "macos")]
    {
        macos::AvfBackend::open(spec).map(|b| Box::new(b) as Box<dyn CaptureBackend>)
    }
    #[cfg(windows)]
    {
        capture_mf::MfBackend::open(spec).map(|b| Box::new(b) as Box<dyn CaptureBackend>)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = spec;
        Err(unsupported())
    }
}

/// Windows capture, in its own file because it is as substantial as the
/// AVFoundation layer it parallels.
#[cfg(windows)]
#[path = "capture_mf.rs"]
mod capture_mf;

/// Platforms with no capture backend compiled in.
///
/// Windows would need a Media Foundation implementation to sit beside the V4L2
/// and AVFoundation ones; until that exists, every entry point fails with this
/// rather than pretending there are zero devices attached — a silent empty list
/// reads as "nothing plugged in" and sends you hunting for a hardware fault.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn unsupported() -> anyhow::Error {
    anyhow!(
        "hdmicap has no capture backend on this platform \
         (V4L2 on Linux, AVFoundation on macOS; Windows would need Media Foundation)"
    )
}

#[cfg(windows)]
pub fn enumerate_capture() -> Result<Vec<DeviceInfo>> {
    enumerate()
}

#[cfg(windows)]
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    capture_mf::enumerate()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn enumerate_capture() -> Result<Vec<DeviceInfo>> {
    Err(unsupported())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    Err(unsupported())
}

// ── Linux backend ─────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};
    use v4l::buffer::Type;
    use v4l::device::Device;
    use v4l::format::{Format, FourCC};
    use v4l::io::mmap::Stream;
    use v4l::io::traits::CaptureStream;
    use v4l::video::Capture;

    use super::{resolve, CaptureBackend, CapturedFrame, DeviceSpec};
    use crate::pixel::{yuyv_to_rgb, LumaPlane, PixelData};

    // Tried in order, highest resolution first, because VIDIOC_S_FMT does not
    // fail on an unsupported request -- it *substitutes* the driver's nearest
    // match and returns success. Asking for 720p MJPG on an MS2131 yields YUYV
    // 720p, which the loop below then accepts, so whatever sits first here is
    // effectively the only entry that ever runs. Listing 720p first therefore
    // capped every capture at 720p and put a floor under OCR error: measured
    // against a 1080p Windows desktop, 720p read as noise while the same screen
    // at 1080p gave up its icon labels and clock.
    const FORMATS: &[(u32, u32, &[u8; 4])] = &[
        (1920, 1080, b"MJPG"),
        (1920, 1080, b"YUYV"),
        (1280, 720, b"MJPG"),
        (1280, 720, b"YUYV"),
        (640, 480, b"YUYV"),
    ];

    const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

    /// How long a candidate format gets to deliver its first frame before the
    /// ladder moves on (see [`LinuxV4LBackend::open`]). Deliberately shorter
    /// than [`FRAME_TIMEOUT`]: a device that is streaming at all delivers well
    /// inside it, even at 1 fps, and this is paid once per rejected entry at
    /// open time.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    /// Bookkeeping for one walk over [`FORMATS`].
    #[derive(Default)]
    struct Ladder {
        /// Every *negotiated* `(w, h, fourcc)` already attempted. Because
        /// `VIDIOC_S_FMT` substitutes rather than fails, two different entries
        /// can land on the same actual format; without this the second one
        /// spends another [`PROBE_TIMEOUT`] re-learning that it does not work.
        tried: Vec<(u32, u32, FourCC)>,
        /// The first *requested* entry whose buffers allocated but which
        /// delivered no frame — the fallback when nothing streams at all.
        allocated: Option<(u32, u32, &'static [u8; 4])>,
    }

    impl Ladder {
        /// Record `actual` as attempted; `false` if an earlier entry already
        /// negotiated down to exactly this format, so the caller should skip it.
        fn first_try(&mut self, actual: &Format) -> bool {
            let key = (actual.width, actual.height, actual.fourcc);
            if self.tried.contains(&key) {
                return false;
            }
            self.tried.push(key);
            true
        }

        /// Note that `entry` got as far as allocating buffers. Only the first
        /// such entry is kept: `FORMATS` is ordered best-first, so the earliest
        /// one that allocated is the best fallback.
        fn note_allocated(&mut self, entry: (u32, u32, &'static [u8; 4])) {
            self.allocated.get_or_insert(entry);
        }

        fn fallback(&self) -> Option<(u32, u32, &'static [u8; 4])> {
            self.allocated
        }
    }

    pub struct LinuxV4LBackend {
        stream: Stream<'static>,
        dev: Box<Device>,
        dims: (u32, u32),
        is_mjpeg: bool,
        /// Reused across frames: constructing one per frame would put the
        /// setup cost back that the scaled decode exists to remove. Lazily
        /// built because `Decompressor::new` can fail and `accept` cannot.
        decompressor: Option<turbojpeg::Decompressor>,
    }

    /// Scale the classification decode runs at. Half, not quarter or eighth:
    /// a bright stroke `s` pixels wide averaged over an `f`x`f` block peaks
    /// near `255 * s / f`, and `classify`'s `BRIGHT` guard needs that at or
    /// above 64, so `f <= 4 * s`. One-pixel strokes are the thinnest real
    /// console text, which caps `f` at 2 -- measured, with 1/4 flipping a
    /// sparse text screen to no-signal by a single point (GitHub #211).
    const CLASSIFY_SCALE: turbojpeg::ScalingFactor = turbojpeg::ScalingFactor::ONE_HALF;

    impl LinuxV4LBackend {
        /// Open `spec` on the best format it will actually stream.
        ///
        /// Each entry of [`FORMATS`] is selected, given buffers, and then made
        /// to produce a frame. The frame is the point: allocating buffers and
        /// running a stream are different things, and a mode can pass
        /// `VIDIOC_REQBUFS` and then fail once `STREAMON` starts moving data —
        /// classically uncompressed 1080p over USB 2.0, about 4.1 MB a frame
        /// against 480 Mbps. Accepting on allocation alone turned the list into
        /// a first-match-wins constant with no way back into the loop (#188).
        ///
        /// If *nothing* delivers a frame, the first entry that at least
        /// allocated is opened anyway rather than failing: a capture device
        /// with no input signal is a normal state here (the target is off, or
        /// not booted yet), some devices deliver no frames at all in it, and
        /// the daemon must be sitting on the best format when a signal finally
        /// arrives. That keeps the no-signal case behaving exactly as it did
        /// before the probe existed.
        pub fn open(spec: &DeviceSpec) -> Result<Self> {
            let idx = resolve(spec)?;
            let dev = Box::new(
                Device::new(idx as usize).map_err(|e| anyhow!("open /dev/video{idx}: {e}"))?,
            );

            let mut ladder = Ladder::default();
            let mut last_err = anyhow!("no formats succeeded");
            for &(w, h, fourcc) in FORMATS {
                let fmt = Format::new(w, h, FourCC::new(fourcc));
                if dev.set_format(&fmt).is_err() {
                    continue;
                }
                // What the driver actually gave us, which is not necessarily
                // what was asked for.
                let actual = dev.format().unwrap_or(fmt);
                if !ladder.first_try(&actual) {
                    continue;
                }
                let dev_ref: &'static Device = unsafe { &*(dev.as_ref() as *const Device) };
                let mut stream = match Stream::with_buffers(dev_ref, Type::VideoCapture, 4) {
                    Ok(s) => s,
                    Err(e) => {
                        last_err = anyhow!("stream init {w}x{h}: {e}");
                        continue;
                    }
                };
                // `next()` is what issues STREAMON in this crate, so this call
                // is the first moment the format is really exercised. Dropping
                // the stream on failure issues STREAMOFF and releases the
                // buffers, leaving the device free for the next entry.
                stream.set_timeout(PROBE_TIMEOUT);
                if let Err(e) = stream.next() {
                    tracing::warn!(
                        "{}x{} {} allocated buffers but produced no frame in {:?}: {e}",
                        actual.width,
                        actual.height,
                        actual.fourcc,
                        PROBE_TIMEOUT
                    );
                    last_err = anyhow!(
                        "no frame from {}x{} {}: {e}",
                        actual.width,
                        actual.height,
                        actual.fourcc
                    );
                    ladder.note_allocated((w, h, fourcc));
                    continue;
                }
                stream.set_timeout(FRAME_TIMEOUT);
                tracing::info!(
                    "capture opened {}x{} {:?}",
                    actual.width,
                    actual.height,
                    if actual.fourcc == FourCC::new(b"MJPG") {
                        "MJPEG"
                    } else {
                        "YUYV"
                    }
                );
                return Ok(Self::accept(stream, dev, &actual));
            }

            let Some((w, h, fourcc)) = ladder.fallback() else {
                return Err(last_err);
            };
            let fmt = Format::new(w, h, FourCC::new(fourcc));
            dev.set_format(&fmt)
                .map_err(|e| anyhow!("re-select {w}x{h}: {e}"))?;
            let actual = dev.format().unwrap_or(fmt);
            let dev_ref: &'static Device = unsafe { &*(dev.as_ref() as *const Device) };
            let mut stream = Stream::with_buffers(dev_ref, Type::VideoCapture, 4)
                .map_err(|e| anyhow!("stream init {w}x{h}: {e}"))?;
            stream.set_timeout(FRAME_TIMEOUT);
            tracing::warn!(
                "no capture format delivered a frame; opening {}x{} {} anyway \
                 (buffers allocate — frames should follow once there is a signal)",
                actual.width,
                actual.height,
                actual.fourcc
            );
            Ok(Self::accept(stream, dev, &actual))
        }

        /// Build the backend around an accepted stream. `dev` moves into the
        /// struct; the stream's `'static` reference aliases the same heap
        /// allocation, which moving the `Box` does not disturb.
        fn accept(stream: Stream<'static>, dev: Box<Device>, actual: &Format) -> Self {
            LinuxV4LBackend {
                stream,
                dev,
                dims: (actual.width, actual.height),
                is_mjpeg: actual.fourcc == FourCC::new(b"MJPG"),
                decompressor: None,
            }
        }

        /// Decode `jpeg` to a half-scale grayscale plane for classification,
        /// and report the frame's true dimensions from its header.
        ///
        /// The header is read rather than inferred from the output, because
        /// the output is deliberately not the frame's size: taking dimensions
        /// from a scaled decode would report 960x540 for a 1080p capture and
        /// silently bump the resolution epoch on every frame.
        fn classify_plane(&mut self, jpeg: &[u8]) -> Result<(LumaPlane, u32, u32)> {
            if self.decompressor.is_none() {
                self.decompressor =
                    Some(turbojpeg::Decompressor::new().context("turbojpeg decompressor init")?);
            }
            let d = self.decompressor.as_mut().expect("just initialised");
            let header = d.read_header(jpeg).context("turbojpeg header read")?;
            d.set_scaling_factor(CLASSIFY_SCALE)
                .context("turbojpeg scaling factor")?;

            let sw = CLASSIFY_SCALE.scale(header.width);
            let sh = CLASSIFY_SCALE.scale(header.height);
            let mut plane = vec![0u8; sw * sh];
            d.decompress(
                jpeg,
                turbojpeg::Image {
                    pixels: &mut plane[..],
                    width: sw,
                    pitch: sw,
                    height: sh,
                    format: turbojpeg::PixelFormat::GRAY,
                },
            )
            .context("turbojpeg grayscale decode failed")?;

            Ok((
                LumaPlane {
                    data: Arc::from(plane.into_boxed_slice()),
                    width: sw as u32,
                    height: sh as u32,
                },
                header.width as u32,
                header.height as u32,
            ))
        }
    }

    /// The bytes of the current frame within the arena buffer v4l2 hands back.
    ///
    /// `CaptureStream::next` returns the whole mmap'd arena buffer — allocated
    /// once at the format's worst-case `sizeimage`, which for MJPG on the
    /// MS2131 is the *uncompressed* 1920x1080x2 = 4,147,200 bytes — paired with
    /// the `bytesused` the driver actually filled. Only that prefix is this
    /// frame; the rest is whatever the buffer held before.
    ///
    /// Serving the untrimmed buffer is invisible to every consumer, because
    /// every JPEG decoder stops at EOI: the browser preview, turbojpeg and the
    /// OCR path all render correctly while the preview stream ships the padding
    /// too. Measured on lab-optiplex-1 before this trim: a real 216,468-byte
    /// frame inside a 4,147,200-byte multipart part, 94.8% zeros, 335 Mbit/s on
    /// the wire to carry 17 Mbit/s of picture.
    ///
    /// `bytesused` is clamped: a driver that reports more than it allocated
    /// must not panic the capture loop.
    fn frame_bytes(buf: &[u8], bytesused: u32) -> &[u8] {
        &buf[..(bytesused as usize).min(buf.len())]
    }

    /// Buffers the stream is allocated with (`Stream::with_buffers`).
    const BUFFERS: u32 = 4;

    /// A dequeued buffer captured longer ago than this is skipped rather than
    /// served (#221). V4L2 hands buffers back oldest first: after the capture
    /// loop sleeps at its idle rate, every buffer in the queue holds a frame
    /// from the start of that sleep, and the one a pull would get is up to a
    /// second older than the screen.
    const MAX_BUFFER_AGE: Duration = crate::demand::FRESH_ENOUGH;

    /// Now on `CLOCK_MONOTONIC`, the clock V4L2 stamps buffers with when it
    /// sets `V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC`.
    fn monotonic_now() -> Option<Duration> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return None;
        }
        Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    }

    /// How long ago the driver captured this buffer, or `None` when that
    /// cannot be trusted: a driver that does not stamp with the monotonic
    /// clock, or a stamp in the future. `None` means "serve it", so a driver
    /// with odd timestamps behaves exactly as before rather than having
    /// every frame skipped.
    fn buffer_age(meta: &v4l::buffer::Metadata, now: Duration) -> Option<Duration> {
        use v4l::buffer::Flags;
        if meta.flags & Flags::TIMESTAMP_MASK != Flags::TIMESTAMP_MONOTONIC {
            return None;
        }
        let (sec, usec) = (meta.timestamp.sec, meta.timestamp.usec);
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return None;
        }
        let stamped = Duration::new(sec as u64, usec as u32 * 1000);
        now.checked_sub(stamped)
    }

    impl CaptureBackend for LinuxV4LBackend {
        fn frame(&mut self) -> Result<CapturedFrame> {
            // Skip buffers that are already too old (see `MAX_BUFFER_AGE`).
            // Each skip returns a buffer the driver filled earlier, so the
            // dequeue is immediate; after `BUFFERS` of them the queue holds
            // only buffers handed back since, and the next is fresh. Bounded
            // so a clock disagreement can cost at most one queue's worth.
            let mut skipped = 0;
            let buf: Vec<u8> = loop {
                let (buf, meta) = self.stream.next().map_err(|e| {
                    if e.kind() == io::ErrorKind::TimedOut {
                        anyhow!("frame timeout (device stalled)")
                    } else {
                        anyhow!("VIDIOC_DQBUF: {e}")
                    }
                })?;
                let stale = monotonic_now()
                    .and_then(|now| buffer_age(meta, now))
                    .is_some_and(|age| age > MAX_BUFFER_AGE);
                if stale && skipped < BUFFERS {
                    skipped += 1;
                    continue;
                }
                // Owned, so the stream's borrow ends with the loop.
                break frame_bytes(buf, meta.bytesused).to_vec();
            };
            if skipped > 0 {
                tracing::debug!("skipped {skipped} stale queued buffer(s)");
            }

            if self.is_mjpeg {
                // Keep the raw JPEG bytes for zero-cost preview serving.
                let jpeg_bytes: Arc<[u8]> = Arc::from(buf.into_boxed_slice());

                // Grayscale at half scale, for signal detection only. The
                // capture thread discards `pixels` whenever raw JPEG bytes are
                // present -- snapshot and OCR re-decode from them on demand --
                // so a full-resolution RGB decode here reconstructed ~2M
                // colour pixels, copied them into an `Arc`, and dropped both,
                // to feed a 64x64 luma lattice (GitHub #211). At 1080p that is
                // 518,400 bytes instead of 6,220,800, and no second copy.
                // From the owned copy, not `buf`: `buf` borrows the stream, so
                // classifying through it would hold that borrow across a
                // `&mut self` call. Classifying exactly the bytes served is
                // the better guarantee anyway.
                let (luma, w, h) = self.classify_plane(&jpeg_bytes)?;
                self.dims = (w, h);

                Ok(CapturedFrame {
                    jpeg: Some(jpeg_bytes),
                    pixels: PixelData::Empty,
                    luma: Some(luma),
                    width: w,
                    height: h,
                })
            } else {
                // YUYV: no raw JPEG, decode to RGB for signal detection and storage.
                let fmt = self.dev.format().ok();
                let (w, h) = fmt
                    .as_ref()
                    .map(|f| (f.width, f.height))
                    .unwrap_or(self.dims);
                self.dims = (w, h);
                let rgb = yuyv_to_rgb(&buf, w, h);
                Ok(CapturedFrame {
                    jpeg: None,
                    pixels: PixelData::Rgb(Arc::from(rgb.into_raw().into_boxed_slice())),
                    luma: None,
                    width: w,
                    height: h,
                })
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn meta(flags: v4l::buffer::Flags, sec: i64, usec: i64) -> v4l::buffer::Metadata {
            v4l::buffer::Metadata {
                flags,
                timestamp: v4l::timestamp::Timestamp { sec, usec },
                ..Default::default()
            }
        }

        /// The age of a monotonic-stamped buffer is now minus its stamp (#221).
        #[test]
        fn a_monotonic_buffer_reports_its_age() {
            let m = meta(v4l::buffer::Flags::TIMESTAMP_MONOTONIC, 100, 250_000);
            assert_eq!(
                buffer_age(&m, Duration::from_millis(100_500)),
                Some(Duration::from_millis(250))
            );
        }

        /// A timestamp that cannot be compared with the monotonic clock is
        /// never grounds to skip a frame: a driver stamping with another
        /// clock, or a stamp from the future, must behave as before.
        #[test]
        fn an_untrustworthy_timestamp_has_no_age() {
            use v4l::buffer::Flags;
            let now = Duration::from_secs(100);
            assert_eq!(buffer_age(&meta(Flags::TIMESTAMP_UNKNOWN, 1, 0), now), None);
            assert_eq!(buffer_age(&meta(Flags::TIMESTAMP_COPY, 1, 0), now), None);
            assert_eq!(
                buffer_age(&meta(Flags::TIMESTAMP_MONOTONIC, 200, 0), now),
                None,
                "a stamp after now"
            );
            assert_eq!(
                buffer_age(&meta(Flags::TIMESTAMP_MONOTONIC, -1, 0), now),
                None
            );
            assert_eq!(
                buffer_age(&meta(Flags::TIMESTAMP_MONOTONIC, 1, 1_000_000), now),
                None,
                "usec out of range"
            );
        }

        /// `VIDIOC_S_FMT` substitutes the driver's nearest match instead of
        /// failing, so asking for 1080p MJPG and then 1080p YUYV on a device
        /// that offers neither lands on the same actual format twice. Probing
        /// it twice costs a second `PROBE_TIMEOUT` for an answer already known.
        #[test]
        fn a_format_the_driver_substitutes_to_twice_is_tried_once() {
            let mut ladder = Ladder::default();
            let substituted = Format::new(1280, 720, FourCC::new(b"YUYV"));
            assert!(
                ladder.first_try(&substituted),
                "the first ask must be tried"
            );
            assert!(
                !ladder.first_try(&substituted),
                "a repeat of an already-tried negotiated format must be skipped"
            );
            // A genuinely different negotiated format is still its own attempt.
            assert!(ladder.first_try(&Format::new(1280, 720, FourCC::new(b"MJPG"))));
            assert!(ladder.first_try(&Format::new(640, 480, FourCC::new(b"YUYV"))));
        }

        /// `FORMATS` is ordered best-first, so when nothing streams the
        /// fallback must be the *first* entry that allocated, not the last one
        /// tried — otherwise a no-signal device opens at 640x480 and is stuck
        /// there when a signal finally arrives.
        #[test]
        fn the_fallback_is_the_first_entry_that_allocated() {
            let mut ladder = Ladder::default();
            assert_eq!(
                ladder.fallback(),
                None,
                "nothing allocated, nothing to fall back to"
            );
            ladder.note_allocated((1920, 1080, b"MJPG"));
            ladder.note_allocated((640, 480, b"YUYV"));
            assert_eq!(ladder.fallback(), Some((1920, 1080, b"MJPG")));
        }

        /// One MJPG arena buffer as v4l2 hands it back: a real JPEG in the
        /// first `bytesused` bytes, the rest of the worst-case `sizeimage`
        /// allocation untouched.
        fn padded_mjpg_buffer() -> (Vec<u8>, u32) {
            const SIZEIMAGE: usize = 1920 * 1080 * 2;
            let mut jpeg = vec![0xFF, 0xD8]; // SOI
            jpeg.extend(std::iter::repeat_n(0x5A, 216_464));
            jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
            assert_eq!(jpeg.len(), 216_468, "the frame measured on lab-optiplex-1");

            let used = jpeg.len() as u32;
            let mut arena = jpeg;
            arena.resize(SIZEIMAGE, 0);
            (arena, used)
        }

        /// The bug this trim fixes: the preview served the whole arena buffer,
        /// so every frame shipped ~3.9 MB of zeros after the EOI. JPEG decoders
        /// stop at EOI, so nothing rendered wrong and only the byte count told.
        #[test]
        fn a_frame_is_the_bytesused_prefix_not_the_whole_arena_buffer() {
            let (arena, used) = padded_mjpg_buffer();
            let frame = frame_bytes(&arena, used);

            assert_eq!(
                frame.len(),
                216_468,
                "the frame is bytesused long, not sizeimage long"
            );
            assert_eq!(
                &frame[frame.len() - 2..],
                &[0xFF, 0xD9],
                "the trimmed frame ends exactly at EOI"
            );
            assert_ne!(*frame.last().unwrap(), 0, "no padding survives the trim");
        }

        /// A driver that reports more than it allocated must not panic the
        /// capture loop with a slice out of range.
        #[test]
        fn a_bytesused_past_the_end_of_the_buffer_is_clamped() {
            let buf = [0xFF, 0xD8, 0xFF, 0xD9];
            assert_eq!(frame_bytes(&buf, u32::MAX), &buf);
            assert_eq!(frame_bytes(&buf, 0), &[] as &[u8]);
        }
    }
}

// ── macOS backend ─────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::sync::Arc;

    use anyhow::{anyhow, Result};

    use super::{enumerate, resolve_in, CaptureBackend, CapturedFrame, DeviceSpec};
    use crate::pixel::{yuyv_to_rgb, PixelData};

    // FourCC tags mirrored from capture_avf.m.
    const PIXFMT_NV12: u32 = 0x3432_3076; // '420v'
    const PIXFMT_YUYV: u32 = 0x7975_7673; // 'yuvs'

    /// Mirrors `AvfFrame` in capture_avf.m. Plane buffers are malloc'd by the
    /// ObjC layer; ownership passes to us and is returned via frame_free.
    #[repr(C)]
    struct AvfFrame {
        seq: u64,
        width: u32,
        height: u32,
        pixfmt: u32,
        y: *mut u8,
        y_len: usize,
        cbcr: *mut u8,
        cbcr_len: usize,
    }

    extern "C" {
        pub fn avf_capture_enumerate(
            cb: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char),
            ctx: *mut c_void,
        );
        fn avf_capture_open(
            unique_id: *const c_char,
            err: *mut c_char,
            errlen: usize,
        ) -> *mut c_void;
        fn avf_capture_wait_frame(
            h: *mut c_void,
            last_seq: u64,
            timeout_ms: u32,
            out: *mut AvfFrame,
        ) -> i32;
        fn avf_capture_frame_free(f: *mut AvfFrame);
        fn avf_capture_close(h: *mut c_void);
    }

    /// Matches the Linux backend's 5s VIDIOC_DQBUF timeout; the capture
    /// thread's reconnect loop handles the resulting error.
    const FRAME_TIMEOUT_MS: u32 = 5000;

    pub struct AvfBackend {
        handle: *mut c_void,
        last_seq: u64,
    }

    // The handle is owned and used exclusively by the capture thread; the
    // ObjC side does its own locking between the delegate queue and callers.
    unsafe impl Send for AvfBackend {}

    impl AvfBackend {
        pub fn open(spec: &DeviceSpec) -> Result<Self> {
            let devices = enumerate()?;
            let idx = resolve_in(&devices, spec)?;
            let dev = devices
                .iter()
                .find(|d| d.index == idx)
                .ok_or_else(|| anyhow!("no capture device at index {idx}"))?;
            if dev.id.is_empty() {
                return Err(anyhow!("device {:?} has no uniqueID", dev.name));
            }

            let cid = CString::new(dev.id.as_str())?;
            let mut err = [0i8; 256];
            let handle = unsafe {
                avf_capture_open(cid.as_ptr(), err.as_mut_ptr() as *mut c_char, err.len())
            };
            if handle.is_null() {
                let msg = unsafe { CStr::from_ptr(err.as_ptr() as *const c_char) };
                return Err(anyhow!(
                    "failed to open {:?}: {}",
                    dev.name,
                    msg.to_string_lossy()
                ));
            }
            tracing::info!("capture opened: {} ({})", dev.name, dev.id);
            Ok(AvfBackend {
                handle,
                last_seq: 0,
            })
        }
    }

    impl CaptureBackend for AvfBackend {
        fn frame(&mut self) -> Result<CapturedFrame> {
            let mut raw = AvfFrame {
                seq: 0,
                width: 0,
                height: 0,
                pixfmt: 0,
                y: std::ptr::null_mut(),
                y_len: 0,
                cbcr: std::ptr::null_mut(),
                cbcr_len: 0,
            };
            let rc = unsafe {
                avf_capture_wait_frame(self.handle, self.last_seq, FRAME_TIMEOUT_MS, &mut raw)
            };
            match rc {
                0 => return Err(anyhow!("frame timeout (device stalled)")),
                1 => {}
                _ => return Err(anyhow!("capture session error (device lost?)")),
            }
            self.last_seq = raw.seq;

            let (w, h) = (raw.width, raw.height);
            let y = unsafe { std::slice::from_raw_parts(raw.y, raw.y_len) };
            let frame = match raw.pixfmt {
                PIXFMT_NV12 => {
                    let cbcr = unsafe { std::slice::from_raw_parts(raw.cbcr, raw.cbcr_len) };
                    CapturedFrame {
                        jpeg: None,
                        pixels: PixelData::Nv12 {
                            y: Arc::from(y),
                            cbcr: Arc::from(cbcr),
                        },
                        luma: None,
                        width: w,
                        height: h,
                    }
                }
                PIXFMT_YUYV => {
                    let rgb = yuyv_to_rgb(y, w, h);
                    CapturedFrame {
                        jpeg: None,
                        pixels: PixelData::Rgb(Arc::from(rgb.into_raw().into_boxed_slice())),
                        luma: None,
                        width: w,
                        height: h,
                    }
                }
                other => {
                    unsafe { avf_capture_frame_free(&mut raw) };
                    return Err(anyhow!("unexpected pixel format {other:#x}"));
                }
            };
            unsafe { avf_capture_frame_free(&mut raw) };
            Ok(frame)
        }
    }

    impl Drop for AvfBackend {
        fn drop(&mut self) {
            unsafe { avf_capture_close(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(index: u32, name: &str, id: &str) -> DeviceInfo {
        DeviceInfo {
            index,
            name: name.to_string(),
            misc: String::new(),
            id: id.to_string(),
        }
    }

    fn two_dongles() -> Vec<DeviceInfo> {
        vec![
            dev(0, "USB Video", "0x8300000534d2109"),
            dev(1, "USB Video", "0x8200000534d2109"),
            dev(2, "FaceTime HD Camera", "0x1421000005ac8514"),
        ]
    }

    #[test]
    fn exact_id_match_wins() {
        let devices = two_dongles();
        let spec = DeviceSpec::parse("0x8200000534d2109");
        assert_eq!(resolve_in(&devices, &spec).unwrap(), 1);
    }

    #[test]
    fn unique_name_substring_matches() {
        let devices = two_dongles();
        let spec = DeviceSpec::parse("facetime");
        assert_eq!(resolve_in(&devices, &spec).unwrap(), 2);
    }

    #[test]
    fn ambiguous_name_is_an_error_listing_ids() {
        let devices = two_dongles();
        let spec = DeviceSpec::parse("USB Video");
        let err = resolve_in(&devices, &spec).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("0x8300000534d2109"), "{err}");
        assert!(err.contains("0x8200000534d2109"), "{err}");
    }

    #[test]
    fn no_match_is_an_error() {
        let devices = two_dongles();
        let spec = DeviceSpec::parse("Elgato");
        assert!(resolve_in(&devices, &spec).is_err());
    }

    #[test]
    fn auto_prefers_external_over_builtin() {
        let devices = vec![
            dev(0, "FaceTime HD Camera", "0x1421000005ac8514"),
            dev(1, "USB Video", "0x8300000534d2109"),
        ];
        assert_eq!(resolve_in(&devices, &DeviceSpec::Auto).unwrap(), 1);
    }

    #[test]
    fn empty_id_never_matches_empty_spec_id() {
        // A device with no stable id must not be selected by id equality.
        let devices = vec![dev(0, "USB Video", "")];
        let spec = DeviceSpec::Name(String::new());
        // Empty string parses to Auto via parse(); construct Name directly to
        // prove the id-equality guard, then expect substring match (matches).
        assert_eq!(resolve_in(&devices, &spec).unwrap(), 0);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod caps_tests {
    use super::is_capture_caps;
    use v4l::capability::Flags as F;

    #[test]
    fn capture_caps_filter() {
        // UVC video node: capture + streaming.
        assert!(is_capture_caps(F::VIDEO_CAPTURE | F::STREAMING));
        // UVC metadata node: no VIDEO_CAPTURE.
        assert!(!is_capture_caps(F::META_CAPTURE | F::STREAMING));
        // M2M pipeline stage / codec (Pi pispbe, HEVC decoder).
        assert!(!is_capture_caps(F::VIDEO_M2M | F::STREAMING));
        assert!(!is_capture_caps(
            F::VIDEO_CAPTURE | F::VIDEO_OUTPUT | F::STREAMING
        ));
    }
}
