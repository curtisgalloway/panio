# Recipe: adding a power-control helper for new hardware

Use this to support a new power-switching device: a PDU (networked power
strip), relay board, smart-plug family, USB-PD hub, BMC, or anything else that
switches a target's power.

Device-specific logic never goes in the core crates ([power.md](../power.md)).
It lives in a standalone helper binary that panio drives through four shell
hooks on the target's power channel. `cli/` needs no changes beyond,
optionally, the install step.

Shipped examples:

| Helper | Language | Controls |
|---|---|---|
| [`cambrionix/`](https://github.com/curtisgalloway/panio/tree/main/cambrionix) | Rust | Cambrionix USB hubs, over the hub's control UART (serial port) |
| [`dsdrelay/`](https://github.com/curtisgalloway/panio/tree/main/dsdrelay) | Rust | DSD TECH USB serial AT relay boards (CP2102N, 9600 8N1), NC wiring |
| [`dsdble/`](https://github.com/curtisgalloway/panio/tree/main/dsdble) | Rust | DSD TECH Bluetooth LE relay boards (btleplug), NC wiring, write-only: `state` always fails |
| [`phidget/`](https://github.com/curtisgalloway/panio/tree/main/phidget) | Rust | Phidgets VINT relay modules (REL1100) through a HUB0002, over raw USB (nusb), normally-open wiring, write-only: `state` always fails |
| [`usbtoggle/`](https://github.com/curtisgalloway/panio/tree/main/usbtoggle) | Rust | Not a power helper: a `usb` channel helper (`usb host|target|state`) that presses a USB sharing switch's button through any relay and verifies by probing the peripheral with nusb |
| [`shellyplug/`](https://github.com/curtisgalloway/panio/tree/main/shellyplug) | Rust | Shelly Gen2+ plugs, over their local HTTP RPC |
| [`switchport/`](https://github.com/curtisgalloway/panio/tree/main/switchport) | Rust | Managed-switch port link/PoE (TP-Link Easy Smart, SNMP): a *link* helper, see below |
| [`zigplug/`](https://github.com/curtisgalloway/panio/tree/main/zigplug) | Python | Zigbee smart plugs, via a CC2652 coordinator dongle |

---

## 1. The hook contract

Panio runs each hook with `sh -c <cmd>` (`cli/src/main.rs`,
`run_power_hook`):

| Hook | Run by | Contract |
|---|---|---|
| `on_cmd` | `panio power on` | exit 0 = success; a non-zero exit makes panio exit 101 (`helper_failed`, the hook's code in `child_exit`), or 3 (`not_configured`) for 126/127 — the hook is missing or not executable |
| `off_cmd` | `panio power off` | same |
| `cycle_cmd` | `panio power-cycle` | same; the hook owns the *full* sequence (off, delay, on, confirm) — panio adds no timing of its own |
| `state_cmd` | `panio power-state` | the **first whitespace-delimited token of stdout** must be `on` or `off` (case-insensitive); anything else, or a non-zero exit, is an error. Takes precedence over serial sense-line state when configured |

The environment the helper must tolerate:

- **`sh -c`, no shell profile.** PATH is panio's PATH with the helper dirs
  prepended: the private libexec dir (`~/.local/libexec/panio/bin`), then the
  system package dir (`/usr/libexec/panio/bin`). Helpers installed by
  `panio setup` or the .deb resolve by bare name; absolute paths also work.
  `panio doctor` checks absolute paths with `test -e` and bare names with
  `command -v` under the same resolution.
- **Runs on the channel's control host.** Power commands re-exec over SSH on
  the host that owns the channel (`panio power set --host <labhost>`).
  Install the helper on *that* host.
- **State and temp data go where panio says.** Every invocation gets two env
  vars naming existing directories:
    - `PANIO_STATE_DIR` (`~/.config/panio/helpers/<name>/`) for durable
      state (databases, pairing records).
    - `PANIO_RUNTIME_DIR` (`/tmp/panio-<uid>/<name>/`) for discovery files,
      locks, and logs. Wiped on reboot.

    `<name>` is the basename of the hook command's program (`zigplug …` →
    `zigplug`); channel daemons get the channel name instead (hidrig → `hid`).
    Prefer the env vars; fall back to the same literal paths when run
    standalone. **Never** write unnamespaced files into `~/.config/panio/`:
    the lab file lives there.
- **One-shot, stateless, exclusive.** Open the device, act, exit. Concurrent
  invocations collide on an exclusive-open serial port, so keep long-lived
  modes (pairing windows, monitors) off the hook paths.
- **stdout/stderr pass through**, except `state_cmd` stdout, which is parsed.
  On failure, print to stderr and exit non-zero.
- **Secrets come from the `secret` crate, never a flag's value or the lab
  file.** A helper that needs a password or token `X` depends on
  `secret = { path = "../secret" }`, declares a `secret::Spec` naming env `X`
  and its flags `--x-file` / `--x-command`, and calls `secret::require` (or
  `secret::resolve` for an optional one). The first source set wins, in the
  order env, file, command; a hook can then carry its own source and needs
  nothing in the environment. Map a `secret::NotConfigured` error to exit 3
  (`secret::is_not_configured`), as `amt/src/main.rs` does: retrying will not
  fix a missing file. See [power.md](../power.md#credentials).

## 2. Helper CLI conventions

Mirror the existing helpers:

```
<helper> -d <device> on <id>                  # switch on; confirm if the hw can report
<helper> -d <device> off <id>                 # switch off; confirm
<helper> -d <device> state <id>               # print exactly "on" or "off"
<helper> -d <device> cycle <id> [--delay-ms 3000]   # off → confirm → delay → on → confirm
<helper> -d <device> state                    # (optional) human-readable table of all ids
```

- `-d/--device` is the transport (serial port path, IP, hub address); `<id>`
  selects the outlet (hub port number, IEEE address, outlet index). Both live
  in the hook string, so the helper needs no configuration.
- **Confirm by read-back wherever the hardware reports state.** A silently
  failed power-cycle costs a debugging session.
    - `on`/`off` verify and exit non-zero on mismatch (`zigplug` reads the
      OnOff attribute back; `cambrionix` re-reads the port table after every
      `mode` command).
    - `cycle` confirms *both* phases, so a relay that ignored `off` is not
      reported as a successful cycle.
    - `state` maps only readings it understands; an unknown value is an error
      carrying the raw reading, never a guessed `off`.
- `cycle` defaults to a 3000 ms off-hold, enough for PSU capacitors to drain.
- Extra lifecycle commands are fine (`zigplug form` / `permit` / `list` /
  `remove`); keep them out of the hook strings.
- Global options that come before the verb should take a value (`-d <device>`,
  `-u <user>`, `--password-file <path>`). The doctor check below finds the
  verb by that rule.

### Doctor check (optional, recommended)

`panio doctor` only checks that a hook's program exists. It cannot tell
whether the hardware behind it answers. A helper can report that itself with a
`doctor` subcommand, and a channel opts in to running it with `doctor = true`
(`panio power set -t <target> --doctor`; also on `hid set`, `usb set`, and
`action set`). The reference implementation is `shellyplug doctor`.

**The contract:**

- `<helper> <transport options> doctor --json` exits **0** and prints one JSON
  object on stdout:

  ```json
  {"checks": [{"name": "reachable", "status": "ok", "detail": "SNPL-00112EU, gen 2"},
              {"name": "auth", "status": "fail", "detail": "the device requires authentication"}]}
  ```

  `status` is exactly `ok`, `warn`, or `fail`. `name` is required and
  non-empty; `detail` is optional. Extra keys are ignored. A helper exits 0
  whatever its checks found: the report carries the verdict.
- **Read-only.** It must not change hardware state: no relay switching, no
  pairing, no writes to the device. Reading the current relay state is fine.
- **Bounded.** panio kills the check after **10 s** and reports `FAIL
  … doctor timed out`, so keep your own transport timeouts shorter than that.
- **No `doctor` subcommand is fine.** A non-zero exit with no valid report is
  shown once as `no-check … helper has no doctor check (<program>)` and is not
  a problem. Exit 0 *without* a valid report, or a report that breaks the
  shape above, is a `FAIL` ("broke the doctor contract").

**What panio runs.** The check runs on the channel's control host the way the
hooks run (helper dirs first on PATH), natively when the host is local and as a
POSIX script over SSH otherwise. A Windows control host cannot be probed
remotely, the same limit as every other `doctor` probe. Which part of the
configured command is reused:

- **A full hook** (`cycle_cmd`, `on_cmd`, `off_cmd`, `state_cmd`, an action's
  `cmd`): the hook up to its verb. panio reads the hook as shell words and
  keeps any leading `VAR=value` assignments, the program, and every option word
  (one starting with `-`) together with its value. A `--name=value` word or a
  short option with its value attached (`-dHOST`) stands alone; any other
  option takes the next word as its value. The first word that is not an
  option is the verb, and it and everything after it are dropped, as is a `--`
  marker. Then ` doctor --json` is appended. So
  `AMT_PASSWORD=x amt -d 192.0.2.10 -u admin cycle --delay-ms 500` runs
  `AMT_PASSWORD=x amt -d 192.0.2.10 -u admin doctor --json`.
- **A prefix** (the `hid` and `usb` `cmd`, which panio already appends verbs
  to): the whole `cmd`, then ` doctor --json`.

The kept text is copied verbatim, quoting included. The words before the verb
must form one simple command: an unquoted `;`, `|`, `&`, `<`, `>`, `(`, `)`
or newline there is reported as `FAIL … cannot derive a doctor command`. A
channel whose hooks all share a transport (four power hooks on one plug) runs
the check once. On a power channel, `doctor = true` covers the default hooks
and every named power level's hooks (`[…power.levels.<name>]`), so a `hard`
level on a plug and a `soft` level on a board controller are each asked. Arguments after the verb, such as an outlet id, are not
passed. A helper that drives several outlets checks the device as a whole, or
takes an optional id of its own (`shellyplug doctor [id]`).

**Only opt in a helper that implements the contract.** A hook program that
ignores its arguments (a script that always power-cycles) would run its normal
action when asked for `doctor --json`.

**Output.** Each check becomes one line under its channel:

```
target-machine  power                    @bench1  ok    cycle_cmd,state_cmd
target-machine  power doctor: reachable  @bench1  ok    SNPL-00112EU shellyplusplugs-aa00bb11cc22, gen 2, firmware 1.4.4
target-machine  power doctor: auth       @bench1  ok    authentication disabled
target-machine  power doctor: switch     @bench1  ok    switch:0 present
```

A `fail` counts toward `doctor`'s problem count and exit status 1. A `warn` is
shown but is not a problem. When a channel runs more than one check (two
power levels on different helpers, two actions on different programs), each
line says which hooks it came from: `power doctor (default): …`,
`power doctor (level hard): …`, `actions doctor (action reset): …`.

## 3. Implementation skeleton

Use Rust for a simple serial/HTTP protocol, Python when the driver library is
Python (zigpy-znp). Any language works if it installs an executable into the
libexec dir (`~/.local/libexec/panio/bin`). Helpers stay off PATH; run one
by hand with `panio helper <name> …`, adding `-t <target> --channel
<kind>` or `--host <labhost>` before the name to run it on the host where
the hardware is (see [Power](../power.md)).

For a helper's own help, use `panio helper shellyplug --help`: everything
after the helper name goes to the helper. This is a way to discover a
bundled helper's commands without opening hardware or starting its daemon.

Take state and temp paths from `PANIO_STATE_DIR`/`PANIO_RUNTIME_DIR` (§1);
zigplug's `default_db_path()` and `runtime_dir()` are the reference
implementations, including lazy migration from a pre-API path.

**Rust helper (the `cambrionix` pattern):**

1. `cargo new <helper> --bin` at the repo root; Apache 2.0 headers; `clap`
   (derive) + `anyhow` + whatever transport crate (`serialport`, `ureq`).
2. `main.rs` = CLI surface + command logic; `proto.rs` = transport/protocol.
3. Add the crate name to `CRATES` in [`Makefile`](https://github.com/curtisgalloway/panio/blob/main/Makefile) **and** to
   `HELPER_CRATES` in `cli/src/setup.rs` so `make install` / `panio setup`
   build and install it into the libexec dir (`cargo install --root`).
4. Give it a CI job (`.github/workflows/ci.yml`) and a `crate_job` line in
   `scripts/ci-local.sh`; `scripts/ci-coverage-check.sh` enforces both.

**Python helper (the `zigplug` pattern):**

1. New top-level dir with its own `pyproject.toml` (a uv project, **not** part
   of the root legacy package): `[tool.uv] package = true`,
   `[project.scripts] <helper> = "<pkg>._cli:app"`, src layout, typer CLI.
2. Wrap async device libraries with one `asyncio.run()` per subcommand. Map
   library exceptions to one-line errors; a traceback in hook output looks
   like panio breakage.
3. Add an install block to `cli/src/setup.rs` following zigplug's: probe for
   `uv`, run `uv tool install --force <repo>/<helper>` with
   `UV_TOOL_BIN_DIR` set to the libexec dir, and skip with a note when uv is
   missing.
   Mention it in the Makefile header comment.

Either way, **install the helper before testing hooks** (`panio setup`, or
`cargo install --path <helper> --root ~/.local/libexec/panio` for a
one-off); panio runs installed binaries, not checkouts.

## 4. Hardware verification ladder

Climb in order; each rung isolates one layer and the destructive test is last.

1. **Identify the device node first.** `ioreg -p IOUSB -w0` (macOS) /
   `lsusb` + `/dev/serial/by-id/` (Linux). Don't guess from `/dev`: chips
   without a serial number (e.g. CP2102N) are named by USB topology on macOS
   (`/dev/cu.usbserial-8310` ↔ location `08310000`) and rename when moved.
2. **Run the helper directly** (`panio helper <name> …`): any lifecycle
   setup (e.g. `panio helper zigplug form` + `permit`), then `state <id>`,
   `on`, `off`, `cycle`. Confirm each physically (relay click, LED,
   multimeter).
3. **`panio power-state <target>`**: read-only. Proves the hook string, the
   `sh -c` environment, and the `on`/`off` token contract.
4. **`panio power on/off <target>`**: switching through the full stack.
5. **`panio power-cycle <target>`**: last, because it reboots the target.
6. `panio doctor`: confirms which hooks are configured and that each hook's
   program exists. With `--doctor` set on the channel, it also runs the
   helper's `doctor --json` check (§2).

## 5. Wiring into a target

```bash
panio power set -t <target> \
    --cycle-cmd "<helper> -d <device> cycle <id>" \
    --on-cmd    "<helper> -d <device> on <id>" \
    --off-cmd   "<helper> -d <device> off <id>" \
    --state-cmd "<helper> -d <device> state <id>" \
    [--host <labhost>] \      # the control host that owns the hardware
    [--doctor]                # run `<helper> -d <device> doctor --json` in `panio doctor`
```

All four hooks are optional; wire what the hardware supports. Take secrets
(API tokens) from the environment, never the hook string (see the Home
Assistant example in [power.md](../power.md)).

## 6. Docs + PR checklist

- [ ] `docs/power.md` — usage section: install, setup, commands, hook
      wiring, hardware gotchas
- [ ] `AGENTS.md` — directory-layout entry + the power bullet in
      "Current capabilities"
- [ ] `README.md` — both helper lists (the power row in the subsystem table
      and the `make install` paragraph) + the manual install command block
- [ ] `Makefile` — `CRATES` (Rust) or the header comment (other)
- [ ] `cli/src/setup.rs` — `HELPER_CRATES`, so `panio setup` installs the
      helper from a source clone
- [ ] `.github/workflows/release.yml` — the `HELPERS` env list **and** the
      rust-cache `workspaces` block, or the `.deb`/tarball silently omits the
      binary (v0.1.13 shipped without `amt` this way).
- [ ] `packaging/nfpm.yaml` — the helper list in the package description
- [ ] `.github/workflows/ci.yml` — a job for the new crate
      (`working-directory: <crate>`; copy an existing crate job) **and** a
      matching `crate_job` line in `scripts/ci-local.sh`. The `coverage` job
      (`scripts/ci-coverage-check.sh`) fails until both exist, or if the
      `Makefile`, release `HELPERS`, or `HELPER_CRATES` lists omit the crate.
- [ ] `docs/README.md` — the Power row in the subsystem guide table
- [ ] Apache 2.0 headers on all new source files
- [ ] Optional: a read-only `doctor --json` subcommand (§2, "Doctor check"),
      with tests that need no hardware

## 7. Field notes (earned the hard way)

- **2.4 GHz radios hate USB 3.** zigplug network formation failed with
  zigpy-znp's "too much RF interference" error while the dongle sat next to a
  USB video-capture device; a USB 2.0 extension cable fixed it. Move a radio
  dongle before debugging software.
- **Verify the library API against the installed version.** Device libraries
  (zigpy et al.) break APIs across major versions.
- **One-shot for stateless transports; a daemon for stateful ones.**
    - A self-contained request/response transport (the `cambrionix` UART) is
      fine one-shot; it avoids a service to install and supervise.
    - A transport with *session state* needs a persistent owner. zigplug's
      one-shot version reset the CC2652 on every serial open (sometimes into
      the bootloader, hanging the client), and two concurrent hooks on one ZNP
      session wedged the coordinator for hours and lost its NVRAM.
    - The zigplug fix: an auto-spawned daemon owns the port and serializes
      operations with hard timeouts; the CLI proxies to it. Hook strings stay
      one-shot-shaped (`hidrig serve` does the same on the KVM path).
- **Make `state` cheap and accurate.** Agents poll it. Never cache on the
  helper side; fail loudly rather than guess.

## 8. Link-control helpers (`switchport`)

Not every device a helper drives is a power switch. `switchport` turns a
managed switch's port on and off so the target sees a real link drop (USB
Ethernet adapters keep the PHY powered when the host interface is down). It is
a one-shot, stateless helper like the power helpers, with the same CLI
conventions (`-d <device>`, `doctor --json`, read-back on every write, secrets
through the `secret` crate), but it is wired into the **netboot link hooks**
rather than the power hooks:

```bash
panio netboot set -t <target> \
    --link-down-cmd 'switchport -d <switch> --mgmt-port <N> port <P> off' \
    --link-up-cmd   'switchport -d <switch> --mgmt-port <N> port <P> on'
```

`netif down-hard` runs `link_down_cmd` before admin-down; `netif mode link|netboot|ffx`
admin-up the interface and run `link_up_cmd` before raising the host IP. Its safety
rules (refuse to disable the port the helper itself reaches the switch
through, identity pinning, read-back, opt-in link wait with `--wait-link`) are
in `switchport/AGENTS.md`; any future link helper should keep them.
