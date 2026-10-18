# Panio as a hardware-CI device-control layer — Gap analysis

> Scope: can `panio` serve as the **device-control layer** beneath **KernelCI/LAVA**
> and **Fuchsia Swarming/LUCI**? This document states, per device primitive and per
> ecosystem, what the orchestrator requires, what panio has today, and the delta.
>
> Framing (do not lose this): panio is a **wrangling layer** — power, serial, deploy,
> boot. Test *orchestration* and *result production* live **above** it: LAVA (under
> KernelCI's Maestro) for KernelCI; `botanist`+`testrunner` (under LUCI recipes) for
> Fuchsia. The goal is to make panio's primitives **consumable by** those tools, not
> to make panio emit verdicts.
>
> Evidence: panio claims are cited to source (`file:line`) from a read of the repo at
> branch `claude/panio-ci-integration-0GWQE`. External contracts are cited to upstream
> docs/source, verified May 2026. Two points where the original brief disagrees with the
> verified source are flagged as **DISCREPANCY** — per the ground rules, the source wins.

---

## 0. Discrepancies with the starting brief (source wins)

These materially change the integration shape, so they lead.

**DISCREPANCY 1 — Fuchsia serial is a *device-file path*, not a socket panio serves.**
The brief says to "point `$FUCHSIA_SERIAL_SOCKET` … at panio," implying panio serves
the socket botanist/testrunner consume. The verified contract is the opposite:

- `botanist`'s `DeviceConfig` is only `{ network{nodename, ipv4}, keys[], serial }`, and
  `serial` is **the path to the raw serial *device file*** (e.g. `/dev/ttyUSB0`) — not a
  socket. (`tools/botanist/target`, pkg.go.dev.)
- **botanist itself opens that device**, runs a forwarding `serial.Server`
  (`NewServer(io.ReadWriteCloser)` → `Run(ctx, net.Listener)`), and *creates* the
  Unix-domain socket. It passes that socket path into `Target.Start(…, serialSocketPath)`
  and exports it to children as `$FUCHSIA_SERIAL_SOCKET`. (`tools/serial`, `tools/lib/serial`.)
- `testrunner` is a **client** of that socket — it dials `$FUCHSIA_SERIAL_SOCKET` and types
  `run-test-suite …` over the console when `$FUCHSIA_SSH_KEY` is unset. (`tools/testing/testrunner`.)

So `$FUCHSIA_SERIAL_SOCKET` is **internal to the Fuchsia stack, downstream of botanist**.
Panio cannot "be" that socket. To integrate, panio must hand botanist a **device-file
path it can `Open()`** — i.e. a **PTY** that proxies the real UART — *or* cede the physical
port to botanist entirely. This is the single most important correction in this analysis.

**DISCREPANCY 2 — botanist's device config carries no power hooks.**
The brief implies the botanist device config accepts power on/off hooks. The rendered
`DeviceConfig` schema has **no power/PDU field**. Power/reboot is handled by a separate
`botanist/power` mechanism (e.g. Intel AMT) and/or the bot-host/recipe layer, not the device
JSON. (`tools/botanist/target`; `botanist/power/amt`.) So panio's Fuchsia power seam is a
**recipe/bot-host-level hook or a custom botanist power method**, not a config field.
*(Caveat: pkg.go.dev only renders exported symbols and googlesource was 403-walled; confirm
against `tools/botanist/target/device.go` in a real checkout before building.)*

**DISCREPANCY 3 (minor) — LAVA power fields accept a list, not only a string.**
`power_on_command` / `power_off_command` / `hard_reset_command` / `soft_reboot_command`
are normally scalar strings but **also accept a list of strings** run sequentially. The
device-dictionary generator must support both. (`Linaro/lava base.jinja2`.)

**Note — KCIDB-ng changed backend/endpoint, not the submission schema.** The results JSON
schema is unchanged; only the transport (Rust REST front end, Postgres backing) and endpoint
moved. (`docs.kernelci.org/kcidb/submitter_guide`.)

---

## 1. The four primitives × two ecosystems

| Primitive | LAVA wants | Fuchsia/botanist wants | panio today | Gap |
|---|---|---|---|---|
| **Power** | `power_on`/`power_off`/`hard_reset` shell cmds (string *or* list); **boot must start on power-up alone** | power **not** in device config — `botanist/power` method or bot-host/recipe hook | `power-cycle`, **discrete `power on`/`off`** (generic `[power]` hooks: `cycle_cmd`/`on_cmd`/`off_cmd`/`state_cmd` — *landed*), DTR button pulses (≤500ms soft / ≥3s hard), `power-state` | **Mostly closed** — remaining: a dedicated reset verb if `hard_reset` ≠ `power-cycle` semantics |
| **Serial** | **raw, bidirectional, persistent TCP stream** via `connection_command` = `telnet host port` (ser2net) | **raw device-file path** (`DeviceConfig.serial`); botanist opens it & makes the socket | serialcap owns port; exposes **HTTP + bidirectional WebSocket** `/stream`, JSONL log, `tio` — **no raw TCP socket, no PTY** | **Two different shapes:** raw **TCP listener** (LAVA) + **PTY device path** (Fuchsia). Both absent. |
| **Deploy** | **LAVA owns** download+TFTP serving (standard `tftp` method) | **botanist owns** pave/zedboot (+ CIPD/CAS) | built-in DHCP+TFTP netboot (start/stop together, no selective disable) | **Orchestrator owns deploy in both.** panio netboot must *stand down* in CI to avoid DHCP/TFTP contention |
| **Boot/detect** | `boot` action matches `prompts:` on the serial stream | botanist boots/paves, polls for `summary.json` | netboot + serial JSONL/WebSocket; **no prompt-match / wait-for built in** | Detection is the orchestrator's job once it has the serial stream → low-priority for panio |

Plus a **future cross-cutting primitive** the owner wants: **JTAG** (see §6).

---

## 2. Power — detail

**Required.**
- *LAVA:* discrete `power_on_command` (must cause the DUT to **begin booting unattended** —
  hard requirement, `device-integration.rst`), `power_off_command`, `hard_reset_command`
  (= off + delay + on), optional `soft_reboot_command`. Plain shell commands; string or list.
- *Fuchsia:* not in device config; power lives in `botanist/power` or the bot-host/recipe layer.

**panio today.**
- `power-cycle`, `power on`, `power off` run arbitrary hook commands from the target's
  `[power]` block (`cycle_cmd`/`on_cmd`/`off_cmd` — `cli/src/main.rs`, `model.rs`); helpers
  (`cambrionix`, `zigplug`, `shellyplug`, `amt`) ship for USB-hub, smart-plug, and Intel AMT switching. ≈ `hard_reset` + the
  discrete verbs.
- DTR "power button" pulses on a J2 header: `serial dtr`/`serial reset`, ≤500 ms = soft reset,
  ≥3 s = hard power-off — `cli/src/power.rs`.
- `power-state` reads `state_cmd` (first token `on`/`off`) or falls back to the sense signal via
  daemon `/status` → `power_on`.

**Delta (closed 2026-06).** The discrete verbs and the `[power]` hook config shipped with the
Rust control plane (PWR-1..6 in `requirements.md` §9). What remains is semantic, per-target:
LAVA's `power_on` must *start the boot*, which depends on the target's power path (PDU/smart
plug with on/off, or boot-on-power + DTR long-press for off), not on missing CLI surface.

---

## 3. Serial — detail (the core gap, now two-headed)

**Required.**
- *LAVA:* a shell command (`connection_command`, classically `telnet host port` backed by
  ser2net) that yields a **raw, bidirectional, persistent** stream — reads all boot output,
  injects bootloader keystrokes, **stays attached for the whole job**. (`base.jinja2`,
  `device-integration.rst`.) Panio would be the **server** of that TCP stream.
- *Fuchsia:* a **device-file path** (`DeviceConfig.serial`) botanist `Open()`s itself. Panio
  must present a **PTY** proxying the real UART (or cede the port). The `$FUCHSIA_SERIAL_SOCKET`
  unix socket is created **by botanist, downstream** — not by panio (DISCREPANCY 1).

**panio today.** `serialcap` owns the port exclusively (an exclusive `flock` on a lockfile via `std::fs::File::try_lock`, in `daemon.rs`)
and is **already a fan-out supervisor**: raw bytes → broadcast to WebSocket clients
(`serial_io.rs:321`) + tee to the JSONL capture thread (`:317`). It exposes HTTP+WebSocket on
`127.0.0.1:8724`; `/stream` is **already bidirectional** — clients write bytes back via
binary/text frames → `write_tx` → `wr.write_all()` (`server.rs:210-223`, `serial_io.rs:328-332`).
Interactive `tio` is a separate direct subprocess (`_cli.py:972`), bypassing the daemon.

**Delta.** The bidirectional plumbing and the tee architecture **already exist** — but the
only wire formats are WebSocket and an on-disk JSONL log, and **neither ecosystem speaks
either**. The gap is two additional *raw* endpoints hung off the same supervisor:
1. a **raw TCP listener** (ser2net-equivalent) for LAVA's `telnet host port`; and
2. a **PTY** whose slave path is given to botanist as `DeviceConfig.serial`.

Because the daemon already multiplexes reads and accepts writes, both are additive listeners
on the existing read-broadcast / `write_tx` channels — **not** a rearchitecture. This also
directly delivers the owner's wanted feature (*agents writing to serial*, e.g. a
`panio serial send` verb) from the same channel. Effort: **medium** (new listeners + write
arbitration between `tio`/sockets/`send`). The interactive workflow (JSONL, dashboard, `tio`)
is **preserved** because the new endpoints are siblings, not replacements.

**Open design point — Fuchsia port ownership.** botanist wants to own the UART. Two options:
(a) panio hands botanist a **PTY** and keeps owning the real port (so JSONL/dashboard keep
working during CI); or (b) panio **releases** the port to botanist for the task (loses
capture during the run). (a) is preferable and consistent with "preserve interactive
workflow," at the cost of a PTY proxy hop.

---

## 4. Deploy — detail

**Required.**
- *LAVA:* the **dispatcher owns** it — downloads kernel/ramdisk/DTB into its own TFTP tree and
  serves them; the boot action issues bootloader `tftp` commands against those paths.
  (`actions-deploy.html`.) Delegating to an external TFTP server means leaving the standard
  `tftp` method.
- *Fuchsia:* **botanist owns** paving/zedboot; artifacts arrive via CIPD/CAS.

**panio today.** Pure-Python DHCP+TFTP over a direct USB-Ethernet link, IP `192.168.99.1/24`,
single client `.100` (`_dhcp.py:59`), read-only TFTP (`_tftp.py`). Started/stopped as a pair;
**no selective disable** (`_netboot.py:322-393`). `tftp_root` required for `netboot start`.

**Delta.** In **both** ecosystems the orchestrator owns deploy, so for CI panio's netboot
must **stand down** to avoid two DHCP/TFTP servers contending for the link. This is an
*ownership* resolution, not a feature gap: panio netboot stays valuable for **interactive/
agent bringup** and for boards outside these CI stacks, but it is **not** the CI deploy path.
Effort: **trivial** (don't run netboot under CI; document it). A "full" path where panio
serves images is possible but means a non-standard LAVA deploy method — out of scope for v1.

---

## 5. Boot / detection — detail

**Required.** *LAVA:* `boot` action matches a list of unique `prompts:` on the serial stream.
*Fuchsia:* botanist boots/paves and polls for `summary.json`.

**panio today.** No prompt-match / wait-for-regex; detection is human-eyeball via dashboard
or external polling of the JSONL (`serial log --follow`, `--since <seq> --json`). No hooks.

**Delta.** Minimal for CI: once the orchestrator can *attach to the raw serial stream*
(§3), **boot detection becomes the orchestrator's job** (LAVA matches prompts; botanist polls
`summary.json`). A `panio serial wait --match <regex> --timeout` verb would be a nice
ergonomic win for the **agent/interactive** workflow and for the "minimum viable" CI path, but
it is **not required** by either ecosystem. Effort: **small**, **optional**.

---

## 6. JTAG (future primitive the owner wants) — where it fits

JTAG is not a fourth sibling; it **cuts across** three primitives, which is the argument for
modelling panio's primitives as **verbs backed by selectable per-target backends**:

- **Reset/power:** a JTAG adapter drives `SRST`/`TRST` → another backend for `reset`, alongside
  DTR and PDU scripts. (It generally **cannot cut rail power**, so it complements, not replaces,
  the PDU/script `power off`.)
- **Deploy:** JTAG flash programming (OpenOCD `program`, J-Link) is an alternative deploy
  backend for boards that don't netboot — relevant to the deploy-ownership story.
- **Debug:** genuinely new — halt/resume, memory/register access, and a **GDB server socket**
  (OpenOCD `:3333`) + Tcl/telnet RPC (`:4444`/`:6666`).

Note the pattern: OpenOCD's endpoints are **TCP sockets** — the *same shape* as the raw-serial
TCP listener and serialcap's existing fan-out. So JTAG slots into the same "daemon owns the
hardware, exposes machine-drivable sockets + discrete verbs" architecture. Neither LAVA nor
botanist leans on JTAG for the common path, so it is best scoped as a **first-class extension
point in the v1 design, implemented later** (unless the owner wants it in the first pass).

---

## 7. Host-OS risk (macOS-as-CI-worker)

- **LAVA workers are Debian-only.** No macOS support exists in the docs; a worker just needs
  the `lava-dispatcher` package on Debian. (`first-installation.html`, `debian.html`.) → A
  panio-as-LAVA-lab control host **must be Linux**.
- **Fuchsia Swarming bots** run on Linux in practice (the bot is Python + host tools); macOS
  bots exist for macOS *build* tasks but a Fuchsia-device bot host is Linux.
- **panio's core power/serial/netboot path is clean on Linux** (per the surface map): the
  macOS-specific machinery is OCR (Apple Vision) and the TFTP BPF raw-frame workaround — both
  **irrelevant to headless CI**. Serial/power/DTR are platform-agnostic (Rust CLI + daemons).

**Conclusion:** keep macOS as a first-class **interactive bringup** host, but treat **Linux as
the only supported CI control-host OS**. This is a documentation/positioning decision, not new
code — but it must be stated plainly so nobody tries to run a LAVA worker on a Mac.

---

## 8. Summary of deltas, ranked by effort

| # | Change | Primitive | Effort | Required by |
|---|---|---|---|---|
| 1 | Don't run panio netboot under CI (orchestrator owns deploy) | Deploy | trivial (docs) | LAVA, Fuchsia |
| 2 | ~~Discrete `power on` / `power off` CLI verbs + config~~ — **shipped** (`[power]` hooks) | Power | done | LAVA |
| 3 | Raw **TCP** serial passthrough listener (ser2net-equivalent) off serialcap | Serial | medium | LAVA |
| 4 | **PTY** serial proxy (device-file path) for botanist + write arbitration | Serial | medium | Fuchsia |
| 5 | ~~`panio serial send` (agent write-to-serial)~~ — **shipped** | Serial | done | owner feature; helps both |
| 6 | LAVA device-type template + device dictionary generator (adapter) | — | small–medium | LAVA |
| 7 | botanist device-config emitter + bot-host power/recipe hook (adapter) | — | medium | Fuchsia |
| 8 | `serial wait --match` boot-detect helper | Boot | small (optional) | neither (ergonomics) |
| 9 | JTAG backend (reset/deploy/debug, GDB socket) | cross-cut | large | future / owner |
| — | KCIDB result emission (test-exec + result schema) | above the line | large | **likely out of scope** |
