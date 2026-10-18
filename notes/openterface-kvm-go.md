<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Openterface KVM-Go — architecture and panio support

*Status: **bench-verified 2026-08-24; microSD mux driven from software
2026-08-30.** Both panio channels work with no code changes, and the microSD
mux is now switchable over the existing control port — protocol in
[openterface-usb-mux-spec.md](openterface-usb-mux-spec.md), bench results in
"microSD mux switching" below. Sibling of the
[Mini-KVM deep-control notes](openterface-deep-control.md), which this device
supersedes in several important ways.*

The [Openterface KVM-Go](https://openterface.com/product/kvm-go/) is the
successor to the Openterface Mini-KVM: a keychain-sized KVM-over-USB with a
built-in male video connector (HDMI, DisplayPort, or VGA — this doc covers the
HDMI variant). Hardware design files are published under CERN-OHL-S v2 at
[TechxArtisanStudio/Openterface_KVM-GO_Hardware](https://github.com/TechxArtisanStudio/Openterface_KVM-GO_Hardware).

**Source discipline** (updated 2026-08-30): everything below comes from the
open *hardware* design files, the vendor datasheet, the vendor's published
documentation, or our own bench probing and USB captures — with one deliberate
exception. The mux protocol in
[openterface-usb-mux-spec.md](openterface-usb-mux-spec.md) was obtained by a
**clean-room investigation**: a separate investigator context read the vendor
host applications and returned facts and mechanism prose only, and panio's
implementation was written from that report rather than from the applications.
No vendor source, identifier, or code structure crossed into the implementation
context, and the report was leak-scanned before it did. That is the sanctioned
route, not a relaxation of the wall — reading those applications directly into
an implementation context remains forbidden. They are AGPL-3.0, except
`Openterface_Core`, which carries **no licence at all** and is therefore more
encumbered, not less.

## Bottom line for panio

| Channel | Helper | Status |
|---|---|---|
| `video` | `hdmicap` (AVFoundation / UVC) | ✅ works unmodified |
| `hid` | `ch9329` | ✅ works unmodified |
| microSD | `ch9329` | ✅ switchable in software (serial `0x17`), hardware-verified |

Wiring it into a target needs nothing new:

```bash
panio video set -t <target> --device "<uniqueID from hdmicap devices>"
panio hid   set -t <target> --cmd "ch9329 -d /dev/cu.usbmodemXXXXX"
```

## Architecture

Two boards joined by headers H1/H2 — `KVM_Go_HDMI` (video) and `KVM_Go_KM`
(keyboard/mouse + storage):

```
Host USB-C ─┬─ HUB1 (SL2.1s) ─┬─ CH32V208  USB1 → CDC-ACM control (1a86:fe0c)
            │                 └─ FSUSB42MUX side A ─┐
            └─ MS2130S ─── SuperSpeed direct to host │  (UVC video + UAC audio)
                                                     ├── GL823K ── microSD
Target USB-C ── HUB2 (SL2.1s) ─┬─ CH32V208 USB2 → HID "KeyMod" (1a86:fe00)
                               └─ FSUSB42MUX side B ─┘
```

USB identities observed on the bench:

| Role | Product string | VID:PID | Where |
|---|---|---|---|
| Host control | `USB Serial` (CDC-ACM) | `1a86:fe0c` | behind HUB1 |
| Host video | `Openterface` (UVC + UAC + HID cfg) | `345f:2132` | SuperSpeed, direct |
| Host storage | `USB Storage` (GL823K) | `05e3:0751` | behind HUB1, when mux'd host-side |
| **Target** | `KeyMod` (emulated kbd/mouse) | `1a86:fe00` | behind HUB2 |

> **These IDs are not exclusive to the KVM-Go (2026-09-20).** A V2
> Mini-KVM enumerates the same `1a86:fe0c` / `1a86:fe00` pair alongside an
> MS2109 rather than an MS2130S. Identify the *chip* by PID; do not infer
> the *product* from it. The Linux-specific silence seen on V2 hardware,
> and its cause in the vendor's own sources, are in
> [openterface-v2-linux-serial.md](openterface-v2-linux-serial.md).
| Both hubs | `USB2.0 HUB` (SL2.1s) | `1a40:0101` | — |

Key parts (from the published BOMs):

- **U10 `MS2130S`** (video board) — USB 3.x UVC capture, replacing the
  Mini-KVM's USB 2.0 MS2109. Config storage moved from an AT24C16 I²C EEPROM to
  a **W25Q16 SPI NOR** (U4).
- **U1 `CH32V208GBU6`** (KM board) — one RISC-V MCU does *both* the host CDC
  control interface and the target HID. Replaces the Mini-KVM's CH340C +
  CH9329 pair.
- **HUB1/HUB2 `SL2.1s`** — same hub as the Mini-KVM, one per side.
- **U4 `GL823K`** + `CARD1` — onboard microSD reader on an **`FSUSB42MUX`**
  (U5), switchable between host and target.
- Extras: **`DS18B20`** 1-Wire temperature sensor (U8), an RGB LED, and a
  **BLE antenna** (the CH32V208 has BLE 5.3 on-die; the vendor lists a future
  native iPadOS app). `SW1` is wired to `BOOT0` — ISP entry at reset, mux
  toggle at runtime.

## Differences from the Mini-KVM that matter

1. **No CH340 — but the modem lines still bite.** The Mini-KVM's specific
   `DTR → SW_GND` (A-port disconnect) and `RTS → HIDRESET` wiring — items 2 and
   3 in the [deep-control notes](openterface-deep-control.md) — does not exist
   here. DTR/RTS arrive as a CDC class request (`SET_CONTROL_LINE_STATE`) the
   firmware is free to act on, and **corrected 2026-08-30: it does act on RTS,
   which is a hardware reset of the CH32V208** (see "RTS resets the MCU"
   below). The original upside still holds — opening the control tty does not
   yank a USB device off the bus, which was the load-bearing constraint on the
   Mini-KVM — but "no CH340, therefore the lines are inert" was the wrong
   inference to draw from it.

2. **The USB mux select is on the MCU, not the video chip.** The KM schematic
   labels it plainly: `USB_SW` → FSUSB42 `Sel`, driven by the CH32V208.
   Also `SDPOWER_SW` → SY6280 enable (SD power) and `SD_STATE` ← GL823K
   activity. On the Mini-KVM the equivalent `Sel` hangs off the MS2109 GPIO,
   which is what dead-ended **OTF-3** — though that turned out on 2026-08-30 to
   be reachable after all, by a register write rather than the firmware patch we
   had assumed (see the correction at the top of the deep-control notes). Here
   it sits behind a serial port panio already talks to.

3. **The switched device is onboard** — a microSD reader rather than an
   external USB-A port. True hands-free virtual media with no stick to plug.
   The vendor documents the behaviour: host and target share the card but
   never simultaneously, it **defaults to the host at power-on**, and it can be
   switched by the physical button or by the host app.

4. **SD power is gated from *target* VBUS** (`TVBUS` → SY6280AAC, enabled by
   `SDPOWER_SW`). Bench-confirmed: the card reader only appears once the target
   side is powered.

## The CH9329 protocol works, with one difference

The CH32V208 speaks the CH9329 frame protocol, so the existing
[`ch9329`](../ch9329/README.md) helper drives it as-is:

```
$ ch9329 -d /dev/cu.usbmodem51201 info
chip_version=0x01 target_connected=true num_lock=false caps_lock=false scroll_lock=false baud=115200
```

`chip_version` reports **`0x01`**, not the real CH9329's `0x38` — the MCU
emulates the protocol rather than being the chip. `target_connected` is live
and accurate: it flips to `true` the moment the target enumerates the emulated
HID.

**Verified end-to-end into a Raspberry Pi 5** via the lock-LED round trip —
sending `CAPS_LOCK` flipped `caps_lock` `false` → `true` → `false`, proving the
keystroke reached the target's USB HID stack and the LED state came back over
the CDC link. `move`, `moveabs`, `key`, and `combo` all work.

## There is no proprietary initialization

This was verified directly rather than assumed. A Cynthion capture of the
vendor Linux app driving the device — 5.4 M packets covering enumeration,
app start, and a working picture — contains **zero vendor-type control
requests**:

```bash
tshark -r capture.pcap -Y "usb.bmRequestType.type == 2"   # → 0 hits
```

Everything the app does is standard USB: hub port class requests, MSC
`GET_MAX_LUN`, UVC control enumeration (`GET_INFO`/`GET_DEF`/`GET_CUR`/
`GET_MIN`/`GET_MAX`/`GET_RES`), UVC `VS_PROBE`/`VS_COMMIT`, HID `SET_IDLE`,
and CDC `SET_LINE_CODING`/`SET_CONTROL_LINE_STATE`.

The committed video format was:

```
bmHint=0x0000  bFormatIndex=1  bFrameIndex=1
dwFrameInterval=333333        → 30.000 fps
dwMaxVideoFrameSize=4147200   = 1920×1080×2
dwMaxPayloadTransferSize=16384
```

**Consequence:** the capture chip is a plain UVC device. panio needs no
reverse-engineered handshake, and the clean-room wall never has to be crossed
for the video path.

Incidental CDC observations from the same capture: the app sets
`SET_LINE_CODING` to 9600 8N1, later to 115200 8N1, and asserts DTR+RTS
(`SET_CONTROL_LINE_STATE wValue=0x0003`). Consistent with `ch9329`'s existing
115200/57600/9600 autodetect; nothing to change.

## Operational notes

### A blank capture is usually a sleeping display

**Check this first.** A target whose display has blanked and a capture chip
with no HDMI input are indistinguishable downstream — both yield a flat field
(this device fills black on some links and uniform grey on others). Wake the
target over the `hid` channel *before* investigating the capture path:

```bash
ch9329 -d <dev> move 60 40 ; ch9329 -d <dev> move -60 -40
ch9329 -d <dev> key LEFT_SHIFT
```

During bring-up this cost most of a session and produced three separate wrong
diagnoses (a required vendor init, 4K mode selection, and a NV12-vs-YUY2
format mismatch) before a single mouse wiggle produced `signal=stable` and a
clean 1080p screenshot. `ch9329 info` reporting `target_connected=true`
confirms the wiggle will actually land.

### When the signal goes away, frames stop

Unlike the MS2109 — which streams black frames when idle — this device stops
delivering frames entirely when the HDMI source disappears (display sleeps,
cable pulled, device unplugged). `hdmicap`'s watchdog treats that as a fault:

```
capture stalled (4s with no new frames) — exiting for restart
no frames in 12s after device open — exiting for restart
```

The daemon then exits. Under panio supervision this becomes a restart loop
for as long as the target's screen is asleep. It is *correct* fault detection —
50 minutes of continuous clean streaming was observed once a signal was
present — but the no-signal state arguably wants to be a state rather than a
fatal stall for this device. **Not yet addressed.**

### USB 2.0 fallback

If the capture chip enumerates at High Speed rather than SuperSpeed, the
advertised mode set shrinks sharply — max 1920×1080, no 4K, no 120/144/240 fps,
no `yuvs` at 1080p — and it appears *behind the internal hub*
(`locationID 0x514…`) instead of on its own SuperSpeed root port
(`0x520…`). This is the MS2130S falling back to the USB 2.0 video lines that
run through header H2 to the host-side hub, exactly as the schematic describes.

Usual cause is a **USB 2.0-only USB-C cable**. It is also expected and
unavoidable when the device is routed through a Cynthion, which is USB 2.0
only. 1080p is fine for console work; use a USB 3 cable on a SuperSpeed port to
regain the full mode set.

### 4K is experimental

The vendor lists 4K as an experimental feature that "may generate additional
heat", and ships a documented default of **1080p60 for optimal stability**.
`hdmicap`'s `pick_format()` selects the highest pixel count available, so on a
SuperSpeed link it *will* choose 3840×2160. That has not been shown to cause a
problem, but it is an accidental default rather than a considered one, and
worth a deliberate decision before relying on it.

## microSD mux switching (2026-08-30)

One CH9329-family serial command on the existing CDC control port drives the
mux; frames, reply semantics and the capability gate are in
[openterface-usb-mux-spec.md](openterface-usb-mux-spec.md). Recorded here is
what this bench established that a source investigation could not.

**Verified end to end.** Query, switch-to-target and switch-to-host were each
exercised against our unit with the host side on a Linux box and the target side
on a Mac. The reply's status byte matched the requested position every time, and
the media genuinely moved: a nonce file written on one side read back
byte-identical on the other, filesystem intact. A reply alone proves nothing —
it reports the resulting position, so an ignored request still yields a
well-formed frame stating the old one.

**RTS resets the MCU — deassert it explicitly on open.** This is the correction
that matters most, because the assumption in the original architecture notes was
that with no CH340 on this board the modem lines would be inert. They are not:
RTS is a hardware reset of the CH32V208. Many serial stacks assert RTS and DTR
at open by default. Brief assertion appeared harmless across roughly a dozen
opens here — every `GET_INFO` answered normally — but that is an observation,
not a guarantee, and the vendor's own reset path holds RTS for four seconds.

**The mux resets to the host side on power loss.** Observed directly: after the
unit was unplugged and re-cabled, a card that had been switched to the target
came back host-side. Nothing persists the position, so `state` must be read, never
assumed from the last `attach-*` issued.

**The physical button flips the mux in firmware**, with no host software running
anywhere — proven before any command was known. This is the opposite of the
Mini-KVM's slide switch, which is only *monitored*. It means the MCU already
owns the flip path and the serial command merely triggers it.

**A flip does not disturb the control channel.** `ch9329 info` answered normally
with `target_connected=true` immediately after switching, and the receiving side
enumerated the reader cleanly with nothing in `dmesg`. None of the
re-enumeration storms the Mini-KVM showed under VM passthrough appeared here.

**There is no card-presence state to read.** `GET_INFO` was byte-identical
across ~120 s spanning both mux positions, and the investigation confirms why:
no card-presence, "no card", or transfer-active state exists anywhere in the
protocol, over either transport. The LED must drive those from firmware-local
`SD_STATE`. Do not look for a query that does not exist.

**macOS will not release the card on request.** `diskutil unmount` failed
reproducibly, dissented by Spotlight (`mds` / `mds_stores`) indexing the freshly
formatted volume. Both test flips therefore happened with the volume still
mounted, and the exFAT filesystem survived intact — helped by the reader
reporting `Write cache: disabled`. Surviving twice is not a guarantee: a macOS
unmount path needs `diskutil unmount force`, or a Spotlight exclusion.

## Not yet done

- [x] Drive the microSD mux from panio (`USB_SW` via the CH32V208) — **done
      2026-08-30**, and it needed no USB capture. A clean-room investigation of
      the vendor applications produced the command directly; see
      [openterface-usb-mux-spec.md](openterface-usb-mux-spec.md).
- [ ] Decide `hdmicap` format-selection policy (see "4K is experimental").
- [ ] Decide whether the no-signal stall should exit or become a reported
      state.
- [ ] Read the DS18B20 temperature sensor and RGB LED, if the MCU exposes them.
- [ ] Characterise the BLE interface (vendor lists a future iPadOS app).
- [x] Confirm whether `SET_CONTROL_LINE_STATE` (DTR/RTS) has any hardware
      effect on this design, as it did on the Mini-KVM — **it does, and RTS is
      the dangerous one.** See "RTS resets the MCU" below. DTR's effect here is
      still unverified.
