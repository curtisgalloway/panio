<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Exit status and errors

panio's exit status names the *kind* of failure (`not_configured`,
`daemon_down`, …), each kind belongs to a coarser *category*
(`infrastructure`, `transient`, `device_state`), and `--json-errors` adds the
details as one JSON line. A
*hook* is a lab-file command (`cycle_cmd`, a power `on_cmd`, a USB or HID
`cmd`); a *helper* is a program panio runs for a channel (`serialcap`,
`hdmicap`, `shellyplug`, `netbootd`). A *passthrough* hands back another
program's status ([below](#passthrough-commands)).

## Exit codes

| Code | Kind | Category | Meaning |
|---|---|---|---|
| 0 | — | — | Success. |
| 1 | — | — | The command ran and the answer is "no": `doctor` found problems. No other panio command exits 1 (a [passthrough](#passthrough-commands) may, for its program), and **no error exits 1**. |
| 2 | `usage` | null | Bad flags or arguments. |
| 3 | `not_configured` | `infrastructure` | No lab file, an invalid one, or an unknown target, channel, interface or host; a required channel field missing (such as `tftp_root`); a hook or helper missing or not executable. Retrying will not help. |
| 4 | `unreachable` | `infrastructure` | SSH to the control host failed. The command may not have run, or may have been killed part way. |
| 22 | `timeout` | `transient` | No answer within a deadline (daemon startup, a daemon request, an SSH port forward). The outcome is unknown: check state before repeating a change such as a power cycle. |
| 23 | `target_leased` | `transient` | The target is leased to someone else, or the lab requires a lease and none is held. The message names the holder and when the lease ends. Wait (`panio lease acquire --wait`) and retry. |
| 100 | `daemon_down` | `infrastructure` | The channel's daemon is not running and the command needs it, or its record is stale. Start it with `panio serial watch` or `panio video watch`; `panio daemons` lists what is running. |
| 101 | `helper_failed` | `device_state` | A hook or helper ran and failed: it exited non-zero (its code is in `child_exit`), a daemon exited during startup, or a running daemon refused the request (its reason is the message). |
| 109 | `internal` | null | An unclassified failure; likely a bug. The message says what went wrong. |

**Hooks on Windows.** `cmd.exe` exits 1 for a missing command or script, so a
missing hook reports 101 `helper_failed` with `child_exit` 1, not 3; the
message carries `cmd.exe`'s "is not recognized" or "cannot find the path" text.

## Categories

The `category` field of the [JSON object](#the-json-error-object) says where a
workflow should look, for a caller that does not need the full kind:

| Category | Kinds | Meaning |
|---|---|---|
| `infrastructure` | `not_configured`, `unreachable`, `daemon_down` | panio's own setup failed: the lab file, a control host, a daemon. The target was not reached. Fix the setup; retrying will not help. |
| `transient` | `timeout`, `target_leased` | A retry may succeed. After a `timeout` the first attempt's outcome is unknown: check state before repeating a change. A `target_leased` refusal ran nothing; retry once the lease ends. |
| `device_state` | `helper_failed` | The target or the hardware panio drives refused or failed: a hook or helper reported failure, a daemon rejected the request (such as "no video signal"). |
| null | `usage`, `internal` | A caller's mistake or an unclassified failure; neither points at the setup or the target. |

The category is derived from the kind, so exit codes do not change with it. A
control host older than 0.13 sends an object without the field.

Bands: 2–9 will fail the same way again; 20s may succeed on retry; 100 and up
are panio-specific. Nothing exits 125 or above except a
[passthrough](#passthrough-commands).

**A reader that stops early is not a failure.** When whatever reads
panio's stdout closes it first (`panio skill | head -1`, `| grep -q`),
panio stops writing and exits 0 without a message. Before 0.9.1 it panicked
and exited 101, which read as `helper_failed`.

A command re-run on a control host exits with the **remote** code (a missing
channel on `bench1` is 3 locally too); only an SSH failure is reported locally,
as 4.

`serial wait` is an observation command: a valid result exits 0 and reports
`matched`, `timeout`, `cap`, disconnect, epoch or capture failure in its JSON
`status`. These are acquisition outcomes, not target-task verdicts or the error
kinds above. Accepted raw evidence survives an incomplete acquisition; see
[bounded serial wait](serial.md#bounded-fresh-wait-and-follow).

## The JSON error object

Set `PANIO_JSON_ERRORS=1`, or put `--json-errors` **before** any trailing
arguments: `panio --json-errors hid send …`. Prefer the variable in scripts:
`hid send`, `adb run`, `adb input` and `helper` pass a trailing `--json-errors`
on as an argument.

On failure, the **last line of stderr** is exactly one JSON object:

```console
$ panio --json-errors power-cycle nosuch
target 'nosuch' not found in lab
{"error":{"category":"infrastructure","channel":null,"child_exit":null,"code":3,"host":null,"kind":"not_configured","message":"target 'nosuch' not found in lab","target":"nosuch"}}
$ echo $?
3
```

| Field | Value |
|---|---|
| `kind` | One of the kinds in the table above. |
| `code` | The exit status, repeated. |
| `category` | The kind's [category](#categories): `infrastructure`, `transient`, `device_state`, or null. |
| `message` | The text printed above it; for `usage`, only the first sentence, without its `error: ` prefix. |
| `target` | The target name, when the failure is about one. |
| `channel` | The channel kind (`power`, `serial`, `video`, `hid`, `usb`, `netboot`, …). |
| `host` | The control host, for `unreachable`. |
| `child_exit` | The hook's or helper's own exit code, for `helper_failed`; null when a signal killed it. |

Every field is always present (`null` when it does not apply); key order is
not fixed. Stdout is unchanged, so `--json` output (`discover`,
`serial log --json`) stays parseable. Parse only the last line of stderr. The
variable is not passed to hooks, helpers or daemons.

**Version skew.** A control host on panio 0.4.x or older exits 1 for every
error and rejects `--json-errors` with exit 2
(`unexpected argument '--json-errors'`) without running the command. Upgrade
it (`apt upgrade`, then deliberately restart the affected capture daemon by
name, such as `panio daemons restart serialcap`). `--stale` selects only
proven content differences and skips unknown binary identity.

## Helper exit statuses

`panio hid send` reports any non-zero exit of the `hid` helper as 101
`helper_failed`, with the helper's own status in `child_exit`. `ch9329` uses
two statuses beyond 1 so a caller can tell what a timed-out send left behind:

| `child_exit` | Meaning |
|---|---|
| 40 | The send timed out; the daemon cancelled the command and confirmed it stopped. Held keys the command pressed are released. |
| 41 | The send timed out and the daemon could not be shown to have stopped. The target may still be receiving input. |

See [hid.md](hid.md).

## Passthrough commands

These exit with **the program's** status, like `ssh host cmd`:

- `panio helper <name> …`
- `panio config edit` (your `$EDITOR`)
- `panio video shot`, `panio video devices` (the `hdmicap` helper)
- `panio adb run`, `panio adb input`, `panio adb devices`
- `panio serial connect` (tio) and `panio adb shell`, the interactive
  consoles
- `panio setup --host <host>` (the remote setup)

A signal gives 128+N. Failures *before* the program starts (unknown target,
unreachable host) use the table above.

## A stopped serial daemon

With `serialcap` stopped, `panio serial log` still reads the file but shows
nothing new; it prints a `warning:` on stderr and exits 0. Pass
`--require-live` to fail with 100 (`daemon_down`) instead.
