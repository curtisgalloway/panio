<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# dsdble/ — agent notes

The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
dsdble/          Rust crate: standalone power helper for the DSD TECH SH-BT01E Bluetooth LE
                 relay board (1-channel; 2- and 4-channel variants share the protocol),
                 wired in via the generic power hooks. The Bluetooth sibling of dsdrelay.
  src/main.rs    CLI (clap derive) + command logic: on/off/state/cycle/doctor/scan, written
                 against two small traits: `Relay` (write one frame) and `Probe` (the
                 read-only doctor steps). Tests drive them with fakes.
  src/proto.rs   Pure functions: `frame`, the NC map (`coil_for`/`target_frame`), channel
                 validation, address normalization/matching, `display_address`.
  src/ble.rs     btleplug glue: `Session` implements `Relay` and `Probe` with blocking
                 wrappers over a tokio runtime. No unit tests (no hardware in tests).
```

## Protocol (measured on hardware 2026-10-22)

Board: DSD TECH SH-BT01E; device-info model `SH-K09-1.0`, firmware 6.1.2; advertises the
local name `DSD TECH`.

- GATT service `0000ffe0-0000-1000-8000-00805f9b34fb`, characteristic `0000ffe1-...`:
  write / write-without-response / notify. **Read is not permitted** (GATT "Read Not
  Permitted").
- Frame, 4 bytes: `A0 <channel> <state> <checksum>`, checksum = `(0xA0 + channel + state)
  & 0xFF`. `A0 01 01 A2` energizes channel 1's coil, `A0 01 00 A1` releases it. Idempotent.
  Written with write-with-response.
- **No state feedback at all.** Zero notifications on FFE1, ever. `A0 01 02 A3`,
  `A0 01 03 A4` and a lone `FF` are ignored.
- A second service, TI OAD `f000ffc0-0451-4000-b000-000000000000` (firmware update), is
  also exposed.
- The DIS serial (`1.0.0.0-LE`) and system id are factory placeholders shared by every
  board, and the name is shared too, so nothing but the Bluetooth address (Linux MAC;
  macOS per-host CoreBluetooth UUID) tells two boards apart.
- The board cannot report its channel count. Channels 1 to 4 are accepted; a channel the
  board lacks is silently ignored.

## Rules that are easy to break

- **Never write to anything but FFE1.** The OAD service is a firmware updater. `ble.rs`
  selects the characteristic by its UUID and passes nothing else to `write`; keep it so.
- **Blind, and says so.** `on`/`off`/`cycle` succeed when the write-with-response is
  acknowledged, which proves receipt, not relay movement. `state` always fails with
  `state unknown` (exit 1, empty stdout) because the power-hook contract only accepts
  `on`/`off` on stdout and treats a non-zero exit as an error; there is no "unknown" token.
  **Do not cache the last written state** or add any state file: a remembered value goes
  stale the moment someone uses the board's button or another controller. Do not wire
  `state_cmd` to this helper.
- **NC wiring is hardcoded.** Targets sit on the normally closed contact: `on` = release
  the coil (state byte 0), `off` = energize it (1). The inversion lives in
  `proto::coil_for` only. An unpowered board leaves targets powered. Do not add a flag to
  flip it without discussing it.
- **`scan` and `doctor` never write.** `scan` connects to nothing. `doctor` connects and
  discovers services but sends no frame, and stays inside panio's 10 s doctor budget (scan
  capped at 4 s).
- **macOS is best effort.** CoreBluetooth's privacy gate (TCC) kills an unbundled CLI that
  was not launched from a Bluetooth-permitted terminal app with SIGABRT, so hook use over
  SSH is unsupported there. Agent shells hit this too; never run `scan`/`on`/`off` from an
  agent shell on macOS. Windows (WinRT) only has to compile and pass the CI triple.
- **No hardware in tests.** Tests run against `FakeRelay`/`FakeProbe`.
- **Doctor JSON is hand-written** (`json_string`), as in dsdrelay; do not pull in
  serde_json for a flat list of short strings.

## Dependencies (approved by the maintainer; keep the set minimal)

- `btleplug` 0.13: the cross-platform BLE library (BlueZ over D-Bus on Linux,
  CoreBluetooth on macOS, WinRT on Windows). Building on Linux needs `pkg-config` and
  `libdbus-1-dev` (via `dbus` -> `libdbus-sys`); the binary needs `libdbus-1-3` at
  runtime, which `packaging/nfpm.yaml` declares.
- `tokio` 1 with `rt`, `rt-multi-thread`, `time` and `net`: btleplug is async and needs a
  tokio runtime. `net` provides `Builder::enable_io`: on Linux btleplug talks to BlueZ
  through `dbus-tokio`, which panics on a runtime without the I/O driver ("IO is
  disabled"; measured 2026-10-22, macOS never takes that path). `rt-multi-thread` keeps the D-Bus connection's driver task running while the
  main thread sleeps between a cycle's two writes; `time` is for scan polling and the
  connect/write timeouts. No `macros` feature: nothing uses `#[tokio::main]`.
- `clap` and `anyhow`: as in every helper.
