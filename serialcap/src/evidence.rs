// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! Immutable receive chunks, separate from the assembled line/pending view.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_BYTES: u64 = 65536;
pub const MAX_BYTES: u64 = 1048576;
const SEGMENT_BYTES: u64 = 1048576;
const SEGMENTS: usize = 5;
// A supervisor read contains at most 65536 bytes; JSON encodes each in <=4
// characters. Bound damaged files too, including a maximal trailing record.
const SCAN_SEGMENT_BYTES: u64 = SEGMENT_BYTES + 300000;
const MAX_DIAGNOSTICS: usize = 64;
const ACTIVE: &str = "evidence.jsonl";
const STATE: &str = "evidence-state.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub rx_seq: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReceiveRecord {
    pub epoch: String,
    pub rx_seq: u64,
    pub connection_generation: u64,
    pub rx_at_ms: u64,
    pub captured_at_ms: u64,
    pub bytes: Vec<u8>,
}

impl ReceiveRecord {
    fn valid(&self) -> bool {
        self.rx_seq > 0
            && !self.bytes.is_empty()
            && self.bytes.len() <= 65536
            && !self.epoch.is_empty()
            && self.epoch.len() <= 128
    }
    fn cursor(&self) -> Cursor {
        Cursor {
            epoch: self.epoch.clone(),
            rx_seq: self.rx_seq,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct WriterState {
    last_received: Option<Cursor>,
    #[serde(default)]
    persisted: Option<Cursor>,
    error: Option<String>,
    #[serde(default)]
    failed_receive: Option<ReceiveRecord>,
    #[serde(default)]
    failed_chunks: u64,
    #[serde(default)]
    failed_bytes: u64,
}

impl WriterState {
    fn valid(&self) -> bool {
        self.last_received
            .iter()
            .chain(self.persisted.iter())
            .all(|c| !c.epoch.is_empty() && c.epoch.len() <= 128)
            && self
                .failed_receive
                .as_ref()
                .is_none_or(ReceiveRecord::valid)
    }
}

/// A single capture thread owns this writer. Rotation never rewrites records.
pub struct Journal {
    dir: PathBuf,
    segment_bytes: u64,
    state: WriterState,
    failure_archived: bool,
}

impl Journal {
    pub fn open(dir: PathBuf) -> Self {
        Self {
            dir,
            segment_bytes: SEGMENT_BYTES,
            state: WriterState::default(),
            failure_archived: false,
        }
    }

    pub fn start_epoch(&mut self, epoch: String) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        self.state.last_received = Some(Cursor { epoch, rx_seq: 0 });
        self.state.persisted = self.state.last_received.clone();
        self.save_state()
    }

    fn save_state(&self) -> Result<()> {
        let temp = self.dir.join("evidence-state.tmp");
        crate::capture::write_private(&temp, &serde_json::to_vec(&self.state)?)?;
        fs::rename(temp, self.dir.join(STATE))?;
        Ok(())
    }

    pub fn append(&mut self, record: &ReceiveRecord) -> Result<()> {
        self.state.last_received = Some(record.cursor());
        // Keep the first fault visible even if later writes succeed.
        let result = self.append_inner(record);
        if let Err(e) = &result {
            self.state.failed_chunks += 1;
            self.state.failed_bytes += record.bytes.len() as u64;
            self.state.error.get_or_insert_with(|| e.to_string());
            self.state
                .failed_receive
                .get_or_insert_with(|| record.clone());
            if !self.failure_archived {
                self.failure_archived = self.archive_failure(record).is_ok();
            }
            // This may succeed for a damaged journal even when its append failed.
            let _ = self.save_state();
        }
        result
    }

    fn archive_failure(&self, record: &ReceiveRecord) -> Result<()> {
        for index in (1..SEGMENTS).rev() {
            let dest = self.dir.join(format!("evidence-failure.{index}.json"));
            let source = self
                .dir
                .join(format!("evidence-failure.{}.json", index - 1));
            if dest.exists() {
                fs::remove_file(&dest)?;
            }
            if source.exists() {
                fs::rename(source, dest)?;
            }
        }
        crate::capture::write_private(
            &self.dir.join("evidence-failure.0.json"),
            &serde_json::to_vec(record)?,
        )?;
        Ok(())
    }

    fn append_inner(&mut self, record: &ReceiveRecord) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        // Publish receive progress first: a crash or failed append is a gap,
        // rather than an apparently complete shorter history.
        self.save_state().context("recording received cursor")?;
        let mut data = serde_json::to_vec(record)?;
        data.push(b'\n');
        let active = self.dir.join(ACTIVE);
        let size = fs::metadata(&active).map_or(0, |m| m.len());
        if size > 0 && size + data.len() as u64 > self.segment_bytes {
            for index in (1..SEGMENTS).rev() {
                let dest = self.dir.join(format!("{ACTIVE}.{index}"));
                let source = if index == 1 {
                    active.clone()
                } else {
                    self.dir.join(format!("{ACTIVE}.{}", index - 1))
                };
                if dest.exists() {
                    fs::remove_file(&dest).context("removing expired evidence segment")?;
                }
                if source.exists() {
                    fs::rename(source, dest).context("rotating evidence segment")?;
                }
            }
        }
        let mut file = crate::capture::open_private_append(&active)?;
        file.write_all(&data)
            .context("appending receive evidence")?;
        file.flush().context("flushing receive evidence")?;
        self.state.persisted = Some(record.cursor());
        self.save_state().context("recording persisted cursor")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_bytes: Option<Vec<u8>>,
    pub raw_length: usize,
    pub raw_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receive_cursor: Option<Cursor>,
}

fn diagnostic(kind: &str, message: impl ToString) -> Diagnostic {
    Diagnostic {
        kind: kind.into(),
        message: message.to_string().chars().take(128).collect(),
        segment: None,
        offset: None,
        raw_bytes: None,
        raw_length: 0,
        raw_truncated: false,
        receive_cursor: None,
    }
}

#[derive(Debug, Serialize)]
pub struct Evidence {
    pub records: Vec<ReceiveRecord>,
    pub observed_at_ms: u64,
    pub epoch: Option<String>,
    pub available_epochs: Vec<String>,
    pub earliest: Option<Cursor>,
    pub latest: Option<Cursor>,
    pub continuation: Option<Cursor>,
    pub raw_bytes: usize,
    pub capped: bool,
    pub next_record_bytes: Option<usize>,
    pub gap: bool,
    pub epoch_missing: bool,
    pub capture_pending: bool,
    pub epochs_truncated: bool,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn read(
    dir: &Path,
    epoch: Option<&str>,
    since: Option<u64>,
    max_bytes: u64,
) -> Result<Evidence> {
    if since.is_some() && epoch.is_none() {
        bail!("--since requires --epoch");
    }
    if max_bytes == 0 || max_bytes > MAX_BYTES {
        bail!("--max-bytes must be 1..={MAX_BYTES}");
    }
    let mut out = Evidence {
        records: vec![],
        observed_at_ms: crate::capture::now_ms(),
        epoch: None,
        available_epochs: vec![],
        earliest: None,
        latest: None,
        continuation: None,
        raw_bytes: 0,
        capped: false,
        next_record_bytes: None,
        gap: false,
        epoch_missing: false,
        capture_pending: false,
        epochs_truncated: false,
        diagnostics: vec![],
    };
    let mut acquired = Vec::new();
    let mut found = false;
    let state = match fs::File::open(dir.join(STATE)) {
        Ok(file) => {
            let mut bytes = vec![];
            file.take(300000).read_to_end(&mut bytes)?;
            match serde_json::from_slice::<WriterState>(&bytes) {
                Ok(state) if state.valid() => Some(state),
                Ok(_) => {
                    out.gap = true;
                    out.diagnostics.push(diagnostic(
                        "state_corrupt",
                        "invalid writer checkpoint metadata",
                    ));
                    None
                }
                Err(e) => {
                    out.gap = true;
                    out.diagnostics.push(diagnostic("state_corrupt", e));
                    None
                }
            }
        }
        Err(e) => {
            out.diagnostics.push(diagnostic("state_unavailable", e));
            None
        }
    };
    // Oldest segment first. Open descriptors survive a concurrent rotation.
    let mut files = Vec::new();
    for index in (0..SEGMENTS).rev() {
        let name = if index == 0 {
            ACTIVE.into()
        } else {
            format!("{ACTIVE}.{index}")
        };
        match fs::File::open(dir.join(&name)) {
            Ok(file) => {
                found = true;
                files.push((name, file));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                out.gap = true;
                out.diagnostics.push(diagnostic("read_error", e));
            }
        }
    }
    'segments: for (name, file) in files {
        if file
            .metadata()
            .map_or(true, |m| m.len() > SCAN_SEGMENT_BYTES)
        {
            out.gap = true;
            out.diagnostics.push(diagnostic(
                "scan_limit",
                "damaged segment exceeds bounded scan size; bytes beyond limit were not read",
            ));
        }
        let mut reader = BufReader::new(file.take(SCAN_SEGMENT_BYTES));
        let mut offset = 0;
        loop {
            let mut bytes = Vec::new();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Err(e) => {
                    out.gap = true;
                    out.diagnostics.push(diagnostic("read_error", e));
                    break;
                }
                Ok(_) => {}
            }
            let next_offset = offset + bytes.len() as u64;
            if name == ACTIVE
                && !bytes.ends_with(b"\n")
                && state.as_ref().is_none_or(|s| s.error.is_none())
            {
                out.capture_pending = true;
                let mut d = diagnostic(
                    "incomplete_tail",
                    "active record was not committed; tail may still be appending",
                );
                d.segment = Some(name.clone());
                d.offset = Some(offset);
                d.raw_length = bytes.len();
                bytes.truncate((max_bytes as usize).saturating_sub(out.raw_bytes));
                out.raw_bytes += bytes.len();
                d.raw_truncated = bytes.len() != d.raw_length;
                d.raw_bytes = Some(bytes);
                out.capped |= d.raw_truncated;
                out.diagnostics.push(d);
                break;
            }
            match serde_json::from_slice::<ReceiveRecord>(&bytes) {
                Ok(record) if record.valid() => {
                    if state
                        .as_ref()
                        .and_then(|s| s.last_received.as_ref())
                        .is_some_and(|c| c.epoch == record.epoch)
                        && state
                            .as_ref()
                            .and_then(|s| s.persisted.as_ref())
                            .is_none_or(|c| c.epoch != record.epoch || c.rx_seq < record.rx_seq)
                    {
                        out.capture_pending = true;
                        offset = next_offset;
                        continue;
                    }
                    if !out.available_epochs.contains(&record.epoch) {
                        out.available_epochs.push(record.epoch.clone());
                    }
                    acquired.push(record);
                }
                result => {
                    out.gap = true;
                    let remaining = (max_bytes as usize).saturating_sub(out.raw_bytes);
                    let length = bytes.len();
                    bytes.truncate(remaining);
                    out.raw_bytes += bytes.len();
                    let mut d = diagnostic(
                        "corrupt_record",
                        match result {
                            Err(e) => e.to_string(),
                            Ok(_) => "empty or invalid receive record".into(),
                        },
                    );
                    d.segment = Some(name.clone());
                    d.offset = Some(offset);
                    d.raw_length = length;
                    d.raw_truncated = bytes.len() != length;
                    out.capped |= d.raw_truncated;
                    d.raw_bytes = Some(bytes);
                    out.diagnostics.push(d);
                    if out.diagnostics.len() >= MAX_DIAGNOSTICS {
                        out.diagnostics.push(diagnostic("diagnostic_limit", "corruption diagnostic limit reached; remaining journal bytes were not read"));
                        break 'segments;
                    }
                }
            }
            offset = next_offset;
        }
    }
    if !found && state.is_none() {
        out.diagnostics.push(diagnostic(
            "unavailable",
            "no raw receive journal; older daemons do not collect it",
        ));
    }
    out.epoch = epoch
        .map(str::to_owned)
        .or_else(|| {
            state
                .as_ref()
                .and_then(|s| s.last_received.as_ref().map(|c| c.epoch.clone()))
        })
        .or_else(|| out.available_epochs.last().cloned());
    if let Some(cursor) = state.as_ref().and_then(|s| s.last_received.as_ref()) {
        if !out.available_epochs.contains(&cursor.epoch) {
            out.available_epochs.push(cursor.epoch.clone());
        }
    }
    out.epoch_missing = epoch.is_some_and(|e| !out.available_epochs.iter().any(|v| v == e));
    if out.available_epochs.len() > 64 {
        let expired = out.available_epochs.len() - 64;
        out.available_epochs.drain(..expired);
        out.epochs_truncated = true;
        out.diagnostics.push(diagnostic("epoch_list_limit", "only the newest 64 known epochs are listed; requested epoch selection still checks all retained records"));
    }
    if out.epoch_missing {
        out.diagnostics.push(diagnostic(
            "epoch_missing",
            "requested epoch is absent or expired",
        ));
    }
    let mut selected: Vec<_> = acquired
        .into_iter()
        .filter(|r| Some(&r.epoch) == out.epoch.as_ref())
        .collect();
    selected.sort_by_key(|r| r.rx_seq);
    for pair in selected.windows(2) {
        if pair[0].rx_seq == pair[1].rx_seq && pair[0] != pair[1] {
            out.gap = true;
            if out.diagnostics.len() < MAX_DIAGNOSTICS {
                out.diagnostics.push(diagnostic(
                    "conflicting_cursor",
                    "different records share an epoch/RX sequence",
                ));
            }
        }
    }
    selected.dedup_by(|a, b| a == b);
    out.earliest = selected.first().map(ReceiveRecord::cursor);
    out.latest = selected.last().map(ReceiveRecord::cursor);
    let start = since.unwrap_or(0);
    let received_through = state
        .as_ref()
        .and_then(|s| s.last_received.as_ref())
        .filter(|c| Some(&c.epoch) == out.epoch.as_ref())
        .map_or(0, |c| c.rx_seq);
    if start > received_through && out.latest.as_ref().is_none_or(|c| start > c.rx_seq) {
        out.gap = true;
        out.diagnostics.push(diagnostic(
            "cursor_ahead",
            "requested cursor is ahead of retained receive history",
        ));
    }
    out.continuation = out.epoch.as_ref().map(|e| Cursor {
        epoch: e.clone(),
        rx_seq: start,
    });
    let mut expected = start.saturating_add(1);
    for record in selected.into_iter().filter(|r| r.rx_seq > start) {
        if record.rx_seq != expected {
            out.gap = true;
        }
        expected = record.rx_seq.saturating_add(1);
        if out.raw_bytes + record.bytes.len() > max_bytes as usize {
            out.capped = true;
            out.next_record_bytes = Some(record.bytes.len());
            break;
        }
        out.raw_bytes += record.bytes.len();
        out.continuation = Some(record.cursor());
        out.records.push(record);
    }
    if out.gap {
        out.diagnostics.push(diagnostic(
            "gap",
            "receive history has retention, corruption, read or sequence gaps",
        ));
    }
    let mut failed = vec![];
    for index in 0..SEGMENTS {
        let name = format!("evidence-failure.{index}.json");
        match fs::File::open(dir.join(&name)) {
            Ok(file) => {
                let mut bytes = vec![];
                file.take(300000).read_to_end(&mut bytes)?;
                match serde_json::from_slice::<ReceiveRecord>(&bytes) {
                    Ok(record) if record.valid() => {
                        if Some(&record.epoch) == out.epoch.as_ref() {
                            failed.push(record);
                        }
                    }
                    Ok(_) => {
                        out.gap = true;
                        out.diagnostics.push(diagnostic(
                            "failure_archive_corrupt",
                            "invalid failed-receive metadata",
                        ));
                    }
                    Err(e) => {
                        out.gap = true;
                        let mut d = diagnostic("failure_archive_corrupt", e);
                        d.segment = Some(name);
                        d.raw_length = bytes.len();
                        bytes.truncate((max_bytes as usize).saturating_sub(out.raw_bytes));
                        out.raw_bytes += bytes.len();
                        d.raw_truncated = bytes.len() != d.raw_length;
                        d.raw_bytes = Some(bytes);
                        out.capped |= d.raw_truncated;
                        out.diagnostics.push(d);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                out.gap = true;
                out.diagnostics
                    .push(diagnostic("failure_archive_unreadable", e));
            }
        }
    }
    if let Some(state) = state {
        if let Some(error) = state.error {
            out.gap = true;
            out.diagnostics.push(diagnostic(
                "write_error",
                format!(
                    "{} chunks / {} raw bytes failed journal writes: {error}",
                    state.failed_chunks, state.failed_bytes
                ),
            ));
        }
        if let Some(record) = state.failed_receive {
            if Some(&record.epoch) == out.epoch.as_ref() && !failed.contains(&record) {
                failed.push(record);
            }
        }
        if let Some(last) = state.last_received {
            if Some(&last.epoch) == out.epoch.as_ref()
                && out.latest.as_ref().is_none_or(|c| c.rx_seq < last.rx_seq)
            {
                out.capture_pending = true;
                out.diagnostics.push(diagnostic("capture_pending", "received cursor is ahead of the persisted journal; check live evidence health for write failure"));
            }
        }
    }
    for record in failed {
        out.gap = true;
        let mut d = diagnostic("failed_receive", "first failed receive chunk of an epoch; fault snapshots survive up to five subsequent failed epochs");
        d.receive_cursor = Some(record.cursor());
        let length = record.bytes.len();
        let mut bytes = record.bytes;
        bytes.truncate((max_bytes as usize).saturating_sub(out.raw_bytes));
        out.raw_bytes += bytes.len();
        d.raw_length = length;
        d.raw_truncated = bytes.len() != length;
        d.raw_bytes = Some(bytes);
        out.capped |= d.raw_truncated;
        out.diagnostics.push(d);
    }
    Ok(out)
}

pub fn select_dir(base: &Path, interface: Option<&str>) -> Result<PathBuf> {
    let name = match interface {
        Some(name) => name.to_string(),
        None => {
            let names = crate::capture::list_interface_dirs(base);
            if names.len() != 1 {
                bail!("select one captured interface with --interface NAME");
            }
            names[0].clone()
        }
    };
    Ok(crate::capture::interface_dir(base, &name))
}

pub fn cmd_evidence(
    interface: Option<String>,
    epoch: Option<String>,
    since: Option<u64>,
    max_bytes: u64,
) -> Result<()> {
    let base = crate::capture::capture_dir()?;
    let evidence = read(
        &select_dir(&base, interface.as_deref())?,
        epoch.as_deref(),
        since,
        max_bytes,
    )?;
    serde_json::to_writer(std::io::stdout().lock(), &evidence)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "serial-evidence-{label}-{}-{}",
            std::process::id(),
            crate::capture::now_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn record(epoch: &str, seq: u64, bytes: &[u8]) -> ReceiveRecord {
        ReceiveRecord {
            epoch: epoch.into(),
            rx_seq: seq,
            connection_generation: 1,
            rx_at_ms: 100 + seq,
            captured_at_ms: 200 + seq,
            bytes: bytes.into(),
        }
    }

    #[test]
    fn immutable_prompt_is_distinct_from_later_observation_echo_and_completion() {
        let dir = dir("prompt");
        let mut writer = Journal::open(dir.clone());
        let prompt = record("one", 1, b"prompt> ");
        writer.append(&prompt).unwrap();
        let before = read(&dir, None, None, 4096).unwrap();
        writer
            .append(&record("one", 2, b"command\r\n\xff\x00\x1b[31mcomplete\n"))
            .unwrap();
        let after = read(&dir, Some("one"), Some(0), 4096).unwrap();
        assert_eq!(before.records[0], prompt);
        assert_eq!(after.records[0], before.records[0]);
        assert_eq!(
            after.records[1].bytes,
            b"command\r\n\xff\x00\x1b[31mcomplete\n"
        );
        assert!(after.observed_at_ms >= before.observed_at_ms);
        assert_eq!(after.continuation.unwrap().rx_seq, 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn caps_stop_before_whole_records_without_advancing_cursor() {
        let dir = dir("cap");
        let mut writer = Journal::open(dir.clone());
        writer.append(&record("one", 1, b"abc")).unwrap();
        writer.append(&record("one", 2, b"defg")).unwrap();
        let first = read(&dir, Some("one"), Some(0), 2).unwrap();
        assert!(first.records.is_empty());
        assert!(first.capped);
        assert_eq!(first.continuation.unwrap().rx_seq, 0);
        assert_eq!(first.next_record_bytes, Some(3));
        let first = read(&dir, Some("one"), Some(0), 3).unwrap();
        assert_eq!(first.records[0].rx_seq, 1);
        let second = read(
            &dir,
            Some("one"),
            Some(first.continuation.unwrap().rx_seq),
            4,
        )
        .unwrap();
        assert_eq!(second.records[0].rx_seq, 2);
        assert!(!second.gap);
        assert!(read(&dir, None, Some(1), 4).is_err());
        assert!(read(&dir, None, None, 0).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restarts_epochs_generations_and_retention_are_explicit() {
        let dir = dir("epochs");
        let mut writer = Journal::open(dir.clone());
        writer.segment_bytes = 1;
        for seq in 1..=7 {
            writer.append(&record("one", seq, b"x")).unwrap();
        }
        let retained = read(&dir, Some("one"), Some(0), 100).unwrap();
        assert!(retained.gap);
        assert_eq!(retained.earliest.unwrap().rx_seq, 3);
        let mut writer = Journal::open(dir.clone());
        writer.start_epoch("two".into()).unwrap();
        let quiet = read(&dir, Some("two"), Some(0), 100).unwrap();
        assert!(!quiet.epoch_missing);
        assert!(!quiet.gap);
        let mut next = record("two", 1, b"new");
        next.connection_generation = 2;
        writer.append(&next).unwrap();
        let latest = read(&dir, None, None, 100).unwrap();
        assert_eq!(latest.epoch.as_deref(), Some("two"));
        assert_eq!(latest.records[0], next);
        assert!(
            read(&dir, Some("expired"), Some(0), 100)
                .unwrap()
                .epoch_missing
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corruption_retains_exact_fault_bytes_with_explicit_truncation() {
        let dir = dir("corrupt");
        let mut writer = Journal::open(dir.clone());
        writer.append(&record("one", 1, b"a")).unwrap();
        crate::capture::open_private_append(&dir.join(ACTIVE))
            .unwrap()
            .write_all(b"\xff\x00broken\n")
            .unwrap();
        let result = read(&dir, Some("one"), None, 100).unwrap();
        let fault = result
            .diagnostics
            .iter()
            .find(|d| d.kind == "corrupt_record")
            .unwrap();
        assert_eq!(fault.raw_bytes.as_ref().unwrap(), b"\xff\x00broken\n");
        assert!(!fault.raw_truncated);
        assert!(result.gap);
        let result = read(&dir, Some("one"), None, 2).unwrap();
        let fault = result
            .diagnostics
            .iter()
            .find(|d| d.kind == "corrupt_record")
            .unwrap();
        assert!(fault.raw_truncated);
        assert_eq!(fault.raw_length, 9);
        assert!(result.raw_bytes <= 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_fault_is_retained_and_pending_watermark_is_not_a_gap() {
        let dir = dir("failure");
        let mut writer = Journal::open(dir.clone());
        writer.start_epoch("one".into()).unwrap();
        writer.state.last_received = Some(Cursor {
            epoch: "one".into(),
            rx_seq: 1,
        });
        writer.save_state().unwrap();
        let pending = read(&dir, Some("one"), Some(1), 100).unwrap();
        assert!(pending.capture_pending);
        assert!(!pending.gap);
        fs::create_dir(dir.join(ACTIVE)).unwrap();
        writer.segment_bytes = u64::MAX;
        assert!(writer.append(&record("one", 1, b"\xff\x00fault")).is_err());
        let fault = read(&dir, Some("one"), None, 100).unwrap();
        assert!(fault.gap);
        assert!(fault.diagnostics.iter().any(|d| d.kind == "write_error"));
        let lost = fault
            .diagnostics
            .iter()
            .find(|d| d.kind == "failed_receive")
            .unwrap();
        assert_eq!(lost.raw_bytes.as_ref().unwrap(), b"\xff\x00fault");
        fs::remove_dir(dir.join(ACTIVE)).unwrap();
        let mut restarted = Journal::open(dir.clone());
        restarted.start_epoch("two".into()).unwrap();
        let old = read(&dir, Some("one"), None, 100).unwrap();
        assert!(old.diagnostics.iter().any(
            |d| d.kind == "failed_receive" && d.raw_bytes.as_deref() == Some(b"\xff\x00fault")
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_tail_waits_for_newline_and_persisted_checkpoint() {
        let dir = dir("concurrent");
        let mut writer = Journal::open(dir.clone());
        writer.append(&record("one", 1, b"prompt> ")).unwrap();
        let next = record("one", 2, b"reply\xff");
        writer.state.last_received = Some(next.cursor());
        writer.save_state().unwrap();
        let bytes = serde_json::to_vec(&next).unwrap();
        let midpoint = bytes.len() / 2;
        let mut active = crate::capture::open_private_append(&dir.join(ACTIVE)).unwrap();
        active.write_all(&bytes[..midpoint]).unwrap();
        let partial = read(&dir, Some("one"), Some(1), 4096).unwrap();
        assert!(!partial.gap);
        assert!(partial.capture_pending);
        assert_eq!(partial.continuation.unwrap().rx_seq, 1);
        assert!(partial.records.is_empty());
        active.write_all(&bytes[midpoint..]).unwrap();
        active.write_all(b"\n").unwrap();
        let uncheckpointed = read(&dir, Some("one"), Some(1), 4096).unwrap();
        assert!(!uncheckpointed.gap);
        assert!(uncheckpointed.capture_pending);
        assert!(uncheckpointed.records.is_empty());
        writer.state.persisted = Some(next.cursor());
        writer.save_state().unwrap();
        let published = read(&dir, Some("one"), Some(1), 4096).unwrap();
        assert_eq!(published.records, vec![next]);
        assert!(!published.capture_pending);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corruption_scan_and_epoch_metadata_are_bounded() {
        let dir = dir("bounds");
        fs::write(dir.join(ACTIVE), b"\xff\n".repeat(10000)).unwrap();
        let damaged = read(&dir, None, None, 8).unwrap();
        assert!(damaged.gap);
        assert!(damaged.raw_bytes <= 8);
        assert!(damaged.diagnostics.len() <= MAX_DIAGNOSTICS + 5);
        assert!(damaged
            .diagnostics
            .iter()
            .any(|d| d.kind == "diagnostic_limit"));
        assert!(serde_json::to_vec(&damaged).unwrap().len() < 1024 * 8 + 1048576);
        fs::remove_file(dir.join(ACTIVE)).unwrap();
        let mut writer = Journal::open(dir.clone());
        for epoch in 0..80 {
            writer
                .append(&record(&format!("epoch-{epoch}"), 1, b"x"))
                .unwrap();
        }
        let many = read(&dir, Some("epoch-0"), Some(0), 8).unwrap();
        assert!(many.epochs_truncated);
        assert_eq!(many.available_epochs.len(), 64);
        assert!(!many.epoch_missing);
        assert_eq!(many.records[0].epoch, "epoch-0");
        let path = select_dir(&dir, Some("../../escape")).unwrap();
        assert_eq!(path.parent(), Some(dir.as_path()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn journal_and_checkpoint_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("private");
        Journal::open(dir.clone())
            .append(&record("one", 1, b"x"))
            .unwrap();
        for name in [ACTIVE, STATE] {
            assert_eq!(
                fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
