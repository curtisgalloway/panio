# Distributed control: implementation plan

> **Status: historical — Phases 0–5 shipped** (#20 for 0–3, #22 for 4–5;
> 2026-06-01), **then superseded by the Rust control plane** (see
> [config-redesign.md](https://github.com/curtisgalloway/panio/blob/main/notes/config-redesign.md)). This doc records the original
> *Python* phasing; the mechanisms it names (`@remote_capable`,
> `PANIO_TARGET_CONFIG`, the `targets/*.toml` fallback) are not how the Rust
> CLI works — dispatch now ships a lab slice as a temp file and re-invokes with
> `--lab` (`cli/src/dispatch.rs`), and the legacy targets dir is never read.
> This sequences the build of the
> [distributed-control design](../docs/distributed-control.md) into self-contained,
> independently mergeable phases. Read the design doc first — this is the *how*
> and *in what order*, not the *what* and *why*.
>
> **Shipped in 0–3:** `_ssh.py` transport, `_lab.py` lab model + `--lab`,
> `@remote_capable` transparent re-exec (`_remote.py`), and the tunnelled remote
> `console`. Two refinements emerged during the build and are in the code: a
> per-host **`panio_cmd`** (the remote PATH problem) and the guidance to set a
> per-host **`identity`** (ssh-agent key-spray vs. `MaxAuthTries`) — both folded
> into "Open decisions" below.

## Backbone

Two facts about the current code (verified against `src/panio/`) shape the
whole plan:

1. **`_resolve(name)` in `src/panio/_cli.py` is the single chokepoint** every
   one of the ~20 target-bearing commands passes through to load its
   `TargetConfig`. That means **one interception point can make all the one-shot
   commands location-aware at once** — when the target's host is remote, re-exec
   the entire `panio` invocation on that host and never run the local body.
   Only the four streaming commands (`console`, `video preview`, `serial watch`,
   `serial connect`) need bespoke handling.
2. **Control hosts are stateless** (design principle 3), so a re-exec'd command
   must be *handed* the config slice it needs — the dev machine owns the lab
   file. The daemons (`serialcap`, `hdmicap`) already write a `daemon.json`
   discovery file with their TCP `port`, and the dashboard already honors a
   `?serialws=` override — so streaming needs **no daemon changes**, only SSH
   port-forwarding plus URL construction.

There is **no existing SSH code** in the Python source — the transport layer is
greenfield.

### The localhost-ssh test fixture

A single trick makes the entire remote path testable in dev and CI without a
second machine: define a host whose ssh destination is `localhost`. Re-exec,
config-shipping, and tunnelling then run against the same machine over *real*
SSH. Every phase below is validated this way; CI gets a step that adds the
runner's own key to `authorized_keys` and exercises a `localhost` host.

## Phase 0 — SSH foundation ✅ (shipped, #20)

**New:** `src/panio/_ssh.py`. Pure infrastructure; nothing user-facing.

Owns one **ControlMaster** connection per host (so only the first call per host
pays the SSH handshake; the socket path comes from the host's lab config). API:

- `run(host, argv, *, input=None) -> CompletedProcess` — non-interactive re-exec.
- `run_interactive(host, argv) -> int` — `ssh -t` for tio and other PTY commands.
- `forward(host, remote_port) -> contextmanager(local_port)` — holds an `ssh -L`
  tunnel for its lifetime; picks a free local port.
- `read_remote_file(host, path) -> str | None` — reads a remote discovery file.

A `Host` dataclass: `ssh` destination (required), optional `identity`,
`control_path`.

**Tests:** point a `Host` at `localhost`; assert `run` round-trips stdout/exit
code, `forward` yields a working local port, `read_remote_file` reads a known
file. **Delivers:** the transport primitives, unused so far.

## Phase 1 — Lab file model ✅ (shipped, #20)

**New:** `src/panio/_lab.py`. **Touches:** `_config.py`, `_cli.py`.

Parse the single git-tracked lab file: `[hosts.*]` plus nested `[targets.*]`
with `[targets.X.netboot]`, `[[targets.X.serial]]`, `[targets.X.video]`,
`[targets.X.power]`. **Per-resource `host` is parsed from day one** (forward-
compatible with multi-host targets), but this phase **validates that all of a
target's resources resolve to one host** and errors clearly on a cross-host
target. That restriction is what the deferred multi-host phase lifts.

The compatibility hinge: `_lab` resolves a target down to the **existing flat
`TargetConfig`** plus a single `host` string. Command bodies stay unchanged —
they keep consuming `TargetConfig` exactly as today. `_resolve()` is widened to
return `(TargetConfig, host)`.

- "Point panio at the lab": a `--lab` global option / `PANIO_LAB` env var /
  conventional default path (exact spelling is an open decision — see below).
- **Backward compatibility:** with no lab configured, panio reads the legacy
  `~/.config/panio/targets/*.toml` as today and `host` is always `local`, so
  behavior is byte-for-byte identical. Migration into a lab file is opt-in.

**Tests:** lab round-trip; target-default `host` inheritance by resources;
cross-host target rejected; legacy-mode behavior unchanged. **Delivers:** you can
*describe* a (single- or multi-host) lab and load it; everything still executes
locally.

## Phase 2 — Transparent re-exec for one-shot commands ✅ (shipped, #20)

**Touches:** `_cli.py`, `_ssh.py`, the config loader.

The interception. A `@remote_capable` decorator on each target command (≈20
one-line additions) that, after Typer parses args, inspects the resolved `host`;
if remote it:

1. Serializes the resolved `TargetConfig` to a temp TOML and ships it over the
   ControlMaster connection.
2. Re-execs `panio <same subcommand + args>` on the host with
   `PANIO_TARGET_CONFIG=<tmp>` — a new env var the loader honors so the
   stateless remote runs against the injected slice.
3. Streams stdout/stderr, propagates the exit code, cleans up the temp file.

The local body is skipped entirely for remote targets. Streaming commands are
marked `@remote_capable(mode="tunnel")` and are no-ops here (Phase 3 handles
them).

**Tests:** against the localhost-ssh host, assert `power-cycle`,
`netboot status`, and `serial log` produce identical results run "remotely" vs
locally. **Delivers:** every fire-and-read command is location-transparent.

## Phase 3 — Streaming & the console ✅ (shipped, #20)

**Touches:** `_cli.py`, `_serial.py`, `_video.py`.

The headline capability. For a remote `console` / `video preview`:

1. Re-exec the daemon start (`serial watch` / `video watch`) on the host
   (idempotent).
2. `read_remote_file` each daemon's `daemon.json` to learn its port.
3. Open `forward()` tunnels for **both** hdmicap and serialcap.
4. Open the browser at
   `http://127.0.0.1:<local-hdmi>/?serialws=ws://127.0.0.1:<local-serial>/stream`
   — the existing override stitches the two forwarded ports together with no
   daemon changes.

`console` is **foreground-blocking**: it holds the tunnels until Ctrl-C, then
tears them down (no persistent local state). `serial connect` routes through
`_ssh.run_interactive` (`ssh -t`, no tunnel needed).

This is also where the design's anti-reverse-proxy decision pays off: the browser
on the dev machine is the rendezvous point, so the two daemons never need to
reach each other — which is what makes multi-host dashboards work later for free.

**Tests:** localhost-ssh host — assert the forwarded ports serve the dashboard
and the constructed URL resolves end-to-end. **Delivers:** the console feels
local.

## Phase 4 — `setup --host <name>` ✅ (shipped, #22)

**Touches:** `_cli.py`.

`setup` builds the Rust daemons and installs helpers, which must happen on the
host wired to the hardware. The remote variant re-execs `panio setup` on the
named host over SSH. Small. **Delivers:** provision a control host from the dev
machine.

## Phase 5 — Discovery-assisted `configure` ✅ (shipped, #22)

**Touches:** `_cli.py`, `_lab.py`.

`panio configure <target> --serial <host> --video <host>` runs discovery on the
named hosts over SSH (enumerate serial devices, USB-Ethernet interfaces, capture
devices), merges the inventories into a **proposed** target block, prints a
**diff** against the lab file, and writes nothing until approved. Because the lab
file is in git, approval is a commit; an agent can stage a proposal but never
silently mutate the authoritative config. **Delivers:** the propose/approve
configuration workflow from the design.

## Sequencing

Phases **0 → 1 → 2 → 3** deliver the headline value — location-transparent
commands plus a local-feeling console — and are the natural first milestone.
Phases **4 and 5** are quality-of-life and may be reordered or deferred past the
first milestone. Each phase is an independently reviewable, mergeable PR with its
own tests and doc updates.

## Deferred (designed-for, not built)

- **Multi-host per-resource routing.** Phase 1's same-host validation is the
  single gate; lifting it means `_resolve` returns per-resource hosts and the
  dispatch routes each resource independently. The schema and the dev-machine-hub
  transport already accommodate it.
- **`console --detach`** and the local tunnel registry it requires (transient
  dev-machine runtime state).
- **Multi-user locking / reservations.**
- **Multi-file / multi-lab composition.**

## Decisions (resolved during the build)

- **Config-shipping mechanism (Phase 2).** ✅ Temp TOML over SSH +
  `PANIO_TARGET_CONFIG` (keeps stdin free, avoids arg-quoting fragility).
- **How to "point at the lab"** ✅ `--lab` global option with `envvar=PANIO_LAB`;
  no lab configured → the legacy `~/.config/panio/targets/*.toml` (host = local),
  so existing behavior is byte-for-byte unchanged.
- **Per-resource schema carried ahead of use.** ✅ Done — Phase 1 parses
  per-resource `host` (and rejects cross-host targets) so the multi-host phase
  needs no schema migration.
- **Remote `panio` discovery.** ✅ New finding: re-exec runs over a
  *non-interactive* ssh whose PATH may omit `~/.local/bin`, so bare `panio`
  can fail to resolve. Added a per-host **`panio_cmd`** (default `"panio"`)
  to pin an absolute path. Caught by the localhost integration test.
- **ssh-agent key-spray.** ✅ New finding: an agent offering many keys (1Password)
  trips `MaxAuthTries` on the first connect. Setting a per-host **`identity`**
  makes panio pass `-i <key> -o IdentitiesOnly=yes`. This is the user's ssh
  config concern; the lab field is the lever.

## Validation status

Phases 0–3 are CI-green (unit + a localhost-ssh integration tier on the Linux
runner) and were validated locally end-to-end against a real `localhost` host.
The macOS SSH path (`_control_dir`'s `/tmp` socket-path handling) is confirmed on
macOS 26.5/arm64. **Not yet validated:** the full `console` flow on real
hardware (HDMI capture + serial), which CI can't exercise.
