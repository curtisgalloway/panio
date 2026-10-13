<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Glossary

Plain-language definitions for the terms these docs use. The guides gloss
each term briefly on first use; this page is the longer reference.

## paniolo's own terms

| Term | Meaning |
|---|---|
| **receive boundary** | A capture epoch and exclusive RX chunk sequence; only later received chunks from that epoch belong to an acquisition. |
| **follow / NDJSON** | Deliver completed rows as separate JSON objects, one per line, then a final result object. A row beginning at an arbitrary receive boundary may have an unknown prefix. |
| **capture epoch** | Identifier for one serial interface supervisor lifetime. Receive chunk counters restart with a new epoch. |
| **receive record / evidence cursor** | An immutable chunk of exact bytes received from a target, identified by a capture epoch and RX sequence. It preserves the original receive time; later reads have their own observation time. This cursor is different from a line-log sequence. |
| **connection generation** | Successful port-open count within an epoch; increments on reconnect. |
| **RX / TX** | Bytes received from the target / bytes accepted by the host serial driver. Neither proves command execution. |
| **capture watermark** | Last receive chunk processed by the capture thread; does not guarantee durable disk writes. |
| **target** | The machine being developed on or tested: the board, PC or VM paniolo controls. Also called the DUT. |
| **DUT** | Device under test. Same as target. |
| **control host** | The machine cabled to the target that runs paniolo and its daemons. It can be your laptop or a dedicated box such as a Raspberry Pi. |
| **lab file** | The single config file that describes your hosts, targets and their channels. Managed through the CLI; `paniolo config edit` opens it in your editor. |
| **channel** | One connection to a target: `serial`, `video`, `hid`, `power`, `netboot`, `usb` and so on. |
| **helper** | A separate program paniolo runs to drive one kind of hardware (`hdmicap`, `serialcap`, `amt`, `shellyplug`, `ch9329`, ...). |
| **hook** | A shell command in the lab file that paniolo runs for an action, such as `on_cmd`, `off_cmd` and `state_cmd` for power. |
| **daemon** | A helper that keeps running in the background and holds a device open. `hdmicap` (video) and `serialcap` (serial) are the main ones. |
| **discovery file** | The small file a daemon writes in the runtime directory with its address, PID and token. paniolo's only record that the daemon is running. |
| **stale / untracked daemon** | *Stale*: still running an old binary after an upgrade. *Untracked*: still running after its discovery file was deleted. |
| **passthrough command** | A command that hands back another program's exit status unchanged, such as `paniolo helper <name>` or `paniolo adb run`. See [Exit status and errors](errors.md#passthrough-commands). |
| **credential source** | Where a helper reads a secret such as the AMT password: an environment variable, a file (`--password-file`) or a command whose output is the secret (`--password-command`). The first one set wins. |
| **link mode** | The host side of the direct USB-Ethernet link to a target. `paniolo netif` puts it in one of four modes: netboot, link, ffx or off. |

## Hardware

| Term | Meaning |
|---|---|
| **AMT / ME** | Intel Active Management Technology. The Management Engine (ME) is an always-on controller in some Intel motherboards that can power the machine on and off and share its screen over the network. |
| **MEBx** | The ME's firmware setup screen, where AMT is enabled. |
| **CC2652** | A Texas Instruments Zigbee radio chip, used as the coordinator for Zigbee smart plugs. |
| **CH340** | A common USB-to-serial chip. |
| **CH9329** | A chip that turns serial commands into USB keyboard and mouse input. |
| **FTDI** | A family of USB-to-serial chips. paniolo uses their DTR line to press a target's power button. |
| **JetKVM** | A network KVM that connects to the target by USB and HDMI. Its keyboard and mouse are reachable only over WebRTC, and it allows one session at a time. |
| **KB2040** | An Adafruit board built on the RP2040 microcontroller. The hidrig uses two of them. |
| **KVM** | Keyboard, video and mouse: controlling a machine's screen and input from another machine. |
| **MS2109** | A chip used in cheap USB HDMI capture dongles. |
| **WebRTC** | The browser peer-to-peer protocol (used for video calls). The JetKVM's firmware uses a WebRTC data channel to carry keyboard and mouse commands. |
| **Openterface Mini-KVM** | A USB dongle that combines HDMI capture with keyboard and mouse injection. |
| **PDU** | Power distribution unit: a networked power strip with switchable outlets. |
| **PHY** | The physical-layer chip behind an Ethernet port. |
| **USB mux** | A switch that connects a USB device to one of two hosts. |
| **systemd credential** | A secret systemd hands a service at start (`LoadCredential=`) as a file in a private directory named by `$CREDENTIALS_DIRECTORY`. |
| **Zigbee** | A low-power wireless protocol used by smart plugs. |

## Protocols and interfaces

| Term | Meaning |
|---|---|
| **BPF** | Berkeley Packet Filter: the macOS kernel interface for sending and receiving raw network frames. |
| **CDC-ACM / cdc_acm** | USB's standard virtual serial-port interface / the Linux driver that exposes it as a tty. |
| **endpoint / bulk transfer** | An endpoint is a numbered USB data path. Bulk transfers carry reliable bytes; OUT goes from host to device, IN returns to the host. |
| **ConIn** | UEFI console input: where firmware reads keystrokes from. |
| **DTR** | Data Terminal Ready: a control output on a serial adapter. |
| **RTS** | Request To Send: another serial control output. Hardware may repurpose DTR or RTS, so their electrical effects depend on the device. |
| **line coding** | A USB serial control request describing baud, stop bits, parity, and data bits. Firmware can act on it even when data travels over USB rather than a physical UART. |
| **tty / termios** | A Unix terminal or serial device / the settings that control it, including raw input and automatic echo. |
| **VID/PID** | USB vendor and product identifiers. Shared identifiers do not uniquely identify a board or firmware build. |
| **HID** | Human Interface Device: the USB class for keyboards and mice. |
| **H.264** | A lossy video codec. A JetKVM streams its screen as H.264; the `jetkvm` daemon decodes it with `ffmpeg`. |
| **HTTP Boot** | UEFI's network boot over HTTP, an alternative to PXE. |
| **IHDR** | The header chunk of a PNG file, which holds its dimensions. |
| **keysym** | A key identifier in the X11/RFB scheme (`0x61` is `a`). noVNC sends keysyms; hdmicap maps them to hid key names. |
| **LL address** | An IPv6 link-local address (`fe80::...`), valid only on one link. |
| **mDNS** | Multicast DNS: name lookup on the local network without a DNS server. |
| **MJPEG** | A video stream made of individual JPEG frames. |
| **NBP** | Network boot program: the first file a PXE client downloads and runs. |
| **noVNC** | A VNC client that runs in a browser (RFB over WebSocket). The dashboard's video pane uses it, served by hdmicap's `GET /rfb`. |
| **PTY** | Pseudo-terminal: a software serial port. A VM's serial console is one. |
| **PXE** | Preboot Execution Environment: the standard way a machine boots from the network. |
| **RFB** | Remote Framebuffer, the protocol VNC uses. A server sends rectangles of pixels; a client sends key and pointer events. `hdmicap` is an RFB client for network video sources. |
| **VNC** | Virtual Network Computing: remote-desktop software built on RFB. |
| **SOF** | A JPEG start-of-frame header, which holds the image size. |
| **SLAAC** | Stateless address autoconfiguration: how IPv6 hosts pick their own addresses. |
| **TFTP** | Trivial File Transfer Protocol: the simple file protocol boot ROMs use to download a boot image. |
| **UART** | A serial port's hardware. |
| **USB-CDC** | The USB class for virtual serial ports. |
| **UVC** | USB Video Class: the standard, driverless protocol webcams and capture cards use. |
| **WoL** | Wake-on-LAN: powering a machine on with a network packet. |
| **bInterval** | How often the host polls a USB device, set by the device. |

## Tools and projects

| Term | Meaning |
|---|---|
| **adb** | Android Debug Bridge, the tool for talking to Android devices. |
| **botanist / testrunner** | Fuchsia's tools for running tests on devices. |
| **cloud-init** | A tool that configures a Linux machine on its first boot from files on the boot media. |
| **ControlMaster** | An SSH feature that reuses one connection for many commands. |
| **ffx** | Fuchsia's host-side developer tool. |
| **harness** | The agent program that drives paniolo, such as Claude Code. |
| **LAVA** | KernelCI's system for running tests on lab boards. |
| **OCR** | Optical character recognition: reading text out of a screenshot. |
| **RCS** | Fuchsia's RemoteControlService; `ffx target list` shows `RCS:Y` when it is reachable. |
| **Redfish** | A DMTF standard REST API for managing servers. |
| **tio** | A terminal program for serial ports. |
| **libusb / PyUSB** | A library for accessing USB devices directly / its Python interface. Claiming an interface may require temporarily detaching its kernel driver. |
| **usbmon** | Linux's capture interface for USB requests and completions. It records host-side transfers, not every electrical handshake on the wire. |
| **ZRLE** | Zlib Run-Length Encoding: the lossless RFB encoding hdmicap uses to send changed screen tiles compactly. |
| **xterm.js** | The in-browser terminal the dashboard uses. |
