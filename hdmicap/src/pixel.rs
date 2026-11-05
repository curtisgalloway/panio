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

//! Pixel-format carrier and RGB conversion.
//!
//! Frames travel from the capture backend to the HTTP handlers in their
//! native format; RGB is materialized lazily, only by the handler that needs
//! it (/snapshot, /ocr, the macOS preview encode). This keeps the per-frame
//! hot loop free of full-image conversion.

use std::sync::Arc;

use image::RgbImage;

/// Native pixel data of one captured frame.
#[derive(Clone)]
pub enum PixelData {
    /// No pixels available (NoDevice placeholder, or Linux MJPEG-only frames
    /// where `FrameState.jpeg` carries the image).
    Empty,
    /// Packed RGB8 (Linux decode path, YUYV fallbacks).
    Rgb(Arc<[u8]>),
    /// Bi-planar 4:2:0 as delivered by AVFoundation ('420v', video-range
    /// BT.601): `y` is w*h luma, `cbcr` is w*(h/2) interleaved chroma.
    /// Only the macOS backend constructs this; Linux still matches on it
    /// in the shared handlers, hence the cfg'd dead-code allow.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Nv12 { y: Arc<[u8]>, cbcr: Arc<[u8]> },
}

/// A full-range luma plane for signal classification, carrying its own
/// dimensions because they need not be the frame's.
///
/// The Linux MJPEG path decodes grayscale at half scale (GitHub #211): the only
/// consumer of those pixels is [`crate::frame::classify`]'s 64x64 sample
/// lattice, so reconstructing ~2M full-colour pixels to read ~4k of them was
/// most of what the capture thread cost. Half scale, not quarter or eighth,
/// because averaging dilutes isolated bright pixels and `BRIGHT` is what keeps
/// a sparse console screen from reading as no-signal -- see the calibration on
/// that issue.
///
/// `width`/`height` are the plane's, NOT the frame's. Classifying a scaled
/// plane against the frame's dimensions would read past its end.
#[derive(Clone)]
pub struct LumaPlane {
    pub data: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// Video-range BT.601 YCbCr -> RGB, integer arithmetic.
/// y is the raw luma byte (16..235 nominal), cb/cr raw chroma bytes.
#[inline]
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let c = y as i32 - 16;
    let d = cb as i32 - 128;
    let e = cr as i32 - 128;
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    [
        clamp((298 * c + 409 * e + 128) >> 8),
        clamp((298 * c - 100 * d - 208 * e + 128) >> 8),
        clamp((298 * c + 516 * d + 128) >> 8),
    ]
}

/// Full-resolution NV12 -> RGB. Used by /snapshot and /ocr, where every
/// pixel matters.
pub fn nv12_to_rgb(y: &[u8], cbcr: &[u8], w: u32, h: u32) -> RgbImage {
    let mut rgb = RgbImage::new(w, h);
    let (w, h) = (w as usize, h as usize);
    for row in 0..h {
        let c_row = row / 2;
        for col in 0..w {
            let yi = row * w + col;
            let ci = c_row * w + (col & !1);
            if yi >= y.len() || ci + 1 >= cbcr.len() {
                continue;
            }
            let px = ycbcr_to_rgb(y[yi], cbcr[ci], cbcr[ci + 1]);
            rgb.put_pixel(col as u32, row as u32, image::Rgb(px));
        }
    }
    rgb
}

/// Half-resolution NV12 -> RGB for the preview encode. 4:2:0 chroma is
/// stored exactly per 2x2 luma block, so halving is the natural downscale:
/// average the 4 lumas, take the block's chroma as-is.
pub fn nv12_to_rgb_half(y: &[u8], cbcr: &[u8], w: u32, h: u32) -> RgbImage {
    let (ow, oh) = (w / 2, h / 2);
    let mut rgb = RgbImage::new(ow, oh);
    let w = w as usize;
    for row in 0..oh as usize {
        for col in 0..ow as usize {
            let yi = (row * 2) * w + col * 2;
            let ci = row * w + col * 2;
            if yi + w + 1 >= y.len() || ci + 1 >= cbcr.len() {
                continue;
            }
            let y_avg =
                (y[yi] as u32 + y[yi + 1] as u32 + y[yi + w] as u32 + y[yi + w + 1] as u32 + 2) / 4;
            let px = ycbcr_to_rgb(y_avg as u8, cbcr[ci], cbcr[ci + 1]);
            rgb.put_pixel(col as u32, row as u32, image::Rgb(px));
        }
    }
    rgb
}

/// Compact a possibly-padded NV12 buffer into tightly packed Y and CbCr
/// planes.
///
/// Media Foundation (and other platform capture APIs) may deliver a frame
/// whose rows are padded to a stride wider than the visible `width` — a
/// hardware/driver alignment requirement, not something the capture code
/// controls. [`nv12_to_rgb`] and [`nv12_to_rgb_half`] both index their planes
/// using `width` as the row pitch, so a padded buffer has to be compacted
/// first: copying naive `width*height` bytes out of a strided buffer pulls in
/// padding bytes on every row after the first and, worse, starts the chroma
/// plane at `width*height` instead of its real offset `stride*height`, which
/// silently reads chroma from inside what is actually still luma (or
/// padding).
///
/// `buf` is the raw sample: `height` rows of Y at `stride` pitch, followed
/// immediately by `ceil(height/2)` rows of interleaved CbCr, also at `stride`
/// pitch. Only the first `width` bytes of each row are visible pixels; the
/// rest is padding. Returns `None` when `buf` is too short to hold that
/// layout.
///
/// When `stride == width` this still copies (rather than special-casing a
/// zero-copy return) so the fast path exercises the same code as the padded
/// one and callers always get owned, tightly packed planes.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn compact_nv12(
    buf: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let chroma_rows = height.div_ceil(2);
    let needed = stride.checked_mul(height + chroma_rows)?;
    if buf.len() < needed {
        return None;
    }

    let mut y = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * stride;
        y.extend_from_slice(&buf[start..start + width]);
    }

    let chroma_base = stride * height;
    let mut cbcr = Vec::with_capacity(width * chroma_rows);
    for row in 0..chroma_rows {
        let start = chroma_base + row * stride;
        cbcr.extend_from_slice(&buf[start..start + width]);
    }

    Some((y, cbcr))
}

/// Packed YUYV (Y0 Cb Y1 Cr) -> RGB, full-range coefficients. Kept for the
/// Linux YUYV fallback path and macOS 'yuvs'-only devices.
pub fn yuyv_to_rgb(buf: &[u8], w: u32, h: u32) -> RgbImage {
    let mut rgb = RgbImage::new(w, h);
    let pairs = (w * h / 2) as usize;
    for i in 0..pairs {
        let base = i * 4;
        if base + 3 >= buf.len() {
            break;
        }
        let y0 = buf[base] as f32;
        let cb = buf[base + 1] as f32 - 128.0;
        let y1 = buf[base + 2] as f32;
        let cr = buf[base + 3] as f32 - 128.0;
        let to_u8 = |v: f32| v.clamp(0.0, 255.0) as u8;
        let r = |y: f32| to_u8(y + 1.402 * cr);
        let g = |y: f32| to_u8(y - 0.344 * cb - 0.714 * cr);
        let b = |y: f32| to_u8(y + 1.772 * cb);
        let x0 = ((i * 2) % w as usize) as u32;
        let y_row = ((i * 2) / w as usize) as u32;
        if x0 < w && y_row < h {
            rgb.put_pixel(x0, y_row, image::Rgb([r(y0), g(y0), b(y0)]));
        }
        if x0 + 1 < w && y_row < h {
            rgb.put_pixel(x0 + 1, y_row, image::Rgb([r(y1), g(y1), b(y1)]));
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12_solid(y_val: u8, cb: u8, cr: u8, w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        let y = vec![y_val; (w * h) as usize];
        let cbcr: Vec<u8> = (0..(w * h / 2) as usize)
            .map(|i| if i % 2 == 0 { cb } else { cr })
            .collect();
        (y, cbcr)
    }

    #[test]
    fn nv12_video_black_is_rgb_black() {
        // Video-range black: Y=16, Cb=Cr=128.
        let (y, c) = nv12_solid(16, 128, 128, 8, 8);
        let img = nv12_to_rgb(&y, &c, 8, 8);
        assert_eq!(img.get_pixel(3, 3).0, [0, 0, 0]);
    }

    #[test]
    fn nv12_video_white_is_rgb_white() {
        // Video-range white: Y=235, Cb=Cr=128.
        let (y, c) = nv12_solid(235, 128, 128, 8, 8);
        let img = nv12_to_rgb(&y, &c, 8, 8);
        assert_eq!(img.get_pixel(3, 3).0, [255, 255, 255]);
    }

    #[test]
    fn nv12_half_dims_and_value() {
        let (y, c) = nv12_solid(126, 128, 128, 16, 16);
        let img = nv12_to_rgb_half(&y, &c, 16, 16);
        assert_eq!((img.width(), img.height()), (8, 8));
        // mid-grey stays mid-grey-ish through video-range expansion
        let p = img.get_pixel(4, 4).0;
        assert!(p[0] > 100 && p[0] < 160, "{p:?}");
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
    }

    /// Build a synthetic NV12 buffer with a row stride wider than the
    /// visible width. Every padding byte (both the tail of each Y row and
    /// the tail of each CbCr row) is the sentinel `0xEE`, so any padding
    /// that leaks into a compacted plane is detectable. The byte at the old
    /// (wrong) packed chroma offset `width*height` is also the sentinel,
    /// while the real chroma offset `stride*height` holds recognizable
    /// data — a `compact_nv12` that used `width*height` for the chroma
    /// offset (the bug this test exists to catch) would read sentinel bytes
    /// there instead of real chroma.
    fn padded_nv12(width: usize, height: usize, stride: usize) -> Vec<u8> {
        assert!(stride >= width);
        let chroma_rows = height.div_ceil(2);
        let mut buf = vec![0xEEu8; stride * (height + chroma_rows)];
        for row in 0..height {
            for col in 0..width {
                // Recognizable, position-dependent value distinct from 0xEE.
                buf[row * stride + col] = ((row * width + col) % 200) as u8;
            }
        }
        let chroma_base = stride * height;
        for row in 0..chroma_rows {
            for col in 0..width {
                buf[chroma_base + row * stride + col] = (0x40 + (row * width + col) % 100) as u8;
            }
        }
        buf
    }

    #[test]
    fn compact_nv12_strips_row_padding_and_finds_chroma_at_stride_offset() {
        let (width, height, stride) = (6, 4, 8);
        let buf = padded_nv12(width, height, stride);

        let (y, cbcr) = compact_nv12(&buf, stride, width, height).expect("buffer is long enough");

        assert_eq!(y.len(), width * height);
        assert!(
            !y.contains(&0xEE),
            "Y plane must not contain padding bytes: {y:?}"
        );
        for row in 0..height {
            for col in 0..width {
                assert_eq!(
                    y[row * width + col],
                    ((row * width + col) % 200) as u8,
                    "Y mismatch at row {row} col {col}"
                );
            }
        }

        let chroma_rows = height.div_ceil(2);
        assert_eq!(cbcr.len(), width * chroma_rows);
        assert!(
            !cbcr.contains(&0xEE),
            "CbCr plane must not contain padding, or data read from the wrong (packed) offset: {cbcr:?}"
        );
        for row in 0..chroma_rows {
            for col in 0..width {
                assert_eq!(
                    cbcr[row * width + col],
                    (0x40 + (row * width + col) % 100) as u8,
                    "CbCr mismatch at row {row} col {col}"
                );
            }
        }
    }

    #[test]
    fn compact_nv12_packed_stride_equals_width() {
        let (width, height) = (6, 4);
        let stride = width;
        let buf = padded_nv12(width, height, stride);

        let (y, cbcr) = compact_nv12(&buf, stride, width, height).expect("packed buffer is valid");

        assert_eq!(y.len(), width * height);
        assert_eq!(cbcr.len(), width * height.div_ceil(2));
        assert_eq!(y, buf[..width * height]);
        assert_eq!(cbcr, buf[width * height..]);
    }

    #[test]
    fn compact_nv12_too_short_is_none() {
        let (width, height, stride) = (6, 4, 8);
        let full = padded_nv12(width, height, stride);
        // One byte short of the full Y+CbCr layout.
        let truncated = &full[..full.len() - 1];
        assert!(compact_nv12(truncated, stride, width, height).is_none());
    }
}
