<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# cli/ — agent notes

This file holds the internals of the Rust control-plane crate (`cli/`), moved out of the root AGENTS.md to keep it short. The rules in the root AGENTS.md still apply. Per-OS behavior and the Windows port live in `docs/dev/platform-support.md`.

## Rust control plane (`cli/`)

The CLI + orchestration + device glue is rewritten in Rust (the `cli/` crate),
finishing the Python→Rust migration the daemons started. Design + status:
[`notes/config-redesign.md`](../notes/config-redesign.md). Key differences from the
Python tree below:

- **Config is one CLI-managed lab file** (`~/.config/paniolo/lab.toml`, or
  `--lab`/`PANIOLO_LAB`): hosts + targets, each target's hardware as *channels*
  (`netboot`, `serial[]`, `power`, `video`, `hid`, `usb`, `adb`) with per-channel host
  binding.
  Edited surgically via `toml_edit` (hand-comments survive); validated on load
  and before every save. The legacy `~/.config/paniolo/targets/*.toml` files are
  not used by the Rust CLI.
- **Dispatch is per-channel**: a command resolves the host of the channel it
  touches and re-execs there over SSH against a shipped one-target slice.
  Composites (`console`) require co-located channels.
- **Daemons bind OS-assigned ports** (port 0) and are found via their
  `daemon.json` discovery files — fixed defaults collided with stale tunnels.
  A lab file can pin one per channel (`port` on `video`, `hid`, and any one
  `[[serial]]` interface; `--port` on `video set`/`hid set`/`serial add|set`,
  0 clears) so a standing `ssh -L` reaches the dashboard across restarts.
  Every start path honors it (`watch`, `console`, `daemons restart`, the hid
  daemon's `serve --port N`), the dispatch slice carries it, and validation
  refuses interfaces that disagree or two daemons claiming one port on a host.
- **Daemon APIs are authenticated.** Each daemon (serialcap, hdmicap, hidrig,
  ch9329) generates a 32-byte token at start and publishes it as `token` in
  its discovery file, written owner-only (0600, temp file + rename). Every
  request must carry it — `Authorization: Bearer <token>` (the CLI, via
  `daemons::Endpoint`; helper one-shots routed through a daemon) or
  `?token=<token>` (the dashboard's page load, `<img>` and WebSockets, which
  cannot set headers) — and must name a loopback `Host` and, when a browser
  sends one, a loopback `Origin`; `Access-Control-Allow-Origin` echoes that
  one origin, never `*`. The layer is each daemon's `src/auth.rs`,
  byte-identical across the four like `platform.rs`; only hdmicap's vendored
  `/xterm*` assets are exempt. A daemon started by an older paniolo has no
  token (the CLI then sends none) — a named `paniolo daemons restart`
  replaces it. The URL handed to a **browser** always carries `?token=`, and
  `dispatch::remote_daemon_endpoint` reads the token over SSH so a tunnelled
  `console` carries it too — but **`video preview` is the only command that
  prints it**, and the only place that builds one (inline, in its own arm:
  there is no `preview_url` helper any more, so nothing else can reach for it
  by accident). Every other command prints the token-free
  `http://127.0.0.1:<port>` from `Endpoint::base_url`, because that output
  lands in agent transcripts and CI logs (#196). `video preview --open` prints
  nothing openable at all, and a `console` whose browser launch fails writes
  the URL to a 0600 file rather than printing it.
- **Netboot is rust-engine only** (netbootd); the pure-Python DHCP/TFTP engine
  exists only in the legacy tree.
- **Helpers live off PATH** in the private libexec dir
  (`~/.local/libexec/paniolo/bin`): only `paniolo` itself installs to
  `~/.cargo/bin`. `daemons::find_binary` resolves libexec → PATH → legacy
  `~/.cargo/bin`; hook commands (`*_cmd`, hid `cmd`) run via `sh -c` with
  libexec prepended to PATH, so lab files keep referencing helpers by bare
  name. `paniolo helper [NAME] [ARGS…]` lists or runs them directly.
  The helper name and its args are one trailing positional, so everything
  after the name — `--help`, a helper's own `-t`/`-i` — goes to the helper
  (`paniolo helper serialcap --help`). Placement flags go before the name:
  `--host <labhost>`, or `-t <target> [--channel <kind> [-i <iface>]]`, which
  resolve a host (`dispatch::helper_host`) and re-exec `paniolo helper --
  NAME ARGS…` there without shipping a lab slice (`dispatch::run_on_host`;
  `helper` reads no lab). A channel the target lacks is refused rather than
  falling back to the default host (#300). Listing helpers and reading
  bundled helper help do not open hardware or start a daemon.
- **Plugin roots** (`PANIOLO_PLUGIN_PATH`, #303): a PATH-syntax list of
  install prefixes (`std::env::split_paths`; empty entries skipped, relative
  ones ignored with a one-time stderr warning — `daemons::parse_plugin_path`).
  Each adds `<root>/libexec/paniolo/bin` (`daemons::plugin_helper_dirs`) and
  `<root>/share/paniolo/skills`, searched **after** every bundled dir
  (`daemons::all_helper_dirs`, `skills::search_path`), so a plugin adds but
  never shadows a bundled helper or skill (#206's guarantee; no opt-in to
  shadow). `find_binary` and `hook_path` include them; stray detection
  (`list_stray_helpers`) deliberately does not. `paniolo helper`/`skill`
  lists print a plugin entry as `<name>\tplugin: <root>` (name stays the
  first field). The top-level `--plugin-path` flag (before the subcommand,
  not `global`: it would collide with `host add/set --plugin-path`) sets the
  variable in `main()` before anything runs; a host's lab `plugin_path`
  rides on every re-exec there as `--plugin-path=<v>`
  (`Host::paniolo_argv`), an argument so it crosses interactive dispatch and
  a Windows host. `doctor` prints a `plugins` line per root (local env, and
  each remote host's lab field) and its remote hook probes add the plugin
  dirs (`doctor::hook_path_prefix`).
- **Bundled skills are self-describing**: the agent skills under `skills/`
  (`paniolo`, `kvm-puppeting`, `control-host`) install to
  `~/.local/share/paniolo/skills` (and `/usr/share/paniolo/skills` for the
  Linux packages). `paniolo skill [NAME]` lists them with their frontmatter
  descriptions, or prints one `SKILL.md` (`--path` for the file path) — the
  share/ analogue of `paniolo helper`, so an agent can discover and read them
  without the harness pre-loading them (skills.rs).
- **CLI argument convention**: every runtime command takes the target
  **either** as an optional positional (`netboot start pi5`, `video stop pi5`)
  **or** as `-t/--target` (`netboot start -t pi5`) — never both at once, which
  clap refuses. One flattened `TargetArg` (main.rs) supplies both spellings and
  the single `name()` that merges them, so the two cannot drift apart per verb
  the way they had (#203) and so `resolve_single_target`'s "specify one with
  -t" is true of every command that prints it. A new runtime verb flattens
  `TargetArg`; it does not spell out its own target field.
  Channel-config commands (`set`/`add`/`rm`) take `-t/--target` only, and
  refuse a positional: a command that creates or destroys configuration should
  not act on an implicit target. `hid send`, `adb run`, `adb input`, `action
  run`, and the `usb` verbs also take `-t` only, because their positional tail is the
  helper's / `adb`'s args; `serial send` likewise (it reads two positionals as
  `<target> <text>`, one as just the text). `doctor` keeps a bare positional:
  omitting it checks **all** targets rather than a single implied one, so it is
  not the same argument. One config-command exception: `target rename OLD
  NEW` takes two bare positionals and no `-t` (it renames the target itself,
  carrying all channels and lab-file comments; config-only — running daemons
  keep their runtime dirs under the old name, so `stop` and re-`watch` them).
- **Name rule**: target, host, and serial interface names are constrained to
  `model::NAME_RULE` — letters, digits, `.`, `_`, `-`; never `.` or `..`;
  never leading `-` (it would read as an option where the name becomes a
  positional). Enforced by `model::validate_name` in the labfile editor at
  `add`/`rename`/`set` time, not on load — a hand-written lab with an odd name
  still reads, but the CLI never writes one. Names become path components
  (`state::target_dir`, `daemons::runtime_rel`/`sanitize_component`), so
  anything that reaches a filesystem path from a name that predates this rule
  is sanitized again, defensively, at that point.
- **`video set --ocr-mode {text|gui}`** selects the OCR engine on Linux (see
  [dev/ocr.md](../docs/dev/ocr.md#linux-needs-two-engines-the-other-platforms-need-one));
  unset is the platform default everywhere. The field travels with the video
  channel through remote dispatch, same as `--device`.
- **Env forwarding to a control host**: a fixed allowlist (`ssh::FORWARDED_ENV`
  — today just `AMT_PASSWORD`) is carried across a non-interactive dispatch
  (`run`/`run_passthrough`/`run_stdout_to`) over the remote command's stdin, so
  it never appears in the control host's `ps` output; `run_interactive` (`serial
  connect`, `adb shell`) forwards nothing — its stdin is the user's own
  terminal. See
  [distributed-control.md](../docs/distributed-control.md#env-forwarding-to-a-control-host).
- **`paniolo daemons`** is the unified daemon inventory: every discovery-file
  daemon under `/tmp/paniolo-<uid>/` (the per-target capture daemons listed as
  `serialcap[<target>]`, `hdmicap[<target>]`, `hid[<target>]`; plus host-singleton
  zigplug), netbootd via its state files, plus *stray* helper processes running out of
  the libexec dir (wedged one-shots). `paniolo daemons stop [NAME…|--all]
  [--force]` TERMs them (netbootd via its proper interface-restoring stop),
  escalating to KILL with `--force` after a 3 s grace period. A `NAME` is
  either form the list prints: bare `serialcap` (every instance) or
  `'serialcap[pi5]'` (one target's; quote it, `[]` is a shell glob). Before
  any signal the pid's command line is re-checked for the daemon's binary
  name, so a pid the kernel has recycled is skipped with a message, and a
  signal the OS refuses (EPERM) is reported and fails the command rather than
  being printed as `TERM`. A stray is a process whose *program path* (argv[0])
  is in a helper dir — not one whose arguments merely mention it (the `cargo
  install --root …/libexec/paniolo` that `make install` runs) — and children
  of a known daemon (sudo's netbootd on Linux, hdmicap's OCR helper) are
  never strays.
  A stray running a `daemon` subcommand is not a wedged one-shot but an
  **untracked daemon** — one that outlived its discovery file, which is
  paniolo's only record of it. On Linux the runtime base is under `/tmp`, and
  Debian's stock `q /tmp 1777 root root 10d` deletes a discovery file once its
  daemon has been running ten days without a command against it; the daemon
  keeps its capture device, `show` calls the channel stopped, and the next
  `watch` dies on the advisory lock the orphan still holds. `paniolo daemons`
  lists these separately, `video show` / `serial show` report `running,
  untracked (pid N)`, and `video watch` / `video stop` / `serial watch` /
  `serial stop` reap one (TERM, then KILL) before starting a replacement — its
  port and token died with the file, so a signal is the only handle left
  (`daemons::untracked_of`/`untracked_on_devices`, GitHub #187). A daemon is
  matched to a channel by the devices its command line names, and the two
  capture daemons spell that differently: hdmicap takes one `--device`,
  serialcap repeats `--interface NAME=DEVICE@BAUD[:SENSE]`, so the device is
  parsed out of each interface value. An overlap of one device is enough — that
  is the one the replacement would fail to open. The
  `.deb` ships `/usr/lib/tmpfiles.d/paniolo.conf` (`x /tmp/paniolo-*`) so the
  files stop being swept; a `make install` host should add that drop-in itself.
  A daemon keeps running its binary from when it started; an upgrade or rebuild
  replaces that binary on disk but not the running process. The CLI stamps each
  capture daemon's SHA-256 content digest and pid at spawn (`binmeta.json`);
  Linux also reads `/proc/<pid>/exe` to compare the actual running bytes.
  Only a proven content difference is **stale** in `paniolo daemons` (and on
  `serial show` / `video show`). Identical bytes after reinstall/path/mtime
  changes remain current. Missing identity or unavailable content is explicitly
  **binary identity unknown**, never a restart recommendation; metadata alone
  proves nothing. Digest fields are optional so legacy records still load.
  The platform hash tools are best-effort and use OS paths independently of
  helper PATH (Linux /usr/bin/sha256sum, macOS /usr/bin/shasum, Windows
  .NET streaming SHA256 via
  SystemRoot/System32/WindowsPowerShell/v1.0/powershell.exe, without a
  PowerShell module dependency); unavailable hashing keeps the status unknown.
  Automatic watch restarts and `--stale` select only proven content changes.
  `paniolo daemons restart [NAME…|--all|--stale]`
  cleanly cycles serialcap/hdmicap from the current binary (reusing the lab's
  channel config; it waits for the old process to exit so the new one doesn't
  race it for an exclusive device). netbootd is not auto-restarted — cycle it
  via `paniolo netboot start/stop`, since that touches an in-flight boot.
  Replacing a daemon has two traps, both of which bit in #193 when a restart
  across the `0.1.17` token boundary made every graceful stop fail and every
  daemon get SIGKILLed. First, "the old process is gone" is
  `state::is_named_process_pending`, not a bare name match: a process between
  SIGKILL and reaping is alive to `kill(pid, 0)` and *nameless* to a
  command-line match, so a name match alone releases the device to a
  replacement while the corpse still holds its advisory lock. Second, a killed
  daemon cannot remove its own discovery file, so the replacement's startup
  wait must refuse it — `daemons::wait_for_replacement` rejects the pid being
  replaced, and `stop_capture_daemon_and_wait` reaps the file (guarded by pid)
  once the process is gone. Without both, a failed restart reports success and
  quotes the dead daemon's port as the new one's.

```
cli/src/
  main.rs       clap CLI — all command groups + runtime handler bodies
  model.rs      typed lab (serde), validate(), resolved per-channel view, channel_host
  labfile.rs    toml_edit comment-preserving lab editor (the write side)
  error.rs      the error contract: Kind → exit code, PanioloError (kind +
                target/channel/host/child_exit), classify/report for main(),
                hook_failed, shell_code (passthroughs), the --json-errors
                object and the --help exit-status table (docs/errors.md)
  dispatch.rs   per-channel re-exec: slice building/shipping, maybe_dispatch,
                run_subcommand, remote_daemon_endpoint (port + token over ssh)
  ssh.rs        SSH transport: ControlMaster run/passthrough/interactive, forward
                (tunnels); FORWARDED_ENV + stdin_prelude carry an allowlisted var
                (AMT_PASSWORD) to a non-interactive remote command over its stdin
  daemons.rs    shared daemon contract: find_binary (libexec → plugin
                roots → PATH → legacy ~/.cargo/bin), PANIOLO_PLUGIN_PATH
                parsing + helper_entries (the `paniolo helper` list with each
                entry's plugin root), hook_path, daemon.json discovery
                (Endpoint = port + token; get/post attach the bearer header,
                http_url/ws_url carry it as ?token= for a browser), wait
  serial.rs     bounded `serial wait` forwards a receive boundary/deadline/raw
                and total-output caps to serialcap without opening devices;
                MCP serial_wait maps the same read-only surface.
                serialcap orchestration + tio exec + /input + device listing
                + `serial transact` (POST /transact, MCP serial_transact, a
                write tool; -t only, the positional is the text)
                + read-only health_snapshot (`serial status`, MCP serial_status)
                 + offline read_evidence (`serial evidence`, MCP serial_evidence)
  video.rs      hdmicap orchestration (daemon start/stop, client passthrough)
  adb.rs        adb transport (argv build, shell exec, run/input passthrough,
                exec-out screencap → PNG) — a generic transport in core, no helper
  netboot.rs    netbootd lifecycle (spawn with log, stop, status)
  netif.rs      interface discovery/config (sudo), netboot/link/ffx/off modes
  power.rs      generic power hooks (on/off/cycle/state_cmd via sh -c), DTR via
                serialcap /button (+ direct-serial fallback), power_on sense
  state.rs      netboot state files (JSON-compatible with the Python's)
  doctor.rs     config-vs-reality probing (local + over SSH), including
                required netboot tftp_root and whether an interface holds
                its configured host_ip; and the opt-in helper doctor check
                (#304): a power/hid/usb/actions channel with `doctor = true`
                (`--doctor` on its `set` verb) runs `<program> <transport
                options> doctor --json` (doctor_command_for_hook keeps the
                hook up to its verb: VAR= prefixes, program, option words
                with their values; doctor_command_for_prefix keeps a hid/usb
                cmd whole; both refuse a shell operator before the verb), as
                `Probe::HelperDoctor` (local: platform shell in its own
                process group, killed at 10 s; remote: a POSIX watchdog
                script, exit 124 = timed out). interpret_helper_doctor reads
                `{"checks":[{name,status ok|warn|fail,detail}]}`; a non-zero
                exit without a report is `no-check` (not a problem), exit 0
                without one is a contract `FAIL`. Checks print under the
                channel (`power doctor: <name>`); only FAIL counts. On power,
                `doctor = true` also covers every named level (#302), deduped
                by derived command; several runs are labeled by source
                (`power doctor (level hard): <name>`)
  discover.rs   hardware inventory + the configure proposal block
  setup.rs      installer: paniolo CLI onto PATH (~/.cargo/bin); helpers into
                the private libexec dir (~/.local/libexec/paniolo/bin) via
                cargo install --root; bpf-helper setuid, OCR helpers, zigplug
                (uv tool, shim in libexec), Linux groups; --rust-only fast path;
                installs the bundled skills into ~/.local/share/paniolo/skills
  skills.rs     `paniolo skill`: discover + read the bundled agent skills
                (skills_dirs: repo checkout → ~/.local/share → CLI-relative
                share → /usr/share/paniolo/skills, like daemons.rs helper_dirs
                but under share/; then each plugin root's share/paniolo/skills
                via search_path), list with frontmatter descriptions (plugin
                entries annotated with their root), print one SKILL.md (or
                --path), install_bundled() for setup.rs
  actions.rs    named target actions (#299): find, run_hook, the
                requires_power refusal, and the per-target hook lock
                (std File::try_lock, 120 s wait) that power hooks take too
  mcp.rs        `paniolo mcp` (experimental): hand-written MCP server,
                newline-delimited JSON-RPC over stdio (initialize, ping,
                tools/list, tools/call). stdout is the protocol channel, so a
                tool that drives a channel runs this binary as a child (stdin
                null, stdout captured) instead of calling a handler that
                prints; target_list reads the lab in-process. Each
                CLI-backed tool is one cli_call() → argv (a test parses every
                one with the real clap Cli); writes flag an unknown outcome
                on exit 4/22. Design: notes/mcp-server.md; user doc:
                docs/mcp.md
```

**Errors follow the exit-status contract** ([`docs/errors.md`](../docs/errors.md);
design in `docs/dev/error-contract/design.md`). A new failure a consumer might
branch on returns an `error::PanioloError` of the right kind (use
`target_not_found`, `channel_missing`, `hook_failed`, `daemon_down`,
`daemon_request_failed`, `unreachable_host`); a bare `anyhow!` exits 109
`internal`. Never `std::process::exit(status.code()…)` with a child's status
outside the documented passthroughs, and use `error::shell_code` there so a
signal death is 128+N, not 255. Changing a code is a breaking change; adding a
kind means updating `Kind`, `Kind::ALL` (an exhaustive match beside it stops
the test build until you look), `EXIT_STATUS_HELP` (a test pins it to `Kind::ALL`),
`docs/errors.md` and `skills/paniolo/SKILL.md`.

**Helper state/runtime-dir API** (daemons.rs `helper_env`): paniolo exports
`PANIOLO_STATE_DIR` (`~/.config/paniolo/helpers/<name>/`, durable) and
`PANIOLO_RUNTIME_DIR` (`/tmp/paniolo-<uid>/<name>/`, discovery/locks/logs) —
directories pre-created — on every helper invocation: hook commands (named by
the hook's program basename, see `hook_helper_name`; it skips leading
`VAR=value` assignments — `AMT_PASSWORD=... amt-tool cycle` names `amt-tool`,
not the assignment — and sanitizes the result the same way a target name does,
since a hook string is hand-written and never runs through
`model::validate_name`), `paniolo helper` passthrough (sanitized the same way
on its host-singleton branch), and daemon spawns. `doctor.rs` probes a hook's
program the same way (`daemons::first_program_token`), so a credential-prefixed
hook isn't misreported as a missing binary. The per-target capture daemons
(serialcap/hdmicap/hid) append a `/<target>` segment to their runtime dir
(`/tmp/paniolo-<uid>/<name>/<target>/`) so multiple targets capture concurrently
on one host; host-singleton helpers (zigplug/cambrionix/netbootd) use the base
`<name>/` form above. The base honors `$PANIOLO_RUNTIME_BASE` (default `/tmp`).
Channel daemons use the channel name (hidrig
publishes under `hid`). Helpers prefer the env vars, falling back to the same
literal paths standalone; hdmicap/serialcap/hidrig/zigplug all do, and
zigplug lazily migrates its `zigbee.db` from the legacy top-level
`~/.config/paniolo/` location into its namespaced dir. Contract for new
helpers: docs/dev/adding-power-helpers.md.

