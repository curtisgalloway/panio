<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Glossary

Plain-language definitions for the terms these docs use. The guides gloss
each term briefly on first use; this page is the longer reference.

## panio's own terms

| Term | Meaning |
|---|---|
| **lease** | A time-limited reservation of a whole target by one holder. Others can still read from the target but cannot change it. See [Target leases](leases.md). |
| **holder** | Whoever acquired a lease and has its token: a person, an agent, or several agents sharing the token. |
| **lease token** | The random secret `panio lease acquire` prints. Passed in the `PANIO_LEASE` environment variable to prove a command belongs to the holder. |
| **default host** | A target's `host` field: the host every channel uses unless it names its own. A lease record lives there. |
| **preempt** | Take a lease from its holder with `--preempt`; the holder is told on its next command. |
| **action** | A named hook command in a target's `actions` table for a control that is not power, such as a reset line or boot strap. Run with `panio action run`. See [Named actions](actions.md). |
| **power level** | A named set of power hooks (such as `soft` or `hard`) beside a target's default ones, for a target with more than one independent power layer. Picked with `--level` on the power commands. See [Power levels](power.md#power-levels-soft-and-hard). |
| **receive boundary** | A capture epoch and exclusive RX chunk sequence; only later received chunks from that epoch belong to an acquisition. |
| **transaction / transaction lock** | One `serial transact` exchange: record the receive boundary, send a command, wait for its reply. The per-interface lock it holds makes other transactions wait their turn and refuses plain input (`serial send`, dashboard keystrokes) until it ends. See [transactions](serial.md#requestresponse-transactions). |
| **follow / NDJSON** | Deliver completed rows as separate JSON objects, one per line, then a final result object. A row beginning at an arbitrary receive boundary may have an unknown prefix. |
| **fault rule** | A named literal pattern on a serial interface (`faults` in the lab file, `panio serial fault set`). When it appears in the bytes a `serial wait` acquires, the wait ends with status `fault`. See [Fault rules](serial.md#fault-rules). |
| **capture epoch** | Identifier for one serial interface supervisor lifetime. Receive chunk counters restart with a new epoch. |
| **receive record / evidence cursor** | An immutable chunk of exact bytes received from a target, identified by a capture epoch and RX sequence. It preserves the original receive time; later reads have their own observation time. This cursor is different from a line-log sequence. |
| **connection generation** | Successful port-open count within an epoch; increments on reconnect. |
| **RX / TX** | Bytes received from the target / bytes accepted by the host serial driver. Neither proves command execution. |
| **capture watermark** | Last receive chunk processed by the capture thread; does not guarantee durable disk writes. |
| **target** | The machine being developed on or tested: the board, PC or VM panio controls. Also called the DUT. |
| **DUT** | Device under test. Same as target. |
| **control host** | The machine cabled to the target that runs panio and its daemons. It can be your laptop or a dedicated box such as a Raspberry Pi. |
| **lab file** | The single config file that describes your hosts, targets and their channels. Managed through the CLI; `panio config edit` opens it in your editor. |
| **synced lab** | A lab file copied from a git repository or an HTTPS URL by `panio config sync`. It is read-only where it was copied: edit the source and sync again. A `lab.source.toml` file beside it records where it came from. See [config sync](distributed-control.md#one-lab-on-every-control-host-config-sync). |
| **ETag** | A version tag an HTTP server sends with a file. `panio config sync` records it and sends it back on the next sync, so the server can answer "unchanged" instead of sending the file again. |
| **channel** | One connection to a target: `serial`, `video`, `hid`, `power`, `netboot`, `usb` and so on. |
| **helper** | A separate program panio runs to drive one kind of hardware (`hdmicap`, `serialcap`, `amt`, `shellyplug`, `ch9329`, ...). |
| **plugin root** | An install prefix outside panio's own install that holds extra helpers (`<root>/libexec/panio/bin`) and skills (`<root>/share/panio/skills`). Listed in the `PANIO_PLUGIN_PATH` environment variable, or a lab host's `plugin_path`. Plugins can add helpers and skills but never replace bundled ones. See [plugin roots](power.md#out-of-tree-helpers-and-skills-plugin-roots). |
| **hook** | A shell command in the lab file that panio runs for an action, such as `on_cmd`, `off_cmd` and `state_cmd` for power. |
| **helper doctor check** | A helper's own read-only health report, `<program> <transport options> doctor --json`, which `panio doctor` runs for a channel set with `--doctor` and lists under it as `ok`, `warn` or `FAIL`. *Transport options* are the options before a hook's verb that say how to reach the device, such as `-d 192.0.2.10`. See [Helper health checks](power.md#helper-health-checks-doctor). |
| **daemon** | A helper that keeps running in the background and holds a device open. `hdmicap` (video) and `serialcap` (serial) are the main ones. |
| **discovery file** | The small file a daemon writes in the runtime directory with its address, PID and token. panio's only record that the daemon is running. |
| **stale / untracked daemon** | *Stale*: running executable content proven different from the installed helper; a metadata or inode replacement alone does not establish this. *Untracked*: still running after its discovery file was deleted. |
| **passthrough command** | A command that hands back another program's exit status unchanged, such as `panio helper <name>` or `panio adb run`. See [Exit status and errors](errors.md#passthrough-commands). |
| **credential source** | Where a helper reads a secret such as the AMT password: an environment variable, a file (`--password-file`) or a command whose output is the secret (`--password-command`). The first one set wins. |
| **management port** | The managed-switch port the control host reaches the switch through. `switchport` refuses to turn it off. See [Guaranteed link drop](netif.md#guaranteed-link-drop-with-a-managed-switch). |
| **SNMP / community string** | A network management protocol, and its shared password (sent in clear text). `switchport --backend snmp` uses it to set a switch port's state. |
| **ifIndex / ifAdminStatus** | SNMP's number for an interface, and the setting that turns it on (1) or off (2). |
| **link mode** | The host side of the direct USB-Ethernet link to a target. `panio netif` puts it in one of four modes: netboot, link, ffx or off. |
| **ownership reservation** | An external lab coordinator's exclusive assignment of a target and its shared controls to one operator or runner for an entire operation. Panio's shorter transaction and hook locks do not provide this reservation. |
| **guarded staging runner** | A board-specific program that checks fresh firmware and USB readiness before each artifact's single RAM transfer, enforces deadlines and ownership, and saves evidence. This is an integration contract, not a bundled panio helper. See [USB fastboot staging](fastboot.md). |
| **service-entry witness** | A complete, newly received firmware response proving entry into the download service for the current artifact and startup. A previous prompt or successful host send is insufficient. |
| **staging receipt** | A retained record of one artifact's input identity, readiness observations, transfer result, termination and target verification, including failed or partial steps. |

## Hardware

| Term | Meaning |
|---|---|
| **AMT / ME** | Intel Active Management Technology. The Management Engine (ME) is an always-on controller in some Intel motherboards that can power the machine on and off and share its screen over the network. |
| **MEBx** | The ME's firmware setup screen, where AMT is enabled. |
| **CC2652** | A Texas Instruments Zigbee radio chip, used as the coordinator for Zigbee smart plugs. |
| **CH340** | A common USB-to-serial chip. |
| **CH9329** | A chip that turns serial commands into USB keyboard and mouse input. |
| **FTDI** | A family of USB-to-serial chips. panio uses their DTR line to press a target's power button. |
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
| **TLS / HTTPS** | Transport Layer Security encrypts a connection and checks the server's identity. HTTPS carries HTTP inside TLS. AMT uses verified HTTPS by default. |
| **CA / public root** | A certificate authority signs certificates. A trusted root certificate starts a chain of trust. AMT uses built-in public roots and can add private roots from a file; these are separate from the operating system's trust store. |
| **PEM** | A text format containing encoded certificates between BEGIN/END markers. The AMT CA-file option reads certificates in this format. |
| **SAN** | Subject Alternative Name: the names or IP addresses a server certificate is valid for. Trusting its issuer does not make it valid for a different name. |
| **Digest authentication** | Password challenge/response authentication used by AMT. It does not encrypt commands or replies; HTTPS supplies that protection. |
| **WS-Management / SOAP** | A management protocol and its XML message format. AMT uses these messages for power commands and KVM configuration. |
| **mutual TLS** | TLS in which both client and server authenticate with certificates. The AMT helper supports server certificates, not client certificates. |
| **cipher / signature algorithm** | Cryptographic methods peers must both support to establish and authenticate a TLS connection. A shared TLS version alone is not sufficient. |
| **BPF** | Berkeley Packet Filter: the macOS kernel interface for sending and receiving raw network frames. |
| **CDC-ACM / cdc_acm** | USB's standard virtual serial-port interface / the Linux driver that exposes it as a tty. |
| **endpoint / bulk transfer** | An endpoint is a numbered USB data path. Bulk transfers carry reliable bytes; OUT goes from host to device, IN returns to the host. |
| **ConIn** | UEFI console input: where firmware reads keystrokes from. |
| **DTR** | Data Terminal Ready: a control output on a serial adapter. |
| **RTS** | Request To Send: another serial control output. Hardware may repurpose DTR or RTS, so their electrical effects depend on the device. |
| **line coding** | A USB serial control request describing baud, stop bits, parity, and data bits. Firmware can act on it even when data travels over USB rather than a physical UART. |
| **tty / termios** | A Unix terminal or serial device / the settings that control it, including raw input and automatic echo. |
| **VID/PID** | USB vendor and product identifiers. Shared identifiers do not uniquely identify a board or firmware build. |
| **BLE** | Bluetooth Low Energy, the low-power Bluetooth mode small devices such as the `dsdble` relay board use. |
| **GATT / characteristic** | How a BLE device exposes itself: a list of services, each holding values called characteristics that a client reads, writes or subscribes to. `dsdble` writes one characteristic (`FFE1`). |
| **VINT / hub** | The Phidgets system of small modules (such as the REL1100 relay) that plug into the numbered ports of a USB hub (such as the HUB0002). The `phidget` helper talks to the hub and addresses an output as `<port>/<channel>`. |
| **NO (normally open)** | A relay contact that conducts only while the relay is on. The Phidgets REL1100 is used this way, so `on` conducts and a powered-down module leaves the target unpowered. |
| **NC (normally closed)** | A relay contact that conducts while the relay coil is off. `dsdrelay` and `dsdble` wire the target through it, so an unpowered board leaves the target powered. |
| **HID** | Human Interface Device: the USB class for keyboards and mice. |
| **fastboot / RAM staging** | Fastboot is a bootloader communication protocol. Its download operation places bytes in a temporary RAM buffer; flashing writes persistent storage, and booting executes an image. These are separate operations. |
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
| **harness** | The agent program that drives panio, such as Claude Code. |
| **LAVA** | KernelCI's system for running tests on lab boards. |
| **OCR** | Optical character recognition: reading text out of a screenshot. |
| **RCS** | Fuchsia's RemoteControlService; `ffx target list` shows `RCS:Y` when it is reachable. |
| **Redfish** | A DMTF standard REST API for managing servers. |
| **tio** | A terminal program for serial ports. |
| **libusb / PyUSB** | A library for accessing USB devices directly / its Python interface. Claiming an interface may require temporarily detaching its kernel driver. |
| **usbmon** | Linux's capture interface for USB requests and completions. It records host-side transfers, not every electrical handshake on the wire. |
| **ZRLE** | Zlib Run-Length Encoding: the lossless RFB encoding hdmicap uses to send changed screen tiles compactly. |
| **xterm.js** | The in-browser terminal the dashboard uses. |
