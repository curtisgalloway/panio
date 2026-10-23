// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! Whole-target leases: the on-disk record, a locked store, and the pure
//! decision logic. Design: `notes/target-leases.md`.
//!
//! Everything here is local to the host that runs it. The record lives at
//! `<state dir>/<target>/lease.json`; every read-modify-write goes through
//! [`StoreLock`], an exclusive file lock on `lease.lock` beside it, so two
//! panio processes can never both decide a target is free.

// Callers arrive in later tasks (acquire/release in 5, the check in 6-7).
// Task 7 removes this allow.
#![allow(dead_code)]

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
}

/// One line of `lease-history.jsonl`: why a token stopped working.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub at: u64,
    pub previous_owner: String,
    pub previous_token: String,
    pub new_owner: Option<String>,
    /// `"preempted"`, `"released"` or `"expired"`.
    pub how: String,
    pub cleanup_errors: Vec<String>,
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

/// `lease.json` exists but cannot be used. The caller treats it as no lease
/// and replaces it on the next write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptLease(pub String);

impl std::fmt::Display for CorruptLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unreadable lease record: {}", self.0)
    }
}

impl std::error::Error for CorruptLease {}

/// Why [`StoreLock::read`] failed. Only `Corrupt` means "treat as no lease and
/// replace it"; `Io` (permissions, a directory in the way, a failing disk)
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
/// exists *and* carries the recorded start time. A Linux zombie keeps a start
/// time, so the pid probe alone is not enough, and a recycled pid fails the
/// start-time comparison.
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
    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
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
}

/// Classify a record at `now`. Registered processes for which `alive` is
/// false are dropped from the returned lease. A lease is expired past
/// `expires_at`, or when idle (`last_used_at + idle_secs` has passed) with no
/// live registered process.
pub fn state(l: Option<Lease>, now: u64, alive: impl Fn(&Proc) -> bool) -> State {
    let Some(mut lease) = l else {
        return State::Unleased;
    };
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
        }
    }

    fn proc1() -> Proc {
        Proc {
            pid: 42,
            start_time: 7,
        }
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
            cleanup_errors: vec![],
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
