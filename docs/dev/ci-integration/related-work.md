# Related work: panio vs. labgrid

> Where panio sits relative to the closest existing tool, **labgrid** (Pengutronix), and
> why panio exists alongside it. labgrid is the most relevant point of comparison because it
> occupies the **same layer** panio does: a Python device-control abstraction that sits
> *under* a test framework and does **not** produce verdicts itself.
>
> labgrid facts below are from the project's canonical sources (README/`doc/*.rst` on the
> `master` branch, the v25.0 release notes, and CHANGES) verified May 2026; `readthedocs.io`
> blocked automated fetch, so a few specifics reflect `master` rather than the exact v25.0.1
> tag. Where a claim was not confirmable it is flagged. See [`design.md`](design.md) and
> [`gap-analysis.md`](gap-analysis.md) for panio's CI device-control design that this
> comparison informs.

## TL;DR

**labgrid is the mature, broad, distributed board-farm standard. panio is a focused,
agent-first, single-host tool with capabilities labgrid deliberately does not have (on-device
OCR, HID injection, macOS support).** They agree on the fundamentals — and labgrid's design
choices independently *validate* the direction of panio's CI-integration work (raw-socket
serial, discrete power verbs, a driver/protocol abstraction). Panio's reason to exist is not
to out-feature labgrid on hardware drivers; it is to serve the **single-target,
agent-in-the-loop bring-up** niche that labgrid under-serves.

## At a glance

| | **panio** | **labgrid** |
|---|---|---|
| Origin / maintainer | Personal project (C. Galloway), 2026 | Pengutronix, since 2016; widely used in industry |
| License / language | Apache-2.0; Python + Rust + Swift | LGPL-2.1; Python |
| Layer | Device-control "wrangling" layer | Device-control / hardware-abstraction layer (explicitly *not* a test framework) |
| Produces verdicts? | No (by design) | No — pytest produces them |
| Topology | **Single control host, SSH-driven**; no central server | **Distributed**: coordinator + exporter(s) + client(s) |
| Transport | Plain SSH + per-daemon HTTP/WebSocket | **gRPC** (since v25.0, May 2025; previously crossbar/WAMP) + direct client→exporter SSH for the data plane |
| Multi-user | None (single host) | Coordinator-enforced **places**: acquire/lock + reservations |
| Host OS | **macOS + Linux** | **Linux only** (Debian; exporter requires `ser2net`) |
| Driver breadth | ~5 subsystems, hand-built | **Dozens** of drivers (power/serial/boot/flash/JTAG/mux/instrumentation) |
| Primary use | AI agent over SSH; interactive bring-up | pytest plugin; board farms / CI |

## Architecture & philosophy

**labgrid is distributed-first.** A board is a `Target` composed of **Resources** (passive
access info) and **Drivers** (active, implementing abstract **Protocols** such as
`ConsoleProtocol`, `LinuxBootProtocol`, `ResetProtocol`, `BootstrapProtocol`). Remote boards are
modeled as **Places** that clients/CI **acquire, lock, and reserve**; a central **coordinator**
(now a gRPC server) is a registry + mutual-exclusion authority, while the actual control/data
plane runs **directly client→exporter over SSH**. The exporter runs on the host physically wired
to the boards. This is built for many boards, many hosts, many users.

**panio is single-host-first.** No coordinator, no daemon-of-daemons: the `panio` binary
plus per-subsystem daemons on one control host, driven by an agent over SSH, with state in flat
files (see [`../architecture.md`](../architecture.md)). It trades labgrid's scale for **zero
infrastructure** and a tight agent-in-the-loop loop.

Notably, panio's planned **"verbs backed by per-target backends"** (see [`design.md`](design.md))
is effectively a rediscovery of labgrid's **Protocol/Driver** split — labgrid is evidence that
the abstraction is the right call.

## Device-control primitives, head-to-head

- **Power** — labgrid is broader: discrete on/off/cycle via many `*PowerDriver`s (network PDUs,
  YKUSH/USB-port power, GPIO, Tasmota/MQTT, `ManualPowerDriver`, and `ExternalPowerDriver` for
  custom scripts). panio has `power-cycle` plus the discrete `power on/off` verbs (generic
  `[power]` hook commands, with the `cambrionix`/`zigplug`/`shellyplug`/`amt` helpers), DTR power-button, and
  `power-state`. labgrid's `ExternalPowerDriver` ≈ panio's power hooks.
- **Serial (the key alignment)** — labgrid exposes serial as a **`NetworkSerialPort`**: the
  exporter runs **ser2net** to publish the UART as a **raw TCP / RFC2217 bidirectional stream**,
  which CI binds a `SerialDriver` to over SSH forwarding. **This is the same shape panio is
  adding** (a raw TCP listener, with ser2net-on-PTY as the fallback — see [`design.md`](design.md)).
  labgrid independently validates "raw serial over a socket" as the correct primitive. labgrid has
  **no** timestamped JSONL capture log or agent-OCR pairing — panio's serial differentiators.
- **Deploy/boot** — labgrid is far richer: `UBootDriver`/`BareboxDriver`, USB bootstrap loaders
  (i.MX/MXS/Rockchip), fastboot, `TFTP/NFS/HTTPProviderDriver`, flashing (`flashrom`, Dediprog),
  SD-mux storage write. panio has one path (pure-Python DHCP+TFTP netboot) and its CI design
  **cedes deploy to the orchestrator** anyway.
- **Debug/JTAG** — labgrid ships an `OpenOCDDriver` today; panio has only a *design stub*
  (`[jtag]` extension point). Both are OpenOCD-centric; neither has a deep first-class gdb
  workflow (labgrid's gdb story is not clearly documented).
- **Video/OCR** — **panio's clear edge.** labgrid streams USB video (`USBVideoDriver`,
  GStreamer) but does **no frame analysis**. panio's warm-stream HDMI capture + **on-device
  OCR** (Apple Vision / Tesseract) for reading boot/console screens has no labgrid equivalent.
- **HID injection** — **panio only.** labgrid's `HIDRelay` is relay control, not keyboard/mouse
  emulation. panio's KB2040 two-board HID-injection rig is a capability labgrid does not offer.

## CI / orchestration

Both are device-control layers beneath a verdict producer. labgrid is **complementary to LAVA**
(not a replacement) — strong on complex hardware LAVA struggles with, and uniquely lets the
*same* boards serve interactive development and CI; results come from its mature **pytest
plugin** (JUnit/HTML output). A third-party "FC" framework coordinator even lets LAVA and labgrid
share a board farm. By contrast, panio's orchestrator integration (LAVA device-dictionary,
botanist device-config) is still **in design** (see [`design.md`](design.md)). No authoritative
labgrid↔Fuchsia/botanist relationship surfaced in the sources — that integration is open water
panio is heading into.

## Why panio (alongside labgrid)

Panio is not trying to replace labgrid or beat it on driver breadth. Its reasons to exist:

1. **Agent-first ergonomics.** Designed to be driven by an AI agent over SSH for iterative
   bring-up, with machine-readable serial capture (timestamped, sequence-numbered JSONL) and
   OCR feedback loops — not primarily a pytest fixture library.
2. **Capabilities labgrid lacks:** on-device **OCR** of the screen, a **USB-HID injection** rig
   (real keyboard/mouse emulation), and a combined **video+serial dashboard** for the human/agent
   in the loop.
3. **Zero-infrastructure single host.** No coordinator/exporter/client to stand up; one wired
   control host and you're going. Lower friction for one board than labgrid's distributed model.
4. **macOS support.** labgrid's exporter is Linux/Debian-only; panio runs first-class on macOS
   (as well as Linux), which matters for developers on Mac workstations doing bring-up.

The honest counterpoint: for a **multi-board, multi-user board farm**, labgrid is the right tool
today and panio is not trying to be. Panio should lean into the single-target,
agent-in-the-loop niche labgrid under-serves.

## Open strategic question

Because the two share a layer, labgrid can be treated as a **competitor**, a **model**, or an
**interop target** — and panio's serial work makes interop concrete: panio's raw-TCP serial
listener is **wire-compatible with the ser2net stream labgrid already consumes**, so panio
could plausibly present a labgrid-compatible serial (and power) surface, positioning itself as
the agent-friendly, OCR/HID-equipped front end over hardware that a labgrid farm also uses. This
is recorded as a question, not a decision.

---

## Redfish interop (provider direction)

A second, **higher-value** interop target is **Redfish** (DMTF's vendor-neutral hardware-
management standard) — and unlike labgrid it is an *orchestrator-facing* surface, not a peer at
panio's own layer. The decided direction is **provider**: panio exposes a Redfish API *in
front of* BMC-less boards (so it effectively *becomes the BMC*), rather than acting as a Redfish
client driving existing BMCs. Full design sketch: [`redfish-provider.md`](redfish-provider.md);
tracked as **RF-\*** in [`../requirements.md`](../requirements.md) §9.6.

**Why it's compelling, in brief:**

- **Clean mapping for 3 of 4 primitives** onto work panio already has or is building:
  `#ComputerSystem.Reset` + `PowerState` ← the discrete power verbs (PWR-1..6, already on the
  LAVA roadmap); `Boot.BootSourceOverrideTarget=Pxe`/`Once` ← netboot; `VirtualMedia
  Insert/Eject` ← image deploy.
- **It subsumes adapters.** Redfish is the de-facto bare-metal control plane (Ironic's and
  Metal3's *primary* one; LAVA can `curl` it). One Redfish face lets *anything that speaks
  Redfish* drive a BMC-less panio board — potentially higher leverage than the per-ecosystem
  LAVA/botanist adapters.
- **Direct precedent:** **sushy-tools / sushy-emulator** already exposes Redfish in front of
  BMC-less VMs via a pluggable systems-driver design; panio would be the same pattern over real
  hardware. The recommendation is to **back a sushy-tools-style emulator with a panio backend
  driver**, not hand-roll the OData service.
- **Same serial caveat, same answer:** Redfish carries no serial bytes — `SerialConsole` only
  advertises a separate SSH/IPMI-SOL endpoint. That's the identical control-plane/console split
  as LAVA, botanist, and OpenBMC, so panio's raw-serial socket (SER-1) is the one serial answer
  the Redfish face just points at.

**labgrid vs. Redfish, as interop targets:** labgrid is **passive wire-compat** (free; a labgrid
farm could point its existing ser2net / `ExternalPowerDriver` at panio with no panio-side
work) and a **validation reference**. Redfish-provider is an **active build** worth doing —
sequenced after the M1 device-control core, since it consumes the same power verbs and serial
socket.
