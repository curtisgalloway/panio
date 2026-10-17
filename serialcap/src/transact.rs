// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! One request/response exchange on a console the daemon owns (`POST
//! /transact`): take the interface's transaction lock, record the receive
//! boundary, send the bytes, then acquire the reply with the same bounded wait
//! `serialcap wait` uses ([`crate::wait::run`]), so the result has the same
//! shape and the same explicit stop statuses.
//!
//! The order is the point. The boundary (capture epoch + last RX sequence) is
//! read *before* the write, so the result holds exactly the bytes received
//! after it and nothing older can satisfy `until`. The lock is held from before
//! the boundary to the end of the wait, so a second transaction cannot send in
//! between and read this one's reply; plain input is refused (409) meanwhile
//! rather than queued (see [`crate::serial_io::SerialHandle::try_input`]).
//!
//! Like `wait`, a match proves only that the bytes arrived, not that the target
//! executed anything. No regex, no prompt detection, no retries.

use std::time::Duration;

use bytes::Bytes;
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::serial_io::SerialHandle;
use crate::wait;

/// Default deadline for the whole call, matching `serial wait`.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Default raw reply cap, matching `serial wait`.
pub const DEFAULT_MAX_BYTES: u64 = 65_536;

/// One validated transaction.
pub struct Params {
    /// Literal UTF-8 string that ends the wait when it appears in the reply.
    pub until: String,
    /// Deadline for the whole call: queueing for the lock, the write, and the
    /// wait for the reply.
    pub timeout_ms: u64,
    /// Raw reply byte cap.
    pub max_bytes: u64,
    /// Per-byte pacing for the write.
    pub pace: Duration,
}

/// Why a transaction produced no result.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// The parameters are out of range. Nothing was sent.
    BadRequest(String),
    /// Another transaction held the interface for the whole deadline. Nothing
    /// was sent.
    Busy,
    /// The interface is disconnected or the driver refused the write. The
    /// bytes may have been partly sent.
    Unavailable(String),
}

impl Params {
    /// Check the bounds `wait` enforces, plus a nonzero deadline: a
    /// transaction holds a lock, so it must always end.
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.until.is_empty() {
            return Err(Refusal::BadRequest("until must be nonempty".into()));
        }
        if self.timeout_ms == 0 || self.timeout_ms > wait::MAX_TIMEOUT_MS {
            return Err(Refusal::BadRequest(format!(
                "timeout_ms must be 1..={}",
                wait::MAX_TIMEOUT_MS
            )));
        }
        if self.max_bytes == 0 || self.max_bytes > wait::MAX_BYTES {
            return Err(Refusal::BadRequest(format!(
                "max_bytes must be 1..={}",
                wait::MAX_BYTES
            )));
        }
        Ok(())
    }
}

/// Run one transaction to completion. Spawned as its own task, so a client
/// that hangs up mid-call cannot release the lock early: the reply it would
/// have read still belongs to this transaction, not to the next one.
pub async fn run(handle: SerialHandle, body: Bytes, params: Params) -> Result<Value, Refusal> {
    params.validate()?;
    tokio::spawn(transact(handle, body, params))
        .await
        .map_err(|e| Refusal::Unavailable(format!("transaction task failed: {e}")))?
}

async fn transact(handle: SerialHandle, body: Bytes, params: Params) -> Result<Value, Refusal> {
    let deadline = Instant::now() + Duration::from_millis(params.timeout_ms);
    let guard = tokio::time::timeout_at(deadline, handle.transaction())
        .await
        .map_err(|_| Refusal::Busy)?;

    // The receive boundary, before the write. A chunk the supervisor reads
    // from here on gets a higher sequence number.
    let status = handle.status();
    if !status.connected {
        return Err(Refusal::Unavailable("serial interface disconnected".into()));
    }
    let epoch = status.health.epoch.clone();
    let since = status.health.last_rx_seq;

    let sent = body.len();
    handle
        .write_paced(body, params.pace)
        .await
        .map_err(|e| Refusal::Unavailable(format!("{e:#}")))?;

    // Whatever the queue and write left of the deadline. The shared wait
    // includes journal catch-up. At least 1 ms:
    // 0 means "one snapshot" to `wait::run`, and an expired deadline must still
    // end the call (as `timeout`).
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .max(1) as u64;
    let request = wait::Request {
        epoch,
        since,
        timeout_ms: remaining,
        max_bytes: params.max_bytes,
        max_output_bytes: wait::MAX_OUTPUT_BYTES,
        follow: false,
        pattern: Some(params.until),
        include_pending: true,
    };
    let dir = handle.capture_dir().to_path_buf();
    let probe = handle.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Hold the lock until the reply is acquired, even past a disconnect.
        let _guard = guard;
        wait::run(
            &request,
            |since, max_bytes| {
                Ok(serde_json::to_value(crate::evidence::read(
                    &dir,
                    Some(&request.epoch),
                    Some(since),
                    max_bytes,
                )?)?)
            },
            // In-process health: the same fields `GET /status` serves.
            |_| {
                let st = probe.status();
                Ok(json!({"connected": st.connected, "health": st.health}))
            },
            |_| Ok(()),
        )
    })
    .await
    .map_err(|e| Refusal::Unavailable(format!("transaction wait failed: {e}")))?;
    let mut result = result.map_err(|e| Refusal::Unavailable(format!("{e:#}")))?;
    result["sent_bytes"] = sent.into();
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::serial_io::tests::{recv_until, spawn_on_pty};
    use std::io::{Read, Write};

    fn params(until: &str, timeout_ms: u64, max_bytes: u64) -> Params {
        Params {
            until: until.into(),
            timeout_ms,
            max_bytes,
            pace: Duration::ZERO,
        }
    }

    fn reply_bytes(result: &Value) -> Vec<u8> {
        result["records"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["bytes"].as_array().unwrap().clone())
            .map(|b| b.as_u64().unwrap() as u8)
            .collect()
    }

    /// Read exactly `n` bytes from the pty's far end on a thread (it blocks).
    fn read_far(far: &mut std::fs::File, n: usize) -> Vec<u8> {
        let mut buf = vec![0; n];
        far.read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn parameters_are_bounded() {
        assert!(params("OK", 1, 1).validate().is_ok());
        for bad in [
            params("", 100, 64),
            params("OK", 0, 64),
            params("OK", wait::MAX_TIMEOUT_MS + 1, 64),
            params("OK", 100, 0),
            params("OK", 100, wait::MAX_BYTES + 1),
        ] {
            assert!(matches!(bad.validate(), Err(Refusal::BadRequest(_))));
        }
    }

    /// Output that arrived before the send — including a stale copy of the
    /// terminator — is outside the boundary; the reply is exactly what came
    /// after, and the match is on that alone.
    #[tokio::test]
    async fn boundary_is_taken_before_the_write() {
        let (handle, mut far, dir) = spawn_on_pty("transact-boundary");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;
        far.write_all(b"stale OK\r\n").unwrap();
        recv_until(&mut rx, b"stale OK\r\n").await;

        let mut target = far.try_clone().unwrap();
        let responder = std::thread::spawn(move || {
            let got = read_far(&mut target, 8);
            target.write_all(b"version 1.2\r\nOK\r\n").unwrap();
            got
        });
        let result = run(
            handle.clone(),
            Bytes::from_static(b"version\r"),
            params("OK", 5_000, 4096),
        )
        .await
        .unwrap();
        assert_eq!(responder.join().unwrap(), b"version\r");
        assert_eq!(result["status"], "matched", "{result}");
        assert_eq!(result["sent_bytes"], 8);
        let reply = reply_bytes(&result);
        // The wait matches as soon as the literal arrives, so the reply ends
        // at the chunk holding `OK`; the trailing CR LF may arrive in a later
        // read and fall outside it.
        assert!(reply.starts_with(b"version 1.2"), "{reply:?}");
        assert!(reply.windows(2).any(|w| w == b"OK"), "{reply:?}");
        assert!(
            !reply.windows(5).any(|w| w == b"stale"),
            "pre-send output leaked into the reply: {reply:?}"
        );
        assert!(
            result["since"].as_u64().unwrap() >= 1,
            "the stale chunk is inside the boundary"
        );
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }

    /// The boundary comes from the live receive cursor, which the supervisor
    /// advances before the capture thread persists the chunk. With the
    /// journal held one or more records behind that boundary, the transaction
    /// must wait for it to catch up and then acquire the reply, not end at
    /// once as a `gap` (issue #301, the CI flake in
    /// `boundary_is_taken_before_the_write`).
    #[tokio::test]
    async fn boundary_ahead_of_the_journal_waits_for_it_to_catch_up() {
        let (handle, mut far, dir) = spawn_on_pty("transact-lag");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;

        let held = handle.hold_journal();
        far.write_all(b"stale OK\r\n").unwrap();
        recv_until(&mut rx, b"stale OK\r\n").await;
        let lagging = handle.status().health;
        assert!(
            lagging.last_rx_seq > lagging.evidence_through_seq,
            "the journal must be behind the live cursor: {}",
            json!(lagging)
        );

        let mut target = far.try_clone().unwrap();
        let responder = std::thread::spawn(move || {
            let got = read_far(&mut target, 8);
            target.write_all(b"version 1.2\r\nOK\r\n").unwrap();
            got
        });
        let mut call = tokio::spawn(run(
            handle.clone(),
            Bytes::from_static(b"version\r"),
            params("OK", 5_000, 4096),
        ));
        recv_until(&mut rx, b"OK\r\n").await;
        // Keep the journal stalled a while longer, unless the call has
        // already ended (which is the bug).
        let early = tokio::time::timeout(Duration::from_millis(300), &mut call).await;
        drop(held);
        let result = match early {
            Ok(done) => done.unwrap().unwrap(),
            Err(_) => call.await.unwrap().unwrap(),
        };
        assert_eq!(responder.join().unwrap(), b"version\r");
        assert_eq!(result["status"], "matched", "{result}");
        let reply = reply_bytes(&result);
        assert!(reply.starts_with(b"version 1.2"), "{reply:?}");
        assert!(
            !reply.windows(5).any(|w| w == b"stale"),
            "pre-send output leaked into the reply: {reply:?}"
        );
        assert_eq!(result["since"], lagging.last_rx_seq);
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn journal_still_behind_at_deadline_reports_timeout() {
        let (handle, mut far, dir) = spawn_on_pty("transact-lag-timeout");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;
        let held = handle.hold_journal();
        far.write_all(b"old output\r\n").unwrap();
        recv_until(&mut rx, b"old output\r\n").await;
        let since = handle.status().health.last_rx_seq;
        let result = run(
            handle.clone(),
            Bytes::from_static(b"version\r"),
            params("OK", 100, 4096),
        )
        .await
        .unwrap();
        drop(held);
        assert_eq!(result["status"], "timeout", "{result}");
        assert_eq!(result["since"], since);
        assert_eq!(result["sent_bytes"], 8);
        assert!(reply_bytes(&result).is_empty());
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }

    /// A target that never answers ends the call at the deadline with whatever
    /// arrived; a chatty one ends it at the byte cap.
    #[tokio::test]
    async fn timeout_and_cap_are_explicit_outcomes() {
        let (handle, mut far, dir) = spawn_on_pty("transact-outcomes");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;
        // Establish a receive boundary so this tests reply timeout/cap, not
        // the independent race with capture-thread startup.
        far.write_all(b"ready\r\n").unwrap();
        recv_until(&mut rx, b"ready\r\n").await;

        let mut target = far.try_clone().unwrap();
        let responder = std::thread::spawn(move || {
            read_far(&mut target, 2);
            target.write_all(b"partial").unwrap();
        });
        let result = run(
            handle.clone(),
            Bytes::from_static(b"a\r"),
            params("OK", 300, 4096),
        )
        .await
        .unwrap();
        responder.join().unwrap();
        assert_eq!(result["status"], "timeout", "{result}");
        assert_eq!(result["pending"], json!(b"partial".to_vec()));

        let mut target = far.try_clone().unwrap();
        let responder = std::thread::spawn(move || {
            read_far(&mut target, 2);
            target.write_all(b"0123456789").unwrap();
        });
        let result = run(
            handle.clone(),
            Bytes::from_static(b"b\r"),
            params("OK", 5_000, 4),
        )
        .await
        .unwrap();
        responder.join().unwrap();
        assert_eq!(result["status"], "cap", "{result}");
        assert!(reply_bytes(&result).len() <= 4);
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }

    /// A second transaction queues behind the first and runs once it ends;
    /// one that cannot get the lock within its own deadline is refused as
    /// busy without sending anything.
    #[tokio::test]
    async fn a_second_transaction_queues_or_is_refused_as_busy() {
        let (handle, far, dir) = spawn_on_pty("transact-queue");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;

        let held = handle.transaction().await;
        assert_eq!(
            run(
                handle.clone(),
                Bytes::from_static(b"x\r"),
                params("OK", 100, 64)
            )
            .await
            .unwrap_err(),
            Refusal::Busy
        );

        let queued = tokio::spawn(run(
            handle.clone(),
            Bytes::from_static(b"y\r"),
            params("OK", 5_000, 64),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!queued.is_finished(), "must wait for the lock");
        let mut target = far.try_clone().unwrap();
        let responder = std::thread::spawn(move || {
            let got = read_far(&mut target, 2);
            target.write_all(b"OK\r\n").unwrap();
            got
        });
        drop(held);
        let result = queued.await.unwrap().unwrap();
        // Only the queued transaction's bytes reached the port: the refused
        // one sent nothing.
        assert_eq!(responder.join().unwrap(), b"y\r");
        assert_eq!(result["status"], "matched", "{result}");
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Plain input is refused, not queued, while a transaction holds the
    /// interface — and also while one is queued for it.
    #[tokio::test]
    async fn plain_input_is_refused_during_and_ahead_of_a_transaction() {
        let (handle, far, dir) = spawn_on_pty("transact-input");
        let free = handle.try_input();
        assert!(free.is_some());
        assert!(handle.try_input().is_some(), "plain inputs coexist");

        let waiting = tokio::spawn({
            let handle = handle.clone();
            async move {
                let _g = handle.transaction().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            handle.try_input().is_none(),
            "a queued transaction already refuses new input"
        );
        drop(free);
        waiting.await.unwrap();
        assert!(handle.try_input().is_some());

        let held = handle.transaction().await;
        assert!(handle.try_input().is_none());
        drop(held);
        assert!(handle.try_input().is_some());
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }
}
