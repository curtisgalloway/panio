# KB2040 Dual-Board HID Injector

A USB keyboard/mouse (HID) injector for automated testing of a target machine
(e.g. a Raspberry Pi). Two **Adafruit KB2040** boards (RP2040 microcontrollers)
form a **"dumb pipe"**: the host-side `hidrig` tool composes the HID report
bytes in Rust, and the boards relay them to the target without interpreting
them.

| Board | Faces | Link | I2C1 role |
|---|---|---|---|
| **control** | the control host | **USB-CDC** (USB virtual serial) | **controller** |
| **target** | the device under test (DUT) | **USB-HID** | **peripheral** |

The control board also bridges the DUT's **serial console** (its hardware UART)
and switches **DUT power** through a relay (design §6–§7).

This is the **host-side** doc. The design is in
[`../docs/dev/hid-dual-board-design.md`](../docs/dev/hid-dual-board-design.md);
boards, BOM, wiring and firmware are in the
[`paniolo-hardware`](https://github.com/curtisgalloway/paniolo-hardware)
repo (see [Hardware](#hardware) below).

```
[Control host]
      |  USB-CDC (hidrig writes binary HID frames to the data endpoint)
      v
[Control KB2040]  -- routes frames by type byte; UART console bridge; power relay
      |  I2C1:  GP10 = SDA, GP19 = SCL, GND   (target addr 0x41, 4.7 kΩ pull-ups)
      |  UART0: GP0 = TX, GP1 = RX         <--->  DUT serial console
      |  GP5  -> relay / load-switch        --->  DUT power
      v
[Target KB2040]   -- I2C1 peripheral; relays report bytes to send_report
      |  built-in USB (HID keyboard + absolute mouse)
      v
[Target / DUT]
```

`hidrig` turns each command (`type`, `key`, `moveabs`, …) into HID report
bytes in binary frames. The control board relays them over I2C1 to the target
board, which calls `send_report`.

The command vocabulary is the device-independent **HID serial protocol v1**
([`../docs/dev/hid-serial-protocol.md`](../docs/dev/hid-serial-protocol.md)).
Here it is the *external* interface only; the boards see only binary frames.

## Host CLI (`hidrig`)

`hidrig` installs into panio's private libexec dir (off PATH) with
`make install`, or `cargo install --path hidrig --root ~/.local/libexec/panio`.
Run it via `panio helper hidrig …`, or bare inside a `panio hid set --cmd`
hook string.

`-d` is the control board's **data CDC port**: the *second* `usbmodem` of its
pair (the first is the REPL console). On Linux it is the higher-numbered
`/dev/ttyACM*`.

```bash
hidrig -d /dev/cu.usbmodemXXXX ping              # liveness (control frame)
hidrig -d /dev/cu.usbmodemXXXX version           # -> dual-control/1
hidrig -d /dev/cu.usbmodemXXXX type "hello world"
hidrig -d /dev/cu.usbmodemXXXX key ENTER
hidrig -d /dev/cu.usbmodemXXXX combo LEFT_CONTROL C
hidrig -d /dev/cu.usbmodemXXXX move 300 -50       # relative
hidrig -d /dev/cu.usbmodemXXXX moveabs 16383 16383 # absolute (0..32767 logical; center)
hidrig -d /dev/cu.usbmodemXXXX click right
hidrig -d /dev/cu.usbmodemXXXX scroll -3
hidrig -d /dev/cu.usbmodemXXXX power cycle         # DUT power off/on via the relay
hidrig -d /dev/cu.usbmodemXXXX power off           # off | on | cycle [secs]
hidrig -d /dev/cu.usbmodemXXXX run boot-seq.txt   # command file; '-' = stdin
```

- **`moveabs`** takes `0..32767` across the full screen, so
  `moveabs 16383 16383` is dead center.
- **Key names** are `adafruit_hid` Keycode names (`A`–`Z`, `ENTER`, `TAB`,
  `ESCAPE`, `LEFT_CONTROL`, `LEFT_SHIFT`, `UP_ARROW`, `F1`–`F12`, …).
- **Command files** take one command per line. Blank lines and `# comments` are
  skipped; `delay <ms>` / `sleep <seconds>` pause between commands.

### Daemon mode (`serve`) — the KVM path

The control link has only one owner. `hidrig serve` owns it, holds the
composition state (held keys, virtual cursor), and serves the command
vocabulary on a localhost WebSocket (`GET /hid`) plus `POST /send`.

```bash
hidrig -d /dev/cu.usbmodemXXXX serve             # owns the link, runs until stopped
hidrig -d /dev/cu.usbmodemXXXX type hi           # auto-routes through the daemon
hidrig stop                                      # stop the daemon
```

While a daemon runs, every `hidrig -d <device> …` one-shot routes through it
(over `POST /send`). `panio console` starts the daemon on demand (see
[`../docs/hid.md`](../docs/hid.md)).

Every request needs the token the daemon publishes in its discovery file
(`Authorization: Bearer <token>` or `?token=<token>`), from a loopback
`Host`/`Origin` only. One-shots and `panio console` supply it themselves.

> The control board is a **USB-CDC** device, so there is **no baud
> negotiation**; the nominal "baud" is ignored.

**Replies are demultiplexed.** A control command's `0x02` reply (`power cycle`,
`ping`) shares the CDC stream with the DUT's `0x03` console output, so replies
arrive correctly even while the DUT is booting.

**Every open resyncs.** Every open of the control link first writes a 258-byte
all-zero resync preamble, which completes any frame a killed previous owner left
half-written (design doc §5).

**Shutdown releases everything.** Graceful shutdown (`SIGTERM`/Ctrl-C) releases
every held key, modifier and mouse button, bounded by a short timeout.

### DUT power and serial console

**Power.** `hidrig power off|on|cycle [secs]` switches the DUT through the
relay; use it as a normal panio `power` hook. A `cycle` acks immediately, then
holds power off for the given seconds (firmware default 2 s).

- The relay **state persists across a control-board reset** (stored in NVM;
  default on if unset).
- Power and console work even when the target/HID side (and its pull-ups) isn't
  wired or is powered off.

**Console.** The `serve` daemon also bridges the DUT's serial console (control
board UART0) and **re-exports it as a PTY**, so panio's `serial` channel
attaches to it. The console exists only while the daemon runs; any
`panio hid …` or `panio console` for the target starts it.

Point the lab file's `serial` channel `device =` at the daemon's stable symlink,
`/tmp/panio-<uid>/hid/console`:

```toml
[[targets.pi5.serial]]
name   = "console"
device = "/tmp/panio-501/hid/console"   # the hidrig daemon's console PTY
baud   = 115200                            # nominal; the UART rate is fixed in firmware
# no power_sense_signal — a PTY has no modem-control lines (use `hidrig power` instead)
```

Then `panio serial watch/connect/send/log` work as usual. A PTY has no
DTR/CTS, so `serial dtr`/`reset` and `power_sense_signal` don't apply.

If the symlink can't be created (e.g. a stale non-symlink is there), `console`
in `daemon.json` falls back to the underlying device. Programs should read
`console` (whichever is live) and `console_device` (the real PTY slave, e.g.
`/dev/pts/7`) from `daemon.json` instead of assuming the symlink.

> **Status:** the **relay/power** path is hardware-verified. The **console
> bridge** is **not yet** verified; confirm the PTY round trip through
> `tio`/serialcap before relying on it (design §6).

## panio integration

panio calls the tool through the per-target `hid` channel, a command prefix
like the power hooks.

```bash
panio hid set -t pi5 --cmd "hidrig -d /dev/cu.usbmodemXXXX"
panio hid send -t pi5 type hello
panio hid send -t pi5 key ENTER
```

`panio hid send` appends its arguments to the command and runs it on the
control host that owns the channel. See [`../docs/hid.md`](../docs/hid.md).

## Wire protocol (host ↔ rig)

Both legs (CDC to the control board, I2C1 to the target) use one frame format:

```
[type][b1][len][payload .. len bytes]
  0x01  rid  N   N HID report bytes   (rid 1 = keyboard / 8 B, 2 = abs mouse / 6 B)
  0x02  cmd  N   N arg bytes          (cmd 1 = ping, 2 = version, 3 = power)
  0x03  port N   N raw console bytes  (DUT serial console, both directions)
```

- **HID frames (`0x01`) are fire-and-forget**, with no per-frame ack. `hidrig`
  paces them to the downstream USB poll interval (`bInterval`).
- **Control frames (`0x02`) are request/reply.** `ping`, `version`, and
  `power` draw a `[0x02][cmd][len][payload]` reply. `version` returns the
  control board's implementation id (`dual-control/1`). `power` acks before
  acting (a `power cycle` blocks the board only for its off-time).
- **Console frames (`0x03`) are a fire-and-forget bidirectional byte pipe** to
  the DUT's serial console (see
  [DUT power and serial console](#dut-power-and-serial-console)).

**The descriptor is the contract.** The host composer must match the target
board's HID **descriptor** exactly (report IDs, field order, the 0..32767
absolute range). That descriptor lives in
[`hidrig-kb2040/firmware/target/boot.py`](https://github.com/curtisgalloway/paniolo-hardware/blob/main/hidrig-kb2040/firmware/target/boot.py)
in the paniolo-hardware repo. A change to it there requires a matching change
to `src/compose.rs` here, which holds the composition and framing.

## Hardware

The boards, BOM, wiring and firmware flash runbook are in
[`paniolo-hardware`](https://github.com/curtisgalloway/paniolo-hardware), under
[`hidrig-kb2040/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040).
See [`hidrig-kb2040/README.md`](https://github.com/curtisgalloway/paniolo-hardware/blob/main/hidrig-kb2040/README.md)
for the full build and
[`hidrig-kb2040/SETUP.md`](https://github.com/curtisgalloway/paniolo-hardware/blob/main/hidrig-kb2040/SETUP.md)
for the CircuitPython flash runbook. `hidrig/` here is the host side only.

In brief: two **Adafruit KB2040** boards joined by **I2C1** (`GP10` = SDA,
`GP19` = SCL, common GND, with required ~4.7 kΩ pull-ups), the target board at
I2C address **0x41**. The control board also carries **UART0** (`GP0` = TX,
`GP1` = RX) to the DUT's serial console and **`GP5`** driving the DUT power
relay. See [Wire protocol](#wire-protocol-host--rig) for the descriptor contract
and [DUT power and serial console](#dut-power-and-serial-console) for how those
two GPIOs surface through `hidrig`.

## Host testing tools (macOS)

To test end to end, plug the **target** board into the Mac that drives the
control link and capture its HID reports while you inject. Build with
`cd hidrig/host && make`; details in `host/README.md`.

**`host/hid_capture_usb.m` — leak-safe capture (use this one).** It takes the
board away from macOS via IOUSBHost whole-device capture
(`IOUSBHostObjectInitOptionsDeviceCapture`, needs root) and prints each report
with timestamps. Injected input reaches **only** this tool.

```bash
sudo ./hid_capture_usb            # defaults to the injector serial
sudo ./hid_capture_usb <serial>   # if more than one KB2040 is attached
```

Then run `hidrig -d /dev/cu.usbmodemXXXX moveabs 16383 16383` in another
terminal. **Start the capture tool before injecting**, or the reports leak into
your live session.

> `host/hid_seize_reports.c` (`IOHIDDeviceOpen(..SeizeDevice)`) is
> **non-exclusive** on Darwin 24/25: injected moves still move the real cursor.
> Use it only as a passive raw-report tap.

`host/hid_bench.py` (latency/throughput) and `host/leak_check.py` (cursor leak)
run via `uv run --with pyserial …`, but speak the **retired single-board
firmware's** line protocol and don't drive the dual-board rig.

### macOS serial latency

The host sets the macOS serial read-latency timer (`IOSSDATALAT`) to its floor
on open (`proto.rs`); the default adds ~230 ms to a `ping`/`version` round trip.
A mouse move injects in ~8 ms (the target's 8 ms `bInterval`).

## Files

```
hidrig/
  src/main.rs       # `hidrig` CLI: one-shots, `run`, `serve`/`stop`
  src/compose.rs    # HID composition: command vocabulary -> report bytes -> binary frames
  src/proto.rs      # control-link transport (binary frames over CDC) + command-file parser
  src/uart.rs       # daemon: the single control-link owner (HID/control + console demux)
  src/pty.rs        # daemon: PTY that re-exports the DUT console into panio's serial channel
  src/server.rs     # daemon: axum WebSocket /hid + POST /send
  src/daemon.rs     # daemon: lock, discovery file, console PTY symlink, lifecycle
  host/hid_capture_usb.m   # macOS leak-safe HID capture (IOUSBHost device-capture)
  host/hid_seize_reports.c # macOS passive raw-report tap (non-exclusive on Darwin 24/25)
  host/hid_bench.py        # latency/throughput bench
  host/leak_check.py       # asserts injection does not leak to the live session
  host/Makefile
  host/README.md
```

## History

The first dual-board version (`hidrig/control/`, `hidrig/target/` in git
history) parsed commands on the boards; a later single-board rig composed HID
with `adafruit_hid` in CircuitPython. The current design moved composition to
`src/compose.rs` (design §7). The retired single-board firmware is in
paniolo-hardware under
[`hidrig-kb2040/firmware/single-board/`](https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware/single-board).
