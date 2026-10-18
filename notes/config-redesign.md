<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Config redesign: a CLI-managed lab

Status: **Rust control plane implemented through full command parity** (the `cli/`
crate): config model + CRUD + doctor (R1), per-channel dispatch + SSH transport
(R2), and the serial / video / power / netboot / netif runtimes plus `console`,
`discover`, `configure`, `setup` (R3) — rig-verified against a live Pi 5 bench
(serial round-trip, live HDMI capture, netbootd serving DHCP+TFTP, netif mode
transitions; a real TFTP boot — Fuchsia shim + ZBI on the Pi 5 — verified
2026-06-04). Remaining: live remote-host dispatch test (needs a second control
host) and docs/cutover. (The Openterface CH9329 HID backend, formerly listed
here as deferred, shipped as the `ch9329` helper crate — see
notes/ch9329-spec.md. OCR landed 2026-06-05: `panio video read` wraps the
hdmicap daemon's `GET /ocr`.)

Helper state/runtime-dir API (landed 2026-06-05): panio exports
`PANIO_STATE_DIR` (`~/.config/panio/helpers/<name>/`) and
`PANIO_RUNTIME_DIR` (`/tmp/panio-<uid>/<name>/`) — directories created —
whenever it invokes a helper (hooks, `panio helper`, daemon spawns).
Helpers prefer the env vars and fall back to the same literal paths when run
standalone; hdmicap/serialcap/hidrig/zigplug all read them, and zigplug
auto-migrated its `zigbee.db` from the top of `~/.config/panio/` into its
namespaced dir. Contract documented in docs/dev/adding-power-helpers.md.
The Python Stages 1–4 were the original tested reference; the Python tree
(`src/panio/` + its pytest suite) has since been **removed** now that the Rust
`cli/` crate is the only control-plane implementation.

## Why

Panio's configuration today is split-brained:

- **Legacy** per-target files (`~/.config/panio/targets/<name>.toml`) are
  read *and* written — `target set`, `serial setup`, the video/HID setup
  commands all call `_config.save_target()` through a hand-rolled `_to_toml()`
  serializer. They always run on the local host.
- **Lab** files (`--lab` / `PANIO_LAB`) describe all hosts and targets with
  per-resource host binding, but are **read-only**: there is a parser and no
  writer.

The thing that prompted this redesign was noticing there's no command to *view*
or *change* configuration — you hand-edit TOML. But you can't bolt a clean
config CLI onto two config systems, one of which can't be written. So the
decision is to collapse to **one** model:

> The lab file is the single source of truth, fully CLI-managed, with no
> backward-compatibility shim for the legacy per-target files.

A config CLI is a forcing function on the data model: the verbs you want to type
pin down the nouns. This document records the model, the surface, and the
dispatch architecture the surface demands.

## The data model

A **channel** has two independent coordinates: where it physically *is* (a host)
and what it logically *serves* (a target). The lab file is organized
target-first (channels nested under the target they serve) because that keeps
the common single-host case trivial; each channel carries an optional `host =`
that names where it physically lives.

```
Lab
 ├── hosts: { name -> Host }          # SSH reachability only
 └── targets: { name -> Target }
                    ├── host           # default host for this target's channels
                    ├── note           # optional free-text, survives CLI rewrites
                    └── channels
                         ├── netboot   (singleton)
                         ├── serial[]  (collection, named)
                         ├── power     (singleton)
                         ├── video     (singleton)
                         └── hid       (singleton; added after this design — opaque injector cmd)
```

- **Host** — a machine panio reaches over SSH. Fields: `ssh` (required;
  `"local"` = dev machine), `identity`, `control_path`, `panio_cmd`. Hosts
  carry **connection info only** — never device paths.
- **Target** — a logical device. Has a default `host`, an optional `note`, and a
  set of channels. A channel omitting `host` inherits the target's `host`, which
  defaults to `local`.
- **Channel** — one piece of a target's hardware. Each type keeps its own shape
  (they always have): `netboot` (interface, host_ip, tftp_root), `serial`
  (name, device, baud, power_sense_signal), `power` (cycle_cmd,
  serial_interface), `video` (device). Every channel may carry `host =`.
  *Since this design: `power` grew `on_cmd`/`off_cmd`/`state_cmd` (the generic
  hook block) and a `hid` channel (`cmd`) was added — see
  [power.md](../docs/power.md) and [hid.md](../docs/hid.md) for the current shapes.*

### Identity & uniqueness rules

- Host names are unique within the lab.
- Target names are unique within the lab.
- Serial channel names are unique within a target.
- `netboot`, `power`, `video` are **singletons** — at most one per target, so
  their "name" is just the type.
- Any channel's `host` must be `local` or a declared host. Validated on load
  *and* after every mutation (one shared `validate()`).

### What we are NOT building

Channel→target is one-at-a-time (a cable goes to one place; rewiring is a config
edit, not a runtime event). The genuinely simultaneous case — one host serving
several targets at once — is already expressible (multiple targets reference the
same host). So there is **no** first-class many-to-many channel entity and no
runtime channel multiplexing.

## File location & discovery

Retiring legacy means the lab file needs a default — requiring `--lab` on every
call would kill the "simple on one host" goal.

Discovery order: `--lab PATH` > `$PANIO_LAB` > `~/.config/panio/lab.toml`.

`panio init` creates an empty lab at the default path. Commands that need
config but find none fail with a hint (`run panio target add ...`), not a
stack trace.

## Round-trip: tomlkit surgical edits

The lab file is git-tracked and human-authored, so the CLI must edit it
**politely**: preserve hand-written comments, ordering, and formatting anywhere
in the file, touching only the tables it changes. This rules out
parse-then-regenerate. The Python design used **`tomlkit`**; the shipped Rust
CLI uses **`toml_edit`** (`cli/src/labfile.rs`) for the same surgical document
edits.

- The lab is parsed into typed dataclasses for *reading* (resolution,
  validation, display).
- Mutations go through a thin `tomlkit`-backed document layer that edits the
  live `TOMLDocument` and writes it back, preserving trivia.
- The machine-generated remote slice (shipped over SSH as a temp lab file +
  `--lab`, never hand-edited) keeps using a plain dump — no round-trip concern
  there.

## Command surface

The verb asymmetry is the singleton/collection split surfacing exactly where it
belongs: collections get `add`/`rm` with a name; singletons get `set`/`rm`.

```
read    panio config show                whole lab as a tree
        panio config path                print the active lab file path
        panio config edit                open the raw lab file in $EDITOR
        panio target list | show NAME    show = RESOLVED: each channel + host
        panio host   list | show NAME    inverse index: channels here, targets served
        panio doctor [TARGET|--host H]   probe reality vs config over SSH

write   panio init
        panio host   add  NAME --ssh ... [--identity] [--control-path] [--panio-cmd]
        panio host   set  NAME [--ssh] [--identity] ...
        panio host   rm   NAME
        panio target add  NAME [--host H] [--note ...]
        panio target set  NAME [--host H] [--note ...]
        panio target rm   NAME
        panio serial  add NAME -t TARGET --device ... [--host] [--baud] [--sense]
        panio serial  set NAME -t TARGET [--device] [--baud] [--sense] [--host]
        panio serial  rm  NAME -t TARGET
        panio netboot set -t TARGET --interface ... [--host] [--host-ip] [--tftp-root]
        panio netboot rm  -t TARGET
        panio power   set -t TARGET [--cycle-cmd] [--serial-interface] [--host]
        panio power   rm  -t TARGET
        panio video   set -t TARGET --device ... [--host]
        panio video   rm  -t TARGET
```

*Since this design, the surface also grew `panio hid set/rm` (the `hid`
channel), `panio adb set/rm` (the `adb` channel for Android DUTs, with its
runtime verbs `adb shell/run/screencap/input/show/devices`),
`power set --on-cmd/--off-cmd/--state-cmd`, the runtime verbs
that consume them (`power on/off`, `power-state`, `hid send/serve/stop`),
and `target rename OLD NEW` (a comment-preserving key move; daemons keyed to
the old name must be restarted).*

### Two read views, two purposes

- `target show` renders the **resolved** target: inheritance applied, each
  channel annotated with the host it lands on. This is the truth the runtime
  acts on.
- `config edit` / `config path` expose the **raw** file for hand-editing.

### Config writes are local and pure

A config command edits *your* lab file on *your* machine; it never SSHes. So
config verbs are **not** `@remote_capable`. This keeps editing offline-capable
and removes a whole class of failure. The corollary: the old `serial setup`
conflated "record this channel" (local, pure) with "probe the `/dev`" (remote,
impure). We split them — `serial add` only records; `doctor` does the probing.

## Dispatch architecture (relax the single-host invariant)

We are relaxing the single-host-per-target invariant in the same effort. Today
dispatch is a **command-level** decision: `@remote_capable` resolves *the
target's* one host and re-execs the whole command there. That cannot survive
per-channel hosts, because "which host" is now a property of *the channel a
command touches*, not the command. Dispatch moves down a level.

### Single-channel commands

Most commands touch exactly one channel (`serial connect` → a serial channel;
`screenshot` → video; `netboot start` → netboot; `power-cycle` → power). The
re-exec model survives: resolve *that channel's* host and re-exec the whole
command there (whole-command re-exec is what gives the interactive serial PTY on
the remote for free). The decorator just needs to know which channel each
command operates on.

### Per-host config slice

`_remote.dispatch` currently ships the whole resolved target and the remote
treats everything as local. With per-channel hosts that's wrong — bench2 must
not see bench1's channels. The slice becomes **per-host**: ship only the
channels that resolve to the destination host, with their `host` normalized to
local, so the remote's "everything I see is local" assumption stays true.

### Composite commands — staged

A composite like `boot` (power-cycle + netboot + tail serial) cannot be carried
by one re-exec if its channels span hosts; it must orchestrate locally and fan
out per channel.

**Decision: stage it.** First implement per-channel dispatch for single-channel
commands (this delivers genuine multi-host targets for ~all of the surface).
Composite commands **require their channels to be co-located on one host for
now** and error with a clear message otherwise
(`boot needs power, netboot, and serial on the same host; serial is on bench2`).
Cross-host fan-out is deferred until a real target needs it — observation
channels (video) are the ones that tend to wander, and observation is
single-channel. The per-channel dispatch primitive is built so single-channel
commands are the trivial one-channel case, leaving room to add fan-out later
without reshaping it.

## Staged implementation plan

1. **Model + tomlkit foundation.** New lab dataclasses; default-path discovery
   and `init`; `tomlkit`-backed surgical read/write; shared `validate()` on load
   and after mutation. Add `tomlkit` dep.
2. **Read surface.** `config show/path/edit`, `target list/show` (resolved with
   per-channel host), `host list/show` (inverse index).
3. **Write surface (CRUD).** Local, pure mutators for host/target/serial/
   netboot/power/video, each validating before save.
4. **`doctor`.** Probe reality vs config over SSH; absorb the probing that used
   to live inside `setup` commands.
5. **Per-channel dispatch.** Reusable primitive; per-host slice; composite
   co-location guard; remove the multi-host rejection in resolution.
6. **Retire legacy + docs/tests.** Delete the `~/.config/panio/targets` path
   and the user-facing `_to_toml`; migrate tests; update `AGENTS.md`, `docs/`,
   and `README`.

## Dispatch design (Stage 5 — specified, to be built in Rust)

This is the per-channel dispatch design worked out before the Rust pivot. It was
*not* implemented in Python; it is the spec the Rust version implements. The
read model needed for it (`resolved_target`, `host_slice`) was built and tested
in Python and ports directly.

**Command → channel.** Each location-transparent command touches exactly one
channel kind:

| command(s)                                              | channel | selector            |
|--------------------------------------------------------|---------|---------------------|
| `netboot start/stop/status/tftp-root/logs/link-*`      | netboot | —                   |
| `netif mode/status`                                     | netboot | — (the USB-Eth link)|
| `power-cycle`, `power-state`                            | power   | —                   |
| `serial connect/watch/dtr/reset/show`                   | serial  | `--interface` name  |
| `console` (dashboard)                                   | *composite* (serial + video) |

**Host resolution** — `channel_host(target, kind, serial_name)`:
- singleton kinds (netboot/power/video): the channel's `host`, else target default;
- serial *with* a name: that interface's `host`, else target default;
- serial *without* a name: the common host of all serial interfaces — **error** if
  they span hosts (the `serial watch` case: the daemon owns every interface, so they
  must be co-located);
- channel absent entirely: the target's default host (the body then reports the
  missing channel).

**Slice** — `host_slice(target, host)`: the channels resolving to `host`, flattened
to a single-host `TargetConfig`, channels on other hosts omitted. This is both what
runs locally (`host == local`) and what is shipped to a control host. Because the
slice is single-host with `host` normalized to local, the remote sees everything as
local and never re-dispatches.

**Dispatch flow** per command:
1. If invoked with an injected slice (the `PANIO_TARGET_CONFIG` env, set by the
   dispatcher) → run the body locally; it reads the shipped slice.
2. Else resolve the channel's host. If `local` → run the body locally against
   `host_slice(target, local)`. Otherwise ship `host_slice(target, host)` to that
   host and re-exec the *same* command there, passing stdio + exit code through
   (`serial connect` uses an `ssh -t` PTY).

**Composite commands** (today `console`; a future `boot`) require their channels
**co-located on one host** and error clearly otherwise
(`boot needs power, netboot, serial on one host; serial is on bench2`). Single-channel
commands are the trivial one-host case of the same mechanism. Cross-host fan-out is
deferred until a real target needs it.

**Transport** stays as today: shell out to `ssh` with ControlMaster multiplexing
(one handshake per host), `ssh -t` for interactive serial.

## Pivot to Rust

The repo is already mid-migration from Python to Rust: `netbootd` replaced the
Python `_dhcp`/`_tftp` engines, `hdmicap` replaced the Python video path, `serialcap`
is the Rust serial daemon. The CLI + orchestration + device glue is the last large
Python holdout. Two forces make finishing it in Rust the right call: deploying *both*
a Python env and Rust binaries to every control host is the friction the re-exec model
keeps fighting (`panio_cmd`, PATH notes in AGENTS.md), and a single static binary
deployed beside the daemons removes it; and the dispatch logic gains real safety from
Rust's enums/exhaustive matching exactly where it felt brittle in Python.

Decision: **do not finish Stages 5–6 in Python.** The dispatch design above plus the
Python Stages 1–4 (config model, CRUD, `doctor`) are the reference. Build the control
plane once, in Rust, to parity — then retire the Python tree (including the already-
superseded `_dhcp`/`_tftp`).

### Rust port plan

- **R1 — crate + config model.** New `panio` CLI crate (`clap` derive). Lab data
  model with `serde` for the typed/read side; **`toml_edit`** for comment-preserving
  lab editing (the `cargo`-grade analog of `tomlkit`). `validate()` shared by load and
  save. Port `init` + the read surface (`config show`, `target/host list/show`) and the
  write surface (host/target/serial/netboot/power CRUD) from Stages 1–3.
- **R2 — doctor + dispatch.** Port `doctor`; implement the dispatch design above:
  `ChannelKind` enum, `channel_host`, `host_slice`, `ssh` module (shell out with
  ControlMaster), per-host slice shipping, composite co-location guard.
- **R3 — runtime glue.** Port the command bodies that drive the daemons and the link
  (`netboot`, `netif`, `serial`, `video`, `power`, `ocr`, `state`). Drop legacy
  `_dhcp`/`_tftp`.
- **R4 — HID, polish, cutover.** Port `_hid` (or keep short-term), finish docs
  (`AGENTS.md`, `docs/`, `README`), retire the Python tree.

### Open structural decisions (R1)

- **Workspace vs standalone.** The three daemon crates currently build standalone and
  CI gates them individually; converting the repo to a Cargo workspace changes the
  shared `target/` layout and could disrupt `make install`/CI. R1 starts the `panio`
  crate **standalone** to avoid touching working infrastructure; a workspace (to share
  types with the daemons via a `panio-core` crate) is a deliberate later step, not a
  prerequisite.
- **Binary name.** The crate produces the `panio` binary, same UX as today.
