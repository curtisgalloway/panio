<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# MCP server (experimental)

**Terms:** MCP (Model Context Protocol) lets an agent call named tools;
stdio is the process's standard input/output; a capture epoch identifies one
serial supervisor lifetime. See the
[glossary](glossary.md).

`panio mcp` serves panio's runtime commands as
[Model Context Protocol](https://modelcontextprotocol.io) tools over stdio. An
agent harness that speaks MCP can then drive a target without a shell. For
example, a screenshot comes back as an image in the tool result instead of a
file the agent has to open.

It is experimental: the tool set covers looking at a target (screen, screen
text, serial log, power state) and acting on it (keyboard and mouse, serial
input, power). Configuration stays in the CLI. The design is in
[`notes/mcp-server.md`](https://github.com/curtisgalloway/panio/blob/main/notes/mcp-server.md).

Four lease tools (`lease_acquire`, `lease_renew`, `lease_release`,
`lease_status`) reserve a target for this server. See [Target leases](leases.md#mcp).

## Set it up

Run it on the machine that holds your lab file, usually your own machine,
not a control host. Tools that touch a remote target dispatch over SSH
exactly as the CLI does.

Claude Code:

```bash
claude mcp add panio -- panio mcp
# a lab file other than the default:
claude mcp add panio -- panio --lab ~/lab.toml mcp
```

Any other harness: configure a stdio server whose command is `panio` with
arguments `["mcp"]`. In the common JSON form:

```json
{ "mcpServers": { "panio": { "command": "panio", "args": ["mcp"] } } }
```

The harness starts the server and keeps it running for the session. The
server keeps no state except the lease tokens it was given by `lease_acquire`;
restarting it loses those tokens (the lease itself stays until it expires, so
`lease_release` with `preempt`, or wait).

## Tools

| Tool | Arguments | Returns |
|---|---|---|
| `target_list` | none | JSON: each target's name, description, hosts, and channels (kind, name, host). |
| `video_shot` | `target` (required); `changed_since` (a hash); `stable` (bool); `timeout_ms` (0–120000, default 2000) | The screen as a PNG image, plus a text line `signal=<s>  hash=<h>`. |
| `video_read` | `target`; `stable`; `timeout_ms` | The text on the screen (OCR), as `panio video read`. |
| `serial_status` | `target`; `interface` | Read-only JSON: configured/active identity, process and port state, capture epoch and RX/TX progress. Missing evidence is unknown; none proves target command execution. |
| `serial_wait` | `target`; `epoch` and `since` (required); `interface`; `pattern`; `timeout_ms` (0–3600000); `max_bytes` (1–1048576); `max_output_bytes` (4096–8388608); `include_pending`; `follow` | Bounded fresh raw evidence with timing and explicit matched/timeout/cap/disconnect/fault status. Accepted bytes survive incomplete waits. Follow objects are returned together when the call finishes. |
| `serial_evidence` | `target`; `interface`; `epoch`; `since` (requires epoch); `max_bytes` (1–1048576, default 65536) | Read-only JSON: immutable exact-byte receive records, original RX/capture times, observation time, epoch/generation, retained cursors and cap/gap/fault diagnostics. See [receive evidence](serial.md#immutable-receive-evidence). |
| `serial_log` | `target`; `interface`; a window: `tail` (≤ 5000), `since`, `from`, `to` (sequence numbers) | Console lines `[time] #<seq> <text>`, as `panio serial log`; the last 200 when no window is given. |
| `serial_send` | `target`; `text` (required); `interface`; `newline` (default true); `pace_ms` (≤ 10000) | Writes the text to the console, as `panio serial send`. |
| `serial_transact` | `target`; `text` and `until` (required); `interface`; `timeout_ms` (1–3600000, default 30000); `max_bytes` (1–1048576, default 65536); `newline` (default true); `pace_ms` (≤ 10000) | Sends the text and waits for the reply under the interface's transaction lock, as `panio serial transact`: the `serial_wait` result JSON for the bytes received after the send, plus `sent_bytes`. See [transactions](serial.md#requestresponse-transactions). |
| `hid_send` | `target`; `command` (required): one HID command as an array of words, e.g. `["key", "ENTER"]` | Injects keyboard or mouse input, as `panio hid send`. |
| `action_list` | `target` | JSON array of the target's named actions (name, description, requires_power, host), as `panio action list --json`. See [Named actions](actions.md). |
| `action_run` | `target`; `name` (required) | Runs one named action, as `panio action run`. |
| `power_state` | `target`; `level` | A line like `Power ON  (pi5)`, as `panio power-state`. With no `level`, a target with [power levels](power.md#power-levels-soft-and-hard) gets one line per level, the default first (`Power OFF  (pi5, level hard)`; `Power UNKNOWN … no state_cmd` for a level that cannot be read). With `level`, only that level. |
| `power_on`, `power_off`, `power_cycle` | `target`; `level` | Switches power, as `panio power on`/`off` and `panio power-cycle`, with `--level` when `level` is given. A level without the needed hook fails as `not_configured` (3); it never falls back to the default level. |
| `lease_acquire` | `target`; `owner`; `duration`; `idle`; `wait` (at most `5m`; call again to keep waiting); `preempt` | Reserves the target for this server. The server keeps the token and applies it to this target's commands; the result never shows it. Exit 23 when someone else holds it. See [Target leases](leases.md#mcp). |
| `lease_renew` | `target`; `duration` | Extends the lease this server holds. |
| `lease_release` | `target`; `preempt` | Ends the lease this server holds and cleans the target up; `preempt` ends someone else's. |
| `lease_status` | `target` (optional) | JSON: who holds the target and for how long, never the token; no `target` lists every target. |

Every tool but `target_list` and `lease_status` needs `target`. The tools that
change the target (`serial_send`, `serial_transact`, `hid_send`, `action_run`,
`power_on`, `power_off`, `power_cycle`, `lease_acquire`, `lease_renew`,
`lease_release`) are marked as such, so a harness that asks before side
effects will ask. `lease_status` is marked as not read-only too: it cleans up
a lease that has expired.

`video_shot` needs the target's video daemon to be running already
(`panio video watch <target>`). To wait for the screen to change after an
action, pass the previous result's hash as `changed_since`, the same loop the
`kvm-puppeting` skill teaches with `video shot --changed-since`.

A tool that runs and fails returns a result with `isError: true`. Its text
gives panio's exit code, the error message, and the one-line JSON error
object described in [Exit status and errors](errors.md). When a tool that
changes the target fails with `unreachable` (4) or `timeout` (22), the text
also says the outcome is unknown: the action may have happened, so check the
target's state before repeating it. A malformed call (unknown tool, missing
`target`) is a JSON-RPC error instead.

## How it behaves

- **stdout is the protocol.** Each tool runs the matching `panio` command
  as a child process with stdin closed and stdout captured, so nothing a
  command prints can corrupt the stream. Diagnostics go to stderr, which
  harnesses usually log.
- **One call at a time.** `serial_wait` and `serial_transact` can block other
  calls up to their finite deadline (default 30 seconds, maximum one hour); use a deadline appropriate
  for the task. A `video_shot` waiting on `changed_since` blocks
  other calls until it returns, for at most `timeout_ms`; so does a slow
  `power_cycle`.
- **Dead links fail, not hang.** A remote call over an SSH connection that has
  gone silently dead fails as `unreachable` within about 15 seconds; see
  [Distributed control](distributed-control.md).
- MCP protocol revisions 2024-11-05 through 2025-11-25 are accepted.
