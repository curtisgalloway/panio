<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Target leases

**Terms:**

- **Target**: one board under test, a `[targets.<name>]` block in the lab
  file.
- **Lease**: a time-limited reservation of a whole target by one holder.
- **Holder**: whoever acquired the lease and has its token: one person, one
  agent, or a group of agents the token was handed to.
- **Token**: the random secret `panio lease acquire` prints. A command proves
  it is the holder by carrying the token in the `PANIO_LEASE` environment
  variable.
- **Default host**: the target's `host` field, which every channel of the
  target uses unless it names its own.
- **Control host**: the machine cabled to the target. The lease record lives
  on the target's default host, which is the control host for a remote target.

See the [glossary](glossary.md) for the rest.

A lease keeps two people, or two agents, from driving one board at the same
time. One holder has the board; everyone else can still watch it but cannot
change it. Leases end on their own, and ending one cleans the board up for
the next holder.

A lab that never runs `panio lease` behaves exactly as before.

## When to use one

- **A shared lab of scarce boards.** Several people share a set of boards.
  Someone books a board for a limited time and nobody else drives it until the
  booking ends or is released.
- **Parallel agents on one lab.** Several agents run at once. Without a lease
  a second agent can power-cycle a board in the middle of the first one's
  flash or install. With a lease, each agent's workflow has exclusive control
  of its board, and a second agent is told who holds it and until when.

## Shared-lab workflow

```bash
export PANIO_LEASE=$(panio lease acquire pi5 --duration 4h --owner alice)
panio netboot start pi5
panio power-cycle pi5
panio serial log pi5
panio lease release pi5
```

`acquire` prints only the token on stdout, so the `export` line captures it.
Every later command in that shell carries the token. If someone else holds the
board, `acquire` exits 23 and says who and until when. To wait for it:

```bash
export PANIO_LEASE=$(panio lease acquire pi5 --wait 30m)
```

`--wait` retries every 2 seconds until the board is free or the time runs
out. To see what is going on first, `panio lease status` (no target) lists
every target's lease.

## Parallel-agents workflow

An orchestrating agent leases the board once and hands the token to the
sub-agents that should share it:

```bash
export PANIO_LEASE=$(panio lease acquire pi5)
```

Sub-agents started from that shell inherit `PANIO_LEASE`. For an agent that
does not inherit the environment, pass the token in its instructions and have
it `export PANIO_LEASE=<token>`. Everyone holding the token is the same
holder, so two sub-agents with the token can still step on each other: give
each its own board, or its own turn.

Two agents on the same machine and the same Unix user are different holders
as long as they carry different tokens.

## MCP

`panio mcp` has four lease tools: `lease_acquire`, `lease_renew`,
`lease_release` and `lease_status`. One MCP server is one holder. The server
keeps the token from `lease_acquire` in memory (the tool result never shows
it) and sets `PANIO_LEASE` on every command it runs for that target. A
`PANIO_LEASE` already in the server's own environment when it starts is used
as the token for every target, so a harness can hand a server an existing
lease. `lease_acquire` takes `owner`, `duration`, `idle`, `wait` and
`preempt`; `lease_release` takes `preempt`; `lease_status` with no target
lists them all. See [MCP server](mcp.md).

## Command reference

All lease commands take the target positionally or as `-t/--target`, and may
omit it when the lab has exactly one target. Durations are written `90s`,
`30m` or `2h`, and at most `24h`.

| Command | What it does |
|---|---|
| `panio lease acquire [TARGET]` | Reserve the target and print its token alone on stdout. Exits 23 when someone else holds it. `--owner LABEL` sets the display name (default `user@host` of the machine you run it on). `--duration DUR` is the total length (default `2h`). `--idle DUR` ends the lease after this long with no command and no live registered process (default `30m`). `--wait DUR` polls every 2 s for that long. `--preempt` takes the target from its holder. `--json` prints `{token, owner, expires_at, expires_in_secs}`. If `PANIO_LEASE` already matches, `acquire` renews and prints the same token. |
| `panio lease renew [TARGET]` | Move the end to now plus `--duration` (default: the lease's original duration), capped at 24 h from now. Needs `PANIO_LEASE`. If the lease had already expired, `renew` retires it (cleaning up, as any expiry does) and exits 23 saying so. |
| `panio lease release [TARGET]` | End your lease and clean the target up. Needs `PANIO_LEASE`, or `--preempt` to end someone else's. Releasing a lease that is already over is not an error, so an end-of-job `release` does not retry: with a token that names an ended lease it exits 0 and says on stderr how it ended (and leaves a new holder alone); on a target nobody holds, with no token, it exits 0 and says `'<target>' is not leased`. A token that never named a lease on the target, or no token while someone else holds it, exits 23. |
| `panio lease status [TARGET]` | Who holds the target, until when, idle time left (or, while a registered process is alive, `kept alive by N process(es)`; `idle_left_secs` is `null` then), and registered processes. Never prints the token. With no target in a lab of several, lists them all; an unreachable host is reported in its row. `--json` for scripts. Not read-only: it cleans up a lease that has expired. |

### Preemption

Anyone can take a lease with `acquire --preempt` or `release --preempt`. The
holder's leftovers are cleaned up first, and the displaced holder learns what
happened on its next command (see [ended tokens](#a-token-whose-lease-has-ended)).
Preemption is for the case where the holder has gone away. It is not
access control.

### Timing

A lease ends at the first of:

- its total duration (default 2 h, at most 24 h);
- being idle for the idle timeout (default 30 min): no command by the holder
  and no live registered process. A mutating command by the holder registers
  its own process, so an open `serial connect` or `adb shell` keeps the lease from
  going idle; the total duration still applies. The dashboard is different:
  see [the dashboard and the idle timer](#the-dashboard-and-the-idle-timer);
- `lease release`, or a preemption.

Expiry is **lazy**: no background process watches the clock. The next
mutating command, `acquire`, `release` or `status` that touches the target
finds the expired lease, cleans up, and carries on. A read does not clean up,
so someone running `video shot` never stops the daemon it is reading from.

### Cleanup

Ending a lease, by any route, tears down what the holder left:

- the holder's registered panio processes;
- the target's serialcap, hdmicap and hid daemons, including orphans whose
  discovery file is gone, waiting for each to exit;
- the target's netboot session, and the netboot link is taken down
  (`netif mode off`).

Power is never touched: a board that was on stays on. A step that fails is
reported as a warning and recorded in the history; the remaining steps still
run.

## Reads and mutating commands

On a leased target, commands that only look are always allowed. Commands that
change the target are allowed only with the holder's token. Anyone else gets
exit 23 naming the holder and when the lease ends.

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

`serial wait` and `video shot` stay reads even though they need a running
daemon: they never start one. `console` and `video preview` are mutating
because the dashboard they serve carries keyboard and mouse input.

Not checked at all: commands that edit the lab file (`set`, `add`, `rm`,
`target`, `host`, `config`, `init`), host-wide commands with no target
(`discover`, `setup`, `configure`, `helper`, `skill`, `mcp`, `* devices`,
`netif vlan`), and the `lease` commands themselves.

### The dashboard and the idle timer

`console` and `video preview` hand you a web page. What you do in the page
(typing, mouse, watching) goes to the daemons directly and never runs a panio
command, so it does not count as activity.

- `console` keeps the lease alive itself while it runs, as long as you carry
  the holder's token. It stays in the foreground (a local one prints "Press
  Ctrl-C to close", as a remote one does): every half idle timeout, at most
  every 5 minutes, it counts as holder activity (a remote console makes a read
  of the target on the control host). A console is not registered as a process
  under the lease, so ending the lease never kills it silently. If the lease
  is ever no longer yours (released, preempted, expired), the console prints
  why on stderr and exits with 23 (`target_leased`): a local one within about
  5 seconds, a remote one at its next keep-alive tick. It returns normally, so
  its ssh tunnels are closed. For a released or preempted lease the cleanup has
  already stopped the target's daemons; for one that expired the message says
  the next lease check will stop them. Only the lease check's own answer ends
  a remote console: a network failure on a keep-alive tick is a warning and is
  retried (three in a row warn that the lease may idle out), and a local console
  that cannot read its lease record gives up after three tries with exit 109,
  not 23. If your token is not the
  holder's when the console starts, it says the lease will not be kept alive
  (a local console then returns as before). Without a token, a local `console`
  returns as soon as the page is handed off, as always.
- `video preview` cannot do that: the command returns as soon as the page is
  handed off. With a token set it prints a one-line note on stderr. Run
  `panio lease renew` now and then, or keep any panio command going
  (`serial connect`, a `serial log` loop), while you use the page.

### Daemons

`panio daemons stop` and `restart` look at the daemon's target: the capture
daemons, hid, and a target's netboot session (`netbootd[pi5]`). Naming one that
belongs to a target leased to someone else (`'serialcap[pi5]'`) is refused with
exit 23. `stop --all` and `restart --all|--stale` skip those with one line on
stderr for each, and stop the rest. Every lease check runs before anything is
stopped, so a refusal leaves the host exactly as it was. The `stop --all`
sweep of untracked orphan daemons skips an orphan whose device belongs to a
leased target the same way; with no readable lab it cannot tell and says so.

## Requiring a lease

By default an unleased target runs freely. To make a target refuse mutating
commands unless the caller holds a lease:

```bash
panio target set pi5 --lease-required true
```

`--lease-required false` removes the setting. With it on, a mutating command
on an unleased target exits 23 and says to run `panio lease acquire`; reads
still work. It needs every channel of the target on the target's default host
(see [limits](#limits)); the setting is refused otherwise.

## A token whose lease has ended

If your lease is released, force-released, preempted, or found expired, your
token no longer works. panio does not run quietly as if nothing happened:

- A **mutating** command refuses with exit 23. The message says how and when
  the lease ended (for example "preempted by bob"), and who holds the target
  now, if anyone.
- A **read** runs and prints the same message as a warning on stderr. That
  includes a read on a target someone else holds now (for example after you
  were preempted): it still runs, with a `warning:` naming the new holder.
- A token that matches nothing on that target (it never named a lease there)
  warns that the target is not leased and runs. A holder legitimately keeps
  its token while it touches a different, unleased target.

The record of why a token stopped working is kept in `lease-history.jsonl`
(the last 50 entries).

## Where it lives

On the target's default host, under `~/.local/share/panio/<target>/`:
`lease.json` (the record, owner-only), `lease-history.jsonl`, and `lease.lock`
(held briefly while the record is read or changed). In the lab file the only
setting is `lease_required`.

`PANIO_LEASE` is forwarded to the control host on stdin like `AMT_PASSWORD`,
so it never appears on a remote command line. For interactive commands
(`serial connect`, `adb shell`) stdin is your terminal, so the token
travels in a short-lived owner-only file named by a hidden `--lease-file`
option, which the remote panio reads and deletes. You do not use that option
yourself.

The check runs once, before the command, on the host where the target's
default host is local: the control host after dispatch, or this machine when
every channel and the default host are local.

## Limits

- **Single-host targets only.** A target whose channels sit on more than one
  host cannot be leased; `acquire` names the channels and hosts. Cross-host
  leasing is deferred (see [distributed control](distributed-control.md#whats-deferred)).
- **Not a security boundary.** A lease prevents accidents between cooperating
  users and agents. Anyone can preempt, and anyone who can read the control
  host's lease file can read the token. A process that talks to a daemon's
  HTTP API directly bypasses panio and the lease.
- **Windows.** Cleanup ends a registered process by pid. The Windows
  `serial connect` and `adb shell` run their tool as a child of panio inside a
  job object that kills it when panio ends, so the cleanup also frees the port.
  Processes the tool itself starts (adb's background server) leave the job and
  are not killed.
- **Not a calendar.** There are no future bookings and no queue; waiting is
  `--wait`, which polls.
- **Whole target.** There is no per-channel lease. Agents that want to share
  one board share one token.
