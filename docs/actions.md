<!--
SPDX-FileCopyrightText: 2026 The Panio Authors
SPDX-License-Identifier: Apache-2.0
-->

# Named target actions

`panio action` runs a target's physical controls that are not power: a
cold or warm reset line, a boot-source strap, a button combination that
enters recovery or a USB download mode. Each action is a shell command you
write into the lab file, plus a name and a one-line description, so an agent
can discover what a target supports and run it by name.

**Terms** (more in the [glossary](glossary.md)):

- **action** — one named hook command in a target's `actions` table.
- **hook** — a shell command stored in the lab file that panio runs for
  you, the way the power commands run `on_cmd` and friends.
- **helper** — a device-specific program panio ships or you install (for
  example `cambrionix`); a hook usually calls one.
- **strap** — a pin or setting a board reads at power-on to decide how to
  boot (from SD, USB, recovery, …).

panio defines no meaning for any action name. It finds the command, runs it
on the right control host, and reports the result, so device knowledge stays
in the helper, as it does for power.

---

## Defining actions

```bash
panio action set -t target-machine reset \
    --cmd "boardctl -d /dev/serial/by-id/usb-board-ctl reset" \
    --description "Cold reset the SoC"
panio action set -t target-machine boot-usb \
    --cmd "boardctl -d /dev/serial/by-id/usb-board-ctl boot usb" \
    --description "Strap boot source to USB" --requires-power off
panio action set -t target-machine --host bench1 recovery \
    --cmd "boardctl -d /dev/serial/by-id/usb-board-ctl mode recovery"
```

That writes:

```toml
[targets.target-machine.actions]
host = "bench1"

[targets.target-machine.actions.reset]
cmd = "boardctl -d /dev/serial/by-id/usb-board-ctl reset"
description = "Cold reset the SoC"

[targets.target-machine.actions.boot-usb]
cmd = "boardctl -d /dev/serial/by-id/usb-board-ctl boot usb"
description = "Strap boot source to USB"
requires_power = "off"
```

Hand-written inline tables work too
(`reset = { cmd = "…", description = "…" }`), and `action set` edits them in
place.

- `--cmd` is required for a new action and optional when updating one.
- `--host` binds **all** of the target's actions to one control host, like any
  channel's `host`. Unset, they run on the target's default host.
- `--requires-power on|off` makes panio check the default power state under
  the target's hook lock and refuse the action in the other state. Use it for
  strap changes that need the board powered off. `--requires-power none` clears it.
- Names follow the usual rule (letters, digits, `.`, `_`, `-`, no leading
  `-`). `host` is reserved, since it is the table's host key.

`panio action rm -t target-machine reset` removes one; the table goes with
the last one.

### Conventional names

Any name works, but these let an agent (and the bundled `panio` skill)
recognize a common control without reading the description:

| Name | Meaning |
|---|---|
| `reset` | Cold reset; prefer it over a power cycle when the target has one |
| `warm-reset` | Warm reset, preserving what a warm reset preserves on that board |
| `recovery` | Reboot into the board's recovery or download mode |

## Listing and running

```bash
panio action list -t target-machine          # name, description, requires_power
panio action list --json                     # every target, machine-readable
panio action run -t target-machine reset
```

`action run` takes the target only as `-t` (or none, when the lab has one
target), because its positional argument is the action name. It takes no
extra arguments: every variant is its own named action, so the lab file is
the complete list of what can be run.

## How an action runs

The contract is the [power hooks'](power.md) one:

- `sh -c` on Unix (`cmd.exe` on Windows), with panio's helper dir first on
  `PATH`, so a hook names helpers bare.
- `PANIO_STATE_DIR` and `PANIO_RUNTIME_DIR` are set for the hook's program,
  as for a power hook.
- A non-zero exit is `helper_failed` (exit 101) with the hook's own code in
  the error; see [Exit status and errors](errors.md).
- When the actions live on another control host, panio re-runs the command
  there over SSH.

**One hook at a time per target.** On a control host, action hooks, power
hooks, and `serial dtr` / `serial reset` for a target share one lock. A second
command waits up to 120 s for the first to finish, then fails with
`timeout` (exit 22). A hook can call panio synchronously for the same target,
for example to read `panio power-state`, using the lock the parent command
already holds. Keep any nested calls in the foreground and wait for them
before the hook exits.

**`requires_power`.** When the action and power controls share a host, panio
takes the lock before reading the default power state and holds it until
the action hook finishes. A wrong power state is `usage` (exit 2):

```text
action 'boot-usb' requires power off, but 'target-machine' is on (panio power off target-machine, then retry)
```

A guarded action needs a power channel that can report state using
`state_cmd` or a serial sense line. For serial sense, the configured interface
must be on the power host. The guard coordinates panio power commands.
Physical buttons and other software can change power independently.

### Actions with power controls on another host

Run the action from the machine with the full lab configuration. Panio
reserves the power host and each host with a serial power button before it
checks the default power state. It records the operation on the action host
before starting the hook. A competing power command on a reserved host fails
with the operation ID. Use current Panio binaries on every host that can
change the target's power. All Panio clients on a control host must use the
same OS account, data directory, and runtime directory for that target.

### Recovering an interrupted action

From the full lab configuration, run `panio action guard-status -t target-machine`.
It prints the operation ID, phase, and power hosts as JSON, or `null` when
the action host has no outstanding operation. Keep the original host
bindings and target name until recovery finishes.

- For `ready` or `finished`, run `panio action recover -t target-machine --id ID`
  with the ID from the status output. This prevents a late execution request
  from starting the action and releases its reservations.
- For `running`, first check the action host and verify that the hook and
  any subprocess it started have stopped touching the target. Then run
  `panio action recover -t target-machine --id ID --confirm-stopped`.
  Verify the hardware state before deciding whether to run the action again.
- If a participant is offline, reconnect it and repeat recovery with the
  same ID. Reservations survive process restarts and have no timeout.
  Keep the state files intact. A power-off requested through Panio also waits for
  recovery while a reservation is outstanding.

An interrupted ordinary power command can also leave a hook or serial pulse
in progress. To inspect it, run `panio action guard-status -t target-machine
--power-host HOST` with the power or serial host's name from the lab. Once
you have verified that its hook, children, and hardware transition have all
stopped, use `panio action recover -t target-machine --power-host HOST --id ID
--confirm-stopped`. When operating directly on that host, use `local`.

## Diagnostics and agents

- `panio doctor` checks that each action's program exists on the actions'
  host, as it does for power hooks. `panio action set -t target-machine
  <name> --doctor` (which, like `--host`, applies to all of the target's
  actions) also runs each action program's read-only `doctor --json` check,
  once per distinct program and transport options. Only opt in programs that
  implement it; see
  [Helper health checks](power.md#helper-health-checks-doctor). `doctor` is
  therefore reserved, like `host`, and cannot name an action.
- `panio helper -t target-machine --channel actions -- boardctl …` runs the
  helper by hand on the actions' host.
- The MCP server offers `action_list` and `action_run`; see [MCP](mcp.md).

The dashboard does not show action buttons yet.
