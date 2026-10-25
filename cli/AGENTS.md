<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# cli/ — agent notes

This file holds the internals of the Rust control-plane crate (`cli/`), moved out of the root AGENTS.md to keep it short. The rules in the root AGENTS.md still apply. Per-OS behavior and the Windows port live in `docs/dev/platform-support.md`.

## Rust control plane (`cli/`)

The CLI, orchestration and device glue are Rust, finishing the Python→Rust
migration the daemons started. Design: [`notes/config-redesign.md`](../notes/config-redesign.md).

- **Config is one CLI-managed lab file** (`~/.config/panio/lab.toml`, or
  `--lab`/`PANIO_LAB`): hosts + targets, each target's hardware as *channels*
  (`netboot`, `serial[]`, `power`, `video`, `hid`, `usb`, `adb`) with per-channel
  host binding. Edited via `toml_edit` (comments survive); validated on load and
  before every save. Legacy `~/.config/panio/targets/*.toml` is unused.
- **`config sync`** (`labsync.rs`, #375) copies the lab from git (depth-1
  fetch by shelling out to `git`, no new crate), validates it, and writes a
  `<stem>.source.toml` record beside it (source, commit, blob id, fetch time).
  The record's presence makes the lab read-only: `edit_lab`, `cmd_init` and
  `config_edit` call `labsync::ensure_writable` (exit 2 `usage`). A new lab
  writer must call it too. `load_for_read` prints the stale warning (> 7
  days); dispatch slices have no record, so remote re-execs never warn.
- **Dispatch is per-channel**: a command re-execs over SSH on the host of the
  channel it touches, against a shipped one-target slice. Composites (`console`)
  require co-located channels.
- **Daemons bind OS-assigned ports** (port 0) and are found via `daemon.json`
  discovery files (fixed defaults collided with stale tunnels). A lab file can pin
  a port per channel (`port` on `video`, `hid`, any one `[[serial]]` interface;
  `--port` on `video set`/`hid set`/`serial add|set`, 0 clears) so a standing `ssh
  -L` survives restarts. Every start path honors it (`watch`, `console`, `daemons
  restart`, hid `serve --port N`), the dispatch slice carries it, and validation
  refuses disagreeing interfaces or two daemons claiming one port on a host.
- **Daemon APIs are authenticated.** Each daemon (serialcap, hdmicap, hidrig,
  ch9329) makes a 32-byte token at start and publishes it as `token` in its
  discovery file (0600, temp file + rename). Every request must carry it —
  `Authorization: Bearer` (CLI, via `daemons::Endpoint`) or `?token=` (browser page
  load, `<img>`, WebSockets, which cannot set headers) — and name a loopback `Host`
  and, when a browser sends one, a loopback `Origin`; `Access-Control-Allow-Origin`
  echoes that origin, never `*`. The layer is each daemon's `src/auth.rs`,
  byte-identical across the four like `platform.rs`; only hdmicap's vendored
  `/xterm*` assets are exempt. A daemon from an older panio has no token (the CLI
  sends none); a named `daemons restart` replaces it. Browser URLs carry `?token=`
  (`dispatch::remote_daemon_endpoint` reads it over SSH so a tunnelled `console`
  does too). **`video preview` is the only command that prints a token** and the
  only place that builds such a URL (inline, deliberately no helper). Every other
  command prints the token-free `http://127.0.0.1:<port>` from
  `Endpoint::base_url`, because output lands in agent transcripts and CI logs
  (#196). `video preview --open` prints nothing openable; a `console` whose browser
  launch fails writes the URL to a 0600 file instead of printing it.
- **Netboot is rust-engine only** (netbootd); the Python engine exists only in the
  legacy tree.
- **Helpers live off PATH** in the private libexec dir
  (`~/.local/libexec/panio/bin`); only `panio` installs to `~/.cargo/bin`.
  `daemons::find_binary` resolves libexec → PATH → legacy `~/.cargo/bin`. Hook
  commands (`*_cmd`, hid `cmd`) run via `sh -c` with libexec prepended to PATH, so
  lab files use bare helper names. `panio helper [NAME] [ARGS…]` lists or runs
  them; name and args are one trailing positional, so everything after the name
  (`--help`, a helper's own `-t`/`-i`) goes to the helper. Placement flags go
  before the name: `--host <labhost>`, or `-t <target> [--channel <kind> [-i
  <iface>]]`, which resolve a host (`dispatch::helper_host`) and re-exec `panio
  helper -- NAME ARGS…` there with no lab slice (`dispatch::run_on_host`). A
  channel the target lacks is refused, not defaulted (#300). Listing helpers or
  reading their help opens no hardware and starts no daemon.
- **Plugin roots** (`PANIO_PLUGIN_PATH`, #303): PATH-syntax list of install
  prefixes (`std::env::split_paths`; empty entries skipped, relative ones ignored
  with a one-time stderr warning — `daemons::parse_plugin_path`). Each adds
  `<root>/libexec/panio/bin` (`daemons::plugin_helper_dirs`) and
  `<root>/share/panio/skills`, searched **after** every bundled dir
  (`daemons::all_helper_dirs`, `skills::search_path`): a plugin adds but never
  shadows a bundled helper or skill (#206; no opt-in). `find_binary` and
  `hook_path` include them; stray detection (`list_stray_helpers`) deliberately
  does not. `helper`/`skill` lists print `<name>\tplugin: <root>`. The top-level `--plugin-path` flag (before the subcommand; not
  `global`, it would collide with `host add/set --plugin-path`) sets the variable
  in `main()`. A host's lab `plugin_path` rides each re-exec there as
  `--plugin-path=<v>` (`Host::panio_argv`), an argument so it crosses interactive
  dispatch and a Windows host. Remote `doctor` hook probes add the plugin dirs
  (`doctor::hook_path_prefix`).
- **Bundled skills** (`skills/`: `panio`, `kvm-puppeting`, `control-host`) install
  to `~/.local/share/panio/skills` (Linux packages: `/usr/share/panio/skills`).
  `panio skill [NAME]` lists them with frontmatter descriptions or prints one
  `SKILL.md` (`--path` for the file path) (skills.rs).
- **CLI argument convention**: every runtime command takes the target **either** as
  an optional positional (`netboot start pi5`) **or** `-t/--target`, never both
  (clap refuses). One flattened `TargetArg` (main.rs) supplies both spellings and
  the single `name()` merging them, so verbs cannot drift (#203). A new runtime verb
  flattens `TargetArg`; it does not spell out its own target field. Channel-config
  commands (`set`/`add`/`rm`) take `-t/--target` only and refuse a positional: a
  command that creates or destroys configuration must not act on an implicit
  target. `hid send`, `adb run`, `adb input`, `action run` and the `usb` verbs take
  `-t` only because their positional tail is the helper's / `adb`'s args; `serial
  send` likewise (two positionals are `<target> <text>`, one is just the text).
  `doctor` keeps a bare positional; omitting it checks **all** targets. `target
  rename OLD NEW` takes two bare positionals and no `-t`; it is config-only, and
  running daemons keep their runtime dirs under the old name (`stop` and re-`watch`).
- **Name rule**: target, host and serial interface names follow `model::NAME_RULE`
  — letters, digits, `.`, `_`, `-`; never `.` or `..`; never a leading `-` (reads
  as an option). Enforced by `model::validate_name` in the labfile editor at
  `add`/`rename`/`set`, not on load: a hand-written odd name still reads, but the
  CLI never writes one. Names become path components (`state::target_dir`,
  `daemons::runtime_rel`/`sanitize_component`), which sanitize again defensively.
- **`video set --ocr-mode {text|gui}`** selects the Linux OCR engine (see
  [dev/ocr.md](../docs/dev/ocr.md#linux-needs-two-engines-the-other-platforms-need-one));
  unset is the platform default. It travels with the video channel through remote
  dispatch, like `--device`.
- **Env forwarding to a control host**: a fixed allowlist (`ssh::FORWARDED_ENV`,
  today `AMT_PASSWORD`) crosses non-interactive dispatch
  (`run`/`run_passthrough`/`run_stdout_to`) over the remote command's stdin, so it
  never shows in `ps`. `run_interactive` (`serial connect`, `adb shell`) forwards
  nothing; its stdin is the user's terminal. See
  [distributed-control.md](../docs/distributed-control.md#env-forwarding-to-a-control-host).
- **`panio daemons`** is the unified inventory: every discovery-file daemon under
  `/tmp/panio-<uid>/` (`serialcap[<target>]`, `hdmicap[<target>]`,
  `hid[<target>]`; host-singleton zigplug), netbootd via its state files, and
  *stray* helper processes. A stray is a process whose *program path* (argv[0]) is
  in a helper dir, not one merely mentioning it in arguments (the `cargo install
  --root …/libexec/panio` from `make install`); children of a known daemon (sudo's
  netbootd on Linux, hdmicap's OCR helper) are never strays.
  - **Stop**: `daemons stop [NAME…|--all] [--force]` TERMs (netbootd via its
    interface-restoring stop), KILL after 3 s with `--force`. `NAME` is bare
    `serialcap` (all instances) or `'serialcap[pi5]'` (quote it; `[]` is a shell
    glob). Before any signal the pid's command line is re-checked for the daemon's
    binary name, so a recycled pid is skipped. A signal the OS refuses (EPERM) is
    reported and fails the command.
  - **Untracked daemon**: a stray running a `daemon` subcommand is a daemon that
    outlived its discovery file, panio's only record of it. Cause on Linux: the
    runtime base is under `/tmp`, and Debian's `q /tmp 1777 root root 10d` deletes
    the file after ten days without a command against it. The daemon keeps its
    device, `show` calls the channel stopped, and the next `watch` dies on the
    advisory lock the orphan holds. `daemons` lists these separately, `show` reports
    `running, untracked (pid N)`, and `video`/`serial` `watch`/`stop` reap one
    (TERM, then KILL) before starting a replacement; its port and token died with
    the file, so a signal is the only handle (`daemons::untracked_of`/
    `untracked_on_devices`, #187). A daemon matches a channel by the devices on its command line: hdmicap
    takes one `--device`, serialcap repeats `--interface NAME=DEVICE@BAUD[:SENSE]`
    (parse the device out of each); one overlapping device is enough. The `.deb`
    ships `/usr/lib/tmpfiles.d/panio.conf` (`x /tmp/panio-*`); a `make install` host
    must add it itself.
  - **Stale**: a running daemon keeps its start-time binary. The CLI stamps each
    capture daemon's SHA-256 digest and pid at spawn (`binmeta.json`; digest
    fields optional so legacy records load); Linux also reads `/proc/<pid>/exe`.
    Only a proven content difference is **stale** (`daemons`, `serial show`,
    `video show`); identical bytes after a reinstall/path/mtime change stay
    current. Missing identity or unavailable content is **binary identity
    unknown**, never a restart recommendation; metadata alone proves nothing. Hash
    tools are best-effort, at OS paths independent of helper PATH:
    `/usr/bin/sha256sum` (Linux), `/usr/bin/shasum` (macOS), .NET streaming SHA256
    via `SystemRoot/System32/WindowsPowerShell/v1.0/powershell.exe` with no module
    dependency (Windows). Automatic watch restarts and `--stale` act only on
    proven changes.
  - **Restart**: `daemons restart [NAME…|--all|--stale]` cycles serialcap/hdmicap
    from the current binary with the lab's channel config, waiting for the old
    process to exit so the new one does not race it for an exclusive device.
    netbootd is not auto-restarted (use `netboot start/stop`; it touches an
    in-flight boot). Two traps (#193, where a restart across the `0.1.17` token
    boundary made every graceful stop fail and SIGKILLed every daemon):
    1. "Old process gone" is `state::is_named_process_pending`, not a bare name
       match: between SIGKILL and reaping a process is alive to `kill(pid, 0)` but
       nameless to a command-line match, so a name match hands the device to the
       replacement while the corpse still holds its lock.
    2. A killed daemon cannot remove its discovery file, so the replacement's
       startup wait must refuse it: `daemons::wait_for_replacement` rejects the pid
       being replaced, and `stop_capture_daemon_and_wait` reaps the file (guarded
       by pid) once the process is gone. Without both, a failed restart reports
       success and quotes the dead daemon's port.

    `hid stop` also uses the shared `wait_for_daemon_exit` gate after its helper
    accepts shutdown, matching HID helper names rather than the runtime name
    `hid` (#296). Helper stop failures still propagate as `hook_failed`.

```
cli/src/
  main.rs       clap CLI — all command groups + runtime handler bodies
  model.rs      typed lab (serde), validate(), resolved per-channel view, channel_host
  labfile.rs    toml_edit comment-preserving lab editor (write side)
  error.rs      error contract: Kind → exit code, PanioError, shell_code,
                --json-errors, --help exit-status table
  dispatch.rs   per-channel re-exec: slice building/shipping, maybe_dispatch,
                run_subcommand, remote_daemon_endpoint (port + token over ssh)
  ssh.rs        SSH transport: ControlMaster run/passthrough/interactive, forward
                (tunnels); FORWARDED_ENV + stdin_prelude
  daemons.rs    shared daemon contract: find_binary, plugin-path parsing,
                helper_entries, hook_path, daemon.json discovery (Endpoint = port +
                token; http_url/ws_url carry ?token= for a browser), wait
  serial.rs     serialcap orchestration + tio exec + /input + device listing;
                `serial wait` (bounded, opens no devices), `serial transact` (POST
                /transact; write tool; -t only), read-only health_snapshot
                (`serial status`), offline read_evidence (`serial evidence`); each
                has an MCP twin. `serial wait` passes the interface's lab
                `faults` as `--fault NAME=PATTERN` (`serial fault set|rm` edit
                them; `build_slice` carries them to remote hosts)
  video.rs      hdmicap orchestration (daemon start/stop, client passthrough)
  adb.rs        adb transport (argv, shell exec, run/input passthrough, exec-out
                screencap → PNG); generic transport in core, no helper
  netboot.rs    netbootd lifecycle (spawn with log, stop, status)
  netif.rs      interface discovery/config (sudo), netboot/link/ffx/off modes, link hooks (`link_down_cmd` after `down-hard`, `link_up_cmd` before link|netboot|ffx; `run_link_hook`)
  power.rs      generic power hooks (on/off/cycle/state_cmd via sh -c), DTR via
                serialcap /button (+ direct-serial fallback), power_on sense
  state.rs      netboot state files (JSON-compatible with the Python's)
  doctor.rs     config-vs-reality probing (local + over SSH), incl. netboot
                tftp_root and whether an interface holds its host_ip. The remote
                address probe accepts successful empty output (off/ffx); only
                failure of both `ip` and `ifconfig` yields unknown (#251). Opt-in helper
                check (#304): a power/hid/usb/actions channel with `doctor = true`
                (`--doctor` on `set`) runs `<program> <transport options> doctor
                --json` as `Probe::HelperDoctor` (doctor_command_for_hook /
                doctor_command_for_prefix build it; both refuse a shell operator
                before the verb; local: killed at 10 s, remote: exit 124 = timed
                out). interpret_helper_doctor reads `{"checks":[{name,status
                ok|warn|fail,detail}]}`; non-zero exit without a report is
                `no-check`, exit 0 without one is a contract `FAIL`; only FAIL
                counts. Power `doctor = true` also covers every named level (#302),
                deduped by derived command and labeled by source
  discover.rs   hardware inventory + the configure proposal block
  setup.rs      installer: CLI onto PATH; helpers into libexec (cargo install
                --root); bpf-helper setuid, OCR, zigplug (uv tool), Linux groups,
                bundled skills; --rust-only fast path
  skills.rs     `panio skill`: skills_dirs (repo checkout → ~/.local/share →
                CLI-relative share → /usr/share/panio/skills, then plugin roots
                via search_path), install_bundled() for setup.rs
  actions.rs    named target actions (#299): find, run_hook, the requires_power
                refusal, and the per-target hook lock (std File::try_lock, 120 s
                wait) that power hooks take too
  lease.rs      target leases (#360): lease.json store under an exclusive file
                lock, the pure `decide` table, `enforce` (the central check:
                reads allowed, mutating needs PANIO_LEASE, ended tokens refuse
                or warn), `cleanup` (registered procs, serialcap/hdmicap/hid
                daemons, netboot, link down; never power and never the lease
                check, since it runs under the lock), `--lease-file` handoff
  lease_cmd.rs  `panio lease acquire|renew|release|status|cleanup|override-cleanup`
                bodies and `retire` (#372: persists a `State::Blocked` cleanup
                block before cleanup, then clears it with a history line or
                keeps it; owns the record removal, so callers never `remove()`;
                a failed cleanup is `target_cleanup_blocked`, exit 24). main.rs `remote_console` runs a
                `Keepalive` thread (holder read every min(idle/2, 5 min)) while
                a caller with a token has a console open. main.rs `lease_scope` classifies every
                command as Read/Mutating/Unchecked with no `_` arm, so a new
                subcommand must be classified; `lease_check` runs it once in
                `run`. Guide: docs/leases.md, design: notes/target-leases.md
  mcp.rs        `panio mcp` (experimental) stdio JSON-RPC server. stdout is
                the protocol channel, so tools run this binary as a child (stdin
                null, stdout captured) rather than call printing handlers; each
                is one cli_call() → argv (a test parses every one with the real
                clap Cli); writes flag an unknown outcome on exit 4/22. See
                notes/mcp-server.md, docs/mcp.md
```

**Errors follow the exit-status contract** ([`docs/errors.md`](../docs/errors.md);
design in `docs/dev/error-contract/design.md`). A new failure a consumer might
branch on returns an `error::PanioError` of the right kind (`target_not_found`,
`channel_missing`, `hook_failed`, `daemon_down`, `daemon_request_failed`,
`unreachable_host`); a bare `anyhow!` exits 109 `internal`. Never
`std::process::exit(status.code()…)` with a child's status outside the documented
passthroughs, and use `error::shell_code` there so a signal death is 128+N, not
255. Changing a code is a breaking change. Adding a kind means updating `Kind`,
`Kind::ALL` (an exhaustive match beside it stops the test build), `EXIT_STATUS_HELP`
(a test pins it to `Kind::ALL`), `docs/errors.md` and `skills/panio/SKILL.md`.

**Helper state/runtime-dir API** (daemons.rs `helper_env`): on every helper
invocation (hook commands, `panio helper` passthrough, daemon spawns) panio
exports pre-created `PANIO_STATE_DIR` (`~/.config/panio/helpers/<name>/`,
durable) and `PANIO_RUNTIME_DIR` (`/tmp/panio-<uid>/<name>/`,
discovery/locks/logs). A hook is named by its program basename (`hook_helper_name`
skips leading `VAR=value` assignments, so `AMT_PASSWORD=... amt-tool cycle` names
`amt-tool`) and the name is sanitized like a target name, since a hook string never
runs through `model::validate_name`; `panio helper` sanitizes the same way on its
host-singleton branch. `doctor.rs` finds a hook's program the same way
(`daemons::first_program_token`), so a credential-prefixed hook is not reported
missing. Per-target capture daemons (serialcap/hdmicap/hid) append `/<target>` so
targets capture concurrently on one host; host-singleton helpers
(zigplug/cambrionix/netbootd) use the base `<name>/`. The base honors
`$PANIO_RUNTIME_BASE` (default `/tmp`). Channel daemons use the channel name
(hidrig publishes under `hid`). Helpers prefer the env vars and fall back to the
same literal paths standalone (hdmicap/serialcap/hidrig/zigplug do); zigplug
lazily migrates `zigbee.db` from the legacy top-level `~/.config/panio/`.
Contract for new helpers: docs/dev/adding-power-helpers.md.
