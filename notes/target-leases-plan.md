<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Target leases implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `panio lease acquire|renew|release|status`, token-identified holders,
refusal of mutating commands for non-holders, lazy expiry with cleanup, and MCP
lease tools (#360).

**Architecture:** A new `cli/src/lease.rs` owns the record, the store on the
target's default host, and a pure decision function. `run()` classifies each
command as `Read`, `Mutating` or `Unchecked` (an exhaustive match) and stores
it in a process-global; every place a target command decides to run locally
calls `lease::enforce(lab, target)`. The token crosses SSH in `FORWARDED_ENV`
(re-exec) or a shipped file (interactive).

**Tech Stack:** Rust, `cli/` crate only. No new dependencies.

**Spec:** `notes/target-leases.md` (read it first; this plan does not repeat
its rationale).

## Global Constraints

- No new crates (root `AGENTS.md`). Token from `/dev/urandom` on Unix,
  `BCryptGenRandom` (or `RtlGenRandom`) through the existing Windows dependency
  on Windows, behind a `platform.rs` function.
- Portable code goes through `cli/src/platform.rs`, never `libc` at a call site.
- Defaults: duration **2 h**, idle **30 min**; each capped at **24 h**.
- Token: 32 lowercase hex chars (128 bits). Only `lease acquire` prints it.
- Times in the record are Unix epoch seconds (`u64`).
- Lease file `~/.local/share/panio/<target>/lease.json`, mode 0600; history
  `lease-history.jsonl` beside it, keep the last 50 lines.
- New error kind `target_leased`, exit **23** (20s band: may succeed on retry).
- Env var `PANIO_LEASE`. Hidden global flag `--lease-file <PATH>`.
- Unleased targets without `lease_required` behave byte for byte as today.
- Every crate gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`, run from `cli/`.
- No private infrastructure in code, tests, docs or commits (examples use
  `bench1`, `pi5`, `192.0.2.10`).
- Commits end with `Co-Authored-By: Claude Opus 5.5 (1M context)
  <noreply@anthropic.com>`; stage files by name, never `-a`.

## Review Focus

1. **A recycled PID during cleanup.** A registered pid now belongs to an
   unrelated process; cleanup must not kill it (Task 2 + Task 6 test
   `cleanup_skips_recycled_pid`).
2. **Two acquires racing.** Two `acquire` calls at the same instant: exactly
   one wins (Task 3 test `concurrent_acquire_one_winner`, threads against one
   dir).
3. **A corrupt or truncated `lease.json`** (crash mid-write, hand edit): treat
   as unleased for reads, refuse nothing silently, report it in `status`, and
   let `acquire` replace it (Task 3 test `corrupt_record_is_replaced`).
4. **Clock skew between dev machine and control host.** All times are taken
   on the control host; the dev machine never computes expiry (Task 5:
   `--json` reports `expires_in_secs` computed remotely).
5. **A holder's token in a dispatched command's argv or output.** Grep test
   that no code path prints the token except `acquire` (Task 5 test
   `status_never_prints_token`, Task 8 test `lease_file_flag_not_token`).

---

### Task 1: `target_leased` error kind

**Files:**
- Modify: `cli/src/error.rs` (`Kind`, `ALL`, `listed_in_all`, `exit_code`,
  `from_exit_code`, `as_str`, a constructor), `EXIT_STATUS_HELP` (wherever it
  lives; `rg EXIT_STATUS_HELP cli/src`)
- Modify: `docs/errors.md` (row for 23)
- Test: `cli/src/error.rs` tests

**Interfaces:**
- Produces: `Kind::TargetLeased` (code 23, `"target_leased"`);
  `pub fn target_leased(target: &str, message: String) -> PanioError`.

- [ ] **Step 1:** Add the variant to `Kind`, `Kind::ALL` (now 8), and the
  `listed_in_all` match. Build fails until `exit_code`/`from_exit_code`/
  `as_str` handle it; the existing help test fails until `EXIT_STATUS_HELP`
  lists 23.
- [ ] **Step 2:** Test `target_leased_round_trips`:
  `assert_eq!(Kind::from_exit_code(23), Some(Kind::TargetLeased));
  assert_eq!(Kind::TargetLeased.as_str(), "target_leased");`
- [ ] **Step 3:** Implement; add the `docs/errors.md` row: "23 |
  `target_leased` | The target is leased to someone else, or the lab requires
  a lease and none is held. The message names the holder and when the lease
  ends. Wait (`panio lease acquire --wait`) and retry."
- [ ] **Step 4:** `cargo test` in `cli/` passes.
- [ ] **Step 5:** Commit `error: add target_leased (exit 23) for #360`.

### Task 2: process start time in `platform.rs`

**Files:**
- Modify: `cli/src/platform.rs`
- Test: same file

**Interfaces:**
- Produces: `pub fn process_start_time(pid: i32) -> Option<u64>` (an opaque,
  monotonic-per-boot value; equal for the same process, different for a
  recycled pid). `pub fn random_token() -> std::io::Result<String>` (32 hex).

- [ ] **Step 1:** Tests (they execute, per root `AGENTS.md`):
  `start_time_of_self_is_stable` (two calls on `std::process::id()` equal,
  `Some`); `start_time_of_child_differs_from_self` (spawn `sleep 5` /
  `timeout` on Windows, compare, kill); `start_time_of_dead_pid_is_none`
  (spawn, wait, then query); `random_token_shape` (len 32, all
  `[0-9a-f]`, two calls differ).
- [ ] **Step 2:** Run, see them fail to compile.
- [ ] **Step 3:** Implement. Linux: field 22 of `/proc/<pid>/stat`, parsed
  after the last `)`. macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` →
  `pbi_start_tvsec * 1_000_000 + pbi_start_tvusec` (via `libc`). Windows:
  `OpenProcess` + `GetProcessTimes` creation time. Token: read 16 bytes from
  `/dev/urandom`; Windows: `BCryptGenRandom` with
  `BCRYPT_USE_SYSTEM_PREFERRED_RNG` (add the feature to the existing
  `windows-sys` dependency if needed; that is not a new crate).
- [ ] **Step 4:** Tests pass on macOS; note in the commit that Linux and
  Windows run in CI.
- [ ] **Step 5:** Commit `platform: process_start_time and random_token (#360)`.

### Task 3: `lease.rs` core: record, store, decision

**Files:**
- Create: `cli/src/lease.rs` (SPDX header; module doc pointing at
  `notes/target-leases.md`)
- Modify: `cli/src/main.rs` (`mod lease;`), `cli/src/state.rs` (make
  `target_dir` `pub(crate)`)

**Interfaces:**
- Consumes: `platform::process_start_time`, `platform::random_token`,
  `platform::write_atomic`.
- Produces:
  ```rust
  pub const DEFAULT_DURATION: u64 = 7200;
  pub const DEFAULT_IDLE: u64 = 1800;
  pub const MAX_SECS: u64 = 86_400;
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct Proc { pub pid: i32, pub start_time: u64 }
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct Lease { pub owner: String, pub token: String, pub acquired_at: u64,
      pub expires_at: u64, pub duration_secs: u64, pub idle_secs: u64,
      pub last_used_at: u64, pub procs: Vec<Proc> }
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct HistoryEntry { pub at: u64, pub previous_owner: String,
      pub previous_token: String, pub new_owner: Option<String>,
      pub how: String /* "preempted" | "released" | "expired" */,
      pub cleanup_errors: Vec<String> }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum Access { Read, Mutating, Unchecked }
  pub struct Store { dir: PathBuf }              // per target
  impl Store {
      pub fn for_target(target: &str) -> Store;
      pub fn at(dir: PathBuf) -> Store;          // tests
      pub fn lock(&self) -> std::io::Result<StoreLock>;   // exclusive, blocking
  }
  impl StoreLock {                               // all reads/writes go through the lock
      pub fn read(&self) -> Result<Option<Lease>, CorruptLease>;
      pub fn write(&self, l: &Lease) -> std::io::Result<()>;
      pub fn remove(&self) -> std::io::Result<()>;
      pub fn append_history(&self, e: &HistoryEntry) -> std::io::Result<()>;
      pub fn history_for_token(&self, token: &str) -> Option<HistoryEntry>;
  }
  pub enum State { Unleased, Held(Lease), Expired(Lease) }
  pub fn state(l: Option<Lease>, now: u64, alive: impl Fn(&Proc) -> bool) -> State;
  pub enum Verdict { Run, RunAndTouch, Refuse(String), CleanupThenRun(Lease) }
  pub fn decide(state: &State, access: Access, token: Option<&str>,
      lease_required: bool, target: &str, now: u64) -> Verdict;
  pub fn parse_duration(s: &str) -> Result<u64, String>;  // "90s" "30m" "2h", bare = secs
  ```

- [ ] **Step 1: Tests** (in `lease.rs`, against `Store::at(tempdir)`):
  - `parse_duration`: `"90s"→90`, `"30m"→1800`, `"2h"→7200`, `"45"→45`;
    `""`, `"5d"`, `"-1m"`, `"0"` are errors.
  - `state_*`: unleased; held; expired by `now > expires_at`; expired by
    idle (`now > last_used_at + idle_secs`, no live proc); **not** expired by
    idle when a proc is alive (`alive` returns true) but still expired past
    `expires_at`; dead procs are pruned from the returned lease.
  - `decide_*`, one per row of the spec's check: unleased+read→Run;
    unleased+mutating, not required→Run; unleased+mutating,
    required→Refuse containing `"lease_required"` hint; held+matching
    token→RunAndTouch (both access kinds); held+no token+read→Run;
    held+wrong token+mutating→Refuse naming owner and remaining time;
    expired+mutating→CleanupThenRun; expired+read→Run (no cleanup);
    `Unchecked`→Run always.
  - `concurrent_acquire_one_winner`: 8 threads each `lock()`, `read()`, and
    `write()` only if `None`; exactly one write lands (count via a returned
    bool).
  - `corrupt_record_is_replaced`: write `"{"` to `lease.json`; `read()` is
    `Err(CorruptLease)`; a following `write` succeeds and `read` returns it.
  - `file_is_owner_only` (`#[cfg(unix)]`): mode `0o600`.
  - `history_keeps_last_50`.
- [ ] **Step 2:** Run, see failures.
- [ ] **Step 3:** Implement. Lock: `lease.lock` beside the record, held with
  an exclusive advisory lock through a new `platform::lock_exclusive(&File)`
  (Unix `flock(LOCK_EX)`, Windows `LockFileEx`) if `platform.rs` has no such
  helper yet. Refuse message format:
  `"target '{target}' is leased to {owner} until {HH:MM} local ({n} min left); \
  run `panio lease status {target}`, or `panio lease acquire {target} --wait 30m`"`.
- [ ] **Step 4:** `cargo test lease` passes.
- [ ] **Step 5:** Commit `lease: record, store and decision logic (#360)`.

### Task 4: `lease_required` lab field and the cross-host rule

**Files:**
- Modify: `cli/src/model.rs` (`Target.lease_required: Option<bool>`,
  validation), `cli/src/labfile.rs` (`set_target` gains it),
  `cli/src/main.rs` (`target set --lease-required <true|false>`; `target
  show` prints it), `cli/src/dispatch.rs` (`build_slice` copies it)

**Interfaces:**
- Produces: `Target::lease_required(&self) -> bool`;
  `model::target_hosts(rt: &ResolvedTarget) -> BTreeSet<String>` (every host
  any channel resolves to).

- [ ] **Step 1: Tests:** `lease_required_round_trips` (labfile set → reparse
  → true; comments preserved); `slice_carries_lease_required`
  (`build_slice` output parses with it true); `lease_required_on_cross_host_target_is_invalid`
  (target with `video.host = "bench2"` and default `bench1`: `validate`
  errors mentioning `lease_required`); `target_hosts_lists_each_host`.
- [ ] **Step 2–4:** Fail, implement, pass.
- [ ] **Step 5:** Commit `model: lease_required target field (#360)`.

### Task 5: `panio lease` commands

**Files:**
- Modify: `cli/src/main.rs` (new `Command::Lease { cmd: LeaseCmd }`),
  `cli/src/lease.rs` (command bodies)
- Test: `cli/tests/lease.rs` (new; `#![cfg(unix)]`, copy the
  `scratch`/`panio_env` helpers from `error_contract.rs`, set `HOME` to the
  scratch dir)

**Interfaces:**
- Consumes: Task 3 store; Task 4 `target_hosts`; `dispatch::maybe_dispatch`
  with the target's **default host** (dispatch on `ChannelKind` is per
  channel; add `dispatch::maybe_dispatch_default_host(lab, target, mode)` that
  routes to `Target::default_host()`).
- Produces: subcommands, each flattening `TargetArg`:
  `acquire [--owner S] [--duration D] [--idle D] [--wait D] [--preempt] [--json]`,
  `renew [--duration D] [--json]`, `release [--preempt]`, `status [--json]`.
  `acquire --json` prints `{"token","owner","expires_at","expires_in_secs"}`;
  `status --json` prints `{"target","held","owner","expires_in_secs",
  "idle_left_secs","procs","last_change"}` and never a token. Owner default:
  `$USER@<short hostname>` read on the **dev machine** and passed to the
  remote as an explicit `--owner`.

- [ ] **Step 1: Tests** (integration, local lab with `[targets.dut]`, no host):
  `acquire_prints_token_only` (stdout is one 32-hex line);
  `acquire_held_fails_23` (second acquire without token → exit 23, stderr
  names the first owner); `acquire_is_idempotent_for_holder` (same
  `PANIO_LEASE` → exit 0, same token); `acquire_wait_times_out` (`--wait 2s`
  → 23 after ≥2 s); `acquire_wait_succeeds_after_release` (release from a
  thread after 1 s); `preempt_takes_over_and_old_token_is_told`
  (`--preempt` → new token; then old token on `power on` → 23 with
  `"preempted by"`; uses Task 7, so mark `#[ignore]` until Task 7 lands and
  un-ignore there); `renew_extends`; `renew_without_token_fails_23`;
  `release_without_token_fails_23`; `release_with_token_clears`;
  `status_never_prints_token` (status and `status --json` stdout/stderr do
  not contain the token); `acquire_cross_host_target_refused` (exit 3,
  message names both hosts); `duration_cap` (`--duration 25h` → exit 2).
- [ ] **Step 2–4:** Fail, implement, pass. `--wait` polls every 2 s.
  `acquire --preempt` and `release` call `lease::cleanup` (Task 6; until
  then a stub that returns no errors, replaced in Task 6).
- [ ] **Step 5:** Commit `lease: acquire/renew/release/status commands (#360)`.

### Task 6: cleanup

**Files:**
- Modify: `cli/src/lease.rs`

**Interfaces:**
- Consumes: the functions the `serial stop`, `video stop`, `hid stop`,
  `netboot stop` and `netif mode off` handlers call today (find them in
  `main.rs`; call the library functions, not a re-exec of `panio`).
- Produces: `pub fn cleanup(lab: &Lab, target: &str, lease: &Lease) -> Vec<String>`
  (one message per failed step; empty on success). Order: (1) terminate each
  proc whose `process_start_time` still matches, via
  `platform::terminate_pid`; (2) stop serialcap, hdmicap and hid daemons of
  this target and its netboot session, skipping any not running; (3) if the
  target has a netboot channel, `netif mode off` on its interface. Never
  touches power or host-singleton daemons.

- [ ] **Step 1: Tests:** `cleanup_kills_registered_proc` (spawn `sleep 60`,
  register `{pid, start_time}`, cleanup, child is gone);
  `cleanup_skips_recycled_pid` (register a live child's pid with a wrong
  `start_time`; after cleanup it is still alive; kill it in teardown);
  `cleanup_continues_after_failure` (a step that errors still lets the next
  run; returned vec has one entry); `cleanup_with_no_channels_is_noop`.
- [ ] **Step 2–4:** Fail, implement, pass. Replace the Task 5 stub.
- [ ] **Step 5:** Commit `lease: cleanup on release, preempt and expiry (#360)`.

### Task 7: enforcement and process registration

**Files:**
- Modify: `cli/src/main.rs` (classification + calls), `cli/src/dispatch.rs`
  (`maybe_dispatch` `Ok(None)` arm), `cli/src/lease.rs`
- Test: `cli/tests/lease.rs`

**Interfaces:**
- Produces:
  - `fn access_of(cmd: &Command) -> lease::Access` in `main.rs`, an
    exhaustive `match` with no `_` arm on every subcommand enum, following
    the spec's read/mutating table exactly (`video preview` and `console` are
    `Mutating`; config verbs, `discover`, `setup`, `helper`, `skill`, `mcp`,
    `*devices`, `netif vlan`, `lease` itself are `Unchecked`; `daemons` is
    `Unchecked` here and handled in Task 9).
  - `lease::set_access(a: Access)` (a `OnceLock`), called once in `run()`.
  - `pub fn enforce(lab: &Lab, target: &str) -> anyhow::Result<()>`: lock,
    `state`, `decide`; on `CleanupThenRun` run `cleanup`, append an `expired`
    history entry, remove the record; on `RunAndTouch` set `last_used_at` and,
    when `Mutating`, push `{pid: self, start_time}` to `procs`; on `Refuse`
    return `error::target_leased`. When the caller's token matches a history
    entry's `previous_token` with `how == "preempted"`, the refusal says
    `"your lease on '{target}' was preempted by {new_owner} at {HH:MM}"`.
    Reads the token from `PANIO_LEASE` or the `--lease-file` value (Task 8).
- Call `enforce` at: the `Ok(None)` arm of `maybe_dispatch` (it has the lab
  and target), and every local branch that does not go through it. Find them
  with `rg -n 'is_local\(' cli/src/main.rs`; known: `action run` (~5527),
  local `console` (~3436), and any `serial_runtime`/`adb_runtime` path that
  runs locally without `maybe_dispatch`.

- [ ] **Step 1: Tests:**
  - Unit `access_table_matches_spec`: a list of argv vectors, parsed with
    `Cli::try_parse_from`, each asserted to its `Access` (cover every row of
    the spec table, both columns).
  - Integration `every_mutating_command_is_refused`: lab with power hooks
    `true`, a serial device path, video, hid helper `true`, usb, actions,
    netboot, adb; another holder's lease written directly to the store; for
    each mutating argv in a table (one per spec row), assert exit 23. This is
    the test that catches a missed call site.
  - Integration `reads_run_on_a_leased_target`: same lab, each read argv,
    assert exit is **not** 23.
  - Integration `holder_runs_mutating` (`power on` with hook `true`, matching
    `PANIO_LEASE` → 0, and `last_used_at` advanced).
  - Integration `unleased_lab_unchanged`: no lease, no `lease_required`,
    `power on` → 0.
  - Integration `lease_required_refuses_unleased`.
  - Integration `expired_lease_cleaned_by_mutating_not_by_read`: write an
    expired lease; `serial log` leaves `lease.json` in place; `power on`
    removes it and appends `expired` to history.
  - Un-ignore `preempt_takes_over_and_old_token_is_told`.
- [ ] **Step 2–4:** Fail, implement, pass.
- [ ] **Step 5:** Commit `lease: enforce on every target command (#360)`.

### Task 8: carrying the token over SSH

**Files:**
- Modify: `cli/src/ssh.rs` (`FORWARDED_ENV` gains `"PANIO_LEASE"`; update its
  doc and `run_interactive`'s doc), `cli/src/dispatch.rs` (`dispatch` with
  `Mode::Interactive` ships a token file), `cli/src/main.rs` (hidden global
  `--lease-file <PATH>`)

**Interfaces:**
- Produces: `dispatch::ship_token(host, host_name, target, token) ->
  anyhow::Result<String>` (same naming and cleanup pattern as `ship_slice`,
  name `.panio-lease-<pid>-<nanos>`); `remote_argv` gains `--lease-file <rel>`
  when a token was shipped. The remote reads the file, deletes it, and uses
  its trimmed content as the token (overriding `PANIO_LEASE`).

- [ ] **Step 1: Tests:** `forwarded_env_includes_panio_lease`
  (`collect_forwarded(FORWARDED_ENV, …)` with a fake lookup returns it);
  `lease_file_flag_not_token` (`remote_argv` for an interactive dispatch
  contains `--lease-file` and a path, never the token text);
  integration `lease_file_is_read_and_deleted` (local run with `--lease-file
  <scratch>/t` holding the holder's token: `power on` exits 0 and the file is
  gone).
- [ ] **Step 2–4:** Fail, implement, pass.
- [ ] **Step 5:** Commit `dispatch: carry PANIO_LEASE to the control host (#360)`.

### Task 9: `daemons stop|restart`

**Files:**
- Modify: `cli/src/main.rs` (`cmd_daemons_*`)

**Interfaces:**
- Consumes: `lease::Store::for_target`, `lease::state`.
- Produces: for each daemon instance with a target name, if that target's
  local lease is `Held` and `PANIO_LEASE` does not match: a named instance →
  exit 23; under `--all` → skipped, and a line `skipped <daemon>/<target>:
  leased to <owner>` on stderr. Daemons with no target (host singletons) are
  unaffected.

- [ ] **Step 1: Tests:** unit over the selection function:
  `named_leased_daemon_refused`, `all_skips_leased`, `holder_can_stop`,
  `unleased_unchanged`.
- [ ] **Step 2–4:** Fail, implement, pass.
- [ ] **Step 5:** Commit `daemons: respect target leases (#360)`.

### Task 10: MCP lease tools

**Files:**
- Modify: `cli/src/mcp.rs`, `notes/mcp-server.md`

**Interfaces:**
- Produces: tools `lease_acquire {target, owner?, duration?, idle?, wait?,
  preempt?}`, `lease_renew {target, duration?}`, `lease_release {target,
  preempt?}`, `lease_status {target?}`. The server holds `tokens:
  Mutex<HashMap<String, String>>`; `lease_acquire` stores the token and its
  result text says the lease is held **without** the token; every
  `run_child` for a target sets `PANIO_LEASE` from the map (or from the
  server's initial `PANIO_LEASE` env when the map has none). `writes: true`
  for acquire, renew and release.

- [ ] **Step 1: Tests:** `lease_acquire_result_hides_token`;
  `child_gets_token_for_its_target` (inspect the `Command` env built for a
  call, via a seam that returns the `Command` instead of running it);
  `initial_env_token_used_when_none_acquired`; `tool_list_includes_lease_tools`.
- [ ] **Step 2–4:** Fail, implement, pass.
- [ ] **Step 5:** Commit `mcp: lease tools hold the token for their children (#360)`.

### Task 11: documentation, skill, evals

**Files:**
- Create: `docs/leases.md` (add to `mkdocs.yml` `nav:`)
- Modify: `docs/distributed-control.md` ("What's deferred": locking now
  exists for single-host targets; cross-host still deferred),
  `skills/panio/SKILL.md` (lease workflow, hand the token to sub-agents,
  exit 23), `README.md` (capabilities), root `AGENTS.md` (Purpose list,
  runtime paths table row for `lease.json`), `cli/AGENTS.md` (module map),
  `evals/scenarios/` (new `lease-*.toml`: acquire, power-cycle, release), `notes/target-leases.md`
  (status → "built")

- [ ] **Step 1:** Write `docs/leases.md`: Terms block; shared-lab workflow;
  parallel-agents workflow (`export PANIO_LEASE=$(panio lease acquire pi5)`,
  pass to sub-agents); MCP; command reference; the read/mutating table
  copied from the spec; `lease_required`; limits (no cross-host, not a
  security boundary).
- [ ] **Step 2:** Verify rendered help: `panio lease --help`, each
  subcommand's `--help`, `panio target set --help`.
- [ ] **Step 3:** `python3.12 evals/run.py --check` passes; `mkdocs build
  --strict` passes if mkdocs is installed, else note it for CI.
- [ ] **Step 4:** Full gate from `cli/`: `cargo fmt --check && cargo clippy
  --all-targets -- -D warnings && cargo test`; `bash scripts/ci-actions-pinned.sh`
  unaffected; grep the diff for private names/addresses.
- [ ] **Step 5:** Commit `docs: target leases (#360)`. The PR body uses
  `Fixes #360`.
