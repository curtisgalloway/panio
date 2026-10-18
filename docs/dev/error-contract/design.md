<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# CLI error contract — Design

Revision: 2026-09-24, draft for approval (read against `a830058`).

**Terms.**

- **Error contract:** the documented promise that the *kind* of a failure can be
  read from the exit status (and, when asked for, from a JSON object) without
  parsing any English text.
- **Consumer:** a program that drives panio and branches on its failures.
- **Dispatch:** panio re-running a subcommand on a target's control host over
  SSH (`cli/src/dispatch.rs`).
- **Passthrough:** a command whose job *is* to run another program and report
  its status (`panio ssh`, `adb` passthrough, `video` passthrough,
  `config edit`).
- **Hook:** a user-configured script panio runs for a channel (`cycle_cmd`,
  `state_cmd`, a USB or HID `cmd`).
- **Exit-code bands:** the house convention that the tens digit of an exit
  code classifies it (2–9 deterministic, 20s transient, 100–124 tool-specific,
  nothing at 125 or above).

## Goal and non-goals

A consumer (first: `bringup-kit`'s panio adapter) must tell "not configured"
from "control host unreachable" from "daemon not running" from "the hook
script failed" without matching message text. The user rejected text matching
on 2026-09-24.

Non-goals: changing any command's success output; rewording error messages;
changing what a *passthrough* reports for its child (see D3); a `--json` for
success output where one does not already exist.

## Current behavior (observed)

- `main()` prints `{e:#}` and exits **1** for every error
  (`cli/src/main.rs:888`). About 140 `anyhow!`/`bail!` sites collapse to 1.
- Clap usage errors exit **2**.
- `model::LabError` (`cli/src/model.rs:75`) is a typed error for lab-file
  problems; `dispatch::maybe_dispatch` already raises it for an unknown target
  (`dispatch.rs:335`).
- **About 38 `std::process::exit` sites**, not 15. They fall into four groups:
  - *dispatch results* (`exit(code)` after `maybe_dispatch`, ~15 sites): the
    remote panio's code, or ssh's own **255** on a transport failure;
  - *hook results* (`run_power_hook` `main.rs:2835`, USB `main.rs:4025`,
    HID `main.rs:4185`, others): the script's own code, so a relay script
    exiting 2 reads as "usage error";
  - *passthroughs* (`ssh::run_passthrough`, `adb::run_passthrough`,
    `video::passthrough`, `config edit`'s `$EDITOR`);
  - *negative answers* (`doctor` found problems, `main.rs:1752`).
- `ssh::run_passthrough` returns `status.code().unwrap_or(-1)`: a child killed
  by a signal becomes `exit(-1)`, which the OS reports as **255** — the same
  code as an ssh transport failure.
- Dispatch rebuilds the remote argv from this process's argv minus `--lab`
  (`dispatch::subcommand_args`), so **a global flag placed anywhere is
  forwarded**. Environment variables cross only if named in
  `ssh::FORWARDED_ENV` (today just `AMT_PASSWORD`), sent over stdin.
- Five subcommands already have their own `--json` flag (e.g. `discover`), so
  a *global* flag named `--json` would collide with them in clap.
- **The "silent power-cycle" failure did not reproduce locally.** With 0.4.1
  and an eval fixture lab, `panio power-cycle nosuch` prints
  `target 'nosuch' not found in lab` and exits 1. The consumer saw no output
  on a control host, so the silent path is somewhere in that setup (likely a
  dispatch or hook path). Finding it is milestone work (R6).
- A stopped serialcap still serves `serial log` from disk with rc 0. `serial
  show` and `serial send` notice. Consumers anchor reads on `log`.

## Requirements and acceptance criteria

- **R1 — exit codes by kind.** Every panio-originated failure exits with the
  code of its kind from the table below. Checked by one test per kind.
- **R2 — JSON error object.** When asked for (D1), panio writes exactly one
  JSON error object as the **last line of stderr**, with the fields below.
  Stdout is unchanged. Checked by the same tests, parsing the object.
- **R3 — dispatch keeps the kind.** A remote failure arrives locally with the
  remote's code and JSON object, and no second local object. An ssh transport
  failure arrives as 4 (`unreachable`), not 255.
- **R4 — no leaked child codes.** No non-passthrough path exits with a child's
  status. A failing hook exits 101 with the script's code in `child_exit`.
  Nothing exits 125–255, including a signal-killed child.
- **R5 — documented.** `panio --help` has an exit-status section listing only
  codes actually emitted; `docs/` and `skills/panio/SKILL.md` describe the
  contract and the JSON object.
- **R6 — no silent failure.** `power-cycle <unknown-target>` prints an error and
  exits 3 in the setup where it was silent.
- **R7 — CI gates.** `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test` pass for `cli`.

## The code table (needs approval)

| Code | `kind` | When |
|---|---|---|
| 0 | — | success |
| 1 | — | completed with a negative answer only: `doctor` found problems (the only such command today). **No longer "any error".** |
| 2 | `usage` | bad flags or arguments (clap, unchanged) |
| 3 | `not_configured` | no lab file, lab invalid (`LabError`), unknown target, channel, interface or host; a hook or helper binary missing or not executable (child 126/127) |
| 4 | `unreachable` | control host not reachable: the lab slice could not be copied to it, or ssh exited 255 (which OpenSSH also does when the remote command is killed, so the outcome is unknown). panio itself never probes device nodes, so it does not emit 4 for one: a daemon that cannot open its device fails to start (22) or answers with an error (101) |
| 22 | `timeout` | no answer within a deadline — a daemon still starting when its deadline passed, a daemon request that timed out, an ssh port forward that never opened; outcome unknown, so check state before retrying a mutation |
| 100 | `daemon_down` | the channel's daemon (serialcap, hdmicap) is not running and the command needs it, or its discovery record is stale (connection refused) |
| 101 | `helper_failed` | a hook or helper ran and exited non-zero (its code is in `child_exit`), a daemon exited non-zero during startup (its code in `child_exit`, its last stderr in the message), or a running daemon refused a request with an error status (its reason is the message) |
| 109 | `internal` | any failure not yet classified (D2) |

No other `1xx` codes are proposed. Adding one later is compatible; changing a
code is not.

The JSON object (one line, last line of stderr):

```json
{"error": {"kind": "not_configured", "code": 3, "message": "target 'nosuch' not found in lab",
           "target": "nosuch", "channel": null, "host": null, "child_exit": null}}
```

`message` is the same text printed above it. `channel` is the channel kind
(`power`, `serial`, …); `host` is the control host name when known. Fields are
always present (null when unknown) so a consumer need not probe.

## Architecture

1. **`cli/src/error.rs` (proposed).** `enum Kind { Usage, NotConfigured,
   Unreachable, Timeout, DaemonDown, HelperFailed, Internal }` with
   `exit_code()` and `as_str()`; `struct PanioError { kind, message, target,
   channel, host, child_exit }` implementing `std::error::Error`, with small
   constructors (`PanioError::not_configured(msg).target(t)`).
2. **Shared lookups.** The ~20 copies of `target '{t}' not found in lab` and
   the "has no … channel" family become two helpers (`require_target`,
   `require_channel`) that return `PanioError`. The message text is
   unchanged, so humans and existing evals see no difference.
3. **`main()`.** Walk the `anyhow` chain with `downcast_ref` for `PanioError`,
   then `LabError` (→ `not_configured`); anything else is `internal`. Print
   `{e:#}` as today; then, if JSON errors are on, the object. Exit with the
   kind's code.
4. **One exit path for children.** Replace the scattered
   `exit(status.code()…)` calls with helpers that return a `PanioError` (or
   an `ExitCode` for the two legitimate pass-through cases):
   - `dispatch` result: 255 → `unreachable` (with `host`); any other code is
     the remote panio's own and is returned unchanged, with no local object.
   - hook result: non-zero → `helper_failed` with `child_exit`; 126/127 →
     `not_configured`; killed by a signal → `helper_failed`, `child_exit` null.
     On Windows (`cmd.exe /C`) a missing hook exits 1, like a failing script,
     so it is `helper_failed` there (decided 2026-09-25 after Windows CI showed
     `cmd.exe /C` exits 1, not 9009, for an unknown command).
   - passthrough: see D3.

## Decisions

- **D1 — how JSON errors are requested: env var `PANIO_JSON_ERRORS=1`, or
  the global `--json-errors` flag, which has the same effect.**
  (Approved 2026-09-24.) The flag cannot be called `--json`: five subcommands
  already own that name. panio reads the variable once at startup and then
  removes it from its own environment, so hooks, helpers and daemons do not
  inherit it (changed during M2, review finding N1: a hook that runs panio
  would otherwise print its own object ahead of the outer one, breaking R2's
  "exactly one"). A hook that wants JSON from a nested panio sets the
  variable itself. **Across dispatch the request travels as the
  `--json-errors` argument, not the variable** (changed during M1): the
  `FORWARDED_ENV` prelude needs a POSIX shell on the control host, so a
  Windows host would fail the whole command, and interactive dispatch skips
  the prelude entirely. An argument crosses both. The cost is D5. Captured
  internal sub-runs (`dispatch::run_subcommand`) do not pass it, since the
  local panio reads their output and reports the failure itself. Clap
  parse errors (exit 2) also get the object when requested, with clap's first
  line as `message` (plus the arguments it names when that line ends in `:`;
  "a subcommand is required" when clap shows a group's help instead).
- **D2 — unclassified errors. Recommend: 109 `internal`.** Keeping 1 would blur
  "negative answer" (R1) and hide unclassified sites. 109 makes them visible,
  and the milestones shrink that set. *Cost:* any script testing `== 1` for
  failure breaks; `!= 0` keeps working. This is a breaking change, so the
  release is 0.5.0.
- **D3 — passthroughs. Recommend: keep the child's code, documented.**
  `panio ssh <t> -- cmd` is ssh-shaped; callers want `cmd`'s status, exactly
  as with ssh itself. Only panio's own failures *before* the child starts
  (unknown target, unreachable host) use the contract. A signal-killed child
  exits 128+N as a shell would, not 255. *Alternative:* wrap every non-zero as
  101 — hides the status callers asked for.
- **D4 — `serial log` with the daemon stopped: keep rc 0 and print a warning
  on stderr (`warning: serialcap daemon for '<t>' is not running — the log
  shows nothing captured since it stopped …`), and `--require-live` makes it
  exit 100 instead.** (Approved 2026-09-24; implemented in M3. The warning
  names the gap rather than the log's modification time, which panio does
  not read.) Current readers keep working, and a consumer can ask for a
  live daemon.
- **D5 — version skew.** Without JSON requested, a 0.4.x remote panio exits
  1 for any error, which the local side cannot tell from a negative answer.
  With JSON requested, a 0.4.x remote rejects the forwarded `--json-errors`
  (exit 2, `unexpected argument '--json-errors' found`) and the command does
  not run: loud rather than silent. Accepted 2026-09-24 (review finding F5):
  control hosts upgrade through apt, and a consumer that relies on the
  contract needs 0.5.0 on the host anyway.

## Cross-cutting concerns

- **Compatibility:** exit 1 → 3/4/100/101/109 for errors is a breaking change
  for scripts matching `== 1`. Message text mostly does not change, so eval
  scenarios asserting text keep passing; scenarios asserting rc 1 must be
  updated. The exceptions (M2/M3): a failing daemon request (`serialcap
  /input`, `/button`, the stable-frame wait before OCR) now reports the
  daemon's own explanation instead of the HTTP library's status line; hook
  and daemon-stop failures print one `… exited with code N` line; `serial
  log` with the daemon stopped adds a `warning:` line on stderr.
- **Public repo:** tests and docs use `bench1`, `nuc`, `192.0.2.10`.
- **Consumer follow-up:** when released, tell the user the version. The
  consumer (`bringup-kit`, `classify_failure` on branch `m6-panio-adapter`)
  then maps these codes, rewrites its test against them, and runs its
  hardware checks against an upgraded control host.
- **Rollout:** after `apt upgrade` on a control host, running daemons keep the
  old binary: `panio daemons restart --stale`, and restart netbootd when
  convenient.

## Verification strategy

- Integration tests (`cli/tests/`, run the built binary against a temp lab):
  one per kind, asserting the exit code and every JSON field, using the inputs
  from the consumer's list (`nosuch`, a target with no `hid` channel, a
  missing serial interface, stopped serialcap).
- Dispatch: a test host whose ssh command is a stub script (exit 255; exit 3
  with a JSON line on stderr) proves R3 without a network. How to point a host
  at a stub needs a quick look at `ssh.rs` (milestone M2 investigation).
- Hook: a lab whose `cycle_cmd` is `exit 7` → rc 101, `child_exit: 7`;
  `cycle_cmd` naming a missing file → rc 3.
- Hardware check: the consumer's adapter against an upgraded control host, and
  the R6 repro in that setup.
- **This machine cannot build `cli` today**: `pkg-config`/`libudev-dev` are
  missing (`libudev-sys` build script fails). Install them, or build on the
  build VM.

## Risks and open questions

- D1–D4 need the user's decision; D2 and D4 change observable behavior.
- R6's silent path is unknown until reproduced on a control host; it may be a
  dispatch/hook interaction this design does not yet name.
- ~140 error sites: classifying all of them is out of scope for the first
  release. Unclassified ones exit 109 and are tracked, not guessed.
