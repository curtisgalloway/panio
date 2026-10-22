<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# ch9329/ — agent notes

This file holds the internals of ch9329/, moved out of the root AGENTS.md to keep it short. The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
ch9329/          WCH CH9329 UART->USB-HID bridge client. Verified on an
                 Openterface Mini-KVM and KVM-Go (a CH32V208 emulating the
                 protocol, chip_version=0x01 vs a real chip's 0x38; see
                 notes/openterface-kvm-go.md); Sipeed NanoKVM-USB (same
                 protocol, 57600) is not bench-verified. Same CLI as hidrig, so
                 a drop-in `hid` channel (`--cmd "ch9329 -d <uart>"`). The chip
                 is the HID device, so it speaks binary frames (HEAD 57 AB /
                 ADDR / CMD / LEN / DATA / SUM), not the line protocol.
                 Spec: notes/ch9329-spec.md
  src/proto.rs     HID serial grammar run against a Session; `execute_line`
                   is the one backend for CLI and `run` files, so the command
                   set matches hidrig (delay/sleep bounded to 0..=3600 s,
                   `type` keeps trailing whitespace, embedded CR/LF refused,
                   moveabs clamp ported from hidrig/src/proto.rs)
  src/session.rs   framing/checksum (rejects a reply from the wrong `raddr`),
                   GET_INFO, held-key/pointer state, `open()`'s baud probe
                   (BAUD_CANDIDATES = 115200, 57600, 9600; force with -b).
                   Preserve:
                   - `open()` pushes an all-zero keyboard + mouse report once
                     GET_INFO confirms the chip (its HID state outlives us).
                   - `tap`/`combo`/`type_text` best-effort release whatever
                     they pressed if a later step fails; `combo` refuses (ERR)
                     a chord needing more than the 6 key slots rather than
                     truncating; typing a usage already held via `down`
                     releases it first so the press is a real edge.
                   - `usb_cmd` (mux switch) turns a reply timeout into a "not
                     supported" error that is not transport loss, so a
                     mux-less `usb state` never trips uart.rs's reopen.
                   - Two verified CH9329-on-Linux workarounds: clicks go
                     through the relative report (libinput coalesces a button
                     transition in an absolute report at an unchanged
                     coordinate); `moveabs` nudges one unit before the exact
                     target (an absolute report equal to the previous one is
                     coalesced away).
                   - `chip_version` is not a reliable Openterface firmware
                     release; no minimum-version check is possible.
                   - On Linux, a GET_INFO timeout on the selected USB
                     1a86:fe0c port gets PARSER_FLUSH_LEN (261) zero bytes
                     and one retry before the next baud candidate (#238);
                     only a timeout triggers it, never a malformed reply.
                     A failure after that adds a compatibility hint.
                     Startup echo and the manual direct-USB drain:
                     notes/openterface-v2-linux-serial.md, docs/hid.md.
                   - Cancellation (#328): `tap`/`combo`/`type_text`/`click`/
                     `move_rel`/`scroll` take a `Cancel` (`&dyn Fn() -> bool`),
                     polled between reports and inside holds/pacing sleeps
                     (taken in 20 ms slices, `sleep_unless_cancelled`). A
                     cancelled command releases what it pressed (keys held
                     earlier with `down` stay) and fails with `CANCELLED`
                     ("cancelled: client went away"), which is not a
                     transport error. The no-daemon path passes
                     `never_cancelled`. `execute_line` has no `delay`/`sleep`
                     (only `run` files do, client-side).
                   - `test_support` (`#[cfg(test)]` FakePort, an in-memory
                     SerialPort stub; `FakePort::acking` ACKs every frame)
                     lets tests run real wire behavior.
  src/uart.rs      UART owner (daemon path): one thread holding a long-lived
                   Session, serializing CLI and WebSocket commands (plus a
                   shutdown Release) onto the wire, one in flight; this also
                   keeps held state across invocations. Only an idempotent
                   query (ping/info/version) is retried once on a lost reply;
                   an input command is not (a retry would duplicate the
                   keystroke or click), so a lost ACK surfaces as a timeout
                   for the caller. A persistent timeout reopens the session;
                   a reopen toggles DTR/RTS, which resets a KVM-Go's MCU. A
                   request whose client gave up (reply channel closed) is
                   dropped, so a timed-out command is not injected late, and
                   one already running is cancelled the same way
                   (`serve_line` passes `|| reply.is_closed()` down to the
                   session; the cancel is broadcast as an error event and
                   logged at info). The daemon's wait is `proto::budget(line)`
                   (a `type`: 10 s + 100 ms per character; else 30 s), the
                   same function the CLI uses (+ `CLIENT_MARGIN` 5 s). A
                   shutdown Release reopens a dropped link first, since the
                   chip holds its last report independent of this process
  src/keys.rs      key name -> USB HID usage (adafruit_hid Keycode names, US
                   layout), shared with the hidrig vocabulary
  src/server.rs    axum: GET /hid + POST /send (sized to one full `type` line,
                   MAX_TYPE_CHARS + the `type ` verb, so the 4096-char cap is
                   reachable; an empty /send body is a no-op, not a forwarded
                   blank command), POST /stop (authenticated shutdown;
                   `ch9329 stop` never signals the discovery-file PID),
                   /status, /version. Cancellation: hyper drops a /send
                   handler when its client disconnects, which closes the
                   reply channel; `handle_ws` races each in-flight `send`
                   against the socket's next frame and drops the send on
                   close (frames arriving meanwhile queue and run in order)
  src/auth.rs      token + loopback Host/Origin layer; byte-identical copy in
                   serialcap/hdmicap/hidrig
  src/daemon.rs    `serve`/`stop`: owns the UART and publishes the same
                   /tmp/panio-<uid>/hid/ discovery file the console reads.
                   Graceful shutdown (SIGTERM/SIGINT or a `/stop`-woken
                   `Notify`) releases held keys/buttons via uart.rs's Release
                   first, and never touches the USB mux; lock-file rule in
                   ../hdmicap/AGENTS.md
  src/main.rs      client. Through a daemon, `post_send_within` waits
                   `budget + 5 s`; on its own timeout, a mid-request
                   connection failure, or the daemon's "did not answer
                   within" error it queues a no-op `version` (never touches
                   the wire, answered only after the earlier command has
                   stopped) and waits 5 s: answered = exit 40 (cancelled and
                   confirmed stopped), else exit 41 (may still be typing).
                   Anything else still exits 1. panio reports these as
                   101 `helper_failed` with `child_exit` 40/41
  README.md        wiring, extras beyond hidrig (`info`, `baud`), status
```
