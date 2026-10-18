# Panio hardware-CI integration — Design

> Companion to `gap-analysis.md`. Defines one ecosystem-agnostic **panio device-control
> API**, then two thin **adapters** (LAVA, Fuchsia/botanist) that consume it. Includes a
> **minimum-viable** path and a **full** path, the exact CLI/socket surface each adapter
> touches, and a blunt **verdict** per ecosystem.
>
> Guiding constraints (owner-set): (1) the interactive, agent-driven bringup workflow
> (dashboard, OCR, HID, `tio`, JSONL) must not regress — CI is **additive**; (2) the owner
> *wants* agent write-to-serial and a future JTAG backend anyway, so the agnostic API is
> designed for those, not just CI; (3) panio is the owner's project and may be refactored
> freely, so we choose the *right* shape over the timid one where they differ.

External contracts cited inline are verified May 2026; see `gap-analysis.md` §0 for the two
points where the verified source overrides the original brief.

---

## 1. Design principle: verbs backed by per-target backends

Both ecosystems, the agent workflow, and future JTAG need the **same primitives in different
shapes**. So model panio's device control as **stable verbs** whose implementation is a
**selectable per-target backend**, and **expose every live primitive as both a discrete CLI
verb and a machine-drivable socket** off the daemon that already owns that hardware.

```
                         ┌───────────────────────── panio device-control API ─────────────────────────┐
   agent / CLI ───────►  │  power {on|off|reset|state}   serial {send|attach|log|...}   deploy   debug   │
   LAVA adapter ──────►  │        │                            │                           │       │      │
   botanist adapter ──►  │   power backend                serialcap (owns UART)        netboot   jtag    │
                         │   {script|dtr|pdu|jtag}        ├─ JSONL log (tee)            (stands   backend │
                         │                                ├─ WebSocket /stream (exists) down in   {openocd}│
                         │                                ├─ raw TCP listener  (NEW)    CI)               │
                         │                                ├─ PTY device path   (NEW)                      │
                         │                                └─ write arbiter (tio|sock|send)                │
                         └──────────────────────────────────────────────────────────────────────────────┘
```

The serial daemon is **already** a fan-out supervisor (read → broadcast + JSONL tee; write via
`write_tx`). We add two *raw* egress shapes and a write arbiter — siblings to the existing
WebSocket/JSONL, not replacements. That keeps the interactive workflow intact by construction.

---

## 2. The agnostic device-control API (target contract)

### 2.1 Power

New discrete verbs (over existing mechanisms; backend selected per target):

| Verb | Semantics | Backend mapping |
|---|---|---|
| `panio power on` | apply power; **DUT must begin booting unattended** | PDU script "on" / board-boots-on-power / JTAG `reset run` |
| `panio power off` | cut power | PDU script "off" / DTR long-press (≥3 s) |
| `panio power reset` | off + delay + on (= hard reset) | existing `power-cycle` script / PDU / JTAG `SRST` |
| `panio power state` | report on/off (read-only, exists) | sense signal via daemon `/status` |

Config (per target) gains an explicit power backend, e.g.:

```toml
[power]
backend = "script"            # "script" | "dtr" | "pdu" | "jtag"
on_cmd   = "pdu --port 3 on"  # used by "script"/"pdu"
off_cmd  = "pdu --port 3 off"
reset_cmd = "pdu --port 3 cycle"   # falls back to off+sleep+on if unset
# dtr backend reuses existing power_serial_interface + pulse durations
```

Backward-compat: keep `power_cycle_cmd` working (maps to `reset`/`script`). Exit codes stay
`0`/`1`.

*As shipped (see §8 and `power.md`): the `[power]` block landed as plain generic hooks —
`cycle_cmd`/`on_cmd`/`off_cmd`/`state_cmd`, no `backend` enum, no `power_cycle_cmd` alias —
with `power on`/`power off` subcommands and the existing top-level `power-cycle`/`power-state`
verbs (no separate `power reset`/`power state`).*

### 2.2 Serial (the core work)

`serialcap` stays the single exclusive owner of the UART and gains, off the same supervisor:

1. **Raw TCP listener** — `panio serial attach --tcp [--port N]` (or always-on when
   configured). A client connecting gets a **raw, bidirectional, persistent** byte stream:
   all UART output, and bytes written back are injected via the existing `write_tx` path.
   This is the ser2net equivalent → LAVA's `connection_command = telnet <host> <port>`.
2. **PTY proxy** — `panio serial attach --pty` prints/records a slave device path (e.g.
   `/dev/pts/N`, optionally a stable symlink under `$XDG_RUNTIME_DIR/panio/…`). Bytes flow
   raw both ways. botanist is handed this path as `DeviceConfig.serial` and opens it like a
   real UART, while panio keeps owning the physical port (so JSONL/dashboard keep working).
3. **`panio serial send <bytes|->`** — one-shot write to the port (owner's wanted feature;
   agent-friendly; same `write_tx` channel). `--enter`, `--hex`, stdin supported.
4. **Write arbitration** — because `tio`, TCP clients, PTY clients, and `send` can all write,
   define a policy. Recommended v1: **cooperative, last-writer-wins with an advisory lock**
   surfaced in `/status` (who currently holds write), plus an opt-in `--exclusive` for CI
   attach so the orchestrator isn't fighting a stray `tio`. (Decision flagged below.)

Existing surfaces unchanged: JSONL log + rotation, `/stream` WebSocket, `serial log`,
`serial dtr/reset`, `tio` via `serial connect`, dashboard.

### 2.3 Deploy

No new deploy *engine*. In CI, **netboot stands down**; the orchestrator owns deploy (LAVA
TFTP; botanist pave). Keep netboot for interactive bringup and non-CI boards. Add a guard so
`netboot start` warns/refuses when a CI attach is active on the same interface (avoid DHCP/TFTP
contention). A future "panio serves images to LAVA" path is explicitly **deferred**.

### 2.4 Boot detect (optional ergonomics)

`panio serial wait --match <regex> [--timeout ms] [--since <seq>]` → exits `0` on match,
non-zero on timeout, prints the matching line. Reads the existing JSONL/stream; **not required**
by either orchestrator (LAVA matches `prompts:`, botanist polls `summary.json`) but valuable for
the agent workflow and the minimum-viable CI smoke path.

### 2.5 Debug / JTAG (extension point; implement later)

Reserve verbs `panio debug {halt|resume|reset|gdb}` and a `[jtag]` config block selecting an
`openocd` backend that exposes the GDB (`:3333`) / Tcl (`:6666`) sockets. v1 ships the **config
schema + verb stubs as the extension point**; the OpenOCD backend is a follow-on.

---

## 3. Adapter A — LAVA lab

panio becomes the thing a **LAVA dispatcher** (Debian host) shells out to for one board.
LAVA owns deploy/boot/test; panio provides **power + raw serial**.

**Serial:** run `ser2net` (or panio's own raw TCP listener) pointed at panio's serial
endpoint, exposing `telnet <host> <port>`. Two wiring options:
- *Simplest:* `panio serial attach --tcp --port 7001 --exclusive` and set
  `connection_command = telnet localhost 7001`.
- *ser2net-classic:* point ser2net at the **PTY** from `serial attach --pty`. Either yields
  the raw bidirectional persistent stream LAVA requires.

**Power:** a generated **device dictionary** whose commands shell out to panio:

```jinja2
{% extends "<base-device-type>.jinja2" %}
{% set power_on_command  = 'panio power on  --target {{ panio_target }}' %}
{% set power_off_command = 'panio power off --target {{ panio_target }}' %}
{% set hard_reset_command = 'panio power reset --target {{ panio_target }}' %}
{% set connection_command = 'telnet localhost {{ serial_tcp_port }}' %}
```
(Generator supports list-valued power commands too — gap-analysis §0 DISCREPANCY 3. `power_on`
must start the boot — ensure the board/PDU wiring honors that.)

**Deploy:** none in panio — LAVA's dispatcher downloads + serves TFTP. panio netboot is
**off** for LAVA targets.

**Host OS:** Debian Linux (LAVA workers are Debian-only). Keep macOS for interactive bringup.

**Deliverable:** a small generator `panio lava device-dict --target <t>` that emits the
device dictionary + a device-type template snippet, plus a "first-device" doc.

---

## 4. Adapter B — Fuchsia / botanist

panio registers the control host as a **Swarming bot** advertising the board's dimensions
(`get_dimensions()` → `device_type:<board>`, entry in `bots.cfg`, matching `test_platforms`
entry in `//build/testing/platforms.gni`; the board is RFC-0130 **Experimental** tier =
self-hosted CI). The Swarming task runs `botanist run` → `testrunner`.

**Serial:** emit a botanist **device config** whose `serial` points at panio's **PTY**:

```json
{ "network": { "nodename": "<board>", "ipv4": "" },
  "keys": [],
  "serial": "/run/panio/<target>/serial.pty" }
```
botanist opens the PTY, runs its own `serial.Server`, and creates `$FUCHSIA_SERIAL_SOCKET` for
`testrunner` (which drives `run-test-suite` over serial when `$FUCHSIA_SSH_KEY` is unset). The
socket is **botanist's**, downstream of panio — panio only supplies the device-file path
(gap-analysis §0 DISCREPANCY 1). panio keeps owning the physical UART, so JSONL/dashboard
stay live during the run.

**Power:** **not** a device-config field (DISCREPANCY 2). Provide power at the **bot-host /
recipe layer** — a wrapper invoked before/around `botanist run` that calls
`panio power {on|reset|off}` (and/or implement a `botanist/power` method that shells to
panio). Confirm against `tools/botanist/target/device.go` in a checkout before building.

**Deploy:** botanist owns pave/zedboot + CIPD/CAS. panio netboot **off** for Fuchsia targets.

**Deliverable:** `panio botanist device-config --target <t>` emitter, a `bot_config.py`
`get_dimensions()` snippet, and a thin power-wrapper for the recipe/bot-host layer.

---

## 5. Minimum-viable vs full

**Minimum viable (recommended first milestone) — "power + raw serial, orchestrator owns deploy":**
- `power on/off/reset` verbs (§2.1) + raw **TCP** serial listener (§2.2.1) + **PTY** (§2.2.2)
  + `serial send` (§2.2.3) + write arbiter (§2.2.4).
- LAVA device-dict generator; botanist device-config emitter + power wrapper.
- netboot stands down under CI; Linux CI host.
- This is enough to bring a board up under **both** orchestrators. It is also exactly the
  owner's wanted "agents read+write serial" feature.

**Full path (later):**
- `serial wait --match` boot-detect helper.
- JTAG backend (reset/deploy/debug + GDB socket).
- Optional: panio-serves-images LAVA deploy method (non-standard; only if a board can't use
  LAVA's TFTP).
- Optional: KCIDB emission — **only if** the owner decides panio should grow a result story
  (this is above the device-control line; see verdict).

---

## 6. Verdict — is panio suitable?

**KernelCI via LAVA lab: SUITABLE WITH CHANGES — good fit.** panio's model (one host wired to
one board, owns power+serial) is exactly LAVA's lab model. Required changes are modest and
ranked: (1) discrete power verbs [small], (2) raw TCP serial passthrough [medium], (3) device-
dictionary generator [small], (4) run on Debian, netboot off [trivial]. The only "can't" is
**macOS as a LAVA worker** — not supported upstream; use Linux. No blockers.

**KernelCI via KCIDB: OUT OF SCOPE (recommended).** This path needs a real test-execution +
result-collection story and KCIDB schema emission — that's orchestration/results, **above** the
device-control line panio occupies. Building it would turn panio into a mini-LAVA. Recommend
**no**, unless the owner explicitly wants panio to grow upward. (Decision flagged.)

**Fuchsia / Swarming+botanist: SUITABLE WITH CHANGES — fit is good but seam is subtler.**
panio can be the bot's device-control layer. The serial seam is a **PTY** (not a socket
panio serves — DISCREPANCY 1), and **power is a recipe/bot-host hook** (not a device-config
field — DISCREPANCY 2). Both are tractable: PTY proxy [medium], power wrapper [small], device-
config emitter [small], bot registration [small + upstream coordination for `bots.cfg`/
`platforms.gni`]. Biggest non-code dependency: a community board is RFC-0130 **Experimental**
tier, so this is **self-hosted CI**, which is fine for the stated goal. One thing to verify in a
real Fuchsia checkout before coding: the exact `DeviceConfig`/power plumbing (pkg.go.dev hides
unexported fields).

**Net:** panio is a **genuinely good device-control layer for both**, with one shared core
change (raw serial passthrough in two shapes — TCP + PTY) that *also* delivers the owner's
agent-write feature, plus small per-ecosystem adapters. It is **not** and should not become a
test orchestrator or result producer.

---

## 7. Decisions (owner-confirmed 2026-05-29)

1. **KCIDB scope** — **OUT OF SCOPE.** panio stays a pure device-control layer; KernelCI is
   targeted via the LAVA-lab path only. LAVA/Maestro produce results.
2. **Fuchsia serial port ownership** — **PTY proxy** (panio keeps the physical port and its
   JSONL/dashboard during CI; hands botanist a PTY device-file path).
3. **Serial write arbitration** — **cooperative, last-writer-wins + advisory lock** surfaced in
   `/status`, plus opt-in `--exclusive` for CI attach.
4. **JTAG in v1** — **extension point only**: ship the `[jtag]`/`[debug]` config schema + verb
   stubs now; OpenOCD backend is a follow-on.
5. **CI host OS** — **Linux-only** for the CI control host; macOS remains a first-class
   interactive-bringup host.
6. **First milestone** — *partially landed* (via the Rust control-plane port): `panio serial
   send`, the `[power]` hook block (`cycle_cmd`/`on_cmd`/`off_cmd`/`state_cmd`), and `power
   on`/`off` are shipped. The PTY proxy, write arbiter/`--exclusive`, raw TCP listener, CI
   stand-down guard, and `[jtag]` stubs remain (see the slicing in §8).

## 8. Implementation decisions (resolved 2026-05-29)

- **Native TCP listener.** serialcap (Rust) grows a built-in raw bidirectional TCP listener
  (`serial attach --tcp`); ser2net-on-PTY remains documented as the LAVA-blessed fallback. Keeps
  panio self-contained and gives agents a trivial raw endpoint.
- **PTY/TCP listeners live in the daemon.** serialcap owns the port, so both endpoints are added
  to the Rust daemon off the existing read-broadcast / `write_tx` channels — a Python wrapper
  can't see the bytes.
- **Serial write lock auto-releases on disconnect.** The `--exclusive` hold is tied to the client
  connection (drops when the socket closes), with an optional `--lock-timeout` safety net; the
  current holder is shown in `/status`.
- **`[power]` block is a breaking change (accepted).** Clean `[power]` block, no `power_cycle_cmd`
  alias; `AGENTS.md` guidance is updated so the agent reconfigures targets on redeploy.

### Milestone slicing of M1 (smallest reversible steps, each with tests)

**Current focus: the owner is doing a Fuchsia port with an agent**, so M1 leads with the
Fuchsia-critical path (PTY + power); the LAVA TCP listener follows in the same milestone but
need not block first hardware bring-up of the Fuchsia target.

1. serialcap: **PTY** proxy (SER-2) + `serial attach --pty` — *Fuchsia `DeviceConfig.serial`.*
2. ~~`panio serial send` (SER-4)~~ — **shipped** (agent write-to-serial via `POST /input`).
3. ~~`[power]` config block + `power on/off` verbs (PWR-1..6) + `AGENTS.md` update (PWR-7)~~ —
   **shipped** (the cycle verb stayed `power-cycle`; no separate `power reset`).
4. Write arbiter: advisory lock, `/status` holder, `--exclusive` + auto-release (SER-5).
5. serialcap: raw **TCP** listener (SER-1) + `serial attach --tcp` — *LAVA; can trail.*
6. netboot CI stand-down guard (DEP-1); `[jtag]`/`debug` verb stubs (JTAG-1).

Then Adapter B first (FX-1 device-config emitter, FX-2 power wrapper, FX-4 verify against a real
Fuchsia checkout), since that's the active use case; Adapter A (LAVA) after.
Regression guard throughout: JSONL, `/stream`, `tio`, dashboard, `serial log/dtr/reset` (SER-7, NF-1).
