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

//! The pixel formats and the ZRLE encoder behind hdmicap's RFB server
//! (`rfb_serve.rs`).
//!
//! ZRLE (RFC 6143 section 7.7.6) cuts a rectangle into 64x64 tiles, encodes
//! each tile on its own as the smallest of solid, packed palette, plain RLE,
//! palette RLE and raw, and runs the concatenation through ONE zlib stream per
//! connection, flushed (not reset) after every rectangle.
//!
//! The format handling is deliberately strict: only 32 bits per pixel,
//! true-colour, 8 bits per channel, byte-aligned shifts. That is what every
//! browser client asks for and it keeps CPIXEL a single rule.

use flate2::{Compress, Compression, FlushCompress};

/// Edge of a ZRLE tile, and of the dirty-tracking grid in `rfb_serve`.
pub const TILE: usize = 64;

/// A client's pixel format, reduced to what hdmicap serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub big_endian: bool,
    /// Bit position of the red, green and blue bytes inside the 32-bit pixel.
    pub shifts: [u8; 3],
    pub depth: u8,
}

impl PixelFormat {
    /// What ServerInit announces: 32bpp depth 24, little-endian, r16 g8 b0.
    pub const DEFAULT: PixelFormat = PixelFormat {
        big_endian: false,
        shifts: [16, 8, 0],
        depth: 24,
    };

    /// Parse the 16-byte PIXEL_FORMAT of SetPixelFormat. The error text is
    /// what the connection is closed with.
    pub fn parse(b: &[u8; 16]) -> Result<PixelFormat, String> {
        let (bpp, depth, big, tc) = (b[0], b[1], b[2] != 0, b[3] != 0);
        let max = |i: usize| u16::from_be_bytes([b[4 + 2 * i], b[5 + 2 * i]]);
        let shifts = [b[10], b[11], b[12]];
        if bpp != 32 {
            return Err(format!(
                "unsupported pixel format: {bpp} bits per pixel (need 32)"
            ));
        }
        if !tc {
            return Err("unsupported pixel format: colour-map (need true colour)".into());
        }
        if (0..3).any(|i| max(i) != 255) {
            return Err("unsupported pixel format: channel max must be 255".into());
        }
        let aligned = shifts.iter().all(|s| matches!(s, 0 | 8 | 16 | 24));
        let distinct = shifts[0] != shifts[1] && shifts[0] != shifts[2] && shifts[1] != shifts[2];
        if !aligned || !distinct {
            return Err("unsupported pixel format: shifts must be distinct bytes".into());
        }
        Ok(PixelFormat {
            big_endian: big,
            shifts,
            depth,
        })
    }

    /// The 16-byte PIXEL_FORMAT for ServerInit.
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = 32;
        b[1] = self.depth;
        b[2] = self.big_endian as u8;
        b[3] = 1;
        for i in 0..3 {
            b[4 + 2 * i..6 + 2 * i].copy_from_slice(&255u16.to_be_bytes());
            b[10 + i] = self.shifts[i];
        }
        b
    }

    fn pack(self, r: u8, g: u8, b: u8) -> u32 {
        (r as u32) << self.shifts[0] | (g as u32) << self.shifts[1] | (b as u32) << self.shifts[2]
    }

    /// The four bytes of a pixel on the wire.
    pub fn wire(self, r: u8, g: u8, b: u8) -> [u8; 4] {
        let v = self.pack(r, g, b);
        if self.big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    /// Index (within the four wire bytes) of the byte a CPIXEL leaves out, or
    /// `None` when CPIXEL is the full four bytes. RFC 6143: CPIXEL is three
    /// bytes when the format is 32bpp, depth <= 24 and every colour bit lies
    /// in the least-significant three bytes or in the most-significant three;
    /// the unused byte is the one omitted, wherever endianness put it.
    fn cpixel_skip(self) -> Option<usize> {
        if self.depth > 24 {
            return None;
        }
        let mask = self.pack(255, 255, 255);
        if mask >> 24 == 0 {
            Some(if self.big_endian { 0 } else { 3 })
        } else if mask & 0xFF == 0 {
            Some(if self.big_endian { 3 } else { 0 })
        } else {
            None
        }
    }

    pub fn cpixel_len(self) -> usize {
        if self.cpixel_skip().is_some() {
            3
        } else {
            4
        }
    }

    /// Append one pixel as a PIXEL (4 bytes).
    pub fn push_pixel(self, out: &mut Vec<u8>, rgb: [u8; 3]) {
        out.extend_from_slice(&self.wire(rgb[0], rgb[1], rgb[2]));
    }

    /// Append one pixel as a CPIXEL.
    pub fn push_cpixel(self, out: &mut Vec<u8>, rgb: [u8; 3]) {
        let w = self.wire(rgb[0], rgb[1], rgb[2]);
        match self.cpixel_skip() {
            None => out.extend_from_slice(&w),
            Some(skip) => {
                for (i, byte) in w.iter().enumerate() {
                    if i != skip {
                        out.push(*byte);
                    }
                }
            }
        }
    }
}

/// One connection's ZRLE state: the zlib stream that must never be reset.
pub struct ZrleEncoder {
    z: Compress,
}

impl ZrleEncoder {
    pub fn new() -> ZrleEncoder {
        ZrleEncoder {
            z: Compress::new(Compression::new(3), true),
        }
    }

    /// Encode the `w` x `h` rectangle at (`x`, `y`) of the packed-RGB image
    /// (`stride` pixels per row) and return the zlib bytes to ship as the
    /// rectangle's payload (without its length prefix).
    #[allow(clippy::too_many_arguments)]
    pub fn encode_rect(
        &mut self,
        fmt: PixelFormat,
        rgb: &[u8],
        stride: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut ty = 0;
        while ty < h {
            let th = TILE.min(h - ty);
            let mut tx = 0;
            while tx < w {
                let tw = TILE.min(w - tx);
                encode_tile(fmt, rgb, stride, x + tx, y + ty, tw, th, &mut raw);
                tx += tw;
            }
            ty += th;
        }
        self.deflate(&raw)
    }

    fn deflate(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() / 4 + 64);
        let start_in = self.z.total_in();
        loop {
            let consumed = (self.z.total_in() - start_in) as usize;
            self.z
                .compress_vec(&input[consumed..], &mut out, FlushCompress::Sync)
                .expect("zlib deflate cannot fail on in-memory buffers");
            let consumed = (self.z.total_in() - start_in) as usize;
            // Everything in, and the flush finished (it did not run out of
            // room): done.
            if consumed == input.len() && out.len() < out.capacity() {
                return out;
            }
            out.reserve(out.capacity().max(256));
        }
    }
}

impl Default for ZrleEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Append the length of a run (>= 1) as ZRLE writes it: 255 repeated, then
/// the remainder, summing to length - 1.
fn push_run_len(out: &mut Vec<u8>, len: usize) {
    let mut n = len - 1;
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

fn run_len_bytes(len: usize) -> usize {
    (len - 1) / 255 + 1
}

/// Distinct colours of a tile in first-seen order, or `None` once there are
/// more than 127 (no palette form applies then). Open addressing over a small
/// fixed table keeps a many-coloured tile cheap to reject.
struct Palette {
    colors: Vec<u32>,
}

impl Palette {
    fn build(tile: &[u32]) -> Option<Palette> {
        const SLOTS: usize = 512;
        let mut table = [u8::MAX; SLOTS];
        let mut colors: Vec<u32> = Vec::new();
        for &c in tile {
            let mut i = (c.wrapping_mul(0x9E37_79B1) >> 23) as usize % SLOTS;
            loop {
                let e = table[i];
                if e == u8::MAX {
                    if colors.len() >= 127 {
                        return None;
                    }
                    table[i] = colors.len() as u8;
                    colors.push(c);
                    break;
                }
                if colors[e as usize] == c {
                    break;
                }
                i = (i + 1) % SLOTS;
            }
        }
        Some(Palette { colors })
    }

    fn index_of(&self, c: u32) -> u8 {
        self.colors
            .iter()
            .position(|&p| p == c)
            .expect("in palette") as u8
    }
}

fn unpack(c: u32) -> [u8; 3] {
    [(c >> 16) as u8, (c >> 8) as u8, c as u8]
}

#[allow(clippy::too_many_arguments)]
fn encode_tile(
    fmt: PixelFormat,
    rgb: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    out: &mut Vec<u8>,
) {
    let mut px: Vec<u32> = Vec::with_capacity(w * h);
    for row in 0..h {
        let base = ((y + row) * stride + x) * 3;
        let (pixels, _) = rgb[base..base + w * 3].as_chunks::<3>();
        px.extend(
            pixels
                .iter()
                .map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32),
        );
    }
    let cp = fmt.cpixel_len();

    let mut runs: Vec<(u32, usize)> = Vec::new();
    for &c in &px {
        match runs.last_mut() {
            Some((last, n)) if *last == c => *n += 1,
            _ => runs.push((c, 1)),
        }
    }
    if runs.len() == 1 {
        out.push(1);
        fmt.push_cpixel(out, unpack(px[0]));
        return;
    }

    let pal = Palette::build(&px);
    let raw_size = 1 + w * h * cp;
    let plain_rle_size = 1 + runs
        .iter()
        .map(|(_, n)| cp + run_len_bytes(*n))
        .sum::<usize>();

    // (size, kind) of the best applicable form.
    #[derive(Clone, Copy)]
    enum Kind {
        Raw,
        PlainRle,
        Packed,
        PaletteRle,
    }
    let mut best = (raw_size, Kind::Raw);
    if plain_rle_size < best.0 {
        best = (plain_rle_size, Kind::PlainRle);
    }
    let bits = |n: usize| match n {
        2 => 1,
        3..=4 => 2,
        _ => 4,
    };
    if let Some(p) = &pal {
        let n = p.colors.len();
        if n <= 16 {
            let size = 1 + n * cp + h * (w * bits(n)).div_ceil(8);
            if size < best.0 {
                best = (size, Kind::Packed);
            }
        }
        let size = 1
            + n * cp
            + runs
                .iter()
                .map(|(_, len)| {
                    if *len == 1 {
                        1
                    } else {
                        1 + run_len_bytes(*len)
                    }
                })
                .sum::<usize>();
        if size < best.0 {
            best = (size, Kind::PaletteRle);
        }
    }

    match best.1 {
        Kind::Raw => {
            out.push(0);
            for &c in &px {
                fmt.push_cpixel(out, unpack(c));
            }
        }
        Kind::PlainRle => {
            out.push(128);
            for &(c, n) in &runs {
                fmt.push_cpixel(out, unpack(c));
                push_run_len(out, n);
            }
        }
        Kind::Packed => {
            let p = pal.expect("packed implies palette");
            let n = p.colors.len();
            let b = bits(n);
            out.push(n as u8);
            for &c in &p.colors {
                fmt.push_cpixel(out, unpack(c));
            }
            for row in 0..h {
                let mut acc = 0u8;
                let mut filled = 0;
                for &c in &px[row * w..(row + 1) * w] {
                    acc = (acc << b) | p.index_of(c);
                    filled += b;
                    if filled == 8 {
                        out.push(acc);
                        acc = 0;
                        filled = 0;
                    }
                }
                if filled > 0 {
                    out.push(acc << (8 - filled));
                }
            }
        }
        Kind::PaletteRle => {
            let p = pal.expect("palette RLE implies palette");
            out.push(128 + p.colors.len() as u8);
            for &c in &p.colors {
                fmt.push_cpixel(out, unpack(c));
            }
            for &(c, n) in &runs {
                let i = p.index_of(c);
                if n == 1 {
                    out.push(i);
                } else {
                    out.push(i | 0x80);
                    push_run_len(out, n);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    /// The reference decoder: inflate the persistent stream, then walk tiles.
    /// Written from the RFC rather than from the encoder, and reading CPIXELs
    /// by rebuilding the 4-byte wire word.
    pub(crate) struct Decoder {
        z: Decompress,
    }

    impl Decoder {
        pub(crate) fn new() -> Decoder {
            Decoder {
                z: Decompress::new(true),
            }
        }

        /// Decode a rectangle's payload into packed RGB (3 bytes per pixel).
        pub(crate) fn decode_rect(
            &mut self,
            fmt: PixelFormat,
            data: &[u8],
            w: usize,
            h: usize,
        ) -> Vec<u8> {
            let mut raw = Vec::with_capacity(data.len() * 8 + 1024);
            let start = self.z.total_in();
            loop {
                let used = (self.z.total_in() - start) as usize;
                self.z
                    .decompress_vec(&data[used..], &mut raw, FlushDecompress::Sync)
                    .unwrap();
                let used = (self.z.total_in() - start) as usize;
                if used == data.len() && raw.len() < raw.capacity() {
                    break;
                }
                raw.reserve(raw.capacity().max(1024));
            }
            let mut pos = 0;
            let mut img = vec![0u8; w * h * 3];
            let mut ty = 0;
            while ty < h {
                let th = TILE.min(h - ty);
                let mut tx = 0;
                while tx < w {
                    let tw = TILE.min(w - tx);
                    let tile = decode_tile(fmt, &raw, &mut pos, tw, th);
                    for r in 0..th {
                        for c in 0..tw {
                            let o = ((ty + r) * w + tx + c) * 3;
                            img[o..o + 3].copy_from_slice(&tile[r * tw + c]);
                        }
                    }
                    tx += tw;
                }
                ty += th;
            }
            assert_eq!(pos, raw.len(), "trailing bytes after the last tile");
            img
        }
    }

    fn read_cpixel(fmt: PixelFormat, raw: &[u8], pos: &mut usize) -> [u8; 3] {
        let n = fmt.cpixel_len();
        let mut word = [0u8; 4];
        if n == 4 {
            word.copy_from_slice(&raw[*pos..*pos + 4]);
        } else {
            // Which byte of the word is missing, derived here from the RFC
            // text: colours in the low three bytes leave the top byte of the
            // value out; in the high three bytes, the bottom byte. The value's
            // top byte is wire index 0 big-endian and 3 little-endian.
            let colours_low = fmt.shifts.iter().all(|&s| s <= 16);
            let missing_is_value_top = colours_low;
            let missing_wire_index = match (missing_is_value_top, fmt.big_endian) {
                (true, true) | (false, false) => 0,
                (true, false) | (false, true) => 3,
            };
            let mut src = raw[*pos..*pos + 3].iter();
            for (i, slot) in word.iter_mut().enumerate() {
                if i != missing_wire_index {
                    *slot = *src.next().unwrap();
                }
            }
        }
        *pos += n;
        let v = if fmt.big_endian {
            u32::from_be_bytes(word)
        } else {
            u32::from_le_bytes(word)
        };
        [
            (v >> fmt.shifts[0]) as u8,
            (v >> fmt.shifts[1]) as u8,
            (v >> fmt.shifts[2]) as u8,
        ]
    }

    fn read_run_len(raw: &[u8], pos: &mut usize) -> usize {
        let mut n = 1;
        loop {
            let b = raw[*pos] as usize;
            *pos += 1;
            n += b;
            if b != 255 {
                return n;
            }
        }
    }

    fn decode_tile(
        fmt: PixelFormat,
        raw: &[u8],
        pos: &mut usize,
        w: usize,
        h: usize,
    ) -> Vec<[u8; 3]> {
        let sub = raw[*pos];
        *pos += 1;
        let mut px = Vec::with_capacity(w * h);
        match sub {
            0 => {
                for _ in 0..w * h {
                    px.push(read_cpixel(fmt, raw, pos));
                }
            }
            1 => {
                let c = read_cpixel(fmt, raw, pos);
                px.resize(w * h, c);
            }
            2..=16 => {
                let pal: Vec<_> = (0..sub).map(|_| read_cpixel(fmt, raw, pos)).collect();
                let bits = match sub {
                    2 => 1,
                    3..=4 => 2,
                    _ => 4,
                };
                for _ in 0..h {
                    let mut bit = 0;
                    let mut byte = 0u8;
                    for _ in 0..w {
                        if bit == 0 {
                            byte = raw[*pos];
                            *pos += 1;
                        }
                        let idx = (byte >> (8 - bits - bit)) & ((1 << bits) - 1);
                        px.push(pal[idx as usize]);
                        bit = (bit + bits) % 8;
                    }
                }
            }
            128 => {
                while px.len() < w * h {
                    let c = read_cpixel(fmt, raw, pos);
                    let n = read_run_len(raw, pos);
                    px.extend(std::iter::repeat_n(c, n));
                }
            }
            130..=255 => {
                let n = (sub - 128) as usize;
                let pal: Vec<_> = (0..n).map(|_| read_cpixel(fmt, raw, pos)).collect();
                while px.len() < w * h {
                    let b = raw[*pos];
                    *pos += 1;
                    let c = pal[(b & 0x7F) as usize];
                    let n = if b & 0x80 != 0 {
                        read_run_len(raw, pos)
                    } else {
                        1
                    };
                    px.extend(std::iter::repeat_n(c, n));
                }
            }
            other => panic!("bad subencoding {other}"),
        }
        assert_eq!(px.len(), w * h, "tile pixel count");
        px
    }

    fn formats() -> Vec<PixelFormat> {
        let mk = |big_endian, shifts| PixelFormat {
            big_endian,
            shifts,
            depth: 24,
        };
        vec![
            mk(false, [16, 8, 0]),
            mk(false, [0, 8, 16]),
            mk(true, [16, 8, 0]),
            mk(true, [0, 8, 16]),
            // colours in the most-significant three bytes
            mk(false, [24, 16, 8]),
            mk(true, [24, 16, 8]),
        ]
    }

    /// Deterministic xorshift, so failures reproduce.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }
    }

    /// An image whose every tile uses (about) `colors` distinct colours.
    fn image_with_colors(rng: &mut Rng, w: usize, h: usize, colors: usize, runny: bool) -> Vec<u8> {
        let pal: Vec<[u8; 3]> = (0..colors)
            .map(|i| {
                let v = rng.next();
                // Keep them distinct even for tiny palettes.
                [
                    (v >> 16) as u8 ^ i as u8,
                    (v >> 8) as u8,
                    v as u8 | (i as u8 & 1),
                ]
            })
            .collect();
        let mut img = Vec::with_capacity(w * h * 3);
        let mut cur = 0;
        for _ in 0..w * h {
            if !runny || rng.next().is_multiple_of(5) {
                cur = rng.next() as usize % colors;
            }
            img.extend_from_slice(&pal[cur]);
        }
        img
    }

    fn round_trip(fmt: PixelFormat, img: &[u8], w: usize, h: usize) -> usize {
        let mut enc = ZrleEncoder::new();
        let mut dec = Decoder::new();
        let data = enc.encode_rect(fmt, img, w, 0, 0, w, h);
        let back = dec.decode_rect(fmt, &data, w, h);
        assert!(back == img, "round trip differs for {fmt:?} {w}x{h}");
        data.len()
    }

    #[test]
    fn structured_and_random_tiles_round_trip_in_every_format() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for fmt in formats() {
            // 130x70: full tiles plus 2- and 6-pixel edge tiles.
            for &(w, h) in &[(64, 64), (130, 70), (1, 1), (65, 3), (7, 129)] {
                for &colors in &[1usize, 2, 3, 4, 5, 16, 17, 127, 128, 200, 4000] {
                    for &runny in &[false, true] {
                        let img = image_with_colors(&mut rng, w, h, colors, runny);
                        round_trip(fmt, &img, w, h);
                    }
                }
            }
        }
    }

    #[test]
    fn the_smallest_subencoding_is_chosen_per_tile() {
        // Subencoding byte of the first tile after the zlib stream is undone.
        fn first_sub(img: &[u8], w: usize, h: usize) -> u8 {
            let fmt = PixelFormat::DEFAULT;
            let mut raw = Vec::new();
            encode_tile(fmt, img, w, 0, 0, w.min(64), h.min(64), &mut raw);
            raw[0]
        }
        let mut rng = Rng(7);
        assert_eq!(first_sub(&[9u8; 64 * 64 * 3], 64, 64), 1);
        // Two colours, no runs worth the name: packed palette (2).
        let mut img = Vec::new();
        for i in 0..64 * 64 {
            img.extend_from_slice(if rng.next().is_multiple_of(2) || i == 0 {
                &[0, 0, 0]
            } else {
                &[255, 255, 255]
            });
        }
        assert_eq!(first_sub(&img, 64, 64), 2);
        // Long runs of one of three colours: palette RLE (128 + 3).
        let mut img = Vec::new();
        for i in 0..64 * 64 {
            img.extend_from_slice(&[[1u8, 2, 3], [4, 5, 6], [7, 8, 9]][(i / 700) % 3]);
        }
        assert_eq!(first_sub(&img, 64, 64), 131);
        // Noise: raw.
        let img = image_with_colors(&mut rng, 64, 64, 4000, false);
        assert_eq!(first_sub(&img, 64, 64), 0);
    }

    #[test]
    fn the_zlib_stream_carries_across_rectangles() {
        // One decoder inflating two rectangles from one encoder only works if
        // the stream is continuous (header once, no reset between rects).
        let fmt = PixelFormat::DEFAULT;
        let mut rng = Rng(99);
        let mut enc = ZrleEncoder::new();
        let mut dec = Decoder::new();
        for _ in 0..4 {
            let img = image_with_colors(&mut rng, 70, 70, 9, true);
            let data = enc.encode_rect(fmt, &img, 70, 0, 0, 70, 70);
            assert!(dec.decode_rect(fmt, &data, 70, 70) == img);
        }
    }

    #[test]
    fn a_sub_rectangle_of_a_wider_image_is_encoded_from_its_origin() {
        let fmt = PixelFormat::DEFAULT;
        let mut rng = Rng(5);
        let (iw, ih) = (200, 150);
        let img = image_with_colors(&mut rng, iw, ih, 12, true);
        let (x, y, w, h) = (37, 11, 90, 100);
        let mut want = Vec::new();
        for r in 0..h {
            let o = ((y + r) * iw + x) * 3;
            want.extend_from_slice(&img[o..o + w * 3]);
        }
        let mut enc = ZrleEncoder::new();
        let data = enc.encode_rect(fmt, &img, iw, x, y, w, h);
        assert!(Decoder::new().decode_rect(fmt, &data, w, h) == want);
    }

    #[test]
    fn cpixel_bytes_follow_the_rfc_for_each_layout() {
        let rgb = [0x11, 0x22, 0x33];
        let cp = |big_endian, shifts| {
            let f = PixelFormat {
                big_endian,
                shifts,
                depth: 24,
            };
            let mut v = Vec::new();
            f.push_cpixel(&mut v, rgb);
            v
        };
        // little-endian r16 g8 b0: value 0x00112233, wire 33 22 11 00.
        assert_eq!(cp(false, [16, 8, 0]), [0x33, 0x22, 0x11]);
        // big-endian r16 g8 b0: wire 00 11 22 33.
        assert_eq!(cp(true, [16, 8, 0]), [0x11, 0x22, 0x33]);
        // noVNC's usual little-endian r0 g8 b16: value 0x00332211.
        assert_eq!(cp(false, [0, 8, 16]), [0x11, 0x22, 0x33]);
        // colours in the high bytes, little-endian: value 0x11223300, wire 00 33 22 11.
        assert_eq!(cp(false, [24, 16, 8]), [0x33, 0x22, 0x11]);
        // ... big-endian: wire 11 22 33 00.
        assert_eq!(cp(true, [24, 16, 8]), [0x11, 0x22, 0x33]);
        // depth 32 keeps all four bytes.
        let f = PixelFormat {
            depth: 32,
            ..PixelFormat::DEFAULT
        };
        assert_eq!(f.cpixel_len(), 4);
    }

    /// A 1920x1080 console-like screen: light 8x16 glyph cells on dark, with
    /// a status bar. Prints the sizes so they can be quoted.
    #[test]
    fn a_text_screen_compresses_far_below_raw() {
        let (w, h) = (1920usize, 1080usize);
        let mut rng = Rng(42);
        let mut img = vec![[24u8, 26, 30]; w * h];
        for row in 0..(h - 48) / 16 {
            let cols = 40 + rng.next() as usize % (w / 8 - 40);
            for col in 0..cols {
                if rng.next().is_multiple_of(6) {
                    continue; // a space
                }
                let glyph: Vec<u16> = (0..16)
                    .map(|r| {
                        if (3..13).contains(&r) {
                            rng.next() as u16 & 0x7E
                        } else {
                            0
                        }
                    })
                    .collect();
                for (gy, bits) in glyph.iter().enumerate() {
                    for gx in 0..8 {
                        if bits >> gx & 1 != 0 {
                            img[(row * 16 + gy) * w + col * 8 + gx] = [210, 214, 205];
                        }
                    }
                }
            }
        }
        for y in h - 32..h {
            for x in 0..w {
                img[y * w + x] = [20, 90, 160];
            }
        }
        let rgb: Vec<u8> = img.iter().flatten().copied().collect();
        let fmt = PixelFormat {
            big_endian: false,
            shifts: [0, 8, 16],
            depth: 24,
        };
        let zrle = round_trip(fmt, &rgb, w, h);
        let raw = w * h * 4;
        eprintln!("1920x1080 text screen: ZRLE {zrle} bytes, Raw {raw} bytes");
        assert!(zrle * 10 < raw, "ZRLE {zrle} vs Raw {raw}");
    }

    #[test]
    fn set_pixel_format_rejects_what_it_cannot_serve() {
        let ok = PixelFormat::DEFAULT.to_bytes();
        assert_eq!(PixelFormat::parse(&ok), Ok(PixelFormat::DEFAULT));
        let mut b = ok;
        b[0] = 16;
        assert!(PixelFormat::parse(&b).unwrap_err().contains("16 bits"));
        let mut b = ok;
        b[3] = 0;
        assert!(PixelFormat::parse(&b).unwrap_err().contains("colour-map"));
        let mut b = ok;
        b[5] = 31;
        assert!(PixelFormat::parse(&b).unwrap_err().contains("255"));
        let mut b = ok;
        b[11] = 4;
        assert!(PixelFormat::parse(&b).unwrap_err().contains("shifts"));
    }
}
