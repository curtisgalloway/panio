<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# OCR: the helper protocol

paniolo reads the target's screen by piping a captured frame to an **OCR
helper binary** and parsing the result. This page is the contract a helper
implements, and which helper runs where.

The helper is a separate process because the best engine on each platform is in
a different language: Swift for Apple Vision, Rust for Windows'
`Windows.Media.Ocr`, Python for Tesseract.

## Which engine runs where

Defaults are **platform-native**, for accuracy and latency:

| Platform | Helper | Engine | Installed by |
| --- | --- | --- | --- |
| macOS | `visionocr` | Apple Vision (`VNRecognizeTextRequest`) | `paniolo setup` compiles `ocr/visionocr.swift` with `swiftc` |
| Windows | `winocr` | `Windows.Media.Ocr` (in-box, offline) | `paniolo setup` builds `ocr/winocr`; the release zip ships it in `libexec` |
| Linux (console/firmware screens) | `linuxocr` | Tesseract 5 | `paniolo setup` copies `ocr/linuxocr`; needs `tesseract-ocr` |
| Linux (GUI screens) | `rapidocr` | PP-OCRv6 via RapidOCR on ONNX Runtime | `paniolo setup` copies `ocr/rapidocr` and, when a lab file asks for it, builds its venv |

`$PANIOLO_VISIONOCR` overrides the choice with an explicit path.

So the same screen OCRs differently on different control hosts. Every result
names its engine (`engine`, `engine_detail`), and the override lets a lab force
one engine across hosts when comparing runs.

### Linux needs two engines; the other platforms need one

`ocr_mode` on the target's `video` channel selects the Linux engine: `"text"`
(the default) or `"gui"`. Set it with `paniolo video set -t <target> --ocr-mode
gui` (see [video.md](../video.md#ocr)). It is configured because it cannot be
inferred at runtime (see the confidence table under
[The contract](#the-contract)).

Measured on a Pi 5 control host against `evals/ocr`:

| screen type | rapidocr | linuxocr (Tesseract) |
| --- | --- | --- |
| GUI | **0.083** token-recall error, ~4.2 s | 0.312, ~1.7 s |
| text | 0.025 CER, ~6.5 s | **0.019**, ~2.0 s |

rapidocr is ~4x more accurate on anti-aliased UI text, slightly worse on
console text, and 2-3x slower. It wins on GUI screens because:

1. **Latency is affordable.** Change detection uses the frame hash (`/status`,
   `--changed-since`); OCR runs only when something asks what the screen says,
   usually once after a state change.
2. **Tesseract fails on GUIs by silently omitting text.** On a BIOS boot-order
   page it returned the headings and dropped every value (see
   [What the engines actually do](#what-the-engines-actually-do)): a confident,
   complete-looking, wrong answer.
3. **So "run Tesseract, fall back on low confidence" cannot work.** Tesseract
   is confident about the rows it *did* read; the missing ones raise no signal.

**No preprocessing for rapidocr.** Upscaling, inversion and binarization were
all slower than `raw` and none more accurate. `visionocr` and `linuxocr`
upscale internally.

**The venv is opt-in.** It is ~317 MB (onnxruntime 58 MB, models 31 MB,
numpy/opencv the rest); `paniolo setup` builds it only when some target sets
`ocr_mode = "gui"`. It is a venv because Pi OS is PEP 668-managed (the system
Python refuses `pip install`). `opencv-python-headless` replaces the
`opencv-python` RapidOCR pulls in: the full build needs `libGL.so.1`, which
headless Pi OS lacks, and fails at first OCR rather than at install.

## The contract

**Help:** `paniolo helper <name> --help` (or `-h`) prints usage and exits 0
without reading an image. All helpers accept `--json`; `visionocr` also offers
`--accurate` (the default), `--fast`, and `--self-test` (internal checks without
an image).

**Input:** a PNG on stdin (`-`) or a path argument.

- `rapidocr` errors on anything without the PNG signature (see
  [Resource limits](#resource-limits)).
- The others accept whatever their platform's image loader recognizes; do not
  rely on that.

**Output:** plain text by default, one line per recognized line, in reading
order, for humans.

**paniolo always passes `--json`**, the machine contract:

```json
{
  "version": 1,
  "engine": "visionocr",
  "engine_detail": "Apple Vision VNRecognizeTextRequest, fast",
  "width": 1920,
  "height": 1080,
  "text": "login:\nPassword:",
  "lines": [
    { "text": "login:", "confidence": 0.97, "bbox": [120, 880, 96, 28] }
  ]
}
```

- `version` — this document's version. Bump on any incompatible change.
- `engine` / `engine_detail` — what produced the result; explains why two
  hosts disagree about a screen.
- `width` / `height` — **the source image's** dimensions, in pixels.
- `text` — every line joined with `\n`, in reading order.
- `lines[].confidence` — `0.0`–`1.0`, and **optional**.
  - A helper normalizes an engine's other scale: Tesseract's `0`–`100` is
    divided by 100, and its `-1` ("no text") means the line is omitted, never
    reported as `0.0`.
  - An engine with no confidence omits the field. Do not read its absence as
    zero.

  Confidence is the least dependable part of this contract; do not design
  around it:

  | Engine | Confidence |
  | --- | --- |
  | Tesseract | Real per-word values |
  | Apple Vision | **Constant** — 0.5 for every line in `--fast`, 1.0 in `--accurate` (one distinct value over a 56-line frame). It indicates the recognition level, not quality. |
  | `Windows.Media.Ocr` | **Not exposed at all** — the field is absent |

  So engine routing is configured, not inferred from confidence.
- `lines[].bbox` — `[x, y, w, h]` in **pixels, origin top-left, in source-image
  coordinates**, always intersected with the source frame: a line crossing an
  edge is reported at the size it occupies inside `[0, width) x [0, height)`.

### The bbox rule is the sharp edge

**Every helper must map its boxes back to the source frame.** Helpers
preprocess, so engine coordinates refer to a *different image* than the caller
supplied. `visionocr`, for example, upscales 2× and pads 16 px (small console
fonts recognize better enlarged; edge glyphs get clipped otherwise).

Consumers crop or click a bbox through the hid channel. A box in an
intermediate buffer's coordinates aims slightly wrong, which is easy to miss
because it is close.

Each helper undoes its own conventions:

- Apple Vision reports **normalized, bottom-left-origin** boxes against the
  upscaled, padded buffer. `visionocr` flips the y axis as well as undoing its
  scale and padding.
- Tesseract reports pixels on that same preprocessed image.
- `Windows.Media.Ocr` reports pixels on the source.

**Mapping must clip, not clamp.** Map *both* corners into source coordinates,
intersect with `[0, width] x [0, height]`, and derive width and height from the
clipped corners. An edge crossing then shrinks the box, and a box entirely
outside the source (garbage input) clips to zero size, not negative.

Two past defects:

- `visionocr --json` reported coordinates normalized against its padded,
  upscaled buffer: off by a few pixels from the unremoved padding, and in the
  wrong units (`0.137` where the answer was `104`).
- A later fix clamped a mapped corner to 0 at the left/top edge without
  shrinking the width/height, so a line flush with the left edge came back a
  few pixels too wide.

## What paniolo does with it

`hdmicap`'s `GET /ocr` runs the helper with `--json` and returns the envelope as
`application/json`. `paniolo video read` prints `text` by default and the whole
envelope under `--json`.

**Version skew degrades rather than fails, in both directions**, because the
helper, daemon and CLI upgrade separately:

- If a helper's stdout does not parse as an envelope, `/ocr` treats it as plain
  text from a pre-v1 helper: it synthesizes an envelope with no `lines` (never
  fabricated boxes) and logs a warning naming the binary.
- `paniolo video read` passes a non-envelope body through unchanged, so a CLI
  newer than its daemon still reads screens.

The envelope check is `version` being present, not "the body is JSON", so a
screen that *shows* JSON with a `text` key is not mined for it.

### Coordinates cross-check

From three different native conventions (see
[the bbox rule](#the-bbox-rule-is-the-sharp-edge)), the helpers converge on the
same frame's first line:

| Helper | bbox |
| --- | --- |
| `visionocr` | `[0, 33, 396, 21]` |
| `linuxocr` | `[2, 34, 390, 16]` |
| `winocr` | `[2, 34, 391, 17]` |

Agreement within a few pixels shows the mapping-back rule is applied. Re-run
it when a helper's preprocessing changes.

## What the engines actually do

Measured on the 13 dongle captures in `evals/ocr/dataset`. The hardest frame
is an AMI BIOS page whose boot-order values sit in cyan dropdown widgets:

| Engine | Boot Option values |
| --- | --- |
| Apple Vision `--accurate` | `UEFI: PXE IPv4 Intel(R) Ethernet C` — exact |
| `Windows.Media.Ocr` | `UEFI: PXE IPva Intel(R) Ethernet C` — reads them, fumbles digits |
| Tesseract | **None.** The widget text does not survive at all |

Tesseract's failure is the dangerous one: well-formed text with whole rows
missing. Vision and winocr garble visibly.

Digit/letter confusion is common and hits the strings bring-up cares about. On
a PXE screen's MAC address:

| Engine | Result |
| --- | --- |
| Apple Vision `--accurate` | `54-B2-03-F0-B5-5C` — exact |
| `Windows.Media.Ocr` | `S4-B2-03-FO-BS-SC` — 5→S, 0→O |
| Apple Vision `--fast` | `54-B2-03-FO-B5-5C` — one 0→O |

Match such strings loosely or corroborate them; do not trust an exact compare.

## Resource limits

paniolo does not bound the bytes or resolution a video channel produces. Every
helper enforces the same three limits before the expensive work (a full decode,
or the 2x upscale in `linuxocr`/`visionocr`), so oversized input fails fast
instead of driving an oversized allocation.

**64 MiB of encoded input.** Each helper reads in bounded chunks, up to the
limit plus one byte, and errors as soon as it sees more than the limit.

**8192 px on a side, 33,177,600 px total** (7680x4320).

- **Before decode**, each helper checks the size the image header declares,
  without rasterizing pixels:
  - `linuxocr`/`rapidocr` parse the PNG IHDR (header chunk) by hand.
  - `visionocr` reads ImageIO's properties (`CGImageSourceCopyPropertiesAtIndex`).
  - `winocr` reads `BitmapDecoder.PixelWidth`/`PixelHeight` before calling
    `GetSoftwareBitmapAsync()`.
- **After decode**, each helper checks again, unconditionally, backstopping
  whatever the header parse missed (Pillow/OpenCV decoding a format the PNG
  check doesn't recognize). For `linuxocr`/`visionocr` this still precedes the
  2x upscale.
- 33,177,600 is exactly 2x a 4K capture (3840x2160) in each dimension, so any
  4K frame passes, with no margin on the pixel count.
- `linuxocr` and `visionocr` check the pair against the source size and again
  against the *working* size of their 2x upscale. The fixed padding (~20-32 px)
  is not part of the limit.
- `rapidocr` and `winocr` do no upscaling, so the limit applies directly to the
  source. `winocr` also intersects the per-side limit with
  `OcrEngine::MaxImageDimension()`, using whichever is stricter.

**`rapidocr` refuses input that is not a PNG**, before decoding anything. Its
pre-decode check reads the PNG IHDR, but `cv2.imdecode` also accepts JPEG, BMP
and WebP, so a JPEG whose SOF (JPEG size header) declared an enormous size
reached the decoder in full (#167). Refusing non-PNG matches the input contract
and what hdmicap sends.

The three numbers must stay **identical** across `ocr/linuxocr`,
`ocr/rapidocr`, `ocr/visionocr.swift` and `ocr/winocr/src/main.rs`. Each
defines them once as named constants, with a comment pointing at the other
three.

**Errors are one line, on stderr, non-zero exit** — the same path each helper
uses for "cannot read", "could not decode image", and so on.

## Every failure leaves the same way

A missing input file, non-image bytes, and a mid-read I/O error all go through
the helper's error exit, because the caller is a daemon parsing stderr. These
paths once escaped it:

| Helper | Was | Now |
| --- | --- | --- |
| `linuxocr` | Opened its input file bare: a missing path gave a `FileNotFoundError` traceback. Preprocessing caught only `ImportError` (a missing Pillow), so non-image bytes gave a `PIL.UnidentifiedImageError` traceback. | Both go through `die()`. |
| `rapidocr` | Opened its input file bare, with the same result. | Goes through the error exit. |
| `visionocr` | Read with `try?`, which turns a failed read into `nil` (same as EOF), so it OCR'd a truncated prefix silently. | Its bounded reader `rethrows`, and the caller dies with the underlying error. |
| `linuxocr` | Spawned `tesseract` bare. A host without it (common: the `.deb` only Recommends it) got a `FileNotFoundError` traceback; a non-zero exit forwarded raw multi-line stderr. | Both go through `die()`: the missing-binary message names the apt package, and a non-zero exit collapses to one line keeping tesseract's message. |

CI runs the Python helpers' error-path tests (`ocr/tests`) and `visionocr`'s
`--self-test`.

## Adding an engine

1. Read a PNG from stdin or a path; support `--json`.
2. Emit the envelope above, with boxes mapped back to source coordinates.
3. Name it in `daemons::helper_dirs()`'s install path and in `paniolo setup`.

If the engine cannot report confidence, omit it rather than invent a number;
real confidence enables fallback routing and high-confidence token matching.
