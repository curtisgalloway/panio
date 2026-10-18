<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Openterface USB-mux switching — clean-room protocol spec

> **Status: KVM-Go half hardware-verified 2026-08-30; Mini-KVM half unblocked
> but untested.** The `0x17` serial command below was exercised end to end on
> our own unit — query, switch to target, switch to host — with the media
> proven to move by a nonce round-trip across the mux, and the filesystem
> intact afterwards. The Mini-KVM `0xDF01` mechanism is corroborated across
> three vendor trees but has not been run against hardware; it supersedes the
> "blocked behind the MS2109 patch wall" conclusion in
> [openterface-deep-control.md](openterface-deep-control.md), which was wrong
> about the vendor's method — the vendor never patches the 8051.
>
> **Provenance.** This is a clean-room report. A separate investigator context
> read the vendor host applications and returned facts and mechanism prose
> only; no vendor source, identifier, or code structure crossed into panio's
> implementation context, and the implementation was written from this document
> rather than from the applications. The vendor trees are AGPL-3.0 except
> `Openterface_Core`, which carries **no licence at all** and was treated as
> maximally encumbered. The draft was scanned with the `os-investigator` leak
> scanner against 159 source files: zero shared token runs, zero shared
> ALL-CAPS identifiers, and twelve identifier hits all triaged benign (nine are
> the tokenizer splitting hex address literals, two are repository/platform
> proper nouns, one is a directory name inside the provenance map). The exact
> file list is the verifier sidecar at
> [`docs/provenance/openterface-usb-mux-map.txt`](../docs/provenance/openterface-usb-mux-map.txt)
> — it exists to be scanned against, not to be read.
>
> Hardware results that were **not** available to the investigator, and that
> refine what follows, are collected in
> [openterface-kvm-go.md](openterface-kvm-go.md).

## Question

How does the vendor host application switch the Openterface USB mux between the host side and
the target side, on two devices?

- **A (primary).** KVM-Go: onboard microSD reader behind an FSUSB42 whose `Sel` is driven by a
  CH32V208 MCU that also exposes a CDC-ACM port emulating the CH9329 protocol. Wanted: exact
  switch bytes, state query, ack, timing, capability gate, line settings, DTR/RTS effect.
- **B (secondary).** Mini-KVM: switchable USB-A port behind an FSUSB42 whose `Sel` hangs off an
  MS2109 GPIO. Wanted: what interface/transfer the app uses, exact bytes, readback, and whether
  the two devices are one abstraction with two backends.

**Scope/assumption:** facts describe what the vendor host applications transmit, not what the
silicon documents (no public datasheet exists for either the CH32V208 firmware or the MS2109
register map, so most values below are necessarily `[source-observed]`). "Host side" and
"target side" below always mean the mux position, never the USB role.

**Sources used, pinned:**

| Repo | Commit | Licence posture |
|---|---|---|
| `TechxArtisanStudio/Openterface_QT` | `f176cf9665e8cc3370dca45de9e3e0dbe258377f` | AGPL-3.0 |
| `TechxArtisanStudio/Openterface_Core` | `b1e6d62182b44f2fd2e9e44ad914d87390d88573` | no licence — all rights reserved |
| `TechxArtisanStudio/Openterface_MacOS` | `4a51de39fc21080a9ca74e057cb3b4e8116f49e6` | AGPL-3.0 |
| `TechxArtisanStudio/Openterface_Android` | `f8a80938f5ee152492c62124fe7209514443c425` | AGPL-3.0 — searched, implements neither mux; contributes nothing |

Every protocol fact in part A was independently corroborated across all three of the QT, Core and
macOS trees. Part B was corroborated across QT, Core and macOS. Where they disagree, that is
called out.

---

## Answer

### A. KVM-Go microSD mux — answered

The mux is driven by a single CH9329-family command with opcode **`0x17`**, carried on the same
CDC-ACM port as keyboard/mouse traffic. The vendor's macOS tree names this command the SD-card
switch explicitly and defines exactly three direction values; the Qt and Core trees call the same
command "USB switch" but build byte-for-byte identical frames. There is **no separate SD command,
no SD-power command, and no card-presence query** anywhere in any vendor tree.

The three request frames and both reply frames, complete (checksums computed and verified
mechanically, not transcribed):

| Operation | Full frame on the wire (hex) |
|---|---|
| Switch mux to **host** side | `57 AB 00 17 05 00 00 00 00 00 1E` |
| Switch mux to **target** side | `57 AB 00 17 05 00 00 00 00 01 1F` |
| **Query** current position | `57 AB 00 17 05 00 00 00 00 03 21` |
| Reply: mux is on host side | `57 AB 00 97 01 00 9A` |
| Reply: mux is on target side | `57 AB 00 97 01 01 9B` |

All five values `[source-observed]`. Set, query and both replies share one frame shape; only the
final data byte and the checksum differ.

### B. Mini-KVM switchable USB-A — answered, and the path is **open**

The app does **not** patch the 8051 and does **not** use a vendor control request or the CH340
serial path. It uses **HID feature reports on the MS2109's factory config interface** carrying a
two-opcode XDATA command set — the same interface you already reach for region reads:

- **XDATA read** opcode `0xB5`, **XDATA write** opcode `0xB6`, address big-endian `[source-observed]`.
- The mux is bit 0 (modern firmware) or bit 4 (legacy firmware) of **XDATA `0xDF01`** — the byte
  immediately after the `0xDF00` slide-switch byte you already found. `1` = target side, `0` = host
  side `[source-observed]`.
- Switching is a **read-modify-write** of that one byte: read `0xDF01`, set or clear the bit, write
  it back. Readback of the current soft position is the same read.

So the operation you could not perform is reachable with the primitives you already have working.

---

## How it works

### A. KVM-Go: mechanism

**Framing.** The `0x17` command rides the ordinary CH9329 frame: a two-byte magic, a device-address
byte, an opcode byte, a payload-length byte, `length` payload bytes, and a one-byte checksum. The
checksum is the low 8 bits of the arithmetic sum of every preceding byte in the frame — i.e. it
covers magic, address, opcode, length and payload, and excludes only itself. Identical rule in all
three trees `[source-observed]`.

**Payload shape.** The declared length is **5**, and only the **last** of those five payload bytes
carries meaning (the direction selector); the first four are zero. The four leading zero bytes are
not optional padding you may trim — the vendor's own macOS tree carries a standing warning that
appending or removing bytes here shifts the checksum position and makes the device reject the
command outright `[source-observed]`. Treat the payload length as fixed at 5.

**Direction selector values** (the tenth byte of the request) `[source-observed]`:

| Value | Meaning |
|---|---|
| `0x00` | drive the mux to the host side |
| `0x01` | drive the mux to the target side |
| `0x03` | query only — do not change the mux |

`0x02` is not defined by any tree. No other value is recognised.

**Reply and how success is judged.** All three request types produce the *same* reply opcode,
`0x97` (the request opcode with bit 7 set — the family-wide response convention), with payload
length `1` and a single status byte that is the **resulting** mux position using the same `0x00`
host / `0x01` target encoding. There is no separate success/failure code. Consequently:

- A **query** and a **set** are indistinguishable on the reply path; a set is confirmed by reading
  the resulting position back out of its own reply.
- The Core tree's synchronous helper writes the set frame, reads one reply, validates it, and
  reports success only if the returned position equals the position asked for. That is the whole
  success criterion `[source-observed]`.
- The Qt application takes a looser, asynchronous route: it writes the set frame and returns
  immediately, letting the reply arrive on the normal receive path and update UI state. The macOS
  application sits in between — it registers a pending completion, fires the write, and settles the
  completion when the next `0x97` frame arrives (or on timeout).

**Reply validation performed by the vendor** (union across trees) `[source-observed]`, in the order
they check: magic bytes correct; address byte zero; opcode byte equals `0x97`; payload-length byte
equals `1`; checksum matches; status byte is one of `0x00`/`0x01`. The Core tree treats a status
byte outside `{0x00, 0x01}` as *not supported* rather than as an I/O error — a useful distinction to
copy. Note the Qt tree computes the received checksum but deliberately does **not** reject on a
mismatch, only logs it — so a strict verifier is a *tightening* relative to shipping behaviour, and
if you see checksum mismatches on real replies that is a symptom the vendor tolerates.

**Reply-vs-echo caution.** Because the reply carries the resulting position and not an ack code,
a device that ignored the request would still answer with a well-formed frame stating the *old*
position. Compare against the requested value; do not treat "a `0x97` arrived" as success.

**No port cycling.** No tree closes, reopens, re-bauds, or otherwise disturbs the serial port around
a switch. The switch is one write on an already-open port `[source-observed]`.

**Statefulness.** The command carries no session, sequence, or operation ID. The macOS tree
maintains an internal operation counter for correlating its own callbacks but explicitly does **not**
put it on the wire — see the warning above `[source-observed]`.

### A. KVM-Go: timing

Every value here is a vendor tuning choice, not a documented silicon requirement.
**All `[source-observed]` — re-derive on hardware.**

| Behaviour | Value | Where |
|---|---|---|
| Settle delay after issuing a switch before reporting done | 100 ms | Qt tree's automation/tool path |
| Completion timeout for a set or a query | 3.0 s | macOS tree |
| Synchronous query wrapper timeout | 2.0 s | macOS tree |
| Delay after the port is open before the first query | 1.0 s | macOS tree |
| Periodic state-poll interval | 3.0 s (macOS) / 1.6 s (Qt) | both |
| Default minimum gap between consecutive commands on this port | 0 ms | Qt tree's send coordinator |

Notes on the above:

- The 100 ms settle is the only post-switch delay anywhere; nothing waits for USB re-enumeration of
  the card reader. Since the mux flip physically detaches and reattaches a mass-storage device,
  **panio will almost certainly need a longer, enumeration-aware wait than 100 ms**, and should
  wait on the block device appearing/disappearing rather than on a fixed delay.
- **There is no retry logic on the switch command in any tree.** A timeout is surfaced as a failure
  and, in the macOS tree, reverts the UI toggle to the previously-known position. It does not resend.
- The two periodic poll intervals disagree between trees; neither is derived from anything. Poll only
  if you want change notification — the query is cheap but it is still traffic on the same port as
  keyboard/mouse.
- The general command-pacing knob defaults to zero, so back-to-back frames on this port are the
  vendor's normal case. Ordering of a switch relative to neighbouring HID frames is
  **not known to be required**.

### A. KVM-Go: capability gate — the two trees disagree, and this matters for you

There are **two different gates** in the vendor code, and they do not agree:

1. **VID/PID gate (Qt and Core — the shipping desktop app).** The chip behind the serial port is
   identified purely from the port's USB vendor/product ID. `1A86:FE0C` selects the CH32V208
   strategy, whose capability flag for "supports the serial USB-switch command" is hard-coded true;
   `1A86:7523` selects the CH9329 strategy, whose same flag is hard-coded false; anything else falls
   back to the CH9329 strategy, i.e. unsupported. No probe, no firmware check, no version gate
   `[source-observed]`.
2. **Chip-version gate (macOS only).** The macOS app additionally derives an SD-capability bit from
   the chip-version byte of the standard device-info reply, treated as a **signed** 8-bit value.
   Unsigned `0x82`, `0x83`, `0x84` are classified as SD-capable; `0x01`–`0x04` are classified as a
   CH9329 with no SD support; everything else, including `0x00`, is conservatively classified as
   unsupported `[source-observed]`.

**This directly touches your bench observation.** Your unit reports chip version `0x01`, which the
macOS table alone would classify as "CH9329, no SD support". But the same macOS tree has a
preceding fix-up step: when the version byte reads `0x00` it re-reads the version from payload
offset 7 of the device-info reply, and when it reads `0x01` **or** `0x02` it re-reads the version
from payload offset 3 of the reply (equivalently, the ninth byte of the whole frame)
`[source-observed]`. So `0x01` at the primary position is explicitly *expected* on these devices and
is a redirect, not a verdict. The vendor's own comment on that branch names a different offset than
the code uses, so the offset is worth confirming against a real reply before you rely on it.

**Recommendation for panio's "unsupported" reporting:** gate on the serial port's VID/PID
(`1A86:FE0C`) as the primary signal — that is what the shipping desktop app does, it needs no
traffic, and it cannot be confused by the version-byte redirect. Optionally *confirm* by sending the
query frame and requiring a well-formed `0x97` reply; a device that does not implement `0x17`
answers nothing (there is no negative ack for an unknown opcode in this protocol), so a timeout on
the query is a clean, positive "unsupported" signal. Do **not** build the version-byte table into
panio without hardware confirmation — it is macOS-only, it is derived from "observed chip
versions" by the vendor's own admission, and it would misclassify your unit if the redirect step
were omitted.

### A. KVM-Go: line settings and control lines

- **Baud: 115200, and only 115200.** Both the Qt chip strategy and the macOS control-chipset layer
  state the CH32V208 supports no other rate, and both refuse or override other requests
  `[source-observed]`. One contradicting datum: the Core tree's device profile table lists a default
  of 9600 for the KVM-Go profiles. That field appears unused on this path and is contradicted by two
  trees; treat 115200 as correct. As a CDC-ACM device the line rate is a class request the firmware
  may ignore entirely, so this likely does not matter — but set 115200 anyway.
- **Framing: 8 data bits, no parity, 1 stop bit, no flow control** `[source-observed]`. The Qt
  application never sets these explicitly and relies on its serial library's defaults, which are
  exactly 8-N-1 with no flow control; the vendor's own standalone test programs set them explicitly
  to those values.
- **RTS is NOT a no-op on the KVM-Go — it is a hardware reset.** This is the most important finding
  in this section and it contradicts the premise in the question. The vendor's factory-reset path
  asserts RTS, holds it for **4 s**, deasserts it, then closes the port, waits, and reopens; the
  documentation on that path names **both** the CH9329 *and* the CH32V208 as using the RTS pin reset
  method. Separately, the Qt tree uses the same RTS sequence as an automatic recovery when a
  CH32V208's target-side USB is detected as dead, describing it as resetting the entire chip
  `[source-observed]`. Timings on that path: RTS asserted 4 s → deasserted → 0.5 s → port closed →
  2 s → port reopened (~6.5 s end to end) `[source-observed] — re-derive on hardware`.

  **Implication for panio:** many serial stacks assert RTS and/or DTR on open by default. On this
  device that is not cosmetic. Explicitly deassert both, or explicitly manage them, when opening the
  port. Whether a *brief* assertion at open is harmless (the vendor holds it 4 s deliberately) is
  **not established** — worth one careful hardware test before shipping.
- **DTR** is used in the Qt tree for a "restart the switchable USB port" action: assert for 500 ms,
  then deassert `[source-observed] — re-derive on hardware`. The Qt code path that invokes it is not
  gated by chip type, but the macOS tree gates the equivalent DTR pulse to MS2109 devices only and
  explicitly routes CH32V208 devices to the serial command instead `[source-observed]`. So DTR's
  effect on a KVM-Go is **unverified** — the vendor's clearer tree treats it as a Mini-KVM-only
  mechanism. Do not rely on it, and do not assume it is inert either.

### B. Mini-KVM: mechanism

**Transport.** HID **feature** reports (set-report to command, get-report to collect the answer) on
the MS2109 configuration interface — not interrupt reports, not a vendor control request, not the
CH340 serial path `[source-observed]`.

**Report layout.** The buffer begins with the HID report ID, then the opcode, then the 16-bit XDATA
address most-significant byte first, then (for a write) the data byte, then zero padding to the
report size:

| Byte index (report-ID at 0) | Write (`0xB6`) | Read (`0xB5`) |
|---|---|---|
| 0 | report ID | report ID |
| 1 | `0xB6` | `0xB5` |
| 2 | address, high byte | address, high byte |
| 3 | address, low byte | address, low byte |
| 4 | value to write | — (zero on the request; **the returned byte on the reply**) |
| 5.. | zero padding | zero padding |

All `[source-observed]`. A read is send-then-get on the same buffer size; the returned value is at
index 4 of the reply, i.e. one past the address. (In the macOS tree the report ID is passed out of
band rather than in the buffer, so its indices are all one lower — the same layout.)

**Report ID and size vary by chip, and the vendor probes rather than knowing.** The trees try a
short list of (report ID, report size) combinations in order and take the first that succeeds
`[source-observed]`:

| Chip | Read attempts, in order (report ID / total report size) | Write attempts, in order |
|---|---|---|
| MS2109 (Mini-KVM) | `0x00`/9, then `0x01`/9 | `0x00`/9, then `0x01`/9 |
| MS2109S | `0x00`/11, `0x01`/11, `0x00`/65 | `0x00`/9, then `0x00`/11 |
| MS2130S | `0x01`/11, `0x00`/11, `0x00`/65 | `0x01`/11, `0x00`/11, `0x00`/65 |

For your Mini-KVM the first entry — report ID `0x00`, 9-byte report — is what the app tries first
and what the shipping Qt code uses on the primary path. Do the same, and fall back to report ID
`0x01` if the first send fails. Note one inconsistency in the vendor's MS2109 fallback branch: on
the report-ID-`0x01` retry it extracts a 4-byte slice starting at index 3 rather than the single
byte at index 4. That looks like a bug in the retry path; use index 4 in both cases.

**The mux bit.** Read `0xDF01`, modify one bit, write it back. Which bit depends on firmware:

| Capture-card firmware version | Target-side bit | Clear mask |
|---|---|---|
| `>= 24081309` | `0x01` (bit 0) | `0xFE` |
| `< 24081309` | `0x10` (bit 4) | `0xEF` |

All `[source-observed]`. The threshold is a date-like build stamp compared as an ordered value; the
Qt tree compares it as a string, the Core tree packs the four version bytes into a single integer
(major×1e6 + minor×1e4 + patch×1e2 + build) and compares numerically against `24081309`. Both reach
the same decision. **Note the vendor's own comment blocks disagree with their code here** — a
comment in the Qt tree says the legacy bit is bit 5, while every code path uses `0x10`, which is
bit 4. Trust `0x10`.

**Firmware version source.** Four consecutive single-byte XDATA registers, read with the same
`0xB5` mechanism `[source-observed]`:

| Chip | Version byte addresses (parts 0..3) |
|---|---|
| MS2109 / MS2109S | `0xCBDC`, `0xCBDD`, `0xCBDE`, `0xCBDF` |
| MS2130S | `0x1FDC`, `0x1FDD`, `0x1FDE`, `0x1FDF` |

If any of the four reads back zero, the Core tree treats the version as unavailable and falls back
to the **modern** bit assignment (bit 0). That default is worth copying — but if your unit is old
enough to want bit 4 and reports a zero version byte, you would silently touch the wrong bit. Read
the version, and if it is unavailable, consider probing: set bit 0, read back, and if the value did
not stick or the slide-switch/mux state did not follow, try bit 4.

**Readback of the *hardware* slide switch** is XDATA `0xDF00` bit 0 — exactly what you already
found, and the vendor's comment agrees on polarity: `1` = the switchable port is routed to the
target, `0` = to the host `[source-observed]`. Note this is only a *report* of the physical slide
position; your independent hardware finding that moving the slide changes nothing electrically is
consistent with the vendor's own documentation that the slide is monitored, not wired.

**Readback of the *soft* position** is `0xDF01` masked with the firmware-appropriate bit — same
register the write targets. There is no separate status register `[source-observed]`.

**Ack.** None. The write is fire-and-forget at the HID layer; success is only "the feature report
was accepted by the OS". The vendor does not read back to verify after a set. If panio wants
certainty, read `0xDF01` back yourself and compare.

**Transaction serialisation.** Both trees hold a mutex/guard across the send-then-get pair for a
read, because a concurrent reader on the same interface can slip its own command between your send
and your get and hand you *its* answer `[source-observed]`. If panio ever polls this interface
from more than one place, that hazard is real and the vendor hit it.

### B.4. One abstraction, two backends — yes, and here is the discriminator

The Core tree models this as one endpoint with a provider list and a `matches` predicate per
provider; the shipping Qt app expresses the same thing as a branch in the UI handler. Both resolve
the same way in practice:

- **Runtime discriminator is the serial port's VID/PID.** `1A86:FE0C` (CH32V208 — **a revision, not a product: V2 Mini-KVMs carry it too**, see [openterface-v2-linux-serial.md](openterface-v2-linux-serial.md)) → serial
  `0x17` command. `1A86:7523` (CH340, Mini-KVM) → MS2109 `0xDF01` register. In the Core tree the
  same distinction is expressed as a protocol flag attached to the matched device profile
  `[source-observed]`.
- **Ordering caveat, flagged as a genuine ambiguity.** The Core tree tries the **MS21xx register
  provider first** and only falls through to the serial provider if the register provider does not
  match — and the KVM-Go device profiles carry *both* protocol flags. So a KVM-Go with its video-chip
  HID interface bound would select the register backend rather than the serial one. Whether that is
  intentional (i.e. the KVM-Go's video chip *also* has a working `0xDF01`) or a latent bug in a
  newer, less-exercised tree, **I could not determine** — the shipping Qt app unambiguously uses the
  serial command for CH32V208 devices and never the register path. Given your schematic shows
  `USB_SW` driven by the MCU, take the serial path for the KVM-Go and ignore the Core's ordering.
- There is also an abstract "USB role switch" capability bit in the Core's per-device capability
  table, present on Mini-KVM, all three KVM-Go variants, and the MS2130S/MS2109S video companions —
  but absent from the plain MS2109 video companion profile `[source-observed]`. That table is a
  static per-profile constant, not a probe.

---

## Reference data

### CH9329-family frame, as used by the `0x17` command

Grouped by frame position, not by driver touch order.

| Field | Width | Value / meaning |
|---|---|---|
| Magic | 2 bytes | `57 AB` `[source-observed]` |
| Device address | 1 byte | `00` in every observed frame `[source-observed]` |
| Opcode | 1 byte | request `0x17`; reply `0x97` (request \| `0x80`) `[source-observed]` |
| Payload length | 1 byte | request `0x05`; reply `0x01` `[source-observed]` |
| Payload | `length` bytes | request: four zero bytes then the direction selector. reply: one status byte `[source-observed]` |
| Checksum | 1 byte | sum of all preceding bytes, mod 256 `[source-observed]` |

Total request length 11 bytes; total reply length 7 bytes. Generic frame size = payload length + 6.

### Direction / status byte encoding (shared by request and reply)

| Value | Request meaning | Reply meaning |
|---|---|---|
| `0x00` | switch to host side | mux is on host side |
| `0x01` | switch to target side | mux is on target side |
| `0x03` | query, no change | (never appears in a reply) |

All `[source-observed]`.

### Related opcodes on the same port (context, not needed for switching)

| Opcode | Reply | Note |
|---|---|---|
| `0x01` | `0x81` | device info; payload byte 0 = chip version, byte 1 = target USB attached (`0x01`), byte 2 = lock-LED bit field (bit 0 num, bit 1 caps, bit 2 scroll); payload length 8 `[source-observed]` |
| `0x0F` | `0x8F` | device reset `[source-observed]` |
| `0x17` | `0x97` | the mux switch, above |
| — | `0xC4` | checksum-error response emitted by the device `[source-observed]` |
| — | `0x99` | appears only in one tree's log-label table; **no command produces it in any tree** — likely dead. Do not implement. |

### MS2109-family XDATA registers reached over HID feature reports

Grouped by function.

| Function | Address | Notes |
|---|---|---|
| Hardware slide-switch position | `0xDF00`, bit 0 | read-only in practice; `1` = target `[source-observed]` |
| Soft mux position (the writable one) | `0xDF01`, bit 0 or bit 4 | see firmware table above `[source-observed]` |
| Firmware version, 4 bytes | `0xCBDC`–`0xCBDF` (MS2109/2109S), `0x1FDC`–`0x1FDF` (MS2130S) | `[source-observed]` |
| HDMI connection status | `0xFA8C` (MS2109), `0xFD9C` (MS2109S), `0xFA8D` (MS2130S) | not needed here `[source-observed]` |

XDATA access opcodes: read `0xB5`, write `0xB6`, address big-endian `[source-observed]`.

### USB IDs seen in the vendor's device tables

| ID | Device / role |
|---|---|
| `1A86:7523` | Mini-KVM serial (CH340) `[source-observed]` |
| `1A86:FE0C` | KVM-Go serial (CH32V208) `[source-observed]` |
| `534D:2109` | MS2109 video/HID companion (Mini-KVM) `[source-observed]` |
| `345F:2109` | MS2109S video/HID companion (KVM-Go V3) `[source-observed]` |
| `345F:2132` | MS2130S video/HID companion (KVM-Go Gen3) `[source-observed]` |

---

## Gotchas & version caveats

1. **RTS on the KVM-Go resets the MCU.** Highest-impact finding; see A/line settings. Deassert
   RTS and DTR explicitly on open.
2. **Payload length is load-bearing.** The four zero bytes before the direction selector are part
   of the declared 5-byte payload. Do not "optimise" the frame — the vendor documents that the
   device rejects it.
3. **The reply reports state, not success.** Always compare the returned position to the requested
   one. A stale/ignored request still yields a well-formed frame.
4. **No negative ack for an unknown opcode.** A device that does not implement `0x17` is silent.
   Your "unsupported" detection must be a timeout, not an error code.
5. **The 100 ms vendor settle is far too short for a mass-storage flip.** The vendor's mux carries a
   card reader; nothing in their code waits for enumeration. Wait on the block device, not a timer.
6. **No retries anywhere on the switch path.** If you add retries, note the command is idempotent
   (setting the mux to where it already is is harmless), so retrying a set is safe.
7. **Two capability gates that disagree** (VID/PID vs chip-version table). Prefer VID/PID. The
   chip-version path has a redirect step for version bytes `0x00`, `0x01`, `0x02` that reads the real
   version from elsewhere in the reply — without which your `0x01`-reporting unit would be
   misclassified as unsupported.
8. **Vendor comments contradict vendor code twice**, both in part B: the legacy `0xDF01` bit is
   commented as bit 5 but coded as bit 4 (`0x10`); and the chip-version redirect offset is commented
   as one index and coded as another. Code won in both cases for this report; both are worth a
   hardware check.
9. **Core-tree provider ordering would pick the register backend on a KVM-Go** if a video-chip HID
   session is bound. Contradicts the shipping app. Do not copy that ordering.
10. **Baud disagreement**: 115200 (two trees, explicit and enforced) vs 9600 (one unused profile
    field). Use 115200.
11. **Shared-interface race on the MS2109 config interface**: a read is send-then-get and is not
    atomic. Serialise all register access through one lock.
12. **The Qt tree does not enforce reply checksums** (computes, logs, proceeds). If panio enforces
    them and sees failures, that is a difference in strictness, not necessarily a bug in your framing.

### What I could not establish (do not guess — capture instead)

- **Card-presence / "no card" and "transfer active" states are not in the protocol.** Nothing in
  any vendor tree reads SD presence or activity over either transport. The `0x97` reply has exactly
  two valid values. If the device's LED shows those states, it is doing so from firmware-local
  signals (consistent with your `SD_STATE` net) with **no host-visible query**. A usbmon capture
  would settle whether any other opcode carries it; I found none.
- **No `SDPOWER_SW` equivalent exists in the host protocol.** No command anywhere toggles SD power.
- **Whether a brief RTS/DTR assertion at port open disturbs the KVM-Go.** The vendor only ever
  asserts deliberately and for seconds. Untested at short durations.
- **Whether the KVM-Go's video chip also exposes a working `0xDF01` mux bit** (the Core provider
  ordering implies it might). Untested; the schematic argues against it.
- **The exact payload offset of the redirected chip-version byte** (comment says one thing, code
  another). One device-info capture resolves it.
- **Whether the CDC firmware honours the requested line rate at all.** Likely irrelevant.

---

## Provenance & clean-room citations

- **Map (where the facts live — directory granularity, deliberately not a reading list):**
  - `TechxArtisanStudio/Openterface_QT@f176cf9665e8cc3370dca45de9e3e0dbe258377f` — the `serial/`
    tree (incl. its `protocol/` and `chipstrategy/` subtrees), the `video/` tree, and the `ui/` and
    `server/` trees.
  - `TechxArtisanStudio/Openterface_Core@b1e6d62182b44f2fd2e9e44ad914d87390d88573` — the
    `include/openterface/` headers and the `src/` tree's `usb_mode/`, `protocol/`, `input/`, `chip/`
    and `profile/` subtrees.
  - `TechxArtisanStudio/Openterface_MacOS@4a51de39fc21080a9ca74e057cb3b4e8116f49e6` — the
    `Managers/` tree (incl. its `serial/` subtree) and the `Core/` tree.
  - `TechxArtisanStudio/Openterface_Android@f8a80938f5ee152492c62124fe7209514443c425` — searched
    exhaustively for both mechanisms; **implements neither**. No facts drawn from it.
  - Exact file list (for an independent verifier's leak scan, **not** for following):
    ``docs/provenance/openterface-usb-mux-map.txt``
- **Cite instead (authoritative, non-encumbered):** none available. There is no public databook for
  the CH32V208 firmware's CH9329 emulation and no public MS2109 register map; the WCH CH9329 datasheet
  documents the frame *shape* (magic, address, opcode, length, sum-mod-256 checksum, reply = request
  with bit 7 set) but not opcode `0x17`, which is a vendor extension. That is why essentially every
  constant above is `[source-observed]` rather than `[databook]`.
- **Confidence:**
  - **High** on the part-A frames, encoding, reply shape and checksum rule — three independent trees
    agree byte-for-byte, and the checksums were recomputed rather than copied.
  - **High** on the part-B transport (HID feature reports), the `0xB5`/`0xB6` opcodes, the `0xDF01`
    address, and the absence of any 8051 patching — three trees agree.
  - **Medium** on the firmware-dependent bit selection in part B (code and comment disagree) and on
    the part-A capability gate (two trees disagree).
  - **Low / unverified** on every timing number (all vendor tuning), on DTR's effect on the KVM-Go,
    and on the Core provider-ordering question.
  - Nothing here was verified against your hardware.
- **Self-scan:** `scripts/leak_scan.py` run against this draft with 159 source files from the
  cloned trees as `--against` (all of Core's `src/` and `include/`; QT's `serial/`, `video/`, `ui/`
  and `server/`; the macOS app's `openterface/`).
  - **0 shared token runs** at the default threshold (>= 10 tokens with >= 5 non-numeric) — the
    check that would catch reproduced or close-paraphrased prose. Clean.
  - **0 ALL-CAPS identifiers** shared with source. Clean.
  - **12 "high-signal" identifier hits, all triaged benign:** nine are the scanner's tokenizer
    splitting hex address literals (`xDF00`, `xDF01`, `xCBDC`–`xCBDF`, `xFA8C`, `xFA8D`, `xFD9C`) —
    these are hardware addresses, i.e. facts the skill explicitly permits, and they are values in my
    own tables, not source text. Two are proper nouns naming a repository and a platform
    (`Openterface_QT`, `macOS`), required to state provenance. One is a directory name
    (`usb_mode`) appearing once, inside the directory-granularity provenance map the report format
    requires.
  - No source-invented function, struct, variable or enum name from any tree appears in this
    document; no struct/enum/macro body, no code, no verbatim comment. Verdict: **pass**.

No source code reproduced; facts and mechanism only.
