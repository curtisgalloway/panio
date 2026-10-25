<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# phidget/ — agent notes

The root AGENTS.md rules still apply here; per-OS behavior lives in [docs/dev/platform-support.md](../docs/dev/platform-support.md).

## Module layout

```
phidget/         Rust crate: standalone power helper for Phidgets VINT modules behind a
                 HUB0002 hub, over raw USB (nusb), wired in via the generic power hooks.
                 Only the REL1100 4-channel solid-state relay is enabled.
  src/main.rs    CLI (clap derive) + command logic: list/on/off/cycle/state/doctor, written
                 against the small `hub::Link` trait (read the ports descriptor, send a
                 packet and wait for its ACK). Tests drive it with a fake.
  src/proto.rs   Pure functions, no I/O: Layer A (VINT) and Layer B (hub) framing, ACK tail
                 parsing, status names, the 42-byte ports descriptor, version decode,
                 `<port>/<channel>` parsing. Tests pin the hardware-verified bytes.
  src/devices.rs The tables: supported modules (`MODULES`) and hubs (`HUBS`), and
                 `check_target`, the refuse-before-write validation.
  src/hub.rs     nusb glue: find hubs, select one by serial, open and claim interface 0,
                 control read of the descriptor, bulk OUT 0x02 / IN 0x81, ACK wait.
```

The wire format, with the hardware results, is in
[`notes/phidgets-vint.md`](../notes/phidgets-vint.md). Read it before changing `proto.rs`.

## Rules that are easy to break

- **Blind, and says so.** `on`/`off`/`cycle` succeed when the hub ACKs, which proves a
  module is attached to that port, not that the relay moved. `state` always fails with
  `state unknown` (exit 1, empty stdout) because the power-hook contract only accepts
  `on`/`off` on stdout and treats a non-zero exit as an error. **Do not cache the last
  written state** or add a state file, and do not wire `state_cmd` to this helper.
- **No handshake, no reset.** Never send the GPP open/close reset, port mode, `RESET`
  (0xFA), `ENABLE` (0xFB) or `PORTPOWER`. A single `SETDUTYCYCLE` works from a cold
  module (measured), `RESET` turns relays off, and `PORTPOWER` turns all four off.
- **Never write blind.** Every write first reads the ports descriptor and goes through
  `devices::check_target` (empty port, unknown module, channel out of range are refused).
  `doctor` and `list` only read descriptors; keep them free of bulk writes.
- **Normally open.** `on` = full duty cycle = conducting. There is no NC inversion.
- **One-shot, one packet id.** The packet id is `port * 21 + 1` and the counter nibble is
  always 0 (the hub does not check it). The ACK is matched by id on the IN endpoint.
- **Exclusive USB interface.** Claiming interface 0 fails busy when another process holds
  the hub; `hub::UsbLink::open` retries for 3 s, then names the likely holders.
- **Linux needs a udev rule** for idVendor 06c2 (documented in docs/power.md, not
  packaged). A permission error is mapped to a message that says so.
- **Windows compiles and tests only.** Running it needs WinUSB bound to the hub and is
  unverified. Use `Interface::control_in`, not `Device::control_in` (unsupported on Windows).
- **Doctor JSON is hand-written** (`json_string`), as in dsdrelay and dsdble.
- **No hardware in tests.** Tests use golden byte vectors and a `FakeLink`.

## Adding a VINT module

1. Add a row to `MODULES` in `src/devices.rs`: the VINT device id (`phidget list` prints
   `VINT id 0x..` for a module it does not support), SKU, description, class, channel count.
2. Check it on hardware with a meter. For a `DigitalOutput` module the `SETDUTYCYCLE` packet
   is the same as the REL1100's, so the row is usually all it takes.
3. A different kind of module needs a new `Class` variant, a packet builder in `proto.rs`
   with golden-byte tests, a match arm in `main.rs::set_output`, and its own CLI verbs.

Other hubs (HUB0000 PID 0x3F, HUB0001 PID 0x43) frame packets differently on old firmware;
leave them out of `HUBS` until one is tested.

## Dependencies (approved by the maintainer; keep the set minimal)

- `nusb` 0.2: raw USB access without libusb (macOS IOKit, Linux usbfs, Windows WinUSB). Its
  API changed a lot from 0.1: `list_devices().wait()`, `DeviceInfo::open().wait()`,
  `claim_interface(..).wait()`, endpoint-based bulk I/O with `transfer_blocking`.
- `clap` and `anyhow`: as in every helper.
