// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! Finite, read-only acquisition after an explicit receive cursor.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
pub const MAX_BYTES: u64 = 1024 * 1024;
pub const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
pub const MIN_OUTPUT_BYTES: u64 = 4096;
const FINISH_RESERVE: u64 = 1024;
const DIAGNOSTIC_BUDGET: usize = 512;

pub struct Request {
    pub epoch: String,
    pub since: u64,
    pub timeout_ms: u64,
    pub max_bytes: u64,
    pub max_output_bytes: u64,
    pub follow: bool,
    pub pattern: Option<String>,
    pub include_pending: bool,
}

#[derive(Clone)]
struct Acquired {
    records: Vec<Value>,
    bytes: Vec<u8>,
    pending: Vec<u8>,
    lines: Vec<Value>,
    prefix_unknown: bool,
    cursor: u64,
}

#[derive(Default)]
struct Diagnostics {
    values: Vec<Value>,
    count: usize,
    raw_length: usize,
    raw_bytes: usize,
    truncated: bool,
    raw_truncated: bool,
}

impl Diagnostics {
    fn push(&mut self, mut note: Value) {
        self.count += 1;
        self.truncated |= note["diagnostics_truncated"] == true;
        let raw_length = note["raw_length"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or_else(|| note["raw_bytes"].as_array().map_or(0, Vec::len));
        self.raw_length = self.raw_length.saturating_add(raw_length);
        let mut raw_bytes = note["raw_bytes"].as_array().map_or(0, Vec::len);
        self.raw_truncated |= note["raw_truncated"] == true || raw_bytes < raw_length;
        if encoded_len(&note) > DIAGNOSTIC_BUDGET as u64 / 2 {
            let kind: String = note["kind"]
                .as_str()
                .unwrap_or("diagnostic")
                .chars()
                .take(64)
                .collect();
            let message: String = note["message"]
                .as_str()
                .unwrap_or("diagnostic details truncated")
                .chars()
                .take(64)
                .collect();
            let raw: Vec<Value> = note["raw_bytes"]
                .as_array()
                .map(|b| b.iter().take(16).cloned().collect())
                .unwrap_or_default();
            raw_bytes = raw.len();
            note = json!({"kind":kind,"message":message,"raw_bytes":raw,
                "raw_length":raw_length,"raw_truncated":raw_bytes < raw_length,
                "diagnostics_truncated":true});
            self.truncated = true;
            self.raw_truncated |= raw_bytes < raw_length;
        }
        if encoded_len(&self.values) + encoded_len(&note) + 1 > DIAGNOSTIC_BUDGET as u64 {
            self.truncated = true;
            self.raw_truncated |= raw_length > 0;
        } else {
            self.raw_bytes += raw_bytes;
            self.values.push(note);
        }
    }
}

fn encoded_len(value: &impl serde::Serialize) -> u64 {
    serde_json::to_vec(value)
        .expect("JSON values serialize")
        .len() as u64
}

struct ResultMeta {
    started_at_ms: u64,
    finished_at_ms: u64,
    elapsed_ms: u64,
    next_record_bytes: Option<usize>,
    required_output_bytes: Option<u64>,
}

fn result(
    request: &Request,
    acquired: &Acquired,
    status: &str,
    notes: &Diagnostics,
    meta: &ResultMeta,
) -> Value {
    json!({
        "type":"result", "status":status, "epoch":request.epoch,
        "since":request.since, "through_rx_seq":acquired.cursor,
        "started_at_ms":meta.started_at_ms,"finished_at_ms":meta.finished_at_ms,
        "elapsed_ms":meta.elapsed_ms,
        "raw_bytes":acquired.bytes.len() + notes.raw_bytes,
        "raw_length":acquired.bytes.len().saturating_add(notes.raw_length),
        "received_bytes":acquired.bytes.len(),"records":acquired.records,
        "complete_lines":acquired.lines,
        "pending":if request.include_pending { Some(&acquired.pending) } else { None },
        "diagnostics":notes.values,"diagnostic_count":notes.count,
        "diagnostics_truncated":notes.truncated,"raw_truncated":notes.raw_truncated,
        "next_record_bytes":meta.next_record_bytes,"required_output_bytes":meta.required_output_bytes,
        "command_execution":"not_assessed"
    })
}

impl Acquired {
    fn new(since: u64) -> Self {
        Self {
            records: vec![],
            bytes: vec![],
            pending: vec![],
            lines: vec![],
            cursor: since,
            prefix_unknown: since > 0,
        }
    }

    fn append(&mut self, record: Value, emit: &mut impl FnMut(&Value) -> Result<()>) -> Result<()> {
        let raw = record
            .get("bytes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("receive record has no raw bytes"))?;
        for byte in raw {
            let byte = byte
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| anyhow!("invalid raw byte"))?;
            self.bytes.push(byte);
            self.pending.push(byte);
            if byte == b'\n' {
                let line = json!({"type": "line", "through_rx_seq": record["rx_seq"], "prefix_unknown": std::mem::replace(&mut self.prefix_unknown, false), "bytes": std::mem::take(&mut self.pending)});
                emit(&line)?;
                self.lines.push(line);
            }
        }
        self.cursor = record["rx_seq"]
            .as_u64()
            .ok_or_else(|| anyhow!("receive record has no cursor"))?;
        self.records.push(record);
        Ok(())
    }
}

/// Readers are injected so the same acquisition loop is tested against finite
/// fixtures and used with the journal and authenticated daemon status.
pub fn run(
    request: &Request,
    mut read: impl FnMut(u64, u64) -> Result<Value>,
    mut health: impl FnMut(Duration) -> Result<Value>,
    mut emit: impl FnMut(&Value) -> Result<()>,
) -> Result<Value> {
    if request.timeout_ms > MAX_TIMEOUT_MS
        || request.max_bytes == 0
        || request.max_bytes > MAX_BYTES
        || !(MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES).contains(&request.max_output_bytes)
    {
        return Err(anyhow!(
            "wait requires timeout <= {MAX_TIMEOUT_MS} ms, 1..={MAX_BYTES} raw bytes, and {MIN_OUTPUT_BYTES}..={MAX_OUTPUT_BYTES} output bytes"
        ));
    }
    if request.epoch.is_empty() || request.pattern.as_ref().is_some_and(String::is_empty) {
        return Err(anyhow!("epoch and a supplied pattern must be nonempty"));
    }
    let started_at_ms = crate::capture::now_ms();
    let start = Instant::now();
    let deadline = start + Duration::from_millis(request.timeout_ms);
    let mut acquired = Acquired::new(request.since);
    let mut diagnostics = Diagnostics::default();
    let mut pending_diagnostics = Vec::<Value>::new();
    let mut generation = None;
    let mut should_read = true;
    let mut emitted_bytes = 0;
    let mut meta = ResultMeta {
        started_at_ms,
        finished_at_ms: started_at_ms,
        elapsed_ms: u64::MAX,
        next_record_bytes: Some(usize::MAX),
        required_output_bytes: Some(u64::MAX),
    };
    if encoded_len(&result(
        request,
        &acquired,
        "daemon_unavailable",
        &Diagnostics::default(),
        &meta,
    )) + 1
        + FINISH_RESERVE
        > request.max_output_bytes
    {
        return Err(anyhow!(
            "receive boundary metadata exceeds the output byte budget"
        ));
    }
    let mut next_record_bytes = None;
    let mut required_output_bytes = None;
    let status = 'acquire: loop {
        let envelope = match if should_read {
            read(
                acquired.cursor,
                request.max_bytes - acquired.bytes.len() as u64,
            )
        } else {
            Ok(json!({"records":[]}))
        } {
            Ok(value) => value,
            Err(e) => {
                diagnostics.push(json!({"kind":"read_error", "message":e.to_string()}));
                break "read_error";
            }
        };
        if let Some(notes) = envelope["diagnostics"].as_array() {
            pending_diagnostics.clear();
            for note in notes {
                if matches!(
                    note["kind"].as_str(),
                    Some("incomplete_tail" | "capture_pending")
                ) {
                    pending_diagnostics.push(note.clone());
                } else {
                    diagnostics.push(note.clone());
                }
            }
        }
        if let Some(records) = envelope["records"].as_array() {
            for record in records {
                if request.timeout_ms > 0 && Instant::now() >= deadline {
                    break 'acquire "timeout";
                }
                let Some(seq) = record["rx_seq"].as_u64() else {
                    diagnostics
                        .push(json!({"kind":"read_error", "message":"invalid receive sequence"}));
                    break 'acquire "read_error";
                };
                if record["epoch"].as_str() != Some(request.epoch.as_str())
                    || seq <= acquired.cursor
                {
                    diagnostics.push(json!({"kind":"read_error", "message":"journal returned data outside the requested receive boundary"}));
                    break 'acquire "read_error";
                }
                let Some(raw) = record["bytes"].as_array() else {
                    diagnostics
                        .push(json!({"kind":"read_error", "message":"invalid receive bytes"}));
                    break 'acquire "read_error";
                };
                let raw_len = raw.len();
                if acquired.bytes.len() + raw_len > request.max_bytes as usize {
                    next_record_bytes = Some(raw_len);
                    break 'acquire "cap";
                }
                let mut candidate = acquired.clone();
                let mut events = Vec::new();
                let appended = candidate.append(record.clone(), &mut |line| {
                    events.push(line.clone());
                    Ok(())
                });
                if let Err(e) = appended {
                    diagnostics.push(json!({"kind":"read_error", "message":e.to_string()}));
                    break 'acquire "read_error";
                }
                let event_bytes: u64 = if request.follow {
                    events.iter().map(|event| encoded_len(event) + 1).sum()
                } else {
                    0
                };
                let required = emitted_bytes
                    + event_bytes
                    + 1
                    + FINISH_RESERVE
                    + encoded_len(&result(
                        request,
                        &candidate,
                        "daemon_unavailable",
                        &Diagnostics::default(),
                        &meta,
                    ));
                if required > request.max_output_bytes {
                    next_record_bytes = Some(raw_len);
                    required_output_bytes = Some(required);
                    break 'acquire "cap";
                }
                if request.follow {
                    for event in &events {
                        emit(event)?;
                    }
                }
                emitted_bytes += event_bytes;
                acquired = candidate;
            }
        }
        if envelope["gap"] == true {
            break "gap";
        }
        if envelope["epoch_missing"] == true {
            break "epoch_missing";
        }
        if diagnostics.count > 0 {
            break "capture_error";
        }
        // Check the process/connection even if acquired bytes contain the
        // pattern: an interrupted acquisition must remain explicit.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let state = match health(
            remaining
                .max(Duration::from_millis(1))
                .min(Duration::from_millis(250)),
        ) {
            Ok(value) => value,
            Err(e) => {
                diagnostics.push(json!({"kind":"daemon_unavailable", "message":e.to_string()}));
                break "daemon_unavailable";
            }
        };
        if state["health"]["epoch"].as_str() != Some(request.epoch.as_str()) {
            break if state["health"]["epoch"].is_null() {
                "unsupported"
            } else {
                "epoch_changed"
            };
        }
        if state["connected"] != true {
            break "disconnected";
        }
        if state["health"]["capture_alive"] == false || !state["health"]["evidence_error"].is_null()
        {
            diagnostics
                .push(json!({"kind":"capture_error", "message":state["health"]["evidence_error"]}));
            break "capture_error";
        }
        let current_generation = state["health"]["connection_generation"].as_u64();
        if acquired
            .records
            .iter()
            .any(|r| r["connection_generation"].as_u64() != current_generation)
        {
            break "reconnected";
        }
        if generation.is_some() && current_generation != generation {
            break "reconnected";
        }
        generation = current_generation;
        if request
            .pattern
            .as_ref()
            .is_some_and(|p| acquired.bytes.windows(p.len()).any(|w| w == p.as_bytes()))
        {
            break "matched";
        }
        if envelope["capped"] == true || acquired.bytes.len() as u64 == request.max_bytes {
            break "cap";
        }
        if Instant::now() >= deadline {
            break "timeout";
        }
        should_read = state["health"]["evidence_through_seq"]
            .as_u64()
            .is_none_or(|seq| seq > acquired.cursor);
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    };
    for note in pending_diagnostics {
        diagnostics.push(note);
    }
    meta.finished_at_ms = crate::capture::now_ms();
    meta.elapsed_ms = start.elapsed().as_millis() as u64;
    meta.next_record_bytes = next_record_bytes;
    meta.required_output_bytes = required_output_bytes;
    let output = result(request, &acquired, status, &diagnostics, &meta);
    if emitted_bytes + encoded_len(&output) + 1 > request.max_output_bytes {
        return Err(anyhow!("wait output byte budget accounting failed"));
    }
    Ok(output)
}

/// Read daemon status without pruning discovery files or opening serial ports.
fn interface_health(interface: &str, timeout: Duration) -> Result<Value> {
    use std::io::Read;
    let path = crate::daemon::runtime_dir()?.join("daemon.json");
    let discovery: crate::daemon::Discovery = serde_json::from_slice(&std::fs::read(path)?)?;
    let pid = i32::try_from(discovery.pid).map_err(|_| anyhow!("invalid daemon pid"))?;
    if !crate::platform::pid_alive(pid) {
        return Err(anyhow!("serialcap process is not live"));
    }
    let encoded: String = interface.bytes().map(|b| format!("%{b:02X}")).collect();
    let mut req = ureq::get(&format!(
        "http://127.0.0.1:{}/status?interface={encoded}",
        discovery.port
    ))
    .timeout(timeout);
    if let Some(token) = discovery.token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let mut bytes = Vec::new();
    req.call()?
        .into_reader()
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(anyhow!("serial status exceeds 64 KiB"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// One supported wait command; complete lines can be delivered as NDJSON.
pub fn cmd_wait(interface: String, request: Request) -> Result<()> {
    use std::io::Write;
    let dir = crate::capture::interface_dir(&crate::capture::capture_dir()?, &interface);
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let result = run(
        &request,
        |since, max_bytes| {
            Ok(serde_json::to_value(crate::evidence::read(
                &dir,
                Some(&request.epoch),
                Some(since),
                max_bytes,
            )?)?)
        },
        |timeout| interface_health(&interface, timeout),
        |line| {
            writeln!(output, "{line}")?;
            output.flush()?;
            Ok(())
        },
    )?;
    writeln!(output, "{result}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(pattern: Option<&str>, max_bytes: u64) -> Request {
        Request {
            epoch: "capture-a".into(),
            since: 10,
            timeout_ms: 0,
            max_bytes,
            max_output_bytes: MAX_OUTPUT_BYTES,
            follow: true,
            pattern: pattern.map(str::to_string),
            include_pending: true,
        }
    }
    fn healthy() -> Value {
        json!({"connected":true,"health":{"epoch":"capture-a","connection_generation":1,"capture_alive":true,"evidence_through_seq":12}})
    }
    fn record(seq: u64, bytes: &[u8]) -> Value {
        json!({"epoch":"capture-a","rx_seq":seq,"connection_generation":1,"rx_at_ms":123,"captured_at_ms":124,"bytes":bytes})
    }
    #[test]
    fn timeout_preserves_raw_faults_complete_lines_and_pending() {
        let mut emitted = vec![];
        let result = run(
            &request(Some("done"), 64),
            |since, cap| {
                assert_eq!((since, cap), (10, 64));
                Ok(json!({"records":[record(11,b"fault\xff\npartial")]}))
            },
            |_| Ok(healthy()),
            |line| {
                emitted.push(line.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result["status"], "timeout");
        assert_eq!(
            result["records"][0]["bytes"],
            json!(b"fault\xff\npartial".to_vec())
        );
        assert_eq!(emitted[0]["bytes"], json!(b"fault\xff\n".to_vec()));
        assert_eq!(result["pending"], json!(b"partial".to_vec()));
    }
    #[test]
    fn matching_across_immutable_chunks_uses_only_fresh_bytes() {
        let result = run(
            &request(Some("done"), 64),
            |_, _| Ok(json!({"records":[record(11,b"do"),record(12,b"ne")]})),
            |_| Ok(healthy()),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(result["status"], "matched");
        assert_eq!(result["through_rx_seq"], 12);
        let invalid = run(
            &request(Some("done"), 64),
            |_, _| Ok(json!({"records":[record(10,b"done")]})),
            |_| Ok(healthy()),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(invalid["status"], "read_error");
    }
    #[test]
    fn cap_and_failed_daemon_keep_acquired_records() {
        for expected in ["cap", "daemon_unavailable"] {
            let result = run(
                &request(None, 4),
                |_, _| Ok(json!({"records":[record(11,b"oops")],"capped":true})),
                |_| {
                    if expected == "cap" {
                        Ok(healthy())
                    } else {
                        Err(anyhow!("daemon gone"))
                    }
                },
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(result["status"], expected);
            assert_eq!(result["raw_bytes"], 4);
            assert_eq!(result["records"].as_array().unwrap().len(), 1);
        }
    }
    #[test]
    fn disconnect_epoch_and_gap_are_explicit() {
        for (state, envelope, expected) in [
            (
                json!({"connected":false,"health":{"epoch":"capture-a"}}),
                json!({"records":[]}),
                "disconnected",
            ),
            (
                json!({"connected":true,"health":{"epoch":"capture-b"}}),
                json!({"records":[]}),
                "epoch_changed",
            ),
            (
                healthy(),
                json!({"records":[record(12,b"late")],"gap":true}),
                "gap",
            ),
        ] {
            let result = run(
                &request(None, 64),
                |_, _| Ok(envelope.clone()),
                |_| Ok(state.clone()),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(result["status"], expected);
        }
    }

    #[test]
    fn actual_output_budget_counts_follow_events_and_preserves_cap_continuation() {
        let source: Vec<Value> = (11..211).map(|seq| record(seq, b"x\n")).collect();
        for follow in [false, true] {
            let mut req = request(None, 4096);
            req.max_output_bytes = MIN_OUTPUT_BYTES;
            req.follow = follow;
            let mut emitted = Vec::new();
            let first = run(
                &req,
                |_, _| Ok(json!({"records":source})),
                |_| Ok(healthy()),
                |event| {
                    emitted.push(event.clone());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(first["status"], "cap");
            let bytes =
                encoded_len(&first) + 1 + emitted.iter().map(|e| encoded_len(e) + 1).sum::<u64>();
            assert!(
                bytes <= req.max_output_bytes,
                "actual output {bytes}: {first}"
            );
            assert_eq!(first["next_record_bytes"], 2);
            assert!(first["required_output_bytes"].as_u64().unwrap() > req.max_output_bytes);
            let cursor = first["through_rx_seq"].as_u64().unwrap();
            assert!(cursor > req.since && cursor < 210);
            assert_eq!(
                first["records"].as_array().unwrap().len(),
                (cursor - req.since) as usize
            );
            assert_eq!(
                emitted.len(),
                if follow {
                    (cursor - req.since) as usize
                } else {
                    0
                }
            );

            req.since = cursor;
            req.max_output_bytes = MAX_OUTPUT_BYTES;
            let remainder = run(&req, |since, _| Ok(json!({"records":source.iter().filter(|r| r["rx_seq"].as_u64().unwrap() > since).collect::<Vec<_>>()})), |_| Ok(healthy()), |_| Ok(())).unwrap();
            assert_eq!(remainder["records"][0]["rx_seq"], cursor + 1);
            assert_eq!(remainder["through_rx_seq"], 210);
            assert_eq!(
                first["records"].as_array().unwrap().len()
                    + remainder["records"].as_array().unwrap().len(),
                source.len()
            );
        }
    }

    #[test]
    fn huge_fault_diagnostics_report_source_lengths_and_explicit_truncation() {
        let mut req = request(None, 4096);
        req.max_output_bytes = MIN_OUTPUT_BYTES;
        let raw_fault = vec![255_u8; 40_000];
        let mut emitted_bytes = 0;
        let output = run(&req, |_, _| Ok(json!({"records":[record(11,b"fault\xff\n")],"diagnostics":[
            {"kind":"malformed_record","message":"bad \"record\" 😀".repeat(20_000),"raw_bytes":raw_fault},
            {"kind":"capture_pending","message":"pending"}
        ]})), |_| Ok(healthy()), |event| { emitted_bytes += encoded_len(event) + 1; Ok(()) }).unwrap();
        assert_eq!(output["status"], "capture_error");
        assert_eq!(
            output["records"][0]["bytes"],
            json!(b"fault\xff\n".to_vec())
        );
        assert_eq!(output["raw_length"], 40_007);
        assert_eq!(output["diagnostics_truncated"], true);
        assert_eq!(output["raw_truncated"], true);
        assert_eq!(output["diagnostics"][0]["raw_length"], 40_000);
        assert_eq!(output["diagnostics"][0]["raw_truncated"], true);
        assert!(encoded_len(&output) + 1 + emitted_bytes <= req.max_output_bytes);
    }

    #[test]
    fn benign_pending_diagnostics_do_not_end_acquisition_or_discard_invalid_utf8() {
        let output = run(&request(Some("done"), 64), |_, _| Ok(json!({"records":[record(11,b"oops\xff\npartial")],"diagnostics":[{"kind":"incomplete_tail","raw_bytes":[255],"raw_length":1}]})), |_| Ok(healthy()), |_| Ok(())).unwrap();
        assert_eq!(output["status"], "timeout");
        assert_eq!(
            output["records"][0]["bytes"],
            json!(b"oops\xff\npartial".to_vec())
        );
        assert_eq!(output["diagnostics"][0]["raw_bytes"], json!([255]));
        assert_eq!(output["raw_truncated"], false);
    }

    #[test]
    fn failure_after_an_accepted_record_retains_its_raw_bytes() {
        let output = run(
            &request(None, 64),
            |_, _| {
                Ok(json!({"records":[record(11,b"fault\xff"),record(12,b"bad")],"diagnostics":[]}))
            },
            |_| Err(anyhow!("unavailable")),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(output["status"], "daemon_unavailable");
        assert_eq!(output["records"][0]["bytes"], json!(b"fault\xff".to_vec()));
        assert_eq!(output["through_rx_seq"], 12);
    }
}
