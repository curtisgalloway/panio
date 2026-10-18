# Panio as a Redfish provider — design sketch

> Should panio expose a **Redfish** API in front of BMC-less boards, so any Redfish client
> (Ironic, Metal3, Redfish-capable LAVA configs, ad-hoc scripts) can drive a panio target as
> if it were a managed server? This sketch makes the case, maps the contract onto panio's
> primitives, and records the open questions. **It is a design sketch, not a committed plan** —
> sequenced after the M1 device-control core (see [`design.md`](design.md) and
> [`requirements.md`](../requirements.md) §9.6 RF-*).
>
> Direction is decided: **provider** (panio *is* the BMC for a BMC-less board), **not client**
> (panio driving existing BMCs). Redfish facts verified May 2026 against the DMTF canonical
> CSDL schemas (`github.com/DMTF/Redfish-Publications`), OpenBMC, and OpenStack Ironic/sushy
> docs; DMTF's rendered pages 403'd automated fetch, so enum lists come from the machine-readable
> CSDL source of truth.

## 1. Why a Redfish provider (and why provider, not client)

Panio exists for boards that have **no BMC** (Raspberry Pi, dev boards) — power is a relay /
USB-PD / PDU, serial is a UART/USB-TTY cable. A Redfish **client** would make panio drive
*other* machines' BMCs, which is not panio's situation. A Redfish **provider** instead makes
panio *act as the BMC* for a BMC-less board: panio already owns power, serial, and netboot —
exactly the primitives Redfish's core resources model.

There is direct precedent. **sushy-tools / sushy-emulator** (OpenStack) exposes a Redfish ReST
API in front of things that have no real BMC (libvirt VMs) and translates `ComputerSystem.Reset`,
boot-device changes, and virtual-media insert/eject into backend actions; its dynamic emulator is
a **pluggable systems-driver design** (libvirt / OpenStack / Ironic / "fake" backends). Panio
would be the same pattern, backed by **real hardware** instead of VMs.
- sushy-tools dynamic emulator: https://docs.openstack.org/sushy-tools/latest/user/dynamic-emulator.html
- repo: https://github.com/openstack/sushy-tools

**The strategic payoff:** Redfish is the de-facto lingua franca for bare-metal power/boot —
it is Ironic's and Metal3's **primary** control plane, and LAVA's generic shell power commands
can trivially `curl` a Redfish `Reset`. So a single Redfish face is an **ecosystem-agnostic
adapter that subsumes per-ecosystem power/boot adapters**: anything that speaks Redfish can drive
a BMC-less panio board. That is potentially higher leverage than the bespoke LAVA-device-dict
and botanist-config adapters individually.
- Ironic redfish driver: https://docs.openstack.org/ironic/latest/admin/drivers/redfish.html
- Metal3 BareMetalHost: https://book.metal3.io/bmo/introduction.html

## 2. What Redfish is (one paragraph)

Redfish is a DMTF standard — protocol **DSP0266** (v1.22.0), data model **DSP0268** (2025.2),
schema bundle **DSP8010** (2025.2, 2025-05-01). It is RESTful **HTTPS + JSON**, modeled with
**OData v4** (`@odata.id`, `@odata.type`). Client-server and **out-of-band**: the Redfish
*service* runs on the managed machine's **BMC**, a separate controller powered even when the host
is off. Clients make HTTPS calls to `https://<bmc>/redfish/v1/...`. Mature, quarterly-versioned,
widely implemented (OpenBMC, Dell iDRAC, HPE iLO, Lenovo XCC). It was created to replace IPMI.
- Spec index: https://www.dmtf.org/standards/redfish

## 3. Contract → panio primitive mapping

Clean for 3 of panio's 4 primitives; serial is metadata-only (§4).

| Redfish surface | Resource / action | panio backend | Roadmap status |
|---|---|---|---|
| **Power on/off/cycle** | `#ComputerSystem.Reset` action; `PowerState` read | discrete power verbs | **already built for LAVA** (PWR-1..6) |
| **Boot override (netboot)** | `Boot.BootSourceOverrideTarget = Pxe` + `BootSourceOverrideEnabled = Once` (PATCH) | netboot | exists today |
| **Deploy via image** | `VirtualMedia` `InsertMedia`/`EjectMedia` | netboot image swap / SD swap | maps to deploy |
| **Serial console** | `SerialConsole.ConnectTypesSupported` (metadata) | advertise SSH endpoint → panio raw-serial socket | needs SER-1 (raw TCP listener) |

### Power (verified enums)
Host = `ComputerSystem` (`/redfish/v1/Systems/<id>`); enclosure/PDU = `Chassis`. Power changes
POST to `#ComputerSystem.Reset` at `/redfish/v1/Systems/<id>/Actions/ComputerSystem.Reset` with a
`ResetType`. Verified `ResetType` members: `On, ForceOff, GracefulShutdown, GracefulRestart,
ForceRestart, Nmi, ForceOn, PushPowerButton, PowerCycle, Suspend, Pause, Resume, FullPowerCycle,
Sleep, Hibernate`. `PowerState` read-back: `On, Off, PoweringOn, PoweringOff, Paused, Sleeping,
Hibernating`. Panio's mapping: `On`→`power on`; `ForceOff`→`power off`; `PowerCycle`/
`ForceRestart`/`GracefulRestart`→`power-cycle`; `GracefulShutdown`→`power off` (graceful if the
backend supports it). Panio advertises only the supported subset per node via
`ResetType@Redfish.AllowableValues` / an `ActionInfo` resource — **honesty about capability is a
requirement** (RF-6), not all values are implementable on a relay/DTR board.
- ResetType / PowerState CSDL: https://raw.githubusercontent.com/DMTF/Redfish-Publications/master/csdl/Resource_v1.xml
- OpenBMC curl examples: https://github.com/openbmc/docs/blob/master/REDFISH-cheatsheet.md

### Boot / deploy (verified enums)
`Boot` object on `ComputerSystem`, set via HTTP **PATCH**. `BootSourceOverrideTarget`: `None, Pxe,
Floppy, Cd, Usb, Hdd, BiosSetup, Utilities, Diags, UefiShell, UefiTarget, SDCard, UefiHttp,
RemoteDrive, UefiBootNext, Recovery`. `BootSourceOverrideEnabled`: `Disabled, Once, Continuous`.
`BootSourceOverrideMode`: `Legacy, UEFI`. Plus `HttpBootUri` (UEFI HTTP boot). For panio, `Pxe`
+ `Once` maps to a netboot cycle. **VirtualMedia** resource: actions `#VirtualMedia.InsertMedia` /
`#VirtualMedia.EjectMedia`; properties `Image, Inserted, MediaTypes, TransferProtocolType`
(`CIFS, FTP, SFTP, HTTP, HTTPS, NFS, SCP, TFTP, OEM`). On panio this could mount/point an image
into the netboot path or swap an SD image.
- Boot CSDL: https://raw.githubusercontent.com/DMTF/Redfish-Publications/master/csdl/ComputerSystem_v1.xml
- VirtualMedia CSDL: https://raw.githubusercontent.com/DMTF/Redfish-Publications/master/csdl/VirtualMedia_v1.xml

## 4. The serial caveat (the same one, again — and that's reassuring)

**Redfish does NOT carry the serial byte stream.** `SerialConsole` (on the `Manager` resource,
mirrored on `ComputerSystem`) is **metadata only**: sub-properties `ServiceEnabled`,
`MaxConcurrentSessions`, and `ConnectTypesSupported` (enum: `SSH, Telnet, IPMI, Oem`). To get
bytes you open a **separate** SSH / IPMI-SOL / Telnet session. OpenBMC is the concrete model:
Redfish (bmcweb) for control + `obmc-console` as an independent serial channel.

This is the **same control-plane / console-bytes split** as LAVA (telnet/ser2net), Fuchsia
botanist (`$FUCHSIA_SERIAL_SOCKET`), and Ironic (which uses socat or a `redfish-graphical` VNC
console, explicitly *not* Redfish serial). So panio's planned **raw-serial socket** (SER-1) is
the single serial answer for all four ecosystems; the Redfish face merely **advertises** "SSH
here for the console" pointing at that socket. One serial refactor, four consumers.
- SerialConsole CSDL: https://raw.githubusercontent.com/DMTF/Redfish-Publications/master/csdl/Manager_v1.xml
- DMTF serial console enhancements: https://www.dmtf.org/content/redfish-serial-console-enhancements
- OpenBMC obmc-console: https://github.com/openbmc/obmc-console
- Ironic console (socat / redfish-graphical): https://docs.openstack.org/ironic/latest/admin/console.html

## 5. Recommended implementation approach

**Do not hand-roll the OData service.** Back a **sushy-tools-style emulator with a panio
"systems driver"** — its dynamic emulator is explicitly pluggable for exactly this. That turns
"implement Redfish" into "write a backend adapter" (panio-sized): the driver translates
`Reset`→`panio power …`, `Boot`/`VirtualMedia`→netboot, and populates `SerialConsole` metadata
pointing at panio's serial socket. Minimum viable resource tree: `ServiceRoot` →
`ComputerSystem` (with `Reset` action + `Boot` + `PowerState`) → `Manager` (`SerialConsole`
metadata) → optionally `VirtualMedia`.

**Sequencing:** after the M1 device-control core, because the provider *consumes* the discrete
power verbs (PWR-1..6) and the raw-serial socket (SER-1). No new core hardware work is needed —
the Redfish provider is an adapter layer over primitives panio already has or is already
building.

## 6. Verdict & open questions

**Verdict: high value, low marginal cost.** A Redfish provider is the highest-leverage adapter
because it (a) rides power-verb + serial-socket work already on the roadmap for LAVA/Fuchsia, and
(b) subsumes many consumers (Ironic, Metal3, scripts, Redfish-capable LAVA) behind one standard
face. The serial caveat is not a blocker — it matches every mature stack in this space, and
panio's serial socket already answers it.

**Open questions (recorded, not decided):**
1. **Replace or complement?** Does the Redfish provider *replace* the bespoke LAVA/botanist
   adapters or *complement* them? Redfish could shrink the LAVA adapter to "a device dict whose
   power commands `curl` panio's Redfish", but **botanist's PTY serial seam still needs the
   direct path**, so it is not a full replacement. (Tracked as RF-8.)
2. **VirtualMedia scope.** Is `VirtualMedia` (mount-an-image deploy) worth implementing, or is
   `Boot=Pxe,Once` over panio's existing netboot sufficient for the target boards? (RF-4.)
3. **sushy-tools dependency.** Acceptable to depend on / vendor a sushy-tools-style emulator, or
   build a minimal native provider? (RF-7.) Note panio's constraint of a small dependency
   footprint (core = `typer` only) — a Python Redfish provider is a heavier add.

## 7. Relationship to labgrid interop (for contrast)

Unlike Redfish, **labgrid is a peer at the same device-control layer**, not an orchestrator above
panio, so building *toward* labgrid is largely redundant. Interop with labgrid is mostly free
and **passive**: panio's raw-TCP serial listener is wire-compatible with the **ser2net** stream
labgrid already consumes, and labgrid's `ExternalPowerDriver` can shell out to `panio power`. So
labgrid is best treated as a **validation reference + passive wire-compat target**, not a build
priority. See [`related-work.md`](related-work.md) for the full panio-vs-labgrid comparison.
