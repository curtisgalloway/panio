// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! Whole-target leases: the on-disk record, a locked store, and the pure
//! decision logic. Design: `notes/target-leases.md`.
//!
//! Everything here is local to the host that runs it. The record lives at
//! `<state dir>/<target>/lease.json`; every read-modify-write goes through
//! [`StoreLock`], an exclusive file lock on `lease.lock` beside it, so two
//! panio processes can never both decide a target is free.

use std::fs::File;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::platform;

/// Default lease length when `--duration` is not given: 2 hours.
pub const DEFAULT_DURATION: u64 = 7200;
/// Default idle timeout: 30 minutes without a command or a live process.
pub const DEFAULT_IDLE: u64 = 1800;
/// No lease, and no renewal, reaches more than 24 hours from now.
pub const MAX_SECS: u64 = 86_400;
/// How many history lines are kept.
const HISTORY_KEEP: usize = 50;

/// A process registered under a lease. The start time makes the identity
/// survive pid reuse.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: i32,
    pub start_time: u64,
}

/// The lease record. Times are Unix epoch seconds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Lease {
    pub owner: String,
    pub token: String,
    pub acquired_at: u64,
    pub expires_at: u64,
    pub duration_secs: u64,
    pub idle_secs: u64,
    pub last_used_at: u64,
    pub procs: Vec<Proc>,
    /// Present while the previous holder's cleanup is unfinished or failed:
    /// the target is blocked and nobody holds it. Absent on every ordinary
    /// record, including those written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupBlock>,
}

/// How far ahead the outer fields of a blocked record are pushed, so a CLI
/// that ignores [`Lease::cleanup`] still sees a lease nobody can idle out.
const BLOCKED_HORIZON_SECS: u64 = 10 * 365 * 86_400;

/// Where a [`CleanupBlock`] stands.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CleanupPhase {
    /// Written before cleanup ran. Seen later only when the process that was
    /// cleaning up died before recording a result.
    Pending,
    /// Cleanup ran and at least one step failed.
    Failed,
}

impl CleanupPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            CleanupPhase::Pending => "pending",
            CleanupPhase::Failed => "failed",
        }
    }
}

/// How one cleanup step ended.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// The step changed something.
    Ok,
    /// There was nothing to do: the daemon was not running, the process was
    /// already gone.
    AlreadySatisfied,
    Failed,
}

/// One cleanup step's outcome. `step` is a stable id (`registered_processes`,
/// `serial_daemon`, `video_daemon`, `hid_daemon`, `netboot_session`,
/// `netif_mode_off`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StepResult {
    pub step: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl StepResult {
    pub fn failed(&self) -> bool {
        self.status == StepStatus::Failed
    }
}

/// What a target is blocked on: the departed holder's cleanup, and enough
/// about it to retry or to explain an override later.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CleanupBlock {
    /// Names this block in `lease cleanup --id`. Public, unlike the token: it
    /// is printed.
    pub cleanup_id: String,
    pub phase: CleanupPhase,
    /// How the lease ended: `"expired"`, `"released"` or `"preempted"`.
    pub how: String,
    pub ended_at: u64,
    pub previous_owner: String,
    /// The ended holder's token, kept so the history can explain it. Never
    /// printed.
    pub previous_token: String,
    pub new_owner: Option<String>,
    /// The registered processes the retry must still terminate.
    pub procs: Vec<Proc>,
    /// The target's cleanup-relevant config when the lease ended
    /// ([`scope_snapshot`]); a retry under different config needs an override.
    pub scope: serde_json::Value,
    #[serde(default)]
    pub last_attempt_at: Option<u64>,
    #[serde(default)]
    pub results: Vec<StepResult>,
}

impl CleanupBlock {
    /// Ids of the steps that failed.
    pub fn failed_steps(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| r.failed())
            .map(|r| r.step.as_str())
            .collect()
    }
}

impl CleanupBlock {
    /// The failed steps as history lines; a pending block that never recorded
    /// a result says so instead.
    pub fn failure_lines_or_unfinished(&self) -> Vec<String> {
        let lines = failure_lines(&self.results);
        if lines.is_empty() && self.phase == CleanupPhase::Pending {
            return vec!["cleanup did not finish".to_string()];
        }
        lines
    }
}

/// `"<step>: <message>"` for each failed result.
pub fn failure_lines(results: &[StepResult]) -> Vec<String> {
    results
        .iter()
        .filter(|r| r.failed())
        .map(|r| format!("{}: {}", r.step, r.message.as_deref().unwrap_or("failed")))
        .collect()
}

impl Lease {
    /// The cleanup block, when this record is one.
    pub fn cleanup_block(&self) -> Option<&CleanupBlock> {
        self.cleanup.as_ref()
    }

    /// The record that blocks the target after `old` ended. The outer fields
    /// describe a lease nobody can token into and that outlives any idle or
    /// duration check, so a CLI that does not know about `cleanup` still
    /// refuses other callers instead of reusing the target. (Such a CLI's
    /// `--preempt` still overrides it; the block binds only CLIs that read
    /// the field.)
    pub(crate) fn blocked(old: &Lease, block: CleanupBlock, now: u64) -> std::io::Result<Lease> {
        let horizon = now.saturating_add(BLOCKED_HORIZON_SECS);
        Ok(Lease {
            owner: format!("cleanup-blocked (was {})", old.owner),
            token: platform::random_token()?,
            acquired_at: old.acquired_at,
            expires_at: horizon,
            duration_secs: MAX_SECS,
            idle_secs: MAX_SECS,
            last_used_at: horizon,
            procs: Vec::new(),
            cleanup: Some(block),
        })
    }
}

/// One line of `lease-history.jsonl`: why a token stopped working.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct HistoryEntry {
    pub at: u64,
    pub previous_owner: String,
    pub previous_token: String,
    pub new_owner: Option<String>,
    /// `"preempted"`, `"released"`, `"expired"` or `"corrupt"`.
    pub how: String,
    pub cleanup_errors: Vec<String>,
    /// How a cleanup block was cleared: `"verified"` (a retry succeeded) or
    /// `"overridden"` (a person cleared it without verification). Separate
    /// from `how`, which keeps the original end reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_id: Option<String>,
    /// The reason given to `override-cleanup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What a command does to the target, for the lease check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Mutating,
    /// Not checked at all (lab editing, host-wide commands, the lease
    /// commands themselves, which do their own arbitration).
    Unchecked,
}

/// `lease.json` exists but cannot be used. A mutation is refused until
/// `lease override-cleanup --corrupt` moves the file aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptLease(pub String);

impl std::fmt::Display for CorruptLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unreadable lease record: {}", self.0)
    }
}

impl std::error::Error for CorruptLease {}

/// Why [`StoreLock::read`] failed. `Corrupt` blocks the target until it is
/// quarantined; `Io` (permissions, a directory in the way, a failing disk)
/// must not be overwritten, so callers surface it.
#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    Corrupt(CorruptLease),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "cannot read lease record: {e}"),
            ReadError::Corrupt(c) => c.fmt(f),
        }
    }
}

impl std::error::Error for ReadError {}

/// Unix seconds now. One clock for every lease timestamp.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True while the registered process is still the same process: its pid
/// exists *and* carries the recorded start time, so a recycled pid fails the
/// comparison. On Linux a zombie (exited, not yet reaped) passes the pid probe
/// but has no start time (`platform::process_start_time` reads it as gone), so
/// it counts as dead there; elsewhere a zombie reads as alive until its parent
/// reaps it.
pub fn proc_is_live(p: &Proc) -> bool {
    platform::pid_alive(p.pid) && platform::process_start_time(p.pid) == Some(p.start_time)
}

/// Per-target lease storage.
pub struct Store {
    dir: PathBuf,
}

/// An exclusive hold on a target's lease store. The lock goes away when this
/// is dropped (the file closes), or when the process dies.
pub struct StoreLock {
    dir: PathBuf,
    _file: File,
}

impl Store {
    pub fn for_target(target: &str) -> Store {
        Store {
            dir: crate::state::target_dir(target),
        }
    }

    /// A store rooted at `dir` (tests).
    #[cfg(test)]
    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
    }

    /// Whether a history file exists, without creating anything.
    pub fn history_exists(&self) -> bool {
        self.dir.join("lease-history.jsonl").exists()
    }

    /// Whether a lease record file exists, without creating anything.
    pub fn record_exists(&self) -> bool {
        self.dir.join("lease.json").exists()
    }

    /// Block until this process holds the store's exclusive lock.
    pub fn lock(&self) -> std::io::Result<StoreLock> {
        std::fs::create_dir_all(&self.dir)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(self.dir.join("lease.lock"))?;
        platform::lock_exclusive(&file)?;
        Ok(StoreLock {
            dir: self.dir.clone(),
            _file: file,
        })
    }
}

impl StoreLock {
    fn record_path(&self) -> PathBuf {
        self.dir.join("lease.json")
    }

    fn history_path(&self) -> PathBuf {
        self.dir.join("lease-history.jsonl")
    }

    /// The current record: `Ok(None)` when there is none,
    /// `Err(Corrupt)` when the file is not a valid record, `Err(Io)` when it
    /// cannot be read at all.
    pub fn read(&self) -> Result<Option<Lease>, ReadError> {
        let text = match std::fs::read_to_string(self.record_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(ReadError::Corrupt(CorruptLease(e.to_string())))
            }
            Err(e) => return Err(ReadError::Io(e)),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| ReadError::Corrupt(CorruptLease(e.to_string())))
    }

    pub fn write(&self, l: &Lease) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(l).map_err(std::io::Error::other)?;
        write_private(&self.record_path(), &data)
    }

    /// Move an unreadable record aside as `lease.json.corrupt-<ts>` and return
    /// the new path.
    pub fn quarantine(&self, ts: u64) -> std::io::Result<PathBuf> {
        let to = self.dir.join(format!("lease.json.corrupt-{ts}"));
        std::fs::rename(self.record_path(), &to)?;
        Ok(to)
    }

    /// Delete the record. Already gone is success.
    pub fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.record_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn history(&self) -> Vec<HistoryEntry> {
        let Ok(text) = std::fs::read_to_string(self.history_path()) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Append a line and keep only the newest 50.
    pub fn append_history(&self, e: &HistoryEntry) -> std::io::Result<()> {
        let mut all = self.history();
        all.push(e.clone());
        let skip = all.len().saturating_sub(HISTORY_KEEP);
        let mut out = Vec::new();
        for entry in &all[skip..] {
            serde_json::to_writer(&mut out, entry).map_err(std::io::Error::other)?;
            out.push(b'\n');
        }
        write_private(&self.history_path(), &out)
    }

    /// The newest history line, if any.
    pub fn last_history(&self) -> Option<HistoryEntry> {
        self.history().pop()
    }

    /// The newest history line whose previous holder used `token`.
    pub fn history_for_token(&self, token: &str) -> Option<HistoryEntry> {
        self.history()
            .into_iter()
            .rev()
            .find(|e| e.previous_token == token)
    }
}

/// Atomic write, owner-only. A new file starts 0600; the explicit chmod
/// covers an older file that `write_atomic` carried a wider mode over from.
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    platform::write_atomic(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A lease as the check sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Unleased,
    Held(Lease),
    Expired(Lease),
    /// The previous holder's cleanup did not finish; carries the blocking
    /// record. Nobody holds the target and nobody can acquire it.
    Blocked(Lease),
}

/// Classify a record at `now`. Registered processes for which `alive` is
/// false are dropped from the returned lease. A record with a cleanup block is
/// `Blocked` before anything else is looked at, and never expires. A lease is
/// expired past `expires_at`, or when idle (`last_used_at + idle_secs` has
/// passed) with no live registered process.
pub fn state(l: Option<Lease>, now: u64, alive: impl Fn(&Proc) -> bool) -> State {
    let Some(mut lease) = l else {
        return State::Unleased;
    };
    if lease.cleanup.is_some() {
        return State::Blocked(lease);
    }
    lease.procs.retain(|p| alive(p));
    let past_expiry = now > lease.expires_at;
    let idle = now > lease.last_used_at.saturating_add(lease.idle_secs) && lease.procs.is_empty();
    if past_expiry || idle {
        State::Expired(lease)
    } else {
        State::Held(lease)
    }
}

/// What the check tells the caller to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Run,
    /// Run, and bump `last_used_at` and register the process.
    RunAndTouch,
    /// Do not run; the string is the user-facing reason.
    Refuse(String),
    /// Clean up the expired holder's leftovers, then run.
    CleanupThenRun(Lease),
    /// Clean up the expired holder's leftovers, then refuse: the target is
    /// now unleased and `lease_required` is set.
    CleanupThenRefuse(Lease, String),
    /// Do not run: the target's last cleanup failed. The string is the
    /// refusal; the error kind is `target_cleanup_blocked`.
    CleanupBlocked(String),
}

/// `HH:MM local`, or `HH:MM UTC` when the system cannot convert.
fn clock(epoch: u64) -> String {
    if let Some(t) = platform::local_hhmm(epoch) {
        return format!("{t} local");
    }
    let secs = epoch % 86_400;
    format!("{:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60)
}

/// The lease check's decision table (spec "The check").
pub fn decide(
    state: &State,
    access: Access,
    token: Option<&str>,
    lease_required: bool,
    target: &str,
    now: u64,
) -> Verdict {
    if access == Access::Unchecked {
        return Verdict::Run;
    }
    let required_msg = || {
        format!(
            "target '{target}' has lease_required = true and no lease is held; \
             run `panio lease acquire {target}` and export the token as PANIO_LEASE"
        )
    };
    match state {
        State::Unleased => {
            if access == Access::Mutating && lease_required {
                Verdict::Refuse(required_msg())
            } else {
                Verdict::Run
            }
        }
        State::Expired(lease) => {
            if access == Access::Mutating && lease_required {
                Verdict::CleanupThenRefuse(lease.clone(), required_msg())
            } else if access == Access::Mutating {
                Verdict::CleanupThenRun(lease.clone())
            } else {
                Verdict::Run
            }
        }
        State::Blocked(lease) => {
            if access == Access::Mutating {
                Verdict::CleanupBlocked(blocked_message(target, lease))
            } else {
                Verdict::Run
            }
        }
        State::Held(lease) => {
            if token == Some(lease.token.as_str()) {
                Verdict::RunAndTouch
            } else if access == Access::Read {
                Verdict::Run
            } else {
                let mins = lease.expires_at.saturating_sub(now).div_ceil(60);
                Verdict::Refuse(format!(
                    "target '{target}' is leased to {} until {} ({mins} min left); \
                     run `panio lease status {target}`, or \
                     `panio lease acquire {target} --wait 30m`",
                    lease.owner,
                    clock(lease.expires_at),
                ))
            }
        }
    }
}

/// The refusal for a target whose last cleanup did not finish: names the
/// failed steps, the cleanup id, and both recovery commands.
pub fn blocked_message(target: &str, lease: &Lease) -> String {
    let Some(b) = lease.cleanup.as_ref() else {
        return format!("target '{target}' is cleanup-blocked");
    };
    let what = match b.phase {
        CleanupPhase::Failed => format!("failed ({})", b.failed_steps().join(", ")),
        CleanupPhase::Pending => "did not finish".to_string(),
    };
    format!(
        "target '{target}' is cleanup-blocked: cleaning up after {}'s lease ({}) {what}; \
         cleanup id {id}; retry with `panio lease cleanup {target} --id {id}`, or clear \
         it without verification with `panio lease override-cleanup {target} --id {id} \
         --reason TEXT`",
        b.previous_owner,
        b.how,
        id = b.cleanup_id,
    )
}

/// The refusal for an unreadable record.
pub fn corrupt_message(target: &str, c: &CorruptLease) -> String {
    format!(
        "target '{target}' is cleanup-blocked: its lease record is unreadable ({}); \
         move it aside with `panio lease override-cleanup {target} --corrupt --reason TEXT`",
        c.0
    )
}

/// Why a lease stops this caller from touching a target's daemons.
#[derive(Debug, PartialEq)]
pub struct Block {
    pub owner: String,
    /// `HH:MM local` when the lease ends; empty for a cleanup block.
    pub until: String,
    /// The full refusal text, the same wording the central check uses.
    pub message: String,
    /// The target is blocked on a failed cleanup (or an unreadable record)
    /// rather than leased: the refusal is `target_cleanup_blocked`.
    pub cleanup_blocked: bool,
}

/// `Some` only when `state` is a live lease held by someone other than the
/// holder of `token`, or a cleanup block (which binds everyone). An unleased
/// or expired target blocks no one.
pub fn block_for(state: &State, token: Option<&str>, target: &str, now: u64) -> Option<Block> {
    if let State::Blocked(lease) = state {
        let owner = lease
            .cleanup
            .as_ref()
            .map_or_else(|| lease.owner.clone(), |b| b.previous_owner.clone());
        return Some(Block {
            owner,
            until: String::new(),
            message: blocked_message(target, lease),
            cleanup_blocked: true,
        });
    }
    let State::Held(lease) = state else {
        return None;
    };
    match decide(state, Access::Mutating, token, false, target, now) {
        Verdict::Refuse(message) => Some(Block {
            owner: lease.owner.clone(),
            until: clock(lease.expires_at),
            message,
            cleanup_blocked: false,
        }),
        _ => None,
    }
}

/// [`block_for`] against the lease on disk for `target`, for callers that run
/// without a lab (`daemons stop|restart`). Reads only: it never creates the
/// state directory, cleans up an expired lease, or touches a held one. A
/// target with no `lease.json` is not blocked; an unreadable file (corrupt)
/// blocks; a record that cannot be opened at all is reported on stderr and
/// not treated as a block.
pub fn foreign_holder(target: &str) -> Option<Block> {
    let store = Store::for_target(target);
    if !store.record_exists() {
        return None;
    }
    let lock = match store.lock() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("warning: {target}: cannot read its lease: {e}");
            return None;
        }
    };
    let rec = match lock.read() {
        Ok(r) => r,
        Err(ReadError::Corrupt(c)) => {
            return Some(Block {
                owner: "unknown".to_string(),
                until: String::new(),
                message: corrupt_message(target, &c),
                cleanup_blocked: true,
            });
        }
        Err(e) => {
            eprintln!("warning: {target}: cannot read its lease: {e}");
            return None;
        }
    };
    let now = now();
    block_for(
        &state(rec, now, proc_is_live),
        caller_token().as_deref(),
        target,
        now,
    )
}

/// What one cleanup step found.
type StepOutcome = Result<StepStatus, String>;

/// One cleanup step: a stable id and the work itself.
type Step<'a> = (&'static str, Box<dyn FnOnce() -> StepOutcome + 'a>);

/// Run every step in order, whatever happens to the earlier ones.
fn run_steps(steps: Vec<Step<'_>>) -> Vec<StepResult> {
    steps
        .into_iter()
        .map(|(step, work)| match work() {
            Ok(status) => StepResult {
                step: step.to_string(),
                status,
                message: None,
            },
            Err(message) => StepResult {
                step: step.to_string(),
                status: StepStatus::Failed,
                message: Some(message),
            },
        })
        .collect()
}

/// Map a local capture-daemon stop to a step outcome.
fn stopped(r: anyhow::Result<crate::Stopped>) -> StepOutcome {
    match r {
        Ok(crate::Stopped::Daemon(st)) if !st.success() => Err(format!("stop exited with {st}")),
        Ok(_) => Ok(StepStatus::Ok),
        Err(e) => Err(e.to_string()),
    }
}

/// Terminate a registered process that is still the same process (pid plus
/// start time, so a recycled pid is never signaled): TERM, a short grace, then
/// KILL after re-checking identity, then confirm it is gone.
fn terminate_registered(p: &Proc) -> Result<(), String> {
    terminate_with(
        p.pid,
        || proc_is_live(p),
        |sig| platform::try_signal_pid(p.pid, sig),
        std::time::Duration::from_secs(2),
    )
}

/// [`terminate_registered`] over injected OS primitives: `is_live` is the
/// identity-checked liveness test, `signal` sends one signal, and `grace`
/// bounds each wait (after TERM, and again after KILL).
fn terminate_with(
    pid: i32,
    is_live: impl Fn() -> bool,
    signal: impl Fn(platform::Signal) -> std::io::Result<()>,
    grace: std::time::Duration,
) -> Result<(), String> {
    let wait_gone = || {
        let deadline = std::time::Instant::now() + grace;
        while is_live() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        !is_live()
    };
    if !is_live() {
        return Ok(());
    }
    if let Err(e) = signal(platform::Signal::Term) {
        if !is_live() {
            return Ok(());
        }
        return Err(format!("could not stop pid {pid}: {e}"));
    }
    if wait_gone() {
        return Ok(());
    }
    if let Err(e) = signal(platform::Signal::Kill) {
        if is_live() {
            return Err(format!("could not kill pid {pid}: {e}"));
        }
        return Ok(());
    }
    if wait_gone() {
        Ok(())
    } else {
        Err(format!("pid {pid} survived SIGKILL"))
    }
}

/// The part of `target`'s config that [`cleanup`] acts on: its host binding
/// and its serial, video, hid and netboot channels, as far as cleanup reads
/// them. Not power, not the rest of the lab. A missing target is `Null`. A
/// retry compares this with the snapshot stored when the lease ended.
pub fn scope_snapshot(lab: &crate::model::Lab, target: &str) -> serde_json::Value {
    use serde_json::json;
    let Some(t) = lab.targets.get(target) else {
        return serde_json::Value::Null;
    };
    json!({
        "host": t.host,
        "serial": t.serial.iter().map(|s| json!({
            "name": s.name,
            "device": s.device,
            "host": s.host,
            "port": s.port,
        })).collect::<Vec<_>>(),
        "video": t.video.as_ref().map(|v| json!({
            "device": v.device,
            "host": v.host,
            "port": v.port,
        })),
        "hid": t.hid.as_ref().map(|h| json!({
            "cmd": h.cmd,
            "host": h.host,
            "port": h.port,
        })),
        "netboot": t.netboot.as_ref().map(|n| json!({
            "interface": n.interface,
            "host_ip": n.effective_host_ip(),
            "host": n.host,
        })),
    })
}

/// Tear down what a departing holder left on the target: its registered
/// processes, the target's daemons, the netboot link. One result per step the
/// target has a channel for, in a fixed order; a daemon that is not running
/// is `already_satisfied`, never omitted. A target missing from the lab has
/// no steps.
///
/// Runs with the lease's [`StoreLock`] held, so it calls only the library
/// functions beneath the runtime handlers (`serial_stop_local`,
/// `video_stop_local`, `hid_stop_local`, `netboot::stop`,
/// `netif::mode_off`), never `panio` itself and never `lease::enforce`.
pub fn cleanup(lab: &crate::model::Lab, target: &str, procs: &[Proc]) -> Vec<StepResult> {
    let Some(t) = lab.targets.get(target) else {
        return Vec::new();
    };
    let mut steps: Vec<Step<'_>> = Vec::new();
    steps.push((
        "registered_processes",
        Box::new(move || {
            let live: Vec<&Proc> = procs.iter().filter(|p| proc_is_live(p)).collect();
            if live.is_empty() {
                return Ok(StepStatus::AlreadySatisfied);
            }
            let errs: Vec<String> = live
                .into_iter()
                .filter_map(|p| terminate_registered(p).err())
                .collect();
            if errs.is_empty() {
                Ok(StepStatus::Ok)
            } else {
                Err(errs.join("; "))
            }
        }),
    ));
    if !t.serial.is_empty() {
        let serials = &t.serial;
        steps.push((
            "serial_daemon",
            Box::new(move || {
                if crate::serial::daemon(target).is_none()
                    && crate::serial::untracked(serials).is_none()
                {
                    return Ok(StepStatus::AlreadySatisfied);
                }
                stopped(crate::serial_stop_local(target, serials))
            }),
        ));
    }
    if let Some(v) = &t.video {
        let device = v.device.as_deref();
        steps.push((
            "video_daemon",
            Box::new(move || {
                if crate::video::daemon(target).is_none()
                    && device.is_none_or(|d| crate::video::untracked(d).is_none())
                {
                    return Ok(StepStatus::AlreadySatisfied);
                }
                stopped(crate::video_stop_local(target, device))
            }),
        ));
    }
    if t.hid.is_some() {
        steps.push((
            "hid_daemon",
            Box::new(move || {
                if crate::daemons::daemon_pid("hid", Some(target)).is_none() {
                    return Ok(StepStatus::AlreadySatisfied);
                }
                crate::hid_stop_local(lab, target)
                    .map(|()| StepStatus::Ok)
                    .map_err(|e| e.to_string())
            }),
        ));
    }
    if t.netboot.is_some() {
        steps.push((
            "netboot_session",
            Box::new(move || {
                if !crate::state::is_netboot_running(target) {
                    return Ok(StepStatus::AlreadySatisfied);
                }
                crate::netboot::stop(target)
                    .map(|()| StepStatus::Ok)
                    .map_err(|e| e.to_string())
            }),
        ));
    }
    if let Some(nb) = &t.netboot {
        if let Some(iface) = nb.interface.as_deref() {
            let host_ip = nb.effective_host_ip().to_string();
            steps.push((
                "netif_mode_off",
                Box::new(move || {
                    crate::netif::mode_off(target, iface, &host_ip)
                        .map(|()| StepStatus::Ok)
                        .map_err(|e| e.to_string())
                }),
            ));
        }
    }
    run_steps(steps)
}

/// The token read from `--lease-file`, once. `Some(None)` means the flag was
/// given but the file held nothing.
static FILE_TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Read and delete the `--lease-file` a dispatching panio shipped, and keep
/// its trimmed content as the caller's token for the rest of this process
/// (it overrides, and is exported as, `PANIO_LEASE`). Call once, right after argument parsing: the
/// file is deleted on read so the token does not outlive the command. An
/// unreadable file is an error, never a silent run without a token.
pub fn take_lease_file(path: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the shipped lease token file '{path}'"))?;
    // Best effort: the dispatching side also removes it afterward.
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("warning: could not delete the lease token file '{path}': {e}");
    }
    let t = text.trim().to_string();
    if t.is_empty() {
        anyhow::bail!("the shipped lease token file '{path}' is empty");
    }
    // Single-threaded here (called from main before `run`): children, hooks
    // and helpers see the token as PANIO_LEASE too.
    unsafe { std::env::set_var("PANIO_LEASE", &t) };
    let _ = FILE_TOKEN.set(Some(t));
    Ok(())
}

/// The caller's lease token: the `--lease-file` content when one was given,
/// else `PANIO_LEASE` (trimmed; empty is none).
pub fn caller_token() -> Option<String> {
    if let Some(Some(t)) = FILE_TOKEN.get() {
        return Some(t.clone());
    }
    std::env::var("PANIO_LEASE")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The start of every message saying the caller's own lease ended.
const ENDED_PREFIX: &str = "your lease on '";
/// The start of the message for a token that is not the current holder's.
const NOT_HOLDER_PREFIX: &str = "PANIO_LEASE does not match the current holder";
/// The start of the warning for a token on a target nobody holds.
const NOT_LEASED_PREFIX: &str = "PANIO_LEASE is set but '";

/// True when a warning line (without its `warning:` label) is one of the
/// lease check's messages that the caller no longer holds the lease, as
/// opposed to an unrelated warning or a transport failure.
pub fn is_lease_loss_message(text: &str) -> bool {
    let t = text.trim();
    t.starts_with(ENDED_PREFIX)
        || t.starts_with(NOT_HOLDER_PREFIX)
        || t.starts_with(NOT_LEASED_PREFIX)
}

/// The text for a lease that has ended, for a caller still carrying its token.
fn ended_message(target: &str, at: u64, reason: &str) -> String {
    format!(
        "{ENDED_PREFIX}{target}' ended at {} ({reason}); run `panio lease acquire {target}` \
         to take it again, or unset PANIO_LEASE to run without a lease",
        clock(at)
    )
}

/// Why the caller's token stopped working, if the history knows.
pub(crate) fn lost_lease_message(
    lock: &StoreLock,
    token: Option<&str>,
    target: &str,
) -> Option<String> {
    let e = lock.history_for_token(token?)?;
    let reason = match (e.how.as_str(), &e.new_owner) {
        ("preempted", Some(o)) => format!("preempted by {o}"),
        ("preempted", None) => "ended by a forced release".to_string(),
        ("released", _) => "released".to_string(),
        ("expired", _) => "found expired".to_string(),
        _ => return None,
    };
    Some(ended_message(target, e.at, &reason))
}

/// The target has no lease now. Tell a caller whose token names one that
/// ended: refuse a mutating command, warn on a read. Any other token only
/// gets a warning, since a holder legitimately keeps its token while touching
/// a different, unleased target.
fn note_no_lease(
    lock: Option<&StoreLock>,
    access: Access,
    token: Option<&str>,
    target: &str,
    expired: Option<&Lease>,
) -> anyhow::Result<()> {
    let Some(t) = token else {
        return Ok(());
    };
    let msg = lock
        .and_then(|l| lost_lease_message(l, Some(t), target))
        .or_else(|| {
            expired.filter(|l| l.token == t).map(|l| {
                ended_message(
                    target,
                    l.expires_at.min(l.last_used_at.saturating_add(l.idle_secs)),
                    "found expired",
                )
            })
        });
    match msg {
        Some(m) if access == Access::Mutating => Err(crate::error::target_leased(target, m).into()),
        Some(m) => {
            eprintln!("warning: {m}");
            Ok(())
        }
        None => {
            eprintln!(
                "warning: {NOT_LEASED_PREFIX}{target}' is not leased; \
                 running without a lease"
            );
            Ok(())
        }
    }
}

/// The `target_cleanup_blocked` error for the blocking record `lease`.
pub fn blocked_error(target: &str, lease: &Lease) -> anyhow::Error {
    crate::error::cleanup_blocked(target, blocked_message(target, lease)).into()
}

/// The lease check for the running command against `target`, on the host
/// where it executes. Takes the store lock, decides, and releases the lock
/// before returning: it is never held while the command runs.
pub fn enforce(lab: &crate::model::Lab, target: &str, access: Access) -> anyhow::Result<()> {
    enforce_with(lab, target, access, true)
}

/// [`enforce`], with a choice about registering this process under the lease
/// when the holder runs a mutating command. A long-lived command that keeps the
/// lease alive itself (the console) passes `false`: a registered pid would be
/// terminated by the cleanup when the lease ends, with no reason printed.
pub fn enforce_with(
    lab: &crate::model::Lab,
    target: &str,
    access: Access,
    register: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;
    if access == Access::Unchecked {
        return Ok(());
    }
    let lease_required = lab.targets.get(target).is_some_and(|t| t.lease_required());
    let token = caller_token();
    if access == Access::Read && token.is_none() {
        return Ok(());
    }
    let store = Store::for_target(target);
    if !lease_required && !store.record_exists() {
        if token.is_none() {
            return Ok(());
        }
        if !store.history_exists() {
            return note_no_lease(None, access, token.as_deref(), target, None);
        }
    }
    let lock = store
        .lock()
        .with_context(|| format!("locking the lease store for '{target}'"))?;
    let rec = match lock.read() {
        Ok(r) => r,
        Err(ReadError::Corrupt(c)) if access == Access::Mutating => {
            return Err(crate::error::cleanup_blocked(target, corrupt_message(target, &c)).into());
        }
        Err(ReadError::Corrupt(c)) => {
            eprintln!("warning: {}", corrupt_message(target, &c));
            return Ok(());
        }
        Err(e @ ReadError::Io(_)) => {
            return Err(anyhow::anyhow!(e).context(format!("lease for '{target}'")));
        }
    };
    let now = now();
    let st = state(rec, now, proc_is_live);
    let refuse = |lock: &StoreLock, msg: String| {
        let msg = lost_lease_message(lock, token.as_deref(), target).unwrap_or(msg);
        anyhow::Error::from(crate::error::target_leased(target, msg))
    };
    match decide(&st, access, token.as_deref(), lease_required, target, now) {
        Verdict::Run => match &st {
            State::Unleased => note_no_lease(Some(&lock), access, token.as_deref(), target, None),
            State::Expired(l) => {
                note_no_lease(Some(&lock), access, token.as_deref(), target, Some(l))
            }
            State::Blocked(l) => {
                eprintln!("warning: {}", blocked_message(target, l));
                Ok(())
            }
            State::Held(l) => {
                // A read that carries a token which is not the holder's runs,
                // but never silently: the caller most likely lost its lease.
                let msg = lost_lease_message(&lock, token.as_deref(), target)
                    .unwrap_or_else(|| format!("{NOT_HOLDER_PREFIX} ({}) of '{target}'", l.owner));
                eprintln!("warning: {msg}");
                Ok(())
            }
        },
        Verdict::RunAndTouch => {
            let State::Held(mut l) = st else {
                return Ok(());
            };
            l.last_used_at = now;
            if access == Access::Mutating && register {
                let pid = std::process::id() as i32;
                if let Some(start_time) = platform::process_start_time(pid) {
                    l.procs.retain(|p| p.pid != pid);
                    l.procs.push(Proc { pid, start_time });
                }
            }
            lock.write(&l).context("recording lease activity")?;
            Ok(())
        }
        Verdict::Refuse(msg) => {
            let held = matches!(st, State::Held(_));
            match lost_lease_message(&lock, token.as_deref(), target) {
                Some(ended) if held => {
                    let m = format!("{ended}; meanwhile: {msg}");
                    Err(crate::error::target_leased(target, m).into())
                }
                _ => Err(refuse(&lock, msg)),
            }
        }
        Verdict::CleanupBlocked(msg) => Err(crate::error::cleanup_blocked(target, msg).into()),
        Verdict::CleanupThenRun(old) => {
            match crate::lease_cmd::retire(lab, target, &lock, &old, "expired", None)? {
                crate::lease_cmd::Retired::Clean => {
                    note_no_lease(Some(&lock), access, token.as_deref(), target, None)
                }
                crate::lease_cmd::Retired::Blocked(b) => Err(blocked_error(target, &b)),
            }
        }
        Verdict::CleanupThenRefuse(old, msg) => {
            match crate::lease_cmd::retire(lab, target, &lock, &old, "expired", None)? {
                crate::lease_cmd::Retired::Clean => Err(refuse(&lock, msg)),
                crate::lease_cmd::Retired::Blocked(b) => Err(blocked_error(target, &b)),
            }
        }
    }
}

/// The idle timeout of the live lease on `target` when the caller's token is
/// its holder's, else `None` (no token, no lease, someone else's). Read-only.
pub fn held_idle_secs(target: &str) -> Option<u64> {
    let token = caller_token()?;
    let store = Store::for_target(target);
    if !store.record_exists() {
        return None;
    }
    let lock = store.lock().ok()?;
    match state(lock.read().ok()?, now(), proc_is_live) {
        State::Held(l) if l.token == token => Some(l.idle_secs),
        _ => None,
    }
}

/// What one look at the lease showed a console that holds it.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// The caller still holds the lease.
    Holds,
    /// It does not: the reason, never including the token.
    Lost(String),
    /// The record could not be read (I/O error, corrupt): not proof of
    /// anything, so the caller retries.
    Unreadable(String),
}

/// Whether the caller's `token` still holds the lease in `store`. Read-only.
fn lost_reason(store: &Store, token: &str, target: &str, now: u64) -> Poll {
    let lock = match store.lock() {
        Ok(l) => l,
        Err(e) => return Poll::Unreadable(format!("cannot read the lease: {e}")),
    };
    let rec = match lock.read() {
        Ok(r) => r,
        Err(ReadError::Corrupt(c)) => {
            return Poll::Unreadable(format!("the lease record is corrupt: {}", c.0))
        }
        Err(e) => return Poll::Unreadable(format!("cannot read the lease: {e}")),
    };
    match state(rec, now, proc_is_live) {
        State::Held(l) if l.token == token => Poll::Holds,
        State::Held(l) => Poll::Lost(
            lost_lease_message(&lock, Some(token), target)
                .unwrap_or_else(|| format!("{NOT_HOLDER_PREFIX} ({}) of '{target}'", l.owner)),
        ),
        State::Expired(l) if l.token == token => Poll::Lost(format!(
            "{ENDED_PREFIX}{target}' expired at {}; the next lease check will stop \
             this target's daemons",
            clock(l.expires_at.min(l.last_used_at.saturating_add(l.idle_secs)))
        )),
        State::Blocked(l) => Poll::Lost(blocked_message(target, &l)),
        State::Expired(_) | State::Unleased => Poll::Lost(
            lost_lease_message(&lock, Some(token), target)
                .unwrap_or_else(|| format!("'{target}' is no longer leased to you")),
        ),
    }
}

/// One in-process poll for a local console: what the lease on `target` looks
/// like to the caller, and, while it holds it, a holder read (`enforce` with
/// [`Access::Read`], which touches `last_used_at`) when `touch` is set. Cheap
/// without `touch`: one brief lock and a file read.
pub fn keepalive_tick(lab: &crate::model::Lab, target: &str, touch: bool) -> Poll {
    let Some(token) = caller_token() else {
        return Poll::Holds;
    };
    let poll = lost_reason(&Store::for_target(target), &token, target, now());
    if poll == Poll::Holds && touch {
        if let Err(e) = enforce(lab, target, Access::Read) {
            return Poll::Unreadable(format!("{e:#}"));
        }
    }
    poll
}

/// Parse `90s`, `30m`, `2h`, or bare seconds. Zero and negative values are
/// errors.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (digits, unit) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(format!("invalid duration '{s}': use e.g. 90s, 30m, 2h")),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration '{s}': use e.g. 90s, 30m, 2h"))?;
    if n == 0 {
        return Err(format!("invalid duration '{s}': must be greater than zero"));
    }
    n.checked_mul(unit)
        .ok_or_else(|| format!("invalid duration '{s}': too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> Lease {
        Lease {
            owner: "alice@bench1".into(),
            token: "tok-a".into(),
            acquired_at: 1000,
            expires_at: 8200,
            duration_secs: 7200,
            idle_secs: 1800,
            last_used_at: 1000,
            procs: vec![],
            cleanup: None,
        }
    }

    fn proc1() -> Proc {
        Proc {
            pid: 42,
            start_time: 7,
        }
    }

    fn lab_from(text: &str) -> crate::model::Lab {
        toml::from_str(text).expect("lab parses")
    }

    #[cfg(unix)]
    fn lease_with(procs: Vec<Proc>) -> Lease {
        Lease { procs, ..lease() }
    }

    #[cfg(unix)]
    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep")
    }

    #[cfg(unix)]
    fn registered(child: &std::process::Child) -> Proc {
        let pid = child.id() as i32;
        Proc {
            pid,
            start_time: platform::process_start_time(pid).expect("start time"),
        }
    }

    fn write_held(store: &Store, token: &str, expires_at: u64, last_used_at: u64) {
        let l = Lease {
            token: token.into(),
            expires_at,
            last_used_at,
            ..lease()
        };
        store.lock().unwrap().write(&l).unwrap();
    }

    fn ended(store: &Store, token: &str, how: &str, new_owner: Option<&str>) {
        store
            .lock()
            .unwrap()
            .append_history(&HistoryEntry {
                at: 5000,
                previous_owner: "alice@bench1".into(),
                previous_token: token.into(),
                new_owner: new_owner.map(str::to_string),
                how: how.into(),
                ..HistoryEntry::default()
            })
            .unwrap();
    }

    #[test]
    fn lost_reason_is_none_for_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        write_held(&store, "tok-a", now() + 3600, now());
        assert_eq!(lost_reason(&store, "tok-a", "dut", now()), Poll::Holds);
    }

    #[test]
    fn lost_reason_names_how_the_lease_ended() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        // Preempted: a new holder, the old token in the history.
        ended(&store, "tok-a", "preempted", Some("bob@bench1"));
        write_held(&store, "tok-b", now() + 3600, now());
        let r = lost("tok-a", &store);
        assert!(r.contains("preempted by bob@bench1"), "{r}");
        assert!(!r.contains("tok-a"), "{r}");
        // A token that never named a lease here, while someone holds it.
        let r = lost("tok-x", &store);
        assert!(
            r.contains("does not match the current holder (alice@bench1)"),
            "{r}"
        );
        // Released: nobody holds it now.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = Store::at(dir2.path().to_path_buf());
        ended(&store2, "tok-a", "released", None);
        let r = lost("tok-a", &store2);
        assert!(r.contains("released"), "{r}");
        // Expired but not yet cleaned up.
        let dir3 = tempfile::tempdir().unwrap();
        let store3 = Store::at(dir3.path().to_path_buf());
        write_held(&store3, "tok-a", now() - 10, now() - 20);
        let r = lost("tok-a", &store3);
        assert!(r.contains("expired at"), "{r}");
        assert!(r.contains("next lease check will stop"), "{r}");
        assert!(!r.contains("tok-a"), "{r}");
    }

    fn lost(token: &str, store: &Store) -> String {
        match lost_reason(store, token, "dut", now()) {
            Poll::Lost(r) => r,
            other => panic!("expected a loss, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_record_is_not_a_loss() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        std::fs::write(dir.path().join("lease.json"), "{not json").unwrap();
        match lost_reason(&store, "tok-a", "dut", now()) {
            Poll::Unreadable(m) => assert!(m.contains("corrupt"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn loss_messages_are_recognized_and_others_are_not() {
        for m in [
            "your lease on 'pi5' ended at 10:30 local (released); run it",
            "PANIO_LEASE does not match the current holder (alice@bench1) of 'pi5'",
            "PANIO_LEASE is set but 'pi5' is not leased; running without a lease",
        ] {
            assert!(is_lease_loss_message(m), "{m}");
        }
        for m in ["cannot release: no token", "ssh: connect timed out", ""] {
            assert!(!is_lease_loss_message(m), "{m}");
        }
        // The producing functions use the same prefixes.
        assert!(is_lease_loss_message(&ended_message(
            "pi5", 1000, "released"
        )));
    }

    #[test]
    fn a_lost_lease_exits_23() {
        assert_eq!(crate::error::Kind::TargetLeased.exit_code(), 23);
    }

    #[test]
    fn cleanup_with_no_channels_is_noop() {
        let lab = lab_from("[targets.t6-empty]\n");
        let r = cleanup(&lab, "t6-empty", &[]);
        assert_eq!(
            r,
            vec![StepResult {
                step: "registered_processes".into(),
                status: StepStatus::AlreadySatisfied,
                message: None
            }]
        );
        assert!(cleanup(&lab, "not-in-the-lab", &[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_kills_registered_proc() {
        let lab = lab_from("[targets.t6-kill]\n");
        let mut child = sleeper();
        let l = lease_with(vec![registered(&child)]);
        let r = cleanup(&lab, "t6-kill", &l.procs);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].status, StepStatus::Ok, "{r:?}");
        let status = child.try_wait().expect("try_wait");
        assert!(status.is_some(), "child should have been terminated");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_skips_recycled_pid() {
        let lab = lab_from("[targets.t6-recycled]\n");
        let mut child = sleeper();
        let mut p = registered(&child);
        p.start_time += 1;
        let l = lease_with(vec![p]);
        let r = cleanup(&lab, "t6-recycled", &l.procs);
        assert_eq!(r[0].status, StepStatus::AlreadySatisfied, "{r:?}");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "must survive"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn cleanup_continues_after_failure() {
        use std::cell::RefCell;
        let ran = RefCell::new(Vec::new());
        let steps: Vec<Step<'_>> = vec![
            (
                "first",
                Box::new(|| {
                    ran.borrow_mut().push("first");
                    Err("boom".to_string())
                }),
            ),
            (
                "second",
                Box::new(|| {
                    ran.borrow_mut().push("second");
                    Ok(StepStatus::Ok)
                }),
            ),
        ];
        let errs = run_steps(steps);
        assert_eq!(failure_lines(&errs), vec!["first: boom".to_string()]);
        assert_eq!(errs[1].status, StepStatus::Ok);
        assert_eq!(*ran.borrow(), vec!["first", "second"]);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_runs_while_the_lease_lock_is_held() {
        let _env = crate::daemons::RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let base = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PANIO_RUNTIME_BASE");
        // Safe: serialized by RUNTIME_BASE_LOCK; restored below.
        unsafe { std::env::set_var("PANIO_RUNTIME_BASE", base.path()) };
        let dir = crate::daemons::ensure_runtime_dir("hid", Some("t6-locked")).unwrap();
        std::fs::write(
            dir.join("daemon.json"),
            format!(r#"{{"pid":{},"port":7}}"#, std::process::id()),
        )
        .unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at(store_dir.path().to_path_buf());
        let _guard = store.lock().expect("lock");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let lab = lab_from("[targets.t6-locked]\n[targets.t6-locked.hid]\ncmd = \"false\"\n");
            let _ = tx.send(cleanup(&lab, "t6-locked", &[]));
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(10));
        match prev {
            Some(p) => unsafe { std::env::set_var("PANIO_RUNTIME_BASE", p) },
            None => unsafe { std::env::remove_var("PANIO_RUNTIME_BASE") },
        }
        let out = out.expect("cleanup must not block on the held lock");
        let failed: Vec<_> = out.iter().filter(|r| r.failed()).collect();
        assert_eq!(
            failed.len(),
            1,
            "the hid step must have run and failed: {out:?}"
        );
        assert_eq!(failed[0].step, "hid_daemon", "{out:?}");
    }

    #[test]
    fn terminate_with_fails_when_the_process_survives_sigkill() {
        let signals = std::cell::RefCell::new(Vec::new());
        let r = terminate_with(
            4242,
            || true,
            |sig| {
                signals.borrow_mut().push(format!("{sig:?}"));
                Ok(())
            },
            std::time::Duration::from_millis(100),
        );
        let err = r.expect_err("a process that survives KILL is not terminated");
        assert!(err.contains("4242") && err.contains("survived"), "{err}");
        assert_eq!(*signals.borrow(), vec!["Term", "Kill"]);
    }

    #[test]
    fn terminate_with_succeeds_when_the_process_dies_after_kill() {
        let killed = std::cell::Cell::new(false);
        let r = terminate_with(
            4243,
            || !killed.get(),
            |sig| {
                if matches!(sig, platform::Signal::Kill) {
                    killed.set(true);
                }
                Ok(())
            },
            std::time::Duration::from_millis(100),
        );
        assert_eq!(r, Ok(()));
        assert!(killed.get(), "KILL must have been sent");
    }

    #[cfg(unix)]
    #[test]
    fn terminate_registered_kills_a_process_that_ignores_term() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap \"\" TERM; sleep 30"])
            .spawn()
            .expect("spawn sh");
        let p = registered(&child);
        let waiter = std::thread::spawn(move || child.wait());
        assert!(terminate_registered(&p).is_ok());
        assert!(!proc_is_live(&p), "process must be gone");
        let status = waiter.join().expect("waiter").expect("wait");
        assert!(!status.success(), "killed, not a clean exit: {status:?}");
    }

    #[cfg(unix)]
    #[test]
    fn terminate_registered_ignores_a_process_that_already_exited() {
        let mut child = sleeper();
        let p = registered(&child);
        let _ = child.kill();
        let _ = child.wait();
        assert!(terminate_registered(&p).is_ok());
    }

    #[test]
    fn parse_duration_accepts_units_and_bare_seconds() {
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert_eq!(parse_duration("45"), Ok(45));
    }

    #[test]
    fn parse_duration_rejects_bad_input() {
        for bad in [
            "",
            "5d",
            "-1m",
            "0",
            "0m",
            "m",
            "1.5h",
            "99999999999999999999h",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be an error");
        }
    }

    fn blocked_lease() -> Lease {
        let block = CleanupBlock {
            cleanup_id: "id1".into(),
            phase: CleanupPhase::Failed,
            how: "released".into(),
            ended_at: 5000,
            previous_owner: "alice@bench1".into(),
            previous_token: "tok-a".into(),
            new_owner: None,
            procs: vec![],
            scope: serde_json::Value::Null,
            last_attempt_at: Some(5001),
            results: vec![StepResult {
                step: "hid_daemon".into(),
                status: StepStatus::Failed,
                message: Some("boom".into()),
            }],
        };
        Lease::blocked(&lease(), block, 5000).unwrap()
    }

    #[test]
    fn a_block_wins_over_expiry_idle_and_token() {
        let b = blocked_lease();
        // Far past every deadline, no live process, the old token: still blocked.
        let st = state(Some(b.clone()), 10_000_000, |_| false);
        assert_eq!(st, State::Blocked(b.clone()));
        for access in [Access::Mutating] {
            match decide(&st, access, Some("tok-a"), false, "pi5", 10_000_000) {
                Verdict::CleanupBlocked(m) => {
                    assert!(m.contains("cleanup id id1"), "{m}");
                    assert!(m.contains("hid_daemon"), "{m}");
                    assert!(m.contains("panio lease cleanup pi5 --id id1"), "{m}");
                    assert!(m.contains("override-cleanup"), "{m}");
                    assert!(!m.contains("tok-a"), "{m}");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            decide(&st, Access::Read, None, true, "pi5", 1),
            Verdict::Run
        );
        let blk = block_for(&st, Some("tok-a"), "pi5", 1).expect("blocks everyone");
        assert!(blk.cleanup_blocked);
        assert_eq!(blk.owner, "alice@bench1");
    }

    #[test]
    fn the_outer_fields_of_a_blocked_record_look_held_to_an_older_cli() {
        let b = blocked_lease();
        assert!(b.owner.starts_with("cleanup-blocked"), "{}", b.owner);
        assert_ne!(b.token, "tok-a");
        assert!(b.expires_at > 5000 + 365 * 86_400);
        assert!(b.procs.is_empty());
        // An older CLI deserializes it (unknown field ignored) as a live
        // lease: not expired, not idle.
        #[derive(Deserialize)]
        struct Old {
            expires_at: u64,
            idle_secs: u64,
            last_used_at: u64,
        }
        let old: Old = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert!(old.expires_at > 6000);
        assert!(old.last_used_at.saturating_add(old.idle_secs) > 6000);
    }

    #[test]
    fn a_record_written_before_blocks_existed_still_parses() {
        let text = r#"{"owner":"a","token":"t","acquired_at":1,"expires_at":2,
            "duration_secs":1,"idle_secs":1,"last_used_at":1,"procs":[]}"#;
        let l: Lease = serde_json::from_str(text).unwrap();
        assert!(l.cleanup.is_none());
        assert!(!serde_json::to_string(&l).unwrap().contains("cleanup"));
        let h = r#"{"at":1,"previous_owner":"a","previous_token":"t","new_owner":null,
            "how":"expired","cleanup_errors":[]}"#;
        let e: HistoryEntry = serde_json::from_str(h).unwrap();
        assert_eq!(e.resolution, None);
    }

    #[test]
    fn state_unleased() {
        assert_eq!(state(None, 5000, |_| false), State::Unleased);
    }

    #[test]
    fn state_held() {
        assert_eq!(state(Some(lease()), 1500, |_| false), State::Held(lease()));
    }

    #[test]
    fn state_expired_by_time() {
        assert!(matches!(
            state(Some(lease()), 8201, |_| false),
            State::Expired(_)
        ));
        let mut busy = lease();
        busy.last_used_at = 8000;
        assert!(matches!(state(Some(busy), 8200, |_| false), State::Held(_)));
    }

    #[test]
    fn block_for_only_a_held_lease_of_someone_else() {
        let held = State::Held(lease());
        let other = block_for(&held, Some("not-it"), "pi5", 1500).expect("blocked");
        assert_eq!(other.owner, lease().owner);
        assert!(other.message.contains("lease status pi5"));
        assert!(block_for(&held, None, "pi5", 1500).is_some());
        assert!(block_for(&held, Some(&lease().token), "pi5", 1500).is_none());
        assert!(block_for(&State::Unleased, None, "pi5", 1500).is_none());
        assert!(block_for(&State::Expired(lease()), None, "pi5", 1500).is_none());
    }

    #[test]
    fn state_expired_by_idle_without_live_proc() {
        // last_used 1000 + idle 1800 = 2800
        assert!(matches!(
            state(Some(lease()), 2801, |_| false),
            State::Expired(_)
        ));
        assert!(matches!(
            state(Some(lease()), 2800, |_| false),
            State::Held(_)
        ));
    }

    #[test]
    fn state_not_idle_expired_while_a_proc_is_alive() {
        let mut l = lease();
        l.procs = vec![proc1()];
        match state(Some(l.clone()), 5000, |_| true) {
            State::Held(h) => assert_eq!(h.procs, vec![proc1()]),
            other => panic!("expected Held, got {other:?}"),
        }
        assert!(matches!(state(Some(l), 8201, |_| true), State::Expired(_)));
    }

    #[test]
    fn state_prunes_dead_procs() {
        let mut l = lease();
        let live = Proc {
            pid: 43,
            start_time: 8,
        };
        l.procs = vec![proc1(), live.clone()];
        match state(Some(l), 1500, |p| p.pid == 43) {
            State::Held(h) => assert_eq!(h.procs, vec![live]),
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn proc_is_live_tracks_this_process_and_rejects_wrong_start_time() {
        let pid = std::process::id() as i32;
        let start = platform::process_start_time(pid).expect("own start time");
        assert!(proc_is_live(&Proc {
            pid,
            start_time: start
        }));
        assert!(!proc_is_live(&Proc {
            pid,
            start_time: start + 1
        }));
        assert!(!proc_is_live(&Proc {
            pid: i32::MAX,
            start_time: 1
        }));
    }

    fn held() -> State {
        State::Held(lease())
    }

    #[test]
    fn decide_unleased_read_runs() {
        let v = decide(&State::Unleased, Access::Read, None, true, "pi5", 1500);
        assert_eq!(v, Verdict::Run);
    }

    #[test]
    fn decide_unleased_mutating_not_required_runs() {
        let v = decide(&State::Unleased, Access::Mutating, None, false, "pi5", 1500);
        assert_eq!(v, Verdict::Run);
    }

    #[test]
    fn decide_unleased_mutating_required_refuses_with_hint() {
        match decide(&State::Unleased, Access::Mutating, None, true, "pi5", 1500) {
            Verdict::Refuse(m) => {
                assert!(m.contains("lease_required"), "{m}");
                assert!(m.contains("panio lease acquire pi5"), "{m}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn decide_held_matching_token_touches() {
        for access in [Access::Read, Access::Mutating] {
            let v = decide(&held(), access, Some("tok-a"), false, "pi5", 1500);
            assert_eq!(v, Verdict::RunAndTouch, "{access:?}");
        }
    }

    #[test]
    fn decide_held_no_token_read_runs() {
        assert_eq!(
            decide(&held(), Access::Read, None, false, "pi5", 1500),
            Verdict::Run
        );
        assert_eq!(
            decide(&held(), Access::Read, Some("wrong"), false, "pi5", 1500),
            Verdict::Run
        );
    }

    #[test]
    fn decide_held_wrong_token_mutating_refuses_with_owner_and_time_left() {
        for token in [None, Some("wrong")] {
            match decide(&held(), Access::Mutating, token, false, "pi5", 1000) {
                Verdict::Refuse(m) => {
                    assert!(m.contains("'pi5'"), "{m}");
                    assert!(m.contains("alice@bench1"), "{m}");
                    assert!(m.contains("120 min left"), "{m}");
                    assert!(m.contains("panio lease status pi5"), "{m}");
                    assert!(m.contains("panio lease acquire pi5 --wait 30m"), "{m}");
                }
                other => panic!("expected Refuse, got {other:?}"),
            }
        }
    }

    #[test]
    fn decide_expired_mutating_cleans_up_then_runs() {
        let s = State::Expired(lease());
        assert_eq!(
            decide(&s, Access::Mutating, None, false, "pi5", 9000),
            Verdict::CleanupThenRun(lease())
        );
    }

    #[test]
    fn decide_expired_mutating_required_cleans_then_refuses() {
        let s = State::Expired(lease());
        match decide(&s, Access::Mutating, None, true, "pi5", 9000) {
            Verdict::CleanupThenRefuse(l, m) => {
                assert_eq!(l, lease());
                assert!(m.contains("lease_required"), "{m}");
            }
            other => panic!("expected CleanupThenRefuse, got {other:?}"),
        }
    }

    #[test]
    fn decide_expired_read_runs_without_cleanup() {
        let s = State::Expired(lease());
        assert_eq!(
            decide(&s, Access::Read, None, true, "pi5", 9000),
            Verdict::Run
        );
    }

    #[test]
    fn decide_unchecked_always_runs() {
        for s in [State::Unleased, held(), State::Expired(lease())] {
            let v = decide(&s, Access::Unchecked, None, true, "pi5", 1500);
            assert_eq!(v, Verdict::Run);
        }
    }

    #[test]
    fn concurrent_acquire_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let lock = Store::at(path).lock().unwrap();
                    if lock.read().unwrap().is_none() {
                        let mut l = lease();
                        l.owner = format!("thread-{i}");
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        lock.write(&l).unwrap();
                        true
                    } else {
                        false
                    }
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn unreadable_record_is_an_io_error_not_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("lease.json")).unwrap();
        let lock = Store::at(dir.path().to_path_buf()).lock().unwrap();
        assert!(matches!(lock.read(), Err(ReadError::Io(_))));
    }

    #[test]
    fn invalid_utf8_record_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lease.json"), [0xff, 0xfe]).unwrap();
        let lock = Store::at(dir.path().to_path_buf()).lock().unwrap();
        assert!(matches!(lock.read(), Err(ReadError::Corrupt(_))));
    }

    #[test]
    fn corrupt_record_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lease.json"), "{").unwrap();
        let lock = Store::at(dir.path().to_path_buf()).lock().unwrap();
        assert!(matches!(lock.read(), Err(ReadError::Corrupt(_))));
        lock.write(&lease()).unwrap();
        assert_eq!(lock.read().unwrap(), Some(lease()));
    }

    #[test]
    fn remove_deletes_the_record_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        let lock = Store::at(dir.path().to_path_buf()).lock().unwrap();
        lock.write(&lease()).unwrap();
        lock.remove().unwrap();
        assert_eq!(lock.read().unwrap(), None);
        lock.remove().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lease.json"), "{").unwrap();
        let lock = Store::at(dir.path().to_path_buf()).lock().unwrap();
        for _ in 0..2 {
            lock.write(&lease()).unwrap();
            let mode = |n: &str| {
                std::fs::metadata(dir.path().join(n))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("lease.json"), 0o600);
        }
        lock.append_history(&entry(1, "tok-a")).unwrap();
        let m = std::fs::metadata(dir.path().join("lease-history.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }

    fn entry(at: u64, token: &str) -> HistoryEntry {
        HistoryEntry {
            at,
            previous_owner: "alice@bench1".into(),
            previous_token: token.into(),
            new_owner: Some("bob@bench2".into()),
            how: "preempted".into(),
            ..HistoryEntry::default()
        }
    }

    #[test]
    fn history_keeps_last_50() {
        let dir = tempfile::tempdir().unwrap();
        let lock = Store::at(dir.path().to_path_buf()).lock().unwrap();
        for i in 0..60 {
            lock.append_history(&entry(i, &format!("tok-{i}"))).unwrap();
        }
        let text = std::fs::read_to_string(dir.path().join("lease-history.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 50);
        assert!(lock.history_for_token("tok-9").is_none());
        assert_eq!(lock.history_for_token("tok-10").unwrap().at, 10);
        assert_eq!(lock.history_for_token("tok-59").unwrap().at, 59);
        assert!(lock.history_for_token("nope").is_none());
    }
}
