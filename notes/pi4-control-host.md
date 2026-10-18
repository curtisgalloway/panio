# Pi 4 as a self-contained control host

> **Status: superseded in part.** A Pi control host is no longer hypothetical:
> a Pi 5 on Raspberry Pi OS Trixie, provisioned from a cloud-init seed on
> 2026-08-20, has been running a CI rack since. The recipe and its
> hardware-validated findings are in
> [control-host-provisioning.md](control-host-provisioning.md). What remains
> unbuilt from this plan is the **Linux USB-HID-gadget backend** below — that
> deployment uses external injectors (`ch9329` KVM devices) over the normal
> `hid` channel instead.

This is a bring-up plan for running the **entire** panio control host on a single
Raspberry Pi 4 (Linux, ARM64) sitting next to one target — netboot, HDMI capture +
OCR, serial console, HID injection, and power control, with the Pi reachable as a
remote control host over the normal lab-file/SSH path. The motivation is a cheap,
always-on box per bench instead of tying up a workstation.

panio's daemons are already cross-platform, so most of this is configuration. The
**one piece that needs building** is a Linux USB-HID-gadget backend — today HID is
delegated to an external KB2040 over serial (see [hid.md](../docs/hid.md)); on a Pi 4 the
control host can *be* the HID device on its own USB-C port. That backend is small
(it reuses the existing Rust report composition and only swaps the transport), but
it does not exist yet.

> **Status.** Netboot, video, serial, power, and remote control work on Linux/ARM64
> today. Gadget HID is **net-new** (design sketched below, not implemented). Treat
> this doc as a checklist + plan, not a description of a shipped configuration.

---

## What works today vs. what's new

| Subsystem | On the Pi 4 | Notes |
|---|---|---|
| Netboot ([netboot.md](../docs/netboot.md)) | ✅ works | Single-binary Rust `netbootd`, ARM64-native. Needs a **secondary** NIC for the direct link — see topology below. |
| Video capture ([video.md](../docs/video.md)) | ✅ works | V4L2 path (`v4l` + `turbojpeg`), no macOS-only deps. MJPEG dongles tee compressed frames straight to `/preview` — cheap on a Pi. |
| OCR | ⚠️ works, weaker | Linux uses Tesseract (`linuxocr`), not Apple Vision. Real accuracy regression on small console fonts — fine for coarse screen state, weaker for exact text. |
| Serial console ([serial.md](../docs/serial.md)) | ✅ works | Any `/dev/tty*` works as the `--device`, including the Pi's own GPIO UART. Voltage + UART-routing caveats below. |
| Power control ([power.md](../docs/power.md)) | ✅ works | Generic `cycle_cmd`/`on_cmd`/`off_cmd`/`state_cmd` hooks call a helper that talks to a plug/relay/hub. The Pi switches nothing itself. |
| HID injection ([hid.md](../docs/hid.md)) | 🔨 needs building | New USB-HID-gadget backend on the USB-C port (`/dev/hidg0`). See [HID gadget backend](#hid-gadget-backend-net-new) below. |
| Remote control ([distributed-control.md](../docs/distributed-control.md)) | ✅ works | The Pi is just another host in the lab file; the dev machine tunnels to it over SSH. |

---

## Physical wiring & port map

The Pi 4 has **one** USB-OTG-capable controller (the BCM2711 dwc2), wired to the
**USB-C** connector. The four USB-A ports hang off the VL805 PCIe host controller and
are host-only — they can never be USB devices. So HID-gadget *must* live on USB-C, and
to free that port for data we power the Pi over GPIO instead.

| Function | Pi 4 connection | Notes |
|---|---|---|
| Pi power | 5V on GPIO pins 2 & 4, GND on pin 6 | Bypasses the USB-C input fuse — **no over-current protection**. Use a solid regulated 5V/3A+ supply; the Pi 4 browns out / corrupts the SD under sag. |
| HID to target | USB-C (dwc2 in peripheral mode → `/dev/hidg0`) | The target supplies VBUS on this cable; the Pi is self-powered from GPIO. **Verify enumeration first** — see the gadget VBUS note below. |
| Serial console | GPIO 14 (TXD, pin 8) / GPIO 15 (RXD, pin 10) / GND | 3.3 V TTL only. **Level + UART-routing caveats below.** Cross TX↔RX to the target. |
| HDMI capture | any USB-A port (UVC dongle) | MJPEG-class MS2109 dongle is the tested baseline. |
| Target link (netboot) | onboard GbE **or** a USB-A↔Ethernet adapter | Must be the Pi's **non-primary** NIC — see topology. |
| Pi uplink | the *other* of {onboard GbE, Wi-Fi} | Carries the default route + SSH from the dev machine. |
| Target power | (none on the Pi) | An external plug/relay/hub driven by a power helper. |

Port budget works out comfortably: HID on USB-C, serial on GPIO, power on GPIO leaves
the HDMI dongle plus three USB-A ports free. (Keeping the legacy KB2040 HID injector
instead would eat one USB-A port and keep an extra board on the bench — moving HID to
the gadget port is what buys the headroom.)

---

## Netboot link topology

`netbootd` deliberately **refuses to run DHCP/TFTP on the host's primary NIC** (so it
can't flood your real network), and the link to the target is a dedicated
point-to-point wire. So the Pi needs **two** interfaces:

- **Option A — onboard GbE = direct link, Wi-Fi = uplink.** Wi-Fi holds the default
  route (so it's "primary"); onboard GbE is the secondary, point-to-point link to the
  target. No extra adapter, but the uplink is wireless.
- **Option B — onboard GbE = uplink, USB-Ethernet = direct link.** Wired uplink; a
  USB-A↔GbE adapter (e.g. the tested TP-Link UE330) is the direct link. Costs one
  USB-A port.

Either is fine; the deciding factor is whether you want the management uplink wired.
See [netif.md](../docs/netif.md) for how the link flips between netboot and ffx modes.

---

## OS configuration (`config.txt` + `cmdline.txt`)

On Raspberry Pi OS (Bookworm), the boot config lives in `/boot/firmware/`.

**1. UART to the GPIO header.** By default the good PL011 (UART0) is bonded to the
Bluetooth modem and the header gets the flaky mini-UART (its baud drifts with the core
clock). Free the PL011:

```ini
# /boot/firmware/config.txt
enable_uart=1
dtoverlay=disable-bt        # gives PL011 (UART0) to GPIO 14/15; disables BT entirely
                            # (use dtoverlay=miniuart-bt + a fixed core_freq to keep BT)
```

```bash
sudo systemctl disable hciuart
```

**2. Stop the OS fighting for the console.** Raspberry Pi OS runs a login getty and
kernel console on that UART out of the box — it will collide with `serialcap`:

```bash
sudo systemctl disable --now serial-getty@ttyAMA0.service
```

Remove the `console=serial0,115200` token from `/boot/firmware/cmdline.txt` (leave
`console=tty1`). After `disable-bt`, the primary UART is `/dev/ttyAMA0` (and
`/dev/serial0` symlinks to it).

**3. USB-C in peripheral mode (for gadget HID).**

```ini
# /boot/firmware/config.txt
dtoverlay=dwc2,dr_mode=peripheral
```

`dr_mode=peripheral` forces device mode so enumeration doesn't depend on VBUS
detection (see the gadget note below). The HID functions themselves are created at
runtime via configfs (`libcomposite`), exposing `/dev/hidg0` (keyboard) and optionally
`/dev/hidg1` (mouse).

---

## Serial level matching (decide per target)

The Pi's GPIO UART is **3.3 V TTL and not 5 V tolerant**. The target's console must
match, or you need a shifter inline:

| Target console level | What to do |
|---|---|
| 3.3 V TTL | Wire GPIO 14/15 directly (TX↔RX crossed, common GND). |
| 1.8 V | **Needs a level shifter.** 1.8 V TX may not clear the Pi's logic-high threshold, and the Pi's 3.3 V TX can over-volt a 1.8 V target RX. Common on modern SoCs — *verify the target's level before wiring* (several RK3588-class boards use 1.8 V; confirm yours). |
| RS-232 (±12 V) | **Needs a transceiver** (MAX3232-class). Direct connection destroys the Pi GPIO. |

> **Why this matters more than it used to.** The current rig uses a level-*selectable*
> USB-TTL cable (the DSD TECH SH-U09C5 does 1.8/2.5/3.3/5 V — see [hardware.md](../docs/hardware.md)),
> which quietly absorbs this problem. Going to the raw GPIO UART removes that buffer, so
> the level decision becomes yours to make per target.

Then it's just a normal serial interface in the lab file:

```bash
panio serial add console -t <target> --device /dev/ttyAMA0 --baud 115200
```

---

## HID gadget backend (net-new)

This is the only software that doesn't exist yet. The shape:

- **Runtime gadget setup** — a small script/service writes a HID gadget under
  `/sys/kernel/config/usb_gadget/` (vendor/product ids, a boot-keyboard report
  descriptor, optionally a mouse), binds it to the dwc2 UDC, and yields `/dev/hidg0`
  (+ `/dev/hidg1`). This is standard Linux configfs gadget plumbing.
- **panio backend** — a new HID transport that writes raw HID reports to the hidg
  character device. Crucially, this **reuses the existing Rust report-composition
  code** (the same logic that today frames reports to the KB2040's CDC, per
  [hid-dual-board-design.md](../docs/dev/hid-dual-board-design.md)); only the output sink changes
  from "frame to serial" to "write to `/dev/hidg0`". No external board, no firmware, no
  baud negotiation — arguably simpler than the current path.
- **Wiring into the lab file** — exposed on the target's `hid` channel like any other
  HID backend, so the web-console KVM and `panio hid` drive it unchanged.

> **Test this first (the one real unknown).** With the Pi self-powered over GPIO and the
> USB-C plugged into the target, confirm the Pi enumerates as a keyboard *before*
> building anything on top. Two failure modes to watch: (1) if the cable carries the
> target's VBUS into the Pi's USB-C while GPIO also feeds 5 V, check there's no rail
> contention/back-powering; (2) a data-only (VBUS-cut) cable avoids contention but can
> leave dwc2 unsure a host is attached — `dr_mode=peripheral` is meant to force device
> mode regardless, but verify on the actual hardware. This is the highest-risk step;
> everything above it is config you can validate independently.

---

## Bring-up order

Validate each layer on its own so a failure points at one thing:

1. **Base OS + uplink + SSH.** Add the Pi to the lab file as a host; confirm the dev
   machine reaches it (`panio --lab … <cmd> --host <pi>`).
2. **Serial.** Configure the GPIO UART (level-match first!), `panio serial watch`,
   confirm boot log capture from the target.
3. **Video.** Plug the UVC dongle, `panio video devices` → `video set` → `video watch`,
   confirm `/preview` and a `shot`. Sanity-check `video read` (Tesseract) accuracy.
4. **Power.** Wire the chosen helper (zigplug/shellyplug/relay), confirm
   `panio power-cycle` and `power-state`.
5. **Netboot.** Bring up the secondary NIC as the direct link, `panio netboot`,
   confirm the target TFTP-boots.
6. **HID (after the backend exists).** Run the gadget VBUS/enumeration test, then the
   `/dev/hidg0` backend, then KVM from the web console.

---

## Open items / risks

- **Gadget VBUS interaction** — the empirical unknown above; bench-test before relying on it.
- **OCR accuracy** — Tesseract is materially weaker than Apple Vision for console text;
  agent flows that depend on exact reads should account for it.
- **GPIO power has no protection** — a flaky supply shows up as undervolt throttling or
  SD corruption, not an obvious error. Use a known-good supply.
- **One UART per Pi** — the GPIO header gives one reliable console. Multiple targets per
  Pi would need USB-serial adapters anyway, which defeats the single-board appeal.
