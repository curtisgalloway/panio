<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# hidrig/ — agent notes

This file holds the internals of hidrig/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
hidrig/          USB HID injector: host CLI + daemon (Rust). The dual-board KB2040
                 firmware it drives is a separate custom-hardware design, now in the
                 paniolo-hardware repo (hidrig-kb2040/), not in this tree
  src/main.rs      `hidrig` CLI — one-shot subcommands of the HID command
                   vocabulary (type/key/.../moveabs/ping/version) + `run` command
                   files; `serve`/`stop` for the daemon. A `Sender` routes each
                   one-shot through a running daemon (POST /send) when one owns
                   the same device, else opens the control CDC link and composes
                   frames in-process
  src/compose.rs   HID composition: turns each command into HID report bytes and
                   wraps them in the binary frames the boards relay (F_HID 0x01 /
                   F_CTRL 0x02). Holds the held-key + virtual-cursor state so
                   relative `move` and `moveabs` share one absolute-pointer device
  src/proto.rs     control-link transport for the *direct one-shot* path: writes
                   a 258-byte all-zero resync preamble then binary frames to the
                   control board's data CDC endpoint (no baud negotiation — CDC;
                   nominal 115200); `read_control_reply` demultiplexes inbound
                   bytes with uart.rs's `split_frames` rather than assuming the
                   next bytes are the `0x02` reply — the board interleaves `0x03`
                   console frames on the same stream whenever the DUT UART has
                   bytes, so e.g. `power cycle` during a boot log still gets
                   acked; command-file sequence parser (delay/sleep bounded to
                   0..=3600s, `type` keeps trailing whitespace) + clamp_abs
  src/uart.rs      the control-link owner (daemon path): a dedicated *blocking-
                   serialport* thread (NOT tokio-serial — its async reads don't
                   get read-readiness on a macOS tty) running a full-duplex poll
                   loop. It drains an mpsc command queue (Line + a shutdown
                   Release variant, CLI + web serialized onto one wire), pumps
                   PTY console input down as 0x03, then reads + demuxes inbound
                   frames: 0x02 replies fulfil the in-flight control request
                   (deadline-tracked), 0x03 payloads go to the console PTY
                   master. HID is fire-and-forget; a request whose client already
                   gave up (its reply channel is closed) is dropped without
                   composing or writing, so a timed-out command is not injected
                   late; broadcast transcript; lazy open (via proto::open_port, so
                   it also writes the resync preamble) + reopen-on-transport-error;
                   `split_frames` is `pub(crate)`, shared with proto.rs's one-shot
                   reader
  src/pty.rs       allocates a PTY (libc posix_openpt) for the DUT serial-console
                   bridge and opens its own raw-mode handle on the slave, held
                   for the daemon's lifetime — otherwise the slave sits at
                   cooked tty defaults (ECHO|ICANON) until some other reader
                   attaches, and every byte written to the master in that window
                   echoes back and gets framed to the DUT UART as if typed;
                   paniolo's serial channel opens a *second*, separate handle on
                   the slave via the stable symlink the daemon publishes
  src/server.rs    axum: GET /hid + POST /send (WebSocket message / body sized to
                   one full `type` line — MAX_TYPE_CHARS + the `type ` verb — so
                   the documented 4096-char `type` cap is actually reachable),
                   POST /stop (authenticated shutdown; `hidrig stop` never signals
                   the discovery-file PID), /status, /version. WS clients send
                   command lines; all results are broadcast as `evt ok|err …`
                   frames so observers see the intermixed stream
  src/auth.rs      token + loopback Host/Origin layer over the whole router
                   (byte-identical in serialcap/hdmicap/ch9329)
  src/daemon.rs    advisory lock, discovery file at /tmp/paniolo-<uid>/hid/<target>/
                   (the channel name, not "hidrig", so paniolo finds it without
                   knowing the helper); brings up the console PTY (removing a
                   stale `console` symlink from a previous daemon first) and
                   publishes both the stable symlink (discovery `console`, a
                   convenience — falls back to the raw device path if the
                   symlink can't be made) and the real slave path (discovery
                   `console_device`, always present, the source of truth); tokio
                   runtime, graceful shutdown — on SIGTERM/SIGINT or a `POST
                   /stop`-woken `Notify` alike — releases held keys/buttons via
                   uart.rs's Release request, then removes daemon.json and the
                   console symlink. Shutdown removes daemon.json only — NOT
                   the lock file, which stays open (flock'd) until this
                   process exits; see the identical note on hdmicap's
                   daemon.rs above
  (no firmware/ here — the dual-board KB2040 CircuitPython firmware, and the
                   retired single-board "smart" firmware, moved to the paniolo-hardware
                   repo: https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware
                   Summary, for readers of this crate: the **control** board
                   (`firmware/control/`) is a USB-CDC <-> I2C1 controller; it reads
                   framed input from usb_cdc.data, relays 0x01 HID frames verbatim
                   over I2C1 to the target, answers 0x02 control frames
                   (ping/version/power -> dual-control/1; power drives a DUT relay on
                   D5) locally, and bridges 0x03 console frames to/from the DUT UART
                   (TX=GP0/RX=GP1). The **target** board (`firmware/target/`) is an
                   I2C1 peripheral that relays report bytes to usb_hid send_report —
                   no adafruit_hid, no parsing; its boot.py holds the HID descriptor
                   (keyboard + custom absolute-pointer, 0..32767 axes) and the
                   dev/HID-only NVM flag (BOOT button GP11 toggles; D2->GND at reset
                   forces dev). Both discard a partial frame after 50ms of no
                   completion (`_rxbuf_started`/`discard_stale_rxbuf`) so a
                   killed-and-restarted daemon's resync preamble has something to
                   resync — UNVERIFIED on hardware. Any change to the HID descriptor
                   or frame format there requires a matching change to this crate's
                   `src/compose.rs` — see docs/dev/hid-dual-board-design.md.
  host/hid_seize_reports.c  macOS IOKit tool: seizes the HID device exclusively
                   and prints raw input reports — for pipeline testing without
                   keystrokes reaching the focused app. Build with host/Makefile.
  README.md        topology, wiring, frame protocol, CLI usage. The command
                   vocabulary spec is docs/dev/hid-serial-protocol.md; the dual-board
                   design + frame format is docs/dev/hid-dual-board-design.md

```
## hid daemon + KVM (`hidrig serve`)

The control link can have only one owner, so KVM streaming and CLI injection
can't both open it. `hidrig serve` resolves this: it owns the link and
re-exposes the command vocabulary over a WebSocket (`GET /hid`) and `POST
/send`. Every command — from a
browser, from `paniolo hid send`, from another script — flows through one
`mpsc` queue in `uart.rs`, one in flight, request/reply; that single queue is
what makes events intermix correctly. `paniolo console` starts the daemon when
the target has a `hid` channel (local: `?hid=PORT`; remote: an SSH-tunnelled
`?hidws=` URL). In the default noVNC view the browser's key and pointer events reach
this daemon through hdmicap (`/rfb` -> hid_link.rs), not from the page; `video watch`,
`console` and `daemons restart` start the local hid daemon for every video device (only
`rfb+hid:` errors if it will not start), so on a USB-capture target with a hid channel
starting video also makes the hid daemon own the injector. In MJPEG mode the
**`⌨ Capture input`** overlay button toggles capture (no click-to-grab, no
host-key release): engaged, the page streams `down`/`up`/`moveabs`/`scroll` to
the daemon; click the button again to release. The mouse is absolute (the
firmware's custom HID descriptor), so the cursor follows where you point in the
video, and the local cursor stays **visible as a crosshair** (no Pointer Lock —
deliberately, so you never lose your pointer). Mouse listeners live on the
`<img>`, so the overlay buttons never inject; window blur releases. paniolo
discovers the daemon by the
channel name `hid` (`daemons::daemon_port("hid")`), staying agnostic to the
helper. Hardware-verified end-to-end on the pi5 Linux desktop (2026-06-04).

**Latency.** HID frames are **fire-and-forget** over the USB-CDC link (no
per-frame round-trip), so streaming stays responsive without a baud
negotiation — the control board is USB-CDC and USB sets the rate. The dashboard
also **coalesces mouse moves** to one `moveabs` per `requestAnimationFrame`
(newest position only). The remaining floor is the target board's USB interrupt
`bInterval` (~8 ms per report on the CircuitPython firmware). Only `0x02`
control frames (ping/version) draw a reply; macOS drops the `IOSSDATALAT` read
timer on open to keep those round trips prompt.

## hidrig (USB HID injector)

The `hidrig/` directory holds a Rust host CLI/daemon for a DIY USB HID injector: the
**dual-board "dumb pipe"** KB2040 rig. The CircuitPython 9.x firmware it drives is a
custom-hardware design that lives in a separate repo, paniolo-hardware
(https://github.com/curtisgalloway/paniolo-hardware, `hidrig-kb2040/firmware/`) —
`hidrig/` here is the host side only. This is only one of many HID injection devices
paniolo can use.

### Architecture

```
[control host]
  |-- USB-CDC (hidrig writes binary HID frames) --> [Control KB2040]
                                                      |-- I2C1 (GP10 SDA / GP19 SCL,
                                                      |   addr 0x41, 4.7k pull-ups) -->
                                                    [Target KB2040]
                                                      |-- USB HID --> [target / DUT]
```

The host composes HID reports (`src/compose.rs`) and writes binary frames to the
**control** board's data CDC endpoint; the control board relays `0x01` HID
frames verbatim over I2C1 to the **target** board, which calls `send_report` —
neither board parses HID semantics (the "dumb pipe", `docs/dev/hid-dual-board-design.md`).
The target board's USB faces the DUT as a device-mode HID keyboard + absolute
mouse (and is DUT-powered, so it reboots with the DUT); the control board is
independently host-powered. The command vocabulary (`type`/`key`/`moveabs`/…)
is the device-independent **HID serial protocol v1** (`docs/dev/hid-serial-protocol.md`),
but it is the *external* interface only — `hidrig` consumes it and composes; the
line protocol never reaches a wire. `hidrig` (`src/main.rs`, `src/compose.rs`,
`src/proto.rs`) is the host client; the reference firmware
(`hidrig-kb2040/firmware/control/`, `hidrig-kb2040/firmware/target/` in
paniolo-hardware) is the board side. The retired single-board "smart" firmware
(`hidrig-kb2040/firmware/single-board/{boot,code,config}.py` in paniolo-hardware,
line protocol + `adafruit_hid`) is kept there for a future dumb single-board on the
same composition.


### USB identity (paniolo-hardware `hidrig-kb2040/firmware/target/boot.py`)

In normal (HID-only) operation the DUT must see a plain keyboard + mouse, so
the target board's boot.py disables the CIRCUITPY drive, the CDC REPL, and
MIDI. The mode lives in NVM byte 0 — `0` = HID-only, anything else (including
erased `0xFF`, so a fresh board boots editable) = dev mode with CIRCUITPY +
REPL + HID. **Tap the BOOT button (GP11)** while running and code.py flips the
flag and resets, toggling dev ↔ HID-only with no jumper; grounding **D2** at
reset forces dev mode regardless of the flag, as a hardware fallback for a
wedged code.py. Do firmware edits on a dev machine, not the DUT. boot.py only
re-runs on hard reset. The status NeoPixel (core `neopixel_write`, no /lib
dependency): blue = up, waiting for the controller over I2C; green blip = a
frame arrived; red blip = `send_report` failed (DUT not enumerated yet). The
retired single-board `boot.py` (paniolo-hardware `hidrig-kb2040/firmware/single-board/`)
used the D2 jumper alone and a different colour code (blinking red = waiting for
enumeration, solid red = last command failed).

### paniolo integration

`paniolo hid set -t <target> --cmd "hidrig -d <uart>"` stores an opaque
command prefix in the lab file's `[targets.<name>.hid]` channel (mirroring the
generic power hooks; no device-specific code in `cli/`). `paniolo hid send -t
<target> <args...>` shell-quotes and appends the args and runs the result via
`sh -c` on the channel's host (transparent SSH dispatch via
`ChannelKind::Hid`). `paniolo doctor` probes absolute-path helpers with
`test -e`.

### Host testing tool (`hidrig/host/`)

`hid_seize_reports.c` is a macOS IOKit utility that opens the injector's HID
interface with `kIOHIDOptionsTypeSeizeDevice`, preventing any keystroke from
reaching the focused application. It registers an input report callback and
prints hex dumps of every keyboard and mouse report. Use it to verify the full
pipeline end-to-end without a target:

```bash
cd hidrig/host && make
sudo ./hid_seize_reports   # grant Input Monitoring in System Settings first
```

Run `hidrig -d <adapter> type/key/move/click/scroll ...` in a second terminal
and read the reports. The tool prints the 156-byte report descriptor on first
device match, so you can verify the HID descriptor matches expectations.

VID/PID are 0x239A/0x8106 (KB2040 running CircuitPython). The built binary is
gitignored; re-run `make` after cloning.

### Negative number arguments (`move`, `scroll`)

clap treats a token starting with `-` as a potential option flag; the `dx`/
`dy`/`amount` args use `allow_hyphen_values` so `hidrig move 50 -30` and
`hidrig scroll -3` work without a `--` separator (same for `paniolo hid send`,
whose trailing args allow hyphen values — keep `-t` before them).

