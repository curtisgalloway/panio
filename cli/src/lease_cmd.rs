// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! The `panio lease acquire|renew|release|status` commands. Design:
//! `notes/target-leases.md`.
//!
//! Each command validates its arguments on the dev machine, then dispatches to
//! the target's default host (where the lease lives) like any runtime
//! command; the bodies below run on that host, against its lease store.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::error::{self, Kind, PanioError};
use crate::lease::{
    self, CleanupBlock, CleanupPhase, HistoryEntry, Lease, ReadError, State, StepResult, Store,
    StoreLock,
};
use crate::model::Lab;

/// How often `acquire --wait` looks again.
const POLL: Duration = Duration::from_secs(2);

/// `lease acquire`'s options, as typed.
#[derive(Default)]
pub struct AcquireOpts {
    pub owner: Option<String>,
    pub duration: Option<String>,
    pub idle: Option<String>,
    pub wait: Option<String>,
    pub preempt: bool,
    pub json: bool,
}

/// One target's lease, as `lease status` reports it. Never carries the token.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StatusRow {
    pub target: String,
    /// `"held"`, `"unleased"`, `"blocked"` (the last lease's cleanup failed or
    /// the record is unreadable), or `"unknown"` when the host could not be
    /// asked.
    #[serde(default)]
    pub state: String,
    /// `None` when the target's host could not be asked.
    pub held: Option<bool>,
    pub owner: Option<String>,
    pub expires_in_secs: Option<u64>,
    /// `None` while a live registered process keeps the lease from idling out.
    pub idle_left_secs: Option<u64>,
    pub procs: u64,
    /// The lease's idle timeout in seconds (a keepalive paces itself by it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u64>,
    /// Epoch seconds of the last change: the acquisition of a held lease, else
    /// the newest history line.
    pub last_change: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The cleanup block, when `state` is `"blocked"` because of one. Never
    /// the ended holder's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupInfo>,
}

/// What `lease status` shows of a [`CleanupBlock`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CleanupInfo {
    pub cleanup_id: String,
    pub phase: String,
    pub how: String,
    pub ended_at: u64,
    pub previous_owner: String,
    pub results: Vec<StepResult>,
}

impl CleanupInfo {
    fn of(b: &CleanupBlock) -> CleanupInfo {
        CleanupInfo {
            cleanup_id: b.cleanup_id.clone(),
            phase: b.phase.as_str().to_string(),
            how: b.how.clone(),
            ended_at: b.ended_at,
            previous_owner: b.previous_owner.clone(),
            results: b.results.clone(),
        }
    }
}

/// Parse a duration flag and enforce the 24 h cap. A bad value is a usage
/// error, raised on the dev machine before anything is dispatched.
fn parse_capped(flag: &str, text: &str) -> Result<u64> {
    let secs = lease::parse_duration(text)
        .map_err(|e| PanioError::new(Kind::Usage, format!("{flag}: {e}")))?;
    if secs > lease::MAX_SECS {
        return Err(PanioError::new(
            Kind::Usage,
            format!("{flag}: '{text}' is more than the 24 h maximum"),
        )
        .into());
    }
    Ok(secs)
}

fn parse_opt(flag: &str, text: &Option<String>) -> Result<Option<u64>> {
    text.as_deref().map(|t| parse_capped(flag, t)).transpose()
}

/// `$USER@<short hostname>`, read on the machine the user typed the command
/// on.
fn default_owner() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let host = crate::model::local_fqdn()
        .map(|f| f.split('.').next().unwrap_or(f).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{user}@{host}")
}

/// The caller's token, if `PANIO_LEASE` is set and non-empty.
fn env_token() -> Option<String> {
    lease::caller_token()
}

/// v1 leases cover targets on one host only.
fn refuse_cross_host(lab: &Lab, target: &str) -> Result<()> {
    let rt = lab
        .resolved_target(target)
        .ok_or_else(|| error::target_not_found(target))?;
    let hosts = crate::model::target_hosts(&rt);
    if hosts.len() > 1 {
        let list: Vec<String> = hosts.into_iter().collect();
        return Err(PanioError::not_configured(format!(
            "target '{target}' has channels on more than one host ({}); \
             leases cover targets whose channels share one host",
            list.join(" and ")
        ))
        .target(target)
        .into());
    }
    Ok(())
}

/// Dispatch to the default host; exits with the remote's status when it did.
fn dispatch_or_local(lab: &Lab, target: &str, extra: &[String]) -> Result<()> {
    if let Some(code) = crate::dispatch::maybe_dispatch_default_host(
        lab,
        target,
        crate::dispatch::Mode::Reexec,
        extra,
    )? {
        std::process::exit(code);
    }
    Ok(())
}

/// Read the record and classify it. An unreadable record is a
/// `target_cleanup_blocked` error (`override-cleanup --corrupt` moves it
/// aside), and one that cannot be read at all is an error too, so neither is
/// ever overwritten.
fn read_state(lock: &StoreLock, target: &str) -> Result<State> {
    let rec = match lock.read() {
        Ok(r) => r,
        Err(ReadError::Corrupt(c)) => {
            return Err(error::cleanup_blocked(target, lease::corrupt_message(target, &c)).into());
        }
        Err(e @ ReadError::Io(_)) => {
            return Err(anyhow!(e).context(format!("lease for '{target}'")));
        }
    };
    Ok(lease::state(rec, lease::now(), lease::proc_is_live))
}

fn open(target: &str) -> Result<StoreLock> {
    Store::for_target(target)
        .lock()
        .with_context(|| format!("locking the lease store for '{target}'"))
}

/// How [`retire`] ended.
#[derive(Debug)]
pub(crate) enum Retired {
    /// Cleanup verified, history written, record removed.
    Clean,
    /// Cleanup failed or could not be recorded as done: the target stays
    /// blocked, and this is the blocking record.
    Blocked(Box<Lease>),
}

/// End `old`: block the target, clean up after the holder, and either clear
/// the block (every step ok or already satisfied) or leave it. Owns the whole
/// transition, so callers never remove the record themselves.
///
/// Order, under the held lock: (1) persist a `pending` block before touching
/// anything, so a crash mid-cleanup still leaves the target blocked; (2) run
/// every cleanup step; (3) on success append the history, then remove the
/// record; on failure record the results and keep the block. An `Err` means
/// persistence failed: the record is never removed on that path, and no new
/// token is minted.
pub(crate) fn retire(
    lab: &Lab,
    target: &str,
    lock: &StoreLock,
    old: &Lease,
    how: &str,
    new_owner: Option<&str>,
) -> Result<Retired> {
    let started = lease::now();
    let cleanup_id = crate::platform::random_token().context("minting a cleanup id")?;
    let mut block = CleanupBlock {
        cleanup_id: cleanup_id.clone(),
        phase: CleanupPhase::Pending,
        how: how.to_string(),
        ended_at: started,
        previous_owner: old.owner.clone(),
        previous_token: old.token.clone(),
        new_owner: new_owner.map(str::to_string),
        procs: old.procs.clone(),
        scope: lease::scope_snapshot(lab, target),
        last_attempt_at: None,
        results: Vec::new(),
    };
    let pending = Lease::blocked(old, block.clone(), started).context("minting a lease token")?;
    lock.write(&pending)
        .context("recording the pending cleanup")?;

    let results = lease::cleanup(lab, target, &old.procs);
    block.last_attempt_at = Some(lease::now());
    block.results = results.clone();
    let errors = lease::failure_lines(&results);
    for e in &errors {
        eprintln!("warning: {target}: cleanup: {e}");
    }
    let entry = HistoryEntry {
        at: lease::now(),
        previous_owner: old.owner.clone(),
        previous_token: old.token.clone(),
        new_owner: new_owner.map(str::to_string),
        how: how.to_string(),
        cleanup_errors: errors.clone(),
        cleanup_id: Some(cleanup_id),
        ..HistoryEntry::default()
    };
    if errors.is_empty() {
        lock.append_history(&entry)
            .context("recording the lease history (the target stays blocked; run `panio lease cleanup` to finish)")?;
        lock.remove().context("removing the lease record")?;
        return Ok(Retired::Clean);
    }
    block.phase = CleanupPhase::Failed;
    let failed = Lease::blocked(old, block, lease::now()).context("minting a lease token")?;
    lock.write(&failed)
        .context("recording the failed cleanup")?;
    if let Err(e) = lock.append_history(&entry) {
        eprintln!("warning: {target}: could not record the lease history: {e}");
    }
    Ok(Retired::Blocked(Box::new(failed)))
}

fn mins(secs: u64) -> u64 {
    secs.div_ceil(60)
}

fn held_error(target: &str, l: &Lease) -> anyhow::Error {
    let left = l.expires_at.saturating_sub(lease::now());
    error::target_leased(
        target,
        format!(
            "target '{target}' is leased to {} ({} min left); wait for it, \
             use `panio lease acquire {target} --wait 30m`, or take it with --preempt",
            l.owner,
            mins(left)
        ),
    )
    .into()
}

/// Why `token` is not the holder, with the history's explanation when the
/// token belonged to a displaced holder.
fn not_holder_error(
    lock: &StoreLock,
    target: &str,
    token: Option<&str>,
    what: &str,
) -> anyhow::Error {
    let Some(token) = token else {
        return error::target_leased(
            target,
            format!("{what}: no lease token; set PANIO_LEASE to the token `panio lease acquire` printed"),
        )
        .into();
    };
    let msg = match lock.history_for_token(token) {
        Some(h) if h.how == "preempted" => match &h.new_owner {
            Some(n) => format!("{what}: your lease on '{target}' was preempted by {n}"),
            None => format!("{what}: your lease on '{target}' was released by someone else"),
        },
        Some(h) => format!("{what}: your lease on '{target}' ended ({})", h.how),
        None => format!("{what}: PANIO_LEASE does not match a lease on '{target}'"),
    };
    error::target_leased(target, msg).into()
}

enum Outcome {
    Got(Lease),
    Held(Lease),
}

struct AcquireArgs {
    owner: String,
    duration: Option<u64>,
    idle: Option<u64>,
    preempt: bool,
}

fn try_acquire(lab: &Lab, target: &str, a: &AcquireArgs) -> Result<Outcome> {
    let lock = open(target)?;
    let now = lease::now();
    let fresh = |lock: &StoreLock| -> Result<Outcome> {
        let duration = a.duration.unwrap_or(lease::DEFAULT_DURATION);
        let l = Lease {
            owner: a.owner.clone(),
            token: crate::platform::random_token().context("minting a lease token")?,
            acquired_at: now,
            expires_at: now + duration,
            duration_secs: duration,
            idle_secs: a.idle.unwrap_or(lease::DEFAULT_IDLE),
            last_used_at: now,
            procs: Vec::new(),
            cleanup: None,
        };
        lock.write(&l).context("writing the lease record")?;
        Ok(Outcome::Got(l))
    };
    let after = |r: Retired| match r {
        Retired::Clean => fresh(&lock),
        Retired::Blocked(b) => Err(lease::blocked_error(target, &b)),
    };
    match read_state(&lock, target)? {
        State::Unleased => fresh(&lock),
        State::Blocked(b) => Err(lease::blocked_error(target, &b)),
        State::Expired(old) => after(retire(lab, target, &lock, &old, "expired", Some(&a.owner))?),
        State::Held(mut l) => {
            if env_token().as_deref() == Some(l.token.as_str()) {
                let duration = a.duration.unwrap_or(l.duration_secs);
                l.duration_secs = duration;
                l.expires_at = now + duration;
                if let Some(i) = a.idle {
                    l.idle_secs = i;
                }
                l.last_used_at = now;
                lock.write(&l).context("writing the lease record")?;
                Ok(Outcome::Got(l))
            } else if a.preempt {
                after(retire(lab, target, &lock, &l, "preempted", Some(&a.owner))?)
            } else {
                Ok(Outcome::Held(l))
            }
        }
    }
}

/// `panio lease acquire`.
pub fn acquire(lab: &Lab, target: &str, o: &AcquireOpts) -> Result<()> {
    let duration = parse_opt("--duration", &o.duration)?;
    let idle = parse_opt("--idle", &o.idle)?;
    let wait = parse_opt("--wait", &o.wait)?;
    refuse_cross_host(lab, target)?;
    // Name the owner here so a remote default host does not substitute its
    // own identity.
    let owner = o.owner.clone().unwrap_or_else(default_owner);
    let extra: Vec<String> = if o.owner.is_none() {
        vec!["--owner".to_string(), owner.clone()]
    } else {
        Vec::new()
    };
    dispatch_or_local(lab, target, &extra)?;

    let args = AcquireArgs {
        owner,
        duration,
        idle,
        preempt: o.preempt,
    };
    let deadline = wait.map(|w| Instant::now() + Duration::from_secs(w));
    let l = loop {
        match try_acquire(lab, target, &args)? {
            Outcome::Got(l) => break l,
            Outcome::Held(l) => {
                let remaining = deadline
                    .map(|d| d.saturating_duration_since(Instant::now()))
                    .unwrap_or_default();
                if remaining.is_zero() {
                    return Err(held_error(target, &l));
                }
                std::thread::sleep(remaining.min(POLL));
            }
        }
    };
    let left = l.expires_at.saturating_sub(lease::now());
    if o.json {
        let v = serde_json::json!({
            "token": l.token,
            "owner": l.owner,
            "expires_at": l.expires_at,
            "expires_in_secs": left,
        });
        println!("{v}");
    } else {
        println!("{}", l.token);
    }
    Ok(())
}

/// `panio lease renew`.
pub fn renew(lab: &Lab, target: &str, duration: &Option<String>) -> Result<()> {
    let duration = parse_opt("--duration", duration)?;
    dispatch_or_local(lab, target, &[])?;
    let lock = open(target)?;
    let token = env_token();
    let what = "cannot renew";
    match read_state(&lock, target)? {
        State::Held(mut l) if token.as_deref() == Some(l.token.as_str()) => {
            let now = lease::now();
            let d = duration.unwrap_or(l.duration_secs);
            l.duration_secs = d;
            l.expires_at = now + d;
            l.last_used_at = now;
            lock.write(&l).context("writing the lease record")?;
            println!("Lease on '{target}' renewed; {} min left", mins(d));
            Ok(())
        }
        State::Held(_) => Err(not_holder_error(&lock, target, token.as_deref(), what)),
        State::Blocked(b) => Err(lease::blocked_error(target, &b)),
        State::Expired(l) => {
            // Retire it now so the history can say what happened to this token.
            if let Retired::Blocked(b) = retire(lab, target, &lock, &l, "expired", None)? {
                return Err(lease::blocked_error(target, &b));
            }
            let msg = lease::lost_lease_message(&lock, token.as_deref(), target)
                .unwrap_or_else(|| format!("the lease on '{target}' has expired"));
            Err(error::target_leased(target, format!("{what}: {msg}")).into())
        }
        State::Unleased => Err(not_holder_error(&lock, target, token.as_deref(), what)),
    }
}

/// `panio lease release`.
pub fn release(lab: &Lab, target: &str, preempt: bool) -> Result<()> {
    dispatch_or_local(lab, target, &[])?;
    let lock = open(target)?;
    let token = env_token();
    let what = "cannot release";
    match read_state(&lock, target)? {
        State::Blocked(b) => Err(lease::blocked_error(target, &b)),
        State::Held(l) => {
            let retired = if token.as_deref() == Some(l.token.as_str()) {
                retire(lab, target, &lock, &l, "released", None)?
            } else if preempt {
                retire(lab, target, &lock, &l, "preempted", None)?
            } else if let Some(msg) = lease::lost_lease_message(&lock, token.as_deref(), target) {
                // The caller's own lease already ended and someone else holds
                // the target now: nothing of theirs is left to release, and
                // the new holder's lease is not theirs to end.
                eprintln!("note: {msg}");
                return Ok(());
            } else {
                return Err(not_holder_error(&lock, target, token.as_deref(), what));
            };
            if let Retired::Blocked(b) = retired {
                return Err(lease::blocked_error(target, &b));
            }
            println!("Lease on '{target}' released");
            Ok(())
        }
        State::Expired(l) => {
            if let Retired::Blocked(b) = retire(lab, target, &lock, &l, "expired", None)? {
                return Err(lease::blocked_error(target, &b));
            }
            println!("The lease on '{target}' had expired; cleaned up");
            Ok(())
        }
        State::Unleased if preempt => {
            lock.remove().context("removing the lease record")?;
            println!("'{target}' is not leased");
            Ok(())
        }
        State::Unleased => match token.as_deref() {
            None => {
                println!("'{target}' is not leased");
                Ok(())
            }
            Some(t) => match lease::lost_lease_message(&lock, Some(t), target) {
                Some(msg) => {
                    eprintln!("note: {msg}");
                    Ok(())
                }
                None => Err(not_holder_error(&lock, target, token.as_deref(), what)),
            },
        },
    }
}

/// Fill `row` from a live lease. While a registered process is alive the lease
/// cannot idle out, so there is no idle countdown to report.
fn fill_held(row: &mut StatusRow, l: Lease, now: u64) {
    row.state = "held".to_string();
    row.held = Some(true);
    row.expires_in_secs = Some(l.expires_at.saturating_sub(now));
    row.idle_left_secs = if l.procs.is_empty() {
        Some(
            l.last_used_at
                .saturating_add(l.idle_secs)
                .saturating_sub(now),
        )
    } else {
        None
    };
    row.procs = l.procs.len() as u64;
    row.idle_secs = Some(l.idle_secs);
    row.last_change = Some(l.acquired_at);
    row.owner = Some(l.owner);
}

/// A status row with nothing filled in.
fn blank_row(target: &str) -> StatusRow {
    StatusRow {
        target: target.to_string(),
        state: "unleased".to_string(),
        held: Some(false),
        owner: None,
        expires_in_secs: None,
        idle_left_secs: None,
        procs: 0,
        idle_secs: None,
        last_change: None,
        error: None,
        cleanup: None,
    }
}

/// Fill `row` from a cleanup-blocked record. Nobody holds the target.
fn fill_blocked(row: &mut StatusRow, l: &Lease) {
    row.state = "blocked".to_string();
    row.held = Some(false);
    row.cleanup = l.cleanup_block().map(CleanupInfo::of);
    row.last_change = l.cleanup_block().map(|b| b.ended_at);
}

/// This host's view of one target, cleaning up an expired lease first (once:
/// a blocked target is only reported afterward).
fn local_row(lab: &Lab, target: &str) -> Result<StatusRow> {
    let lock = open(target)?;
    let mut row = blank_row(target);
    let now = lease::now();
    let rec = match lock.read() {
        Ok(r) => r,
        Err(ReadError::Corrupt(c)) => {
            row.state = "blocked".to_string();
            row.held = None;
            row.error = Some(format!(
                "lease record unreadable ({}); `panio lease override-cleanup {target} \
                 --corrupt --reason TEXT` moves it aside",
                c.0
            ));
            return Ok(row);
        }
        Err(e @ ReadError::Io(_)) => {
            return Err(anyhow!(e).context(format!("lease for '{target}'")));
        }
    };
    match lease::state(rec, now, lease::proc_is_live) {
        State::Held(l) => fill_held(&mut row, l, now),
        State::Blocked(l) => fill_blocked(&mut row, &l),
        State::Expired(l) => match retire(lab, target, &lock, &l, "expired", None)? {
            Retired::Clean => row.last_change = lock.last_history().map(|h| h.at),
            Retired::Blocked(b) => fill_blocked(&mut row, &b),
        },
        State::Unleased => row.last_change = lock.last_history().map(|h| h.at),
    }
    Ok(row)
}

fn row_text(r: &StatusRow) -> String {
    if r.state == "blocked" {
        if let Some(c) = &r.cleanup {
            let failed: Vec<&str> = c
                .results
                .iter()
                .filter(|x| x.failed())
                .map(|x| x.step.as_str())
                .collect();
            let what = if c.phase == "pending" {
                "cleanup did not finish".to_string()
            } else {
                format!("cleanup failed ({})", failed.join(", "))
            };
            return format!(
                "{t}: blocked: {what}; cleanup id {id}; run panio lease cleanup {t} --id {id}",
                t = r.target,
                id = c.cleanup_id,
            );
        }
    }
    match (&r.error, r.held) {
        (Some(e), _) if r.state == "blocked" => format!("{}: blocked: {e}", r.target),
        (Some(e), _) => format!("{}: unknown ({e})", r.target),
        (None, Some(true)) => {
            let idle = match r.idle_left_secs {
                Some(left) => format!("idle timeout in {} min", mins(left)),
                None => format!("kept alive by {} live process(es)", r.procs),
            };
            format!(
                "{}: leased to {}, expires in {} min, {idle}",
                r.target,
                r.owner.as_deref().unwrap_or("?"),
                mins(r.expires_in_secs.unwrap_or(0)),
            )
        }
        _ => format!("{}: unleased", r.target),
    }
}

/// `panio lease status TARGET`.
pub fn status(lab: &Lab, target: &str, json: bool) -> Result<()> {
    dispatch_or_local(lab, target, &[])?;
    let row = local_row(lab, target)?;
    if json {
        println!("{}", serde_json::to_string(&row)?);
    } else {
        println!("{}", row_text(&row));
    }
    Ok(())
}

/// One target's row from wherever its default host is. A host that cannot be
/// reached becomes an `error` row instead of failing the rest.
fn row_for(lab: &Lab, target: &str) -> StatusRow {
    let failed = |msg: String| StatusRow {
        state: "unknown".to_string(),
        held: None,
        error: Some(msg),
        ..blank_row(target)
    };
    let Some(rt) = lab.resolved_target(target) else {
        return failed("not in the lab".to_string());
    };
    let host_name = rt.default_host;
    if lab.host(&host_name).is_local(&host_name) {
        return local_row(lab, target).unwrap_or_else(|e| failed(format!("{e:#}")));
    }
    let args = ["lease", "status", "--json", "-t", target];
    match crate::dispatch::run_subcommand(lab, target, &host_name, &args) {
        Ok(out) if out.status == 0 => serde_json::from_str::<StatusRow>(out.stdout.trim())
            .map(|mut row| {
                // A control host older than the cleanup block sends no state.
                if row.state.is_empty() {
                    row.state = match row.held {
                        Some(true) => "held",
                        Some(false) => "unleased",
                        None => "unknown",
                    }
                    .to_string();
                }
                row
            })
            .unwrap_or_else(|e| failed(format!("unreadable reply from '{host_name}': {e}"))),
        Ok(out) => failed(format!(
            "'{host_name}' exited {}: {}",
            out.status,
            out.stderr.trim()
        )),
        Err(e) => failed(format!("{e:#}")),
    }
}

/// `panio lease status` with no target in a lab that has several: one row per
/// target.
pub fn status_all(lab: &Lab, json: bool) -> Result<()> {
    let rows: Vec<StatusRow> = lab
        .target_names()
        .into_iter()
        .map(|t| row_for(lab, t))
        .collect();
    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        for r in &rows {
            println!("{}", row_text(r));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Proc;

    fn lease(procs: Vec<Proc>) -> Lease {
        Lease {
            owner: "alice@bench1".into(),
            token: "tok".into(),
            acquired_at: 100,
            expires_at: 7300,
            duration_secs: 7200,
            idle_secs: 1800,
            last_used_at: 100,
            procs,
            cleanup: None,
        }
    }

    fn row(l: Lease, now: u64) -> StatusRow {
        let mut r = blank_row("dut");
        fill_held(&mut r, l, now);
        r
    }

    #[test]
    fn idle_countdown_shown_without_live_processes() {
        let r = row(lease(vec![]), 400);
        assert_eq!(r.idle_left_secs, Some(1500));
        assert_eq!(r.idle_secs, Some(1800));
        assert!(
            row_text(&r).contains("idle timeout in 25 min"),
            "{}",
            row_text(&r)
        );
    }

    #[test]
    fn live_processes_replace_the_idle_countdown() {
        let p = Proc {
            pid: 42,
            start_time: 7,
        };
        let r = row(lease(vec![p]), 400);
        assert_eq!(r.idle_left_secs, None);
        assert_eq!(r.procs, 1);
        let text = row_text(&r);
        assert!(text.contains("kept alive by 1 live process(es)"), "{text}");
        assert!(!text.contains("idle timeout"), "{text}");
        let json = serde_json::to_value(&r).unwrap();
        assert!(json["idle_left_secs"].is_null(), "{json}");
    }
}
