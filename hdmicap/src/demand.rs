// Copyright 2026 The Panio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! What the daemon's clients currently want from the capture loop (#221).
//!
//! The capture loop used to run at `TARGET_FRAME_INTERVAL` whenever anything
//! held a receiver, so a daemon nobody was looking at cost as much CPU as one
//! streaming to a browser. The server now records demand here and the Linux
//! capture loop picks its interval from it:
//!
//! - a **stream** — an open `/preview`, or a `/snapshot` waiting on
//!   `changed_since`/`wait=stable` — wants every frame for as long as it lasts;
//! - a **pull** — `/snapshot`, `/ocr` — wants one fresh frame now, and may
//!   come back for more in a moment.
//!
//! A pull that finds the warm frame older than [`FRESH_ENOUGH`] wakes the
//! capture thread and waits briefly for the next frame, so dropping the idle
//! rate never hands an agent a picture of the screen from before its last
//! action.
//!
//! Only the Linux loop sleeps between frames; on macOS and Windows the backend
//! blocks on the device's own cadence, and this record goes unread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use crate::frame::TARGET_FRAME_INTERVAL;

/// The longest the capture loop may sleep between frames, whatever
/// [`capture_interval`] says. The stall watchdog in `capture_thread` flags a
/// stall after 4 s without a frame, and `STALE_AFTER` reports a frame older
/// than 3 s as `Stale`, so an idle interval must stay well under both or an
/// unwatched daemon would be mistaken for a broken one.
pub const MAX_IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// A pull accepts the warm frame as-is when it is at most this old; anything
/// older makes the pull wake the capture thread and wait for a new one.
pub const FRESH_ENOUGH: Duration = Duration::from_millis(66);

/// How long a pull waits for that new frame before serving what it has. One
/// device frame plus a decode is ~25 ms; this is slack for a busy host, not
/// an expected wait.
pub const FRESH_WAIT: Duration = Duration::from_millis(250);

/// The capture interval when nothing is streaming and no pull is recent:
/// 5 fps. Keeps `/status` and the dashboard signal within 0.2 s, and lets an
/// unwatched daemon settle to `Stable` after a mode switch in
/// `STABLE_FRAMES` x 200 ms = 1.6 s.
pub const IDLE_INTERVAL: Duration = Duration::from_millis(200);

/// How long full rate lasts after a pull. Long enough that a burst of
/// `shot`/`read` calls in one agent step never waits for a fresh frame;
/// short enough that a grid polling `/snapshot` at 1 Hz spends most of each
/// second idle.
pub const PULL_LINGER: Duration = Duration::from_millis(300);

/// How long the capture loop should wait between frames, given what clients
/// want right now.
///
/// - `streams`: open `/preview` connections plus `/snapshot` requests blocked
///   waiting for a change or for a stable signal. Each wants every frame.
/// - `since_pull`: time since the last `/snapshot` or `/ocr` request, or
///   `None` if there has never been one.
///
/// Contract the rest of the daemon relies on:
/// - with `streams > 0` the answer is `TARGET_FRAME_INTERVAL`, or a stream
///   is served stale frames;
/// - right after a pull (`since_pull` near zero) the answer is
///   `TARGET_FRAME_INTERVAL`, or the pull's wake-up does not produce the
///   fresh frame it is waiting for;
/// - the caller clamps the result to [`MAX_IDLE_INTERVAL`].
pub fn capture_interval(streams: usize, since_pull: Option<Duration>) -> Duration {
    let recent_pull = since_pull.is_some_and(|t| t < PULL_LINGER);
    if streams > 0 || recent_pull {
        TARGET_FRAME_INTERVAL
    } else {
        IDLE_INTERVAL
    }
}

/// Shared between the server (which records demand) and the capture thread
/// (which reads it and is woken by it).
#[derive(Default)]
pub struct Demand {
    streams: AtomicUsize,
    last_pull: Mutex<Option<Instant>>,
    capture: OnceLock<Thread>,
}

impl Demand {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register the capture thread so demand can cut its sleep short. Called
    /// once, from the thread itself.
    pub fn set_capture_thread(&self, t: Thread) {
        let _ = self.capture.set(t);
    }

    /// A stream that wants every frame until the guard drops.
    pub fn stream(self: &Arc<Self>) -> StreamGuard {
        self.streams.fetch_add(1, Ordering::SeqCst);
        self.wake();
        StreamGuard(self.clone())
    }

    /// A one-off request for the current frame.
    pub fn pulled(&self) {
        if let Ok(mut t) = self.last_pull.lock() {
            *t = Some(Instant::now());
        }
        self.wake();
    }

    /// The interval the capture loop should use right now.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn interval(&self) -> Duration {
        let since_pull = self
            .last_pull
            .lock()
            .ok()
            .and_then(|t| t.map(|t| t.elapsed()));
        capture_interval(self.streams.load(Ordering::SeqCst), since_pull).min(MAX_IDLE_INTERVAL)
    }

    /// Open streams right now; for tests that watch a request hold one.
    #[cfg(test)]
    pub fn stream_count(&self) -> usize {
        self.streams.load(Ordering::SeqCst)
    }

    fn wake(&self) {
        if let Some(t) = self.capture.get() {
            t.unpark();
        }
    }
}

/// Keeps the capture loop at full rate while held; see [`Demand::stream`].
pub struct StreamGuard(Arc<Demand>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.streams.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_guard_counts_while_held() {
        let d = Demand::new();
        assert_eq!(d.streams.load(Ordering::SeqCst), 0);
        let g1 = d.stream();
        let g2 = d.stream();
        assert_eq!(d.streams.load(Ordering::SeqCst), 2);
        drop(g1);
        assert_eq!(d.streams.load(Ordering::SeqCst), 1);
        drop(g2);
        assert_eq!(d.streams.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_pull_wakes_a_parked_capture_thread() {
        let d = Demand::new();
        let d2 = d.clone();
        let h = std::thread::spawn(move || {
            d2.set_capture_thread(std::thread::current());
            let start = Instant::now();
            std::thread::park_timeout(Duration::from_secs(10));
            start.elapsed()
        });
        // Wait for the thread to register before waking it.
        while d.capture.get().is_none() {
            std::thread::yield_now();
        }
        d.pulled();
        assert!(h.join().unwrap() < Duration::from_secs(5));
    }

    /// The contract in `capture_interval`'s doc: a stream always gets every
    /// frame, and a pull that just happened gets its fresh frame promptly.
    #[test]
    fn capture_interval_honors_the_contract() {
        for since in [None, Some(Duration::ZERO), Some(Duration::from_secs(60))] {
            assert_eq!(capture_interval(1, since), TARGET_FRAME_INTERVAL);
            assert_eq!(capture_interval(3, since), TARGET_FRAME_INTERVAL);
        }
        assert_eq!(
            capture_interval(0, Some(Duration::ZERO)),
            TARGET_FRAME_INTERVAL
        );
    }

    /// Full rate for `PULL_LINGER` after a pull, then a step to idle (#221).
    #[test]
    fn a_pull_holds_full_rate_for_the_linger_then_steps_to_idle() {
        let just_before = PULL_LINGER - Duration::from_millis(1);
        assert_eq!(
            capture_interval(0, Some(just_before)),
            TARGET_FRAME_INTERVAL
        );
        assert_eq!(capture_interval(0, Some(PULL_LINGER)), IDLE_INTERVAL);
        assert_eq!(
            capture_interval(0, Some(Duration::from_secs(60))),
            IDLE_INTERVAL
        );
    }

    #[test]
    fn a_daemon_nobody_has_asked_anything_runs_idle() {
        assert_eq!(capture_interval(0, None), IDLE_INTERVAL);
    }

    /// A 1 Hz grid poll must not keep a daemon at full rate: the linger has to
    /// end well inside the gap between polls.
    #[test]
    fn a_one_hertz_poll_leaves_most_of_each_second_idle() {
        assert!(PULL_LINGER * 2 < Duration::from_secs(1));
    }

    #[test]
    fn the_interval_is_clamped_below_the_stall_watchdog() {
        let d = Demand::new();
        assert!(d.interval() <= MAX_IDLE_INTERVAL);
        assert!(IDLE_INTERVAL <= MAX_IDLE_INTERVAL);
        assert!(MAX_IDLE_INTERVAL < crate::frame::STALE_AFTER);
    }
}
