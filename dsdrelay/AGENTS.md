<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# dsdrelay/ — agent notes

The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
dsdrelay/        Rust crate: standalone power helper for DSD TECH SH-UR01A-style
                 USB serial relay boards (Silicon Labs CP2102N, VID 10c4 / PID ea60,
                 9600 8N1, ASCII AT commands), wired in via the generic power hooks.
  src/main.rs    CLI (clap derive) + command logic: on/off/state/cycle/doctor/list.
  src/proto.rs   The AT dialect as pure functions over a `Transport` (Read + Write +
                 clear_input), the NC map (`coil_for`/`target_on`), reply parsing,
                 and `select_port`. `proto::fake::FakeRelay` is the in-memory board
                 the tests drive; it can ignore `off` to prove read-back catches it.
```

## Protocol

`AT` -> `OK`; `AT+CHn=?` -> `OK+CHn=0|1`; `AT+CHn=1|0` -> `OK+CHn=1|0`; an unknown
command or a channel the board lacks -> `Error`. No terminator is required, every
reply ends in `\n`, the reply arrives in about 55 ms. Opening or closing the port
does not change relay state. Only channel 1 of the 1-channel board is verified on
hardware; `AT+CHn` for n = 1..N on multi-channel boards is expected, not measured.

## Rules that are easy to break

- **NC wiring is hardcoded.** Targets sit on the normally closed contact:
  `on` = release the coil (`=0`), `off` = energize it (`=1`). The inversion lives in
  `proto::coil_for` / `proto::target_on` only. An unpowered board leaves targets
  powered. Do not add a flag to flip it without discussing it; the lab file and the
  hook strings carry no wiring knowledge.
- **`list` must never open a port.** A Sonoff ZBDongle-P (CC2652) uses the same
  CP2102N VID:PID and opening its port can reset it into the bootloader.
  `--serial` matching enumerates only; it opens the one port it selected.
- **`state` maps only 0 and 1.** Any other reply is an error carrying the raw reply.
- **on/off confirm twice** (the command echo, then a fresh `AT+CHn=?`), and `cycle`
  confirms both phases.
- **No new dependencies.** The doctor JSON is hand-written (`json_string`), because
  the report is a flat list of short strings; do not pull in serde_json for it.
- **No hardware in tests.** Tests run against `FakeRelay` and a fabricated port list.
