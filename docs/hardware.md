# Tested hardware

> **Choosing what to buy?** Start with [Recommended hardware](recommended-hardware.md): a
> short list of first choices by use case. This page is the full list of everything panio has
> been tested with.

Links go to the exact items purchased (not an endorsement). Equivalents should work if they
meet the requirement in each subsystem guide: usually **UVC** (driverless USB video) for video,
**FTDI** (USB-serial with DTR) for serial and DTR, **CC2652** (TI Zigbee chip) for Zigbee.
Testing is best-effort; no guarantee of bug-free behavior.

## Power control

See the [power guide](power.md).

| Device | Role |
|---|---|
| [Sonoff Zigbee 3.0 USB Dongle Plus (ZBDongle-P, CC2652P)](https://www.amazon.com/dp/B09KXTCMSC) | Zigbee coordinator for the `zigplug` helper. |
| [ThirdReality Zigbee Smart Plug (15 A, energy monitoring)](https://www.amazon.com/dp/B0BPY5D1KC) | Switched mains outlet driven by `zigplug`. |
| [AINOPE USB 3.0 extension cable (6.6 ft)](https://www.amazon.com/dp/B07RQRMGKB) | Moves the Zigbee dongle away from USB 3 devices, whose RF noise (especially video capture) breaks Zigbee pairing. |
| Cambrionix programmable USB hub | Per-port USB power switching via the [`cambrionix` helper](power.md#cambrionix-hub-control) (control UART, 115200 8N1). |
| DSD TECH SH-UR01A (1-channel) and SH-UR04A (4-channel) USB serial relay boards | Per-channel power switching via the [`dsdrelay` helper](power.md#usb-serial-relay-board-control-dsdrelay) (9600 8N1 AT commands). Wired NC: an unpowered board leaves the target powered. The SH-UR01A's CP2102N has a unique USB serial (select it with `-s`); the SH-UR04A's CP2102 reports the factory default `0001`, so select it by port path (`-d`). |
| [USB 3.0 sharing switch for 2 computers (one-button, remote jack)](https://www.amazon.com/dp/B0HCHDC8X4) plus any relay across the button | A `usb` channel via the [`usbtoggle` helper](usb.md#usb-sharing-switch-usbtoggle), and a cheap USB plug/unplug tester: it switches VBUS, USB 2 and SuperSpeed, with no hub inside. The switch only toggles, so the helper probes for the peripheral to learn the position. Verified with a DSD TECH relay on its remote jack. Many look-alikes are sold under other names (this one's board is marked `KL-SW301-V1.2`); [check one](usb.md#check-a-switch-before-you-rely-on-it) before relying on it. |
| Phidgets REL1100 4-channel solid-state relay on a HUB0002 VINT hub | Per-channel power switching via the [`phidget` helper](power.md#phidgets-vint-relay-control-phidget) over raw USB. Wired normally open (on conducts), write-only: no read-back. |
| [Shelly Plug US Gen4 (S4PL-00116US, Wi-Fi, energy monitoring)](https://www.amazon.com/dp/B0G2YY8TCJ) | Wi-Fi mains outlet driven by the [`shellyplug` helper](power.md#shelly-smart-plug-control-shellyplug) over local HTTP RPC (no cloud). Any Shelly Gen2+ device works. |
| DSD TECH SH-BT01E Bluetooth LE relay board (1-channel) | Wireless fallback when a USB relay or Wi-Fi plug is not an option, via the [`dsdble` helper](power.md#bluetooth-le-relay-board-control-dsdble). Usable, but blind: the board cannot report its state, so panio cannot confirm a switch and there is no `state_cmd`. Prefer the boards above that read back. Wired NC like the USB boards; use it on a Linux control host within Bluetooth range. |
| Dell OptiPlex 7060 (Intel vPro) | Intel AMT target driven by the [`amt` helper](power.md#intel-amt-power-control-amt) (WS-Management over verified HTTPS, port 16993; legacy HTTP on port 16992 requires explicit opt-in), with true power-state readback; no plug hardware. |

## Serial console

See the [serial guide](serial.md).

| Device | Role |
|---|---|
| [DSD TECH SH-U09C5 USB-to-TTL cable (FTDI, 1.8/2.5/3.3/5 V selectable)](https://www.amazon.com/dp/B07WX2DSVB) | TTL UART to a GPIO header; its DTR line can drive the Pi 5 J2 power button ([DTR power control](power.md#dtr-power-control-ftdi-j2-wiring)). |
| [Waveshare Industrial USB-to-TTL (D), FT232RNL](https://www.amazon.com/dp/B0CX5C5KR4) | Pi 5 debug (UART) connector; ships with an SH1.0 3-pin plug and a 4-pin header. |

## HID injection

See the [HID guide](hid.md) and [`hidrig/README.md`](https://github.com/curtisgalloway/panio/blob/main/hidrig/README.md).

| Device | Role |
|---|---|
| 2× Adafruit KB2040 | Reference HID injector: a **control** board (host USB-CDC, I2C1 controller) and a **target** board (DUT USB-HID, I2C1 peripheral) joined by I2C1 (GP10 SDA / GP19 SCL). Any CircuitPython RP2040 board with a free I2C1 works with minor pin edits. Host CLI: [`hidrig/`](https://github.com/curtisgalloway/panio/blob/main/hidrig/README.md); firmware: [`paniolo-hardware`](https://github.com/curtisgalloway/paniolo-hardware) under [`hidrig-kb2040/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040). |
| 2× 4.7 kΩ resistors | I2C1 pull-ups (SDA→3.3 V, SCL→3.3 V). Required: the control board won't open the bus without them. [Wiring diagram](https://github.com/curtisgalloway/paniolo-hardware/blob/main/hidrig-kb2040/README.md). |
| [Openterface KVM-Go (HDMI)](https://openterface.com/product/kvm-go/) | CH32V208 emulating the CH9329 protocol over USB-CDC. Works with the [`ch9329`](https://github.com/curtisgalloway/panio/blob/main/ch9329/README.md) helper unmodified; reports `chip_version=0x01` (a real CH9329 reports `0x38`). [KVM-Go notes](https://github.com/curtisgalloway/panio/blob/main/notes/openterface-kvm-go.md). |
| [Openterface Mini-KVM](https://openterface.com/) | A real CH9329 behind a CH340C adapter; same `ch9329` helper. [Deep-control notes](https://github.com/curtisgalloway/panio/blob/main/notes/openterface-deep-control.md). |

## Video capture

See the [video guide](video.md).

| Device | Role |
|---|---|
| [Generic 4K HDMI capture dongle (MS2109-class, UVC)](https://www.amazon.com/dp/B09FLN63B3) | Target HDMI → `hdmicap` + OCR. Any UVC capture card works. |
| [IPEVO V4K 8 MP USB document camera (UVC)](https://www.amazon.com/dp/B079DLTG9F) | Non-capture-card UVC source for `hdmicap`; also watches the bench. |
| [Openterface KVM-Go (HDMI)](https://openterface.com/product/kvm-go/) | Capture + HID in one unit: MS2130S UVC capture (up to 4K; 1080p60 default), male HDMI plug. Works with `hdmicap` unmodified. |

## Netboot link

See the [netboot](netboot.md) and [link mode](netif.md) guides.

| Device | Role |
|---|---|
| [TP-Link UE330 — 3-port USB 3.0 hub + Gigabit Ethernet](https://www.amazon.com/dp/B01N9M32TA) | Direct host↔target Ethernet link plus spare USB ports. |
| [Anker USB-C to Gigabit Ethernet adapter](https://www.amazon.com/dp/B08CK9X9Z8) | USB-C adapter for the direct link. |
| TP-Link TL-SG108PE (hardware 3.0) or TL-SG205E (hardware 1.0) managed switch | Guaranteed link drop: the [`switchport` helper](netif.md#guaranteed-link-drop-with-a-managed-switch) disables the target's port over the switch's web interface (`--backend easysmart`). A USB Ethernet adapter cannot drop its own link. Measured (SG108PE): the peer loses carrier 0.8 s after `off`, back 1.7 s after `on` (SG205E: 1.1 s / 2.7 s). |
| D-Link DGS-1100-05V2 (hardware A1, firmware 1.00.003) managed switch | Same role over SNMP (`--backend snmp`; SNMP must be enabled and saved once, see the `managed-switch` skill). No PoE. Measured: carrier lost 0.6 s after `off`, back 1.7 s after `on`; the saved configuration survives a power cycle. |

### Cabling a managed switch

Any port can carry the control host's connection to the switch (the
*management port*), but keep it on one fixed port and label it. The link
hooks name it by number (`--mgmt-port`), and it is the only port `switchport`
refuses to turn off. If the cables are later swapped, a hook can turn off the
host's own path to the switch without any warning. On the bench this nearly
happened: after a replug the host had moved from port 1 to port 5. Suggested
layout for a 5-port switch: port 5 for the control host, ports 1 to 4 for
targets. After any recabling, find the management port again before relying
on the hooks (the `managed-switch` skill, step 5). If a port is turned off by
mistake, power-cycle the switch: port changes made by `switchport` are not
saved, so every port comes back enabled.

______________________________________________________________________

*When you verify new hardware, add it here and note what an equivalent must provide.*
