<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Target leases (#360): design

Status: built (#360). See [As built](#as-built) for where the code differs from this design; the code wins.

**Terms**

- **Target**: one board under test, a `[targets.<name>]` block in the lab file.
- **Control host**: the machine wired to a target's hardware; panio re-execs
  commands there over SSH (`dispatch.rs`).
- **Default host**: the target's `host` field, which every channel uses unless
  it names its own (`Target::default_host`).
- **Lease**: a time-limited reservation of a whole target by one holder,
  recorded on the target's default host.
- **Holder**: whoever acquired the lease and has its token. One person, one
  agent, or a group of agents the token was handed to.

## Why

Two uses, neither possible today:

1. **A shared lab of scarce boards.** Several people share a set of boards.
   Someone books a board for a limited time and nobody else drives it until
   the booking ends or is released.
2. **Parallel agents on one lab.** Several agents run at once against the
   same lab. Today nothing stops a second agent power-cycling a board in the
   middle of the first one's fastboot flash or OS install. With a lease, each
   agent's multi-step workflow gets exclusive control of its board, and a
   second agent is told who holds it and until when, instead of breaking it.

`docs/distributed-control.md` lists "Multi-user / locking / reservations" as
deferred and assumes a single user. This design lifts that assumption
without changing behavior for anyone who does not use it.

## Goals

- `panio lease acquire|release|renew|status` for one target.
- A holder is recognized by a **token**, so two agents on the same dev
  machine, same Unix user, are different holders.
- Non-holders are refused **mutating** commands on a leased target; **reads**
  still work, so others can watch.
- Leases end on their own: a total **duration** and an **inactivity** timeout.
- Anyone can **preempt** a lease explicitly, and the displaced holder finds
  out on its next command.
- Ending a lease (release, expiry, preemption) **cleans the target up** for
  the next holder.

## Non-goals

- **Security.** A lease prevents accidents between cooperating users and
  agents. Anyone can preempt, and the lease file is readable by anyone who can
  log in to the control host as the lab account. Do not describe it as access
  control.
- **A booking calendar.** No future reservations, no FIFO queue. Waiting is
  `--wait`, which polls.
- **Per-channel leases.** The lease covers the whole target. Two agents that
  want to share one board share one token.
- **Guarding the daemons' own HTTP APIs.** A process that talks to hdmicap or
  serialcap directly with the daemon token bypasses panio and the lease. That
  is already "driving around panio" (root `AGENTS.md`, gotchas).
- **A lease service or reaper daemon.** No new long-running process.

## Anti-goals (failure even if it works)

- **Breaking existing labs.** A single-user lab that never runs `panio lease`
  must behave byte for byte as before.
- **Blocking observers.** Refusing `video shot`, `serial log` or `power-state`
  to non-holders would make a shared lab opaque.
- **Silently wrong ownership.** A command must never run as if it held a
  lease it does not hold. When in doubt, refuse and name the holder.
- **Printing the token where it shouldn't be.** Only `lease acquire` prints
  it (as `video preview` is the only command that prints a daemon token, #196).
  `lease status` shows owner and times, never the token.

## Decisions (the maintainer's, 2026-10-22)

| Question | Decision |
|---|---|
| Holder identity | **Token + owner label.** `acquire` prints a random secret token; the holder passes it in `PANIO_LEASE`. The owner label (`--owner`, default `<user>@<dev-host>`) is display only. |
| Non-holder commands | **Refuse mutating, allow reads.** Mutating commands exit with a new error kind; reads run. |
| Acquire when held | **Fail now, optional `--wait <dur>`** that polls until free or the deadline. |
| Cleanup on end | **Stop the target's daemons**, **kill the holder's recorded panio processes**, **take the netboot link down** (`netif mode off`). Power is left alone. |
| Default timing | **2 h duration, 30 min inactivity**, overridable with `--duration` / `--idle`, each capped at 24 h. |
| Preemption | **Anyone, with `--preempt`.** Recorded; the old holder's next command fails with a "preempted by X" error. |
| Unleased targets | **Run freely, as today.** A per-target lab field `lease_required = true` makes mutating commands refuse unless the caller holds a lease. |
| Expiry cleanup timing | **Lazy.** The next mutating panio command, `acquire`, `release` or `status` touching the target finds the expired lease and cleans up first. |
| Cross-host targets | **Excluded from v1.** `lease acquire` refuses a target whose channels span more than one host; a follow-up issue covers them. |
| Interactive sessions and idle | **A live registered process counts as activity.** An open `serial connect`, `console` or `adb shell` keeps the lease from going idle; the total duration still applies. |
| MCP | **Add lease tools to the MCP server.** The server holds the token in memory and applies it to its own child commands. |
| `video preview` | **Holder only** (mutating): its dashboard carries live keyboard and mouse input when the target has a hid channel. |

## How it works

### Where the lease lives

On the target's **default host**, in the durable state dir:
`~/.local/share/panio/<target>/lease.json` (under `state::state_dir`; the
per-target helper is private today and needs exposing), created 0600. In v1
every channel of a leasable target lives on that host, so the lease, every
check and every cleanup step happen on one machine.

The `lease` commands dispatch to the default host like any other runtime
command. A lab whose default host is the dev machine itself keeps the lease
locally, which is still useful for parallel agents on one machine.

### The record

```json
{
  "owner": "alice@laptop",
  "token": "<32 hex chars>",
  "acquired_at": 1792774800,
  "expires_at": 1792782000,
  "idle_secs": 1800,
  "last_used_at": 1792777330,
  "procs": [{"pid": 4242, "start_time": 1792777200123}],
  "preempted": null
}
```

After a preemption the file is replaced by the new holder's record, and a
short `lease-history.jsonl` beside it gets `{at, previous_owner, new_owner,
how: "preempted" | "released" | "expired"}`. That history line is how the
old holder learns why its token stopped working; the error quotes it.

The token is stored in plain text. Hashing it would need a new dependency and
buys nothing, since anyone who can read the file can also `--preempt`.
(My call.) Times are Unix epoch seconds: panio has no date/time crate, and
formatting for humans happens only in `lease status`. (My call.)

### Commands

```
panio lease acquire [TARGET] [--owner LABEL] [--duration DUR] [--idle DUR]
                             [--wait DUR] [--preempt] [--json]
panio lease renew   [TARGET] [--duration DUR]     # needs PANIO_LEASE
panio lease release [TARGET]                      # needs PANIO_LEASE, or --preempt
panio lease status  [TARGET]                      # anyone; omit TARGET for all on reachable hosts
```

- Target argument follows the CLI convention (positional or `-t`, never both).
- `acquire` prints the token on stdout (alone, or in `--json` with owner and
  expiry) so `export PANIO_LEASE=$(panio lease acquire pi5)` works. If the
  caller already holds the lease (its `PANIO_LEASE` matches), `acquire` is
  idempotent and behaves like `renew`.
- `renew` moves `expires_at` to now + duration (default: the lease's original
  duration), still capped at 24 h from now.
- DUR syntax: `90s`, `30m`, `2h` (match an existing duration parser in `cli/`
  if there is one; otherwise add a small one).

### Carrying the token

`PANIO_LEASE` is added to `ssh::FORWARDED_ENV`, so it reaches the control
host on stdin like `AMT_PASSWORD` and never appears on a remote command line.

Interactive dispatch (`Mode::Interactive`: `serial connect`, `adb shell`,
`console`'s remote leg) forwards no environment, because stdin is the user's
terminal. For those, the token travels in a 0600 file shipped beside the
lab slice in the same step that ships the slice, and the remote command line
names only the file's path. The remote panio reads it, deletes it, and
treats it as `PANIO_LEASE`. (My call; the builder confirms how each mode
ships its slice today and picks the narrowest way to add one file.)

### MCP

`mcp.rs` runs each tool as a `panio` child inheriting the server's
environment, but the token is minted after the server starts, so setting
`PANIO_LEASE` on the server is not enough. Add `lease_acquire`,
`lease_renew`, `lease_release` and `lease_status` tools. The server keeps the
token from `lease_acquire` in memory, per target, and sets `PANIO_LEASE` on
every child it runs for that target. One MCP server is one holder; a
`PANIO_LEASE` already in the server's environment is used as the initial
token for every target, so a harness can hand a server an existing lease.

### The check

Every runtime command that touches a target runs a lease check **on the host
where it executes**, after dispatch has decided the command runs locally. On
the control host the shipped slice has every `host` stripped, so the
re-exec'd panio lands in the local path and checks there. The obvious hook is
the `Ok(None)` arm of `dispatch::maybe_dispatch` (about 15 call sites), but
several paths bypass it and each needs the check too: `cmd_console` →
`remote_console` (`run_subcommand`), the `serial_runtime` / `adb_runtime`
wrappers, the direct `dispatch::dispatch` call in `main.rs`,
`dispatch_stdout_to_file`, `run_on_host`, and `cmd_daemons_*`. A test should
make a missed site fail, not a reviewer's memory.

The check:

1. Reads `lease.json` for the target under a short file lock, and drops
   `procs` entries whose process is gone (pid not alive, or alive with a
   different start time).
2. If the lease is **expired** (past `expires_at`, or idle: `last_used_at +
   idle` in the past **and** no live registered process): for a mutating
   command, `acquire`, `release` or `status`, runs the cleanup below, records
   `expired` in the history and deletes the lease. A **read** treats an
   expired lease as unleased and cleans nothing, so an observer's `video
   shot` never stops the daemon it is reading from.
3. If **unleased**: a mutating command is refused only when the target has
   `lease_required = true`; otherwise everything runs.
4. If **leased** and the caller's `PANIO_LEASE` matches: update
   `last_used_at`, register the process (below), run.
5. If **leased** and the token is missing or wrong: reads run; mutating
   commands are refused with the holder's owner label and expiry. If the
   caller's token matches a *previous* holder in the history, the message
   says it was preempted, by whom and when.

**Cross-host targets are out of scope for v1.** `lease acquire` refuses a
target whose channels resolve to more than one host, naming the channels and
hosts. `lease_required = true` on such a target is a lab validation error, so
it cannot silently go unenforced.

`lease_required` must survive into the shipped slice (`dispatch::build_slice`)
so the control host sees it.

**`daemons stop|restart`** load no lab and run on the local host only. They
read `lease.json` by the daemon's target name: a named daemon of a target
leased to someone else is refused; `--all` skips those daemons and says
which it skipped. (`lease_required` is not visible there and is not
enforced for `daemons`. My call.)

### Which commands are mutating

Lab-editing commands (`set`, `add`, `rm`, `rename`, `config`, `host`,
`target`, `init`) edit the dev machine's lab file, never touch the target, and
are **not checked**. Host-wide commands with no target (`discover`, `setup`,
`helper`, `skill`, `mcp`, `* devices`, `netif vlan`) are not checked.

| Reads (always allowed) | Mutating (holder only on a leased target) |
|---|---|
| `serial log`, `evidence`, `wait`, `status`, `show` | `serial connect`, `watch`, `stop`, `send`, `transact`, `dtr`, `reset` |
| `video shot`, `read`, `show` | `video watch`, `stop`, `preview` |
| `netboot status`, `logs`, `tftp-root` | `netboot start`, `stop` |
| `netif status` | `netif mode`, `down-hard` |
| `power-state` | `power on`, `off`, `power-cycle` |
| `usb state` | `usb attach-host`, `attach-target` |
| `action list` | `action run` |
| `adb show`, `screencap` | `adb shell`, `run`, `input` |
| `doctor` | `hid send`, `serve`, `stop`; `console` |
| `daemons list` | `daemons stop`, `restart` naming a leased target's daemon |

`serial wait` and `video shot` stay reads even though they need a running
daemon: they never start one. `console` and `video preview` are mutating
because their dashboard offers KVM input. `doctor` is a read because it only
probes.

The classification lives in one function next to the command enum, with a
test that every runtime subcommand is classified (an exhaustive `match`, so a
new subcommand does not compile until someone decides).

### Process registration

A mutating command running under a lease, on the control host, adds `{pid,
start_time}` to `procs` when it starts. Nothing removes it on exit: the check
prunes dead entries (step 1), which also covers `serial connect` and `adb
shell`, which `exec_replace` themselves into `tio` / `adb`. Exec keeps the pid
and the start time, so the entry still identifies the `tio` or `adb` process
cleanup must kill.

Process identity is pid **plus start time**, so a recycled pid is never
killed. This is new code: `panio daemons stop` only checks that a pid is
alive and its command line contains a name (`pid_runs_one_of`), which would
match any other panio. Add `platform::process_start_time(pid)` (macOS
`proc_pidinfo`, Linux `/proc/<pid>/stat` field 22, Windows
`GetProcessTimes`) to `cli/src/platform.rs` only; the four daemon copies do
not need it.

Only processes on the control host are registered. On the dev machine, a
remote `serial connect` is an `ssh` client and a remote `console` holds local
tunnels; both end when their remote side is killed, so they need no entry.

### Cleanup

Runs on `release`, on `--preempt` (before the new holder's record is
written), and lazily on expiry, on the default host, in this order:

1. Terminate the recorded panio processes that are still the same process.
2. Stop the target's daemons: serialcap, hdmicap and hid for that target
   (per-target daemons), and the target's netboot session. Host-singleton
   daemons shared with other targets (zigplug, cambrionix) are left alone.
3. If the target has a netboot channel: `netif mode off` on its interface.

Power is deliberately not touched. Each step is best effort; a failure is
reported (and kept in the history line) but does not stop the next step or
keep the lease alive.

### Errors

One new error kind, `target_leased`, with its own exit code, for both "held by
someone else" and "lease required but none held". The message distinguishes
them, and the preempted case names the new owner. Per the error contract this
means updating `Kind`, `Kind::ALL`, `EXIT_STATUS_HELP`, `from_exit_code` (so a
refusal on the control host is classified correctly on the dev machine),
`docs/errors.md` and `skills/panio/SKILL.md`. An agent branching on the code can then `lease
acquire --wait` and retry.

`acquire` on a held target without `--wait` or `--preempt` exits with the same
`target_leased` code.

## Documentation

- New `docs/leases.md` (in the `mkdocs.yml` nav): the shared-lab workflow,
  the parallel-agent workflow, the command reference, the read/mutating table.
- `docs/distributed-control.md`: remove locking from "What's deferred", link
  the new doc.
- `skills/panio/SKILL.md`: acquire before multi-step work, export
  `PANIO_LEASE`, hand the token to sub-agents, release when done; the
  `target_leased` code and what to do about it.
- `README.md` capabilities table, root `AGENTS.md` (capabilities line, runtime
  paths table: `lease.json`), `cli/AGENTS.md` module map.
- `notes/mcp-server.md` and the MCP tool descriptions: the lease tools.
- `evals/scenarios/`: at least one scenario that acquires, works, and
  releases.

## Left to the builder

- Module layout (`cli/src/lease.rs` is the obvious home) and how the check is
  threaded through the handlers that call `maybe_dispatch`.
- File-locking mechanism around `lease.json` on Unix and Windows (it must
  work through `platform.rs`, not `libc` at the call site).
- Random token source without a new dependency (`/dev/urandom` on Unix; a
  Win32 call via the existing `windows` dependency).
- Exact `lease status` output and `--json` shape.
- Duration parser (none exists today).
- The exit code number for `target_leased`, chosen per `docs/errors.md`'s bands.

## As built

Deviations from the design above, all deliberate. User guide: `docs/leases.md`.

- **A token whose lease has ended is not silent.** A token whose lease was
  released, force-released, preempted or found expired refuses mutating
  commands with exit 23 (the message says how and when it ended, and who holds
  the target now, if anyone) and warns on reads. A token that matches nothing
  on that target warns and runs. A read carrying a token that is not the
  current holder's also warns (naming the new holder) and runs. ("Silent
  surprises are bad.")
- **`release` and `renew` after a lease ended.** `release` with a token that
  names an ended lease exits 0 with the history message on stderr (a new
  holder is left alone); with no token on an unleased target it exits 0
  (`not leased`). `renew` on an expired lease retires it (expired history) and
  exits 23 with the history wording.
- **The dashboard does not refresh the idle timer.** A remote `console` runs a
  keepalive while it runs and the caller holds the lease: every
  min(idle/2, 5 min) a holder read (`video show`) on the control host; a
  `warning:` on its stderr or a non-zero exit means the lease is lost: the
  console prints the reason once and exits 23. A local `console` with a token
  that holds the lease stays in the foreground, is not registered in
  `lease.procs` (`enforce_with(.., register = false)`; it is still checked as
  mutating), polls the lease state every 5 s (`lease::keepalive_tick`, reading
  it directly), touches it at the keepalive interval, and on loss prints the
  reason and exits 23 within about 5 s. `video preview`
  returns as soon as the page is handed off; with a token it prints a note to
  renew. The Windows job also sets SILENT_BREAKAWAY_OK, so a server the tool
  launches is not killed; the tool is assigned right after spawn (not by
  putting panio in the job first, which breakaway would defeat). `lease status --json` gained `idle_secs` for the pacing.
- **Windows `serial connect`/`adb shell`** run their tool in a kill-on-close
  Job object, so terminating the registered panio pid kills the tool.
- **`lease status`** shows `kept alive by N process(es)` instead of an idle
  countdown while a registered process is alive; `idle_left_secs` is `null`
  then. It is not read-only (it retires an expired lease), and the MCP tool is
  annotated so. MCP `lease_acquire` caps `wait` at 5 minutes.
- **`lease_required` needs every channel on the target's default host.** A
  cross-host target cannot be leased, and the lab fails validation if it sets
  `lease_required`. `target set --lease-required` is the editing verb.
- **One central check.** `lease_scope` classifies every command (an exhaustive
  match, so a new subcommand must be classified) and `lease_check` runs once
  in `run`, before the handler, on the host where the target's default host is
  local (the control host after dispatch). It replaces the per-call-site hooks
  the design listed.
- **`daemons stop|restart`** refuse a named daemon of a target leased to
  someone else (exit 23): the capture daemons, hid, and a target's netboot
  session. `stop --all` and `restart --all|--stale` skip them with one stderr
  line each (`--stale` is restart-only). All checks run before anything is
  stopped. The `stop --all` stray sweep skips untracked orphans whose device
  belongs to a leased target (matched through the lab; no lab, a warning).
- **Cleanup** stops the target's serialcap, hdmicap and hid daemons (reaping
  orphans and waiting for exit), its netboot session, and takes the netboot
  link down; it never touches power. It runs under the lease lock, so it calls
  library functions, never the lease check.
- **`--lease-file`** is a hidden flag used only by interactive dispatch to
  carry the token in a 0600 file instead of the environment.
- The history record also keeps the previous token (so an ended token can be
  explained), and the lease record keeps `duration_secs` (so `renew` can default
  to it).
