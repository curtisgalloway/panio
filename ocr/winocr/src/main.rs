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

//! `winocr` — read text from an image using `Windows.Media.Ocr`.
//!
//! The Windows sibling of `visionocr` (Apple Vision) and `linuxocr`
//! (Tesseract), and the reason "platform-native OCR defaults" is a real
//! statement on Windows rather than an aspiration. On-device, offline, no model
//! download: the engine ships with the OS.
//!
//!   winocr [--json] [PATH | -]
//!
//!   --json   emit the v1 OCR envelope (see docs/dev/ocr.md) instead of plain text
//!
//! **No confidence scores.** `Windows.Media.Ocr` does not expose a per-word or
//! per-line confidence — unlike Tesseract, and unlike Apple Vision, which
//! exposes one that turns out to be constant. Per docs/dev/ocr.md an engine that
//! cannot report confidence reports its absence rather than inventing a number,
//! so `confidence` is simply omitted from every line here. A consumer that
//! needs to rank results must not read a missing field as zero.

use std::io::Read;

#[cfg(windows)]
mod win {
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::BitmapDecoder;
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

    pub struct Line {
        pub text: String,
        pub bbox: [i32; 4],
    }

    pub struct Recognized {
        pub lines: Vec<Line>,
        pub width: u32,
        pub height: u32,
        pub language: String,
    }

    /// Decode `png` and run the OS OCR engine over it.
    pub fn recognize(png: &[u8]) -> windows::core::Result<Recognized> {
        // WinRT decodes from a random-access stream, so the bytes go through an
        // in-memory one rather than a temp file.
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(png)?;
        writer.StoreAsync()?.join()?;
        writer.FlushAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;

        let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
        // PixelWidth/PixelHeight come from the decoder's parse of the image
        // header, not from rasterizing it -- read them and reject an
        // over-limit image here, before GetSoftwareBitmapAsync() below pays
        // the cost of decoding it into a full software bitmap.
        let width = decoder.PixelWidth()?;
        let height = decoder.PixelHeight()?;
        let engine_max = OcrEngine::MaxImageDimension().ok();
        if let Err(msg) = crate::check_dimensions(width, height, engine_max) {
            return Err(windows::core::Error::new(windows::core::HRESULT(-1), msg));
        }
        let bitmap = decoder.GetSoftwareBitmapAsync()?.join()?;

        // Prefer the user's own languages; fall back to English, which is what
        // every screen paniolo reads is in. A machine with no OCR language pack
        // installed yields neither, and the caller gets a clear error rather
        // than empty text that looks like a blank screen.
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .ok()
            .or_else(|| {
                Language::CreateLanguage(&windows::core::HSTRING::from("en-US"))
                    .ok()
                    .and_then(|l| OcrEngine::TryCreateFromLanguage(&l).ok())
            });
        let Some(engine) = engine else {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(-1),
                "no OCR language pack available (Settings > Language > \
                 add the Optional feature \"Optical character recognition\")",
            ));
        };
        let language = engine
            .RecognizerLanguage()
            .and_then(|l| l.LanguageTag())
            .map(|t| t.to_string_lossy())
            .unwrap_or_else(|_| "unknown".to_string());

        let result = engine.RecognizeAsync(&bitmap)?.join()?;
        let mut lines = Vec::new();
        for line in result.Lines()? {
            let text = line.Text()?.to_string_lossy();
            // A line has no rect of its own; take the union of its words'.
            let (mut x0, mut y0) = (f64::MAX, f64::MAX);
            let (mut x1, mut y1) = (f64::MIN, f64::MIN);
            let mut any = false;
            for word in line.Words()? {
                let r = word.BoundingRect()?;
                x0 = x0.min(r.X as f64);
                y0 = y0.min(r.Y as f64);
                x1 = x1.max((r.X + r.Width) as f64);
                y1 = y1.max((r.Y + r.Height) as f64);
                any = true;
            }
            let bbox = if any {
                [
                    x0.round() as i32,
                    y0.round() as i32,
                    (x1 - x0).round() as i32,
                    (y1 - y0).round() as i32,
                ]
            } else {
                [0, 0, 0, 0]
            };
            lines.push(Line { text, bbox });
        }
        Ok(Recognized {
            lines,
            width,
            height,
            language,
        })
    }
}

fn die(msg: &str) -> ! {
    eprintln!("winocr: {msg}");
    std::process::exit(1);
}

// Resource limits shared with ocr/linuxocr, ocr/rapidocr and
// ocr/visionocr.swift -- MAX_ENCODED_BYTES, MAX_DIMENSION and MAX_PIXELS
// must be identical across all four helpers (see docs/dev/ocr.md's
// "Resource limits" section). winocr does no upscaling, so these apply
// directly to the source image, intersected with Windows.Media.Ocr's own
// `OcrEngine::MaxImageDimension()` -- see `check_dimensions`.
const MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024; // 64 MiB of encoded input

// Only `mod win` (cfg(windows)) calls check_dimensions, which is the only
// non-test reader of these two -- so a non-Windows build (this crate builds
// on macOS/Linux too, just without the OCR itself) sees them as dead code.
// The lint stays live on Windows, where CI actually runs it.
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_DIMENSION: u32 = 8192; // px, per side
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_PIXELS: u64 = 33_177_600; // 7680x4320 total px -- 2x a 4K capture

/// Reject dimensions before they drive a decode. `engine_max`, when
/// present, is `OcrEngine::MaxImageDimension()`; the stricter of it and the
/// shared per-side limit applies, so a more conservative engine limit is
/// still honored.
#[cfg_attr(not(windows), allow(dead_code))]
fn check_dimensions(width: u32, height: u32, engine_max: Option<u32>) -> Result<(), String> {
    let side_limit = match engine_max {
        Some(m) if m < MAX_DIMENSION => m,
        _ => MAX_DIMENSION,
    };
    if width > side_limit || height > side_limit {
        return Err(format!(
            "image is {width}x{height}; exceeds the {side_limit}px-per-side limit. \
             Downscale before OCR."
        ));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(format!(
            "image is {width}x{height} ({pixels} px); exceeds the {MAX_PIXELS}px limit. \
             Downscale before OCR."
        ));
    }
    Ok(())
}

/// Read at most `max_bytes` + 1 bytes from `r`, in chunks, erroring if the
/// input turns out to be longer than `max_bytes`. Mirrors the bounded read
/// in ocr/linuxocr and ocr/rapidocr; reading one byte past the cap (rather
/// than stopping exactly at it) is what tells "exactly at the limit" from
/// "over the limit" apart without ever buffering more than a byte more than
/// the limit allows.
fn read_bounded<R: Read>(mut r: R, max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1 << 16];
    while buf.len() <= max_bytes {
        let want = (max_bytes + 1 - buf.len()).min(chunk.len());
        let n = r.read(&mut chunk[..want]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if buf.len() > max_bytes {
        return Err(format!(
            "input is over {max_bytes} bytes (encoded-input limit)"
        ));
    }
    Ok(buf)
}

fn main() {
    let mut json = false;
    let mut path: Option<String> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "Read text from an image using Windows.Media.Ocr (on-device).

Usage: winocr [OPTIONS] [PATH | -]

Read an image from PATH, or stdin when PATH is omitted or '-'.
Print recognized text in reading order by default.

Options:
  --json        Emit the v1 OCR JSON envelope with source-pixel boxes
  -h, --help    Print help and exit"
                );
                return;
            }
            "--json" => json = true,
            "-" => path = None,
            other => path = Some(other.to_string()),
        }
    }

    let data = match &path {
        Some(p) => std::fs::File::open(p)
            .map_err(|e| format!("cannot read {p}: {e}"))
            .and_then(|f| read_bounded(f, MAX_ENCODED_BYTES))
            .unwrap_or_else(|e| die(&e)),
        None => read_bounded(std::io::stdin(), MAX_ENCODED_BYTES).unwrap_or_else(|e| die(&e)),
    };
    if data.is_empty() {
        die("no image data");
    }

    #[cfg(not(windows))]
    {
        let _ = (json, data);
        die("winocr is Windows-only (macOS uses visionocr, Linux uses linuxocr)");
    }

    #[cfg(windows)]
    {
        let r = win::recognize(&data).unwrap_or_else(|e| die(&format!("{e}")));
        let text: Vec<&str> = r.lines.iter().map(|l| l.text.as_str()).collect();
        if !json {
            for t in &text {
                println!("{t}");
            }
            return;
        }
        // Note the absent `confidence`: see the module docs.
        let lines: Vec<serde_json::Value> = r
            .lines
            .iter()
            .map(|l| serde_json::json!({ "text": l.text, "bbox": l.bbox }))
            .collect();
        let envelope = serde_json::json!({
            "version": 1,
            "engine": "winocr",
            "engine_detail": format!("Windows.Media.Ocr, {}", r.language),
            "width": r.width,
            "height": r.height,
            "text": text.join("\n"),
            "lines": lines,
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    }
}

// These exercise the two pure resource-limit checks directly, without a
// BitmapDecoder or any Windows API -- so they run in the `cargo test` step
// of the Windows CI job (see .github/workflows/ci.yml's "winocr" step)
// alongside the fmt/clippy/build checks that job already runs, same as
// ocr/tests' pytest cases for linuxocr/rapidocr and visionocr's
// `--self-test` cover the equivalent checks on the other two platforms.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_dimensions_accepts_100x100() {
        assert!(check_dimensions(100, 100, None).is_ok());
    }

    #[test]
    fn check_dimensions_accepts_exactly_the_limits() {
        let h = (MAX_PIXELS / u64::from(MAX_DIMENSION)) as u32;
        assert!(check_dimensions(MAX_DIMENSION, h, None).is_ok());
    }

    #[test]
    fn check_dimensions_rejects_over_the_side_limit() {
        assert!(check_dimensions(MAX_DIMENSION + 1, 100, None).is_err());
    }

    #[test]
    fn check_dimensions_rejects_over_the_pixel_limit_within_the_side_limit() {
        // 6000x6000 is under MAX_DIMENSION (8192) on each side but over
        // MAX_PIXELS (33,177,600) in total: 36,000,000 > 33,177,600.
        assert!(check_dimensions(6000, 6000, None).is_err());
    }

    #[test]
    fn check_dimensions_uses_the_stricter_of_the_shared_and_engine_limits() {
        // A stricter engine_max than the shared limit wins...
        assert!(check_dimensions(3000, 100, Some(2000)).is_err());
        assert!(check_dimensions(1000, 100, Some(2000)).is_ok());
        // ...and a looser engine_max does not relax the shared limit.
        assert!(check_dimensions(MAX_DIMENSION + 1, 100, Some(50_000)).is_err());
    }

    #[test]
    fn read_bounded_accepts_exactly_the_cap() {
        let data = vec![7u8; 10];
        let got = read_bounded(&data[..], 10).expect("within the cap");
        assert_eq!(got, data);
    }

    #[test]
    fn read_bounded_rejects_one_byte_over() {
        let data = [7u8; 11];
        assert!(read_bounded(&data[..], 10).is_err());
    }

    #[test]
    fn read_bounded_accepts_empty_input() {
        let got = read_bounded(&b""[..], 10).expect("empty is within the cap");
        assert!(got.is_empty());
    }

    #[test]
    fn read_bounded_reassembles_input_larger_than_its_internal_chunk_size() {
        // Exercises the loop across more than one internal chunk (64 KiB).
        let data = vec![9u8; (1 << 16) + 1000];
        let got = read_bounded(&data[..], data.len()).expect("within the cap");
        assert_eq!(got, data);
    }
}
