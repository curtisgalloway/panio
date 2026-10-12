<!--
Copyright 2026 Curtis Galloway

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# ch9329 — CH9329 USB-HID injector for paniolo

**Terms:** HID is USB keyboard/mouse input; UART is a hardware serial port;
CDC-ACM is USB's virtual serial-port interface. See the
[glossary](../docs/glossary.md).

`ch9329` is a paniolo `hid` helper (the target's keyboard and mouse) for the
**WCH CH9329**, a chip that turns UART commands into USB keyboard and mouse
input on the target. It has the **same CLI surface** as
[`hidrig`](../hidrig/README.md), so `paniolo hid send` drives it the same way.

| Device | How it connects | Status |
|---|---|---|
| **[Openterface Mini-KVM](https://openterface.com/)** (its keyboard/mouse half) | a real CH9329 behind its CH340 USB-serial adapter | bench-verified |
| **Openterface KVM-Go** | a CH32V208 emulating the CH9329 protocol over its own USB-CDC port; reports `chip_version=0x01` instead of a real chip's `0x38` (see [`notes/openterface-kvm-go.md`](../notes/openterface-kvm-go.md)) | bench-verified, unmodified helper |
| **Sipeed NanoKVM-USB** | same protocol, linked at 57600 baud | should work; not bench-verified here |

The CH9329 *is* the USB HID device, so `ch9329` speaks its **binary frame
protocol** directly (`HEAD 57 AB · ADDR · CMD · LEN · DATA · SUM`); see
[`notes/ch9329-spec.md`](../notes/ch9329-spec.md).

## Wiring it into a target

```bash
# Build/install with everything else (lands in ~/.local/libexec/paniolo/bin):
make install            # from the repo root; or: make rust

# Bind the CH9329 to a target's hid channel (use its CH340 serial device):
paniolo hid set -t winbox --cmd "ch9329 -d /dev/cu.usbserial-4120"

# Drive it — identical to any hid helper:
paniolo hid send -t winbox type "hello world"
paniolo hid send -t winbox combo LEFT_CONTROL S
paniolo hid send -t winbox moveabs 16384 16384   # center; 0..32767 logical
paniolo hid send -t winbox click left
paniolo hid send -t winbox ping
```

Add a `video` channel on the same Openterface's MS2109 capture
(`paniolo video set -t winbox --device "<id>"`) for a full KVM:
`paniolo video shot` for eyes, `ch9329` for hands.

## Commands

The full [HID serial protocol](../docs/dev/hid-serial-protocol.md) §3 surface:
`type`, `key`, `combo`, `down`, `up`, `releaseall`, `move`, `moveabs`, `click`,
`mdown`, `mup`, `scroll`, `ping`, `version`, and `run <file>` for command
sequences.

- **Key names** are `adafruit_hid` Keycode names (`A`–`Z`, `ENTER`,
  `LEFT_CONTROL`, `FORWARD_SLASH`, `F1`…`F12`, `PRINT_SCREEN`, `SCROLL_LOCK`,
  `PAUSE`, `NUM_LOCK`, `APPLICATION`).
- **`type`** assumes a **US layout** and rejects a character it can't type with
  `ERR`.
- **`combo`** chords at most 6 non-modifier keys and refuses more with `ERR`.
- **`version`** reports `1 ch9329/0.1.0 moveabs`: it has a true absolute
  pointer. It does *not* advertise `baud` (see
  [`baud`](#extras-beyond-hidrig) below).

### Extras beyond hidrig

- **`ch9329 -d <dev> info`**: CH9329 `GET_INFO`. Reports device `chip_version`,
  whether the target has enumerated the emulated HID (`target_connected`),
  lock-LED state, and the negotiated baud. Useful for `paniolo doctor`-style
  checks.
- **`ch9329 -d <dev> usb host|target|state`**: switch or query the USB mux that
  shares the onboard microSD reader between host and target. **Openterface
  KVM-Go** only; a real CH9329 lacks it. Wired into paniolo as the `usb` channel
  (`paniolo usb attach-host|attach-target|state`, docs/usb.md); protocol in
  notes/openterface-usb-mux-spec.md.
  - All three print the resulting side; `host`/`target` fail if the mux did not
    move.
  - A device without a mux never answers, so the wait times out and the daemon
    reports "this device does not support USB mux switching". A `usb state`
    query on ordinary hardware does not trigger a reopen (each reopen briefly toggles DTR/RTS, which resets a
    KVM-Go's MCU).
- **`ch9329 -d <dev> baud <rate>`**: **persistently** set the chip's serial
  baud (`SET_PARA_CFG` → flash → `RESET` to activate), then reconnect at the new
  rate. Range 1200..=115200 (Openterface default 115200; NanoKVM-USB 57600;
  factory chips 9600). The `RESET` re-enumerates the USB HID, so the target
  briefly sees the keyboard/mouse disconnect.
  - Unlike the protocol's transient `baud`, this survives power-cycle, so
    `version` does not advertise it and the daemon never calls it.
- **`-b/--baud <rate>`**: force the link rate for *this* connection without
  changing the chip (default: autodetect 115200, 57600, then 9600). Needed
  after `baud` set a rate outside that list.

## Status and limitations

**Openterface CDC on Linux:** failed connections to a selected `1a86:fe0c`
port include a compatibility warning. Startup data echoed by the tty can leave
the firmware parser unresponsive; a cold direct-USB drain before opening the
tty restored operation on one tested Mini-KVM. See
[the Linux guide](../docs/hid.md#openterface-cdc-on-linux) and
[captured investigation](../notes/openterface-v2-linux-serial.md).
`chip_version` may identify the board variant, not its firmware release. No
reliable minimum release or automatic outdated-firmware check is established.

**Verified end to end** on a chip 0x38 over a CH340 at 115200 into a Raspberry
Pi OS desktop: typing (US layout), special keys (`ENTER`/`ESCAPE`/`CAPS_LOCK`,
confirmed via the lock-LED state in `GET_INFO`), absolute pointer, click and
right-click. The **Openterface KVM-Go** is verified too (`chip_version=0x01`,
115200; see [`notes/openterface-kvm-go.md`](../notes/openterface-kvm-go.md)).
The `baud` command (`notes/ch9329-spec.md` §5) is hardware-verified and
persists across processes.

**Lock keys are held for 200 ms** (`CAPS_LOCK` etc.) before release, because
macOS ignores shorter lock-key presses.

**Two CH9329-on-Linux quirks are worked around in `session.rs`:**

- *Clicks use the **relative** report.* libinput ignores a button change in an
  absolute report at an unchanged coordinate. A zero-motion relative `BTN`
  report clicks at the current pointer, so `moveabs` then `click` lands
  correctly.
- *`moveabs` nudges one unit first*, because the device ignores an absolute
  report equal to its previous one.

**The KVM daemon (`serve`/`stop`) is hardware-verified.** `ch9329 serve` owns
the UART and serves the protocol on a localhost WebSocket (`GET /hid`) plus a
`POST /send` one-shot endpoint. It publishes
`/tmp/paniolo-<uid>/hid/<target>/daemon.json` (port and per-start token) for
paniolo's `console`, so the web-console "Capture input" KVM works.

- While a daemon runs, `paniolo hid send` routes through it, so the CLI and
  browser share the UART.
- The UART uses the blocking `serialport` path on a dedicated thread, because
  tokio-serial's async reads are unreliable on a macOS tty.

**Held state (`down`/`up`/`mdown`/drag) persists across commands only through
the daemon.** Without one, `ch9329 down A` resets per process. `combo` and
`run` sequences still compose within one process.

**Recovery clears the chip's own report.** A fresh `open()` (first start, or a
reopen after a transport error) sends an all-zero keyboard and mouse report once
`GET_INFO` confirms the chip, instead of trusting a blank in-memory `Session`.

- A lost reply is retried once before the daemon reopens.
- `tap`/`combo`/`type` try to release what they pressed if a later step fails.
- Graceful shutdown (`SIGTERM`/Ctrl-C) releases every held key, modifier and
  mouse button. It never touches the USB mux.

## License

Apache 2.0 — see the repository [LICENSE](../LICENSE).
