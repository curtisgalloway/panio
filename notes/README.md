<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# notes/

Point-in-time documents: the design a feature was built from, a hardware
bring-up's findings, a plan that has since shipped.

**These are records, not documentation.** They describe what was true when they
were written and are not maintained against the current code. Nothing here is
published to the docs site. Documentation of panio's verified current state
lives in [`docs/`](../docs/README.md) — user guides at the top level, developer
documentation under [`docs/dev/`](../docs/dev).

If you are looking for how panio works *today*, start at
[`docs/README.md`](../docs/README.md).

| Note | What it records |
|---|---|
| [config-redesign.md](config-redesign.md) | The lab data model, CRUD command surface, and per-channel dispatch design, plus the Python→Rust pivot. Shipped. |
| [distributed-control-plan.md](distributed-control-plan.md) | The original Python-era phased build sequence for distributed control. Phases 0–5 shipped. |
| [uefi-http-boot-design.md](uefi-http-boot-design.md) | The design netbootd's UEFI PXE / HTTP Boot support was built from. Shipped behavior is in [docs/netboot.md](../docs/netboot.md). |
| [ch9329-spec.md](ch9329-spec.md) | Clean-room protocol spec for the WCH CH9329 HID bridge, written before the `ch9329` helper. Implemented. |
| [openterface-kvm-go.md](openterface-kvm-go.md) | Bench findings for the Openterface KVM-Go (2026-08-24): both panio channels work unmodified. |
| [openterface-deep-control.md](openterface-deep-control.md) | Deep-control findings for the Openterface Mini-KVM, with the parts that remain unverified or blocked. |
| [openterface-usb-mux-spec.md](openterface-usb-mux-spec.md) | Clean-room protocol spec for switching the Openterface USB mux: the KVM-Go microSD serial command (hardware-verified 2026-08-30) and the Mini-KVM USB-A register write (unblocked, untested). |
| [vm-targets-and-rfb.md](vm-targets-and-rfb.md) | Whether a VM should be a first-class panio target, and the RFB/VNC question underneath it (2026-08-30, measured on hardware 2026-09-10). Decided: no `vm` channel kind; the real decision is RFB as a second transport for video and HID, which also serves BMCs and remote KVMs. The video half was then measured against AMT KVM on a real machine and the case for it weakened — read the postscript before costing any of it. |
| [host-peer.md](host-peer.md) | Letting panio own a host peer's privileged traffic (#325, 2026-10-21): an fd handed from a capability-bearing helper for raw Ethernet, a udev rule for USB. Design, not built. |
| [network-video-rfb.md](network-video-rfb.md) | Network video sources via RFB (milestone 2, first consumer JetKVM): why RFB is the common ingest format, WebSocket transport, ffmpeg decoding, and the deferred live-view (egress) options. Built and hardware-verified 2026-10-07. |
| [console-front-door.md](console-front-door.md) | A parked design: one stable port with server-side fan-out for the remote dashboard. Not built. |
| [desktop-app.md](desktop-app.md) | A Tauri desktop app as the lab's front door: a thumbnail grid of every target, each dashboard in its own window (2026-09-14). Not built. Argues it is console-front-door.md's Phase C in a different runtime and inverts that note's phasing. One Tauri assumption gates the architecture and is unverified — read *Verification status* before starting. |
| [mcp-server.md](mcp-server.md) | A plan for `panio mcp`: a stdio MCP server on the dev machine, with persistent per-host workers and reconnect rules (2026-10-02). Not built. Measured: per-call dispatch adds ~17 ms on a LAN, so the worker is deferred. |
| [control-host-provisioning.md](control-host-provisioning.md) | The cloud-init seed recipe for standing up a Linux control host (x86 or Pi) from blank hardware, plus the hardware-validated findings from the first `pi-sd` build (a Pi 5 on Pi OS Trixie, 2026-08-20). The `pi-sd` seed now ships in [`packaging/host-seed/`](../packaging/host-seed); the x86 flavor and the generator are not built. |
| [pi4-control-host.md](pi4-control-host.md) | The original bring-up plan for a self-contained Pi control host. Largely superseded — see control-host-provisioning.md; the unbuilt part is the Linux USB-HID-gadget backend. |
