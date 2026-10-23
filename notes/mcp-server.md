<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# An MCP server for panio

> **Status: built (steps 1–4); hardware-tested for video only.** Converged
> 2026-10-02. Step 1 (measure per-call dispatch) is done: on a LAN it costs
> ~17 ms per call, so the spike uses per-call dispatch and the persistent
> worker is deferred (see *Step 1 result*). Step 2 (SSH keepalive) is done and
> hardware-checked. Step 3's spike, `panio mcp` with `target_list` and
> `video_shot`, is built and passed its stop point against a real target and
> a real Claude Code client. Step 4 (the full tool set) is built and tested
> without hardware. Still untested: a target with a live picture, and the new
> tools against real serial and HID hardware.

## Goal

Let an agent drive a target through MCP tools instead of shelling out to
`panio` over Bash and SSH. The CLI remains the one implementation; the MCP
server is a thin front end over the same command handlers.

## Why

1. **Images in the tool result.** The KVM loop in
   `skills/kvm-puppeting/SKILL.md` is: `video shot -o screen.png`, open the
   file, copy the `hash=` off stderr for the next `--changed-since`. A
   `video_shot` tool returns the image and the hash in one result. This is
   the most-repeated loop an agent runs, so it gains the most.
2. **Agents without a shell.** Harnesses with no Bash or SSH (desktop chat
   clients, other agent frameworks) can drive a target.
3. **Typed calls.** Schemas replace flag-guessing. Error kinds already exist
   (`docs/errors.md`, `--json-errors`) and map directly onto tool errors.

## Decisions

| Question | Decision | Why |
|---|---|---|
| Transport | **stdio**, spawned by the harness | No new listening port. Every daemon today is loopback-only and token-authenticated (#196); a network MCP endpoint would be the first non-loopback listener able to cut power and inject keys. |
| Where it runs | **The dev machine** (`panio mcp`) | The dev machine is the hub ([distributed-control.md](../docs/distributed-control.md)). One target's channels can live on several control hosts; only the hub reaches all of them. |
| Remote placement | `ssh <host> panio mcp` is a **fallback only** | When the harness runs on a machine with no panio install or lab file. The harness owns that pipe and generally will not relaunch a dead stdio server, so a drop loses every tool until restart. |
| Reaching control hosts | **One persistent worker per host**, opened by `panio mcp` on first use (step 3) | Removes per-call setup cost; see *Cost of per-call dispatch*. |
| Tool surface | **Runtime verbs only** | Configuration (`set`/`add`/`rm`) stays CLI-only, matching the rule that config commands never act on an implied target. |
| Implementation | Tools **run the CLI as a child process** (stdin null, stdout captured); pure lab reads run in-process | No second implementation to drift. Calling handlers in-process was the first idea, but handlers print to stdout, which is the protocol channel, and `video shot` exits the process. A child costs a few ms against an agent turn of seconds. Investigate generating schemas from the clap tree so docs/help/skill/MCP stay one source. |
| Protocol library | **Hand-written** JSON-RPC over stdio, no new dependency | Decided 2026-10-02. `rmcp` (the official SDK) scored 8.5/10 on `dep-quality`, but at 0.35 confidence (GitHub data unavailable), and would add 35 crates to `cli` (102 → 137): tokio, futures and a second `syn` major in an otherwise synchronous CLI. It also shipped three major versions in about three months. A tools-only stdio server is five messages and a few hundred lines. Revisit `rmcp` for an HTTP transport, resources or notifications, or if tracking spec revisions by hand starts to hurt. |
| Server state | **None of its own** | The daemons already hold the real state (capture, serial logs, held keys). Restarting `panio mcp` loses nothing. |

### First tool set

`target_list`, `video_shot` (image + hash), `video_read`, `serial_log`,
`serial_send`, `hid_send`, `power_state`, `power_cycle`, `power_on`,
`power_off`.

## Cost of per-call dispatch

Today a command whose channel is on another host does three SSH operations
(`cli/src/dispatch.rs`), each over the shared ControlMaster
(`cli/src/ssh.rs`), so there's no new TCP connection or authentication:

1. `sftp_put` — ship a one-target lab file.
2. `ssh … panio …` — start a fresh `panio` on the host, which parses that
   file and runs the command.
3. `sftp_rm` — delete the file.

Three round trips plus a process start per action. An agent loop of
`shot → moveabs → click → shot` pays that four times: 12 round trips where a
persistent worker needs 4.

## Persistent worker design

```
harness ──stdio──▶ panio mcp (dev machine)
                     ├── ssh bench1 panio worker   ◀─ one long-lived pipe
                     └── ssh bench2 panio worker   ◀─ opened on first use
```

- `panio mcp` opens `panio worker` on a control host the first time a
  tool needs that host, and keeps it.
- The lab slice is sent once, when the worker starts.
- Each call is one JSON line down the pipe and one JSON line back: one round
  trip, no new process. Binary results (PNG) go base64 in the JSON, or as a
  length-prefixed frame if that turns out to matter.
- The worker runs the same handlers the CLI does, taking requests from stdin
  instead of argv. No new port, no new auth.
- A multi-host target routes each channel's call to that channel's host.
- The CLI keeps per-call dispatch. Nothing changes for a person at a shell.

### Reconnect

Because `panio mcp` owns the `ssh` child, it can restart it.

**Detecting the drop:**

- A clean drop (link down, host reboot): `ssh` exits, the pipe reads EOF,
  restart the worker.
- A silent drop (laptop sleep, Wi-Fi change) leaves TCP half-open and a read
  waits forever. Fix with `ServerAliveInterval=5` + `ServerAliveCountMax=3`
  (ssh exits within ~15 s), and a per-request deadline that kills the worker
  on expiry. `ssh.rs` sets neither ServerAlive option today — this is step 2,
  and it helps the CLI too.

**Retrying the call in flight:** if the pipe dies after a request was sent but
before the reply arrived, the outcome is unknown.

| Call | On a drop |
|---|---|
| `video_shot`, `video_read`, `serial_log`, `power_state`, `target_list` | Reconnect and retry automatically. Reads change nothing. |
| `hid_send`, `serial_send`, `power_cycle`, `power_on`, `power_off` | Reconnect but **do not resend**. Return an "outcome unknown: connection lost; the action may or may not have run" error; the agent re-reads state and decides. |

This is the rule `ch9329/src/uart.rs` already follows: retry idempotent
queries only. Request-ID deduplication doesn't help, because the remote worker
usually dies with its SSH session and so forgets which IDs it ran.

## Steps

Each step is independently useful and has a stop point.

### 1. Measure per-call dispatch cost

Run on the dev machine (the one holding the lab file). Pick a target `T`
whose video channel is on a different control host `H`. `video show` is a
dispatched read that does almost no work on the far side, so its time is
mostly dispatch overhead.

```bash
T=<target>; H=<control host>

# A. Full per-call dispatch: sftp put + ssh exec + sftp rm.
for i in 1 2 3 4 5 6; do /usr/bin/time -p panio video show "$T" >/dev/null; done 2>&1 | grep real

# B. One bare round trip over the warm ControlMaster (what a worker call costs, minus handler time).
#    Reuse panio's master socket so this matches what dispatch sees.
for i in 1 2 3 4 5 6; do /usr/bin/time -p ssh "$H" true; done 2>&1 | grep real

# C. The command run directly on the control host (handler + process start, no dispatch).
ssh "$H" "for i in 1 2 3 4 5 6; do /usr/bin/time -p panio video show $T >/dev/null; done 2>&1 | grep real"
```

Drop the first run of each (it may open the ControlMaster). `hyperfine` gives
cleaner numbers if it is installed.

Then the same for the loop that matters, with a real screenshot:

```bash
for i in 1 2 3 4 5 6; do /usr/bin/time -p panio video shot "$T" -o /tmp/s.png; done 2>&1 | grep real
ls -l /tmp/s.png   # payload size per shot
```

Record: median of A, B, C; shot time; PNG size; whether `H` is on the LAN or
across a WAN/VPN.

**Decision:**

- **A − B ≥ ~100 ms** → build the persistent worker into the spike (step 3).
- **A − B is a few tens of ms** → spike with per-call dispatch; an agent's own
  turn takes seconds. Add the worker when the KVM loop shows it is needed. The
  tool interface is the same either way.
- **Shot time dominated by PNG transfer** → neither transport choice fixes
  that; look at JPEG or downscaled returns for `video_shot` instead.

#### Step 1 result (2026-10-02)

Measured from the dev machine against one target whose video channel is on a
Pi 5 control host (`bench1`) on the same LAN (ping RTT 0.26 ms). panio 0.6.0
on both ends. Six runs each, the first dropped; the remaining five were
identical at the timer's resolution (10 ms for A/B/D, 1 ms for C).

| | Measurement | Median |
|---|---|---|
| A | `panio video show T` — full per-call dispatch | 0.02 s |
| B | `ssh bench1 true`, fresh connection | 0.12 s |
| B′ | `ssh bench1 true` through panio's ControlMaster socket | < 0.01 s |
| C | `panio video show T` run on `bench1` itself | 0.003 s |
| D | `panio video shot T -o …` (daemon running) | 0.04 s |

PNG size: 33 KB — but the target had no signal, so that is a blank frame. A
real desktop frame will be larger and slower to encode.

**Reading:** with the ControlMaster warm, dispatch adds about **17 ms per
call** (A − C), well under the 100 ms threshold. A screenshot round trip is
about 40 ms. An agent's own turn takes seconds, so a persistent worker would
save a few percent at best on a LAN.

**Decision:** spike with per-call dispatch; defer the worker. Revisit if:

- a control host sits across a VPN or WAN, where each of dispatch's three SSH
  operations pays a real round trip (re-run A and B′ there);
- `video shot` on a live desktop frame turns out to be transfer-bound, which
  the worker would not fix anyway (see the third bullet of the decision rule).

The worker design above stays as the plan for that case. Reconnect still
matters without it: per-call dispatch over a half-open ControlMaster hangs the
same way, so step 2 is still worth doing first.

### 2. SSH keepalive (independent, small) — done

Add `ServerAliveInterval=5` and `ServerAliveCountMax=3` to the options in
`cli/src/ssh.rs`. Helps every remote CLI call today, and the worker depends on
it later.

Done 2026-10-02: `ssh::liveness_args` now supplies `ConnectTimeout` and both
`ServerAlive*` options to every ssh and sftp panio starts (`base_args` and
`transfer_args`), so whichever invocation starts the ControlMaster carries
them. A half-open link now fails as `unreachable` (exit 4) in ~15 s instead of
hanging.

Hardware check, 2026-10-02 (dev machine → `bench1`, same LAN, no keepalive in
`~/.ssh/config`). Dead link simulated by `SIGSTOP` on the control host's
`sshd-session` serving panio's ControlMaster, always resumed afterwards.
Command: `video shot T --changed-since H --timeout 60000` under `timeout 100`.

| Build | Result |
|---|---|
| This branch (fresh master) | exit 4 (`unreachable`), **16 s** from STOP to exit. A plain `video show T` straight afterwards succeeded. |
| 0.6.0, run 1 | still blocked 78 s after STOP (past its own 60 s timeout), then interrupted by hand. |
| 0.6.0, run 2 | exited 0 on its own 62 s after STOP, as if the link were fine. **Unexplained** — same method, same binary. |

The new build behaves as designed. The baseline is inconsistent, so treat
"0.6.0 hangs" as likely (run 1, and `ServerAliveInterval` 0 in its config)
rather than shown. Not yet tried against a real sleeping laptop.

For whoever repeats it: `-o /dev/null` fails for `video shot` (it writes a
temp file beside the output path), and unprivileged `ss -p` on the control host
does not show sshd's pid, so the process has to be found by elimination.

### 3. Spike `panio mcp` — built, stop point passed

- New module `cli/src/mcp.rs`, one clap variant `Command::Mcp`.
- Two tools only: `target_list` and `video_shot` (image + hash).
- Per-call dispatch or worker, per step 1's decision.
- Needs a dependency decision: an MCP crate (e.g. `rmcp`) versus a hand-rolled
  JSON-RPC over stdio. AGENTS.md requires discussion before a new dependency.
- Try it: point Claude Code at `panio mcp`, run one look-act-verify cycle
  against a real target.

Built 2026-10-02: hand-written (see *Decisions*), per-call dispatch, tools
run the CLI as a child. Unit tests drive the server over in-memory
stdin/stdout, and `video_shot`'s tests execute a fake `panio` script to
prove the argv, the closed stdin and the image read-back. An end-to-end run of
the real binary over a pipe (handshake, `tools/list`, `target_list`, and a
`video_shot` with no daemon returning a clean tool error) worked. User doc:
`docs/mcp.md`.

#### Step 3 result (2026-10-02)

Run on the dev machine against `target-machine`, whose video channel is on
`bench1`, same LAN. Its daemon was the only one running in the lab, and the
target had **no signal**, so every shot was a blank frame: this proves the
plumbing, not reading a live screen.

**Protocol over a pipe: pass on every check.**
- stdout was exactly the 4 expected JSON lines and nothing else; stderr was
  empty.
- `video_shot` returned `isError: false`, a `image/png` item and
  `signal=no_signal  hash=ffffffffffffffff`.
- The decoded PNG was valid: 1920×1080, 33 KB, uniformly near-black.
- A whole session (process start, `initialize`, one `video_shot`) took a
  median of **52 ms** over 6 runs.
- `changed_since` with that hash and `timeout_ms: 3000` returned after
  3,056 ms with `(timeout)`, as expected on a static screen.

**Real harness: pass.** A headless Claude Code run (`claude -p`, a throwaway
`--mcp-config` with `--strict-mcp-config`):
- found and called both tools: `target_list`, then `video_shot` with
  `stable: true`, which it chose on its own;
- took 4 turns and 22 s, one of them a `ToolSearch`: Claude Code loads MCP
  tools as deferred, so the model finds them by searching. Tool names and
  descriptions are what that search matches, so they need to say plainly what
  each tool does (screenshot, serial log, …), not just name the panio verb;
- described the screenshot correctly: a solid near-black 1920×1080 frame with
  `signal=no_signal`, so nothing was reaching the capture input;
- listed all the lab's targets correctly.

**Verdict:** the image reaches the model in one call of about 50 ms, with no
file path and no second read step, and the model understood it unprompted.
That clears the stop point, so step 4 can go ahead.

**Still open:** a target with a live picture. Also, `target_list` returns the
lab's real host and target names, so a transcript quoted from a real run must
be scrubbed before it goes anywhere public.

**Stop point:** if image-in-the-result is not clearly better than
shot-then-read, stop here.

### 4. Fill out the tool set — done

The remaining tools from *First tool set*, with the retry rules from
*Reconnect* if the worker exists.

Done 2026-10-02: `video_read`, `serial_log`, `serial_send`, `hid_send`,
`power_state`, `power_on`, `power_off`, `power_cycle`.
- Each maps onto one panio command through `cli_call()`. A test parses
  every resulting argv with the real clap `Cli`, so a tool can't build a
  command line the CLI would reject; that includes text and HID words that
  look like options (`-rf /`, `-30`, `--help`).
- There is no worker, so there is nothing to retry. Instead, a write tool that
  fails with `unreachable` (4) or `timeout` (22) says "Outcome unknown" in its
  error, which is the *Reconnect* rule applied to per-call dispatch. A read
  needs no such warning: running it again is harmless.
- Write tools carry `readOnlyHint: false` and `destructiveHint: true`, so a
  harness that asks before side effects asks.
- `hid_send` takes `command` as an array of words (`["key", "ENTER"]`), the
  helper's own vocabulary. Its description lists the common verbs and points
  at `panio skill kvm-puppeting` for the rest.
- Checked end to end with the real binary against a lab whose power hooks are
  shell commands: `power_state` and `power_cycle` ran them. `serial_log`,
  `serial_send` and `hid_send` each returned the expected classified error
  (helper missing, daemon down, no channel). Not yet run against real serial
  or HID hardware.

### Lease tools (#360)

`lease_acquire`, `lease_renew`, `lease_release` and `lease_status` wrap
`panio lease acquire|renew|release|status` (see
[target-leases.md](target-leases.md)) and run as children like every other
tool, so remote dispatch is unchanged.

- **One MCP server is one holder.** The server keeps the token `lease_acquire`
  mints, per target, in memory, and sets `PANIO_LEASE` on every child it runs
  for that target. The agent never sees the token: the result says the lease is
  held, by whom and until when. `acquire` runs with `--json`; output that does
  not parse is a tool error and is never echoed.
- A child for a target with no stored token gets the `PANIO_LEASE` the server
  itself started with (a harness can hand a server an existing lease), else the
  variable is removed from the child's environment so a token never leaks to
  another target.
- A successful `lease_release` forgets the target's token. A refusal (exit 23,
  "your lease ... ended") leaves the map alone for the agent to read and decide.
  `renew` and `release` with no stored token still run the child, which refuses.
- `acquire`, `renew` and `release` are write tools; `lease_status` is read-only
  and its target is optional (omitted, it lists every target).

### 5. Measure against the skill

Add an **MCP** condition beside Cold / Warm / Preloaded in
`docs/dev/agent-evals.md` and run the KVM scenarios. Expand the surface only
if MCP beats the registered skill.

### 6. Ship it

Per the AGENTS.md PR checklist: `docs/` page (and `mkdocs.yml` nav),
`--help` text, `skills/panio/SKILL.md`, the AGENTS.md module layout, and a
note in `README.md`'s capabilities table.

## Open questions

- Generate tool schemas from clap, or hand-write the ten? Generation keeps one
  source of truth; hand-writing gives better tool descriptions.
- Does `serial_log` want MCP resources or notifications for a live tail, or is
  polling with a line range enough? Client support for notifications is uneven,
  so start with polling.
- Should `video_shot` return PNG, JPEG, or a downscaled image by default? Image
  size costs agent context as well as transfer time.
