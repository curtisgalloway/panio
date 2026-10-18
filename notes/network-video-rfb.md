# Network video via RFB (milestone 2)

Status: built and hardware-verified on one JetKVM (2026-10-07). Decision
record; the user guide is [docs/video.md](../docs/video.md#network-sources-rfb).
[vm-targets-and-rfb.md](vm-targets-and-rfb.md) predicted the client half of
this; that half now exists.

## Terms

- **RFB**: Remote Framebuffer, the protocol VNC uses. A server sends rectangles
  of pixels; a client sends key and pointer events.
- **VNC**: the remote-desktop system built on RFB.
- **noVNC**: a VNC client that runs in a browser (JavaScript, RFB over
  WebSocket).
- **H.264**: a lossy video codec. A JetKVM streams its screen as H.264.
- **Ingest / egress**: ingest is video entering `hdmicap`; egress is video
  leaving it toward a person or agent.

The full list is in [docs/glossary.md](../docs/glossary.md).

## The split that frames everything

Ingest and egress are different problems.

- **Egress to agents is stills.** An agent never consumes a stream. It asks for
  a PNG (`video shot`) or text (`video read`), and `hdmicap` already produces
  both from raw decoded frames. Nothing about network sources changes that.
- **Egress to people** (the dashboard's live view) is a separate question,
  deferred below.
- **Ingest** is the question this milestone answered: how does a frame from a
  device that is not a UVC capture stick get into `hdmicap` as raw pixels?

## Ingest options weighed

1. **MJPEG bridge** (decode the device's video, re-encode as JPEG, feed it to
   `hdmicap` like a capture stick). Rejected. It is lossy-on-lossy: H.264
   artifacts are re-quantized by JPEG, which hurts exactly the thin console
   fonts OCR depends on.
2. **H.264 as the ingest format** (teach `hdmicap` to take an H.264 stream).
   Rejected. It fits only the JetKVM. Capture sticks emit MJPEG or YUV, and
   AMT, VMs and BMCs emit RFB. It would also spread lossy change-detection
   noise to every source that could otherwise be lossless.
3. **RFB as the common network-source format.** Chosen. One RFB client in
   `hdmicap` (`hdmicap/src/rfb.rs`) covers AMT KVM, VMs, BMCs and PiKVM
   directly, with no per-device code. The JetKVM is decoded once, inside its
   own helper, and presented as an RFB server.

## Choices inside the RFB option

**Transport for our own daemons: WebSocket on the token-authenticated HTTP
port.** The `jetkvm` daemon serves RFB at `GET /rfb`, behind the same bearer
token and loopback Host/Origin checks as every other daemon route. Weighed
against:

- a unix socket (no Windows story, and a second discovery mechanism);
- loopback TCP with a VNC password (RFB's password scheme is an 8-character
  DES challenge, weak, and it would be a second secret to carry).

The WebSocket route reuses the discovery file, the token, and noVNC
compatibility (the `binary` subprotocol is accepted).

**Device forms.** `rfb://HOST:PORT` for plain TCP (security None only),
`rfb+ws://127.0.0.1:PORT/rfb` for a daemon (token in env `HDMICAP_RFB_TOKEN`,
never argv), and `rfb+discovery:`, which reads the daemon's discovery file
(named by env `HDMICAP_RFB_DISCOVERY`) on every reconnect, so a restarted
daemon's new port and token are picked up. The lab file spells the last one
`rfb+hid:`: "the RFB feed of this same target's hid daemon". The CLI starts the
hid daemon first, then `hdmicap`. No device-specific code lives in `cli/`; the
CLI knows only that a `hid` channel can host a feed.

**Decoder: an `ffmpeg` subprocess**, not openh264 or cgo. A subprocess keeps
the Go helper cgo-free (cross-compiles, no codec license questions in our
binaries) and ffmpeg's decoder is the reference one. The cost is a runtime
requirement, and only for JetKVM video: HID works without it.

**One WebRTC session.** The JetKVM firmware allows one session, so video rides
the session the HID daemon already holds (a receive-only H.264 track added to
the same offer). Decoding runs only while an `/rfb` client is attached.

**RFB subset.** RFB 3.3/3.7/3.8; Raw, CopyRect and DesktopSize. Keepalive:
`hdmicap` sends a non-incremental update request every 5 s, and treats the link
as dead after 15 s of silence (publishing `no_device`, then reopening). The
JetKVM side sends whole frames as Raw rectangles and ignores input messages;
input goes through the `hid` path.

## Known limitations

- **Frame hashes were noisy on a lossy source; a change threshold fixed it.**
  On an unchanged JetKVM screen the exact digest changed in 3 of 4 shots about
  1 s apart. Diffing saved frames separated the two suspected causes: two
  frames of a screen with nothing happening differed in 316 pixels, by at most
  8 brightness levels and none above 16, which is H.264 decode noise; real
  changes (typed text, a clock tick) differed by up to 255 levels in over a
  thousand pixels. hdmicap now keeps a reference frame and gives a new frame a
  new hash only when some pixel's luma differs from the reference by more than
  a per-channel threshold (default 16; 0 = exact). Measured after the change:
  the hash moved only with the text cursor's blink, a 10x18-pixel box at up to
  250 levels, plus H.264 artifacts beside it at up to 45. A cursor blink is a
  real change and moves the hash on a USB capture too; ignoring it would need
  a minimum changed area, which was considered and not adopted, since a single
  small glyph is about the same size.
- **No VNC password authentication yet** (security None only). AMT or a VM
  that requires a password does not work yet.
- **Whole-frame Raw updates** from the JetKVM (about 8 MB at 1080p); no dirty
  rectangles.
- **No keyframe request after packet loss** (RTCP PLI is sent at start, after
  reconnect, and every 3 s until the first frame, not on loss).
- **One session**: a browser on the JetKVM's own web UI evicts the daemon, and
  vice versa.

## Hardware verification (2026-10-07)

JetKVM firmware 0.5.9 in front of an x86 board, macOS control host:

- `panio video watch` started both daemons.
- `video shot` returned a 1920x1080 PNG of the real screen.
- `hid send` typed into an application while video ran on the same session (no
  eviction).
- `video read --stable` OCR'd the screen in about 0.3 s warm. The first OCR
  call took about 15 s while Apple Vision loaded its model, a one-time macOS
  cold start that is not specific to RFB.

Not verified: a Windows control host (CI covers build and test), a Linux
control host, `rfb://` against AMT or a VM, and recovery from packet loss.

## Egress (live view): built

**noVNC** is the browser client, and it is built (2026-10). Decisions:

- **All targets, served by hdmicap.** hdmicap serves its warm frame as RFB at
  `GET /rfb` (WebSocket, token auth, subprotocol `binary`) for every video
  source, USB capture included, not only the JetKVM's own `/rfb`. One viewer
  path, one place for tokens.
- **Input is server-side and native.** noVNC key and pointer events are mapped
  to the hid vocabulary in hdmicap (`keysym.rs`) and sent to the target's hid
  daemon over its `/hid` WebSocket (`hid_link.rs`, found through
  `HDMICAP_HID_DISCOVERY`). No hid channel means a watch-only view.
- **It replaces the MJPEG pane**, which stays behind a video VNC | MJPEG toggle
  and as the automatic fallback when `/rfb` cannot connect. The Capture-input
  overlay exists only in MJPEG mode.
- **Lossless ZRLE** (one zlib stream per connection) over changed 64x64 tiles,
  with Raw as the fallback. No lossy encoding, so what the viewer sees is what
  OCR and `shot` see.

Measured: a synthetic mostly-text 1920x1080 frame is 81,417 bytes as ZRLE against
8,294,400 bytes Raw, about 100 times smaller. Hardware-verified on a JetKVM
target from a macOS control host in Chrome: connect, scaled render, click to
position the remote cursor, typing including capitals and a shifted `!`, the
MJPEG toggle both ways, and reconnect on reload. Not verified: a USB-capture
target, a remote control host through the `console` tunnel, Windows or Linux
browsers, and the scroll wheel on hardware.

**H.264 egress** (stream the encoded video to a browser) is kept as a later
option, with this reference cost: Raspberry Pi's published tests put
low-latency software H.264 at 1080p30 at 60 to 90 percent of one core (15 to
22.5 percent of the four-core CPU) on a Pi 5, which has no hardware video
encoder (source: Raspberry Pi's published documentation). The cost is paid per
target while a viewer is watching, and is about four times that at 4K. That is
affordable for an occasional human viewer and wrong for an always-on path,
which is why agents get stills.
