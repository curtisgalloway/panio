<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# HID serial protocol, version 1

A device-independent text protocol for driving a USB HID keyboard + mouse
plugged into a *target* machine from a *control host*. It defines the command
vocabulary (`type`, `key`, `combo`, `move`, `moveabs`, `click`, …) and the
`OK`/`ERR` reply convention.

This document is **normative for the command vocabulary (§3 onward)**. §1's
device-side framing is historical: it describes the retired single-board rig.

On the **default rig, the dual-board KB2040 "dumb pipe"**
([hid-dual-board-design.md](hid-dual-board-design.md); KB2040 is an Adafruit
RP2040 board), this vocabulary is the external interface: the `hidrig` CLI,
`panio hid send`, and the daemon's WebSocket carrier (§1). `hidrig`
**composes** commands into HID report bytes and sends **binary frames** to the
boards; the line protocol does not travel on the device wire.

Two **rig-specific** `hidrig` features sit outside this vocabulary: DUT
**power** control (`hidrig power off|on|cycle`) and a **serial-console**
bridge. They use panio's `power` and `serial` channels, not `hid`, so other
HID backends need not implement them. See
[hid-dual-board-design.md](hid-dual-board-design.md) §6–§7.

| Consumer | Status |
|---|---|
| `hidrig` CLI / `serve` daemon (`hidrig/src/`) | Reference host implementation — composes the vocabulary into HID reports |
| [`hidrig-kb2040/firmware/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware) (paniolo-hardware repo) — dual-board | Default rig: relays report bytes, does **not** parse this protocol |
| [`hidrig-kb2040/firmware/single-board/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware/single-board) (paniolo-hardware repo) — single-board firmware | **Retired** reference device implementation (over a UART) |
| WCH CH9329 bridge (serial-to-USB-HID chip; see [ch9329-spec.md](https://github.com/curtisgalloway/panio/blob/main/notes/ch9329-spec.md)) | Implemented — the host-side [`ch9329`](https://github.com/curtisgalloway/panio/blob/main/ch9329/README.md) crate (one-shot CLI + `serve` daemon), a helper backend for the same `hid` channel |

Above `hidrig`, panio and the dashboard speak only this vocabulary, so any
injector behind the generic `hid` channel works unchanged.

---

## 1. Transport

- Any bidirectional byte stream. The reference implementation uses a UART:
  **115200 baud, 8N1, no flow control, 3.3 V logic**. USB CDC or a TCP socket
  are equally valid.
- Encoding is **UTF-8**.
- No framing, checksums, or escaping: the stream is assumed reliable (a bench
  wire).

**WebSocket carrier (daemon / KVM path).** Same command grammar as the UART.

- `hidrig serve` owns the device link and re-exposes the line protocol at
  `GET /hid`. Each client text frame is one command line (no trailing `\n`).
- Results are **broadcast to all connected clients** as transcript frames
  (`evt ok <line> :: <reply>` / `evt err <line> :: <reply>`); an issuer reads its
  own result off the shared stream. `POST /send` returns the raw `OK`/`ERR`
  reply directly.
- Authentication: present the token from the daemon's discovery file —
  `?token=` on the WebSocket URL, `Authorization: Bearer` on `/send` — from a
  loopback `Host`/`Origin`.
- The daemon serializes all clients' commands onto the device link, one in
  flight.

## 2. Framing and flow control

- A **command** is a single line terminated by `\n` (LF). Implementations
  MUST tolerate and strip a trailing `\r` (CR).
- A **response** is a single line, also LF-terminated:
  - `OK` — success.
  - `OK <data>` — success with payload (e.g. `version`).
  - `ERR <message>` — failure; `<message>` is free-form human-readable text.
- Exactly one response per command, in order; no unsolicited lines.
- The host MUST wait for the response before sending the next command; this
  is the only flow control. Responses to long commands (`type` of a long
  string, a large `move`) arrive after the last HID report is submitted.
- A device that cannot parse or execute a command MUST reply `ERR ...` and
  keep running.
- Empty lines are ignored (no response).

## 3. Commands

Commands and key/button names are case-insensitive (canonical: lowercase
verbs, UPPERCASE keys). Arguments are separated by single spaces.

| Command | Reply | Effect |
|---|---|---|
| `type <text>` | `OK` | Type `<text>` (everything after the first space, verbatim) as keystrokes |
| `key <NAME>` | `OK` | Tap (press + release) one key |
| `combo <NAME>...` | `OK` | Chord: press all named keys, then release all |
| `down <NAME>` | `OK` | Press and hold a key |
| `up <NAME>` | `OK` | Release a held key |
| `releaseall` | `OK` | Release all held keys |
| `move <dx> <dy>` | `OK` | Relative mouse move; signed decimal integers |
| `moveabs <x> <y>` | `OK` | Absolute mouse move in a `0..32767` logical space (capability `moveabs`) |
| `click <button>` | `OK` | Tap (press + release) a mouse button |
| `mdown <button>` | `OK` | Press and hold a mouse button |
| `mup <button>` | `OK` | Release a held mouse button |
| `scroll <amount>` | `OK` | Scroll wheel; signed decimal integer, positive = up |
| `baud <rate>` | `OK` | Switch the serial link to `<rate>` baud (capability `baud`) |
| `ping` | `OK` | No-op liveness check |
| `version` | `OK <ver> <impl> [caps...]` | Protocol version + implementation id + capability tokens |

- `<button>` is `left`, `right`, or `middle`.
- `move` / `scroll` values may exceed one HID report's range (int8 for
  boot-protocol relative mice). The device MUST split them into multiple
  reports (or, for an absolute-pointer device, accumulate the delta into its
  tracked cursor).
- `combo` presses at most **6 keys** at once (the boot keyboard report's key
  slots). Modifiers have their own byte and don't count. A chord that needs
  more — counting keys already held via `down` — MUST reply `ERR`, not drop
  keys.
- `moveabs <x> <y>` positions the pointer in `0..32767` on each axis, which the
  host OS maps across the full screen; callers scale pixel coordinates against
  the screen size: `logical = round(pixel * 32767 / screen_size)` on each axis.
  - It is **optional**, advertised by the `moveabs` capability in `version`. A
    device without it MUST reply `ERR`; callers fall back to relative `move`.
  - It requires an absolute-axis HID report descriptor on the device.
- `type` text is the remainder of the line after `type `, verbatim:
  - It may contain spaces and `#`. Trailing spaces are part of the text; only
    the line terminator is stripped.
  - No quoting or escaping. An embedded CR/LF is invalid (§2).
  - A device MUST reply `ERR` for a character outside its keyboard layout
    (reference: US) rather than type it approximately or drop it; a
    partially-typed string is worse than a refused one.
- `baud <rate>` changes the serial link's speed mid-session (optional,
  capability `baud`). It applies to a UART and is meaningless on TCP/WebSocket.
  - The **device boots at its default rate** (115200 for the reference
    firmware, §1).
  - **Handshake:** the device replies `OK` **at the current rate**, then
    switches to `<rate>`. The host reads that `OK`, switches its port to
    `<rate>`, waits briefly, and confirms with a `ping` (reverting on no reply).
  - The device SHOULD return to its boot default on power-cycle so a later
    naive connect re-syncs.
  - A device without it MUST reply `ERR` and stay at the current rate.
- `version` replies `OK <ver> <impl> [caps...]`, e.g.
  `OK 1 kb2040-circuitpython/1.0 moveabs baud`. `<ver>` is the protocol
  version (decimal integer) for compatibility; `<impl>` is a free-form id; each
  remaining token is an **optional capability** (e.g. `moveabs`, `baud`). A
  missing token means that command will `ERR`.

### Key names

`<NAME>` values are HID usage names in the `adafruit_hid` `Keycode`
convention. Implementations MUST accept at least:

- Letters `A`–`Z`; digits `ZERO`–`NINE` (top row); `KEYPAD_ONE`-style names
  are optional.
- `ENTER`, `TAB`, `SPACE`, `ESCAPE`, `BACKSPACE`, `DELETE`, `INSERT`,
  `HOME`, `END`, `PAGE_UP`, `PAGE_DOWN`.
- `UP_ARROW`, `DOWN_ARROW`, `LEFT_ARROW`, `RIGHT_ARROW`.
- `LEFT_CONTROL`, `LEFT_SHIFT`, `LEFT_ALT`, `LEFT_GUI` and the `RIGHT_*`
  forms.
- `F1`–`F12`.
- `MINUS`, `EQUALS`, `LEFT_BRACKET`, `RIGHT_BRACKET`, `BACKSLASH`,
  `SEMICOLON`, `QUOTE`, `GRAVE_ACCENT`, `COMMA`, `PERIOD`,
  `FORWARD_SLASH`, `CAPS_LOCK`, `PRINT_SCREEN`, `SCROLL_LOCK`, `PAUSE`,
  `NUM_LOCK` (alias `KEYPAD_NUMLOCK`), `APPLICATION` (alias `MENU`).

An unknown name is an `ERR`. The reference implementation accepts the full
`adafruit_hid.Keycode` table; other implementations map names to HID usage IDs
themselves.

## 4. Device behavior

- **Boot:** the device serves the protocol as soon as it is ready, with no
  banner (the host could not tell one from a stale buffer). Hosts `ping` to
  detect liveness.
- **Target not enumerated:** commands that need USB may block until
  enumeration or fail with `ERR`; the device MUST NOT crash. The reference
  implementation blocks at startup until the target enumerates, then replies
  `ERR` on send failures (e.g. target suspend).
- **Power:** an injector powered from the target's USB port reboots with the
  target, losing held keys. Hosts must tolerate serial silence while the target
  is off.
- **State:** held keys/buttons (`down`/`mdown`) plus, for an absolute-pointer
  device, the tracked cursor (`move`/`moveabs`). `releaseall` clears held keys.
  There is no reset command; power-cycling the device is the reset.

## 5. Reserved extensions

New capabilities follow the `moveabs` pattern: an optional command advertised
by a `version` token, `ERR` when unsupported, no protocol version bump. Only a
change that breaks v1 semantics of an existing command bumps the `version`
integer. Reserved:

- `consumer <NAME>` — consumer-control usages (volume, media keys).

## 6. Conformance checklist for a new implementation

1. Serve the byte stream (UART/CDC/TCP): UTF-8, LF-terminated lines, CR
   tolerated.
2. Implement every required command in §3; reply `ERR` (never crash or go
   silent) on anything unparseable or unsupported.
3. One response per command, in order, after the HID effect is submitted.
4. Accept the §3 key-name set case-insensitively.
5. Split oversized `move`/`scroll` into multiple HID reports.
6. Reply `OK 1 <your-impl-id> [caps...]` to `version`, listing each optional
   capability you implement (`moveabs` if you have an absolute pointer).
7. Verify with `hidrig -d <port> ping`, `version`, a `type` round-trip, and
   `hidrig run` of a sequence file. For `moveabs`, check the cursor lands
   correctly across the target's full screen.
