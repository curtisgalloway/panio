<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# ch9329/ — agent notes

This file holds the internals of ch9329/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
ch9329/          Rust crate: the *other* hid helper — a WCH CH9329 UART->USB-HID
                 bridge client, hardware-verified against an Openterface
                 Mini-KVM and an Openterface KVM-Go (a CH32V208 emulating the
                 CH9329 protocol; reports chip_version=0x01, not a real chip's
                 0x38 — see notes/openterface-kvm-go.md). The Sipeed
                 NanoKVM-USB speaks the same frame protocol at 57600 but is
                 not bench-verified here. Same
                 CLI surface as hidrig, so it drops into a `hid` channel
                 identically (`paniolo hid set --cmd "ch9329 -d <uart>"`); the
                 chip *is* the HID device, so it speaks the binary frame
                 protocol (HEAD 57 AB / ADDR / CMD / LEN / DATA / SUM) rather
                 than relaying the line protocol. Clean-room spec:
                 notes/ch9329-spec.md
  src/proto.rs     the HID serial protocol grammar executed against a Session
                   instead of forwarded to a microcontroller; `execute_line` is
                   the one backend for CLI subcommands and `run` files, so the
                   accepted command set matches hidrig exactly (sequence parser
                   — delay/sleep bounded to 0..=3600s, `type` keeps trailing
                   whitespace, an embedded CR/LF is refused on this direct path
                   too — + moveabs clamp ported from hidrig/src/proto.rs)
  src/session.rs   the link itself: framing/checksum (rejects a reply from the
                   wrong `raddr`), GET_INFO, held-key/pointer state, and
                   `open()`'s baud probe (BAUD_CANDIDATES = 115200, 57600 — the
                   NanoKVM-USB default — then 9600; force one with -b).
                   Failed Linux opens add a compatibility hint only for the
                   selected USB 1a86:fe0c port (stable aliases resolved).
                   `chip_version` is not a reliable Openterface firmware release;
                   no minimum-version check is possible from it. Captured startup
                   echo and the direct-USB drain workaround are recorded in
                   notes/openterface-v2-linux-serial.md and docs/hid.md.
                   `open()` pushes an all-zero keyboard + mouse report once
                   GET_INFO confirms the chip, since the chip's own HID state
                   outlives the daemon process; `tap`/`combo`/`type_text` best-
                   effort release whatever they pressed if a later step fails,
                   `combo` refuses (ERR) a chord needing more than the 6 key
                   slots rather than truncating it, and typing a usage already
                   held via `down` releases it first so the press is a real
                   edge. `usb_cmd` (the mux-switch command) reworks a reply
                   timeout into a "not supported" message that does NOT read as
                   transport loss, so a mux-less device's `usb state` query
                   never trips uart.rs's reopen. A `#[cfg(test)] pub(crate)
                   test_support` module (FakePort, a full `SerialPort` stub over
                   an in-memory byte queue + write log) lets proto.rs/uart.rs/
                   session.rs tests execute real wire behaviour without
                   hardware. Holds two verified CH9329-on-Linux workarounds —
                   clicks go through the *relative* report (libinput coalesces a
                   button transition in an absolute report at an unchanged
                   coordinate), and `moveabs` nudges one unit before the exact
                   target (an absolute report equal to the previous one is
                   coalesced away)
  src/uart.rs      the UART owner (daemon path): one dedicated thread holding a
                   long-lived Session, serializing CLI- and WebSocket-injected
                   commands (plus a shutdown Release request) onto the one wire,
                   one in flight — which is also what makes held state survive
                   across separate invocations. Only an idempotent query
                   (ping/info/version) is retried once on a lost reply; an input
                   command (key/type/move/click/...) is NOT — re-running the whole
                   line would inject a duplicate keystroke or click, so a lost ACK
                   surfaces as the timeout for the caller to decide about (a
                   persistent timeout still reopens the session; a reopen briefly
                   toggles DTR/RTS, which resets a KVM-Go's MCU). A request whose
                   client already gave up (its reply channel is closed) is dropped
                   without executing, so a timed-out command is not injected late;
                   a shutdown Release reopens the link if a transport error had
                   dropped it before clearing, since the chip holds its last
                   report independent of this process
  src/keys.rs      key-name -> USB HID usage mapping (adafruit_hid Keycode names,
                   US layout, incl. PRINT_SCREEN/SCROLL_LOCK/PAUSE/NUM_LOCK/
                   APPLICATION), shared with the hidrig vocabulary
  src/server.rs    axum: GET /hid + POST /send (WebSocket message / body sized to
                   one full `type` line — MAX_TYPE_CHARS + the `type ` verb — so
                   the documented 4096-char `type` cap is actually reachable and
                   not ~5 characters short; an empty /send body is a no-op, not a
                   forwarded blank command), POST /stop (authenticated shutdown;
                   `ch9329 stop` never signals the discovery-file PID), /status,
                   /version
  src/auth.rs      token + loopback Host/Origin layer over the whole router
                   (byte-identical in serialcap/hdmicap/hidrig)
  src/daemon.rs    `serve`/`stop`: owns the UART, publishes the same
                   /tmp/paniolo-<uid>/hid/ discovery file paniolo's console
                   reads; graceful shutdown — on SIGTERM/SIGINT or a `POST
                   /stop`-woken `Notify` alike — releases held keys/buttons
                   (via uart.rs's Release request) before exiting, and never
                   touches the USB mux. Shutdown removes daemon.json only — NOT
                   the lock file, which stays open (flock'd) until this process
                   exits; see the identical note on hdmicap's daemon.rs above
  README.md        wiring, extras beyond hidrig's surface (`info` reports target
                   USB enumeration + lock LEDs; `baud` persists a rate to flash),
                   and the hardware-verified status notes
```
