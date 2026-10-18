<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# CH9329 USB-HID bridge — clean-room driver spec

> **Status: implemented.** The host-side [`ch9329`](../ch9329/README.md) crate
> speaks this protocol — an **external helper** wired in through the generic
> `hid` channel like any other injector (`panio hid set --cmd "ch9329 -d
> <uart>"`), with no device-specific code in `cli/`. It exposes the same CLI
> surface as the KB2040 `hidrig`, translating each
> [HID serial protocol](../docs/dev/hid-serial-protocol.md) command into the binary frames
> below. Both one-shot injection and the `serve`/`stop` KVM daemon (for
> `panio console`) are implemented and hardware-verified. The facts below
> remain the clean-room reference the implementation is built from.

**Device:** WCH CH9329 UART→USB-HID bridge, behind a CH340 USB-serial
bridge in the Openterface Mini-KVM. The host sends framed serial commands; the
CH9329 emulates a USB HID keyboard/mouse to the *target*.

All facts below derive from the WCH **CH9329 Datasheet (DS1, V1.1)** and the WCH
**"CH9329 serial communication protocol" (V1.0)** — restated in original words.
No application/firmware source (incl. the GPL Openterface app) was reproduced.

## 1. Frame format

Same layout host→chip and chip→host: `HEAD(2)=57 AB` · `ADDR(1)=00` ·
`CMD(1)` · `LEN(1)=#data` · `DATA(0..64)` · `SUM(1)`.
`SUM = (57 + AB + ADDR + CMD + LEN + ΣDATA) & 0xFF`.
Chip normal response CMD = `request | 0x80`; error response CMD = `request | 0xC0`.
Master/slave: host is master, 500 ms response timeout; default 3 ms inter-byte
gap ends a packet.

Example — press 'A' (usage 0x04): `57 AB 00 02 08 00 00 04 00 00 00 00 00 10`.

## 2. Baud / framing

Power-on default **9600**; supported 1200…**115200**; framing **8N1**.
115200 requires 5 V supply (not guaranteed at 3.3 V). A factory/unconfigured chip
is at 9600 — do not assume 115200; detect it.

Vendor-configured rates observed on real hardware: Openterface Mini-KVM **115200**,
Sipeed NanoKVM-USB **57600** (hardcoded as `SERIAL_BAUD_RATE` in the vendor's
`browser/src/libs/device/serial-port.ts`). `ch9329` auto-detects 115200, 57600,
then 9600.

## 3. GET_INFO — `0x01` → `0x81`, 8-byte response

Request `57 AB 00 01 00 03`. Response payload: `[0]` chip version (`0x30+minor`);
**`[1]` USB enumeration status: 0x01 = target enumerated the HID, 0x00 = not**;
`[2]` lock LEDs (bit0 Num, bit1 Caps, bit2 Scroll); `[3..7]` reserved.

## 4. Keyboard report — `0x02` → `0x82`

`LEN=0x08`, DATA = USB boot-keyboard report: `[0]` modifier bitmask, `[1]`
reserved (0x00), `[2..7]` up to 6 HID usage codes. Modifier bits: 0 L-Ctrl,
1 L-Shift, 2 L-Alt, 3 L-GUI, 4 R-Ctrl, 5 R-Shift, 6 R-Alt, 7 R-GUI. Response =
1 status byte (`0x00` = success). (Multimedia keys `0x03`/`0x83`; mouse abs
`0x04`, rel `0x05`.)

## 5. Parameter config — `0x08` (get) / `0x09` (set), 50-byte block

`GET_PARA_CFG 0x08` → `0x88` + 50-byte block. `SET_PARA_CFG 0x09` (LEN 0x32) →
`0x89` + status. Block offsets that matter:

| Offset | Field | Notes |
|---|---|---|
| 0 | Working mode | set `0x00–0x03`; `0x00`=KB+mouse+HID, `0x02`=KB+mouse |
| 1 | Serial comm mode | set `0x00`=protocol (required), `0x01`=ASCII, `0x02`=transparent |
| 2 | Serial address | default `0x00` |
| 3..6 | **Baud (big-endian)** | 9600=`00 00 25 80`; **115200=`00 01 C2 00`** |
| 9..10 | Packet interval ms | default 3 |
| 11..14 | USB VID/PID | default VID 0x1A86 |
| … | (ASCII-mode/USB-string fields) | offsets >9 derived — verify by readback |

**Procedure to set 115200:** `GET_PARA_CFG` → keep all 50 bytes → set offset 0=`0x00`,
offset 1=`0x00`, offset 3..6=`00 01 C2 00` → recompute SUM → `SET_PARA_CFG`. Config
**persists to flash** and **activates only after reset/power-cycle** — then reopen the
host port at 115200.

## 6. Reset

- `CMD_RESET 0x0F` → `0x8F`: reset chip (re-open at configured baud).
- `CMD_SET_DEFAULT_CFG 0x0C` → `0x8C`: restore factory defaults (baud→9600); follow
  with `CMD_RESET`, reopen at 9600.
- **Hardware factory reset = `DEF` pin (pin 10) low >3 s, release, ~200 ms → 9600.**
  RTS→DEF is a *board wiring* possibility, **not** a chip fact — **verify on hardware**
  (our RTS pulse did not recover a wedged chip; treat physical replug as the recovery).
- `SET` pin (pin 11) low forces protocol mode regardless of config.

## 7. Working modes

Framed protocol works only in **serial comm mode 0 (protocol mode)** — the power-on
default. ASCII/transparent modes type raw bytes instead of parsing frames.

## Init + baud-upgrade sequence

1. Open at current baud (try 9600, 57600, then 115200) and `GET_INFO`; a `0x81` reply
   confirms the baud and reports USB-enumeration + version.
2. If not already 115200: `GET_PARA_CFG`, set baud 3..6=`00 01 C2 00`, mode/serial=0,
   `SET_PARA_CFG` (expect `0x89`/`0x00`), `CMD_RESET` (expect `0x8F`/`0x00`).
3. Re-open at 115200, `GET_INFO` again; require enumeration byte `0x01` before typing.
4. Per char: press frame then all-zero release frame; check each `0x82` status, small
   inter-frame gap.

## ACK / status codes

`0x00` SUCCESS · `0xE1` TIMEOUT · `0xE2` HEAD · `0xE3` CMD · `0xE4` SUM · `0xE5` PARA
· `0xE6` OPERATE.

## Confidence

High: frame/checksum, baud table, GET_INFO, keyboard report, command codes, config
offsets 0–9, persistence/reset-to-activate, serial reset codes, DEF-pin reset, modes.
Medium (verify on hardware): config offsets >9; baud-field readback. Low: RTS→reset
mapping (board-specific, inferred).

## Canonical references

- **CH9329 Datasheet (DS1, V1.1)** — pinout, modes, baud, framing, DEF/RST/SET pins,
  factory-reset timing, persistence. `https://www.wch-ic.com/downloads/CH9329DS1_PDF.html`
  (English mirror: `https://akizukidenshi.com/goodsaffix/ch9329.pdf`).
- **CH9329 serial communication protocol (V1.0)** — frame format, checksum, command/
  response tables, payloads, status codes, key-code appendix. In WCH `CH9329EVT.ZIP`
  (`https://www.wch.cn/downloads/CH9329EVT_ZIP.html`).
- **CH9329 config tool** (field sanity-check): `https://ch9329.ayufan.dev/`.

**Clean-room attestation:** no source code reproduced; vendor-datasheet facts only.
