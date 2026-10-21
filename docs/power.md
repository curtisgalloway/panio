# Power control

**Terms:** a **hook** is a configured shell command; **AMT/ME** is Intel's
management firmware; **TLS/HTTPS** encrypts traffic and verifies the server;
a **CA** signs certificates; **PEM** is their text file format; **SAN** lists
a certificate's valid names or addresses. See the [glossary](glossary.md).

panio switches a target's power two ways:

- **DTR via FTDI**: the serial cable's DTR line drives the target's J2
  power-button header. Wiring only. **Opt-in per serial interface**
  (`power_button = true`): `serial dtr` / `serial reset` refuse an interface
  that has not declared it.
- **Generic power hooks**: four optional shell commands (`on_cmd`, `off_cmd`,
  `cycle_cmd`, `state_cmd`), set with `panio power set` and run via `sh -c`.
  Use them for a smart plug, a USB hub, Intel AMT, or a script. A target with
  more than one power layer can add named
  [power levels](#power-levels-soft-and-hard) (`soft`, `hard`) beside them.

Device-specific logic never goes in the core crates; it lives in helper
binaries behind the hooks (`cambrionix` is the canonical example). To support
new hardware, follow the [power-helper recipe](dev/adding-power-helpers.md).

---

## DTR power control (FTDI J2 wiring)

DTR (a serial adapter control output), wired to J2, "presses" the Raspberry
Pi 5's power button.

### Hardware wiring (Raspberry Pi 5)

```
FTDI DTR  →  1 kΩ  →  Pi J2 Pin 1 (PMIC_POW_BUTTON, pull-up inside DA9091)
FTDI GND  ←─────────  Pi J2 Pin 2
```

Optional power sense — reads whether the Pi is on:

```
Pi 3.3 V (header Pin 1)  →  1 kΩ  →  FTDI CTS# (or DSR#/DCD#/RI#)
                                             │
                                          10 kΩ
                                             │
                                            GND
```

The same FTDI adapter carries the target's serial console.

### Setup

```bash
# Add a serial interface. --power-button declares that this interface's DTR line
# is wired to the J2 power button (this is what enables `serial dtr`/`reset`);
# --sense records the modem-control input wired for power sensing (optional).
panio serial add console -t target-machine \
    --device /dev/tty.usbserial-0001 \
    --baud 115200 \
    --power-button \
    --sense cts

# Only needed when a target has MORE THAN ONE power_button interface: pick which
# one DTR commands default to.
panio power set -t target-machine --serial-interface console
```

To enable or revoke DTR on an existing interface:

```bash
panio serial set console -t target-machine --power-button         # enable
panio serial set console -t target-machine --power-button false   # revoke
```

### DTR commands

```bash
# Pulse DTR on the default power serial interface (200 ms)
panio serial dtr [target-machine]

# Explicit duration — short press signals the OS, long press hard-powers off
panio serial dtr --ms 200 [target-machine]   # soft press
panio serial dtr --ms 4000 [target-machine]  # hard power-off (PMIC)

# Target a specific interface with -i
panio serial dtr -i bmc --ms 200 [target-machine]

# Soft reset (convenience alias for a brief DTR pulse)
panio serial reset [target-machine]
panio serial reset -i console --ms 500 [target-machine]

# Show whether the target is powered on (requires sense signal + daemon running)
panio power-state [target-machine]
```

| Press duration | Effect |
|---|---|
| ≤ 500 ms | Soft power-button event — OS responds (graceful reboot or halt) |
| ≥ 3000 ms | Hard PMIC power-off (equivalent to holding the physical button) |

**Which interface is pressed.** `serial dtr` and `serial reset` pick, in order:

1. an explicit `-i`;
2. the power channel's `serial_interface`;
3. the only interface with `power_button = true`.

If the chosen interface lacks `power_button = true`, or none has it, the
command **errors** with a hint. It never falls back to a lone, possibly unwired
console.

**If a DTR press fails** (when serialcap handles DTR), the command fails and
the daemon reconnects with DTR deasserted. Check the target before retrying:
the physical press may have happened.

> **"Reboot over serial" is not a DTR reset.** `panio serial send <target>
> "reboot"` is a *software* reboot at a logged-in console. `serial reset` /
> `serial dtr` toggle the J2 wiring. When a request says "use serial to
> reboot" without naming DTR or the power button, use the console `reboot` (or
> `panio power-cycle`), not DTR.

---

## Generic power hooks

All four hooks are optional and independent:

```bash
panio power set -t <target> \
    [--cycle-cmd <cmd>]   \   # panio power-cycle
    [--on-cmd    <cmd>]   \   # panio power on
    [--off-cmd   <cmd>]   \   # panio power off
    [--state-cmd <cmd>]   \   # panio power-state (stdout: "on" or "off")
    [--serial-interface <name>]   # default DTR interface when several opt in
    [--host <labhost>]
```

Each hook runs via `sh -c <cmd>`; its exit code decides success. Ready-made
helpers:

| Helper | Switches | Section |
|---|---|---|
| `cambrionix` | A port on a Cambrionix USB hub | [Cambrionix hub control](#cambrionix-hub-control) |
| `zigplug` | A Zigbee smart plug | [Zigbee smart plug control](#zigbee-smart-plug-control-zigplug) |
| `shellyplug` | A Shelly Wi-Fi plug or relay | [Shelly smart plug control](#shelly-smart-plug-control-shellyplug) |
| `amt` | An Intel AMT (vPro) machine's own power | [Intel AMT power control](#intel-amt-power-control-amt) |

Helpers install into the private libexec dir (`~/.local/libexec/panio/bin`),
not onto `PATH`. Hook strings name them bare; panio looks in libexec first.
To run one by hand, use `panio helper <name> …`. List installed helpers
with `panio helper`, then read a helper's own help with
`panio helper amt --help`. Everything after the helper name, `--help`
included, goes to the helper; a `--` before the name is accepted but not
needed. Listing helpers and reading bundled helper help do not open hardware
or start a daemon.

A helper runs on the local machine unless you say where the hardware is.
Put these flags **before** the helper name:

```bash
panio helper -t target-machine --channel power cambrionix -d /dev/ttyUSB0 state
panio helper --host bench1 zigplug -d /dev/ttyUSB1 list
```

`-t T --channel K` runs on the host the lab binds that target's channel to
(`-i NAME` picks a serial interface when they span hosts); `-t T` alone
works when all of the target's channels share one host; `--host H` names a
lab host. It re-execs `panio helper` on that host over SSH, so the helper
gets that host's own state and runtime dirs, as a hook would.

### Out-of-tree helpers and skills (plugin roots)

A helper or skill panio does not ship (a lab's own board-control tool, a
power script, a skill describing one lab's targets) can live in its own
checkout instead of being copied into `~/.local`. Point
`PANIO_PLUGIN_PATH` at one or more **plugin roots**: install prefixes laid
out like panio's own.

```text
/opt/lab-plugins/
  libexec/panio/bin/labctl                 # a helper
  share/panio/skills/lab-targets/SKILL.md  # a skill
```

```bash
export PANIO_PLUGIN_PATH=/opt/lab-plugins:/srv/more-plugins   # ';' on Windows
panio helper                 # labctl<TAB>plugin: /opt/lab-plugins
panio helper labctl status
panio skill lab-targets
panio --plugin-path /opt/lab-plugins helper   # one command, without the variable
```

- **Plugins add; they never shadow.** panio searches its own helper dirs
  first and the plugin roots after them, in list order, then `PATH`. A plugin
  helper or skill with the same name as a bundled one is never used, so a
  daemon always runs the binary panio installed. There is no setting to
  change that; give the plugin a different name.
- **Hooks name plugin helpers bare.** Hook commands (`*_cmd`, the hid and usb
  `cmd`, actions) run with the plugin helper dirs on `PATH` after the bundled
  ones, so `--cycle-cmd "labctl cycle"` works.
- **Listing shows the source.** `panio helper` and `panio skill` print a
  bundled entry as its bare name and a plugin entry as the name, a tab, and
  `plugin: <root>`. The name is always the first field.
- **Empty entries are skipped; relative ones are ignored** with a warning on
  stderr, since they would resolve against whatever directory panio started
  in.
- **`panio doctor`** prints one `plugins` line per root on this machine: `ok`,
  `MISSING` (counted as a problem), or `incomplete` (no `libexec/panio/bin`
  or `share/panio/skills` inside, or a relative entry). It also names a
  plugin helper that a bundled helper shadows.
- **The variable is read on the host that runs the command.** For a remote
  control host, set the host's `plugin_path` in the lab file (`panio host set
  bench1 --plugin-path /opt/lab-plugins`); see
  [Distributed control](distributed-control.md#plugin-roots-on-a-control-host).
- `panio daemons` does not treat a process running out of a plugin root as
  a stray, so `daemons stop --all` leaves it alone.

The `hidrig` control board can also switch a DUT power relay
(`hidrig power off|on|cycle`) behind these hooks (see
[`hidrig/README.md`](https://github.com/curtisgalloway/panio/blob/main/hidrig/README.md)).

### Commands backed by hooks

```bash
panio power on  [target]        # run on_cmd; error with config hint when unset
panio power off [target]        # run off_cmd; error with config hint when unset
panio power-cycle [target]      # run cycle_cmd
panio power-state [target]      # state_cmd if set; else serial sense-line
```

**`power-cycle`** runs `cycle_cmd` with no built-in timing; the script owns the
sequence. A script may return before the cycle ends (a relay board that times
the off period itself), so when `state_cmd` is set, `power-cycle` then reads it
every 500 ms until it reports `on` and only then prints `Power cycle complete`.
If it still reports `off` after 30 s, panio exits 22 (`timeout`, outcome
unknown). Without `state_cmd` it cannot check, and prints `Power cycle
requested` instead. A failing script makes panio exit 101 (`helper_failed`), with the
script's code as `child_exit` in the `--json-errors` object. A missing or
non-executable script (shell code 127/126) exits 3 (`not_configured`). See
[Exit status and errors](errors.md).

**`power-state`** reads the first whitespace-delimited token of `state_cmd`'s
stdout, which must be `on` or `off` (case-insensitive). Without `state_cmd`, it
uses the serial sense line (sense wired, serialcap daemon running).

### Power levels (soft and hard)

Some targets have more than one independent power layer, and a cycle at one
does not reach the others:

- a **soft** layer: a PMIC enable, a board-control MCU switching the main
  rail, or the power button;
- a **hard** layer: mains or DC input, the only reset for a wedged
  controller.

A debug cable that back-powers the board is the usual trap. ChromeOS Servo v4
and v4.1 can source power through their DUT-side USB-C port, so cutting the
barrel jack alone does not power the board off; a real hard cycle has to drop
both, for example with a smart plug and a Cambrionix hub port.

A **power level** is a named set of hooks beside the default ones. Each level
may set any of `cycle_cmd`, `on_cmd`, `off_cmd` and `state_cmd`:

```toml
[targets.target-machine.power]            # the default level, as before
cycle_cmd = "boardctl -d /dev/ttyUSB0 cycle"
state_cmd = "boardctl -d /dev/ttyUSB0 state"

[targets.target-machine.power.levels.hard]
cycle_cmd = "sh -c 'shellyplug -d 192.0.2.10 off && cambrionix -d /dev/ttyUSB1 off 3 && sleep 5 && cambrionix -d /dev/ttyUSB1 on 3 && shellyplug -d 192.0.2.10 on'"
state_cmd = "shellyplug -d 192.0.2.10 state"
```

Set and remove levels with the CLI, which keeps the rest of the file and its
comments as they are:

```bash
panio power set -t target-machine --level hard \
    --cycle-cmd "~/scripts/hard-cycle.sh" --state-cmd "shellyplug -d 192.0.2.10 state"
panio power rm -t target-machine --level hard   # only that level
panio power rm -t target-machine                # the whole channel, levels included
```

Then pick a level with `--level` on any power verb:

```bash
panio power-cycle target-machine --level hard
panio power on    target-machine --level hard
panio power off   target-machine --level hard
panio power-state target-machine --level hard
```

Rules:

- **No `--level` means the default level**, so labs without levels and the
  scripts that drive them behave exactly as before. `--level default` names
  the default level explicitly, which is why no level may be called
  `default`.
- **A level never borrows a hook from the default level.** If `hard` has no
  `on_cmd`, `power on --level hard` fails with exit 3 (`not_configured`)
  naming the level, rather than running the default `on_cmd`. An unknown
  level fails the same way and lists the levels the target has.
- **`power-cycle --level L` confirms with L's own `state_cmd`**, the same
  500 ms / 30 s wait as the default level. Without one it prints `Power cycle
  requested at level 'L'`.
- **`power-state` without `--level`** prints one line per level when the
  target has levels, the default level first; with none, it prints the single
  line it always has:

  ```text
  Power ON  (target-machine)
  Power OFF  (target-machine, level hard)
  Power UNKNOWN  (target-machine, level pmic: no state_cmd)
  ```

  A level that has no way to read state is reported as `UNKNOWN … no
  state_cmd`, not as an error. A level whose `state_cmd` fails is reported as
  `UNKNOWN … state read failed`, and the command then exits with that
  failure (101, `helper_failed`) after printing every line. With
  `--level NAME` only that level's line is printed.
- Level names follow the same rule as target names: letters, digits, `.`,
  `_` and `-`, not starting with `-`. Each level needs at least one hook.
- Every level runs on the power channel's host (`--host` is set for the whole
  channel, without `--level`), and every level's hooks take the same
  per-target hook lock as the default hooks and the
  [named actions](actions.md), so a soft and a hard cycle never overlap.
- **The dashboard's power controls drive the default level only**, as does
  an action's `requires_power` check. Use the CLI or the MCP tools for the
  others.

### `panio doctor` hook probing

`panio doctor` probes every hook, over SSH for remote hosts: `test -e` for an
absolute path, `command -v` for a bare name, in the runtime lookup order:

1. the per-user libexec dir (`~/.local/libexec/panio/bin`);
2. the system package dir (`/usr/libexec/panio/bin`);
3. each plugin root's `libexec/panio/bin` (for a remote host, its lab
   `plugin_path`, else its own `PANIO_PLUGIN_PATH`; see
   [plugin roots](#out-of-tree-helpers-and-skills-plugin-roots));
4. `PATH`.

It lists configured hooks by name, e.g. `cycle_cmd,on_cmd,off_cmd,state_cmd`.
Each [power level](#power-levels-soft-and-hard) is probed the same way and
listed after the default hooks (`cycle_cmd,state_cmd; level hard:
cycle_cmd,state_cmd`); a missing program is reported with its level, as
`level 'hard': …`.

### Helper health checks (`--doctor`)

That probe proves the helper is installed, not that the plug or hub answers.
A helper with a `doctor` subcommand can check the hardware too, read-only.
Opt the channel in:

```bash
panio power set -t target-machine --doctor      # --no-doctor turns it off
```

`panio doctor` then runs the hook's program with the hook's own transport
options and ` doctor --json` in place of the verb. For
`shellyplug -d 192.0.2.10 cycle 0` that is
`shellyplug -d 192.0.2.10 doctor --json`. The check runs once per distinct
command, has 10 s to finish, and each result is listed under the channel:

```
target-machine  power doctor: reachable  @bench1  ok    SNPL-00112EU …, gen 2, firmware 1.4.4
target-machine  power doctor: auth       @bench1  ok    authentication disabled
target-machine  power doctor: switch     @bench1  ok    switch:0 present
```

A `fail` (or a check that times out) is a problem and makes `doctor` exit 1;
a `warn` is shown but is not. A helper without a `doctor` subcommand shows
once as `no-check … helper has no doctor check` and is not a problem. Of the
bundled power helpers, `shellyplug` implements it. Only opt in a hook whose
program implements the check: a script that ignores its arguments would run
its normal action. The derivation rule and the JSON contract are in
[Adding power helpers](dev/adding-power-helpers.md#doctor-check-optional-recommended).
The same `--doctor` flag exists on `hid set`, `usb set`, and `action set`.

The flag belongs to the whole power channel, like `--host`, and also covers
every named power level: a level's hooks are asked the same way, once per
distinct command, so a level on the same transport as another hook folds into
that run. When there is more than one run, each line names the hooks it came
from; here a `hard` level on another helper gets its own:

```
target-machine  power doctor (default): reachable  @bench1  ok        SNPL-00112EU …
target-machine  power doctor (level hard)          @bench1  no-check  helper has no doctor check (relayctl)
```

### Example: Home Assistant script (cycle_cmd)

```bash
panio power set -t target-machine \
    --cycle-cmd /Users/you/.config/panio/scripts/power-cycle-target-machine.sh
```

```bash
#!/usr/bin/env bash
set -euo pipefail
HA_URL="http://homeassistant.local:8123"
ENTITY="switch.pi_power_strip"
TOKEN="${HA_TOKEN:?HA_TOKEN not set}"

curl -sf -X POST "$HA_URL/api/services/switch/turn_off" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"entity_id\": \"$ENTITY\"}"

sleep 10

curl -sf -X POST "$HA_URL/api/services/switch/turn_on" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"entity_id\": \"$ENTITY\"}"
```

Never hardcode `HA_TOKEN` in the script or the panio config. Inject it at
call time:

```bash
# 1Password CLI (op): reads secrets from a .env file or vault and injects them
#    .env file format:  HA_TOKEN=op://vault/item/field
op run --env-file .env -- panio power-cycle target-machine

# direnv: place "export HA_TOKEN=..." in an .envrc in your working directory;
#    direnv loads it automatically when you cd there
panio power-cycle target-machine   # HA_TOKEN already in environment via direnv

# Inline export (quick/manual use — clears from shell history if prefixed with space)
HA_TOKEN="$(cat ~/.secrets/ha_token)" panio power-cycle target-machine

# SSH with env forwarding (when running from a remote agent host)
ssh -o SendEnv=HA_TOKEN control-mac "panio power-cycle target-machine"
# (requires AcceptEnv HA_TOKEN in sshd_config on control-mac)
```

---

## Cambrionix hub control

The `cambrionix` helper drives a Cambrionix USB hub's control UART: 115200
8N1, `>>` prompt, commands `mode c|s|o <port>` and `state`.

### Installation

`make install` / `panio setup` installs it into the private libexec dir. Run
it by hand via `panio helper cambrionix …`.

### Commands

```bash
cambrionix -d <device> state              # table of all ports (volts, mA, attach/mode)
cambrionix -d <device> state <port>       # print exactly "on" or "off" (state_cmd contract)
cambrionix -d <device> on <port>          # mode c (charging/on), confirm by read-back
cambrionix -d <device> off <port>         # mode o (off), confirm by read-back
cambrionix -d <device> cycle <port> [--delay-ms 3000]
                                          # off → confirm → delay → restore prior mode → confirm
```

- **Ports:** 1–15. Port 0 is the hub's own row, read-only.
- **`cycle`** restores Sync (`s`) if the port was Sync, otherwise charging
  (`c`).
- **Every change is confirmed by re-reading the port table**, since the hub
  does not acknowledge `mode`. `on` requires a powered state, `off` requires
  `O`; a mismatch exits non-zero.
- **`state <port>`** reads the second flag of the PDSync port table, which
  is case-sensitive: `I` (idle), `C` (charging), `F` (finished charging: the
  device stopped drawing current), `c` (power on, no device) and `S` (host
  port) are `on`; `O` is `off`; any other letter is an error.
- **`off` and `cycle` switch VBUS only, not the data lines.** On a
  PDSync-4, D+/D- stayed connected while a port was off (Cambrionix's mode
  description says otherwise), so a USB device that does not sense VBUS (a
  board whose VBUS-detect input is strapped valid, a self-powered device)
  stays enumerated on its host through a whole cycle. The hub's `mode`
  command (`c`, `s`, `b`, `o`) has no data-only disconnect, so the helper
  offers none. A test that needs a real re-enumeration must use a device
  that drops its pull-up on VBUS loss, or a physical unplug.
- **Bounded responses:** 3 s and 64 KiB each; a hub error line fails the
  command, so a wrong `-d` fails fast instead of hanging the hook.

### Wiring into panio power hooks

Example: a Raspberry Pi 5 on hub port 4, hub UART at
`/dev/cu.usbserial-AA00BB11`:

```bash
panio power set -t pi5 \
    --cycle-cmd "cambrionix -d /dev/cu.usbserial-AA00BB11 cycle 4" \
    --on-cmd    "cambrionix -d /dev/cu.usbserial-AA00BB11 on 4" \
    --off-cmd   "cambrionix -d /dev/cu.usbserial-AA00BB11 off 4" \
    --state-cmd "cambrionix -d /dev/cu.usbserial-AA00BB11 state 4"
```

`panio power on pi5`, `panio power off pi5`, `panio power-cycle pi5`, and
`panio power-state pi5` then work.

---

## Zigbee smart plug control (zigplug)

The `zigplug` helper switches Zigbee smart plugs through a CC2652-based
coordinator dongle (e.g. Sonoff ZBDongle-P), using
[zigpy-znp](https://github.com/zigpy/zigpy-znp).

Device data lives in a sqlite DB at
`~/.config/panio/helpers/zigplug/zigbee.db` (`--db` to override). A DB at the
pre-0.3 location is migrated automatically.

**Operations run through a persistent daemon** that the CLI starts on first
use, so hook strings stay one-shot. Opening the serial port resets the chip
(sometimes into its bootloader), and concurrent one-shot sessions wedge the
coordinator, which can lose its network NVRAM. The daemon opens the port once,
serializes operations, and gives each a hard timeout.

It follows the standard daemon contract
(`/tmp/panio-<uid>/zigplug/daemon.json`, localhost HTTP, OS-assigned port)
and shows in `panio daemons`. Every request must present the per-run bearer
token in the discovery file (mode 0600).

Manual control: `zigplug serve` / `stop` / `status` (`stop` and `status` need
no `-d`). `--no-daemon` forces the direct path, for debugging only.

### Installation

`panio setup` / `make install` installs `zigplug/` as a uv tool when `uv` is
on `PATH`, with its shim in the private libexec dir. Run it by hand via
`panio helper zigplug …`.

```bash
# manual equivalent
UV_TOOL_BIN_DIR=~/.local/libexec/panio/bin uv tool install --force ~/src/panio/zigplug
```

### One-time setup: form the network

```bash
panio helper zigplug -d /dev/cu.usbserial-XXXX form              # channel picked by energy scan
panio helper zigplug -d /dev/cu.usbserial-XXXX form --channel 25 # or explicit (25-26 avoid Wi-Fi)
```

`form` is idempotent: an existing network prints its channel/PAN and exits.

**If formation fails with "too much RF interference":** put the dongle on a
USB 2.0 extension cable, away from USB 3.x ports, hubs and video-capture
devices. To factory-reset stale dongle state, run
`python -m zigpy_znp.tools.nvram_reset <device>` from the `zigplug/` venv.

### Pairing plugs

```bash
panio helper zigplug -d <device> permit --time 120   # open a join window
# put the plug in pairing mode (hold button until LED blinks; factory-fresh
# plugs usually enter pairing mode on first power-up)
panio helper zigplug -d <device> list                # IEEE, NWK, manufacturer, model, state
```

`permit` prints each join and exits non-zero if nothing paired. A plug paired
to another hub needs a full factory reset (often a ~10 s button hold).

### Commands

As hook strings (or after `panio helper` when run by hand):

```bash
zigplug -d <device> list                  # table of joined plugs + live state
zigplug -d <device> state <ieee>          # print exactly "on" or "off" (state_cmd contract)
zigplug -d <device> on <ieee>             # switch on, confirm by read-back
zigplug -d <device> off <ieee>            # switch off, confirm by read-back
zigplug -d <device> cycle <ieee> [--delay-ms 3000]
                                          # off → delay → on → confirm
zigplug -d <device> remove <ieee>         # unpair (ZDO leave + forget)
zigplug -d <device> serve                 # start the daemon by hand (automatic otherwise)
zigplug stop                              # stop the daemon (no -d: one daemon per host)
zigplug status                            # daemon + network status (no -d)
zigplug -d <device> backup [-o FILE]      # network backup (key, counters) as JSON
zigplug -d <device> restore [-i FILE]     # write a backup into coordinator NVRAM
```

IEEE addresses are accepted with or without `:`/`-` separators.

### Coordinator NVRAM recovery (backup/restore)

The device DB holds an automatic network backup. If a formed dongle reports
`coordinator has no Zigbee network`, recover **without re-pairing**:

```bash
panio helper zigplug -d <device> stop      # restore needs the port exclusively
panio helper zigplug -d <device> restore   # newest auto-backup from zigbee.db
panio helper zigplug -d <device> list      # verify the plugs answer
```

`restore` bumps the frame counter (`--counter-increment`, default 10000) so
joined devices accept it. A long-orphaned plug may not answer until it
rescans; power-cycling it at the wall forces a rejoin (and cycles its load).
Keep an off-host copy with `zigplug backup -o <file>`.

### Wiring into panio power hooks

```bash
panio power set -t target-machine \
    --cycle-cmd "zigplug -d /dev/cu.usbserial-XXXX cycle ff:ff:b4:0e:06:04:ea:b7" \
    --on-cmd    "zigplug -d /dev/cu.usbserial-XXXX on    ff:ff:b4:0e:06:04:ea:b7" \
    --off-cmd   "zigplug -d /dev/cu.usbserial-XXXX off   ff:ff:b4:0e:06:04:ea:b7" \
    --state-cmd "zigplug -d /dev/cu.usbserial-XXXX state ff:ff:b4:0e:06:04:ea:b7"
```

The first hook starts the daemon (a few seconds); later ones answer in about a
second and queue safely. `form`, `restore`, and `backup` (with no daemon
running) open the port directly and refuse to run while the daemon is up — run
`zigplug stop` first.

## Shelly smart plug control (shellyplug)

The `shellyplug` helper switches **Shelly Gen2+ smart plugs and relays** (Plus,
Pro, Gen3, Gen4) over the device's **local HTTP RPC API** — no cloud, Home
Assistant, or Matter. Pure Rust ([ureq](https://crates.io/crates/ureq)), no
daemon: each invocation makes one `GET /rpc/<Method>` call.

- **Supported:** Gen2/3/4 JSON-RPC (`Switch.Set`, `Switch.GetStatus`,
  `Shelly.GetDeviceInfo`). Gen1's REST API (`/relay/0?turn=on`) is **not**
  supported.
- **Auth:** only devices with authentication **disabled** (`auth_en: false`,
  the factory default). An auth-enabled device answers HTTP 401 and the helper
  says so.

### Installation

`make install` / `panio setup` installs it into the private libexec dir. Run
it by hand via `panio helper shellyplug …`.

### Addressing

- **`-d <host>`**: a bare IP or hostname (`10.0.0.5`, `shelly.local`),
  optionally with a scheme or port (`http://10.0.0.5:8080`). Use a **DHCP
  reservation** or the device's mDNS name (`shellyplugusg4-<mac>.local`) so a
  lease change does not break the hook.
- **`[id]`**: switch component id, default `0`. Multi-channel devices (e.g. a
  Pro 4PM) use `0..N`.

### Commands

```bash
shellyplug -d <host> status [id]          # device info + switch state and power metering
shellyplug -d <host> state  [id]          # print exactly "on" or "off" (state_cmd contract)
shellyplug -d <host> on     [id]          # switch on, confirm by read-back
shellyplug -d <host> off    [id]          # switch off, confirm by read-back
shellyplug -d <host> cycle  [id] [--delay-ms 3000]
                                          # off → confirm → delay → on → confirm
shellyplug -d <host> doctor [id] [--json] # read-only health check (panio doctor)
```

`state` reads `Switch.GetStatus` live on every call and fails if the device is
unreachable.

`doctor` never switches the relay. It reports three checks: `reachable`
(`Shelly.GetDeviceInfo` answers; model and firmware), `auth` (fails when the
device has authentication enabled, which shellyplug does not support yet), and
`switch` (the switches `Shelly.GetStatus` lists, or the one `[id]` names). A
failed check ends the list. `--json` prints the report `panio doctor` reads
and exits 0; without it, one line per check and exit 1 if any failed.

### Wiring into panio power hooks

```bash
panio power set -t target-machine \
    --cycle-cmd "shellyplug -d 10.0.0.5 cycle 0" \
    --on-cmd    "shellyplug -d 10.0.0.5 on 0" \
    --off-cmd   "shellyplug -d 10.0.0.5 off 0" \
    --state-cmd "shellyplug -d 10.0.0.5 state 0" \
    --doctor
```

`--doctor` makes `panio doctor` run `shellyplug -d 10.0.0.5 doctor --json`
(see [Helper health checks](#helper-health-checks-doctor)).

### Gotcha: macOS Local Network privacy

On macOS Sequoia and later, LAN access is granted per binary, attributed to
the launching app. `shellyplug` is the only helper that reaches a LAN device
(loopback is exempt), so it can fail with **`No route to host` (EHOSTUNREACH)**
while `curl` and a browser reach the device fine. Fix: System Settings →
Privacy & Security → Local Network, and enable the app that launches the hook
(e.g. iTerm2/Terminal).

---

## Intel AMT power control (amt)

The `amt` helper switches **Intel AMT (vPro) machines** through their
Management Engine (ME, an always-on motherboard controller), with no plug. It
speaks **WS-Management** (XML management messages over verified HTTPS on
port 16993 by default); pure Rust, one-shot
and stateless.

Prefer AMT where the hardware has it: `state` is a true reading from the ME
(on, off, sleeping), and no outlet or wiring is needed. Under the hood it calls
`CIM_PowerManagementService.RequestPowerStateChange` and reads back
`CIM_AssociatedPowerManagementService.PowerState`.

### Requirements and TLS trust

AMT must be enabled in MEBx (the ME firmware setup screen) and provisioned
with a server certificate. HTTPS uses port **16993** by default, with
certificate-chain, name, and validity checks on every connection. HTTP
Digest password authentication still runs inside that encrypted connection.
There is **no automatic fallback to HTTP**, no skip-verification option, and
no custom certificate verifier or pinning mode. Redirects are rejected and
proxy environment variables are ignored; the helper connects directly.
TLS socket I/O, including the handshake, shares the call's deadline rather
than restarting the timeout for each received byte. Platform DNS resolution
can exceed that budget.

Trust starts with **built-in public roots**, not the operating system's
certificate store. For private provisioning, add a PEM file containing one
or more trusted certificates with `--tls-ca-file /etc/panio/amt-ca.pem`.
This adds trust to the public roots. `AMT_TLS_CA_FILE` supplies the default
when the flag is absent; the explicit flag wins. An unreadable, empty, or
malformed file fails before any network request.

Use the name in the certificate's Subject Alternative Name (SAN). An IP
address works only when the certificate has that IP SAN. Adding a CA file
establishes issuer trust; it **cannot fix a name mismatch**. Certificates
must satisfy standard rustls verification, including self-signed certificates;
if an existing certificate cannot, provision a suitable certificate rather
than disabling checks.

The CA path and environment fallback are read **where the helper executes**.
For a remote power channel, install the file on the control host and put its
path in every hook. Panio does **not** forward `AMT_TLS_CA_FILE` over SSH.
Unlike the password, a CA certificate is not secret.

Firmware must support TLS 1.2 or newer **and** a cipher/signature combination
supported by rustls. Some older AMT firmware offers TLS 1.2 with only legacy
algorithms, so the version alone does not establish compatibility. TLS and
certificate failures are terminal, including during power-confirmation
polling. Certificate provisioning and mutual TLS (client certificates) are
outside this helper's scope. `kvm` settings use the same HTTPS connection;
this does **not** encrypt the separate RFB/VNC session on port 5900.

### Transport selection and migration

Existing bare addresses now select HTTPS. Update **all four power hooks**
(`on_cmd`, `off_cmd`, `cycle_cmd`, and `state_cmd`) together before upgrading
an HTTP-only setup: provision TLS, or explicitly accept HTTP's risks.

| Device address | Option | Transport and default port |
|---|---|---|
| `amt-host.example` or `https://amt-host.example` | none | Verified HTTPS, 16993 |
| `amt-host.example` or `http://amt-host.example` | `--allow-insecure-http` | Unencrypted HTTP, 16992; warning on stderr |
| `http://amt-host.example` | none | Rejected before credentials or network access |
| `https://amt-host.example` | `--allow-insecure-http` | Rejected as contradictory |

An explicit port overrides only the port: `amt-host.example:16992` now
means **TLS on 16992**, not HTTP. HTTP requires the flag, which has no
environment-variable equivalent. `--tls-ca-file` (including its environment
fallback) is incompatible with HTTP.

For a legacy HTTP deployment, use `amt --allow-insecure-http -d
http://amt-host.example state` and add the same flag to every power hook.
The warning appears on stderr on each invocation, including successful
`panio power-state`; the helper's stdout stays exactly `on` or `off`.
HTTP exposes the password exchange to offline guessing and lets an attacker
alter commands or state replies. Do not select it as an automatic response
to a certificate or connectivity failure.

### Credentials

**Keep the password out of the lab file, flags, and repositories.** The lab
file holds the address and username. The helper takes the password from the
first of these that is set:

| Source | Use it for |
|---|---|
| `AMT_PASSWORD` in the environment | `op run`, CI, a one-off export |
| `--password-file <path>` | a systemd credential, a Docker/Kubernetes secret mount, a `0600` file |
| `--password-command <cmd>` | any secret manager's CLI: `op read`, `pass show`, the macOS Keychain |

A path or a command does not reveal the password, the same way ssh's
`IdentityFile` does not, so it may sit in a hook in the lab file. The file's
single trailing newline is dropped. The helper warns (but proceeds) when the
file is readable by group or others. It runs the command through `sh -c`
(`cmd /C` on Windows) with no stdin, uses its stdout, lets its stderr through,
and kills it after 30 s. A missing file, a failing or silent command, or no
source at all exits **3** (not configured); panio reports the hook as
`helper_failed` with `child_exit: 3` and the helper's message.

### Setting up the credential source

Put the source in the hook itself, so it works from any shell, cron job, or
unattended runner with nothing in the environment:

- **A command** — 1Password CLI (a service account token must be in the
  environment for unattended use), or the macOS Keychain:

  ```bash
  panio power set -t nuc \
      --state-cmd "amt state -d amt-host.example --password-command 'op read op://lab/amt/password'"
  # macOS Keychain (add once: security add-generic-password -s panio-amt -a admin -w)
  --state-cmd "amt state -d amt-host.example --password-command 'security find-generic-password -s panio-amt -w'"
  ```

- **A file** — a plain `0600` file, or a systemd credential for a runner that
  is a service. systemd copies the secret into a private per-service
  directory and names it in `$CREDENTIALS_DIRECTORY`, which the hook's shell
  expands:

  ```ini
  # the runner's unit
  [Service]
  LoadCredential=amt-password:/etc/panio/amt-password
  ```

  ```bash
  --state-cmd 'amt state -d amt-host.example --password-file "$CREDENTIALS_DIRECTORY/amt-password"'
  ```

- **The environment** — for one-off use, `op run` or an interactive export
  (`read -rs AMT_PASSWORD && export AMT_PASSWORD` keeps it out of history):

  ```bash
  # .env:  AMT_PASSWORD=op://<vault>/<item>/password
  op run --env-file .env -- bash -c 'panio power-state <target>'
  ```

  The single quotes stop the parent shell expanding `$AMT_PASSWORD` before
  `op run` sets it (same for `HA_TOKEN` under
  [Generic power hooks](#generic-power-hooks)). A fetch-and-exec wrapper
  (`AMT_PASSWORD="$(fetch-secret …)" exec "$@"`) around `panio` also still
  works, but `--password-command` does the same job without a script to
  install.

**Where each source is read.** A file path or command in a hook is resolved on
the host where the hook runs, which for a remote power channel is the control
host. `AMT_PASSWORD` instead is set on the machine you type on: panio
forwards it to a remote control host over the remote command's **stdin**,
never argv, so `ps` there never shows it. Since the environment comes first,
a forwarded `AMT_PASSWORD` overrides a hook's file or command. Two limits:

- Only `AMT_PASSWORD` is forwarded (`ssh::FORWARDED_ENV`). `HA_TOKEN` and other
  hook secrets need a wrapper on the control host or sshd `AcceptEnv` (see
  [Generic power hooks](#generic-power-hooks)).
- Only non-interactive commands (`power-cycle`, `power on/off/state`) forward
  it; `serial connect` forwards nothing.

### Commands

```bash
amt -d <host> status                 # firmware identity + power state detail
amt -d <host> state                  # print exactly "on" or "off" (state_cmd contract)
amt -d <host> on                     # power on, confirm by read-back
amt -d <host> off                    # power off (hard), confirm by read-back
amt -d <host> cycle [--delay-ms 3000]  # off → confirm → delay → on → confirm

amt -d <host> kvm status             # is port 5900 open, is consent required
amt -d <host> kvm enable             # open 5900 to VNC clients, enable redirection
amt -d <host> kvm disable            # close 5900, keep the stored RFB password
```

- **`-d <host>`**: hostname, IPv4, or bracketed IPv6 (`[fe80::1]`), optional
  port (default 16993 for HTTPS, 16992 for explicitly allowed HTTP).
  `https://` is accepted; `http://` requires `--allow-insecure-http`.
  A path, query, fragment, userinfo, or unbracketed IPv6 is rejected.
- **`-u <user>`**: Digest username (default `admin`).
- **`state`** prints `on` only for PowerState 2 (running). Sleep, hibernate,
  and soft-off print `off`. Any other value (`Other`, transitional) is an
  error naming the raw value.
- **`off`** is an **unconditional power-off** (CIM "Off - Soft"), like holding
  the power button, not an OS shutdown.
- **`cycle`** powers off any host not already at Off - Soft (including S3
  sleep and hibernate), holds for `--delay-ms`, powers on, and confirms each
  phase, so the result is a true cold boot.
- **`status`** is the debugging view: firmware identity (HTTP `Server:`
  header) and the raw CIM PowerState name and number.

#### KVM redirection (the ME's built-in VNC server)

`kvm enable` makes an AMT machine a **network KVM for any standard VNC
client**, with no capture card or HID rig.

The RFB (VNC protocol) password is a second secret with the same three sources:
**`AMT_RFB_PASSWORD`**, `--rfb-password-file <path>`, or
`--rfb-password-command <cmd>`, first set wins; never the password itself on
the command line. It must be:

- **exactly 8 characters**;
- at least one capital, one lowercase, one digit, and one special character;
- **not** containing `"`, `,` or `:`.

The helper validates it before writing, because AMT **locks the RFB password**
after a few failed attempts (re-Putting the password clears a lock). Single-quote
it: `!` in double quotes is shell history expansion.

```bash
AMT_PASSWORD=… AMT_RFB_PASSWORD='Ab3!defG' amt -d <host> kvm enable
amt -d <host> --password-file ~/.config/panio/amt \
    kvm enable --rfb-password-file ~/.config/panio/amt-rfb
```

Defaults: `OptInPolicy=false` (no local consent prompt) and
`SessionTimeout=0`. Pass `--opt-in` to require consent, or
`--session-timeout <minutes>` for an idle drop.

Before relying on KVM:

- **KVM must be enabled in MEBx** (`kvm status`: `enabled in MEBx`). If it is
  `NO`, only the firmware setup screen can fix it.
- **Newer firmware drops port 5900:** Kaby Lake 11.8.94, Cannon Lake 12.0.93,
  Comet Lake 14.1.70, Tiger Lake 15.0.45 and Alder/Raptor Lake 16.1.25 onward.
  There KVM needs the 16994/16995 redirection protocol, which this helper does
  not implement. Check `amt -d <host> status` first.

### Wiring into panio power hooks

```bash
panio power set -t target-machine \
    --cycle-cmd "amt cycle -d amt-host.example -u admin --delay-ms 5000" \
    --on-cmd    "amt on -d amt-host.example -u admin" \
    --off-cmd   "amt off -d amt-host.example -u admin" \
    --state-cmd "amt state -d amt-host.example -u admin"
```

For a private CA, add `--tls-ca-file /etc/panio/amt-ca.pem` to **each**
hook. Each hook also needs a password source: add `--password-file` or
`--password-command` to it (see
[Setting up the credential source](#setting-up-the-credential-source)), or run
`panio` with `AMT_PASSWORD` set on the machine you type on.

### Gotchas

- **The AMT NIC drops link for a few seconds around power transitions**
  ("no route to host"). The helper retries transient transport errors
  (connection, I/O timeout, DNS) within a 20 s budget; deterministic failures
  (TLS/certificate errors, bad address, unparseable response, proxy) fail immediately. Unreachable
  past 20 s is real.
- **`state` reflects the host, not the outlet**: S3 and hibernate report `off`.
  `on` boots or wakes; `cycle` cold-boots.
- **BIOS "AC Recovery" does not matter**: AMT power-on is an explicit ME
  command, not a power restore.
