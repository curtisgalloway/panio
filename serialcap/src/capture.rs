// Copyright 2026 Curtis Galloway
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

//! Timestamped, line-oriented capture of serial output.
//!
//! The daemon owns the port and is the only process that sees every byte, so it
//! assembles incoming bytes into lines, stamps each with a UTC timestamp and a
//! monotonic sequence number, and appends them to a rolling on-disk log under the
//! runtime dir. History survives daemon restarts (the sequence counter resumes
//! from the last line on disk) and grows well past the live-view window; old
//! lines age out by segment rotation.
//!
//! The `serialcap log` client reads these files directly — no daemon round-trip,
//! and it still works after the daemon has stopped. The current unterminated line
//! (e.g. a `login:` prompt that hasn't emitted a newline yet) lives only in the
//! daemon's memory, so it is mirrored to a small sidecar file the reader folds in
//! as the most recent (partial) line.
//!
//! Lines are stored *raw* (ANSI escapes and control bytes preserved); the reader
//! cleans them for display unless `--raw` is given, so no information is lost.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::error;

/// Default number of lines retained across all rotated segments.
pub const DEFAULT_BUFFER_LINES: u64 = 50_000;
/// Rotated segments kept (active + this many `.N` files).
const MAX_SEGMENTS: usize = 5;
/// Hard cap on a single unterminated line; bytes past this are force-flushed so a
/// newline-less stream can't grow the pending buffer without bound.
const MAX_PENDING: usize = 64 * 1024;
/// Ceiling on how often the pending-line sidecar is rewritten (~10/s). It is a
/// UI nicety for a reader that wants to see an in-flight line, not the record
/// of truth (that's the JSONL segments), so a busy console doing a write+rename
/// on every ingested chunk was needless disk churn. See `LineLog::sidecar_due`.
const SIDECAR_DEBOUNCE: Duration = Duration::from_millis(100);

const ACTIVE: &str = "serial.jsonl";
const PENDING: &str = "pending.json";

/// One line of captured serial output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Line {
    pub seq: u64,
    /// Wall-clock capture time, milliseconds since the Unix epoch (UTC).
    pub ts_ms: u64,
    /// Raw line text: ANSI escapes / control bytes preserved, trailing CR removed.
    pub text: String,
    /// True only for the in-flight line that has not seen its newline yet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Base directory holding per-interface capture sub-directories. Shares the
/// daemon's runtime dir so the writer (daemon) and reader (`serialcap log`)
/// always agree on the path.
pub fn capture_dir() -> Result<PathBuf> {
    let dir = crate::daemon::runtime_dir()?.join("capture");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The capture sub-directory for one named interface.
pub fn interface_dir(base: &Path, name: &str) -> PathBuf {
    base.join(sanitize(name))
}

/// Make an interface name safe as a single path component (interface names are
/// user-chosen). Keeps alphanumerics, `-`, `_`, `.`; collapses everything else
/// to `_`. An empty/degenerate result falls back to `_`.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        "_".to_string()
    } else {
        s
    }
}

/// Names of interfaces that have a capture sub-directory under `base`.
pub(crate) fn list_interface_dirs(base: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(base) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(n) = e.file_name().to_str() {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

// ── writer (owned by the capture thread) ────────────────────────────────────

/// Append-only writer that turns a byte stream into timestamped lines on disk.
/// Lives on a dedicated thread; never shared, so it needs no locking.
pub struct LineLog {
    dir: PathBuf,
    active_path: PathBuf,
    writer: Option<File>,
    next_seq: u64,
    seg_lines: u64,
    active_lines: u64,
    pending: Vec<u8>,
    pending_ts: Option<u64>,
    /// True when `pending` has changed on disk since the sidecar last mirrored
    /// it — see [`Self::sidecar_due`].
    sidecar_dirty: bool,
    last_sidecar_write: Option<Instant>,
    /// Real disk writes the sidecar has made. Test-only instrumentation for
    /// proving the debounce actually bounds them.
    #[cfg(test)]
    sidecar_writes: u64,
}

impl LineLog {
    /// Open (creating it if needed) the capture log in `dir`, resuming the
    /// sequence counter after the highest line already on disk. A stale pending
    /// sidecar from a previous run is discarded (its line was never completed).
    pub fn open(dir: PathBuf, buffer_lines: u64) -> Self {
        let _ = fs::create_dir_all(&dir);
        let active_path = dir.join(ACTIVE);
        let seg_lines = (buffer_lines / MAX_SEGMENTS as u64).max(1);
        let next_seq = recover_next_seq(&dir);
        let active_lines = count_lines(&active_path);
        let _ = fs::remove_file(dir.join(PENDING));
        let writer = open_private_append(&active_path).ok();
        LineLog {
            dir,
            active_path,
            writer,
            next_seq,
            seg_lines,
            active_lines,
            pending: Vec::new(),
            pending_ts: None,
            sidecar_dirty: false,
            last_sidecar_write: None,
            #[cfg(test)]
            sidecar_writes: 0,
        }
    }

    /// Feed a chunk of received bytes. Completed lines are appended to the log;
    /// any trailing partial line is mirrored to the sidecar (debounced — see
    /// [`Self::sidecar_due`]).
    pub fn ingest(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                if self.pending.last() == Some(&b'\r') {
                    self.pending.pop();
                }
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                let ts = self.pending_ts.take().unwrap_or_else(now_ms);
                self.commit(ts, text);
                self.pending.clear();
            } else {
                if self.pending.is_empty() {
                    self.pending_ts = Some(now_ms());
                }
                self.pending.push(b);
                if self.pending.len() >= MAX_PENDING {
                    // Force-flush at a UTF-8 boundary: splitting mid-sequence
                    // would turn a character that just needed one more byte
                    // into U+FFFD once `from_utf8_lossy` runs on this half.
                    let boundary = utf8_floor_boundary(&self.pending);
                    let text = String::from_utf8_lossy(&self.pending[..boundary]).into_owned();
                    let ts = self.pending_ts.take().unwrap_or_else(now_ms);
                    self.commit(ts, text);
                    self.pending.drain(..boundary);
                    if !self.pending.is_empty() {
                        self.pending_ts = Some(now_ms());
                    }
                }
            }
        }
        self.note_pending_dirty();
    }

    fn commit(&mut self, ts_ms: u64, text: String) {
        let line = Line {
            seq: self.next_seq,
            ts_ms,
            text,
            partial: false,
        };
        self.next_seq += 1;
        match self.writer.as_mut() {
            None => error!("capture writer is None; line seq={} lost", line.seq),
            Some(w) => match serde_json::to_string(&line) {
                Err(e) => error!("capture serialize seq={} failed: {e}", line.seq),
                Ok(mut s) => {
                    s.push('\n');
                    match w.write_all(s.as_bytes()) {
                        Ok(()) => self.active_lines += 1,
                        Err(e) => error!("capture write seq={} failed: {e}", line.seq),
                    }
                }
            },
        }
        if self.active_lines >= self.seg_lines {
            self.rotate();
        }
    }

    /// Shift `serial.jsonl(.k)` → `.k+1`, dropping the oldest, and start a fresh
    /// active segment. Keeps at most `MAX_SEGMENTS` files.
    fn rotate(&mut self) {
        self.writer = None; // close before renaming
        let _ = fs::remove_file(self.dir.join(format!("{ACTIVE}.{}", MAX_SEGMENTS - 1)));
        for k in (1..MAX_SEGMENTS - 1).rev() {
            let from = self.dir.join(format!("{ACTIVE}.{k}"));
            let to = self.dir.join(format!("{ACTIVE}.{}", k + 1));
            let _ = fs::rename(&from, &to);
        }
        let _ = fs::rename(&self.active_path, self.dir.join(format!("{ACTIVE}.1")));
        self.writer = open_private_append(&self.active_path).ok();
        self.active_lines = 0;
    }

    /// Mirror the current unterminated line to the sidecar (or remove it when the
    /// pending buffer is empty). Written via a temp file + rename so a concurrent
    /// reader never sees a half-written sidecar.
    fn write_pending_sidecar(&self) {
        let path = self.dir.join(PENDING);
        if self.pending.is_empty() {
            let _ = fs::remove_file(&path);
            return;
        }
        let line = Line {
            seq: self.next_seq,
            ts_ms: self.pending_ts.unwrap_or_else(now_ms),
            text: String::from_utf8_lossy(&self.pending).into_owned(),
            partial: true,
        };
        if let Ok(s) = serde_json::to_string(&line) {
            let tmp = self.dir.join("pending.tmp");
            if write_private(&tmp, s.as_bytes()).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }

    /// Mark the pending line changed and write the sidecar now if the
    /// debounce window has elapsed since the last write, or leave it dirty
    /// for [`Self::sidecar_due`]/[`Self::flush_sidecar`] to pick up later.
    fn note_pending_dirty(&mut self) {
        self.sidecar_dirty = true;
        self.maybe_flush_sidecar(Instant::now());
    }

    fn maybe_flush_sidecar(&mut self, now: Instant) {
        if !self.sidecar_dirty {
            return;
        }
        let due = match self.last_sidecar_write {
            None => true,
            Some(last) => now.duration_since(last) >= SIDECAR_DEBOUNCE,
        };
        if due {
            self.write_pending_sidecar();
            self.last_sidecar_write = Some(now);
            self.sidecar_dirty = false;
            #[cfg(test)]
            {
                self.sidecar_writes += 1;
            }
        }
    }

    /// How long the capture thread should wait before an outstanding,
    /// debounced sidecar write becomes due; `None` when nothing is pending
    /// (safe to block indefinitely on the next chunk). Lets an idle port
    /// still get its last partial line mirrored within the debounce window
    /// rather than only on the next byte received — see `spawn_capture` in
    /// `serial_io.rs`.
    pub fn sidecar_due(&self, now: Instant) -> Option<Duration> {
        if !self.sidecar_dirty {
            return None;
        }
        let last = self.last_sidecar_write?;
        Some(SIDECAR_DEBOUNCE.saturating_sub(now.duration_since(last)))
    }

    /// Write the sidecar now regardless of the debounce window, clearing the
    /// dirty flag. Called when [`Self::sidecar_due`]'s deadline elapses, and
    /// on capture-thread shutdown so the last partial line is never lost to
    /// a debounce window that never got to close.
    pub fn flush_sidecar(&mut self) {
        if self.sidecar_dirty {
            self.write_pending_sidecar();
            self.last_sidecar_write = Some(Instant::now());
            self.sidecar_dirty = false;
            #[cfg(test)]
            {
                self.sidecar_writes += 1;
            }
        }
    }
}

/// The largest prefix of `bytes` that is safe to treat as complete UTF-8:
/// the whole slice, unless it ends mid-way through an otherwise-valid
/// multibyte sequence — in which case the incomplete tail is excluded, so a
/// force-flush (`MAX_PENDING`) can't turn a character that just needed one
/// more byte into a replacement character. An invalid byte that is *not* at
/// the end needs no such care: `from_utf8_lossy` already replaces exactly
/// that byte wherever the text is decoded, so only end-of-buffer truncation
/// shortens the boundary.
fn utf8_floor_boundary(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => e.error_len().map_or(e.valid_up_to(), |_| bytes.len()),
    }
}

/// Open `path` for append, creating it 0600 on Unix. The capture holds
/// whatever the target printed — boot logs, prompts, anything typed at them —
/// so a fresh file must be unreadable by other users on its own, not only by
/// virtue of the runtime base's 0700. As with any `open(2)`, the mode applies
/// at creation; an existing file keeps what it has.
pub(crate) fn open_private_append(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

/// `fs::write`, but creating the file 0600 on Unix (see
/// [`open_private_append`]).
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut o = OpenOptions::new();
    o.create(true).write(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)?.write_all(bytes)
}

// ── reader (used by `serialcap log`) ─────────────────────────────────────────

/// A line selection for [`read_lines`]. An unset field imposes no constraint.
#[derive(Default)]
pub struct Query {
    /// Keep only the most recent N lines (applied after the seq filters).
    pub tail: Option<u64>,
    /// Lowest sequence number to include (inclusive).
    pub from: Option<u64>,
    /// Highest sequence number to include (inclusive).
    pub to: Option<u64>,
    /// Only lines with `seq` strictly greater than this.
    pub since: Option<u64>,
    /// Fold in the current unterminated line as the last (partial) entry.
    pub include_pending: bool,
}

/// Read captured lines from `dir`, oldest first, applying `q`.
pub fn read_lines(dir: &Path, q: &Query) -> Vec<Line> {
    let mut all: Vec<Line> = Vec::new();
    for k in (1..MAX_SEGMENTS).rev() {
        for_each_line(&dir.join(format!("{ACTIVE}.{k}")), |l| all.push(l));
    }
    for_each_line(&dir.join(ACTIVE), |l| all.push(l));

    if q.include_pending {
        if let Some(p) = read_pending(dir) {
            // The sidecar is debounced independently of completed appends.
            // It can still describe a line that has already been committed.
            // Only a sequence newer than all completed records is pending.
            if all.iter().all(|line| line.seq < p.seq) {
                all.push(p);
            }
        }
    }

    let mut out: Vec<Line> = all
        .into_iter()
        .filter(|l| {
            if let Some(s) = q.since {
                if l.seq <= s {
                    return false;
                }
            }
            if let Some(f) = q.from {
                if l.seq < f {
                    return false;
                }
            }
            if let Some(t) = q.to {
                if l.seq > t {
                    return false;
                }
            }
            true
        })
        .collect();

    if let Some(n) = q.tail {
        let n = n as usize;
        if out.len() > n {
            out.drain(0..out.len() - n);
        }
    }
    out
}

fn read_pending(dir: &Path) -> Option<Line> {
    let s = fs::read_to_string(dir.join(PENDING)).ok()?;
    serde_json::from_str(&s).ok()
}

/// Parse each JSON-lines record in `path`, invoking `f` per valid line. Missing
/// files and unparseable lines (e.g. a torn final append) are skipped silently.
fn for_each_line(path: &Path, mut f: impl FnMut(Line)) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(parsed) = serde_json::from_str::<Line>(&line) {
            f(parsed);
        }
    }
}

fn recover_next_seq(dir: &Path) -> u64 {
    let mut max_seq: Option<u64> = None;
    for k in (1..MAX_SEGMENTS).rev() {
        for_each_line(&dir.join(format!("{ACTIVE}.{k}")), |l| {
            max_seq = Some(max_seq.map_or(l.seq, |m| m.max(l.seq)));
        });
    }
    for_each_line(&dir.join(ACTIVE), |l| {
        max_seq = Some(max_seq.map_or(l.seq, |m| m.max(l.seq)));
    });
    max_seq.map_or(0, |m| m + 1)
}

fn count_lines(path: &Path) -> u64 {
    let mut n = 0;
    for_each_line(path, |_| n += 1);
    n
}

// ── the `log` subcommand ─────────────────────────────────────────────────────

/// Options for [`cmd_log`], mirroring the `serialcap log` CLI flags.
pub struct LogArgs {
    pub interface: Option<String>,
    pub tail: Option<u64>,
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub since: Option<u64>,
    pub raw: bool,
    pub json: bool,
    pub no_pending: bool,
}

/// Print captured lines to stdout per `args`.
pub fn cmd_log(args: LogArgs) -> Result<()> {
    let base = capture_dir().context("locating capture dir")?;
    let dir = match &args.interface {
        Some(name) => interface_dir(&base, name),
        None => {
            let names = list_interface_dirs(&base);
            match names.len() {
                // Exactly one interface (or none yet): no need to disambiguate.
                0 | 1 => names.first().map_or(base.clone(), |n| base.join(n)),
                // Expected user-choice condition, not a fault: a clean message,
                // no error chain / backtrace.
                _ => {
                    eprintln!(
                        "serialcap: multiple interfaces captured ({}); pass --interface NAME",
                        names.join(", ")
                    );
                    std::process::exit(2);
                }
            }
        }
    };

    // With no selector, default to a recent window rather than the whole history.
    let no_selector =
        args.tail.is_none() && args.from.is_none() && args.to.is_none() && args.since.is_none();
    let q = Query {
        tail: if no_selector { Some(200) } else { args.tail },
        from: args.from,
        to: args.to,
        since: args.since,
        include_pending: !args.no_pending,
    };

    let lines = read_lines(&dir, &q);
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for l in lines {
        if args.json {
            if let Ok(s) = serde_json::to_string(&l) {
                let _ = writeln!(out, "{s}");
            }
        } else {
            let text = if args.raw {
                l.text
            } else {
                strip_ansi(&l.text)
            };
            let seq = if l.partial {
                format!("{}*", l.seq)
            } else {
                l.seq.to_string()
            };
            let _ = writeln!(out, "[{}] #{:<7} {}", format_utc(l.ts_ms), seq, text);
        }
    }
    Ok(())
}

// ── formatting helpers ───────────────────────────────────────────────────────

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Format epoch milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC). Done by hand so
/// the crate needs no calendar dependency.
fn format_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d)
}

/// Strip ANSI escape sequences and control noise for readable, agent-friendly
/// text. Removes CSI/OSC and other escape sequences, applies bare-`\r` carriage
/// returns as overwrites (keeps text after the last `\r`), and drops remaining
/// control characters except tab. Operates on raw bytes so it is robust to
/// partial UTF-8.
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    // CSI: parameters/intermediates until a final byte 0x40..=0x7e.
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    // OSC (]), DCS (P), APC (_), PM (^), SOS (X): all run
                    // until BEL or ST (ESC \). The other four used to fall
                    // into the two-byte-escape catch-all below, which only
                    // consumed the introducer and left their payload —
                    // whatever the target wrote between the introducer and
                    // its terminator — in the "clean" text.
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => i += 1, // two-byte escape: drop the following byte too
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }

    let cleaned = String::from_utf8_lossy(&out);
    // Bare carriage returns redraw the line; keep only the final overwrite.
    let cleaned = match cleaned.rfind('\r') {
        Some(idx) => &cleaned[idx + 1..],
        None => &cleaned,
    };
    cleaned
        .chars()
        .filter(|&c| (c == '\t' || !c.is_control()) && !is_format_char(c))
        .collect()
}

/// True for the Unicode "format" (Cf) characters that matter in text pulled
/// off a terminal-derived stream: the bidirectional overrides/embeddings/
/// isolates (the U+202E RIGHT-TO-LEFT OVERRIDE class — printed text can
/// visually reorder itself, hiding what a reader or an agent parsing
/// `serialcap log` actually sees), the zero-width spacing/joining marks, and
/// the byte-order mark. `char::is_control` (Cc) does not cover these — they
/// are not control characters, just invisible or reordering ones. This is a
/// curated subset of Cf, not the whole category (which also has rare
/// non-BMP annotation and language-tag characters); it is the part that can
/// plausibly reach a captured console line.
fn is_format_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}' // soft hyphen
        | '\u{0600}'..='\u{0605}' // Arabic number/sign marks
        | '\u{061C}' // Arabic letter mark
        | '\u{06DD}' // Arabic end of ayah
        | '\u{070F}' // Syriac abbreviation mark
        | '\u{200B}'..='\u{200F}' // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
        | '\u{2060}'..='\u{2064}' // word joiner, invisible separators
        | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
        | '\u{FEFF}' // BOM / zero width no-break space
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "serialcap-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Every file the writer creates — the active segment, the segment a
    /// rotation opens, and the pending sidecar — must be readable by nobody
    /// else. With a one-line segment size each ingested line rotates, so one
    /// ingest exercises all three creation paths.
    #[cfg(unix)]
    #[test]
    fn capture_files_are_created_private() {
        use std::os::unix::fs::MetadataExt;
        let dir = tmp();
        // seg_lines = max(1 / MAX_SEGMENTS, 1) = 1: every committed line rotates.
        let mut log = LineLog::open(dir.clone(), 1);
        log.ingest(b"first\nsecond\npartial");
        let mode = |name: &str| fs::metadata(dir.join(name)).unwrap().mode() & 0o777;
        assert_eq!(mode(ACTIVE), 0o600, "active segment (opened by rotate)");
        assert_eq!(
            mode(&format!("{ACTIVE}.2")),
            0o600,
            "oldest segment (opened by LineLog::open)"
        );
        assert_eq!(mode(PENDING), 0o600, "pending sidecar");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn splits_lines_and_strips_cr() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
        log.ingest(b"hello\r\nworld\n");
        let lines = read_lines(&dir, &Query::default());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[0].seq, 0);
        assert_eq!(lines[1].text, "world");
        assert_eq!(lines[1].seq, 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unterminated_line_is_pending() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
        log.ingest(b"done\nlogin: ");
        let with = read_lines(
            &dir,
            &Query {
                include_pending: true,
                ..Default::default()
            },
        );
        assert_eq!(with.len(), 2);
        assert!(with[1].partial);
        assert_eq!(with[1].text, "login: ");
        assert_eq!(with[1].seq, 1); // seq it will take once completed

        let without = read_lines(&dir, &Query::default());
        assert_eq!(without.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tail_and_range_and_since() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
        for i in 0..10 {
            log.ingest(format!("line{i}\n").as_bytes());
        }
        let tail = read_lines(
            &dir,
            &Query {
                tail: Some(3),
                ..Default::default()
            },
        );
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].text, "line7");
        assert_eq!(tail[2].text, "line9");

        let range = read_lines(
            &dir,
            &Query {
                from: Some(2),
                to: Some(4),
                ..Default::default()
            },
        );
        assert_eq!(
            range.iter().map(|l| l.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        let since = read_lines(
            &dir,
            &Query {
                since: Some(7),
                ..Default::default()
            },
        );
        assert_eq!(since.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![8, 9]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn seq_resumes_after_reopen() {
        let dir = tmp();
        {
            let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
            log.ingest(b"a\nb\n");
        }
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
        log.ingest(b"c\n");
        let lines = read_lines(&dir, &Query::default());
        assert_eq!(
            lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(lines[2].text, "c");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotation_drops_oldest_keeps_recent() {
        let dir = tmp();
        // buffer_lines / MAX_SEGMENTS = seg_lines; 50 / 5 = 10 lines per segment.
        let mut log = LineLog::open(dir.clone(), 50);
        for i in 0..120 {
            log.ingest(format!("line{i}\n").as_bytes());
        }
        let lines = read_lines(&dir, &Query::default());
        // At most MAX_SEGMENTS * seg_lines retained, oldest aged out.
        assert!(lines.len() <= 50, "retained {} lines", lines.len());
        assert_eq!(lines.last().unwrap().text, "line119");
        assert_eq!(lines.last().unwrap().seq, 119);
        // Sequence numbers stay monotonic and contiguous over what survives.
        for w in lines.windows(2) {
            assert_eq!(w[1].seq, w[0].seq + 1);
        }
        fs::remove_dir_all(&dir).ok();
    }

    // ── sidecar debounce (Review low e) ──────────────────────────────────

    /// A burst of changes inside the debounce window must not each rewrite
    /// the sidecar; the first change (nothing written yet) and the first
    /// change past the window each do.
    #[test]
    fn pending_sidecar_writes_are_debounced_but_eventually_flush() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);

        log.ingest(b"a");
        assert_eq!(
            log.sidecar_writes, 1,
            "nothing written yet: goes straight through"
        );

        for _ in 0..50 {
            log.ingest(b"b");
        }
        assert_eq!(
            log.sidecar_writes, 1,
            "debounced: no extra writes inside the window"
        );

        std::thread::sleep(SIDECAR_DEBOUNCE + Duration::from_millis(50));
        log.ingest(b"c");
        assert_eq!(log.sidecar_writes, 2, "past the window: writes again");

        // Debouncing must not lose data, only the intermediate writes: the
        // final on-disk content reflects everything ingested.
        let pending = read_lines(
            &dir,
            &Query {
                include_pending: true,
                ..Default::default()
            },
        );
        let want = format!("a{}c", "b".repeat(50));
        assert_eq!(pending.last().unwrap().text, want);
        fs::remove_dir_all(&dir).ok();
    }

    /// The pure state machine behind the debounce: dirty/last-write tracking
    /// and the deadline `sidecar_due` reports, driven directly rather than by
    /// real sleeps.
    #[test]
    fn sidecar_due_and_flush_sidecar_state_transitions() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
        let now = Instant::now();

        assert_eq!(log.sidecar_due(now), None, "nothing pending yet");

        log.sidecar_dirty = true;
        log.last_sidecar_write = Some(now);
        assert_eq!(log.sidecar_due(now), Some(SIDECAR_DEBOUNCE));
        assert_eq!(
            log.sidecar_due(now + SIDECAR_DEBOUNCE + Duration::from_millis(1)),
            Some(Duration::ZERO)
        );

        log.pending = b"partial".to_vec();
        log.flush_sidecar();
        assert!(!log.sidecar_dirty, "flush_sidecar clears the dirty flag");
        assert!(log.last_sidecar_write.is_some());
        fs::remove_dir_all(&dir).ok();
    }

    // ── UTF-8-safe force flush (Review low f) ────────────────────────────

    #[test]
    fn utf8_floor_boundary_flushes_whole_valid_input() {
        assert_eq!(utf8_floor_boundary("hello".as_bytes()), 5);
    }

    #[test]
    fn utf8_floor_boundary_holds_back_a_truncated_trailing_sequence() {
        let full = "a€".as_bytes(); // 'a' (1 byte) + '€' (3 bytes) = 4 bytes.
        assert_eq!(utf8_floor_boundary(full), 4, "complete input flushes whole");
        assert_eq!(
            utf8_floor_boundary(&full[..3]),
            1,
            "2 of 3 continuation bytes: hold the lead byte back too"
        );
        assert_eq!(
            utf8_floor_boundary(&full[..2]),
            1,
            "1 of 3 continuation bytes: hold back"
        );
        assert_eq!(
            utf8_floor_boundary(&full[..1]),
            1,
            "just 'a': already a complete char"
        );
    }

    #[test]
    fn utf8_floor_boundary_flushes_past_an_interior_invalid_byte() {
        // 0xFF is never valid UTF-8 and it is not at the end, so it is not a
        // truncation — from_utf8_lossy's usual per-byte replacement handles
        // it; no boundary protection is needed.
        let bytes = [b'a', 0xFF, b'b'];
        assert_eq!(utf8_floor_boundary(&bytes), 3);
    }

    /// A force-flush landing mid-character must not turn the last legible
    /// character into U+FFFD; it must hold the incomplete tail back for the
    /// next chunk to complete.
    #[test]
    fn force_flush_does_not_mangle_a_multibyte_char_at_the_boundary() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), DEFAULT_BUFFER_LINES);
        log.ingest(&vec![b'a'; MAX_PENDING - 1]);
        log.ingest("€".as_bytes());
        log.flush_sidecar(); // bypass the debounce so the read below is current
        let lines = read_lines(
            &dir,
            &Query {
                include_pending: true,
                ..Default::default()
            },
        );
        let joined: String = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(
            !joined.contains('\u{FFFD}'),
            "a multibyte char was split: {joined:?}"
        );
        assert!(
            joined.ends_with('€'),
            "the character must survive intact: {joined:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strip_ansi_removes_color_and_handles_cr() {
        assert_eq!(strip_ansi("\x1b[1;32mgreen\x1b[0m text"), "green text");
        assert_eq!(strip_ansi("progress 10%\rprogress 90%"), "progress 90%");
        assert_eq!(strip_ansi("keep\ttab"), "keep\ttab");
        assert_eq!(strip_ansi("bell\x07gone"), "bellgone");
        // OSC title sequence terminated by BEL.
        assert_eq!(strip_ansi("\x1b]0;my title\x07shell"), "shell");
    }

    /// Review lows (f). DCS/APC/PM/SOS used to fall into the generic
    /// two-byte-escape case, which only ate the introducer and left the
    /// sequence's own payload — anything the target wrote before the
    /// terminator — sitting in the "clean" text. Each must be consumed like
    /// OSC, to its own BEL or ST (ESC \) terminator.
    #[test]
    fn strip_ansi_consumes_dcs_apc_pm_sos_payloads_to_their_terminator() {
        // DCS (ESC P) terminated by ST.
        assert_eq!(
            strip_ansi("before\x1bPsome dcs payload\x1b\\after"),
            "beforeafter"
        );
        // APC (ESC _) terminated by BEL.
        assert_eq!(strip_ansi("before\x1b_hidden apc\x07after"), "beforeafter");
        // PM (ESC ^) terminated by ST.
        assert_eq!(
            strip_ansi("before\x1b^private message\x1b\\after"),
            "beforeafter"
        );
        // SOS (ESC X) terminated by ST.
        assert_eq!(
            strip_ansi("before\x1bXstart of string\x1b\\after"),
            "beforeafter"
        );
    }

    /// Review lows (f). Unicode format characters (Cf) are invisible or
    /// reordering, not `char::is_control`, so the old filter let them
    /// through. U+202E in particular can make an extension like "evil.exe"
    /// display as something else entirely.
    #[test]
    fn strip_ansi_removes_bidi_override_and_zero_width_format_chars() {
        assert_eq!(strip_ansi("safe\u{202E}evil.exe"), "safeevil.exe");
        assert_eq!(
            strip_ansi("zero\u{200B}width\u{FEFF}space"),
            "zerowidthspace"
        );
    }

    #[test]
    fn format_utc_known_values() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        // 2021-01-01T00:00:00Z = 1609459200 s.
        assert_eq!(format_utc(1_609_459_200_000), "2021-01-01T00:00:00.000Z");
        assert_eq!(format_utc(1_609_459_200_123), "2021-01-01T00:00:00.123Z");
    }

    #[test]
    fn completed_records_win_over_a_stale_pending_sidecar() {
        let dir = tmp();
        let mut log = LineLog::open(dir.clone(), 100);
        log.ingest(b"login:");
        let stale = fs::read(dir.join(PENDING)).unwrap();
        log.ingest(b" ready\nnext\n");
        // Model the sidecar snapshot a reader can see during debounce,
        // without depending on how fast the test machine runs.
        fs::write(dir.join(PENDING), stale).unwrap();
        let query = Query {
            include_pending: true,
            ..Default::default()
        };
        let lines = read_lines(&dir, &query);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "login: ready");
        assert!(lines.iter().all(|l| !l.partial));
        let tail = read_lines(
            &dir,
            &Query {
                tail: Some(1),
                include_pending: true,
                ..Default::default()
            },
        );
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text, "next");
        let since = read_lines(
            &dir,
            &Query {
                since: Some(0),
                include_pending: true,
                ..Default::default()
            },
        );
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].seq, 1);
        log.ingest(b"new prompt");
        log.flush_sidecar();
        let lines = read_lines(&dir, &query);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].partial);
        assert_eq!(lines[2].seq, 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
