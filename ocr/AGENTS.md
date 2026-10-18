<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# ocr/ — agent notes

This file holds the internals of this component, moved out of the root AGENTS.md to keep it short. The rules in the root AGENTS.md still apply; per-OS behavior lives in `docs/dev/platform-support.md`.

## Module layout

```
ocr/             OCR helpers (compiled/installed binaries are gitignored):
                   visionocr.swift  Apple Vision OCR (macOS); built by panio setup via swiftc
                   linuxocr         Tesseract OCR wrapper (Linux); copied by panio setup
```

## OCR

Two entry points feed the same warm frame:
- **`panio video read [target] [--stable]`** wraps the daemon's `GET /ocr`.
- **Dashboard button + hdmicap `GET /ocr`**: the daemon PNG-encodes the current frame, pipes it to the OCR tool (`tokio::process`) and returns the text. It finds the tool via `PANIO_VISIONOCR` (the installed path, set by `panio video watch` so a stale PATH hit is never used), then a `visionocr`/`linuxocr` sibling of its own executable (libexec dir), then bare PATH. If absent, `/ocr` returns 501 and the button shows an error.

`panio setup` installs the platform-appropriate tool. Every OCR helper accepts `--help` and `-h` without reading image input; `panio helper <name> --help` prints its options and exits 0.

**macOS: `ocr/visionocr.swift`** (`VNRecognizeTextRequest`, Apple Vision) runs on-device with no network or model download. `panio setup` compiles it with `swiftc` into the libexec dir (`~/.local/libexec/panio/bin`). Tuning that matters for small console text:
- `recognitionLevel = .accurate` is the default; `--fast` selects `.fast`. Measured on `evals/ocr/dataset`, `.accurate` is better or equal on every frame, console text included, while `.fast` garbles BIOS menus and reads MAC zeros as the letter O (see the comment at `recognitionLevel`).
- The tool 2×-upscales and black-pads the frame before recognition (fixes colon misreads and first-character clipping at the edge).
- `minimumTextHeight` is lowered (it is a fraction of image height; the default 1/32 skips ~16px console text).

**Linux: `ocr/linuxocr`** (Tesseract) is copied into the libexec dir by `panio setup`. It needs `sudo apt-get install tesseract-ocr`; Pillow (`pip install Pillow`) is optional and enables the same 2×-upscale + black-pad preprocessing.

## Gotchas

- **Character confusions on small console fonts** (`1`↔`l`↔`I`, `2`↔`Z`, IPv6 colon spacing) persist despite the preprocessing; larger boot-screen text is markedly better. Fix them with higher capture resolution or Tesseract's `--psm` mode. **Do not change the target's console font** to improve OCR: other agents (e.g. the Fuchsia bring-up agent reading kernel/bootloader output) rely on it.
