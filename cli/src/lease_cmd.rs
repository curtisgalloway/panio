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
use crate::lease::{self, HistoryEntry, Lease, ReadError, State, Store, StoreLock};
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

/// Read the record and classify it. A corrupt record is reported and treated
/// as no lease; a record that cannot be read at all is an error, so it is
/// never overwritten.
fn read_state(lock: &StoreLock, target: &str) -> Result<State> {
    let rec = match lock.read() {
        Ok(r) => r,
        Err(ReadError::Corrupt(c)) => {
            eprintln!("warning: {target}: {c}; treating it as no lease and replacing it");
            None
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

/// Clean up after `old` and record why its token stopped working.
pub(crate) fn retire(
    lab: &Lab,
    target: &str,
    lock: &StoreLock,
    old: &Lease,
    how: &str,
    new_owner: Option<&str>,
) -> Result<()> {
    let errs = lease::cleanup(lab, target, old);
    for e in &errs {
        eprintln!("warning: {target}: cleanup: {e}");
    }
    lock.append_history(&HistoryEntry {
        at: lease::now(),
        previous_owner: old.owner.clone(),
        previous_token: old.token.clone(),
        new_owner: new_owner.map(str::to_string),
        how: how.to_string(),
        cleanup_errors: errs,
    })
    .context("recording the lease history")
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
        };
        lock.write(&l).context("writing the lease record")?;
        Ok(Outcome::Got(l))
    };
    match read_state(&lock, target)? {
        State::Unleased => fresh(&lock),
        State::Expired(old) => {
            retire(lab, target, &lock, &old, "expired", Some(&a.owner))?;
            fresh(&lock)
        }
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
                retire(lab, target, &lock, &l, "preempted", Some(&a.owner))?;
                fresh(&lock)
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
        State::Expired(l) => {
            // Retire it now so the history can say what happened to this token.
            retire(lab, target, &lock, &l, "expired", None)?;
            lock.remove().context("removing the expired lease")?;
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
        State::Held(l) => {
            if token.as_deref() == Some(l.token.as_str()) {
                retire(lab, target, &lock, &l, "released", None)?;
            } else if preempt {
                retire(lab, target, &lock, &l, "preempted", None)?;
            } else if let Some(msg) = lease::lost_lease_message(&lock, token.as_deref(), target) {
                // The caller's own lease already ended and someone else holds
                // the target now: nothing of theirs is left to release, and
                // the new holder's lease is not theirs to end.
                eprintln!("note: {msg}");
                return Ok(());
            } else {
                return Err(not_holder_error(&lock, target, token.as_deref(), what));
            }
            lock.remove().context("removing the lease record")?;
            println!("Lease on '{target}' released");
            Ok(())
        }
        State::Expired(l) => {
            retire(lab, target, &lock, &l, "expired", None)?;
            lock.remove().context("removing the lease record")?;
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

/// This host's view of one target, cleaning up an expired lease first.
fn local_row(lab: &Lab, target: &str) -> Result<StatusRow> {
    let lock = open(target)?;
    let mut row = StatusRow {
        target: target.to_string(),
        held: Some(false),
        owner: None,
        expires_in_secs: None,
        idle_left_secs: None,
        procs: 0,
        idle_secs: None,
        last_change: None,
        error: None,
    };
    let now = lease::now();
    match read_state(&lock, target)? {
        State::Held(l) => fill_held(&mut row, l, now),
        State::Expired(l) => {
            retire(lab, target, &lock, &l, "expired", None)?;
            lock.remove().context("removing the lease record")?;
            row.last_change = lock.last_history().map(|h| h.at);
        }
        State::Unleased => row.last_change = lock.last_history().map(|h| h.at),
    }
    Ok(row)
}

fn row_text(r: &StatusRow) -> String {
    match (&r.error, r.held) {
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
        target: target.to_string(),
        held: None,
        owner: None,
        expires_in_secs: None,
        idle_left_secs: None,
        procs: 0,
        idle_secs: None,
        last_change: None,
        error: Some(msg),
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
        Ok(out) if out.status == 0 => serde_json::from_str(out.stdout.trim())
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
        }
    }

    fn row(l: Lease, now: u64) -> StatusRow {
        let mut r = StatusRow {
            target: "dut".into(),
            held: Some(false),
            owner: None,
            expires_in_secs: None,
            idle_left_secs: None,
            procs: 0,
            idle_secs: None,
            last_change: None,
            error: None,
        };
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
