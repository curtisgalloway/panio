<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# hdmicap/ — agent notes

This file holds the internals of this component, moved out of the root AGENTS.md to keep it short. The rules in the root AGENTS.md still apply; per-OS behavior lives in `docs/dev/platform-support.md`.

## Module layout

```
hdmicap/         Rust crate: warm-stream HDMI capture daemon
  build.rs       compiles src/capture_avf.m via cc on macOS, links AVFoundation
  src/
    main.rs      CLI subcommands: daemon, devices, shot, watch, preview, stop
    capture.rs   capture backends: v4l (Linux, raw MJPEG tee + turbojpeg);
                 macOS module wraps the C ABI of the ObjC layer below
    capture_avf.m  our ObjC AVFoundation layer (macOS): enumeration, open at
                 native resolution with NV12 delivery, blocking frame wait.
                 Never sets frame durations — MS2109-class HDMI sticks throw
                 NSException on those setters
    capture_thread.rs  std::thread owning device, publishes into watch channel.
                 A watchdog thread flags (not kills) a stall — no new frame for
                 12s after open, or 4s of no progress thereafter — and the main
                 loop reopens the backend in place the next time backend.frame()
                 returns, publishing no_device while it does; StallTracker exits
                 the process only after 8 consecutive stalls with no healthy
                 frame in between. If the flag is still set a full watchdog poll
                 later (backend.frame() itself never returned to let the main
                 thread see it), that IS the wedged case and it still exits
    rfb_serve.rs RFB server at `GET /rfb` for every video source: WebSocket
                 (subprotocol `binary`, token via header or `?token=`), RFB
                 3.8/3.7/3.3, security None, at most MAX_CLIENTS (8) sessions.
                 Sends only changed 64x64 tiles; ZRLE, Raw fallback,
                 DesktopSize; honors 32bpp true-color SetPixelFormat. Key and
                 pointer events go to hid_link.rs; held keys/buttons are
                 released on disconnect. No plain TCP listener. Tests in
                 rfb_serve/tests.rs
    zrle.rs      ZRLE encoder (one zlib stream per connection, lossless)
    keysym.rs    X11 keysym -> hid key name: printable ASCII, editing and
                 navigation, F1-F12, modifiers (Meta -> Alt, AltGr -> RIGHT_ALT),
                 keypad -> main-block keys; F13-F24 and unmapped keysyms are
                 dropped (the hid vocabulary has no F13+)
    hid_link.rs  client of the target's hid daemon `/hid` WebSocket, found via
                 the discovery file named by env `HDMICAP_HID_DISCOVERY` (set
                 by the CLI whenever the target has a local hid channel,
                 whatever the video device); reconnects, re-reading port and
                 token; absent = the noVNC view is watch-only
    rfb.rs       platform-independent RFB client capture backend for network
                 video sources (device strings `rfb://HOST:PORT` plain TCP,
                 security None only; `rfb+ws://127.0.0.1:PORT/rfb` with the
                 bearer token in env `HDMICAP_RFB_TOKEN`, never argv;
                 `rfb+discovery:`, which re-reads the discovery file named by
                 env `HDMICAP_RFB_DISCOVERY` on every reconnect so a restarted
                 daemon's port and token are picked up). RFB 3.3/3.7/3.8; Raw,
                 CopyRect, DesktopSize. Keepalive: a non-incremental update
                 request every 5 s; no bytes for 15 s = link dead, publish
                 no_device and reopen. Deps: tokio-tungstenite (no TLS),
                 futures-util. No VNC password auth yet. Design:
                 notes/network-video-rfb.md
    frame.rs     FrameState, Signal enum, one-pass strided classification
                 (exact frame digest + no-signal from 4k luma samples)
                 + Settler: the published hash only moves when some pixel's
                 luma differs from the last accepted frame by more than
                 --change-threshold (default 16, 0 = exact); compares run
                 only when the exact digest changes
    pixel.rs     PixelData (Rgb/Nv12/Empty) + NV12/YUYV -> RGB converters;
                 compact_nv12() re-packs a strided (row-padded) NV12 buffer
                 into tight Y/CbCr planes — needed by the Windows backend,
                 whose samples may pad rows past the visible width
    server.rs    axum HTTP API: GET / (dashboard; CSP frame-ancestors 'none'),
                 /status (adds `rfb_clients`, `rfb_input`), /snapshot, /preview,
                 /ocr, /devices, POST /power-cycle, GET /rfb (the RFB server,
                 below), GET /novnc.js, and the /xterm.* and /novnc.js static
                 assets (the only token-exempt routes; `PUBLIC_ASSETS`).
                 /snapshot matches the inner Result of `rx.changed()`, not just
                 the outer timeout — otherwise a dropped capture-thread Sender
                 makes it spin at 100% CPU until the deadline instead of
                 answering 503 "capture thread gone" right away. Readiness is
                 snapshot_ready(): wait=stable and changed_since given together
                 require BOTH (stable AND a differing hash) — not stability
                 alone, which used to hand back the very frame the caller said
                 they already had. PNG
                 encode/decode (incl. Linux turbojpeg) and the preview JPEG
                 fallback run on spawn_blocking, not inline on a tokio worker;
                 PNG encoding, the OCR subprocess, and /preview's fallback
                 encode all share AppState.expensive (a 2-permit Semaphore) so
                 repeated dashboard clicks (or a handful of open preview tabs)
                 queue rather than piling up CPU work or visionocr children.
                 /preview also coalesces: AppState.preview_cache (a std Mutex,
                 never held across an await) keeps the last fallback-encoded
                 JPEG keyed by the frame's captured_at, so every client
                 watching one frame costs one encode, not N. And /preview
                 checks FrameState::effective_signal(), not just
                 signal == NoDevice — once the warm frame goes Stale (or
                 NoSignal/NoDevice), the stream stops sending that frame's
                 bytes and instead sends a placeholder JPEG (dark field, red
                 X) once per transition, with every part carrying an
                 X-Signal header naming the effective signal. A live frame
                 whose fallback encode FAILS (malformed RGB buffer) is recorded
                 as Served::Failed(captured_at) and skipped (should_attempt_live)
                 until a new frame arrives — attempted once + one warn!, not
                 re-encoded every 67 ms tick draining the semaphore. Subprocess
                 hooks all set kill_on_drop(true) and run under wait_with_timeout
                 — 504 on timeout, process killed: the OCR child under
                 OCR_TIMEOUT (30 s), and every power hook (/power on/off/cycle
                 via run_paniolo_action, /power-state) under POWER_TIMEOUT (60 s,
                 generous for a real power-cycle). /devices runs the synchronous
                 capture::enumerate() on spawn_blocking, off the tokio worker
    auth.rs      token + loopback Host/Origin layer over the whole router
                 (byte-identical in serialcap/hidrig/ch9329)
    daemon.rs    advisory lock, discovery file (pid, port, token; owner-only),
                 tokio runtime, graceful shutdown. Shutdown removes daemon.json
                 only — NOT the lock file, which stays open (flock'd) until
                 this process exits; unlinking it while still held would let a
                 next daemon lock a fresh inode at the same path while this
                 process (and its lock on the old inode) is still alive
  assets/        index.html (combined dashboard) + vendored xterm.js/css/fit addon
                 + novnc.js (noVNC v1.7.0 as one esbuild ESM bundle, MPL-2.0 plus
                 pako MIT, notices in novnc-LICENSE.txt; rebuild reproducibly with
                 scripts/vendor-novnc.sh, pinned sha512 and esbuild)
```

## Combined dashboard (video + serial)

hdmicap's `GET /` serves a two-pane page: a noVNC view of `GET /rfb` on top (a
**video VNC | MJPEG** toggle, stored in `localStorage` as `paniolo-video-mode`; MJPEG
also the automatic fallback when `/rfb` cannot connect; the Capture-input overlay
exists only in MJPEG mode), an xterm.js terminal below. The terminal opens a WebSocket to **serialcap** (a separate
daemon/port), so the two subsystems stay decoupled — hdmicap only references
serialcap by URL. Defaults to `ws://<host>:8724/stream`; override with
`?serialws=<url>` (or `?serial=<port>`, which cannot carry a token). `paniolo
console` passes `?serialws=` — serialcap's `/stream` with serialcap's own
`?token=` inside, percent-encoded — on both the local and the remote/tunnel
path, plus hdmicap's token as `?token=`; the page refuses a non-loopback URL.
serialcap sends serial bytes as binary
frames and accepts keystrokes back over the same socket. xterm.js and noVNC are vendored
(not CDN) so the dashboard works on an isolated lab network. This is the first
concrete instance of the "Option B" inter-subsystem coordination described above.

**Multi-pane serial:** the page fetches `GET /interfaces` from serialcap on
load and calls `buildPanes(names)`. With one interface a single terminal fills
the panel and connection status appears in the top bar. With multiple interfaces
each gets its own `.serial-pane` div (label + status bar + xterm.js terminal),
laid out side by side in bottom mode or stacked in right-panel mode. All fits
are tracked in `allFits[]` so resize and layout-toggle events re-fit every
terminal. `?interface=<name>` bypasses the fetch and opens single-pane mode
pinned to that interface.

**Layout toggle:** a button in the status bar switches the serial panel between
bottom (default, 40 vh) and right-panel (380 px fixed, video fills remaining
width) layouts. The choice is persisted in `localStorage` under the key
`paniolo-serial-layout`.

**Power controls:** an on/off **toggle switch** (`Power [switch] ON/OFF`,
reflecting live state) plus a separate **⟳ Cycle** button appear in the video
overlay, each gated by a confirmation modal. Availability + state come from
**`GET /power`** — non-acting: it runs `paniolo power-state <target>` and returns
`on`/`off`/`unknown` from the output's *first line* (`power_state_verdict`; with
named power levels that is the default level's line), and the dashboard polls it
every 5 s to keep the toggle synced. The controls drive the default power level
only. The actions are **`POST /power-on` | `/power-off` | `/power-cycle`** →
`paniolo power on|off` / `power-cycle <target>`. All use the `PANIOLO_TARGET` env
var set when the daemon starts via `paniolo video watch <target>`; the controls
are hidden (501) if no target was passed, so shared dashboards are safe.
(Previously the availability probe was `POST /power-cycle`, which *triggered* a
cycle on every page load — the probe is now the read-only `GET /power`.)

## Gotchas

- **hdmicap device identity.** Capture devices have a stable, port-derived id
  (AVFoundation `uniqueID` on macOS, `/dev/v4l/by-path` symlink on Linux) shown
  by `hdmicap devices` / `paniolo video devices`. Prefer the id in lab files —
  identical dongles (MS2109s ship without USB serials) are indistinguishable by
  name. A name substring matching more than one device is a hard error listing
  the candidates' ids; with several non-built-in captures (e.g. MS2109 + Razer
  Kiyo), `paniolo configure` lists the id alternatives as comments.
- **macOS capture is our own AVFoundation layer** (`hdmicap/src/capture_avf.m`,
  C ABI consumed by `capture.rs`), replacing nokhwa + a vendored bindings fork.
  Two hard-won rules live in it: (1) never set
  `activeVideoMin/MaxFrameDuration` — MS2109-class HDMI sticks throw
  NSException from those KVC paths (the bug the old vendor patch existed for);
  (2) `activeFormat` alone is ignored — the session's default preset scales
  output to 1080p-class, so native resolution requires explicit
  `kCVPixelBufferWidth/HeightKey` in the output's `videoSettings`
  (`AVCaptureSessionPresetInputPriority` is iOS-only). Note the macOS UVC
  stack decodes MJPEG before AVFoundation — raw-MJPEG passthrough (the Linux
  tee) is impossible on macOS; frames arrive as NV12 ('420v', video-range)
  and RGB materializes lazily per request.
