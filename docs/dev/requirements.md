# Panio — Requirements & progress tracker

> Source of truth for *what panio must do* and *how far along each capability is*.
> Panio is a **device-control layer** (power, serial, deploy/netboot, video, HID); test
> orchestration and results stay above it (§9).
>
> Design: [`docs/dev/ci-integration/`](ci-integration/gap-analysis.md); per-feature docs under
> [`docs/`](../README.md). **Update the Status column as work lands.**
>
> Last updated: 2026-10-17.

## Status legend

| Symbol | Meaning |
|---|---|
| ☑ | Done / shipped |
| ◐ | In progress |
| ☐ | Not started |
| ⤵ | Deferred (planned, later) |
| ⊘ | Out of scope (recorded, not planned) |

**Pri** = M(ust) / S(hould) / C(ould). **Source/Notes** cites the driving need or contract
(in §9, Source is LAVA, FX = Fuchsia/botanist, BOTH = LAVA and FX, or OWNER = the project
owner's own need).

---

## 1. Foundations / platform

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| CORE-1 | Daemonless target config readable by every command; single target is the default | M | ☑ | *was per-target `targets/<name>.toml` (`_config.py`); superseded by CORE-7* |
| CORE-2 | CLI (`panio`) over subcommands; SSH-drivable from a dev machine into the control host | M | ☑ | Remote-control pattern (README) |
| CORE-3 | Run on macOS 10.14+ and Linux (x86-64/arm64) | M | ☑ | `panio setup` installs daemons/tools |
| CORE-4 | Rust daemons (`hdmicap`, `serialcap`) + Swift `visionocr` helper build & install | M | ☑ | `panio setup` |
| CORE-5 | Predictable runtime paths (configs, daemon discovery, capture logs) | M | ☑ | [architecture.md §4](architecture.md#4-configuration-and-state-model) |
| CORE-6 | Agent-oriented guidance kept current (`AGENTS.md`) as the surface changes | M | ◐ | track §9 power/serial changes |
| CORE-7 | One-file **lab** model (`--lab`/`PANIO_LAB`): hosts + targets, per-channel host binding | S | ☑ | `cli/src/model.rs`, `labfile.rs`; [distributed-control](../distributed-control.md) (no legacy targets dir) |
| CORE-8 | Transparent re-exec of host-operating commands on a target's **remote control host** over SSH | S | ☑ | `cli/src/dispatch.rs` (ships a lab slice + `--lab`); `cli/src/ssh.rs` transport |
| CORE-9 | Tunnelled `console` for a remote target (dashboard reachable locally) | S | ☑ | `remote_console` in `cli/src/main.rs`; `?serialws=` stitch |
| CORE-10 | Multi-host targets (one target spanning control hosts) | C | ◐ | per-channel dispatch (`dispatch.rs`); `console` requires co-located channels |
| CORE-11 | Remote `setup --host` + discovery-assisted `configure` | C | ☑ | `panio setup --host`, `discover`, `configure` (`cli/src/main.rs`) |
| CORE-12 | Carry a helper's required secrets to its **remote** control host, without exposing them in the host's process list | S | ☑ | `AMT_PASSWORD` (`ssh::FORWARDED_ENV`) is forwarded on every non-interactive dispatch (`power-*`, `power set` hooks) over **stdin**, not argv, so `ps` never shows it (`ssh::stdin_prelude`/`ssh::launch` in `cli/src/ssh.rs`, wired in `cli/src/dispatch.rs`). `run_interactive` (e.g. `serial connect`) forwards nothing: its stdin is the user's terminal. See [distributed-control.md](../distributed-control.md#env-forwarding-to-a-control-host) and [power.md](../power.md#credentials) |
| CORE-14 | Put one lab file on every control host from its git or HTTPS source, read-only there, with drift and age visible | S | ☑ | `panio config sync` (`cli/src/labsync.rs`), `--git` or `--url` with a bearer token from env/file/command; see [distributed-control.md](../distributed-control.md#one-lab-on-every-control-host-config-sync) (#375) |
| CORE-13 | Re-image path for a control host: one human action from blank media to agent-reachable (the disposability principle, made real) | C | ◐ | `pi-sd` shipped and hardware-validated ([`packaging/host-seed/`](https://github.com/curtisgalloway/panio/tree/main/packaging/host-seed), [control-host.md](../control-host.md)). `x86-usb` (Ubuntu autoinstall) and the argument-taking generator are unbuilt; design in [notes/control-host-provisioning.md](https://github.com/curtisgalloway/panio/blob/main/notes/control-host-provisioning.md) |

## 2. Netboot / deploy

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| NET-1 | Built-in DHCP + TFTP over a direct USB-Ethernet link | M | ☑ | `netbootd/` (Rust engine, default); `192.168.99.1/24` |
| NET-2 | `netboot start/stop/status`, `tftp-root`, `logs` (filterable, followable) | M | ☑ | `cli/src/netboot.rs` |
| NET-3 | Bare-link up/down/status for interface testing | M | ☑ | `netif mode link`/`off` + `netif status` (carrier); `netif down-hard` forces a real carrier drop (WoL off + admin-down). Replaced `netboot link-up/down/status` |
| NET-4 | TFTP root configurable per target (`--tftp-root`) | M | ☑ | required for `netboot start` |
| DEPLOY-USB-RAM | Document guarded RAM-only USB fastboot staging with fresh per-artifact entry, separate bounded USB readiness, exclusive ownership and retained evidence | S | ☑ | [Workflow and external runner contract](../fastboot.md) (#285); documentation shipped, no bundled fastboot channel or staging runner |
| NET-5 | `netif mode netboot\|link\|ffx\|off` — atomic, idempotent link-mode switch; `link` = bare host IP (no daemon) for link testing; stops netboot before SD boot, sets up host `fe80::1`/64 for ffx; `netif status` probes the active mode + carrier | S | ☑ | `cli/src/netif.rs`; from rpi5 ffx bring-up |

## 3. Serial console

A capture epoch identifies one serial supervisor lifetime; RX and TX mean
received bytes and driver-accepted transmitted bytes. See the
[glossary](../glossary.md).

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| SER-WAIT | Finite fresh serial acquisition after an epoch/RX boundary, with raw/output caps, complete-row follow and explicit incomplete outcomes | S | ☑ | `serial wait`, MCP `serial_wait`; task verdict stays with the caller (#281) |
| SER-HEALTH | Read-only configured/active identity, process/port state, epoch, reconnect and RX/TX/capture progress through CLI and MCP | S | ☑ | `serial status`, MCP `serial_status`; task-specific response remains the caller's judgment (#279) |
| SER-EVIDENCE | Immutable exact-byte receive evidence with original timestamps, epoch/generation cursors, byte caps and explicit retention/fault diagnostics | S | ☑ | `serial evidence`, MCP `serial_evidence`; separate from line/pending view (#280) |
| SER-A | `serialcap` daemon owns the port exclusively; supervisor fans out reads | M | ☑ | `serialcap/`; lockfile |
| SER-B | Timestamped rolling JSONL capture log, addressable by seq; rotation | M | ☑ | `capture.rs`; `serial log` |
| SER-C | Interactive terminal via `tio` (`serial connect`) | M | ☑ | |
| SER-D | Bidirectional live `/stream` (WebSocket) — read + write-back | M | ☑ | `server.rs` (used by dashboard) |
| SER-E | `serial add/set/rm/devices/show`, multi-interface per target | M | ☑ | |
| SER-F | DTR control: `serial dtr`, `serial reset` (soft-reset semantics) | M | ☑ | `cli/src/power.rs` |
| SER-G | Power-sense read via modem-control input (`--power-sense cts\|dsr\|dcd\|ri`) | S | ☑ | `/status` → `power_on` |

## 4. Power control

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| PWR-A | `power-cycle` via configurable script (`--power-cycle-cmd`) | M | ☑ | *superseded by `[power]` hooks (`--cycle-cmd` et al.), PWR-5* |
| PWR-B | `power-state` (read-only on/off via sense signal) | M | ☑ | `power-state` |
| PWR-C | DTR-based hardware power-button toggling (J2 header): ≤500ms soft / ≥3s hard | M | ☑ | `cli/src/power.rs` |

## 5. Video / OCR

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| VID-1 | HDMI/USB capture via warm-stream `hdmicap` daemon | M | ☑ | `hdmicap/`; Linux V4L2 + macOS |
| VID-2 | `video watch/preview/shot/read/devices/show/stop`; stable & changed-since capture | M | ☑ | |
| VID-3 | On-device OCR (`video read`): Apple Vision (macOS), Tesseract (Linux) | S | ☑ | `ocr/` helpers via hdmicap `GET /ocr` (no legacy `--json` flag) |
| VID-4 | Change detection sensitive enough for small-region edits | C | ☐ | `--changed-since` compares a 64-bit aHash over an 8x8 grid, so a sub-cell change (moved cursor, one BIOS checkbox) reads as "unchanged". Wants an opt-in finer comparison (region-of-interest or a second hash), not a bigger default. Distinct from the `signal` bugs fixed in #103 |

## 6. HID injection

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| HID-1 | USB keyboard/mouse injection via KB2040 injector (dual-board "dumb pipe": host composes, control board CDC → I2C1 → target HID) | S | ☑ | `hidrig/` crate (this repo) + [`hidrig-kb2040/firmware/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware) (paniolo-hardware repo) |
| HID-2 | Device-independent HID serial protocol (v1) so other microcontrollers can implement the injector | S | ☑ | `docs/dev/hid-serial-protocol.md` |
| HID-3 | Generic `hid` lab channel: `panio hid set/rm/send` appends args to an opaque helper cmd | S | ☑ | mirrors power hooks; SSH dispatch |
| HID-4 | Absolute mouse (`moveabs`, advertised capability) for click-where-you-point | S | ☑ | abs-pointer HID descriptor in firmware |
| HID-5 | `hidrig serve` daemon: owns the control link, re-exposes the command vocabulary over a WebSocket; one-shots route through it | S | ☑ | `panio hid serve/stop` |
| HID-6 | KVM in `panio console`: stream web keyboard + absolute mouse, intermixed with CLI injection | S | ☑ | hardware-verified (pi5 Linux) |
| HID-7 | KVM latency: HID frames fire-and-forget over USB-CDC (no per-frame round-trip), coalesce mouse moves (per-frame); floor is the target's USB `bInterval` (~8 ms) | S | ☑ | macOS `IOSSDATALAT` floored for control-frame replies |

### 6.1 Openterface deep control (OTF)

The Openterface Mini-KVM (USB KVM dongle) has open hardware (v1.9). Its switchable USB-A port
and CH340 (USB-serial chip) modem lines make it a small programmable USB fixture. Details and
bench checklist:
[`openterface-deep-control.md`](https://github.com/curtisgalloway/panio/blob/main/notes/openterface-deep-control.md).

- **OTF-1 verified 2026-08-18** as far as the bench allows: DTR/`SW_GND` and RTS/`HIDRESET`
  characterized, `DATAFLIP` not observable, EEPROM dumped.
- The MS2109 (capture chip) GPIO write is **unblocked** (2026-08-30): the mux is a register
  write over the HID config interface.
- The lab host is a VM with USB passthrough; verify USB observations on the hypervisor.

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| OTF-1 | Bench-verify the control paths on our unit: PCB rev; MS2109 GPIO write for the A-port mux; DTR→`SW_GND` replug polarity + hold time; RTS→CH9329 reset; `DATAFLIP` semantics; serial-open DTR-pulse side effects; EEPROM backup dump | M | ☑ | **done**: v1.9; DTR asserted = disconnected; serial-open *does* replug the A-port; RTS resets the CH9329 (threshold 20–50 ms, ~700 ms boot, A-port undisturbed); `DATAFLIP` not observable (1,956 samples); EEPROM dumped (`sha256 9b46336d…`); switch is software-monitored. **blocked**: MS2109 GPIO write |
| OTF-2 | RTS hardware reset of the CH9329 (`HIDRESET` line) as a recovery verb in the shipped `ch9329` backend; guard against serial-open modem-line pulses disturbing the A-port (see OTF-1) | S | ☐ | **unblocked**: pulse RTS low ≥50 ms, wait ≥800 ms for boot. Route through the existing session (opening the tty asserts DTR+RTS, replugging the A-port). No status comes back (`DATAFLIP` unreadable). A reconnecting watchdog should force the baud, not autodetect (#81) |
| OTF-3 | `usb attach-host` / `usb attach-target`: software flip of the A-port mux (MS2109 GPIO) — hands-free physical media (image stick host-side, boot it target-side, BIOS-visible) | S | ☐ | **unblocked 2026-08-30**: no 8051 patch needed. Read-modify-write XDATA `0xDF01` bit 0 (bit 4 on capture firmware < `24081309`) over the MS2109 HID config interface with XDATA opcodes `0xB5`/`0xB6` (already working in findings 7–9, which stopped one address past `0xDF00`). Protocol: [openterface-usb-mux-spec.md](https://github.com/curtisgalloway/panio/blob/main/notes/openterface-usb-mux-spec.md). Untested on the Mini-KVM |
| OTF-4 | `usb replug [--hold-ms]`: soft surprise-unplug/replug of the A-port device via CH340 DTR ground-float — scripted hot-plug exerciser for USB driver testing | S | ☐ | mechanism confirmed. Off the VM the blockers **did not reproduce** (28/28 cycles at 480 Mbps, finding 10); they look like USB-passthrough artifacts. Before shipping: repeat on bare-metal *Linux* (run was macOS) and re-check whether opening the tty alone unplugs the A-port (not on macOS `/dev/cu.*`) |
| OTF-5 | EEPROM serial-stamping utility (AT24C16 via MS2109) → unique USB serials → stable by-id paths on multi-unit benches | C | ☐ | **premise corrected**: the `????????` serial is a RAM-resident string descriptor at XDATA `0xC676`, not an EEPROM field, so it needs a firmware patch (OTF-3 does not). CH340 stays serial-less; use by-path |
| OTF-6 | Extension-pins target-side gadget slot (spare downstream port of each hub on pads): MCU mass-storage gadget (true virtual media) or analyzer tap | C | ⤵ | solder mod; revisit after OTF-3/4 prove out |
| OTF-7 | KVM-Go microSD mux (`USB_SW` via the CH32V208): `usb attach-host` / `attach-target` / `state` over the existing CDC control port | S | ☑ | **done 2026-08-30, hardware-verified.** The generic `usb` channel + the `ch9329` helper's `usb` verb. Serial opcode `0x17`; query and both directions verified by a nonce round-trip across the mux. The reply reports the *resulting* position; compare it to the request. An unimplemented opcode is **silent**, so gate "unsupported" on VID/PID (`1A86:FE0C`) plus a query timeout, never the chip-version table. **RTS on this device is an MCU hardware reset — deassert RTS+DTR on open.** Protocol: [openterface-usb-mux-spec.md](https://github.com/curtisgalloway/panio/blob/main/notes/openterface-usb-mux-spec.md) |

## 7. Dashboard

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| DASH-1 | Combined video + serial web UI (`panio console`); auto-starts daemons | S | ☑ | preselect serial via `-i` |
| DASH-2 | Dashboard power-cycle control | S | ☑ | |

## 8. Cross-cutting / non-functional

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| NF-1 | Interactive/agent bring-up workflow (dashboard, OCR, HID, `tio`, JSONL) never regresses | M | ◐ | guard every change, esp. §9 serial |
| NF-2 | Changes land as smallest reversible steps, each with tests | M | ◐ | |
| NF-3 | Core power/serial path stays functional on both macOS and Linux | M | ☑ | CI-only features may be Linux-only (see §9) |
| NF-4 | External contracts re-verified against upstream before relying on them | M | ◐ | re-check Fuchsia `device.go` (FX-4) |
| NF-5 | Failures are machine-classifiable: exit code by kind, optional one-line JSON object on stderr | M | ☑ | [error contract](error-contract/design.md), [errors.md](../errors.md); shipped in 0.5.0 (#242) |

---

## 9. Hardware-CI integration (KernelCI/LAVA + Fuchsia/botanist)

> Goal: make panio's primitives **consumable by** LAVA (KernelCI's board-test lab) and
> `botanist`+`testrunner` (Fuchsia's device-test tools), *without* owning orchestration or
> results. Analysis:
> [`ci-integration/gap-analysis.md`](ci-integration/gap-analysis.md); design:
> [`ci-integration/design.md`](ci-integration/design.md).
>
> **Focus:** the owner's **Fuchsia port**. No other users, so breaking changes are free. M1 leads
> with the Fuchsia path (PTY + power); botanist before LAVA.

### 9.0 Decisions (locked 2026-05-29)

| ID | Decision | Resolution |
|---|---|---|
| D-1 | KCIDB results path | ⊘ Out of scope — LAVA-lab path only for KernelCI |
| D-2 | Fuchsia serial ownership | PTY proxy; panio keeps the physical port (JSONL/dashboard stay live) |
| D-3 | Serial write arbitration | Cooperative last-writer-wins + advisory lock in `/status` + opt-in `--exclusive`, **auto-released on client disconnect** (+ optional `--lock-timeout`) |
| D-4 | JTAG in v1 | Extension point only (schema + verb stubs); OpenOCD backend deferred |
| D-5 | CI control-host OS | Linux-only for CI; macOS stays first-class for interactive bring-up |
| D-6 | Deploy ownership in CI | Orchestrator owns deploy (LAVA TFTP / botanist pave); panio netboot stands down |
| D-7 | Serial TCP endpoint | Native Rust TCP listener in serialcap; ser2net-on-PTY as LAVA fallback |
| D-8 | `[power]` config | Breaking change accepted — clean `[power]` block, no `power_cycle_cmd` alias; update `AGENTS.md` |

### 9.1 Agnostic device-control API — Power

| ID | Requirement | Source | Pri | Status | Notes |
|---|---|---|---|---|---|
| PWR-1 | `panio power on` — applies power; DUT begins booting unattended | LAVA | M | ☑ | `on_cmd` hook (2026-06-04) |
| PWR-2 | `panio power off` — cuts power | LAVA | M | ☑ | `off_cmd` hook (2026-06-04) |
| PWR-3 | `panio power reset` — off+delay+on (hard reset) | LAVA | M | ☐ | verb is `power-cycle`; `cambrionix` `cycle` does off+delay+on |
| PWR-4 | `panio power state` — read on/off | BOTH | M | ☑ | `power-state`, `state_cmd`-backed when configured; rename only |
| PWR-5 | `[power]` config block w/ `backend = script\|dtr\|pdu\|jtag` + on/off/reset cmds | BOTH | M | ☑ | landed 2026-06-04 as generic hooks (`cycle/on/off/state_cmd`); no backend enum, logic in helpers |
| PWR-6 | Power commands usable as plain shell cmds (string or list) from a generator | LAVA | M | ☑ | hooks are plain `sh -c` strings |
| PWR-7 | Update `AGENTS.md` for the new `[power]` config + verbs | OWNER | M | ☑ | done with the hooks change |

### 9.2 Agnostic device-control API — Serial (core gap)

| ID | Requirement | Source | Pri | Status | Notes |
|---|---|---|---|---|---|
| SER-1 | serialcap exposes a **raw bidirectional TCP listener** (ser2net-equivalent) | LAVA | M | ☐ | backs `connection_command = telnet host port` |
| SER-2 | serialcap exposes a **PTY** whose slave path is a real device file | FX | M | ☐ | handed to botanist as `DeviceConfig.serial` |
| SER-3 | New endpoints **tee off the existing supervisor** (JSONL/WS/dashboard unaffected) | OWNER | M | ☐ | preserves NF-1 |
| SER-4 | `panio serial send <bytes\|->` one-shot write (agent feature) | OWNER | M | ☑ | same `write_tx` channel; shipped as `serial send [target] <text>` with `--pace-ms` and `--no-newline` (no `--hex` or stdin) |
| SER-5 | Write arbitration per D-3 (lock, `/status` holder, `--exclusive`, auto-release) | OWNER | M | ☐ | |
| SER-6 | Stable socket/PTY paths under `$XDG_RUNTIME_DIR/panio/<target>/` | BOTH | S | ☐ | predictable for adapters |
| SER-7 | Existing JSONL log, `/stream`, `tio`, `serial log/dtr/reset` unchanged | OWNER | M | ☐ | regression guard / tests |

### 9.3 Agnostic device-control API — Deploy / boot / debug

| ID | Requirement | Source | Pri | Status | Notes |
|---|---|---|---|---|---|
| DEP-1 | netboot **stands down** under CI; no DHCP/TFTP contention | BOTH | M | ☐ | guard `netboot start` under CI attach |
| DEP-2 | netboot remains available for interactive/non-CI use | OWNER | M | ☑ | exists (NET-1..4); not the CI path |
| DEP-3 | (Full) panio-serves-images as a non-standard LAVA deploy method | LAVA | C | ⤵ | only if a board can't use LAVA TFTP |
| BOOT-1 | `panio serial wait --match <regex> [--timeout]` boot-detect helper | OWNER | S | ⤵ | ergonomics; neither orchestrator needs it |
| JTAG-1 | `[jtag]`/`[debug]` config schema + `panio debug {halt\|resume\|reset\|gdb}` stubs | OWNER | C | ☐ | extension point only per D-4 |
| JTAG-2 | OpenOCD backend: reset, flash-deploy, GDB `:3333` / Tcl `:6666` sockets | OWNER | C | ⤵ | deferred |

### 9.4 Adapter A — LAVA lab

| ID | Requirement | Source | Pri | Status | Notes |
|---|---|---|---|---|---|
| LAVA-1 | Device-dictionary + device-type template generator (`panio lava device-dict`) | LAVA | M | ☐ | power_* → `panio power …`; connection → telnet |
| LAVA-2 | Generator supports list-valued power commands | LAVA | S | ☐ | |
| LAVA-3 | "First device" onboarding doc (Debian worker, ser2net/TCP wiring, tokens) | LAVA | S | ☐ | internet-reachable lab; tokens to KernelCI admins |
| LAVA-4 | Verified on a Debian LAVA worker against a real board | LAVA | S | ☐ | macOS unsupported (D-5) |

### 9.5 Adapter B — Fuchsia / botanist

| ID | Requirement | Source | Pri | Status | Notes |
|---|---|---|---|---|---|
| FX-1 | botanist device-config emitter (`panio botanist device-config`) → PTY path | FX | M | ☐ | `{network,keys,serial}`; serial = PTY (SER-2) |
| FX-2 | Bot-host/recipe **power wrapper** calling `panio power {on\|reset\|off}` | FX | M | ☐ | power is NOT a device-config field |
| FX-3 | `bot_config.py` `get_dimensions()` snippet advertising `device_type:<board>` | FX | S | ☐ | + `bots.cfg`, `platforms.gni` (upstream) |
| FX-4 | Verify `DeviceConfig`/power plumbing against a real Fuchsia checkout | FX | M | ☐ | confirm `tools/botanist/target/device.go` |
| FX-5 | Document RFC-0130 Experimental tier (self-hosted CI) | FX | C | ☐ | community board is not "Supported" tier |

---

### 9.6 Adapter C — Redfish provider

> **Decision (D-9, 2026-05-29):** Redfish (DMTF server-management REST API) as **provider**:
> panio exposes Redfish in front of BMC-less boards. **Not client.** Redfish is the common
> bare-metal control API (Ironic/Metal3; LAVA can `curl` it), so one provider beats
> per-ecosystem adapters. Sequenced **after** M1; consumes PWR-1..6 and SER-1. Design:
> [`ci-integration/redfish-provider.md`](ci-integration/redfish-provider.md). Verified against
> DMTF CSDL (DSP0266 v1.22.0, DSP8010 2025.2), OpenBMC, Ironic/sushy.

| ID | Requirement | Source | Pri | Status | Notes |
|---|---|---|---|---|---|
| RF-1 | Redfish provider service: `ServiceRoot` → `ComputerSystem` → `Manager` (→ `VirtualMedia`) resource tree | OWNER | S | ⤵ | after M1; provider, not client |
| RF-2 | `#ComputerSystem.Reset` → power verbs (On→on, ForceOff→off, PowerCycle/ForceRestart→reset); `PowerState` → power-state | OWNER | S | ⤵ | depends on PWR-1..6 |
| RF-3 | `Boot.BootSourceOverrideTarget=Pxe` + `BootSourceOverrideEnabled=Once` → netboot | OWNER | S | ⤵ | maps to existing netboot |
| RF-4 | `VirtualMedia` `InsertMedia`/`EjectMedia` → image deploy | OWNER | C | ⤵ | open: needed vs. Pxe-once sufficient? |
| RF-5 | `SerialConsole` advertises out-of-band SSH/console endpoint pointing at panio raw-serial socket (metadata only) | OWNER | S | ⤵ | depends on SER-1; Redfish carries no serial bytes |
| RF-6 | Accurate per-node `ResetType@Redfish.AllowableValues` / `ActionInfo` for the supported subset | OWNER | S | ⤵ | relay/DTR boards lack some `ResetType`s |
| RF-7 | Implement via a sushy-tools-style emulator + panio backend driver (not a hand-rolled OData service) | OWNER | S | ⤵ | open: dependency footprint (the core CLI is a single Rust binary) |
| RF-8 | Document/decide whether Redfish provider replaces or complements LAVA/botanist adapters | OWNER | S | ⤵ | botanist PTY serial still needs the direct path → not a full replacement |

## 10. Security

> **TODO — owner to populate.** Panio grants physical-equivalent control of a target (power,
> raw serial, netboot/TFTP, HID), is **SSH-driven into the control host**, and §9 adds
> **network-facing serial endpoints**. The threat model needs first-class requirements.

| ID | Requirement | Pri | Status | Notes |
|---|---|---|---|---|
| SEC-0 | Define panio's threat model and security requirements | M | ☐ | **Placeholder — to be written.** |

Open questions (not yet requirements):

- **Serial endpoint exposure (§9):** SER-1 mirrors serialcap's loopback bind (`127.0.0.1`). Who
  may connect from a LAVA worker / Swarming bot: auth, bind address, TLS, or SSH tunnel +
  isolation?
- **Write arbitration (SER-5/D-3):** is `--exclusive` only cooperative, or a safety guard?
- **Netboot/DHCP/TFTP:** read-only, single-client; rogue-DHCP risk on a shared network vs. the
  assumed direct link?
- **Power/HID authority:** control-host access means power-cycle and HID; what bounds that
  (host access, per-target ACLs)?
- **Secrets:** LAVA tokens, `$FUCHSIA_SSH_KEY`, CIPD/Swarming creds: storage and handling.
- **Supply chain:** `panio setup` builds/install Rust + Swift + Homebrew components.

---

## 11. Milestones

| Milestone | Contents | Status |
|---|---|---|
| M0 — Analysis & design | gap-analysis, design, this tracker, decisions | ☑ |
| Shipped baseline | §1–§7 capabilities (netboot, serial, power, video, HID, dashboard) | ☑ |
| M1 — Agnostic device-control core | SER-2, SER-4, PWR-1..7, SER-5, SER-1, DEP-1, JTAG-1 (Fuchsia path first) | ☐ (awaiting go-ahead) |
| M2 — Adapters | FX-1..4 (first), then LAVA-1..3 | ☐ |
| M3 — Verify on hardware | FX-3/FX-5, LAVA-4, BOOT-1 | ☐ |
| Security | §10 (SEC-*) | ☐ (to be defined) |
| M4 — Full (deferred) | DEP-3, JTAG-2 | ⤵ |

## 12. Open implementation questions

| ID | Question | Status |
|---|---|---|
| SER-Q1 | Native TCP listener vs. ser2net-on-PTY | ✓ Resolved (D-7): native listener; ser2net fallback |
| SER-Q2 | Write-lock lifetime | ✓ Resolved (D-3): auto-release on disconnect + optional `--lock-timeout` |
| PWR-Q1 | `[power]` shape + `power_cycle_cmd` migration | ✓ Resolved (D-8): clean breaking block, no alias |
