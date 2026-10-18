<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Openterface Mini-KVM deep control — findings & testing TODO

*Status: **bench-verified 2026-08-18** as far as this bench allows. DTR→`SW_GND`
and RTS→`HIDRESET` are both characterized; `DATAFLIP` is not observable from the
host; the EEPROM is dumped; the slide-switch position is readable without a
firmware patch at XDATA `0xDF00` (finding 9). Driving the mux is still blocked
on the MS2109 firmware-patch wall (finding 7), with two untried leads in
finding 8. Tracker: requirements §6.1 (OTF-1…6).*

> **Correction (2026-08-30): OTF-3 is not blocked, and the firmware-patch wall
> was never the vendor's path.** A clean-room investigation of the vendor host
> applications shows they drive this mux with **no 8051 patching at all** — a
> read-modify-write of XDATA `0xDF01` (the byte immediately after the `0xDF00`
> slide-switch byte of finding 9) over the same MS2109 HID config interface this
> bench already reads from successfully, using XDATA opcodes `0xB5` and `0xB6`.
> Findings 7 and 8 remain accurate about what *ms-tools* cannot do; they were
> wrong only in inferring that the vendor needed it. We stopped one address
> short. Protocol and the firmware-dependent bit selection:
> [openterface-usb-mux-spec.md](openterface-usb-mux-spec.md). Untested on this
> hardware — the Mini-KVM was not on the bench when this was established.

> **This doc is about the Mini-KVM.** For its successor see
> [Openterface KVM-Go](openterface-kvm-go.md), which uses a different chipset
> (MS2130S + CH32V208 instead of MS2109 + CH340C/CH9329). Findings 2 and 3
> **do not carry over**: there is no CH340 on that board, so the
> `DTR`→`SW_GND` and `RTS`→`HIDRESET` modem-line behaviours don't exist there.
> The USB mux `Sel` also moved off the MS2109 GPIO onto the MCU, which makes
> the **OTF-3 equivalent reachable** on that device.
>
> **Correction (2026-09-20): the USB IDs do not identify the product.**
> A board that is physically a Mini-KVM can carry the CH32V208 and
> enumerate `1a86:fe0c` + `1a86:fe00` — the vendor names that constant
> `SERIAL_PID_V2`, a *revision* marker. On such a V2 unit the control port
> is a CDC-ACM `/dev/ttyACM*`, it is fixed at 115200, and baud
> autodetection actively wedges the chip. See
> [openterface-v2-linux-serial.md](openterface-v2-linux-serial.md).

The Openterface Mini-KVM's hardware is open source
([TechxArtisanStudio/Openterface_Mini-KVM_Hardware](https://github.com/TechxArtisanStudio/Openterface_Mini-KVM_Hardware),
CERN-OHL-S v2). Reading the v1.9 schematic, BOM, and datasheet shows the
device is considerably more automatable than "capture + HID": the switchable
USB-A port and the serial chip's modem lines form a small programmable USB
fixture. This doc records the findings and the bench tests needed before
panio grows verbs for them.

**Source discipline:** everything below comes from the open *hardware* design
files, the vendor datasheet, the vendor's published product documentation, or
our own bench probing. The Openterface host applications were not read
(copyleft).

**Measurement caveat — read before trusting any USB observation.** The lab
host (`dev`) is a QEMU/KVM VM, and the Openterface reaches it by *per-device*
USB passthrough (`qm config 102`: `host=1-1.1.2.1/.2/.3`). Passthrough puts
each forwarded device on its own emulated root port, so **the guest cannot see
the real topology** — from inside the VM the unit looks like three independent
host connections with no hub, which is an artifact. Passthrough binds by
physical port path, so genuine device changes *are* visible to the guest, but
topology, enumeration bursts, and bus-level errors are not. Ground truth is
`journalctl -k` on the hypervisor; **`dmesg` on proxmox is unusable** —
`usbhid-ups` floods it and the ring buffer holds only ~15 minutes. Guest
`dmesg` timestamps also lag `CLOCK_MONOTONIC` by ~13 s, so attribute events by
before/after counting rather than by timestamp comparison.

## Architecture (v1.9 schematic) — confirmed on hardware

Two SL2.1s USB 2.0 hubs, one per side:

```
Host USB-C ── HUB1 ──┬─ MS2109  (HDMI→UVC video + HDMI audio; vendor cfg interface)
                     ├─ CH340C  (USB-serial → CH9329; modem lines repurposed)
                     ├─ FSUSB42MUX side A ─┐
                     └─ extension pads      │   USB-A female
                                            ├── (switchable port)
Target USB-C ─ HUB2 ──┬─ CH9329 (HID)      │
                      ├─ FSUSB42MUX side B ─┘
                      └─ extension pads
```

Confirmed 2026-08-18 from the hypervisor: hub `1a40:0101` (Terminus
Technology — the SL2.1s) enumerates at `1-1.1.2`, with the MS2109 on its
port 1, the CH340 on port 2, and the switchable A-port device on port 3 — one
host cable feeding one hub, exactly as drawn.

Both VBUS rails power it through CH213K current limiters — it survives
target power cycles while the host side is connected. The target always
enumerates an SL2.1s hub in front of the CH9329 (and the A-port when
switched over).

## Findings

1. **Software-switchable USB-A port — the switch is *monitored*, not a
   control.** The A-port routes through an FSUSB42MUX between the two hubs.
   Per the vendor's
   [USB switch documentation](https://docs.openterface.com/products/minikvm/usb-switch/):
   *"The Hardware Switch, despite being physical, is monitored by software and
   does not directly control the circuit direction. Instead, the software
   interprets the switch position and manages the actual circuit switching."*
   The device **defaults to the host connection at startup**, and the software
   switch takes priority over the physical toggle (the vendor documents a
   four-state sync/out-of-sync table; inward = host, outward = target).

   Bench-confirmed 2026-08-18: with no Openterface host application running,
   moving the slide switch between "H" and "T" produced **no** electrical
   change — the A-port device stayed on the host in both positions, with zero
   USB events over 15 s of polling. This corrects the earlier schematic-derived
   reading that the mux `Sel` node is *shared* between the switch and MS2109
   GPIO: the MS2109 drives `Sel`, and the switch is an input it senses.

   Use case unchanged: hands-free physical media (image a real stick
   host-side, present it to the target — visible to BIOS, unlike streamed
   virtual media).

2. **CH340 `DTR#` → net `USB_CTRL` → MOSFET Q11 → `SW_GND`** — the A-port
   connector's ground return (p1/p3). **Confirmed on hardware**, with polarity:

   | DTR | A-port device |
   |---|---|
   | asserted (1) | disconnected |
   | deasserted (0) | present |

   Four deliberate transitions produced four matching kernel events, and eight
   further pulses behaved identically. Consequences:

   - **Merely opening the control tty unplugs the A-port device**, because the
     kernel asserts DTR (and RTS) at open; closing it plugs the device back in
     via `HUPCL`. Any `panio hid` invocation would do this. This is the
     load-bearing constraint for OTF-2/3/4: the replug and reset verbs must go
     through the existing session's owner thread, never a second `open()`.
   - **Replugged devices usually come back degraded.** The hypervisor
     negotiated *full-speed* (12 Mbps) on most ground-float replugs of a stick
     that first enumerated at high-speed (480 Mbps), with the occasional
     high-speed result — nondeterministic. A physical unplug/replug restores
     high-speed. Real bus behavior, verified host-side, not an emulation
     artifact.
   - **A pulse is not one clean replug.** The host log shows a *burst* of
     re-enumerations per pulse (four in four seconds in one case), which the
     guest coalesces into a single event.
   - **Repeated cycles destabilize the shared hub.** After ~9 cycles the
     hypervisor logged `device not accepting address, error -71` and
     `device descriptor read/64, error -71` storms on the A-port device, and
     the *sibling* CH340 on the same hub simultaneously began failing control
     transfers in the guest (`failed to send control message: -71`, then `EIO`
     on the tty). Neither USB re-authorize nor a root-port disable cycle
     recovered it; a physical replug restored everything with no lasting
     damage. Mechanism unconfirmed — plausibly hub-level disruption from the
     brownout storm — but the practical point stands: **this primitive can
     take down the control channel panio needs for HID.**

3. **CH340 `RTS#` → net `HIDRESET` → CH9329 reset/config pin** (p3).
   **Confirmed on hardware.** An RTS-low pulse resets the chip; recovery is
   strikingly deterministic:

   | RTS-low hold | Result | Recovery |
   |---|---|---|
   | 20 ms | no reset | — |
   | 50 ms | reset | ~747 ms |
   | 100 ms ×3 | reset | ~696–698 ms |

   So the **reset threshold sits between 20 and 50 ms**, and the chip needs
   **~700 ms to boot** before it answers again. The A-port device stayed
   attached across every pulse, satisfying OTF-2's requirement that the reset
   not disturb it. This makes the watchdog action actionable: pulse RTS low
   for ≥ 50 ms, then allow ≥ 800 ms before the first command.

   Note this unit's CH9329 runs at **9600 baud**, the factory rate, not the
   115200 Openterface default — relevant because the helper's autodetect is
   unreliable against a factory-baud part
   ([#81](https://github.com/curtisgalloway/panio/issues/81)).

   **`DATAFLIP` is not observable from the host.** The schematic reading has a
   third line running to the CH9329 `DEF`/`UP` pins, but sampling all four
   CH340 modem inputs at ~200 Hz across a baseline window *and* a full reset
   cycle (1,956 samples) recorded **zero transitions** — CTS, DSR, CD and RI
   all sat at `0` throughout, including during the reset and boot, the one
   event guaranteed to change the chip's state. Whether it is a static strap,
   driven by the MS2109, or a misreading of the schematic cannot be settled
   from the host side. Practical consequence: OTF-2 gets **no status input
   back** from the CH9329, and `DEF`/`UP` cannot be reached via the CH340.

4. **USB descriptors in a writable AT24C16 EEPROM** hanging off the MS2109's
   config I2C (p2, with a WP line). **Confirmed**: ms-tools reports an
   `EEPROM` region of exactly 2048 bytes (16 Kbit = AT24C16). Two independent
   dumps were byte-identical (`sha256 9b46336d…`). Layout: magic `a5 5a` at
   0x00; two length-prefixed ASCII string slots at 0x10 (`0c` + "Openterface")
   and 0x20 (`0d` + "OpenterfaceA"), 0xff-padded; 8051 code from 0x30.

   **Correction to the OTF-5 premise:** the unit reports a USB serial of
   `????????`, but that is **not** an unprogrammed EEPROM field. It is a
   fully-formed USB string descriptor synthesized into XDATA at `0xC676`
   (`12 03` followed by 8× `3f 00`), immediately preceding the device
   descriptor at `0xC688` — which cross-checks exactly against sysfs
   (`bDeviceClass ef`, `bMaxPacketSize0 40`, idVendor `534d`, idProduct
   `2109`, bcdDevice `2100`). Persistent serial stamping therefore requires
   patching the firmware that populates that region, not writing a data field
   — which puts OTF-5 behind the same wall as OTF-3 (finding 7).

5. **"Extension Pins" (datasheet ⑥, "for developer use")** expose a spare
   downstream port of *each* hub plus power (p1 `PROBE_USB`). The
   target-side one is a permanent slot for a gadget the target sees
   natively — e.g. a microcontroller mass-storage gadget (true virtual
   media) or a tap for a protocol analyzer.

6. **HDMI-embedded audio** is forwarded as a USB audio device (datasheet).
   **Confirmed**: the MS2109 exposes interfaces 2 and 3 as Audio class bound to
   `snd-usb-audio`, alongside the two Video interfaces and the vendor HID
   config interface (interface 4).

7. **The MS2109 config interface is reachable, but not patchable.**
   [BertoldVdb/ms-tools](https://github.com/BertoldVdb/ms-tools) (MIT) speaks
   the chip's factory HID interface via hidraw feature reports. Build it with
   `-tags puregohid` to avoid the cgo/hidapi dependency; in that mode
   `list-dev` is unsupported and `--raw-path=/dev/hidrawN` is mandatory. It
   reads fine — `list-regions` returns `RAM 65536`, `IRAM 256`, `EEPROM 2048`,
   `USERCONFIG 48 @ RAM.CBD0`, `USERRAM 8192 @ RAM.C000`, and region reads
   work with `--no-patch`.

   **But every *active* capability requires uploading code into the running
   8051, and that upload fails on this unit.** This is one wall, not several:
   `gpio-get` and `i2c-scan` both refuse under `--no-patch` with "not
   supported in this mode", and with patching enabled the run reports
   `Could not patch code`. `uart-tx` and `dump-rom` depend on the same
   mechanism. Region reads are the only thing that works.
   Verbose logging shows the chip detected, a 14-byte patch allocated, and the
   writes genuinely landing (`ROMOut`/`ROMIn` readbacks match) — but the
   handshake byte at `0xCBD4` never changes from `0b`, so the injected code
   never executes. Expected, given the EEPROM carries vendor 8051 code rather
   than a stock dongle image. **OTF-3 and OTF-5 have no path through ms-tools as
   it stands.** The remaining clean-room option is to capture the vendor
   application's USB traffic while toggling its software switch and derive the
   command from observed bytes.

   **ms-tools runs on macOS** — the `hidraw`/`--raw-path` requirement is a
   *pure-Go-mode* limitation, not a platform one. Build it without
   `-tags puregohid` against Homebrew hidapi
   (`CGO_CFLAGS=-I/opt/homebrew/include CGO_LDFLAGS='-L/opt/homebrew/lib -lhidapi'`)
   and `list-dev` works, finding the unit on interface 4, usage page `0xff00`.
   That matters because it allows every ms-tools result here to be re-taken
   off a directly-attached host, without the passthrough caveat above.

   **Reads are size-sensitive and flaky.** Measured over hidapi on macOS:
   256-byte reads 5/6, 1 KB 2/6, 4 KB 0/6, and a whole-region read almost
   always fails. `read <region> <addr>` with the length *omitted* sometimes
   succeeds where an explicit full length errors. Dump in 256-byte chunks with
   retries — that yields a clean 64 KB XDATA image with zero failed chunks.

8. **GPIO on these chips is 8051 SFR P2/P3 — and ms-tools has a no-patch path
   for it, gated to the wrong chip.** From the ms-tools source (MIT, so
   readable, unlike the vendor host apps): `gpioUpdateSFR` reads and writes
   **P2 at SFR `0xA0`** (pin state) and **P3 at SFR `0xB0`** (direction), and
   `GPIOUpdate` uses that direct-SFR path *instead of* the 8051 patch whenever
   an `SFR` memory region exists. The ROM commands are `0xc5`/`0xc6` for SFR
   read/write (8-bit address) and `0xb5`/`0xb6` for XDATA (16-bit), carried in
   9-byte HID feature reports laid out `[reportID=0, cmd, addr…]`.

   The catch: `MemoryRegionList` only offers `SFR` when `deviceType == 2130`.
   Ours is 2109, so ms-tools never takes that path for us.

   **Probed, and the MS2109 ROM does not implement `0xc5`.** Frame layout was
   validated first against known values via the working XDATA command — `raw-cmd
   b50001` → `b5 00 01 **13**`, `b5000b` → `21`, `b5000c` → `09`, all matching
   the dumps, confirming the reply byte sits at index 3 for a 16-bit read (index
   2 for an 8-bit one). Command `0xc5` then returned the address echo followed by
   **`00` at every SFR tried** — `0x80` P0, `0x81` SP, `0x90` P1, `0xA0` P2,
   `0xB0` P3, `0xD0` PSW, `0xE0` ACC, `0xF0` B.

   That is an unimplemented command, not a chip whose registers are all zero:
   `SP` and `ACC` cannot both read `0x00` on a running 8051 (`SP` is never zero
   in normal operation), and the same transport returned varied, correct data
   for `0xb5` moments earlier. So ms-tools' `deviceType == 2130` gate reflects
   the hardware rather than an oversight, and **the no-patch SFR route is
   genuinely closed on this chip.**

   No further opcodes were swept looking for an equivalent: an unknown command
   id could be a write or a reset, and the EEPROM has no external recovery path
   confirmed yet. Remaining leads for OTF-3 are disassembling our own dumped
   EEPROM image, and the USB capture of the vendor app.

9. **The slide-switch position is readable from XDATA at `0xDF00`, with no
   firmware patch.** Bit 0 mirrors the switch: `0x00` inward/H, `0x01`
   outward/T. Found by differencing full 64 KB XDATA dumps across switch
   positions and confirmed live across four consecutive flips.

   Method, because the noise floor is what makes it interpretable: XDATA is
   live RAM, so two dumps in the *same* position already differ. Taking two
   dumps per position established that floor at **41 of 65,536 bytes (0.06%)**,
   almost all in a `0x0040`–`0x0070` cluster that looks like video/audio
   counters. Of the whole address space only three bytes were stable within
   each position yet differed across positions — `0x0042` and `0x0066`, both
   inside that noisy cluster and treated as coincidence, and `0xDF00`, an
   isolated clean single-bit change. Polling `0xDF00` while the switch was
   flipped H→T→H→T→H tracked every transition.

   This is the **sense** path, not the control path: it is most likely a
   firmware-maintained copy of the polled switch position rather than the mux
   register itself, and it does not by itself enable OTF-3. Its value is that
   it is the first read-only foothold in the `USB_SWITCH` block and anchors a
   byte of known meaning near whatever drives `Sel` — a far better starting
   point than a blind search. It also confirms finding 1 from the chip's side:
   the firmware *does* sense the switch even when no host application is
   running to act on it.

## Testing TODO (OTF-1)

Done 2026-08-18:

- [x] Identify the unit's PCB revision — **v1.9**.
- [x] `TIOCMSET` DTR: polarity, disconnect/re-enumerate, hold time. Asserted =
      disconnected. Holds ≥500 ms reliably produce a replug (floor below
      500 ms not probed; low value given finding 2's instability).
- [x] Serial-open side effects — **confirmed**: opening the tty replugs the
      A-port device, closing restores it.
- [x] Dump the AT24C16 before any write; archive per-unit backups.
- [x] Does the switch override, or gate, the GPIO? **Neither** — see finding 1.
- [x] `TIOCMSET` RTS: **resets the CH9329**. Threshold between 20 and 50 ms;
      ~700 ms to boot; A-port undisturbed. See finding 3.
- [x] Map `DATAFLIP`: **not observable** on any CH340 modem input across
      1,956 samples spanning a reset. See finding 3.
- [x] Read the switch position without patching the firmware — **XDATA
      `0xDF00` bit 0**, confirmed across four flips. See finding 9.
- [x] Re-run the ms-tools reads on a non-virtualized host — done on macOS via
      the hidapi build; see finding 7. (The DTR/hot-plug half of that TODO is
      still outstanding, below.)

Blocked, not merely pending — each needs something this bench cannot supply:

- [ ] Find the MS2109 GPIO write that drives the mux `Sel` — blocked on
      finding 7; needs a USB capture of the vendor app, or vendor register
      documentation. `i2c-scan` is blocked by the same wall, so the config
      I2C bus cannot be enumerated either. **Two untried leads before falling
      back to a USB capture:** a read-only ROM `0xc5` SFR probe (finding 8),
      and disassembling our own EEPROM image — the 8051 code from `0x30` is
      already dumped and is our own hardware's contents, so it involves no
      vendor source. Searching it for the write that sets the `0xDF00`
      neighbourhood is the natural next step.
- [ ] Measure mux-flip behavior once a flip is possible: does the previously
      attached side see a clean disconnect? Settle time?
- [ ] Video-disturbance check during mux flips — **not yet meaningful**: the
      current target outputs a black HDMI signal, so before/after frame
      comparison proves nothing. Needs real content on screen.
- [x] Re-run the DTR hot-plug characterization with the unit attached to a
      non-virtualized host — **done, and it overturns two of finding 2's
      sub-claims. See finding 10.**
      Related: the `ch9329` baud-autodetect failure of issue #81 likewise did
      **not** reproduce off the VM — 10/10 against the same factory-baud chip
      on a directly-attached host — which is independent evidence that the
      passthrough layer, not the hardware, is responsible for at least some
      of what this bench has measured.
- [ ] Root-cause the failed-enumeration storm in finding 2 before OTF-4 ships —
      **but it did not occur at all off the VM** (finding 10), so start by
      confirming it is reproducible on bare-metal Linux before hunting a
      hardware cause.

10. **Off the VM, the DTR replug is clean: no speed degradation, no
    instability.** Re-ran finding 2's characterization with the unit attached
    directly to a macOS host (`/dev/cu.usbserial-*`, DTR driven via `TIOCMSET`
    through pyserial), against a Kingston DT microDuo 3C in the A-port that
    enumerates at high speed.

    | claim (finding 2, on the VM) | bare metal |
    |---|---|
    | asserted DTR disconnects the A-port device | **confirmed** — ABSENT on 6/6 probes taken *during* assertion, present again on release |
    | replugs usually come back at full-speed 12 Mbps, "nondeterministic" | **did not reproduce** — 28/28 cycles returned at high-speed 480 Mbps, zero degraded |
    | ~9 cycles destabilize the hub and wedge the sibling CH340 | **did not reproduce** — 28 cycles with the CH340 and MS2109 checked every cycle, both healthy throughout; `ch9329 info` answered normally afterwards |

    Taken with the same pattern in issue #81 (that autodetect failure also did
    not reproduce off the VM), the reasonable reading is that **the degraded
    link speeds and the enumeration storm are artifacts of per-device USB
    passthrough, not properties of this hardware.** The practical consequence
    is large: OTF-4 (`usb replug`) was held back as "not production-ready"
    mainly because of those two behaviours, and neither survives off the VM.

    Two honest limits on this. It is macOS, not bare-metal Linux — the cleanest
    remaining experiment is the same unit on a Linux host with no
    virtualization, which separates "not Linux" from "not passthrough". And one
    sub-claim of finding 2 needs re-examination rather than confirmation:
    **merely opening the control tty did not unplug the A-port device here.**
    Opening `/dev/cu.*` left it present and enumerated; only an explicit
    deasserted→asserted DTR transition disconnected it. That may be the BSD
    call-out (`cu`) versus dial-in (`tty`) distinction rather than a platform
    difference, and it matters because the "route replug through the existing
    session, never a second `open()`" constraint for OTF-2/3/4 rests on it.
    Do not relax that constraint on the strength of one macOS observation.

## Integration sketch

- The CH9329 `hid` backend (the `ch9329/` helper crate,
  [clean-room spec](ch9329-spec.md)) has shipped; add RTS hardware reset as
  its recovery verb (OTF-2). The reset is now characterized — pulse RTS low
  for ≥ 50 ms, then wait ≥ 800 ms for the ~700 ms boot before the first
  command — so this is implementable. Route it through the existing session
  rather than a second `open()`, since opening asserts DTR and RTS. A watchdog
  that reconnects should force the baud rather than rely on autodetect —
  see [#81](https://github.com/curtisgalloway/panio/issues/81), filed from
  this bench's factory-baud chip.
- `usb attach-host` / `usb attach-target` (OTF-3) is **blocked** on a way to
  drive the MS2109 GPIO.
- `usb replug [--hold-ms]` (OTF-4) works mechanically but is **not
  production-ready**: degraded link speed, re-enumeration bursts, and
  hub-level instability that can take the CH340 with it.
- EEPROM serial-stamping utility (OTF-5) — blocked with OTF-3.
- Extension-pins gadget slot exploration (OTF-6, deferred).
