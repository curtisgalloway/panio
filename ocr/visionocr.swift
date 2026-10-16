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

// visionocr — read text from an image using Apple's Vision framework.
// On-device, no network, no model download. Reads an image (path arg or PNG on
// stdin) and prints recognized text in reading order, one observation per line.
//
//   visionocr [--fast] [--json] [PATH | -]
//   visionocr --self-test
//
//   --fast        use the fast recognition level (lower latency, worse on
//                 every frame measured — see the note at recognitionLevel
//                 below)
//   --json        emit the v1 OCR envelope (see docs/dev/ocr.md): engine identity,
//                 source dimensions, joined text, and per-line text +
//                 confidence + [x, y, w, h] bbox in SOURCE pixels, origin
//                 top-left
//   --self-test   run the pure helpers and exit 0/1 -- the bbox clipping math
//                 (clipBoxToSource) against a set of edge-crossing cases, the
//                 shared dimension limits, and the bounded reader, including
//                 that it propagates a read error rather than reading it as
//                 EOF; no image or Vision needed

import CoreGraphics
import Foundation
import ImageIO
import Vision

func die(_ msg: String) -> Never {
    FileHandle.standardError.write(("visionocr: " + msg + "\n").data(using: .utf8)!)
    exit(1)
}

// Resource limits shared with ocr/linuxocr, ocr/rapidocr and
// ocr/winocr/src/main.rs -- maxEncodedBytes, maxDimension and maxPixels must
// be identical across all four helpers (see docs/dev/ocr.md's "Resource
// limits" section). This helper upscales 2x during preprocessing (see
// upscaleAndPad below), so maxDimension/maxPixels are sized against that
// *working* (post-2x, before padding) size: exactly 2x a 4K capture
// (3840x2160) in each dimension, so any 4K frame passes with margin. The
// small fixed padding upscaleAndPad adds on top is not part of the limit --
// it is a constant 32 px, negligible against an 8192px/33-megapixel budget.
let maxEncodedBytes = 64 * 1024 * 1024  // 64 MiB of encoded input
let maxDimension = 8192  // px, per side, of the post-upscale (pre-pad) working image
let maxPixels = 33_177_600  // 7680x4320 total px of the post-upscale (pre-pad) image

// Reject dimensions before they drive an allocation. Pure so `--self-test`
// can exercise it without an image or Vision; returns a one-line message
// when `w`x`h` exceeds either shared limit, nil otherwise.
func checkDimensions(_ w: Int, _ h: Int) -> String? {
    if w > maxDimension || h > maxDimension {
        return "image is \(w)x\(h); exceeds the \(maxDimension)px-per-side limit"
    }
    if w * h > maxPixels {
        return "image is \(w)x\(h) (\(w * h) px); exceeds the \(maxPixels)px limit"
    }
    return nil
}

// Reads chunks from `next(want)` -- each call asks for no more than what's
// left before `maxBytes` + 1 -- until input ends or the limit is passed.
// Pure aside from the `next` closure, so `--self-test` can drive it with an
// in-memory chunk source instead of a real FileHandle; the actual callers
// below wrap `FileHandle.read(upToCount:)`. Returns whatever was read,
// which may be up to one byte more than `maxBytes` -- the caller decides
// whether that's an error (it always is, here).
//
// `next` throws and this rethrows, so a read error on the descriptor reaches
// the caller as an error. The call site used to spell it `try?`, which
// collapses a failed read into nil -- indistinguishable from EOF, so the
// helper would go on to OCR a silently truncated image and report whatever
// text survived (#168). Non-throwing closures, like the self-test's, are
// unaffected by `rethrows`.
func readBounded(maxBytes: Int, next: (_ want: Int) throws -> Data?) rethrows -> Data {
    var data = Data()
    while data.count <= maxBytes {
        let want = maxBytes + 1 - data.count
        guard let chunk = try next(want), !chunk.isEmpty else { break }
        data.append(chunk)
    }
    return data
}

// Upscale and black-pad an image. Small thin console text recognizes far better
// when enlarged, and padding stops glyphs flush to the frame edge from being
// clipped (which drops the first/last character of a line).
func upscaleAndPad(_ img: CGImage, scale: CGFloat, pad: Int) -> CGImage? {
    let w = Int((CGFloat(img.width) * scale).rounded())
    let h = Int((CGFloat(img.height) * scale).rounded())
    let outW = w + pad * 2
    let outH = h + pad * 2
    guard
        let ctx = CGContext(
            data: nil, width: outW, height: outH, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { return nil }
    ctx.setFillColor(CGColor(red: 0, green: 0, blue: 0, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: outW, height: outH))
    ctx.interpolationQuality = .high
    ctx.draw(img, in: CGRect(x: pad, y: pad, width: w, height: h))
    return ctx.makeImage()
}

// Map a processed-space box (top-left pixel origin, both corners) into
// source coordinates and clip it to the source image. `toSource` below
// converts a Vision-normalized, bottom-left-origin box into these
// coordinates first; this function is the pure math and is exercised
// directly by `--self-test`.
//
// Both corners are mapped independently through the inverse of the
// preprocessing (undo the padding, then the upscale) and rounded, then
// intersected with [0, srcW] x [0, srcH] so the returned box never extends
// past the source frame — a recognition rectangle that crosses an edge
// shrinks on that side instead of reporting a size measured in the padding.
// A box that lands entirely outside the source (only possible for garbage
// input) clips to zero size rather than negative.
func clipBoxToSource(
    px0: CGFloat, py0: CGFloat, px1: CGFloat, py1: CGFloat,
    pad: CGFloat, scale: CGFloat, srcW: CGFloat, srcH: CGFloat
) -> [Int] {
    let sx0 = ((px0 - pad) / scale).rounded()
    let sy0 = ((py0 - pad) / scale).rounded()
    let sx1 = ((px1 - pad) / scale).rounded()
    let sy1 = ((py1 - pad) / scale).rounded()

    let x0 = min(max(0, sx0), srcW)
    let y0 = min(max(0, sy0), srcH)
    let x1 = max(min(sx1, srcW), x0)
    let y1 = max(min(sy1, srcH), y0)

    return [Int(x0), Int(y0), Int(x1 - x0), Int(y1 - y0)]
}

private struct BBoxCase {
    let name: String
    let px0: CGFloat
    let py0: CGFloat
    let px1: CGFloat
    let py1: CGFloat
    let pad: CGFloat
    let scale: CGFloat
    let srcW: CGFloat
    let srcH: CGFloat
    let expected: [Int]
}

// Exercises clipBoxToSource against the edge-crossing cases from issue #149,
// without needing Vision or an actual image. Numbers mirror ocr/tests'
// Python cases against the same synthetic source frame (100 x 80) and
// preprocessing (scale=2, pad=10).
private func runSelfTest() -> Bool {
    let cases: [BBoxCase] = [
        BBoxCase(
            name: "fully inside", px0: 40, py0: 40, px1: 60, py1: 50,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [15, 15, 10, 5]),
        BBoxCase(
            name: "left edge crossing", px0: 0, py0: 40, px1: 30, py1: 60,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [0, 15, 10, 10]),
        BBoxCase(
            name: "top edge crossing", px0: 40, py0: 0, px1: 60, py1: 30,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [15, 0, 10, 10]),
        BBoxCase(
            name: "right edge crossing", px0: 190, py0: 40, px1: 230, py1: 60,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [90, 15, 10, 10]),
        BBoxCase(
            name: "bottom edge crossing", px0: 40, py0: 150, px1: 60, py1: 190,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [15, 70, 10, 10]),
        BBoxCase(
            name: "fully outside bottom-right", px0: 300, py0: 300, px1: 340, py1: 340,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [100, 80, 0, 0]),
        BBoxCase(
            name: "fully outside top-left", px0: -40, py0: -40, px1: -10, py1: -10,
            pad: 10, scale: 2, srcW: 100, srcH: 80, expected: [0, 0, 0, 0]),
    ]

    var ok = true
    var count = 0
    func fail(_ name: String, _ got: Any, _ want: Any) {
        FileHandle.standardError.write(
            "visionocr --self-test: FAIL \(name): got \(got), want \(want)\n"
                .data(using: .utf8)!)
        ok = false
    }

    for c in cases {
        count += 1
        let got = clipBoxToSource(
            px0: c.px0, py0: c.py0, px1: c.px1, py1: c.py1,
            pad: c.pad, scale: c.scale, srcW: c.srcW, srcH: c.srcH)
        if got != c.expected {
            fail(c.name, got, c.expected)
        }
    }

    // checkDimensions -- issue #150/#151's shared resource limits.
    count += 1
    if let err = checkDimensions(100, 100) {
        fail("checkDimensions accepts 100x100", err, "nil")
    }
    count += 1
    if checkDimensions(maxDimension + 1, 100) == nil {
        fail("checkDimensions rejects over per-side limit", "nil", "an error")
    }
    count += 1
    // 6000x6000 is under maxDimension (8192) on each side but over maxPixels
    // (33,177,600) in total: 36,000,000 > 33,177,600.
    if checkDimensions(6000, 6000) == nil {
        fail("checkDimensions rejects over pixel-count limit", "nil", "an error")
    }
    count += 1
    let atLimitHeight = maxPixels / maxDimension
    if let err = checkDimensions(maxDimension, atLimitHeight) {
        fail("checkDimensions accepts exactly the limits", err, "nil")
    }

    // readBounded, driven from an in-memory chunk source rather than a real
    // FileHandle -- one 200-byte chunk, then EOF.
    count += 1
    let exact = Data(repeating: 7, count: 10)
    var servedExact = false
    let gotExact = readBounded(maxBytes: 10) { _ in
        if servedExact { return nil }
        servedExact = true
        return exact
    }
    if gotExact != exact {
        fail("readBounded returns data at exactly the cap", gotExact.count, exact.count)
    }
    count += 1
    let over = Data(repeating: 7, count: 11)
    var servedOver = false
    let gotOver = readBounded(maxBytes: 10) { _ in
        if servedOver { return nil }
        servedOver = true
        return over
    }
    if gotOver.count <= 10 {
        fail("readBounded surfaces one byte over the cap", gotOver.count, 11)
    }
    count += 1
    // Multiple small chunks, well under the cap: nothing should be dropped.
    var chunks = [Data([1, 2]), Data([3, 4, 5]), Data([6])]
    let gotChunked = readBounded(maxBytes: 100) {
        _ in chunks.isEmpty ? nil : chunks.removeFirst()
    }
    if gotChunked != Data([1, 2, 3, 4, 5, 6]) {
        fail("readBounded reassembles multiple chunks", Array(gotChunked), [1, 2, 3, 4, 5, 6])
    }
    count += 1
    // A read error must reach the caller rather than looking like EOF -- the
    // `try?` this replaced turned a failed descriptor read into a short image
    // the helper then OCR'd without complaint (#168).
    struct FakeReadError: Error {}
    var chunksBeforeFailure = 1
    var propagated = false
    do {
        _ = try readBounded(maxBytes: 100) { _ in
            if chunksBeforeFailure > 0 {
                chunksBeforeFailure -= 1
                return Data([1, 2, 3])
            }
            throw FakeReadError()
        }
    } catch is FakeReadError {
        propagated = true
    } catch {
        // Some other error: still not the one thrown, so the check below fails.
    }
    if !propagated {
        fail("readBounded propagates a read error", "returned normally", "FakeReadError")
    }
    count += 1
    // The error must surface even when it lands on the very first read, with
    // no partial data to be mistaken for a complete short image.
    var propagatedImmediately = false
    do {
        _ = try readBounded(maxBytes: 100) { _ in throw FakeReadError() }
    } catch is FakeReadError {
        propagatedImmediately = true
    } catch {
        // As above.
    }
    if !propagatedImmediately {
        fail(
            "readBounded propagates an error from the first read",
            "returned normally", "FakeReadError")
    }

    if ok {
        print("visionocr --self-test: all \(count) cases passed")
    }
    return ok
}

// Checks a source size and the working (post-upscale, pre-pad) size
// upscaleAndPad is about to allocate for it against the shared limits, and
// dies with checkDimensions's message if either is over. The small fixed
// padding upscaleAndPad adds on top is not part of the check -- see
// maxDimension/maxPixels's comment.
func enforceDimensionLimits(sourceW: Int, sourceH: Int, scale: CGFloat) {
    if let err = checkDimensions(sourceW, sourceH) { die(err) }
    let workingW = Int((Double(sourceW) * Double(scale)).rounded())
    let workingH = Int((Double(sourceH) * Double(scale)).rounded())
    if let err = checkDimensions(workingW, workingH) { die(err) }
}

var accurate = true
var json = false
var selfTest = false
var path: String? = nil
for arg in CommandLine.arguments.dropFirst() {
    switch arg {
    case "--help", "-h":
        print("""
        Read text from an image using Apple Vision (on-device).

        Usage: visionocr [OPTIONS] [PATH | -]

        Read an image from PATH, or stdin when PATH is omitted or '-'.
        Print recognized text in reading order by default.

        Options:
          --json        Emit the v1 OCR JSON envelope with source-pixel boxes
          --accurate    Use accurate recognition (default)
          --fast        Use faster, less accurate recognition
          --self-test   Run internal checks without an image; exit 0 on success
          -h, --help    Print help and exit
        """)
        exit(0)
    case "--accurate": accurate = true
    case "--fast": accurate = false
    case "--json": json = true
    case "--self-test": selfTest = true
    case "-": path = nil
    default: path = arg
    }
}

if selfTest {
    exit(runSelfTest() ? 0 : 1)
}

let handle: FileHandle
if let p = path {
    guard let h = FileHandle(forReadingAtPath: p) else { die("cannot read \(p)") }
    handle = h
} else {
    handle = FileHandle.standardInput
}
let data: Data
do {
    data = try readBounded(maxBytes: maxEncodedBytes) { want in
        try handle.read(upToCount: want)
    }
} catch {
    die("cannot read \(path ?? "stdin"): \(error)")
}
if data.count > maxEncodedBytes {
    die("input is over \(maxEncodedBytes) bytes (encoded-input limit)")
}
if data.isEmpty { die("no image data") }

guard let src = CGImageSourceCreateWithData(data as CFData, nil) else {
    die("could not decode image")
}

let preScale: CGFloat = 2.0
let prePad = 16

// Inspect dimensions from the image header -- ImageIO reports these without
// decoding pixels -- before CGImageSourceCreateImageAtIndex below ever
// rasterizes the image, checking both the source size and the working
// (post-upscale) size upscaleAndPad is about to allocate.
if let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any],
    let headerW = props[kCGImagePropertyPixelWidth] as? Int,
    let headerH = props[kCGImagePropertyPixelHeight] as? Int
{
    enforceDimensionLimits(sourceW: headerW, sourceH: headerH, scale: preScale)
}

guard let decoded = CGImageSourceCreateImageAtIndex(src, 0, nil) else {
    die("could not decode image")
}
// Backstop for inputs the header-based check above could not read a size
// from (should not happen for a format ImageIO recognizes, but the decode
// has already run by here regardless of whether that check ran).
enforceDimensionLimits(sourceW: decoded.width, sourceH: decoded.height, scale: preScale)

let upscaled = upscaleAndPad(decoded, scale: preScale, pad: prePad)
let image = upscaled ?? decoded
// When upscaleAndPad falls back, no transform was applied and the boxes are
// already in source coordinates.
let appliedScale: CGFloat = upscaled == nil ? 1.0 : preScale
let appliedPad: CGFloat = upscaled == nil ? 0.0 : CGFloat(prePad)

let request = VNRecognizeTextRequest()
// This defaulted to .fast, on the belief that .accurate (tuned for natural
// document text) missed small thin console fonts entirely. Measured against 13
// real dongle captures in evals/ocr/dataset, that is not what happens —
// .accurate is better or equal on every one, including the console frames the
// old default existed to protect:
//
//   BIOS dropdowns   .accurate reads "UEFI: PXE IPv4 Intel(R) Ethernet C";
//                    .fast garbles it to "UEFI: PXE11*4 IntellR> Ethemet c"
//   PXE/Gigaboot     .accurate reads the MAC "54-B2-03-F0-B5-5C" correctly;
//                    .fast returns "54-B2-03-FO-B5-5C" — letter O for zero
//   repeated lines   .accurate finds 21 of 24 "GetGicDriver" lines; .fast 15
//
// The one frame where .fast returns more is a Fuchsia virtcon showing an
// ASCII-art logo: .fast emits 29 lines of hallucinated text off the artwork
// ("ff ffftfflflff ff") and .accurate emits none. Neither reads that frame's
// real status line, so .fast is not finding anything there — it is inventing,
// which is worse for a caller that cannot tell the difference.
request.recognitionLevel = accurate ? .accurate : .fast
// Console/boot/code text is not natural language; correction hurts more than
// it helps (it "fixes" identifiers, hex, paths).
request.usesLanguageCorrection = false
// Vision's default minimumTextHeight (1/32 of image height) skips small console
// fonts. It's a fraction of height; 0.0 means "default", so use a small
// positive floor to catch tiny text.
request.minimumTextHeight = 0.005

let handler = VNImageRequestHandler(cgImage: image, options: [:])
do {
    try handler.perform([request])
} catch {
    die("\(error)")
}

let observations = request.results ?? []

// Vision returns observations unordered. Sort into reading order. boundingBox
// origin is bottom-left, so a larger y is higher on screen.
let sorted = observations.sorted { a, b in
    let dy = a.boundingBox.origin.y - b.boundingBox.origin.y
    if abs(dy) > 0.01 { return dy > 0 }
    return a.boundingBox.origin.x < b.boundingBox.origin.x
}

if json {
    // Vision reports normalized, bottom-left-origin boxes against the image it
    // was handed — which is the upscaled, padded one, not the caller's frame.
    // Map back to source pixels with a top-left origin: undo the normalization,
    // flip y, then remove the padding and the scale. Reporting boxes in the
    // coordinates of an intermediate buffer would silently aim any consumer
    // (a crop, or a hid click) at the wrong place. See docs/dev/ocr.md.
    let procW = CGFloat(image.width)
    let procH = CGFloat(image.height)
    let srcW = CGFloat(decoded.width)
    let srcH = CGFloat(decoded.height)

    func toSource(_ b: CGRect) -> [Int] {
        let px0 = b.origin.x * procW
        // Flip to a top-left origin while still in processed pixels.
        let py0 = (1.0 - b.origin.y - b.size.height) * procH
        let px1 = (b.origin.x + b.size.width) * procW
        let py1 = (1.0 - b.origin.y) * procH
        return clipBoxToSource(
            px0: px0, py0: py0, px1: px1, py1: py1,
            pad: appliedPad, scale: appliedScale, srcW: srcW, srcH: srcH)
    }

    var lines: [[String: Any]] = []
    var texts: [String] = []
    for obs in sorted {
        guard let top = obs.topCandidates(1).first else { continue }
        texts.append(top.string)
        lines.append([
            "text": top.string,
            "confidence": top.confidence,
            "bbox": toSource(obs.boundingBox),
        ])
    }
    let envelope: [String: Any] = [
        "version": 1,
        "engine": "visionocr",
        "engine_detail": "Apple Vision VNRecognizeTextRequest, "
            + (accurate ? "accurate" : "fast"),
        "width": Int(srcW),
        "height": Int(srcH),
        "text": texts.joined(separator: "\n"),
        "lines": lines,
    ]
    let out = try JSONSerialization.data(withJSONObject: envelope, options: [.prettyPrinted])
    FileHandle.standardOutput.write(out)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
} else {
    for obs in sorted {
        if let top = obs.topCandidates(1).first {
            print(top.string)
        }
    }
}
