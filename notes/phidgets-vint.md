<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# HUB0002 + REL1100 wire protocol (from libphidget22 1.26)

Status: the wire-format spec for the `phidget` helper (see `phidget/AGENTS.md`). libphidget22 is BSD-3 licensed
(`COPYING`), so reimplementing its wire format is permitted.

## Measured on hardware (2026-10-25, macOS, read-only)

- The hub enumerates as VID `0x06C2` PID `0x0046` (HUB0002 in
  `devices.gen.c`, not HUB0000), full speed, `bcdDevice` 0x0101.
- One interface, class `0xFF`, no kernel driver attached
  (`AppleUSBHostCompositeDevice` only). libusb opens it from user space
  without detaching anything.
- Endpoints: `0x81` IN and `0x02` OUT, **both bulk** (`bmAttributes` 0x2),
  64-byte max packet.
- GET_DESCRIPTOR type `0x43` returned 42 bytes: port 0 `wID` 0x01A
  (REL1100), firmware version 130 (the `_120` failsafe + frequency variant);
  ports 1-5 empty.

## Write tests (2026-10-25, macOS, libusb via pyusb)

Ground truth was a Bus Pirate 5 on channel 0's output: IO0 to the output
terminal with its 10k pull-up to 3.3 V, GND to a REL1100 ground terminal.
About 3.2 V means open (off), 0.0 V means conducting (on). The REL1100 has
**no indicator LEDs**, so it cannot be checked by eye or camera.

| Test | Result |
|---|---|
| Bare SETDUTYCYCLE (one 8-byte bulk OUT, no open reset, port mode, RESET or ENABLE) | switches both ways; ACK `0xE0` |
| Same, sent to a freshly rebooted module (hub port power off/on) | works; **no handshake is needed** |
| Module reboot (PORTPOWER `0x43` off, then on) | relay comes back **off**; port reads `wID` 0 while unpowered |
| Process exit / interface release | state **holds** |
| GPP open reset + close reset (`41 04 2000`, `41 04 2100`) | state **holds** |
| RESET (`0xFA`) then bare ON | works (RESET does not disable the channel) |
| Write to an empty hub port | status `0xE9` NOTATTACHED |
| Starting counter nibble 0 repeated across processes, or 9 | ACKed; the hub does not check the nibble |

Consequences for a panio helper:

- A **one-shot, stateless** hook helper works: write one packet, wait for
  its ACK on EP 0x81, exit. No daemon is needed, and the library's
  close-time RESET (which turns relays off) is simply never sent.
- **The ACK only proves the module is attached.** There is no state
  read-back in the protocol, so `state` cannot be answered from hardware.
  It is write-only, like `dsdble`.
- A cold `cycle` via PORTPOWER off/on also turns every relay on that
  module off; use per-channel SETDUTYCYCLE instead.

Paths are relative to `libphidget22-1.26.20260828/src`. All multi-byte VINT payload
fields are **big-endian** (`pack16`, util/packing.c:71-74). The USB descriptor structs
are copied raw off the wire, so they are **little-endian**.

## 0. Caveat: no macOS backend in this tarball

Only `plat/linux/usblinux.c` (libusb-1.0) is present. There is no `plat/mac`. macOS
facts below come from `usb.h:34-36,86-90` (`#include <IOKit/usb/IOUSBLib.h>`,
`IOUSBInterfaceInterface300 **intf`) and `usb.c:583-586` (`AbortPipe(intf, 1)`), which show
the library uses raw IOKit USB interface access on macOS, not IOKit HID. I could not read
the mac transfer code. Linux libusb semantics are the reference for the wire format; the
wire bytes are identical on any OS.

## 1. USB transport

HUB0002 is a vendor-class ("PHIDUSB") device, VID 0x06C2 PID 0x46.
- devices.gen.c:1477 `USB_PHIDGET(PHIDUID_HUB0002, PHIDID_HUB0002, PHIDCLASS_HUB, USBVID_PHIDGETS, USBID_0x46, 0, 0, hub, 1)` (interface 0, canNak 0)
- Connection type is decided at run time from the descriptor, usblinux.c:655-661:
  `bInterfaceClass == 0xFF` -> `PHIDCONN_PHIDUSB`; `== 0x03` -> `PHIDCONN_HIDUSB`.
  HUB0002 is handled only in the PHIDUSB branches of hubdevice.c (e.g. :991-1000 and
  :1034-1048 use `transferpacket`, `updatePortProperties` :248 returns early unless
  `connType == PHIDCONN_PHIDUSB`), so the hub is vendor class 0xFF. **Not verified
  against a real descriptor**; confirm with `ioreg`/`lsusb -v` (expect bInterfaceClass
  0xFF, 1 interface, bNumEndpoints <= 2).
- Expected endpoints (usblinux.c:663-683): EP 0x81 IN (interrupt or bulk, ep1type), EP 0x02
  OUT bulk (ep2type must be BULK). Max packet sizes come from a Phidget endpoint
  descriptor (type 0x42, 4 bytes: bLength, 0x42, wMaxPacketSize LE) trailing each
  endpoint descriptor, and a Phidget device descriptor (type 0x40, 8 bytes) trailing the
  configuration descriptor (usblinux.c:686-745; structs usb.h:~172-186). Config must
  have 1-2 interfaces, interface 0 with exactly 1 altsetting.
- Kernel driver: library does `libusb_kernel_driver_active` then `libusb_detach_kernel_driver`
  if active (usblinux.c:1157-1161), then `libusb_claim_interface(interfaceNum=0)` (:1163).
  For a vendor-class interface nothing should normally be attached on macOS.
  BUSY on claim -> "device may already be open" (another process owns it).

### Which transfers carry what (HUB0002)
`sendpacket()` PHIDCONN_VINT branch (phidget.c:1626-1664) builds a hub packet then calls
`sendpacketWithTracking(hubDevice,...)` -> `sendpacket()` PHIDCONN_PHIDUSB branch
(phidget.c:1509-1530):

```
PhidgetUSBTransferPhidgetPacket(iop, phidusbConn, PHIDGETUSB_REQ_BULK_WRITE, 0, 0, bufferIn, &len, getTimeout(device)=2000ms)
```
-> usblinux.c:227-233: `libusb_bulk_transfer(handle, endpoint 0x02, buf, len, ..., timeout)`.

**VINT data packets = bulk OUT on EP 0x02** (len must be <= maxPacketEP2, else INVALIDARG,
:216-217).

Hub configuration packets (port mode, port power) are **vendor control transfers**
(usblinux.c:235-243, request enum usb.h:186-193):
- bmRequestType = `0x40 | 0x01` = **0x41** (host->device, vendor, interface recipient)
  for writes; 0xC1 for reads.
- bRequest = `PHIDGETUSB_REQ_CHANNEL_WRITE` = **0x00** (sendHubPortPacket, hubdevice.c:1034-1045)
  or `DEVICE_WRITE` = 0x02 (sendHubPacket, :991-1000). Others: CHANNEL_READ 0x01,
  DEVICE_READ 0x03, GPP_WRITE 0x04, GPP_READ 0x05, BULK_WRITE 0x06 (not a real control request).
- wValue = `(packetType << 8) | index` (`USB_PHIDGET_CH_REQ_MAKE_TYPE_AND_INDEX`, usb.h:~198).
  For hub packets packetType = `VINTHUB_PACKET_HUB(0x40) | hubPacketType`, index = port
  (CHANNEL_WRITE) or 0 (DEVICE_WRITE).
- wIndex = interface number (0). Data stage = payload. Timeout 100 ms for hub packets.

Hub packet types (hubdevice.h:44-53): SETPORTMODE 0x00, UPGRADE_FIRMWARE 0x01,
GETTXBUFFERSTATUS 0x02, PORTPOWER 0x03, CALIBRATION_* 0x05-0x07, PORTAUTOSETSPEED 0x08.

Concrete examples:
- Set port N to VINT mode: bmRT 0x41, bReq 0x00, wValue `0x4000 | N`, wIndex 0, data `[0x00]`
  (`PORT_MODE_VINT_PORT = 0`, types.gen.h:951; hubdevice.c:1105-1108).
- Port power: bmRT 0x41, bReq 0x00, wValue `0x4300 ... ` precisely `(0x40|0x03)<<8 | N` = `0x4300|N`,
  data `[on]` (hubdevice.c:~820-826). Library never sends this on channel open.
- Read TX buffer status: bmRT 0xC1, bReq 0x03 (DEVICE_READ), wValue `0x4200`, wIndex 0, len 7
  (hubdevice.c:155-158); returns `[bufLen, free_port0..free_port5]`.

## 2. Packet layout

### Layer A: VINT device packet, `PhidgetVINTDevice_makePacket` (device/vintdevice.c:185-199)
Constants: `VINT_DATA = 0x00`, `VINT_CMD = 0x80`, `VINT_DATA_wCHANNEL = 0x40` (vint.h:43-46);
`VINT_MAX_OUT_PACKETSIZE = 48`.

For channel index 0 (`uniqueIndex == 0`):
```
[ len+1 ][ packetType ][ payload... ]          len = payload length
```
For channel index 1..3 (`uniqueIndex != 0`):
```
[ 0x40 | (len+2) ][ channelIndex ][ packetType ][ payload... ]
```
No sequence number, no checksum at this layer. `channelIndex = vintChannel->uniqueIndex`
= index of the channel in the device's channel list (phidget.c:916 `channel->uniqueIndex = uniqueIndex`;
REL1100 defs `COUNT(4)`, devices.gen.c:1778-1782), i.e. 0..3 = relay number.

Numeric values (vintpackets.h):
- `VINT_PACKET_TYPE_DIGITALOUTPUT_SETDUTYCYCLE` = **0x0A** (:200)
- `VINT_PACKET_TYPE_DIGITALOUTPUT_SETFREQUENCY` = 0x0F; `..._FAILSAFE` (device->host event) = 0x0D
- `VINT_PACKET_TYPE_FAILSAFE_TIME` = **0xF6**, `VINT_PACKET_TYPE_FAILSAFE_RESET` = **0xF7** (:59-60)
- `VINT_PACKET_TYPE_PHIDGET_RESET` = **0xFA**, `..._PHIDGET_ENABLE` = **0xFB** (:62-63)
- Device commands (not data; vint.h:61-66): RESET 0x83, UPGRADE_FIRMWARE 0x8B, FW_UPGRADE_DONE 0x8C,
  SETSPEED2 0x8F (payload 4-byte BE Hz; only if both port and device are VINT2 + support set-speed).

REL1100 payloads (sendREL1100, vintpackets.c:7697-7755), identical for all 3 firmware UIDs:
- setState(on): packetType 0x0A, payload `pack16(on ? 0x8000 : 0)` = `80 00` / `00 00`
- setDutyCycle(d): `pack16(d * 32768)`
- Failsafe time (UID 110 / 120 only): type 0xF6, payload `pack16(ms)`, valid 500..30000 (digitaloutput.gen.c:832-840)
- Failsafe reset (110/120): type 0xF7, no payload
- Frequency (120 only): type 0x0F, `pack32(Hz)`, 100..20000, default 15625

Firmware-version to UID mapping (devices.gen.c:1776-1796), version = `(wVersion>>8)*100 + (wVersion&0xFF)`
from the hub's ports descriptor: 100-109 -> REL1100 (no failsafe), 110-119 -> Failsafe, 120-199 -> Failsafe_Frequency.

### Layer B: hub wrapper, `PhidgetHubDevice_makePacket` (hubdevice.c:921-961, HUB0002 case :944)
```
byte0 = ((packetOutCounter[port] << 4) & 0xF0) | (port & 0x0F)
byte1 = vintID & 0xFF                 // REL1100 vintID = 0x1A -> 0x1A
byte2 = (vintID >> 4) & 0xF0          // -> 0x00 for 0x1A
byte3 = packetID                      // 1..126, see below
byte4.. = Layer A bytes
```
`port` = hub port 0..5. `packetOutCounter[port]` is a per-port counter, starts at 0 (zero-alloc'd),
incremented after each successful send (phidget.c:1653), only the low nibble is used.
Packet IDs are per-port ranges: `VINTHUB_PACKETID_MAX=126`, `VINTHUB_PACKETIDS_PER_PORT = 126/6 = 21`
(hubdevice.h:109-110); a packet from port p uses an ID in `[p*21+1, (p+1)*21]`
(phidget.c:1632-1633). The library round-robins within the range (packettracker.c:307-332).
The ID is echoed in the ACK and is the only thing that ties a reply to its write. Total length
= 4 + Layer A length; must be <= maxPacketEP2.

### Worked example: relay 0 on hub port 3 ON (first packet)
Bulk OUT EP 0x02, 8 bytes: `03 1A 00 <id> 03 0A 80 00` where `<id>` in 64..84 (3*21+1 .. 4*21), e.g. `40`.
Relay 2 on port 3 ON: `03 1A 00 40 | 43 02 0A 80 00`  (second counter value would give byte0 = `13`).
Relay 0 OFF: payload `00 00`.

## 3. Open handshake (what the library does)

Hub open (`openAttachedUSBDevice`, usb.c:458-... ; hub initAfterOpen hubdevice.c:79-170):
1. claim interface 0 (see section 1).
2. `GPP_open_reset`: control OUT, bmRT 0x41, bReq 0x04 (GPP_WRITE), wValue `0x2000`
   (`PHID_GENERAL_PACKET_OPEN_RESET = 0x20`, gpp.h:59, <<8 | 0), wIndex 0, no data (gpp.c:526-528). Done only for PHIDUSB.
3. Start async IN reads (32 queued transfers, 64-byte) and a read thread.
4. Hub initAfterOpen: control IN `bmRT 0xC1, bReq 0x03, wValue 0x4200, len 7` to learn per-port
   TX queue free space (used for host-side flow control only).
5. initAfterAttach (hubdevice.c:179-235): per port, GET_DESCRIPTOR vendor port descriptor
   (type 0x45, index = port) for protocol version, speed, mode, power, props.
6. Device discovery (vint.c:~277,`scanVintDevices`): standard GET_DESCRIPTOR for type 0x43 (below).
   For each port with id > 0x0F, `scanVintDevice` reads the VINT device descriptor (type 0x44, index
   = port) when id > HUB_PORT_ID_MAX (vint.c:~190-215). Only for protocol/speed info.

Channel open (dispatch.c:1107-1200, in order):
1. `BP_OPENRESET` -> vintdevice.c:137-153: hub `SETPORTMODE` (control, `[0x00]` for a normal VINT channel,
   `hubPortMode = PORT_MODE_VINT_PORT`, phidget.c:1910) **then** (unless channel persistence) VINT data packet
   `PHIDGET_RESET` (type 0xFA, no payload; relay goes to default = off).
2. `channel->initAfterOpen` / `setDefaults`: only updates host-side state for DigitalOutput (digitaloutput.gen.c:1399-1420
   "Reset values" are host fields; **no USB traffic**). No setSpeed, no failsafe config, no port-power packet.
3. attach callback.
4. `BP_ENABLE` -> `PHIDGET_ENABLE` (type 0xFB, no payload) (vintdevice.c:155-161).
After step 4 the library's setState goes out as the Layer A/B packet above.

Is it required? **Not determinable from the library source** because firmware behavior is not here. What the code
shows: the library never sends a data packet to a channel before RESET + ENABLE, and the hub tracks per-port
state (port mode, "NOTATTACHED"/"INVALIDSEQUENCE" replies, vintdevice.c / hubdevice.c:482-486 map
`INVALIDSEQUENCE -> EPHIDGET_NOTCONFIGURED`). The status code name `INVALIDSEQUENCE` ("not sent in the required
sequence", vintpackets.h:559) suggests the device may reject SETDUTYCYCLE before ENABLE. Safest raw sequence,
mirroring the library: (optional) port mode `[0]` control; `RESET` (0xFA); `ENABLE` (0xFB); then
SETDUTYCYCLE. A raw client that only wants to flip a relay while leaving other relays alone should skip RESET
(the library's "persistence" path skips it, dispatch.c:1110 / phidget.c:3035) and test whether ENABLE alone is
needed. Needs a hardware experiment.
Port mode is already 0 (VINT) after a prior session's close reset; `isVintChannel` always runs OPENRESET.

## 4. Incoming path / ACKs

Reader: `PhidgetUSBReadPacket` (usblinux.c:404-470): IN endpoint `0x80 | (interfaceNum + 1)` = **0x81**, size
`maxPacketEP1` (<= 64). In practice the library keeps 32 async IN transfers pending (usb.h:XFER_CNT). HUB0002 does
not use the General Packet Protocol on IN (gpp.c:80-86, PHIDUSB excluded) so every IN packet goes to
`PhidgetHubDevice_dataInput` (hubdevice.c:513-788).

IN packet format:
```
byte0: bit7 unused here; bits6-4 = packetCounter (3-bit, increments per IN packet; lost-packet detection);
       bits3-0 = packetReturnLen = number of ACK bytes at the END of the packet
byte1: bit7 = hub message flag (0x80); bits5-0 = nextPacketStart (offset to first whole VINT packet)
[hub message, if byte1 bit7: byte2 = type (0 TXBUFFERSTATUS, 1 OVERCURRENT, 2 DETACH, 3 DISABLE,
  4 SPEEDCHANGE, 5 REENUMERATION), then payload]
... VINT data from devices (first byte of each has bit 0x08 set; byte0 bits7-4 hi nibble of vintID, bits 2-0 port,
    byte1 low byte of vintID, byte2 length (&0x3F), then dataCount+... bytes) ...
tail (last packetReturnLen bytes): ACKs for your writes
```
Tail ACK parse (processPacketReturnCodes, hubdevice.c:403-440): repeat: `b = next byte; packetID = b & 0x7F;
if (b & 0x80) status = next byte else status = ACK(0xE0)`. So a bare packet-ID byte is an ACK; ID|0x80 is followed by a
status byte. Status codes (vintpackets.h:555-573): ACK 0xE0, NAK 0xE1 (resend), INVALIDARG 0xE2,
INVALIDPACKETTYPE 0xE3, INVALIDSEQUENCE 0xE4, INVALIDCOMMAND 0xE5, MALFORMED 0xE6, NOSPACE 0xE7,
UNEXPECTED 0xE8, NOTATTACHED 0xE9, TOOBIG 0xEA, FAILSAFE 0xEB, DEVICE_ERR0-4 0xF0-0xF4.

Must a reply be read per write? The library blocks until the ACK arrives (`waitForPendingPacket`, timeout 2000 ms,
phidget.c:1514-1520 / getTimeout :1431), but that is library policy. USB-wise the write is a plain bulk OUT that
completes on the USB ACK. A raw one-shot client can just write and, if it wants confirmation, read IN EP 0x81 until
it sees its packetID in a tail. Whether the hub's IN queue wedges if never read is not shown by the source; read it
to be safe (a few 64-byte packets of ACK/keepalive). Also note the hub's per-port TX queue is finite
(`internalPacketInBufferLen` from the TXBUFFERSTATUS read; `NOSPACE` 0xE7 if exceeded).

## 5. GET_DESCRIPTOR for the VINT ports descriptor

`USB_DESC_TYPE_VINT_PORTS_DESC = 0x40 | 0x03 = 0x43` (usb.h:~151); also VINT_DEVICE_DESC 0x44, VINT_PORT_DESC 0x45.
Request (usblinux.c:298-307, `PhidgetUSBGetDeviceDescriptor`; call vint.c:~277):
`bmRequestType 0x80` (IN | STANDARD | DEVICE), `bRequest 0x06` (GET_DESCRIPTOR), `wValue = (0x43 << 8) | 0` = **0x4300**,
`wIndex 0`, length `sizeof(struct)`, timeout 500 ms. Reply length must equal the struct size or the library bails
(vint.c:~330).

`USBD_VINTPortsDescStruct_6_4` (usb.h:~230-244, packed, all LE) = **42 bytes**:
```
u8  bLength; u8 bDescriptorType;
struct { u16 wID; u16 wVersion; } port[6];            // 24 bytes. wID = VINT ID (0 = empty), wVersion = fw version
struct { u16 wID; u16 wVersion; } portModeDevice[4];  // 16 bytes (digital in/out, voltage in, voltage ratio modes)
```
Offsets: bLength@0, type@1, port[i].wID @ 2+4i, port[i].wVersion @ 4+4i, portModeDevice[j] @ 26+4j.
REL1100 on port i: `wID == 0x001A`; firmware = `(wVersion>>8)*100 + (wVersion&0xFF)` (so wVersion 0x0078 -> 120).
HUB0002 uses this 6/4 struct (vint.c:~288 case list includes `PHIDUID_HUB0002`; HUB0007 uses the 1_4 variant).
A port with wID <= 0x0F is "nothing / hub-port-mode device" (vint.c:`deviceID > HUB_PORT_ID_MAX` where
`HUB_PORT_ID_MAX = 0x0F`).

Port descriptor (type 0x45, index = port; USBD_VINTPortDescStruct, usb.h:262-270): u8 bLength, u8 type, u8 bPortMode,
u8 bPowered, u32 dwSpeedHz, u8 bVINTProtocolVersion, u8 VINTProperties[10] — useful to check `bPowered` and `bPortMode`.

## 6. macOS

- Library uses IOKit `IOUSBInterfaceInterface300` (raw USB interface, usb.h:86-90), not IOKit HID, for PHIDUSB
  devices. HID is only for the legacy `PHIDCONN_HIDUSB` family (interface class 0x03), which HUB0002 is not.
- If the hub enumerates vendor-class (0xFF), macOS attaches no driver, so nothing to detach; nusb/rusb can claim interface 0 directly.
  Check with `ioreg -p IOUSB -l -w0 | grep -A30 -i "VINT"` and look at bInterfaceClass.
- Another process (Phidget Control Panel, phidget22 service/webserver, a previous holder) holding the interface
  makes claim fail with BUSY.
- On USB error macOS path calls `PhidgetUSBResetDevice` (usb.c:~205) – library resets the whole device on I/O trouble.

## 7. Failsafe, close behavior

- Failsafe exists only on REL1100 firmware >= 110 (UID `REL1100_Failsafe*`). It is **off by default**: the library
  sends `FAILSAFE_TIME` (0xF6, `pack16(ms)`, 500-30000) only when the app calls `enableFailsafe`
  (digitaloutput.gen.c:1488-1495); setDefaults does not (digitaloutput.gen.c:1405-1411 no failsafe field set).
  When enabled, the app must send `FAILSAFE_RESET` (0xF7, no payload) periodically; on expiry the device sends
  `DIGITALOUTPUT_FAILSAFE` (0x0D) event and then replies `0xEB` FAILSAFE to every packet except RESET until a
  channel RESET/reopen (dispatch.c:1161-1175, hubdevice.c EPHIDGET_FAILSAFE). The resulting output state on trip is
  not in the source (presumably relays off; the library says "Failsafe procedure initiated"). Firmware 100-109
  has no failsafe at all.
- If the host merely stops talking and failsafe is not armed: nothing in the library indicates any timeout; the
  relay should hold its last state as long as hub/port power stays up. Not verifiable from source.
- Channel close (`Phidget_close`, phidget.c:3034-3045): `BP_CLOSERESET` -> VINT `PHIDGET_RESET` (0xFA; relay off, i.e.
  returns to the reset defaults) **then** hub `SETPORTMODE [0]`, then waits for pending packets (vintdevice.c:116-135).
  Skipped if the channel opened with persistence (DigitalOutput supports it, devices.gen.c:2313 `CHANNEL_SUPPORT_PERSISTENCE`;
  set via `Phidget_setChannelPersistence`, phidget.c:3974-3982). **So the stock library turns relays off on close
  unless persistence is set.** A raw client that never sends 0xFA leaves the relay as set.
- Hub close (last user gone, phidget.c:769-771): `GPP_close_reset` control OUT (bmRT 0x41, bReq 0x04, wValue `0x2100`,
  wIndex 0, no data; gpp.c:549) then releases the interface. What the hub firmware does on this for already-configured
  ports/relays is **not determinable from source** (there is no handling on the host side that undoes relay state).
  Test on hardware: set relay on, issue/omit the close reset, release, observe the relay.

## Minimal raw sequence (untested, derived from the above)
1. (optional) claim interface 0; control IN `C1 03 00 42 ...` (TX buffer status) and GET_DESCRIPTOR 0x4300 to find the port with wID 0x1A.
2. Optionally `GPP_open_reset`: control OUT `41 04 00 20 00 00 00 00` (bmRT,bReq,wValue lo/hi,wIndex,wLength).
3. Optionally port mode: control OUT `41 00 <port> 40 00 00 01 00` with data `00`.
4. Bulk OUT EP 0x02: `[ctr<<4|port, 0x1A, 0x00, id, (ch? 0x40|(len+2),ch : len+1), 0x0A, 0x80 or 0x00, 0x00]`.
5. Read IN EP 0x81; ACK is `id` (or `id|0x80` + status) in the trailing `byte0 & 0x0F` bytes.
Unknown/untested: whether RESET/ENABLE are strictly needed, counter-nibble tolerance when starting at a non-zero value
on a hub that was previously used by another process, and relay state after hub close reset.
