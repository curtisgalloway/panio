<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# ocr/ — agent notes

This file holds the internals of this component, moved out of the root AGENTS.md to keep it short. The rules in the root AGENTS.md still apply; per-OS behavior lives in `docs/dev/platform-support.md`.

## Module layout

```
ocr/             OCR helpers (compiled/installed binaries are gitignored):
                   visionocr.swift  Apple Vision OCR (macOS); built by paniolo setup via swiftc
                   linuxocr         Tesseract OCR wrapper (Linux); copied by paniolo setup
```

## OCR

Two entry points, both feeding the same warm frame:
- **`paniolo video read [target] [--stable]`** — OCRs the current frame (wraps
  the daemon's `GET /ocr`).
- **Dashboard button + hdmicap `GET /ocr`** — the daemon PNG-encodes the current
  frame and pipes it to the OCR tool (`tokio::process`), returning the text. The
  daemon finds the tool via `PANIOLO_VISIONOCR` (the installed path, set by
  `paniolo video watch`), then a `visionocr`/`linuxocr` sibling of its own
  executable (both live in the libexec dir), then bare PATH; if absent, `/ocr`
  returns 501 and the button shows an error.

`paniolo setup` installs the platform-appropriate tool. `PANIOLO_VISIONOCR` is
set to the resolved path when the daemon starts, so the daemon always uses the
installed binary (never a stale PATH hit).

Every OCR helper accepts `--help` and `-h` without reading image input;
`paniolo helper <name> --help` prints its supported options and exits 0.

**macOS — `ocr/visionocr.swift`** (`VNRecognizeTextRequest`, Apple Vision):
on-device, no network, no model download. `paniolo setup` compiles it (`swiftc`)
into the libexec dir (`~/.local/libexec/paniolo/bin`).

Tuning that matters for small console text:
- `recognitionLevel = .accurate` is the default; `--fast` selects `.fast`. The
  old `.fast` default rested on the belief that `.accurate` (tuned for natural
  document text) misses thin console fonts; measured on the captures in
  `evals/ocr/dataset` it is better or equal on every frame, while `.fast`
  garbles BIOS menus and reads MAC zeros as the letter O (see the comment at
  `recognitionLevel` in the source).
- The tool 2×-upscales and black-pads the frame before recognition (fixes colon
  misreads and first-character clipping at the frame edge).
- `minimumTextHeight` is lowered (it's a fraction of image height; the default
  1/32 skips ~16px console text).

**Linux — `ocr/linuxocr`** (Tesseract via `tesseract-ocr` system package):
`paniolo setup` copies the script into the libexec dir. Requires
`sudo apt-get install tesseract-ocr`; Pillow (`pip install Pillow`) is optional
but enables the same 2×-upscale + black-pad preprocessing as visionocr.

**Do not change the target's console font** to try to improve OCR accuracy —
the font is relied upon by other agents (e.g. the Fuchsia bring-up agent that
reads kernel/bootloader output). Character confusions on thin console fonts
(`1`↔`l`↔`I`, IPv6 colons, etc.) are better addressed by increasing capture
resolution or adjusting Tesseract's `--psm` mode.

## Gotchas

- **OCR character confusions on small console fonts.** Both visionocr and linuxocr
  2×-upscale and black-pad before recognition, but thin terminal fonts still
  produce confusions (`1`↔`l`↔`I`, `2`↔`Z`, colon spacing in IPv6). Accuracy
  improves markedly on larger boot-screen text. Do not change the target's console
  font to work around this — the font is relied upon by other agents (see OCR section).
  On macOS, visionocr uses `VNRecognizeTextRequest` `.accurate`, measured better
  than or equal to `.fast` on every captured frame, console text included.
