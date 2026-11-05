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

//! Windows capture: our own Media Foundation layer.
//!
//! The third backend beside V4L2 (Linux) and AVFoundation (macOS), and written
//! the same way: thin platform bindings (Microsoft's own `windows` crate),
//! with the backend logic ours. No third-party camera abstraction — the same
//! call made in PR #39 when the macOS path stopped using nokhwa.
//!
//! It delivers **NV12**, which [`crate::pixel::PixelData::Nv12`] already
//! carries for the macOS path, so every downstream consumer — RGB conversion,
//! JPEG encode, the preview stream — works unchanged.
//!
//! Two rules learned from the AVFoundation layer apply here too, for the same
//! MS2109-class HDMI sticks:
//!
//! 1. **Never let the OS pick the format.** Media Foundation will happily hand
//!    back a scaled, converted stream if asked vaguely. We enumerate the
//!    device's *native* media types and select one explicitly, so the frame is
//!    the panel's real resolution rather than something rescaled to 1080p.
//! 2. **Prefer the device's own largest NV12 mode.** These sticks advertise
//!    several; the biggest is the one that matches the source.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFMediaBuffer, IMFMediaSource, IMFMediaType, IMFSourceReader,
    MFCreateAttributes, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources,
    MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_NV12, MFSTARTUP_FULL,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use super::{CaptureBackend, CapturedFrame, DeviceInfo, DeviceSpec};
use crate::pixel::{self, PixelData};

/// Start Media Foundation once per process.
///
/// `MFStartup` is refcounted, but calling it per capture would make the
/// matching `MFShutdown` a lifetime problem for no benefit: the daemon holds
/// its device until it exits. So it is started once and never shut down while
/// the process lives.
fn ensure_mf_started() -> Result<()> {
    use std::sync::Once;
    static START: Once = Once::new();
    static mut RESULT: Option<windows::core::Error> = None;
    START.call_once(|| {
        // Safe: called exactly once, before any other MF entry point.
        let r = unsafe { MFStartup(mf_version(), MFSTARTUP_FULL) };
        if let Err(e) = r {
            // Safe: still inside the Once, so no other thread reads this yet.
            unsafe { RESULT = Some(e) };
        }
    });
    // Safe: written only inside the Once above, which has completed.
    match unsafe { (*std::ptr::addr_of!(RESULT)).clone() } {
        Some(e) => Err(anyhow!("MFStartup failed: {e}")),
        None => Ok(()),
    }
}

/// The MF API version word `MFStartup` expects (`MF_VERSION`).
///
/// The `windows` crate does not export the macro that builds it, so it is
/// assembled here: the SDK defines `MF_VERSION` as
/// `(MF_SDK_VERSION << 16) | MF_API_VERSION`, with the current SDK version 2
/// and API version 0x70.
const fn mf_version() -> u32 {
    (2 << 16) | 0x0070
}

/// Read a string attribute from an activation object, or None if unset.
fn activate_string(dev: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
    let mut ptr: windows::core::PWSTR = windows::core::PWSTR::null();
    let mut len: u32 = 0;
    // Safe: MF allocates the string and reports its length; we free it below.
    unsafe {
        dev.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
        if ptr.is_null() {
            return None;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr.0, len as usize));
        windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as *const _));
        Some(s)
    }
}

/// Every video capture device Media Foundation can see.
///
/// The symbolic link is the stable identity: it is derived from the device's
/// USB topology, so it is the Windows analogue of AVFoundation's `uniqueID`
/// and Linux's `/dev/v4l/by-path` symlink — and it distinguishes two identical
/// dongles, which the friendly name cannot.
fn enumerate_activates() -> Result<Vec<(IMFActivate, String, String)>> {
    ensure_mf_started()?;
    // Safe: standard MF enumeration; the returned array is freed below.
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1).context("MFCreateAttributes")?;
        let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned nothing"))?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .context("selecting video capture devices")?;

        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFEnumDeviceSources(&attrs, &mut raw, &mut count).context("MFEnumDeviceSources")?;

        let mut out = Vec::new();
        for i in 0..count as usize {
            if let Some(dev) = (*raw.add(i)).clone() {
                let name = activate_string(&dev, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                    .unwrap_or_else(|| format!("capture {i}"));
                let id = activate_string(
                    &dev,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                )
                .unwrap_or_default();
                out.push((dev, name, id));
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(raw as *const _));
        Ok(out)
    }
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    Ok(enumerate_activates()?
        .into_iter()
        .enumerate()
        .map(|(i, (_, name, id))| DeviceInfo {
            index: i as u32,
            name,
            misc: String::new(),
            id,
        })
        .collect())
}

/// A frame source backed by an `IMFSourceReader`.
pub struct MfBackend {
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    /// Set once the first frame has logged which stride path it took and
    /// what pitch it saw, so a bench run can confirm the padding handling
    /// without a debugger and the log is not flooded per frame.
    stride_logged: bool,
    /// Row stride (bytes) reported by the selected media type's
    /// `MF_MT_DEFAULT_STRIDE` attribute, read once at open time. Used only
    /// as a fallback in [`MfBackend::frame`] when a sample's buffer does not
    /// support `IMF2DBuffer::Lock2D` (which reports the true per-sample
    /// pitch); `None` means the attribute was absent, in which case `frame`
    /// falls back further, to `width`.
    default_stride: Option<u32>,
}

impl MfBackend {
    pub fn open(spec: &DeviceSpec) -> Result<Self> {
        let devices = enumerate_activates()?;
        if devices.is_empty() {
            return Err(anyhow!("no video capture devices found"));
        }
        let infos: Vec<DeviceInfo> = devices
            .iter()
            .enumerate()
            .map(|(i, (_, name, id))| DeviceInfo {
                index: i as u32,
                name: name.clone(),
                id: id.clone(),
                misc: String::new(),
            })
            .collect();
        let index = super::resolve_in(&infos, spec)? as usize;
        let (activate, name, _) = &devices[index];

        // Safe: the activation object came from MFEnumDeviceSources, and each
        // interface below is released when its wrapper drops.
        unsafe {
            let source: IMFMediaSource = activate
                .ActivateObject()
                .with_context(|| format!("opening capture device '{name}'"))?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, None)
                .with_context(|| format!("creating source reader for '{name}'"))?;
            let (media_type, width, height) = select_native_nv12(&reader)
                .with_context(|| format!("no usable NV12 format on '{name}'"))?;
            // Read once, from the media type we are about to select — the
            // fallback stride for samples whose buffer doesn't support
            // IMF2DBuffer::Lock2D (see MfBackend::frame). Absent on many
            // drivers, in which case `frame` falls back further, to `width`.
            let default_stride = media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok();
            reader
                .SetCurrentMediaType(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    None,
                    &media_type,
                )
                .context("selecting the capture format")?;
            Ok(MfBackend {
                reader,
                width,
                height,
                default_stride,
                stride_logged: false,
            })
        }
    }
}

/// Pick the device's largest native NV12 mode.
///
/// Enumerating the *native* types and choosing one explicitly is the Windows
/// form of the AVFoundation lesson: ask vaguely and the OS inserts a converter
/// that rescales to a 1080p-class default, so a 1440p source silently arrives
/// downscaled.
unsafe fn select_native_nv12(reader: &IMFSourceReader) -> Result<(IMFMediaType, u32, u32)> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut best: Option<(IMFMediaType, u32, u32)> = None;
    for i in 0.. {
        let Ok(mt) = reader.GetNativeMediaType(stream, i) else {
            break; // MF_E_NO_MORE_TYPES — enumeration finished.
        };
        if mt.GetGUID(&MF_MT_MAJOR_TYPE).ok() != Some(MFMediaType_Video) {
            continue;
        }
        if mt.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
            continue;
        }
        let Ok(packed) = mt.GetUINT64(&MF_MT_FRAME_SIZE) else {
            continue;
        };
        let (w, h) = ((packed >> 32) as u32, packed as u32);
        if w == 0 || h == 0 {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(_, bw, bh)| (w as u64 * h as u64) > (*bw as u64 * *bh as u64))
        {
            best = Some((mt, w, h));
        }
    }
    best.ok_or_else(|| {
        anyhow!(
            "device offers no NV12 mode (only NV12 is implemented; \
             MJPEG-only capture devices are not yet supported on Windows)"
        )
    })
}

/// Tightly packed Y and CbCr planes plus the row stride they were compacted
/// from, as both lock paths below return them.
type CompactedPlanes = (Vec<u8>, Vec<u8>, usize);

/// Lock `buffer` through the `IMF2DBuffer` interface and compact it,
/// treating the reported pitch as the row stride.
///
/// Returns `None` (rather than an error) when `buffer` doesn't implement
/// `IMF2DBuffer`, or its `Lock2D` fails — both mean "this buffer doesn't
/// support the 2D path," which is a normal fallback trigger, not a capture
/// failure. Once `Lock2D` itself succeeds, every problem after that
/// (negative pitch, a null scanline, a too-short buffer) is reported as a
/// real error rather than falling back, since a working 2D lock is the
/// authoritative source for the stride.
///
/// Safe to call from the `unsafe` block in [`MfBackend::frame`]: dereferences
/// the scanline pointer only for the byte range Media Foundation guarantees
/// backs the sample at the pitch it just reported, and always calls
/// `Unlock2D` before returning when `Lock2D` succeeded.
unsafe fn lock_and_compact_2d(
    buffer: &IMFMediaBuffer,
    w: usize,
    h: usize,
) -> Option<Result<CompactedPlanes>> {
    let buf2d = buffer.cast::<IMF2DBuffer>().ok()?;
    let mut scanline0: *mut u8 = std::ptr::null_mut();
    let mut pitch: i32 = 0;
    buf2d.Lock2D(&mut scanline0, &mut pitch).ok()?;

    let chroma_rows = h.div_ceil(2);
    // The buffer was made contiguous above, so its current length bounds
    // what the scanline pointer may be read through; never trust the pitch
    // alone to size the slice.
    let got = buffer.GetCurrentLength().map(|n| n as usize).unwrap_or(0);
    let result = if pitch < 0 {
        // Bottom-up NV12 would need the scanline pointer walked backwards
        // and is not a layout any capture device we target produces;
        // treat it as an explicit unsupported case rather than guessing.
        Err(anyhow!(
            "bottom-up NV12 frame (negative stride {pitch}) is not supported"
        ))
    } else if scanline0.is_null() {
        Err(anyhow!("Lock2D returned a null scanline pointer"))
    } else if got < (pitch as usize) * (h + chroma_rows) {
        Err(anyhow!(
            "short NV12 frame (2D): {got} bytes for {w}x{h} at stride {pitch} \
             (expected {})",
            (pitch as usize) * (h + chroma_rows)
        ))
    } else {
        let stride = pitch as usize;
        let needed = stride * (h + chroma_rows);
        let all = std::slice::from_raw_parts(scanline0, needed);
        pixel::compact_nv12(all, stride, w, h)
            .map(|(y, cbcr)| (y, cbcr, stride))
            .ok_or_else(|| {
                anyhow!(
                    "short NV12 frame (2D, stride {stride}) for {w}x{h} \
                     (expected {needed} bytes)"
                )
            })
    };
    let _ = buf2d.Unlock2D();
    Some(result)
}

/// Lock `buffer` through the plain `IMFMediaBuffer::Lock` (1D) interface and
/// compact it, using `stride` as the row pitch.
///
/// This is the fallback used when [`lock_and_compact_2d`] reports that the
/// buffer has no 2D interface: `stride` is the caller's best remaining
/// guess — `MF_MT_DEFAULT_STRIDE` from the selected media type, or `width`
/// when even that attribute was absent.
unsafe fn lock_and_compact_1d(
    buffer: &IMFMediaBuffer,
    stride: usize,
    w: usize,
    h: usize,
) -> Result<CompactedPlanes> {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len: u32 = 0;
    buffer
        .Lock(&mut ptr, None, Some(&mut len))
        .context("locking the frame buffer")?;

    let chroma_rows = h.div_ceil(2);
    let needed = stride * (h + chroma_rows);
    let got = len as usize;
    let result = if ptr.is_null() || got < needed {
        Err(anyhow!(
            "short NV12 frame: {got} bytes for {w}x{h} at stride {stride} (expected {needed})"
        ))
    } else {
        let all = std::slice::from_raw_parts(ptr, needed);
        pixel::compact_nv12(all, stride, w, h)
            .map(|(y, cbcr)| (y, cbcr, stride))
            .ok_or_else(|| {
                anyhow!(
                    "short NV12 frame: {got} bytes for {w}x{h} at stride {stride} (expected {needed})"
                )
            })
    };
    let _ = buffer.Unlock();
    result
}

impl CaptureBackend for MfBackend {
    fn frame(&mut self) -> Result<CapturedFrame> {
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        // Safe: the reader is owned by self; every buffer we lock (2D or 1D)
        // is unlocked before the sample is dropped, and the copy happens
        // while the lock is held.
        unsafe {
            // A device can legitimately return an empty sample (a stream tick,
            // or a format change in flight), so retry rather than fail the
            // frame — a black screen from a real capture is a genuine answer,
            // but "no sample yet" is not.
            for _ in 0..32 {
                let mut flags: u32 = 0;
                let mut ts: i64 = 0;
                let mut sample = None;
                self.reader
                    .ReadSample(
                        stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut ts),
                        Some(&mut sample),
                    )
                    .context("ReadSample")?;
                let Some(sample) = sample else {
                    continue;
                };
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .context("ConvertToContiguousBuffer")?;

                let (w, h) = (self.width as usize, self.height as usize);
                // Media Foundation buffers can be padded to a row stride
                // wider than the visible width (a driver/hardware alignment
                // requirement). Prefer the per-sample stride from
                // IMF2DBuffer::Lock2D when the buffer supports it; only fall
                // back to the stride recorded from the media type (or,
                // failing that, `width`) when it doesn't.
                let (result, path) = match lock_and_compact_2d(&buffer, w, h) {
                    Some(result) => (result, "IMF2DBuffer::Lock2D"),
                    None => {
                        let stride = self.default_stride.map(|s| s as usize).unwrap_or(w);
                        let path = if self.default_stride.is_some() {
                            "MF_MT_DEFAULT_STRIDE"
                        } else {
                            "width (no stride reported)"
                        };
                        (lock_and_compact_1d(&buffer, stride, w, h), path)
                    }
                };

                let (y, cbcr, stride) = result?;
                if !self.stride_logged {
                    tracing::info!(
                        "NV12 {w}x{h}: row stride {stride} bytes via {path}{}",
                        if stride == w {
                            ""
                        } else {
                            " (rows are padded)"
                        }
                    );
                    self.stride_logged = true;
                }
                return Ok(CapturedFrame {
                    jpeg: None,
                    luma: None,
                    pixels: PixelData::Nv12 {
                        y: Arc::from(y),
                        cbcr: Arc::from(cbcr),
                    },
                    width: self.width,
                    height: self.height,
                });
            }
            Err(anyhow!("no frame from the capture device after 32 reads"))
        }
    }
}

/// Shut Media Foundation down. Only meaningful in tests; the daemon exits
/// without unwinding.
#[allow(dead_code)]
pub fn shutdown() {
    // Safe: matching a successful MFStartup.
    unsafe {
        let _ = MFShutdown();
    }
}
