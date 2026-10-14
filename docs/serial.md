# Serial console

**Terms:** a capture epoch identifies one interface supervisor's lifetime;
RX is bytes received from the target, TX is bytes accepted by the serial driver;
a connection generation counts successful opens within an epoch.
A receive record is one immutable byte chunk; its evidence cursor is the
pair of capture epoch and RX sequence, distinct from line-log sequences.
See the [glossary](glossary.md).

| Mode | Command | What you get |
|---|---|---|
| Interactive | `serial connect` | A foreground `tio` terminal |
| Daemon | `serial watch` | The serialcap daemon: a timestamped capture log and a dashboard terminal |

Both modes need the port exclusively. Run one at a time.

---

## Setup

```bash
paniolo serial add console -t target-machine \
    --device /dev/tty.usbserial-0001 \
    --baud 115200

# Optional: also wire a power sense signal on this interface (see power.md)
paniolo serial set console -t target-machine --sense cts

# Optional: declare this interface's DTR line is wired to the J2 power button,
# which enables `serial dtr` / `serial reset` on it (opt-in; see power.md)
paniolo serial set console -t target-machine --power-button

# Optional: a fixed port for this target's serialcap daemon (set on any one
# interface; 0 clears). See dashboard.md "Fixed ports".
paniolo serial set console -t target-machine --port 8724
```

A target can have several named interfaces (e.g. `console`, `bmc`). Remove
one with:

```bash
paniolo serial rm console -t target-machine
```

List detected serial devices:

```bash
paniolo serial devices
```

Show a target's configured interfaces:

```bash
paniolo serial show [target-machine]
```

---

## Virtual machine consoles

A VM's serial console is a pty, so it works like any other port. Use it to
watch firmware, the bootloader or an unattended installer before SSH exists.

The VM gets a new device path on every boot:

```bash
utmctl attach <vm-name>          # UTM: prints "PTTY: /dev/ttys006"
                                 # qemu: -serial pty prints the path at startup
```

Configure it as an ordinary interface:

```bash
paniolo serial add console -t winvm --device /dev/ttys006 --baud 115200
paniolo serial watch winvm
paniolo serial log winvm --tail 50
```

After a VM restart, re-point the interface:

```bash
paniolo serial set console -t winvm --device "$(utmctl attach winvm | sed -n 's/^PTTY: //p')"
```

- **`--baud` is recorded but not applied.** A pty has no line rate; the
  daemon logs `is a pty — opening without a line rate`.
- **`--device` can be a symlink to the pty.** The `hidrig` console bridge
  publishes a DUT's console this way (see
  [hid-dual-board-design.md](dev/hid-dual-board-design.md)).
- **Both modes work**, one at a time.
- **Writing is a real keypress.** On an EDK2 guest the PL011 UART is ConIn
  (UEFI console input), so `serial send` answers "Press any key to boot from
  CD or DVD" headlessly. The prompt's timing varies by tens of seconds, so send
  in a loop:

  ```bash
  for i in $(seq 120); do paniolo serial send winvm " "; sleep 1; done
  ```

- **Input works in firmware only**: bootloader menus, the UEFI Shell, boot
  prompts. A Windows guest stops reading ConIn when WinPE starts.

You can also wire a VM's lifecycle to the [power](power.md) hooks
(`on_cmd`/`off_cmd`/`state_cmd`): `utmctl start|stop`, with a `state_cmd` that
translates `utmctl status` into `on`/`off`.

---

## Interactive mode

```bash
paniolo serial connect [-i console] [target-machine]
```

Opens a foreground `tio` session. Exit with Ctrl+T Q. It conflicts with the
daemon.

---

## Daemon mode

```bash
paniolo serial watch [target-machine]   # start serialcap daemon
paniolo serial stop  [target-machine]   # stop it (on the target's host)
```

There is **one serialcap daemon per target**, owning all its interfaces, so
several targets can capture at once on one host. It prints its URL on start;
the [dashboard](dashboard.md) shows it too.

**Stale daemons.** A proven difference between the running and installed
executable content shows as **stale** in `paniolo serial show` and
`paniolo daemons`. `paniolo serial watch` restarts it automatically, or run
`paniolo daemons restart --stale`. An identical reinstall, path change, or
modification time change does not count as changed content.

A successful content comparison reports **binary content matches installed**
when the bytes are equal, including after an identical reinstall.

If executable content cannot be compared, the commands report **binary identity
unknown** with the reason. Unknown identity never triggers automatic restart and
is excluded from `--stale`; `paniolo daemons restart serialcap` still intentionally
restarts it. See [architecture](dev/architecture.md).

**Untracked daemons.** A daemon whose discovery file was deleted keeps
running and holding the ports. On Linux, systemd's `/tmp` cleanup
(`q /tmp 1777 root root 10d` on Debian) removes the file after ten idle days.

- `serial show` reports `running, untracked (pid N)`.
- `paniolo daemons` lists it under **Untracked daemons**.
- `serial watch` and `serial stop` reap it (`SIGTERM`, then `SIGKILL`).

paniolo matches it by the devices on its command line
(`--interface NAME=DEVICE@BAUD[:SENSE]`). See [video.md](video.md) for the
`tmpfiles.d` drop-in that prevents this.

---

## Querying captured output

`paniolo serial log` reads the capture log from disk, with no daemon round
trip. The target may be positional or `-t`; omit the target and `-i` when
there is only one.

```bash
# Tail the last 50 lines from the `console` interface
paniolo serial log target-machine -i console --tail 50

# Only lines newer than a previously-seen sequence number (poll mode)
paniolo serial log target-machine -i console --since 1840

# Specific sequence number range
paniolo serial log target-machine -i console --from 1000 --to 1200

# Keep ANSI escape codes (stripped by default)
paniolo serial log target-machine -i console --raw

# JSON Lines output (includes timestamp and sequence number)
paniolo serial log target-machine -i console --json
```

Each line has a sequence number (`seq`, stable across rotation) and a UTC
timestamp (`ts_ms`). `--since` returns lines with `seq` greater than the value
given.

**When the daemon is stopped**, `serial log` prints a `warning:` line on
stderr and exits 0. Add `--require-live` to fail with exit 100 (`daemon_down`)
instead ([exit codes](errors.md)).

**Pending lines.** The last, unterminated line can change without its `seq`
changing. When polling with `--since`, advance your cursor only past completed
records, or use `--no-pending`.

Each interface has its own log:
`/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/serial.jsonl`.

---

## Immutable receive evidence

Use `paniolo serial evidence target-machine -i console` (MCP:
`serial_evidence`) when exact received bytes and their original times matter.
This opt-in JSON representation reads the journal without opening the port,
starting a daemon, sending input, or changing the existing line/pending view.
The daemon always journals RX; older daemons report unavailable evidence.

Each `records` entry has `epoch`, `rx_seq`, `connection_generation`,
`rx_at_ms`, `captured_at_ms`, and `bytes` (an array of integers 0–255).
Invalid UTF-8, ANSI escapes and control bytes survive exactly. Synthetic
connected/disconnected markers are excluded. A prompt received before a send
keeps its original receive time even when it is read after a later echo or
completion. `observed_at_ms` timestamps this read separately.

The envelope includes `available_epochs`, selected `epoch`, retained
`earliest`/`latest` cursors, `continuation`, `raw_bytes`, `capped`,
`next_record_bytes`, `capture_pending`, `gap`, `epoch_missing`, and
`diagnostics`. Reconnect changes generation within the same epoch; a daemon
restart starts a new epoch and RX counter. Omitted `--epoch` selects the
newest known epoch, including a quiet epoch with no RX yet. Use the returned
epoch with `--since N` to request chunks strictly after N; `--since` requires
`--epoch`, so a restarted counter cannot silently reuse an old cursor.
The epoch list shows up to 64 newest epochs (`epochs_truncated` reports the
limit); explicit epoch selection still searches every retained record.

`--max-bytes N` limits the raw byte count, including raw diagnostic bytes
(default 65536, range 1–1048576). Records are indivisible: a chunk that would
exceed the remaining cap is omitted, `capped` becomes true, and
`continuation` stays at the last included RX sequence. `next_record_bytes`
gives the minimum size needed for that chunk. Increase the cap and use that
same continuation; never advance to `latest` when capped. JSON integer arrays
use up to four characters per byte; per-record metadata adds overhead. The
output is bounded by 1024 times the raw cap plus 1 MiB of envelope and
diagnostic metadata. Diagnostic byte truncation is explicit (`raw_truncated`,
original `raw_length`, segment and byte offset); it never implies complete
evidence. Treat caps, gaps, missing epochs and faults as incomplete evidence.

The owner-only journal retains five segments, each at most 1 MiB of encoded
JSON (an individual RX chunk is at most 64 KiB). It appends across restarts
until rotation expires older records. Retention and discontinuous sequences
set `gap`; completed corrupt records retain exact fault bytes in diagnostics.
An in-progress append remains `capture_pending`, without advancing a cursor
or inventing a corruption gap. A checkpoint advertises the received and
persisted cursors. The status field `health.evidence_through_seq` advances
only after journal append, flush and checkpoint; `health.evidence_error`
retains a write fault. Up to five first-fault receive snapshots retain exact
failed-chunk bytes across restarts; later failed chunks may be unavailable
when storage writes fail. These are process-level writes, not an fsync or
power-loss durability guarantee. If storage cannot even record its own fault,
only live health can report it; an offline journal cannot prove that every
byte ever received was saved. Consult `serial status` as well when capture
completeness matters. No automatic restart, reset or resend follows a fault.

## Sending input

`paniolo serial send` writes a line through the **running daemon**, so capture
continues. The daemon must be running (`paniolo serial watch`) and the
interface connected.

With two positionals the first is the target (`serial send <target> <text>`).
With one, it is the text. `-t` also works.

```bash
# Send a command (a carriage return is appended by default)
paniolo serial send target-machine -i console "iochk --live-dangerously /block/000"

# Send without the trailing carriage return
paniolo serial send target-machine -i console --no-newline "partial"
```

Success means the driver accepted every byte, not that the target ran the
command. Bytes unsent at a disconnect are dropped, not replayed. An error can
follow partial delivery, so check the console before retrying.

### Pacing a slow console (`--pace-ms`)

A **polled** console with no flow control drops input sent at full rate
(common in early bring-up, e.g. a Zircon polled console). `--pace-ms` sends
one byte every N ms. About 8 ms/byte is known-good at 115200 baud.

```bash
# Drip one byte every 8 ms — slow but overflow-proof
paniolo serial send -i console --pace-ms 8 "iochk --live-dangerously /block/000"
```

- A paced send of N bytes takes at least `(N - 1) * pace_ms` ms and blocks
  until the driver accepts the whole line.
- Requests run in FIFO order, including dashboard input.
- `--pace-ms 0` (the default) sends at full rate.

RTS/CTS is not used because many bring-up headers (e.g. the Pi 5 debug
header) have no flow-control pins.

This is `POST /input?interface=NAME[&pace_ms=N]` on the daemon (see
[HTTP API](#http-api-serialcap-daemon)).

---

## Integration with the video dashboard

`paniolo console` opens the hdmicap dashboard and starts both daemons if
needed. Its terminal connects to serialcap's WebSocket (`/stream`). A target
with no serial channel gets video (and KVM input, with a hid channel) and no
terminal.

```bash
paniolo video watch [target-machine]    # hdmicap — serves the page
paniolo serial watch [target-machine]   # serialcap — backs the terminal
paniolo console [-i <interface>] # open in browser (auto-starts both)
```

- **Scrollback.** On connect, serialcap replays up to 64 KB.
- **Multiple interfaces.** One pane per interface. `paniolo console -i <name>`
  pins one.
- **Slow clients.** A lagging client loses its own oldest output, never the
  capture log or other clients. The gap is marked:
  `── serial client lagged, dropped N chunks [HH:MM:SS UTC] ──`.
- **Authentication.** `paniolo console` passes serialcap's token in the
  `?serialws=` URL. serialcap's CORS header allows only that loopback origin.

See [dashboard.md](dashboard.md) for layout and URL parameters.

---

## DTR power control (FTDI wiring)

An FTDI adapter wired to the target's J2 power button header can press it by
toggling DTR. This is **opt-in**: the interface needs `power_button = true`
(`paniolo serial set <iface> --power-button`), or these commands error.

```bash
paniolo serial dtr [--ms 200] [-i console] [target-machine]   # pulse DTR
paniolo serial reset [-i console] [target-machine]             # soft reset (200 ms)
```

> **This is a hardware reset, not a console `reboot`.** To reboot from a
> shell, use `paniolo serial send <target> "reboot"`.

**Errors.** A failed press returns an error, but may have physically pressed
the button. Check the target before retrying.

**Opening the port pulses DTR.** The OS raises DTR on every open (daemon start
and each reconnect), which the daemon cannot suppress. A `serial dtr` press
reuses the open port, so it adds no extra pulse. This is unverified on real
hardware: check with a scope or LED for one pulse, or watch `power_on` in
`paniolo serial show`/`GET /status` settle once.

See [power.md](power.md) for wiring, the power hooks
(`cycle_cmd`/`on_cmd`/`off_cmd`/`state_cmd`), and a command reference.

---

## Runtime paths

| Purpose | Path |
|---|---|
| serialcap discovery | `/tmp/paniolo-<uid>/serialcap/<target>/daemon.json` (`{pid, port, token, interfaces:[...]}`; owner-only, it holds the token) |
| serialcap advisory lock | `/tmp/paniolo-<uid>/serialcap/<target>/daemon.lock` |
| serialcap stderr log | `/tmp/paniolo-<uid>/serialcap/<target>/daemon.log` (truncated on each start; shown on start timeout) |
| Capture log (per interface) | `/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/serial.jsonl(.1..)` |
| Pending (unterminated) line | `/tmp/paniolo-<uid>/serialcap/<target>/capture/<name>/pending.json` |

The runtime base honors `$PANIOLO_RUNTIME_BASE` (default `/tmp`).

---

## HTTP API (serialcap daemon)

Per-interface endpoints take `?interface=NAME` (default: the first interface).

**Every request needs the daemon's token**, from `token` in the discovery
file (new on each start). Send it as `Authorization: Bearer <token>` or
`?token=<token>`. The daemon also requires a loopback `Host` and `Origin`. No
token gets 401; a foreign Host or Origin gets 403.

```bash
d=/tmp/paniolo-$(id -u)/serialcap/target-machine/daemon.json
curl -s -H "Authorization: Bearer $(jq -r .token "$d")" \
    "http://127.0.0.1:$(jq -r .port "$d")/status"
```

A daemon from a paniolo older than the token accepts unauthenticated
requests; `paniolo daemons restart serialcap` replaces it.

`serialcap stop` uses `POST /stop` and never signals the PID. To stop an
older daemon without that endpoint, run `paniolo daemons stop serialcap`.

| Method | Path | Purpose |
|---|---|---|
| POST | `/stop` | Authenticated daemon shutdown; never signals a discovery-file PID |
| GET | `/stream` | Bidirectional WebSocket: serial output (binary) + client keystrokes |
| GET | `/status` | One interface (`?interface=`) or all; `{name, device, baud, connected, power_on}` |
| GET | `/interfaces` | All interfaces and their status |
| GET | `/devices` | Serial devices on the host |
| POST | `/button` | Pulse DTR for `?ms=N`; 503 if assertion or release fails; see [power.md](power.md) |
| POST | `/input` | Write the request body to the port; `?pace_ms=N` drips one byte per N ms |

**`POST /input`:**

- Waits until the driver accepts all bytes.
- The body is capped at 64 KiB and `pace_ms` at 10 000.
- Returns 200 on completion, 400 for a pace past the ceiling, 404 for an
  unknown interface, 413 for an oversized body, and 503 for a disconnected
  interface or interrupted write.
- A failed write may have partly reached the target; the rest is discarded.

`/stream` client messages are also capped at 64 KiB and share the same FIFO
writer.

## Read-only health

`paniolo serial status [target-machine] [-i console]` returns a JSON snapshot
without opening a device, starting a daemon, or sending input. The MCP tool
`serial_status` returns the same JSON.

- `configured` contains the lab's selected interfaces. `active` contains the
  running daemon's interfaces, which may differ after a lab edit.
- `process_live` and `daemon_state` distinguish stopped, untracked, unreachable,
  and reachable daemons. An untracked or unreachable process supplies no port
  or traffic evidence. Missing fields on an older daemon mean unknown.
- Each active interface has `connected` and a `health` object: `epoch`,
  `started_at_ms`, `connection_generation`, `connected_at_ms`, `active_device`,
  `active_baud`, and `identity_source`. Linux resolves the opened file
  descriptor; other platforms report the opened port's name. A null identity
  is unknown. Disconnected interfaces have null active device and baud.
- `last_rx_seq`, `last_rx_at_ms`, `last_rx_age_ms`, `rx_bytes`, and `tx_bytes`
  distinguish a connected but quiet port from receiving traffic. Counters
  exclude paniolo's synthetic connection/button markers. A sequence is a
  received chunk, independent of the line log's sequence. Reconnect increments
  the connection generation; a new supervisor gets a new epoch and counters.
- `capture_through_seq` and `capture_through_at_ms` report bytes processed by
  the capture thread, with `capture_alive` indicating its receive channel.
  This is an assembly watermark, not a guarantee of durable disk writes;
  pending-line sidecars may still be waiting for their debounce deadline.
  `observed_at_ms` is snapshot time, not receive time. Times are Unix milliseconds.

Save a status before input and compare later status in the same epoch. Driver
acceptance (`tx_bytes`) does not prove receipt by the target, and receive
traffic does not prove command execution. Verify the task-specific response
in the captured output. The status command never restarts or retries anything.

## Bounded fresh wait and follow

A receive boundary is an epoch plus an exclusive RX chunk sequence, as defined
in the [glossary](glossary.md). Save `health.epoch` and `health.last_rx_seq` from
`serial status` before input, then give both to `serial wait`. A line-log sequence
is a different counter and must not be used here.

```bash
paniolo serial wait target-machine -i console \
    --epoch capture-example --since 42 --pattern 'test complete' \
    --timeout-ms 30000 --max-bytes 65536 --max-output-bytes 8388608 \
    --include-pending --follow
```

The example boundary must be replaced with the saved status values. Matching
uses a literal UTF-8 string against exact newly acquired bytes and can span
receive chunks; it does not strip ANSI codes or classify test success. No
pattern collects until the deadline or a cap. The command never sends input,
opens the serial port, resets hardware, restarts a daemon, or retries an action.
MCP `serial_wait` exposes the same options and result.

`--timeout-ms` defaults to 30000, is limited to one hour, and uses a monotonic
clock; zero performs one immediate observation. Waiting and status requests
are bounded by the remaining deadline; bounded journal reading, JSON processing
and output add processing overhead. `--max-bytes` limits raw acquisition
(1–1048576 bytes, default 65536). `--max-output-bytes` is an independent hard
limit on total UTF-8 stdout, including every follow event and the final JSON
object (4096–8388608, default 8388608). MCP transport framing is additional.
A whole record that cannot fit is left for a later read: `through_rx_seq` stays
before it, and `next_record_bytes` / `required_output_bytes` explain the cap.

The final `type: result` object always retains accepted immutable `records`,
the receive cursor, `started_at_ms`, `finished_at_ms`, `elapsed_ms`, raw byte
counts, `complete_lines`, diagnostics, and optional final `pending` byte array.
All byte arrays preserve invalid UTF-8 and control bytes. `pending` contains
only bytes acquired after the boundary, not a copied historical prompt. Fault
diagnostics are bounded too; `diagnostics_truncated`, `raw_truncated`,
`raw_length`, and `diagnostic_count` disclose omitted detail. Use `serial evidence`
to replay retained journal fault details with a larger cap.

`status` is an observation outcome, independent of the target task:

| Status | Meaning |
|---|---|
| `matched` | The literal pattern appeared in newly acquired bytes. |
| `timeout` | The finite deadline ended; accepted bytes remain in the result. |
| `cap` | The raw or total-output cap stopped acquisition; the cursor can resume. |
| `disconnected`, `reconnected` | The port is disconnected or its connection generation changed. |
| `daemon_unavailable` | Process discovery, liveness or authenticated status failed. |
| `epoch_changed`, `epoch_missing` | The daemon restarted, or the selected epoch was unavailable/expired. |
| `gap`, `capture_error`, `read_error` | Retention, journal/capture faults, or reading prevented complete evidence. |
| `unsupported` | The daemon lacks the required epoch evidence. |

For a valid observation these statuses return exit 0; inspect `status` in the
JSON. Configuration/argument/helper-launch failures use the normal CLI error
contract. Pattern matches, new receive bytes and a completed wait do not prove
that a target command executed; judge its task-specific response separately.

With `--follow`, each newline-terminated row is delivered as a `type: line`
JSON object before the final result (NDJSON: one JSON object per line). The
first row has `prefix_unknown: true` when a nonzero boundary might start
mid-line. Later complete rows need no partial-row merging. The final pending
bytes are included only with `--include-pending`. The MCP server returns its
collected follow objects after the tool call finishes; it does not stream them
as protocol notifications.
