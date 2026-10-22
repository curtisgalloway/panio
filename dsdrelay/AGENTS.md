<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# dsdrelay/ — agent notes

The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
dsdrelay/        Rust crate: standalone power helper for DSD TECH SH-UR01A (1-channel,
                 CP2102N) and SH-UR04A (4-channel, older CP2102) USB serial relay
                 boards (VID 10c4 / PID ea60, 9600 8N1, ASCII AT commands), wired in
                 via the generic power hooks.
  src/main.rs    CLI (clap derive) + command logic: on/off/state/cycle/doctor/list.
  src/proto.rs   The AT dialect as pure functions over a `Transport` (Read + Write +
                 clear_input + settle), the NC map (`coil_for`/`target_on`), reply parsing,
                 and `select_port`. `proto::fake::FakeRelay` is the in-memory board
                 the tests drive. It models the SH-UR04A framing (acts only on
                 `\r\n`-terminated lines, keeps a partial line in `inbuf`, bare line =
                 no reply, replies end `\n\0`; `nul = false` models the SH-UR01A) and
                 can ignore `off` to prove read-back catches it.
```

## Protocol

`AT` -> `OK`; `AT+CHn=?` -> `OK+CHn=0|1`; `AT+CHn=1|0` -> `OK+CHn=1|0`; an unknown
command or a channel the board lacks -> `Error`. Opening or closing the port does not
change relay state. Measured on hardware:

- **SH-UR04A (4-channel, older CP2102, product string "CP2102 USB to UART Bridge
  Controller")**: channels 1..4 (`AT+CH5=?` -> `Error`) switch independently and read
  back exactly (all four verified; switching through a wired load not yet). Its USB
  serial number is the factory default `0001`, shared by every such board, so macOS
  names the node `/dev/cu.usbserial-0001` for all of them. Every reply ends `\n\0`
  (a NUL after the newline).
- **SH-UR01A (1-channel, CP2102N, unique serial)**: accepts `\r\n`-terminated
  commands (`AT\r\n` -> `OK\n`), replies without the NUL, in about 55 ms.
- **Commands must end in `\r\n`.** An unterminated command gets no reply and its
  bytes stay in the board's input buffer *across port close and reopen*, corrupting
  the next command (which answers `Error`). A bare `\r\n` on a clean buffer gets no
  reply; on leftover garbage it gets `Error\n\0`, after which the next command is
  answered correctly.
- So `proto::exchange` resyncs before every command: write a bare `\r\n`, wait
  `RESYNC_SETTLE` (150 ms, `Transport::settle`, a no-op in tests), clear input
  (discarding the resync's `Error`), then write `cmd\r\n`. The reply reader skips NUL
  bytes anywhere and does not count them toward the cap. Callers pass commands without
  terminators.

## Rules that are easy to break

- **NC wiring is hardcoded.** Targets sit on the normally closed contact:
  `on` = release the coil (`=0`), `off` = energize it (`=1`). The inversion lives in
  `proto::coil_for` / `proto::target_on` only. An unpowered board leaves targets
  powered. Do not add a flag to flip it without discussing it; the lab file and the
  hook strings carry no wiring knowledge.
- **`list` must never open a port.** A Sonoff ZBDongle-P (CC2652) uses the same
  CP2102N VID:PID and opening its port can reset it into the bootloader.
  `--serial` matching enumerates only; it opens the one port it selected.
- **`list` shows one line per device** (macOS `/dev/tty.X` twin dropped via
  `prefer_cu`) and warns on stderr when USB serials are shared or `0001`
  (`serial_warning`). `-s` is for the SH-UR01A; use `-d` for the SH-UR04A (Linux:
  `/dev/serial/by-path/...`).
- **`state` maps only 0 and 1.** Any other reply is an error carrying the raw reply.
- **on/off confirm twice** (the command echo, then a fresh `AT+CHn=?`), and `cycle`
  confirms both phases.
- **No new dependencies.** The doctor JSON is hand-written (`json_string`), because
  the report is a flat list of short strings; do not pull in serde_json for it.
- **No hardware in tests.** Tests run against `FakeRelay` and a fabricated port list.
