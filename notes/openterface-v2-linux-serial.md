<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Openterface Mini-KVM **V2** (CH32V208) — why its control port is silent on Linux

*Status: **bench-measured 2026-09-20** on a bare-metal x86_64 Linux host
(Ubuntu 26.04, kernel 7.0), against paniolo 0.4.1 installed from the apt repo.
The chip never answered on Linux under any condition tested. The **same unit,
same cable, moved to a macOS host, answered on the first try and stayed
reliable** — so the hardware is healthy and this is a platform gap, not a
device fault. The root cause on the Linux side is **not yet established**.*

> **This note corrects two claims made elsewhere in these notes.** See
> "What we had wrong" below before trusting
> [openterface-deep-control.md](openterface-deep-control.md) or
> [openterface-usb-mux-spec.md](openterface-usb-mux-spec.md) on the subject of
> USB IDs.

## Sources

All citations are to **TechxArtisanStudio/Openterface_QT** — despite the chip
naming, that repository is titled *"Openterface Mini-KVM: Host Applications for
Windows and Linux"* — pinned at commit `0536836c0d6d` (2026-09-20). It is
GPL-3.0; everything below is a description of observable behavior and of facts
the vendor states in prose, not copied implementation.

| what | where |
|---|---|
| `FE0C` is the **V2 Mini-KVM** serial PID, not a KVM-Go marker | `device/platform/DeviceConstants.h` (`SERIAL_PID_V2`) |
| chip selected by VID/PID | `serial/chipstrategy/ChipStrategyFactory.h` (`CH32V208 = 0x1A86FE0C`) |
| the V2 chip is **fixed at 115200** | `serial/chipstrategy/CH32V208Strategy.cpp` (`supportedBaudrates()` returns one rate; `validateBaudrate()` warns and overrides anything else; `buildReconfigurationCommand()` returns empty — no command-based reconfiguration) |
| **close/reopen wedges the chip** | `docs/hotplug-serial-fix.md` |
| a separate Linux-only enumeration failure and its remedy | `host/UsbPortResetter.h` |
| the tty mapping and the udev rules the vendor ships | `docs/tutorial/06-platform-guides.md`, `packaging/debian/postinst` |

## The hardware in hand

Physically a Mini-KVM (confirmed by eye), enumerating as:

```
Host USB-C ── HUB1 (1a40:0101) ─┬─ 345f:2109  MACROSILICON   (MS2109 video)
                                └─ 1a86:fe0c  "USB Serial"   (CH32V208 → /dev/ttyACM0)
Target USB-C ─ HUB2 (1a40:0101) ── 1a86:fe00  "KeyMod"       (emulated HID)
```

Note `/dev/tty**ACM**0`, not `/dev/ttyUSB*`: this is a CDC-ACM device on
`cdc_acm`, not a CH340 on `ch341`. Commands written for the V1 board do not
transfer verbatim.

## What we had wrong

1. **`1A86:FE0C` does not mean "KVM-Go".** `openterface-usb-mux-spec.md` §B.4
   states the runtime discriminator as *"`1A86:FE0C` (CH32V208, KVM-Go)"*, and
   `openterface-kvm-go.md` presents that PID pair as the KVM-Go's signature.
   The vendor's own constant is named `SERIAL_PID_V2`: it marks a **hardware
   revision**, and a Mini-KVM shell can contain it. The PID still selects the
   *chip strategy* correctly; it does not identify the *product*.
2. **"No CH340, therefore this is the successor board"** was the wrong
   inference, and it is the same shape of error the deep-control notes already
   corrected once for the modem lines.

## Why the port is silent

The vendor's `docs/hotplug-serial-fix.md` gives the mechanism as its first root
cause: **force-closing and reopening the serial port drives the CH32V208 into an
unusable state.** Their described cascade: the chip takes an unexpected
interruption, `CMD_RESET` then goes unanswered, the app reports *"the device
does not recognize the command"*, **recovery via an RTS hardware reset fails**,
and the port still reports Connected while the chip is dead. Each further
connect attempt repeats the close/reopen and makes it worse.

Two things do exactly that on Linux, before any deliberate use:

- **ModemManager.** The CDC interface advertises `bInterfaceProtocol 1`
  (AT-commands), so MM opens the port and probes it with AT strings on every
  enumeration. Observed here: MM probed and released it, logging *"not supported
  by any plugin"*. The probe is itself the poison.
- **paniolo's own baud autodetection.** `ch9329`'s `Session::open()` opens the
  port, sends `GET_INFO`, closes, and reopens at the next candidate — the
  autodetect loop *is* a close/reopen loop, up to three per invocation.

## Measurements

All against a device that enumerated cleanly every time, with the target side
attached and powered (`KeyMod` present, so the MCU was running).

| probe | result |
|---|---|
| `ch9329 info`, autodetect | timeout |
| `ch9329 -b <rate> info`, 3× at each of 115200 / 57600 / 9600 | **0/9** |
| raw `57 AB 00 01 00 03` (GET_INFO) via termios at 115200 | no reply |
| same, with RTS cleared after open, 1.0 s and 2.5 s settle | no reply |
| same, with RTS **and** DTR cleared | no reply |
| raw keyboard frame, watching the target's own `/dev/input` event node | no keystroke |
| `USBDEVFS_RESET` on the serial device, then re-probe 3× | no reply |
| physical unplug/replug of both cables, then re-probe | no reply |

The raw frame failing identically to the helper is what rules the helper out:
nothing on that port was answering. `RTS` clearing failing is consistent with
the vendor's statement that RTS recovery does not work once the chip is in this
state — it is not evidence against the RTS wiring.

## The macOS control — the measurement that settles it

Moved to a macOS host, the identical unit enumerated as `/dev/cu.usbmodem51201`
and answered immediately:

```
chip_version=0x02 target_connected=true num_lock=false caps_lock=false scroll_lock=false baud=115200
```

| probe (macOS) | result |
|---|---|
| `ch9329 -b 115200 info`, single open | answered first try |
| `ch9329 info`, autodetect, 6 runs | **6/6** |
| forced 9600, then forced 115200, 4 cycles | **4/4** — no poisoning |

Two things follow, and the second was a surprise:

1. **The device is fine and autodetect does not harm it — on macOS.** The
   close/reopen wedging the vendor documents did not reproduce here at all.
   Note the caveat on the 6/6: 115200 is autodetect's *first* candidate and this
   chip uses it, so those runs succeeded on the first open and never exercised
   the reopen path. The 4-cycle test was written to force that path.
2. **Baud is a no-op on this device.** The forced-9600 probe, which the test
   expected to fail, *answered* — every cycle. That is correct CDC-ACM
   behavior: the line rate is a class request the firmware may ignore, and the
   data rides bulk endpoints regardless. `openterface-usb-mux-spec.md` suspected
   this ("whether the CDC firmware honours the requested line rate at all —
   likely irrelevant"); it is now measured. It also contradicts
   `CH32V208Strategy::supportedBaudrates()`, which reports a single supported
   rate: that constraint is the app's policy, not the device's behavior.

**A wrong baud therefore cannot be a failure mode on a CDC-ACM Openterface**,
which makes baud autodetection there not merely unnecessary but meaningless.

## What this means for paniolo

1. **Openterface V2 is effectively unsupported on Linux today.** That is the
   headline. It is not a paniolo bug: a hand-built `GET_INFO` frame written
   straight to the port through raw termios fails exactly as the helper does, so
   the problem lives in the Linux CDC-ACM open path, not in this codebase.
2. **Do not autodetect baud on a `1A86:FE0C` device** — but for a better reason
   than the one first written here. Baud is a no-op on CDC-ACM (measured above),
   so the loop cannot ever be *needed*; the helper should branch on the port's
   VID/PID as `ChipStrategyFactory` does and open once. The stronger claim in
   the first draft of this note — that the probing *is what breaks the chip* —
   is **not supported**: autodetect is clean on macOS, 6/6. The vendor's wedging
   mechanism stays on the list of Linux suspects, unproven.
3. **#84's 300 ms settle addresses a loop that should not run** on this
   hardware. It is not wrong, it is just aimed elsewhere.
4. **Linux needs ModemManager kept off the port**, via a udev rule setting
   `ID_MM_DEVICE_IGNORE=1` for `1a86:fe0c` (and `1a86:7523` for V1). The vendor
   ships udev rules for permissions (`TAG+="uaccess"`); the MM exclusion is
   ours to add. Worth shipping in the `.deb` postinst.
5. **`host/UsbPortResetter.h` documents a second, distinct Linux-only failure**:
   the CH32V208 failing to *enumerate* after a target restart (USB `-71`
   EPROTO), recovered by a hub port reset. That is not what was seen here — the
   device always enumerated — but it is the same chip misbehaving on the same
   platform, and it is a no-op on Windows and macOS, which is consistent with
   this whole class of problem being Linux-specific.

## Open

- **The confirming probe was run on 2026-09-20, and it FAILED.** With the udev
  rule verified active on the node (`ID_MM_DEVICE_IGNORE=1`), no process holding
  the port, and no ModemManager probe in the log, a **single** open at a forced
  115200 still drew no reply in three frames. So keeping ModemManager off the
  port is necessary hygiene but is **not** sufficient, and the wedging mechanism
  in `docs/hotplug-serial-fix.md` does not by itself explain this unit.
- **A host-side replug may not power-cycle the MCU.** The deep-control notes
  record that both VBUS rails feed the board through CH213K current limiters, so
  it "survives target power cycles while the host side is connected" — the
  converse should hold too. Any test that depends on a cold MCU must have **both**
  cables out at once; a host-only replug leaves it powered from target VBUS.
  Every "power cycle" above is suspect on this ground.
- **Cold MCU, tested 2026-10-11: still silent.** A V2 Mini-KVM (`1a86:fe0c`;
  possibly the same unit as above, which was not recorded) that had answered
  on macOS earlier the same day
  (`chip_version=0x02 target_connected=true baud=115200`), was moved to an
  x86_64 Linux host (Ubuntu 26.04, kernel 7.0, paniolo 0.7.0) with the
  target side cabled to a running board. Results:

  | probe (Linux) | result |
  |---|---|
  | `ch9329 info`, autodetect | timeout |
  | raw `GET_INFO` via termios at 115200 / 57600 / 9600 | no reply |
  | raw `GET_INFO`, DTR/RTS set to each of the four combinations | no reply |
  | host-cable replug, two separate times | no reply |
  | moved to a different host USB controller and hub | no reply |
  | **both cables unplugged together** (kernel logged the disconnect, re-enumeration about 12 s later), then `ch9329 info` | **no reply** |

  `ID_MM_DEVICE_IGNORE=1` was set on the node, no other process held the port,
  and the device enumerated as a normal two-interface CDC-ACM (`cdc_acm` on
  both). The MS2109 capture on the same unit worked throughout. So a cold MCU
  does not fix it either: the unit works on macOS hours apart from failing on
  Linux, which again points at the Linux host side, not the board.
  What is still untested: a USB protocol capture of the Linux open (to see
  whether the device ever returns bulk-IN data, or the host never sends the
  CDC requests macOS sends).
- **Issue #81 cannot be tested on this unit as written.** Its repro targets a
  factory-baud CH9329 behind a CH340 at `/dev/ttyUSB2`; none of that exists on
  V2 hardware. Whether the settle fix works on a real V1 board is still unproven.
