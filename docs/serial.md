# Serial console

**Terms:** a capture epoch identifies one interface supervisor's lifetime;
RX is bytes received from the target, TX is bytes accepted by the serial driver;
a connection generation counts successful opens within an epoch.
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

**Stale daemons.** A daemon running an old binary after an upgrade shows as
**stale** in `paniolo serial show` and `paniolo daemons`. `paniolo serial
watch` restarts it automatically, or run `paniolo daemons restart serialcap`
(see [architecture](dev/architecture.md)).

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
requests; `paniolo daemons restart --stale` replaces it.

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
