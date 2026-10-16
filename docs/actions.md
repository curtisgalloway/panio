<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Named target actions

`paniolo action` runs a target's physical controls that are not power: a
cold or warm reset line, a boot-source strap, a button combination that
enters recovery or a USB download mode. Each action is a shell command you
write into the lab file, plus a name and a one-line description, so an agent
can discover what a target supports and run it by name.

**Terms** (more in the [glossary](glossary.md)):

- **action** — one named hook command in a target's `actions` table.
- **hook** — a shell command stored in the lab file that paniolo runs for
  you, the way the power commands run `on_cmd` and friends.
- **helper** — a device-specific program paniolo ships or you install (for
  example `cambrionix`); a hook usually calls one.
- **strap** — a pin or setting a board reads at power-on to decide how to
  boot (from SD, USB, recovery, …).

paniolo defines no meaning for any action name. It finds the command, runs it
on the right control host, and reports the result, so device knowledge stays
in the helper, as it does for power.

---

## Defining actions

```bash
paniolo action set -t target-machine reset \
    --cmd "boardctl -d /dev/serial/by-id/usb-board-ctl reset" \
    --description "Cold reset the SoC"
paniolo action set -t target-machine boot-usb \
    --cmd "boardctl -d /dev/serial/by-id/usb-board-ctl boot usb" \
    --description "Strap boot source to USB" --requires-power off
paniolo action set -t target-machine --host bench1 recovery \
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
- `--requires-power on|off` makes paniolo check `power-state` first and refuse
  the action in the other state. Use it for strap changes, which must not hit a
  running board. `--requires-power none` clears it.
- Names follow the usual rule (letters, digits, `.`, `_`, `-`, no leading
  `-`). `host` is reserved, since it is the table's host key.

`paniolo action rm -t target-machine reset` removes one; the table goes with
the last one.

### Conventional names

Any name works, but these let an agent (and the bundled `paniolo` skill)
recognize a common control without reading the description:

| Name | Meaning |
|---|---|
| `reset` | Cold reset; prefer it over a power cycle when the target has one |
| `warm-reset` | Warm reset, preserving what a warm reset preserves on that board |
| `recovery` | Reboot into the board's recovery or download mode |

## Listing and running

```bash
paniolo action list -t target-machine          # name, description, requires_power
paniolo action list --json                     # every target, machine-readable
paniolo action run -t target-machine reset
```

`action run` takes the target only as `-t` (or none, when the lab has one
target), because its positional argument is the action name. It takes no
extra arguments: every variant is its own named action, so the lab file is
the complete list of what can be run.

## How an action runs

The contract is the [power hooks'](power.md) one:

- `sh -c` on Unix (`cmd.exe` on Windows), with paniolo's helper dir first on
  `PATH`, so a hook names helpers bare.
- `PANIOLO_STATE_DIR` and `PANIOLO_RUNTIME_DIR` are set for the hook's program,
  as for a power hook.
- A non-zero exit is `helper_failed` (exit 101) with the hook's own code in
  the error; see [Exit status and errors](errors.md).
- When the actions live on another control host, paniolo re-runs the command
  there over SSH.

**One hook at a time per target.** Action hooks and power hooks for a target
share one lock, because they often drive the same exclusive device (a
board-control microcontroller's serial port). A second hook waits up to 120 s
for the first to finish, then fails with `timeout` (exit 22). A hook that calls
back into paniolo for the same target (an action script running
`paniolo power-state`) does not wait on its own lock: paniolo sets
`PANIOLO_HOOK_LOCK` to the target name for the hook it runs.

**`requires_power`.** paniolo reads the power state wherever the power channel
is, before taking the lock (reading the state takes that same lock). A refusal
is `usage` (exit 2), since only the caller can fix it:

```text
action 'boot-usb' requires power off, but 'target-machine' is on (paniolo power off target-machine, then retry)
```

A target whose action sets `requires_power` needs a power channel that can
report state (`state_cmd`, or a serial sense line).

## Diagnostics and agents

- `paniolo doctor` checks that each action's program exists on the actions'
  host, as it does for power hooks.
- `paniolo helper -t target-machine --channel actions -- boardctl …` runs the
  helper by hand on the actions' host.
- The MCP server offers `action_list` and `action_run`; see [MCP](mcp.md).

The dashboard does not show action buttons yet.
