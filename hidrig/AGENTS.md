<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# hidrig/ — agent notes

This file holds the internals of hidrig/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Overview

`hidrig/` is the host side (Rust CLI + daemon) of a DIY USB HID injector: the dual-board "dumb pipe" KB2040 rig. It is one of several HID injectors panio supports. The CircuitPython 9.x firmware lives in the paniolo-hardware repo (https://github.com/curtisgalloway/paniolo-hardware, `hidrig-kb2040/firmware/`), not here.

```
[control host]
  |-- USB-CDC (binary HID frames) --> [Control KB2040]
                                        |-- I2C1 (GP10 SDA / GP19 SCL,
                                        |   addr 0x41, 4.7k pull-ups) -->
                                      [Target KB2040]
                                        |-- USB HID --> [target / DUT]
```

- The host composes HID reports (`src/compose.rs`) and writes binary frames to the control board's data CDC endpoint. The control board relays `0x01` HID frames verbatim over I2C1; the target board calls `send_report`. Neither board parses HID semantics ([design](../docs/dev/hid-dual-board-design.md)).
- The target board faces the DUT as a HID keyboard + custom absolute pointer (axes 0..32767; its descriptor lives in `firmware/target/boot.py`) and is DUT-powered (reboots with it); the control board is host-powered.
- The vocabulary (`type`/`key`/`moveabs`/...) is the device-independent HID serial protocol v1 ([spec](../docs/dev/hid-serial-protocol.md)), an external interface only; it never reaches a wire.
- Any change to the firmware's HID descriptor or frame format needs a matching change in `src/compose.rs`.
- Control board firmware (`firmware/control/`): USB-CDC to I2C1 controller. It answers `0x02` control frames locally (ping/version/power; power drives a DUT relay on D5) and bridges `0x03` console frames to/from the DUT UART (TX=GP0/RX=GP1). Target board (`firmware/target/`): I2C1 peripheral relaying report bytes to `send_report`. Both discard a partial frame after 50 ms (`discard_stale_rxbuf`) so a restarted daemon's resync preamble works (unverified on hardware).
- Target board USB identity (`firmware/target/boot.py`): the DUT must see a plain keyboard + mouse, so in HID-only mode boot.py disables the CIRCUITPY drive, CDC REPL and MIDI. The mode is NVM byte 0: `0` = HID-only, anything else (including erased `0xFF`) = dev mode. Tap BOOT (GP11) to toggle and reset; grounding D2 at reset forces dev mode. boot.py re-runs only on hard reset. Do firmware edits on a dev machine, not the DUT. Status NeoPixel: blue = waiting for the controller over I2C, green blip = frame arrived, red blip = `send_report` failed (DUT not enumerated).

## Module layout

```
  src/main.rs      `hidrig` CLI: one-shot subcommands of the vocabulary
                   (type/key/.../moveabs/ping/version), `run` command files,
                   `serve`/`stop`. A `Sender` routes a one-shot through a
                   running daemon (POST /send) when one owns the same device,
                   else opens the control CDC link and composes in-process
  src/compose.rs   command -> HID report bytes, wrapped in the binary frames
                   the boards relay (F_HID 0x01 / F_CTRL 0x02). Holds held-key
                   and virtual-cursor state so relative `move` and `moveabs`
                   share one absolute-pointer device
  src/proto.rs     control-link transport for the direct one-shot path: writes
                   a 258-byte all-zero resync preamble, then frames to the CDC
                   endpoint (no baud negotiation; nominal 115200).
                   `read_control_reply` demuxes with uart.rs's `split_frames`,
                   not by assuming the next bytes are the `0x02` reply: the
                   board interleaves `0x03` console frames whenever the DUT
                   UART has bytes (`power cycle` during a boot log must still
                   be acked). Also the command-file sequence parser
                   (delay/sleep bounded to 0..=3600 s; `type` keeps trailing
                   whitespace) and clamp_abs
  src/uart.rs      control-link owner (daemon path): a dedicated blocking-
                   serialport thread, NOT tokio-serial (its async reads get no
                   read-readiness on a macOS tty), in a full-duplex poll loop.
                   It drains an mpsc queue (Line + a shutdown Release variant;
                   CLI and web serialize onto one wire), pumps PTY console
                   input down as `0x03`, then demuxes inbound frames: `0x02`
                   replies fulfil the in-flight request (deadline-tracked),
                   `0x03` payloads go to the console PTY master. HID is
                   fire-and-forget. A request whose client already gave up
                   (reply channel closed) is dropped without composing, so a
                   timed-out command is not injected late. Broadcasts a
                   transcript; opens lazily via proto::open_port (which writes
                   the preamble) and reopens on transport error.
                   `split_frames` is `pub(crate)`, shared with proto.rs
  src/pty.rs       allocates the DUT console-bridge PTY (posix_openpt) and
                   holds its own raw-mode handle on the slave for the daemon's
                   lifetime. Without it the slave sits at cooked defaults
                   (ECHO|ICANON) until a reader attaches, and bytes written to
                   the master in that window echo back and get framed to the
                   DUT UART as if typed. panio's serial channel opens a
                   second, separate handle via the stable symlink
  src/server.rs    axum: GET /hid + POST /send (message/body sized to one full
                   `type` line, MAX_TYPE_CHARS + the `type ` verb, so the
                   4096-char cap is reachable), POST /stop (authenticated
                   shutdown; `hidrig stop` never signals the discovery-file
                   PID), /status, /version. WS clients send command lines; all
                   results are broadcast as `evt ok|err ...` so observers see
                   the intermixed stream
  src/auth.rs      token + loopback Host/Origin layer; byte-identical copy in
                   serialcap/hdmicap/ch9329
  src/daemon.rs    advisory lock; discovery file at
                   /tmp/panio-<uid>/hid/<target>/ (channel name, not
                   "hidrig", so panio finds it without knowing the helper).
                   Brings up the console PTY (removing a stale `console`
                   symlink first) and publishes the stable symlink (`console`,
                   falls back to the raw path if the link can't be made) and
                   the real slave path (`console_device`, always present, the
                   source of truth). Graceful shutdown (SIGTERM/SIGINT or a
                   `/stop`-woken `Notify`) releases held keys/buttons via
                   uart.rs's Release, then removes daemon.json and the console
                   symlink; lock-file rule in ../hdmicap/AGENTS.md
  host/hid_seize_reports.c  macOS IOKit tool (see below)
  README.md        wiring, frame protocol, CLI usage
```

## hid daemon + KVM (`hidrig serve`)

The control link has one owner, so KVM streaming and CLI injection cannot both open it. `hidrig serve` owns the link and re-exposes the vocabulary over `GET /hid` (WebSocket) and `POST /send`. Every command, from a browser, `panio hid send` or a script, goes through the one `mpsc` queue in `uart.rs`, one in flight; that single queue makes events intermix correctly.

- `panio console` starts the daemon when the target has a `hid` channel (local `?hid=PORT`; remote an SSH-tunnelled `?hidws=` URL).
- In the default noVNC view, key and pointer events reach this daemon through hdmicap (`/rfb` -> hid_link.rs), not from the page. `video watch`, `console` and `daemons restart` start the local hid daemon for every video device (only `rfb+hid:` errors if it will not start), so starting video on a USB-capture target with a hid channel also makes the hid daemon own the injector.
- In MJPEG mode the `⌨ Capture input` overlay button toggles capture (no click-to-grab, no host-key release); engaged, the page streams `down`/`up`/`moveabs`/`scroll`. The mouse is absolute, so the cursor follows the pointer; the local cursor stays visible as a crosshair (no Pointer Lock, deliberately). Mouse listeners live on the `<img>` so overlay buttons never inject; window blur releases.
- panio finds the daemon by channel name (`daemons::daemon_port("hid")`), staying agnostic to the helper.

Latency: HID frames are fire-and-forget over USB-CDC (no per-frame round trip, no baud negotiation). The dashboard coalesces mouse moves to one `moveabs` per `requestAnimationFrame`. The floor is the target board's USB interrupt `bInterval` (~8 ms per report). Only `0x02` control frames draw a reply; macOS drops the `IOSSDATALAT` read timer on open to keep those round trips prompt.

## panio integration

`panio hid set -t <target> --cmd "hidrig -d <uart>"` stores an opaque command prefix in the lab file's `[targets.<name>.hid]` channel (no device-specific code in `cli/`). `panio hid send -t <target> <args...>` shell-quotes the args, appends them and runs the result via `sh -c` on the channel's host (SSH dispatch via `ChannelKind::Hid`). `panio doctor` probes absolute-path helpers with `test -e`.

`move` and `scroll` take negative numbers (`hidrig move 50 -30`, `hidrig scroll -3`) via `allow_hyphen_values`, with no `--` needed. Same for `panio hid send`'s trailing args; keep `-t` before them.

## Host testing tool (`hidrig/host/`)

`hid_seize_reports.c` opens the injector's HID interface with `kIOHIDOptionsTypeSeizeDevice`, so no keystroke reaches the focused app, and prints hex dumps of every keyboard and mouse report plus the 156-byte report descriptor on first match. It verifies the pipeline without a target:

```bash
cd hidrig/host && make
sudo ./hid_seize_reports   # grant Input Monitoring in System Settings first
```

Then run `hidrig -d <adapter> type/key/move/click/scroll ...` in a second terminal. VID/PID are 0x239A/0x8106 (KB2040 on CircuitPython). The built binary is gitignored; re-run `make` after cloning.
