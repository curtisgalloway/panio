<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# MCP server (experimental)

**Terms:** MCP (Model Context Protocol) lets an agent call named tools;
stdio is the process's standard input/output; a capture epoch identifies one
serial supervisor lifetime. See the
[glossary](glossary.md).

`paniolo mcp` serves paniolo's runtime commands as
[Model Context Protocol](https://modelcontextprotocol.io) tools over stdio. An
agent harness that speaks MCP can then drive a target without a shell. For
example, a screenshot comes back as an image in the tool result instead of a
file the agent has to open.

It is experimental: the tool set covers looking at a target (screen, screen
text, serial log, power state) and acting on it (keyboard and mouse, serial
input, power). Configuration stays in the CLI. The design is in
[`notes/mcp-server.md`](https://github.com/curtisgalloway/paniolo/blob/main/notes/mcp-server.md).

## Set it up

Run it on the machine that holds your lab file, usually your own machine,
not a control host. Tools that touch a remote target dispatch over SSH
exactly as the CLI does.

Claude Code:

```bash
claude mcp add paniolo -- paniolo mcp
# a lab file other than the default:
claude mcp add paniolo -- paniolo --lab ~/lab.toml mcp
```

Any other harness: configure a stdio server whose command is `paniolo` with
arguments `["mcp"]`. In the common JSON form:

```json
{ "mcpServers": { "paniolo": { "command": "paniolo", "args": ["mcp"] } } }
```

The harness starts the server and keeps it running for the session. The
server holds no state of its own, so restarting it loses nothing.

## Tools

| Tool | Arguments | Returns |
|---|---|---|
| `target_list` | none | JSON: each target's name, description, hosts, and channels (kind, name, host). |
| `video_shot` | `target` (required); `changed_since` (a hash); `stable` (bool); `timeout_ms` (0–120000, default 2000) | The screen as a PNG image, plus a text line `signal=<s>  hash=<h>`. |
| `video_read` | `target`; `stable`; `timeout_ms` | The text on the screen (OCR), as `paniolo video read`. |
| `serial_status` | `target`; `interface` | Read-only JSON: configured/active identity, process and port state, capture epoch and RX/TX progress. Missing evidence is unknown; none proves target command execution. |
| `serial_evidence` | `target`; `interface`; `epoch`; `since` (requires epoch); `max_bytes` (1–1048576, default 65536) | Read-only JSON: immutable exact-byte receive records, original RX/capture times, observation time, epoch/generation, retained cursors and cap/gap/fault diagnostics. See [receive evidence](serial.md#immutable-receive-evidence). |
| `serial_log` | `target`; `interface`; a window: `tail` (≤ 5000), `since`, `from`, `to` (sequence numbers) | Console lines `[time] #<seq> <text>`, as `paniolo serial log`; the last 200 when no window is given. |
| `serial_send` | `target`; `text` (required); `interface`; `newline` (default true); `pace_ms` (≤ 10000) | Writes the text to the console, as `paniolo serial send`. |
| `hid_send` | `target`; `command` (required): one HID command as an array of words, e.g. `["key", "ENTER"]` | Injects keyboard or mouse input, as `paniolo hid send`. |
| `power_state` | `target` | `on` or `off`, as `paniolo power-state`. |
| `power_on`, `power_off`, `power_cycle` | `target` | Switches power, as `paniolo power on`/`off` and `paniolo power-cycle`. |

Every tool but `target_list` needs `target`. The tools that change the target
(`serial_send`, `hid_send`, `power_on`, `power_off`, `power_cycle`) are
marked as such, so a harness that asks before side effects will ask.

`video_shot` needs the target's video daemon to be running already
(`paniolo video watch <target>`). To wait for the screen to change after an
action, pass the previous result's hash as `changed_since`, the same loop the
`kvm-puppeting` skill teaches with `video shot --changed-since`.

A tool that runs and fails returns a result with `isError: true`. Its text
gives paniolo's exit code, the error message, and the one-line JSON error
object described in [Exit status and errors](errors.md). When a tool that
changes the target fails with `unreachable` (4) or `timeout` (22), the text
also says the outcome is unknown: the action may have happened, so check the
target's state before repeating it. A malformed call (unknown tool, missing
`target`) is a JSON-RPC error instead.

## How it behaves

- **stdout is the protocol.** Each tool runs the matching `paniolo` command
  as a child process with stdin closed and stdout captured, so nothing a
  command prints can corrupt the stream. Diagnostics go to stderr, which
  harnesses usually log.
- **One call at a time.** A `video_shot` waiting on `changed_since` blocks
  other calls until it returns, for at most `timeout_ms`; so does a slow
  `power_cycle`.
- **Dead links fail, not hang.** A remote call over an SSH connection that has
  gone silently dead fails as `unreachable` within about 15 seconds; see
  [Distributed control](distributed-control.md).
- MCP protocol revisions 2024-11-05 through 2025-11-25 are accepted.
