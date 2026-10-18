<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Openterface Mini-KVM V2: Linux startup echo and serial silence

**Updated 2026-10-11.** A captured Linux tty open echoed unsolicited startup
data back into the device. A cold direct-USB trial that drained startup data
before any tty open answered two GET_INFO queries; the ordinary helper then
worked too. This makes startup echo and firmware parser recovery the leading
explanation for [#238][issue], not a blanket Linux/kernel defect. The installed
firmware remains unidentified; no fixed minimum release has been established.

**Terms:** CDC-ACM is USB's virtual serial interface; `cdc_acm` exposes its
Linux tty (terminal device). Bulk OUT sends bytes to the device; bulk IN returns
them. Line coding sets serial parameters; DTR/RTS are separate control signals.
usbmon records host-side USB requests and completions. See the
[glossary](../docs/glossary.md).

## Earlier measurements

On 2026-09-20, panio 0.4.1 on x86_64 Ubuntu 26.04, kernel 7.0, could not
communicate with a physically identified Mini-KVM exposing `1a86:fe0c`
through `cdc_acm`, alongside MS2109 video (`345f:2109`). Target-side HID
identified as `1a86:fe00`, KeyMod. The same unit and cable worked on macOS.

| Probe | Linux | macOS |
|---|---|---|
| Helper at 115200 / 57600 / 9600, three attempts each | 0/9 | — |
| Helper autodetect | timeout | 6/6, succeeding at first candidate |
| Forced 9600 then 115200, four cycles | — | 4/4 |
| Raw GET_INFO through termios | no reply | — |
| Clear RTS, then RTS and DTR after open | no reply | — |
| Keyboard command observed at target input device | no event | — |
| USB reset or replug | no reply | — |

Further 2026-10-11 tests with panio 0.7.0 failed at all four DTR/RTS states,
through different host USB connections, and after both cables were removed
together. Whether that was the September unit was not recorded. No process held
the tty and `ID_MM_DEVICE_IGNORE=1` was active. A cold restart followed by
another tty open did not isolate what happened during that open.

## Captured investigation

The user identified the Mini-KVM connected to a powered Pi 4B's USB-A port.
A second `1a86:fe0c` KVM-Go was left alone. Selected-device descriptors:
configuration 1; communication interface 0, data interface 1; alternate 0;
interrupt IN `0x81`, bulk OUT **`0x02`**, bulk IN `0x83`; endpoint packet
sizes 64 bytes; `bcdDevice=1.00`; no serial-number string. USB addresses were
re-enumerated after reconnection.

On Ubuntu kernel `7.0.0-38-generic`, 2026-10-11 local time:

1. A raw tty probe read checksum-valid `57ab008108020000050000000092`.
   Initially this looked successful. **usbmon showed that frame arrived before
   the query.** Its disconnected flag described queued startup data.
2. Before raw mode took effect, tty echo sent these **17 bytes** back:
   `57ab5e40815e485e425e405e405e455e40`. The header is still `57 ab`, but
   byte 4 is `81`: a claimed 129-byte payload, or 135-byte total frame.
   The subsequent six-byte GET_INFO completed on OUT without a fresh reply.
3. The installed helper failed at fixed 115200, fixed 9600, and autodetect.
   A second raw query failed. Captures showed successful line-coding and
   DTR/RTS requests, and successful six-byte query writes.
4. Warm direct USB after detaching `cdc_acm` also failed: no explicit line
   coding, then 9600/8N1, then 115200/8N1 with one-second settling. Writes and
   control requests completed; reads timed out after two seconds. Both original
   driver bindings were restored after each trial. Earlier unprivileged attempts
   failed before claiming interfaces and are not transport measurements.
5. A same-session direct sequence at 115200, DTR/RTS values 0, 1, 3, 2,
   produced four timeouts. Lines returned to 0 and drivers were restored.
   These results have tool logs only: the timed capture ended before the trial.
6. The user unplugged **both USB cables for about ten seconds** and reconnected.
   No tty was opened. Direct USB drained the same 14-byte startup frame for a
   250 ms interval, then queried twice. Both queries returned fresh
   `57ab008108020100050000000093`, with target connected. A second drain
   between queries was empty. Capture: 26 packets, zero drops.
7. After driver restoration, normal `ch9329 -b 115200 info` succeeded with
   `chip_version=0x02 target_connected=true`. Capture: 66 packets, zero
   drops. No firmware update or target keystroke was used. The helper's normal
   successful-open path sends all-keys/buttons-released reports.

The successful trial still allowed kernel **probe-time** initialization before
detachment. It isolates tty opening, not all kernel initialization. Missing:
installed firmware identity, a cold helper-first capture, working macOS capture,
and target keyboard-event verification after recovery. The first raw probe
itself introduced captured echo, so later warm trials cannot establish whether
earlier initialization independently caused a fault.

## Source evidence pinned to revisions

The [v0.0.5 release][release], published 2026-09-28, resolves to
`321d4e4c847f47a6a70d200461653802a1cebc0c` and includes Mini-KVM, KVM-Go and
KeyMod assets. Inspecting its source does not identify the installed binary.

| Source | Verified finding |
|---|---|
| [EP3 configuration fix][ep3], `3b105037b7df73df077f5c1f1eedb06593a0a442` | Enables response endpoint on configuration; present in release source. |
| [Response-queue fix][queue], `4a90be4f7985846c6016eb1f4291f9278b1c00c2` | Re-enables disabled EP3 while draining responses and checks its busy flag; present in release source. |
| [Parser and reply construction][parser] | Waits for advertised length before checksum validation, with no partial-frame timeout in this receive path. GET_INFO's first byte is a board selector, not a release tag. |
| [Main loop][main] | Queues a GET_INFO-shaped startup response without a query. Consistent with the captured startup frame; not proof of identical installed code. |
| [CDC descriptor][descriptor] | Published ACM path uses OUT `0x05`, IN `0x83`; our unit uses OUT `0x02`. Its startup payload also differs (`05` versus source `06`). |
| [Line-coding handler][prop] and [UART initialization][uart] | Outside bridge mode, line coding calls UART initialization, changing DMA and buffer state. USB bulk transport does not make that request harmless. |
| [Upstream baud report][baud] | Reports 9600 working while higher rates fail; proposes initialization side effects. An upstream report, not a reproduced cause here. |
| [Busy-timeout report][regression] | Names introducing [`c9f1cb80293b7afe75d4303fe7878559c01554f4`][busy] and working merge [`bf0a4b5bf32f2bf3c6e0059985ba8e34c8edca9a`][working]. The patch adds 20 ms busy-flag recovery. **v0.0.5 still contains the mechanism and its main-loop call**; do not infer its proposed removal shipped. |
| [Linux v7.0 CDC driver][linux], `028ef9c96e96197026887c0f092424679298aae8` | Initializes 9600/8N1 at probe; later sends line coding when cached settings differ. Ubuntu's exact patch set was not audited; captures independently show successful requests. |
| [Linux arm64 report][linux-report] | Reports successful raw queries on current-generation Mini-KVMs. A comment reports CH583 bootloader identification despite host software's CH32V208 naming. |
| [macOS strategy][mac], `1838cff670571b4a3e9fb4cc8ee8224e78e43c44` | Initializes firmware version as unknown, then formats its displayed version from `AppStatus.chipVersion`; no verified release mapping. |

Release source routes endpoint data into a UART-named ring buffer, then
`USB_DataRx_To_KMHandle`, `CH9329_RxBuffer_Add`, the length/checksum parser,
and command dispatch. GET_INFO responses enter a response queue and leave EP3;
keyboard commands enter target-HID queues. An input-parser stall can explain
both absent replies and absent target input; an EP3-only fault cannot explain
both without another failure. The observed 23 bytes (echo plus first query)
would leave that published parser waiting for 135 bytes.

The older QT evidence was pinned at
`0536836c0d6d4bcd102e87f36d175df092c7d432`. Its fixed-115200 setting is
application policy. Its close/reopen and enumeration reports remain leads,
not proof of this failure. Neither USB identifiers nor the host's chip-strategy
name establishes silicon identity or firmware compatibility.

## Ranked hypotheses

| Rank | Candidate | Support | Limits / contrary evidence | Decisive test |
|---|---|---|---|---|
| 1 | Startup echo stalls firmware parser | Captured malformed echo; published parser waits; cold drain succeeds, then normal helper works | Installed parser unidentified; warm trial was itself contaminated | Independent cold helper-first versus drain-first captures; test a vendor-confirmed parser recovery fix |
| 2 | Initialization order / line-coding side effects | Upstream report and initialization code | Cold direct works despite prior kernel probe; explicit rates and delays did not recover warm state | Cold direct test with CDC binding prevented before enumeration |
| 3 | EP3 response initialization or queue fault | Two verified upstream fixes | Startup IN and fresh cold replies work; response-only fault does not explain missing HID | Compare query-correlated IN and target HID after verifying installed firmware |
| 4 | Busy-timeout regression | Upstream report and actual introducing patch | Report's HID still worked; release retains mechanism; hardware/source mismatch | Separately authorized comparison of compatible firmware builds |
| 5 | Generic USB failure / tty read-path defect | Earlier tty timeouts | OUT completed; startup IN arrived; cold direct and subsequent tty worked | Fresh reply in capture but absent in app points to host buffering/parser |

## Diagnostic invocation and cleanup

[`scripts/openterface_probe.py`](../scripts/openterface_probe.py) uses
PyUSB 1.3.1 and emits timestamped JSON Lines. `list` reads descriptors without
opening a tty. `probe` requires an explicit current bus/address, derives the
CDC interfaces and endpoints, and refuses ambiguous layouts.

Run `uv run scripts/openterface_probe.py list`, identify the intended unit by
physical topology, and stop its port users. After a cold cycle, re-enumerate
and **do not open the tty first**. Start a selected-device usbmon capture.
Invocation shape (substitute current enumeration values):
`uv run scripts/openterface_probe.py probe --bus N --address N --detach --step drain=0.25 --step info`.
For root USB access, use an existing Python environment containing PyUSB;
an unqualified `sudo python` may use a different environment.

| Ordered step | Effect |
|---|---|
| `drain=0.25` | Read and record queued bytes for 250 ms, without counting a reply |
| `info` | Send exactly `57 ab 00 01 00 03`; validate header, address, command, length and checksum |
| `baud=9600` / `baud=115200` | CDC request type `0x21`, request `0x20`, communication-interface index, seven-byte little-endian rate/8N1 payload |
| `lines=0..3` | CDC request `0x22`; DTR bit 0, RTS bit 1 |
| `wait=1` | Delay one second |

An explicit-rate trial orders `baud=9600`, `wait=1`, `drain=0.25`,
`info`; compare 115200 after an independent cold cycle. Same-session
sequences do not reset firmware history. Draining reduces startup contamination,
but there is no transaction ID: use capture timing to reject delayed replies.
usbmon does not show every electrical handshake, so a timeout alone does not
identify the device's bus-level response.

The tool reads active configuration and GET_INTERFACE. It never issues
SET_CONFIGURATION, SET_INTERFACE, reset, mux, bootloader, or firmware-write
commands. Exit codes: 0 valid replies, 1 missing/invalid reply, 2 error,
130 interruption. `--detach` interrupts the selected serial connection;
the tool releases interfaces and restores original bindings on normal exit,
partial failure, Ctrl-C and SIGTERM. Restoration errors fail the command.
Verify both interface driver links afterward. SIGKILL or unplugging can defeat
cleanup; reconnect that unit and verify bindings before reuse. A trial whose
interfaces were manually unbound starts with no bindings to restore.

Text usbmon access was denied under this host's kernel lockdown, but binary
capture with tcpdump worked. Use USB_LINUX_MMAPPED captures with sufficient
snapshot length (256 was enough here) and a filter for the selected device.
Keep captures and device-identifying logs out of Git.

### Avoiding kernel probe-time initialization

Detaching after enumeration is not kernel-clean. On a **dedicated test host
with no other CDC users**, prevent `cdc_acm` loading before reconnecting the
cold device, verify neither interface has a driver, and then claim via libusb.
Restore the original module-loading policy and bind those interfaces afterward.
A blacklist alone does not remove an already-loaded module. Do not unload a
shared driver on a live control host. This procedure was not executed; the
successful no-tty test made startup echo the more useful next investigation.

## Decision tree and next action

1. Bytes arriving before OUT are startup/queued data, not query success.
2. Failed or short OUT: investigate access, ownership, endpoint selection and
   transport before firmware parsing.
3. Fresh response in capture but missing from application: investigate host
   buffering and reply parsing.
4. Cold direct works while tty-first fails: compare echo, line coding and
   line-state ordering. Our finding belongs here.
5. Cold direct also fails: compare target HID with response traffic to separate
   parser/routing faults from response-endpoint faults.

Use cold direct drain as a **bench workaround**, then the normal helper.
Request a vendor-confirmed fix for partial-frame recovery and startup behavior
on the actual hardware. Do not flash v0.0.5 just because it mentions EP3 fixes:
it retains the implicated parser behavior and has a different OUT descriptor.

The helper warns only after failed Linux connections to the selected
`1a86:fe0c` port, resolving stable aliases. It preserves the underlying error.
Successful connections and unknown/unrelated identities do not get the hint.
There is no numeric firmware threshold: `chip_version` is not a reliable
release identifier, and no minimum fixed release has been verified.

## Corrections proposed for #238

No issue or upstream comment has been posted.

- Replace blanket kernel blame with the measured startup interaction.
  Raw termios still uses the kernel tty path.
- Replace “baud is always a no-op” with observed macOS rate tolerance;
  line-coding requests can change firmware state.
- `1a86:fe0c` identifies neither a unique product nor verified silicon.
  `chip_version=0x02` does not establish firmware v0.0.2.
- ModemManager exclusion is useful hygiene but was insufficient here.
- Cold cycling followed by tty opening can immediately reproduce a fault.
  A checksum-valid startup frame is not a successful query.
- Earlier missing target keystrokes remain relevant; this session verified
  GET_INFO recovery, not a new end-to-end HID test.

[issue]: https://github.com/curtisgalloway/panio/issues/238
[release]: https://github.com/TechxArtisanStudio/Openterface_KM/releases/tag/v0.0.5
[ep3]: https://github.com/TechxArtisanStudio/Openterface_KM/commit/3b105037b7df73df077f5c1f1eedb06593a0a442
[queue]: https://github.com/TechxArtisanStudio/Openterface_KM/commit/4a90be4f7985846c6016eb1f4291f9278b1c00c2
[parser]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/APP/usbd_compostie_km.c
[main]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/APP/peripheral_main.c
[descriptor]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/APP/USBLIB/CONFIG/usb_desc.c
[prop]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/APP/USBLIB/CONFIG/usb_prop.c
[uart]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/APP/UART/UART.c
[baud]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/NEW_ISSUES.md
[regression]: https://github.com/TechxArtisanStudio/Openterface_KM/blob/321d4e4c847f47a6a70d200461653802a1cebc0c/docs/bugfix-ch9329-command-response-failure.md
[busy]: https://github.com/TechxArtisanStudio/Openterface_KM/commit/c9f1cb80293b7afe75d4303fe7878559c01554f4
[working]: https://github.com/TechxArtisanStudio/Openterface_KM/commit/bf0a4b5bf32f2bf3c6e0059985ba8e34c8edca9a
[linux]: https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/drivers/usb/class/cdc-acm.c
[linux-report]: https://github.com/TechxArtisanStudio/Openterface_QT/issues/581
[mac]: https://github.com/TechxArtisanStudio/Openterface_MacOS/blob/1838cff670571b4a3e9fb4cc8ee8224e78e43c44/openterface/Core/Control/CH32V208ControlChipset.swift
