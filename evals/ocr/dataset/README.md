<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# OCR benchmark frames

13 frames captured through the real dongle path on the CI rack — never
host-side screenshots, never resized, never re-encoded. `MANIFEST.md` (written
by the session that captured them) has the per-file table, target, and
negotiated capture format.

Two properties of this set are worth preserving if it is ever regenerated.

**It contains a genuine MJPEG-vs-raw A/B.** The two NanoKVM-USB units
negotiated MJPEG 1280x720, so those frames carry in-dongle JPEG 4:2:0 artifacts
baked in before hdmicap saw them; the Pi's KVM-USB v2 negotiated YUYV 4:2:2 with
no JPEG stage. Near-identical content on both sides of that split is what lets
the benchmark separate "engine is bad at this" from "the capture destroyed it".
The format is **negotiated, not a device property** — all three dongles offer
both at 1280x720 — so re-check with `v4l2-ctl --get-fmt-video` before drawing
conclusions from any frame captured later.

**Every frame was kept and de-duplicated by hash, not filtered by signal.**
That is not fastidiousness; see below.

## Do not gate captures on `signal`

hdmicap's `signal` field is unreliable in **both** directions, so a collector
that skips `no_signal` shots loses real frames and one that trusts `stable`
records fictional ones.

`text-nuc-pxe-gigaboot-start.png` is a clean, fully readable frame that hdmicap
labelled `no_signal`. It reproduces deterministically from the committed file:
`frame::classify` samples a 32x32 lattice (`GRID = 8 * CELL_SAMPLES`) and calls
a frame blank when `mean < 10 && var < 64`. On that frame the lattice reports
`mean=0, var=0` — **1024 samples, not one of which lands on a glyph**, though
1.35% of the frame is at luma 255.

That is the shape of every boot screen: sparse bright text on black. Measured
over this set, every TEXT frame has mean luma <= 2, and three of the four clear
the threshold only on variance. The classifier is not detecting "no signal", it
is detecting "mostly black" — and mostly-black is what firmware, bootloaders and
consoles look like, which is the screen type panio exists to read.

Sampling density is the whole story:

| lattice | samples | max luma seen | samples on text |
| --- | --- | --- | --- |
| 32x32 (current) | 1024 | 20 | 0 |
| 64x64 | 4096 | 255 | 42 |

Separately, a stale frame can retain `signal: stable` indefinitely: on a capture
error `capture_thread` does `continue` without publishing, so the last
`FrameState` stays in the watch channel with its old label. Two independent
sightings, one across a mode change and one where a power-cut machine kept
reporting `stable` on its pre-cut desktop.

**So: save every frame, de-duplicate by content hash afterwards, and treat
`signal` as a hint.** The best frame in the second batch would have been thrown
away by a collector that trusted it.

## Ground truth

Not yet written. Text frames get `<stem>.gt.txt` (literal expected text, reading
order); busy GUI frames get `<stem>.gt.json` with `required_tokens` — the
strings panio actually needs to find — scored by recall rather than CER.
