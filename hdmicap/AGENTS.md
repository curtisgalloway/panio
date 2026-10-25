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
    capture.rs   backends: v4l (Linux, raw MJPEG tee + turbojpeg); macOS wraps
                 the C ABI of capture_avf.m
    capture_avf.m  our AVFoundation layer (macOS): enumeration, native-resolution
                 NV12 open, blocking frame wait. Never sets frame durations:
                 MS2109-class sticks throw NSException on those setters
    capture_thread.rs  std::thread owning the device, publishes to a watch
                 channel. A watchdog flags (does not kill) a stall: no frame 12s
                 after open, or 4s of no progress after. The main loop reopens
                 the backend in place when backend.frame() next returns,
                 publishing no_device meanwhile; StallTracker exits after 8
                 consecutive stalls with no healthy frame. A flag still set one
                 watchdog poll later (backend.frame() never returned) is the
                 wedged case and exits
    rfb_serve.rs RFB server at `GET /rfb` for every video source: WebSocket
                 (subprotocol `binary`, token via header or `?token=`), RFB
                 3.8/3.7/3.3, security None, at most MAX_CLIENTS (8). Sends
                 changed 64x64 tiles only; ZRLE with Raw fallback, DesktopSize,
                 32bpp true-color SetPixelFormat. Input goes to hid_link.rs;
                 held keys/buttons release on disconnect. No plain TCP
                 listener. Tests in rfb_serve/tests.rs
    zrle.rs      ZRLE encoder (one zlib stream per connection)
    keysym.rs    X11 keysym -> hid key name; Meta -> Alt, AltGr -> RIGHT_ALT,
                 keypad -> main block; F13-F24 and unmapped keysyms dropped (hid
                 vocabulary has no F13+)
    hid_link.rs  client of the target's hid daemon `/hid` WebSocket, located via
                 the discovery file in env `HDMICAP_HID_DISCOVERY` (set by the CLI
                 whenever the target has a local hid channel); reconnects,
                 re-reading port and token; absent = noVNC view is watch-only
    rfb.rs       RFB client capture backend for network sources. Device strings:
                 `rfb://HOST:PORT` (plain TCP, security None only);
                 `rfb+ws://127.0.0.1:PORT/rfb` (bearer token in env
                 `HDMICAP_RFB_TOKEN`, never argv); `rfb+discovery:` (re-reads the
                 file in env `HDMICAP_RFB_DISCOVERY` on every reconnect, so a
                 restarted daemon's port and token are picked up). RFB
                 3.3/3.7/3.8; Raw, CopyRect, DesktopSize. A non-incremental
                 update request every 5 s; no bytes for 15 s = link dead
                 (publish no_device, reopen). Deps: tokio-tungstenite (no TLS),
                 futures-util. No VNC password auth. Design:
                 notes/network-video-rfb.md
    frame.rs     FrameState, Signal, one-pass strided classification (exact
                 digest + no-signal from 4k luma samples), and Settler: the
                 published hash moves only when some pixel's luma differs from
                 the last accepted frame by more than --change-threshold
                 (default 16, 0 = exact); runs only when the digest changes
    pixel.rs     PixelData (Rgb/Nv12/Empty), NV12/YUYV -> RGB; compact_nv12()
                 strips row padding (Windows samples may pad rows)
    server.rs    axum: GET / (dashboard; CSP frame-ancestors 'none'), /status
                 (adds `rfb_clients`, `rfb_input`), /snapshot, /preview, /ocr,
                 /devices, POST /power-cycle, GET /rfb, GET /novnc.js and
                 /xterm.* assets (the only token-exempt routes; `PUBLIC_ASSETS`), POST /ticket and
                 /proxy/* (session.rs, proxy.rs).
                 /snapshot matches the inner Result of `rx.changed()`, not just
                 the outer timeout: a dropped capture-thread Sender otherwise
                 spins at 100% CPU until the deadline instead of 503 "capture
                 thread gone". snapshot_ready(): wait=stable with changed_since
                 requires BOTH stable AND a differing hash.
                 PNG encode/decode and the preview fallback encode run on
                 spawn_blocking, never on a tokio worker. PNG encoding, the OCR
                 subprocess and the fallback encode share AppState.expensive
                 (2-permit Semaphore) so repeated clicks queue.
                 /preview coalesces through AppState.preview_cache (std Mutex,
                 never held across an await; last JPEG keyed by captured_at),
                 and checks FrameState::effective_signal(), not just NoDevice:
                 once the frame is Stale/NoSignal/NoDevice it sends a
                 placeholder JPEG (dark field, red X) once per transition, every
                 part carrying an X-Signal header. A live frame whose fallback
                 encode fails is recorded Served::Failed(captured_at) and
                 skipped (should_attempt_live) until a new frame: one warn!, not
                 a re-encode every 67 ms tick.
                 Subprocess hooks set kill_on_drop(true) and run under
                 wait_with_timeout (504, process killed): OCR under OCR_TIMEOUT
                 (30 s); power hooks (/power on/off/cycle via
                 run_panio_action, /power-state) under POWER_TIMEOUT (60 s).
                 /devices runs capture::enumerate() on spawn_blocking
    auth.rs      token + loopback Host/Origin layer (byte-identical in
                 serialcap/hidrig/ch9329)
    session.rs   dashboard tickets: `POST /ticket` (bearer only) mints a
                 single-use ticket (120 s); `GET /?ticket=` consumes it, sets
                 an HttpOnly SameSite=Strict cookie `panio_hdmicap_<port>`
                 (port from Host: two daemons on 127.0.0.1 share one cookie
                 jar) and 303s to the URL without it. Middleware OUTSIDE
                 auth.rs: a good ticket/cookie becomes `Authorization: Bearer`
                 and auth.rs judges as usual, so auth.rs stays byte-identical.
                 Cookie counts only same-origin (Origin == http(s)://Host;
                 non-GET must send Origin): SameSite treats every loopback port
                 as one site. In-memory: a daemon restart ends every session
    proxy.rs     /proxy/serial/stream (WS, `interface` only passed on),
                 /proxy/serial/interfaces, /proxy/hid (WS) to serialcap and the
                 hid daemon. Discovery files from env HDMICAP_SERIAL_DISCOVERY
                 (always set by `video watch`) and HDMICAP_HID_DISCOVERY, re-read
                 per request; upstream connect first (502 if down), then
                 upgrade; text/binary only cross. `upstream_ws` is shared with
                 hid_link.rs
    daemon.rs    advisory lock, discovery file (pid, port, token; owner-only),
                 tokio runtime, graceful shutdown. Shutdown removes daemon.json
                 only, NOT the lock file, which stays flock'd until the process
                 exits: unlinking it while held would let a next daemon lock a
                 fresh inode at the same path while this one still holds the old
                 (same rule in serialcap, hidrig, ch9329)
  assets/        index.html (dashboard), vendored xterm.js/css/fit addon, novnc.js
                 (noVNC v1.7.0 as one esbuild ESM bundle, MPL-2.0 plus pako MIT,
                 notices in novnc-LICENSE.txt; rebuild with scripts/vendor-novnc.sh,
                 pinned sha512 and esbuild)
```

## Combined dashboard (video + serial)

`GET /` is a two-pane page: noVNC view of `GET /rfb` on top, xterm.js terminal below. A **video VNC | MJPEG** toggle is stored in `localStorage` as `panio-video-mode`; MJPEG is also the fallback when `/rfb` cannot connect, and the Capture-input overlay exists only in MJPEG mode. xterm.js and noVNC are vendored (no CDN) so it works on an isolated lab network.

The terminal reaches **serialcap** through hdmicap's proxy (`proxy.rs`) when the page has `?serial=proxy`, which is what `panio console` passes (with `?ticket=`, and `?hid=proxy` for KVM input), locally and through the one tunnel; the page then never holds another daemon's token. Hand-opened forms still work: `?serialws=<url>` (serialcap's `/stream`, its own `?token=` inside) or `?serial=<port>` (no token); default `ws://<host>:8724/stream`. The page refuses a non-loopback URL. Tests in `proxy.rs` run the real router against a fake upstream on real sockets: use `#[tokio::test(flavor = "multi_thread")]` there, since blocking `ureq` calls on the default single-thread test runtime starve the in-process server and hang.

**Multi-pane serial:** the page fetches serialcap's `GET /interfaces` and calls `buildPanes(names)`. One interface: a single terminal, status in the top bar. Several: one `.serial-pane` div each, side by side in bottom mode or stacked in right-panel mode; all fits are in `allFits[]` so resize and layout toggles re-fit every terminal. `?interface=<name>` skips the fetch and pins single-pane mode.

**Layout toggle:** a status-bar button switches the serial panel between bottom (default, 40 vh) and right-panel (380 px, video fills the rest); persisted in `localStorage` as `panio-serial-layout`.

**Power controls:** an on/off toggle plus a **⟳ Cycle** button in the video overlay, each behind a confirmation modal; they drive the default power level only. State comes from read-only **`GET /power`**: it runs `panio power-state <target>` and maps the output's first line (`power_state_verdict`) to `on`/`off`/`unknown`; the dashboard polls every 5 s. Actions are **`POST /power-on` | `/power-off` | `/power-cycle`**. All use `PANIO_TARGET`, set by `panio video watch <target>`; without it the controls are hidden (501). Never probe availability with a POST: `/power-cycle` would cycle power on every page load.

## Gotchas

- **Device identity.** Devices have a stable, port-derived id (AVFoundation `uniqueID` on macOS, `/dev/v4l/by-path` symlink on Linux), shown by `hdmicap devices` / `panio video devices`. Prefer the id in lab files: identical dongles (MS2109s have no USB serial) are indistinguishable by name. A name substring matching several devices is a hard error listing their ids; `panio configure` lists id alternatives as comments.
- **macOS capture is our own AVFoundation layer.** (1) Never set `activeVideoMin/MaxFrameDuration`: MS2109-class sticks throw NSException from those KVC paths. (2) `activeFormat` alone is ignored (the default preset scales to 1080p-class), so native resolution needs explicit `kCVPixelBufferWidth/HeightKey` in the output's `videoSettings` (`AVCaptureSessionPresetInputPriority` is iOS-only). macOS decodes MJPEG before AVFoundation, so raw-MJPEG passthrough is impossible there: frames arrive as NV12 ('420v', video-range) and RGB materializes lazily per request.
