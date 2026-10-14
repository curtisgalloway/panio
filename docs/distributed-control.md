# Distributed control: one lab, one file

> **Status: Phases 0–5 implemented** (2026-06-01).
> Shipped: the SSH transport, the one-file lab model (`--lab` / `PANIOLO_LAB`),
> re-exec of one-shot commands on a target's host, a tunneled `console` for a
> remote target, remote `setup --host`, and discovery-assisted `configure`.
> Channels of one target may live on different control hosts; each command
> routes per channel. Not yet built: see [What's deferred](#whats-deferred) and
> the [implementation plan](https://github.com/curtisgalloway/paniolo/blob/main/notes/distributed-control-plan.md).
> Compare [paniolo vs. labgrid](dev/ci-integration/related-work.md).

From your dev machine you run `paniolo console fortune`, and paniolo does the
right thing on whichever control host `fortune` is wired to. You describe the
lab once, in one git-tracked file; paniolo reaches the hosts over SSH.

## The problem

Without this, you SSH into the control host by hand and run `paniolo …` there
(the remote-control pattern in the root
[README](https://github.com/curtisgalloway/paniolo/blob/main/README.md)).
Anything that serves a port (the dashboard, `serial watch`, `video preview`)
then needs a hand-rolled `ssh -L` port forward. The goal is to **abstract away
host location**, including a target whose hardware spans several control hosts.

## The core decision: one lab, one file

A **lab** is one config file in a git repo. Point paniolo at it with `--lab` or
`PANIOLO_LAB`. The file declares every **host**, every **target**, and which
host each piece of a target's hardware lives on. A human edits it (optionally
with an agent's help, see [Configuration workflow](#configuration-workflow));
paniolo reads it but never rewrites it.

**The control host runs the same paniolo.** A re-exec forwards your argv
verbatim (`dispatch::subcommand_args`), so a flag newer than the control host's
release fails there with `unexpected argument`. Keep the hosts on the release
the dev machine runs. Where a spelling is optional (the target as `-t` or
positionally), the positional form works on every past release.

## Design principles

1. **The dev machine is the hub.** It is the only node guaranteed to reach every
   control host. Control hosts may not reach each other, so data **rendezvouses
   at the dev machine**, never between control hosts.
2. **Config is centralized; runtime state lives next to the hardware.** The lab
   file lives in one reviewed place. Daemons, capture logs, advisory locks and
   discovery files live on the control host.
3. **Control hosts are stateless executors.** paniolo ships the relevant slice
   of config at command time. Re-image a host, re-run `paniolo setup`, and it
   resumes its role with nothing to restore (see
   [standing up a control host](control-host.md)).
4. **SSH is the transport.** It already solves auth, encryption and identity.
5. **Don't preclude multi-host targets.** One target may put serial on one host,
   HDMI capture on another and power on a third.

## The config model

Host binding lives on **each resource**, not the target. A target-level `host`
sets the default; each resource inherits it unless it overrides. The default of
that default is `local` (the dev machine), so a lab with one local host and one
target behaves like single-host paniolo.

```toml
# mylab.toml — checked into a git repo; PANIOLO_LAB points here.

[hosts.bench1]
ssh = "user@bench1.local"      # ssh destination — how OTHER machines reach it; the only required field
# hostname = "bench1.local"       # this box's FQDN; set it so bench1 recognizes ITSELF when the
#                                   shared lab file is run on bench1 (matched against `hostname -f`)
# identity = "~/.ssh/id_lab"      # optional key; set it to avoid agent key-spray (below)
# control_path = "~/.ssh/cm-%h"   # optional ControlMaster socket (see Transport)
# paniolo_cmd = "/Users/me/.local/bin/paniolo"  # if paniolo isn't on the host's ssh PATH

[hosts.bench2]
ssh = "user@bench2.local"
# hostname = "bench2.local"

# A normal single-host target. Everything inherits host = bench1.
[targets.fortune]
host = "bench1"                   # default host for this target's resources

[targets.fortune.netboot]
interface = "enx001122334455"
host_ip   = "192.168.99.1"
tftp_root = "~/tftp/fortune"

[[targets.fortune.serial]]
name   = "console"
device = "/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_AA00BB11-if00-port0"
baud   = 115200

[targets.fortune.power]
cycle_cmd = "~/scripts/power-cycle.sh"

# --- the future case: one target spanning two control hosts ---
[targets.fortune.video]
host   = "bench2"                 # HDMI capture is on a different host
device = "0x8300000534d2109"      # USB Video — stable, port-derived id
```

- `host = "local"` (or unset, on a single-host lab) means the dev machine: no
  SSH.
- **One shared lab file, run from any machine.** Give each host a `hostname`
  (its FQDN). Each box compares its own `hostname -f` against every host's
  `hostname`; the match runs **locally** and every other host is **remote**
  (over SSH). The same file then works on the Mac, on `bench1` or on `bench2`.
  - `ssh` is the *reach* path (it may be an `~/.ssh/config` alias); `hostname`
    is the *self-recognition* key.
  - Without a `hostname`, only `ssh = "local"` / `host = "local"` counts as
    local; run the file anywhere else and a host dispatches to itself over SSH.
  - `paniolo host list` prints the detected FQDN and marks the matching host.
- With no `--lab`/`PANIOLO_LAB`, paniolo reads `~/.config/paniolo/lab.toml`. If
  none exists it errors and points at `paniolo init`. Old per-target
  `~/.config/paniolo/targets/*.toml` files are not read.

## Transport (the "Fork B" model)

The transport uses SSH only. The subsystem daemons (`serialcap`, `hdmicap`)
already serve HTTP/WebSocket on a discovery port.

| Command type | Examples | How it reaches the host |
|---|---|---|
| **One-shot control** | `power-cycle`, `netboot start/stop`, `video shot`, `serial log`, `serial send`, config reads | **re-exec over SSH** |
| **Streaming / port-serving** | the dashboard, `serial watch`, `video preview` | **SSH tunnel to the existing daemon** |
| **Interactive `serial connect`** (the tio terminal program) | `ssh -t bench1 paniolo serial connect fortune` | **no tunnel**: tio runs over SSH's own PTY |

**Re-exec.** paniolo runs the same command on the host and forwards
stdin/stdout/stderr and the exit code. An SSH failure exits 4 (see
[Exit status and errors](errors.md)). Logs, locks and discovery stay on the
control host.

**Tunnel.** paniolo starts the daemon remotely (idempotent), reads its discovery
port over SSH, opens an `ssh -L` forward, and points the local browser at the
forwarded port. Only the browser dashboard uses tunnels.

**Latency.** Each host gets one SSH **ControlMaster** connection (a reusable
master connection), shared by every re-exec and `-L` forward. The host's
`control_path` names the master socket.

**Dead links fail, not hang.** Every ssh and sftp paniolo starts carries
`ServerAliveInterval=5` and `ServerAliveCountMax=3`. A connection that goes
silently half-open (laptop sleep, a Wi-Fi change) is noticed within about 15
seconds, and the command fails with `unreachable` (exit 4; see
[errors.md](errors.md)) instead of waiting forever. The next command opens a
fresh master.

**Operational notes:**

- **`paniolo` must be reachable on the host.** Re-exec runs over a
  non-interactive ssh, whose PATH often omits `~/.local/bin`. If bare `paniolo`
  doesn't resolve there, set the host's `paniolo_cmd` to an absolute path.
- **Set `identity` to avoid ssh-agent key-spray.** An agent offering many keys
  (e.g. 1Password) can trip the host's `MaxAuthTries` before the right key. A
  per-host `identity` makes paniolo pass `-i <key> -o IdentitiesOnly=yes`.

### Env forwarding to a control host

A re-exec gets the far side's environment, not the caller's, like any
`ssh host cmd`. For
`power-cycle nuc` to work when `nuc`'s power channel is remote, AMT's
`AMT_PASSWORD` (see [power.md](power.md#credentials)) must reach that host
without landing on its command line: a `KEY=value` prefix would sit in `argv`,
visible to every local user through `ps`.

paniolo forwards a fixed allowlist (`ssh::FORWARDED_ENV`, currently just
`AMT_PASSWORD`) over stdin. When a listed variable is set locally, the remote
command is wrapped as

```sh
sh -c 'IFS= read -r AMT_PASSWORD && export AMT_PASSWORD && exec "$@"' sh paniolo …
```

and the value is written to the child's stdin, one line per variable in a fixed
order, ahead of the command's own stdin (a `serial send` payload, say). A
process listing shows only the name.

- **Every non-interactive dispatch forwards it** (`run`, `run_passthrough`,
  `run_stdout_to` in `cli/src/ssh.rs`): `power-cycle`, `power on/off/state`,
  and any other re-exec or captured command.
- **`run_interactive` forwards nothing** (`serial connect`, `adb shell`,
  `setup --host`). Its stdin is your terminal; those commands don't need
  `AMT_PASSWORD`.

Forwarding is one way to get the password there. A hook can instead name a
`--password-file` or `--password-command` (see
[power.md](power.md#setting-up-the-credential-source)), which is read on the
control host where the hook runs, so a runner on the control host itself
needs nothing forwarded. A forwarded `AMT_PASSWORD` wins over either.

### The dashboard, and why multi-host rules out a reverse-proxy

hdmicap serves the dashboard page, and the page reaches serialcap by an
**absolute URL** (`ws://<host>:8724/stream`), with a `?serialws=` override (see
[architecture §7](dev/architecture.md)). paniolo forwards each daemon's port to
the dev machine and opens
`http://127.0.0.1:<local-hdmi>/?token=<hdmi-token>&serialws=ws://127.0.0.1:<local-serial>/stream?token=<serial-token>`
(nested URL percent-encoded; tokens from each daemon's `daemon.json`, read over
the same SSH session). This works with the daemons on different hosts and needs
no changes to hdmicap or serialcap.

That URL carries bearer tokens, so `paniolo console` hands it to the browser and
prints only `http://127.0.0.1:<local-hdmi>`. `paniolo video preview` prints the
openable video URL; `--open` skips printing it. Run inside an SSH session,
`console` opens no browser and prints an `ssh -N -L …` command that forwards
the tunnels' local ports onward ([dashboard.md](dashboard.md#viewing-from-another-machine-over-ssh));
`--print-url` prints the URL itself.

Having hdmicap reverse-proxy serialcap was rejected: it needs a cross-host
connection, which principle 1 rules out.

### Why not a long-running agent daemon (labgrid's exporter)

A per-host agent with its own RPC API ("Option B" in `AGENTS.md`) would ease
streaming and multi-user locking, but it gives up paniolo's zero-infrastructure
design ([related-work](dev/ci-integration/related-work.md)). It remains an
option if board-farm scale ever becomes a goal.

## Console lifecycle

`paniolo console <target>` is **foreground-blocking**. It opens the forwards,
launches the browser, and holds the tunnel until Ctrl-C, then tears down. It
keeps no local runtime state.

A non-blocking `--detach` mode (print the URL and return; reap on
`console --down` or idle timeout) is deferred. It needs a local tunnel
registry, and most agent workflows use `video shot`/`read` instead of the live
stream.

## Configuration workflow

Discovery scaffolds config; a human writes and approves the lab file.

1. `paniolo configure fortune -H bench1` runs discovery on the host over SSH and
   turns its inventory into a **proposed** `[targets.fortune]` block. It
   best-guesses the USB-Ethernet interface and serial device, and lists other
   candidates as comments.
2. paniolo **prints** the block (with a reconcile-by-hand note if the target
   exists) and writes nothing.
3. You review it, paste it into the lab file, edit, and commit.

An agent can drive step 1 but never changes the lab file itself.
Reconfiguration is the same flow.

## What's deferred

- **`console` on a cross-host target.** Other commands route per channel, but
  `console` needs the **serial and video** channels on one host. It rejects a
  target whose serial and video live apart with a clear error (other channels
  may sit anywhere).
- **`console --detach`** and its local tunnel registry.
- **Multi-user / locking / reservations** (labgrid's *places*). Single-user is
  assumed.
- **A long-running agent daemon / RPC API.**
- **Multi-file / multi-lab composition.**

## Relationship to labgrid

This is a smaller-footprint version of labgrid's distributed shape:

| paniolo | labgrid |
|---|---|
| per-resource host binding | Resources bound to exporters |
| SSH data plane | client→exporter-over-SSH data plane |
| discovery-assisted config | a coordinator-as-registry, minus the always-on server |

The divergence is **no coordinator and no exporter daemon**: one git-tracked lab
file plus SSH. See [related work](dev/ci-integration/related-work.md).

## Resolved design questions

- Lab lookup order: `--lab` flag, then `PANIOLO_LAB`, then
  `~/.config/paniolo/lab.toml`. The legacy `~/.config/paniolo/targets/*.toml`
  files are never read.
- The per-command config slice travels as a temp file copied over SSH,
  re-invoking with `--lab <path>` (`cli/src/dispatch.rs`).
- `setup --host bench1` re-execs over SSH.
