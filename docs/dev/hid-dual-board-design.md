<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# HID rig — dual-board "dumb pipe" design

> **Status:** the core HID dumb-pipe firmware is on this branch. The **relay
> power-cut** is built and **hardware-verified** (off/on/cycle actuate the relay;
> the state persists across a control-board reset via NVM). The **serial-console
> bridge** is built but **not yet hardware-verified**.
> **Branch:** `i2c-kb2040-dual-board`. **Date:** 2026-06-09, rev. 2026-06-14.
> This captures the architecture we converged on for the two-board KB2040 rig so the
> thinking crosses cleanly into implementation. It supersedes the role-based ASCII
> firmware previously on the branch (`code.py`, now the retired single-board firmware
> at [`hidrig-kb2040/firmware/single-board/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware/single-board)
> in the paniolo-hardware repo). The **external** panio
> HID interface — the `hidrig` CLI and [`hid-serial-protocol.md`](hid-serial-protocol.md)
> v1 — is unchanged; only the daemon↔firmware wire format changes.
>
> **Rev. 2026-06-14** adds two capabilities to the control board so a *single* USB-attached
> device drives the DUT completely: a **serial-console bridge** (control-board hardware UART
> ↔ DUT console, multiplexed up to the host and re-exported into panio's existing `serial`
> channel) and a concrete **relay power-cut** for DUT power-cycling. Both ride the existing
> CDC link as new/extended frame types; neither requires a change to panio's CLI.

---

## 1. Motivation & history

The rig has been two-board before. Per `hidrig/README.md`'s History note, the **first**
version was a USB-CDC control board relaying a **binary I2C protocol** to a USB-HID target
board, and it was collapsed to a single board over UART specifically to eliminate "the
second board, the binary I2C protocol, and **the duplicated opcode tables**." The opcode
duplication — both boards having to understand and act on the command set — is what made
two boards painful.

We are going back to two boards because the control link wants to be the board's own
native USB (a clean CDC endpoint to the control host, no USB-serial dongle) while the
target link stays clean device-mode HID. The KB2040 has one USB port, so you cannot have
both on one board.

**A second motivation (rev. 2026-06-14): consolidation.** Once the control board is the
independently-powered, host-facing supervisor, it is the natural home for *everything* the
bench needs to drive a target — not just HID, but the DUT's **serial console** and its
**power**. Folding all three onto one USB-attached device means one cable to the control
host replaces a HID rig *plus* a USB-serial dongle *plus* a smart plug. That consolidation
is the point; §6 and §7 are how the console and power earn their place without breaking the
dumb-pipe rule.

**The "dumb pipe" decision is what keeps this from repeating the old trap.** Instead of
both boards parsing an opcode table, *no* board interprets HID semantics:

- the **host** composes the actual HID report bytes (keycodes, US layout, abs/rel mouse
  math) — the composition lives in exactly one place;
- the **control** board routes frames by a one-byte type tag and forwards HID frames
  downstream verbatim;
- the **target** board relays report bytes straight to `send_report` and never inspects
  them.

There is still a binary protocol on the wire, but there are no duplicated opcode tables,
because the only thing that understands the events is the host daemon. The console bridge
and the power relay (added later) follow the same rule: the control board *moves bytes* and
*toggles a pin* but interprets nothing — all meaning lives on the host.

## 2. Topology & power domains

```
[Control host]
      |
      |  USB (CDC "data") — type-tagged binary frames        <-- powered by control host
      |    0x01 HID  ·  0x02 control/power  ·  0x03 DUT console (bidirectional)
      v
[Control KB2040]  ... owns: frame routing, DUT power relay, UART console bridge, (later) target reset
      |  |  |
      |  |  `-- relay GPIO ----------> [ DUT power ]   (hard-cut load switch on the DUT's 5 V)
      |  |
      |  `----- UART0 (GP0/GP1) <----> [ DUT serial console ]
      |
      |  inter-board link — I2C1 (GP10=SDA, GP19=SCL, addr 0x41)   [as built; §9]
      v
[Target KB2040]  --- native USB (device-mode HID) ---> [Target machine / DUT]
                                                         ^
   powered by the DUT's USB  <-----------------------------
```

Power domains matter and are asymmetric:

- **Control board** is powered by the **control host's** USB. It stays alive across a DUT
  power cycle — which is exactly why it is the right place to *own* DUT power, the console
  bridge, and recovery.
- **Target board** is powered by the **DUT's** USB (confirmed). It boots and dies with the
  DUT. Two consequences: the inter-board link spans a powered side (control) and an
  unpowered side (target) whenever the DUT is off — see the back-powering caution in §7 —
  and a relay-driven DUT power-cut (§7) also drops the HID board, which re-enumerates as the
  DUT boots back up.

## 3. The dumb-pipe decision (what moves where)

| Concern | Before (role-based ASCII firmware) | After (dumb pipe) |
|---|---|---|
| Keycodes / US layout / `type "string"` | firmware (`adafruit_hid`) | **host daemon (Rust)** |
| Abs/rel mouse math, button state | firmware | **host daemon** |
| Command parsing | both boards | **host daemon** composes frames; boards don't parse |
| Target board's job | parse line → adafruit_hid calls | **relay report bytes → `send_report`** |
| Control board's job | forward every line verbatim | **route by type byte**: relay HID, handle control/power, bridge console bytes |
| HID report descriptor | firmware (`boot.py`) | firmware (`boot.py`) — now the host↔rig **contract** |

The target board no longer depends on `adafruit_hid` at all; it needs only the core
`usb_hid` device (the descriptor) and `send_report`. The console bridge and power relay
(§6, §7) extend the control board's "route, don't interpret" role rather than adding any
new interpretation: it shovels console bytes between the UART and the CDC, and it toggles a
relay GPIO on command, but it parses neither.

## 4. Responsibilities

**Host daemon (Rust, `hidrig serve`).** Owns the single CDC port and multiplexes everything
over it. Composition layer: translate the existing v1 ASCII commands (`type`, `key`, `move`,
`moveabs`, `click`, …) into HID report bytes and `0x01` frames. Control layer: emit `0x02`
control/power commands and read their replies. **Console layer (new):** demultiplex the
`0x03` console stream coming up from the DUT and re-export it as a PTY so panio's `serial`
channel can attach (§6). Everything *above* the daemon is unchanged — `hidrig` one-shots,
`panio hid send`, the web console — so this stays internal to the daemon and firmware.

**Control board.** Presents a USB CDC ("data") interface to the control host and is the
rig's independently-powered supervisor. It demultiplexes type-tagged frames:

- `0x01` HID frames → relayed downstream over I2C1 verbatim (no interpretation);
- `0x02` control/power → handled locally and answered (version, ping, **DUT power
  on/off/cycle** via the relay GPIO — §7);
- `0x03` DUT-console bytes → bridged to/from the **hardware UART (UART0, GP0/GP1)**: bytes
  read from the DUT console are framed and sent upstream; `0x03` bytes from the host are
  written to the UART TX. Like the HID relay, this is a pure byte pipe — the control board
  never interprets console content.

*Hardware UART, not PIO.* The console uses one of the RP2040's two hardware UARTs (UART0 on
the free GP0/GP1 pads), deliberately not a PIO soft-UART. A hardware UART has a background
FIFO, so a DUT dumping a boot log at speed does not drop bytes while the same single-threaded
firmware loop is also relaying HID over I2C. PIO would be the escape hatch only if the board
ran out of hardware UARTs or needed the line on pins that don't map to one — neither applies
here. (See §9 for the loop-headroom watch item.)

*Control-board pins.* GP10/GP19 = I2C1 (SDA/SCL, addr `0x41`); GP17 = status NeoPixel;
GP0/GP1 = UART0 (DUT console); plus one free GPIO as a `digitalio` output for the relay.

**Target board.** Device-mode HID to the DUT. Reads frames from the inter-board link and
calls `send_report(payload, report_id)` — nothing else. Holds the HID descriptor that
defines what reports are legal; the host must compose to match it exactly. Unchanged by this
revision (the console and relay both live on the control side).

**Inter-board link.** Carries the same framed HID bytes over **I2C1** (GP10/GP19, addr
`0x41`; §9).

## 5. Wire contract

### Frame format (over the single CDC "data" stream)

```
[type][report-id][len][payload .. len bytes]   type 0x01 = HID report     (host → control → target)
[type][cmd][args ..]                           type 0x02 = control/power   (host ↔ control)
[type][port][len][payload .. len bytes]        type 0x03 = serial console  (host ↔ control, both ways)
```

- **HID report frame** (`0x01`): `report-id` selects the report (1 = keyboard, 2 = absolute
  mouse, matching `boot.py`); `len` + `payload` are the raw report bytes. The relay reads
  `report-id` and `len`, then calls `send_report(payload, report_id)` and never interprets
  `payload`. Carrying an explicit `len` (rather than deriving it from `report-id`) keeps the
  relay descriptor-agnostic and lets the descriptor grow without firmware changes.
- **Control frame** (`0x02`): `cmd` ∈ { power on/off/cycle, version, ping, … (target reset
  later) }. These are rare and fallible, so they keep a **synchronous request → reply**.
  **DUT power is a control command here**, not a second USB endpoint, so it shares the
  single CDC owner (the daemon); a `hidrig power …` one-shot routes through the running
  daemon exactly like a HID one-shot, so there is no port contention (§7).
- **Console frame** (`0x03`): a raw byte stream chunked into frames in **both** directions —
  host→control bytes go to the DUT's UART TX; control→host frames carry DUT UART RX. `port`
  is a console selector (0 = the one UART today) reserved so a second target UART could be
  added without a new type. `len` ≤ 255; larger reads split across frames. Like HID, console
  frames are fire-and-forget (no per-frame ack); the single link preserves ordering.

Note the new **upstream** traffic: previously control→host carried only `0x02` replies; it
now also carries a continuous `0x03` console stream, so the daemon's reader loop must
demultiplex by type rather than assume every inbound frame is a reply.

### Resync after a partial frame

Both boards keep a length-prefixed parse buffer (`_rxbuf` in firmware, the daemon's
`split_frames` on the host) that can be left mid-frame if whatever was writing to it stops
partway through: a daemon killed between a frame's header and its payload, or a host process
crashing mid-write to the control board. A parser that trusts a stale partial frame forever
completes it with whatever bytes arrive next — from an unrelated later session — pressing or
releasing usages that were never actually sent. Two mechanisms close this, one on each side:

- **Host → control board: a resync preamble on every open.** Before writing anything else,
  the host writes 258 zero bytes (3 header bytes + the longest possible payload, 255) to the
  control board's data CDC endpoint (`hidrig::proto::open_port`/`RESYNC_PREAMBLE`, used by
  both the daemon and the one-shot CLI path). `0x00` is not a frame type, so a parser already
  in sync skips every byte of it as unframed noise; a parser left holding a stale partial
  frame by the *previous* owner has that frame completed with an all-zero payload — a
  keyboard report with nothing pressed, a pointer report with no buttons — rather than with
  whichever real bytes happen to follow. This alone only protects the control board (the one
  the host writes to directly), and only if the control board hasn't already given up on the
  stale frame — see the next mechanism.
- **Firmware: discard a partial frame after 50 ms.** Both `control/code.py` and
  `target/code.py` track when the first byte of the currently-pending (not yet complete)
  frame arrived (`_rxbuf_started`) and drop the buffer (`discard_stale_rxbuf`) if more than
  `PARTIAL_FRAME_TIMEOUT_S` (50 ms) passes without it completing. This is what makes the
  preamble effective in the first place — the preamble's zero bytes only resync a parser that
  is *waiting* for them; a board that never discards a stale frame would instead have the
  preamble complete that frame's payload with zeros and then resync cleanly afterward, which
  is still safe, but only by luck of the frame's declared length being short enough for the
  preamble to fully cover it. The 50 ms discard is what makes stale state bounded regardless
  of that timing, and it is also the target board's *only* defense — the control→target I2C
  link carries no preamble of its own (a `0x01` HID frame is written to `i2c.writeto` as a
  single already-complete transaction; the risk there is a physically interrupted I2C write,
  not a killed host process), so a partial frame that timing out is the sole guard against a
  glitched write completing wrong. **Unverified on hardware** — the firmware change has not
  been flashed and exercised on a bench; it needs a bring-up pass (kill the daemon mid-frame,
  restart it, confirm the target does not register a spurious keystroke) before it is trusted.

### The descriptor is a shared constant

Because the host composes reports, the host composer must match the target board's HID
descriptor **exactly** (report IDs, field order, the 0..32767 absolute range). The
descriptor lives in [`hidrig-kb2040/firmware/target/boot.py`](https://github.com/curtisgalloway/paniolo-hardware/blob/main/hidrig-kb2040/firmware/target/boot.py)
in the paniolo-hardware repo — a change there requires a matching change to this repo's
`hidrig/src/compose.rs`. Treat it as a versioned contract: the
`version` control command should let the daemon confirm it is talking to a compatible
descriptor before composing.

### Flow control

- **HID frames are fire-and-forget** — no per-frame ack. The host paces them to the
  downstream HID poll interval (`bInterval`); it must not exceed the rate the target board
  can drain into the DUT, or a bounded buffer backs up. This is what removes the I2C
  reply-synchronization problem from the hot path.
- **Console frames are fire-and-forget** in both directions; the hardware UART's FIFO plus a
  small firmware buffer absorb bursts. The host side is rate-limited by the UART baud, not by
  the CDC link.
- **Control frames keep request/reply.** Rare, fallible, and not latency-critical, so the
  awkward "poll a buffer, strip `0xFF` fill" dance on I2C only ever runs here, never on
  streaming mouse motion or console output.

## 6. Serial console bridge

The control board's hardware UART (UART0, GP0/GP1) connects to the DUT's serial console.
Console traffic rides the **same single CDC "data" stream** as everything else, namespaced
as `0x03` frames (this resolves the one-stream-vs-two-endpoints question in favor of one
stream — see §9). The host daemon owns the CDC port, demultiplexes `0x03`, and must
**re-export the console so it plumbs into panio's existing `serial` channel with no
panio code changes.**

### Why a PTY (the constraint that drives the design)

panio's `serial` channel is **device-path-only**. `serialcap` hands the configured
`device` straight to `tokio_serial::new(device, baud).open_native_async()` — there is no
socket, command, or network backend. And `panio serial connect` `exec`s `tio <device>`,
which needs a real terminal device, not a socket. So the only re-export that satisfies the
*entire* serial surface — `watch`, `send`, `log`, **and** interactive `connect`/tio — with
zero panio changes is a **pseudo-terminal (PTY)**:

- the `hidrig` daemon allocates a PTY, writes demuxed `0x03` bytes to the master, and reads
  host→DUT input from the master back into `0x03` frames;
- it publishes the slave at a **stable symlink** under `/tmp/panio-<uid>/…` (the same
  discovery convention the hid daemon already uses for `daemon.json`), because PTY slave
  names are allocated dynamically;
- the lab file points the target's serial channel at that symlink:

  ```toml
  [[targets.DUT.serial]]
  name   = "console"
  device = "/tmp/panio-<uid>/hid/console"   # hidrig PTY symlink, not a physical tty
  baud   = 115200                              # nominal; the real rate is set on the UART
  # no power_sense_signal — modem-control lines don't exist on a PTY (see caveats)
  ```

panio then opens it exactly like a USB-serial dongle: `serial watch` captures it,
`serial send` types into it, `serial log` reads the capture, `serial connect` attaches tio.
The framing is honest: **hidrig becomes "a USB-serial adapter that also does HID and
power."**

### Caveats

- **Modem-control signals don't exist on a PTY.** `serial dtr` / `serial reset` and
  `power_sense_signal` are meaningless on this channel — which is fine, because
  power-cycling is now the relay's job (§7), not a DTR pulse. Do **not** configure
  `power_sense_signal` on a hidrig console channel.
- **PTY lifecycle.** The symlink must follow the master across daemon restarts, and
  serialcap's reopen loop must tolerate the master disappearing when the daemon restarts.
  The hid daemon's discovery-file pattern is the model for keeping the path stable.
- **Verify before relying on it.** That `tio` and serialcap's `tokio_serial` open + reopen
  loop behave correctly on a hidrig-provided PTY is asserted here, not yet confirmed — a
  short spike should validate it before this is wired into a real bench. If a PTY proves
  fragile, the fallback is teaching serialcap a backend (a ~100-line change), but that would
  not cover interactive `tio`, so the PTY is the preferred path.

### Remote control hosts

The PTY lives on the **control host** (where the USB device is). panio's existing SSH
dispatch ships a single-target lab slice to that host and runs `serialcap` there against the
local PTY, so a remote console works identically to a remote physical serial device — set
`host =` on the serial channel as usual.

## 7. Power & recovery

**DUT power-cycle (the immediate goal).** The control board switches power to the DUT. This
overlaps with panio's existing power helpers (zigplug/shellyplug) by design — it is a
*backend swap*, not a new core concept. It surfaces to panio as **another power-helper
behind the existing `power` hook**, keeping device-specific logic out of the core. It is
worth doing where it *replaces something flaky or consolidates cabling* — e.g. the pi5
bench currently runs on a Zigbee plug that is finicky (formation fails near USB video,
NVRAM-recovery drama); a load-switch on the pi5's 5 V that the control board owns is a
reliability upgrade and collapses the rig toward one cable.

- **Mechanism — hard cut.** A control-board GPIO (`digitalio` output) drives a power
  load-switch / relay on the DUT's 5 V. A soft ATX front-panel pulse was considered and
  rejected: it can't rescue a wedged box, which is the whole point of owning power.
- **Host interface.** Exposed as `hidrig power off|on|cycle`, carried as a `0x02` control
  command. The one-shot routes through the running daemon (the single CDC owner), so it does
  not contend for the port — the same pattern HID one-shots already use.
- **Sizing caveat:** a Pi 5 can pull ~5 A at 5 V, so the switching element is a real power
  load-switch / relay, **not** a bare GPIO driving the rail directly.
- **HID-board coupling.** Because the target board is DUT-powered, a power-cut also drops the
  HID board; it re-enumerates as the DUT boots. This is acceptable and even realistic (the
  DUT sees its keyboard/mouse appear as it powers up), but note that "power cycle" implies
  "HID re-enumerate." A consequence the firmware must handle: dropping the target board also
  removes any target-side I2C pull-ups, so the control board's **I2C init is non-fatal** —
  the relay and console come up regardless and the HID relay simply pauses while the bus is
  down. (Verified on the bench: without this, a `power off` crashed the control firmware.)
- **State persistence.** The relay state is stored in the control board's **NVM** and
  restored at boot, so a control-board reset (replug / code reload / host reboot) does not
  force the DUT back on — it resumes the last commanded state (default on if NVM is erased).
  A *hardware* reset still briefly floats the GPIO (~300 ms) before restore; glitch-free hold
  would need a latching relay or a de-energized-default that matches the safe state.

**Target-board reset (DEFERRED, §10).** A control-board GPIO → target board `RUN` pin would
re-enumerate the HID board *without* rebooting the DUT, and works out-of-band even when that
board is wedged and ignoring the link. Deferred, but noted as useful.

**Recovery hierarchy.** Host owns/recovers the control board; the (independently powered)
control board recovers everything below it — DUT power-cycle for a hung machine, and (later)
target-board reset for a wedged HID link. Two distinct tools for two independent failures.

**Cross-power-domain caution.** Because the target board is DUT-powered, any line the
control board drives toward it (the inter-board I2C link today; the `RUN` reset line later)
sits between a powered and an unpowered chip while the DUT is off. A GPIO driven high into an
unpowered RP2040 can leak through its protection diodes and back-power it. Design for this:
drive control→target lines low / tristate them while the DUT is off, and reference the I2C
pull-ups deliberately (control side). The DUT-side UART (GP0/GP1) and the relay sit in the
DUT's own power domain, so they don't add a new cross-domain leak path beyond the existing
inter-board link.

## 8. What's settled

1. **Dumb pipe.** Composition on the host; firmware relays raw report bytes. No duplicated
   opcode tables — this is the answer to why the old two-board design was abandoned.
2. **Two boards, native USB both ends.** Control board vends CDC to the host; target board
   vends device-mode HID to the DUT.
3. **External interface unchanged.** panio and `hidrig` keep speaking v1 ASCII to the
   daemon; only the daemon↔rig wire format is binary frames. The console reuses panio's
   existing `serial` channel as-is.
4. **One CDC stream, namespaced by a type byte** — `0x01` HID, `0x02` control/power, `0x03`
   console — with the daemon as the single port owner.
5. **DUT power is owned by the control board** as a hard-cut relay on a GPIO, exposed as
   `hidrig power …` behind the existing power hook.
6. **DUT serial console is bridged on the control board's hardware UART** (UART0, GP0/GP1),
   multiplexed as `0x03`, and re-exported by the daemon as a **PTY** that panio's `serial`
   channel attaches to — no panio code changes.
7. **Blast radius is daemon + firmware (+ lab-file config) only** (see §11).

## 9. Open decisions

1. **Inter-board transport — RESOLVED: I2C1.** Wired and working at GP10=SDA / GP19=SCL,
   addr `0x41`, 100 kHz (the design-time lean was UART, but I2C1 is what shipped). Revisit
   only if the rig fans out to **multiple target boards** (addressing).
2. **Target-board firmware — CircuitPython vs Embassy — still open.** The dumb-pipe change
   removed `adafruit_hid`, so the only remaining driver is `bInterval` headroom (sub-8 ms HID
   poll → above ~125 reports/s). **Verify first** whether CircuitPython lets you change
   `bInterval` and whether the DUT honors `bInterval = 1`. Unaffected by this revision — the
   console and relay live on the *control* board.
3. **Host-side CDC topology — RESOLVED: one stream**, namespaced by the type byte. The
   console (`0x03`), HID (`0x01`), and control/power (`0x02`) all want the daemon as a single
   demuxing owner anyway, and power one-shots route through the daemon, so a second CDC
   endpoint buys nothing — and it would cost the `console` CDC, which stays the dev REPL.
4. **DUT power mechanism — RESOLVED: hard-cut relay/load-switch** on a control-board GPIO
   (see §7).
5. **Control-board loop headroom — open / watch.** The firmware loop now interleaves three
   jobs: relay HID over I2C, drain UART RX → `0x03`, and write `0x03` → UART TX. The hardware
   UART's background FIFO covers RX, but a fast DUT boot-log concurrent with HID streaming is
   the scenario most likely to expose loop starvation. If it bites, it is the lever that
   pushes the control board to Embassy/TinyUSB — which would *also* be the only way to give
   the console its own native CDC port and drop the multiplexing.
6. **PTY re-export verification — open.** Confirm `tio` and serialcap behave on a
   hidrig-provided PTY before wiring it into a real bench (§6 caveats).

## 10. Non-goals / deferred

- **Target-board reset line** (control GPIO → `RUN`). Deferred; design the cross-domain
  back-powering protection (§7) when it's added.
- **Multidrop / multiple target boards.** Only if fan-out is needed; would re-open the
  transport decision (§9.1) toward I2C addressing, and would exercise the `0x03` `port`
  selector for a second console.
- **Dedicated native CDC for the DUT console.** Giving the console its own host serial port
  (no multiplexing) needs a second/third CDC interface, which stock CircuitPython does not
  offer — it would require rebuilding CircuitPython in C or moving to Embassy/TinyUSB. Not
  pursued now; noted as the escape hatch in §9.5.
- **Custom vendor USB bulk endpoint.** Not needed and not pursued. CircuitPython's
  `usb_cdc.data` is *already* a bulk endpoint pair — raw binary frames ride it fine; the
  ASCII convention was never a constraint. A true vendor/driverless endpoint would require
  rebuilding CircuitPython in C or moving to Embassy, which the throughput case does not
  justify (the ceiling is the downstream `bInterval`, not the host-side link).

## 11. Blast radius

```
unchanged: panio `hid send` · `hidrig` CLI · web console · hid-serial-protocol.md (v1 ASCII)
           panio `serial` CLI (watch/send/log/connect) — attaches the hidrig PTY as-is
           |
           v
  CHANGED: host daemon  -- composition (keycodes/layout/mouse math) + binary framing
           |               + 0x03 console demux & PTY re-export + 0x02 power command
           |
           |  binary frames over CDC  (0x01 HID · 0x02 control/power · 0x03 console)
           v
  CHANGED: control firmware  -- route by type byte; UART console bridge; relay GPIO
           |
           |  HID frames over I2C1 (GP10/GP19, addr 0x41)
           v
  CHANGED: target firmware   -- relay -> send_report  (unchanged by this revision)

  CONFIG ONLY: lab file gains a `serial` channel pointing at the hidrig PTY symlink
               (and a `power` hook pointing at `hidrig power`) — no panio code change
```

Nothing above the daemon moves. The daemon absorbs the protocol translation, the console
demux, and the power command; the control firmware gains the UART bridge and relay; panio
gains only lab-file configuration. The change stays contained to the daemon and firmware.
