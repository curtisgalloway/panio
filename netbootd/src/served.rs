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

//! What the file servers (TFTP + HTTP) share: traversal-safe path resolution
//! and a sanitizer for the peer-supplied names they log.
//!
//! Both servers expose a single rooted directory and must never let a request
//! escape it. [`resolve`] joins the requested name under `root`, canonicalizes
//! it, and confirms the result is still inside the canonicalized root —
//! rejecting `..` traversal, symlink escapes (a symlink *inside* the root that
//! points outside it canonicalizes to the outside path and is refused), and
//! (because `canonicalize` fails on a missing path) probes for files that do
//! not exist.
//!
//! Also shared: [`warn_rate_limited`], so DHCP's second-client gate and
//! TFTP's peer/transfer-slot gates all cap a sustained flood to one log line
//! per interval rather than one per packet.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest peer-supplied string [`loggable`] lets into the log.
const MAX_LOG_CHARS: usize = 128;

/// Resolve a requested filename inside `root`, rejecting traversal outside it.
///
/// A leading `/` is treated as relative to `root`, never as a host filesystem
/// path. Returns `None` for anything that canonicalizes outside `root` or does
/// not exist.
pub fn resolve(root: &Path, filename: &str) -> Option<PathBuf> {
    let rel = filename.trim_start_matches('/');
    let candidate = root.join(rel);
    let canon = candidate.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    canon.starts_with(&root_canon).then_some(canon)
}

/// A peer-supplied string (an HTTP request-target, a TFTP filename) made safe
/// to write into `netboot.log`: anything but printable ASCII becomes `?`, so a
/// terminal escape or a stray newline cannot forge log lines, and it is cut at
/// [`MAX_LOG_CHARS`] (with a trailing `…`) so a 16 KB request head cannot
/// land in the log whole.
pub fn loggable(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_LOG_CHARS + 4));
    for (i, c) in s.chars().enumerate() {
        if i == MAX_LOG_CHARS {
            out.push('…');
            break;
        }
        out.push(if c.is_ascii_graphic() || c == ' ' {
            c
        } else {
            '?'
        });
    }
    out
}

/// Log `msg` via `warn!`, but only if at least `interval` has passed since
/// the last warning tracked in `last` (updated in place). A sustained
/// flood — a second DHCP client hammering DISCOVER, or a burst of TFTP RRQs
/// from an unleased peer or past the transfer-slot bound — would otherwise
/// turn one ongoing condition into an unbounded number of log lines.
pub fn warn_rate_limited(last: &mut Option<Instant>, interval: Duration, msg: &str) {
    let now = Instant::now();
    let should_warn = match *last {
        Some(t) => now.duration_since(t) >= interval,
        None => true,
    };
    if should_warn {
        tracing::warn!("{msg}");
        *last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique, freshly-created temp dir (no `tempfile` dependency, matching
    /// the sibling modules' pattern).
    fn tmp() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "netbootd-served-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolve_accepts_file_inside_root() {
        let root = tmp();
        fs::write(root.join("kernel.img"), b"x").unwrap();
        let got = resolve(&root, "kernel.img").expect("file in root resolves");
        assert!(got.ends_with("kernel.img"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_strips_leading_slash() {
        let root = tmp();
        fs::write(root.join("boot.img"), b"x").unwrap();
        // An absolute-looking request is treated as relative to root, never as
        // a host filesystem path.
        assert!(resolve(&root, "/boot.img").is_some());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_accepts_file_in_subdir() {
        let root = tmp();
        let sub = root.join("grub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("grub.cfg"), b"set timeout=0").unwrap();
        // Follow-on fetches (e.g. GRUB reading grub.cfg) live in subdirectories.
        assert!(resolve(&root, "grub/grub.cfg").is_some());
        assert!(resolve(&root, "/grub/grub.cfg").is_some());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_rejects_traversal_outside_root() {
        // Lay out  base/secret  and serve from  base/served . A "../secret"
        // request must be rejected even though the target genuinely exists.
        let base = tmp();
        let served = base.join("served");
        fs::create_dir_all(&served).unwrap();
        fs::write(base.join("secret"), b"top secret").unwrap();

        assert!(
            resolve(&served, "../secret").is_none(),
            "must not escape root"
        );
        assert!(resolve(&served, "../../etc/passwd").is_none());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_rejects_missing_file() {
        let root = tmp();
        // canonicalize() fails for a nonexistent path -> None (no info leak about
        // whether a sibling outside root exists).
        assert!(resolve(&root, "nope.img").is_none());
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_pointing_outside_root() {
        // base/secret exists; base/served/link -> ../secret. The link lives
        // inside the root but resolves outside it, so it must be refused.
        let base = tmp();
        let served = base.join("served");
        fs::create_dir_all(&served).unwrap();
        fs::write(base.join("secret"), b"top secret").unwrap();
        std::os::unix::fs::symlink(base.join("secret"), served.join("link")).unwrap();

        assert!(resolve(&served, "link").is_none(), "symlink escape");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn loggable_passes_ordinary_names_through() {
        assert_eq!(loggable("kernel_2712.img"), "kernel_2712.img");
        assert_eq!(loggable("/grub/grub.cfg?v=1"), "/grub/grub.cfg?v=1");
        assert_eq!(loggable("with space"), "with space");
    }

    #[test]
    fn loggable_neutralises_control_bytes_and_non_ascii() {
        // A terminal escape and a CR/LF (a forged log line) come out as `?`.
        assert_eq!(loggable("a\x1b[31mb"), "a?[31mb");
        assert_eq!(loggable("x\r\nINFO forged"), "x??INFO forged");
        assert_eq!(loggable("\u{7f}\u{85}é"), "???");
    }

    #[test]
    fn loggable_caps_the_length() {
        let long = "a".repeat(10_000);
        let shown = loggable(&long);
        assert_eq!(shown.chars().count(), MAX_LOG_CHARS + 1);
        assert!(shown.ends_with('…'));
        // Exactly at the cap: nothing is cut and no ellipsis is added.
        let exact = "b".repeat(MAX_LOG_CHARS);
        assert_eq!(loggable(&exact), exact);
    }

    #[test]
    fn warn_rate_limited_first_call_always_warns_and_records() {
        let mut last = None;
        warn_rate_limited(&mut last, Duration::from_secs(60), "first");
        assert!(last.is_some(), "the first call must record a timestamp");
    }

    #[test]
    fn warn_rate_limited_suppresses_within_the_interval() {
        let mut last = None;
        warn_rate_limited(&mut last, Duration::from_secs(60), "first");
        let recorded = last;
        warn_rate_limited(&mut last, Duration::from_secs(60), "second, too soon");
        assert_eq!(
            last, recorded,
            "a call inside the interval must not move the timestamp"
        );
    }

    #[test]
    fn warn_rate_limited_warns_again_after_the_interval() {
        let mut last = None;
        warn_rate_limited(&mut last, Duration::from_millis(10), "first");
        std::thread::sleep(Duration::from_millis(30));
        let before = last;
        warn_rate_limited(
            &mut last,
            Duration::from_millis(10),
            "second, interval elapsed",
        );
        assert_ne!(
            last, before,
            "a call past the interval must move the timestamp forward"
        );
    }
}
