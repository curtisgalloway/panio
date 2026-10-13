// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

struct Fixture {
    dir: PathBuf,
    stopped: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(label: &str, records: &[Value], stop_after_first: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("serialcap-wait-{label}-{}", std::process::id()));
        let capture = dir.join("capture/console");
        std::fs::create_dir_all(&capture).unwrap();
        let mut journal = String::new();
        for record in records {
            journal.push_str(&record.to_string());
            journal.push('\n');
        }
        std::fs::write(capture.join("evidence.jsonl"), journal).unwrap();
        let last = records.last().map_or(0, |r| r["rx_seq"].as_u64().unwrap());
        std::fs::write(capture.join("evidence-state.json"), json!({"last_received":{"epoch":"capture-a","rx_seq":last},"persisted":{"epoch":"capture-a","rx_seq":last},"error":null}).to_string()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::write(dir.join("daemon.json"), json!({"pid":std::process::id(),"port":listener.local_addr().unwrap().port(),"token":"test-token","interfaces":[]}).to_string()).unwrap();
        let body = json!({"name":"console","connected":true,"health":{"epoch":"capture-a","connection_generation":1,"last_rx_seq":last,"evidence_through_seq":last,"evidence_error":null,"capture_alive":true}}).to_string();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopping = stopped.clone();
        let server = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut byte = [0u8; 1];
                        while !request.ends_with(b"\r\n\r\n") {
                            socket.read_exact(&mut byte).unwrap();
                            request.push(byte[0]);
                        }
                        let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                        assert!(request.starts_with("get /status?interface="));
                        assert!(request.contains("authorization: bearer test-token"));
                        write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                        if stop_after_first {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("health fixture listener: {e}"),
                }
            }
        });
        Self {
            dir,
            stopped,
            server: Some(server),
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_serialcap"))
            .args([
                "wait",
                "--interface",
                "console",
                "--epoch",
                "capture-a",
                "--since",
                "0",
                "--timeout-ms",
                "150",
                "--max-output-bytes",
                "4096",
            ])
            .args(args)
            .env("PANIOLO_RUNTIME_DIR", &self.dir)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn record(seq: u64, bytes: &[u8]) -> Value {
    json!({"epoch":"capture-a","rx_seq":seq,"connection_generation":1,"rx_at_ms":123+seq,"captured_at_ms":124+seq,"bytes":bytes})
}
fn result(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    let text = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn timeout_follow_retains_fresh_raw_faults_and_pending() {
    let fixture = Fixture::new(
        "fresh",
        &[
            record(1, b"old success\n"),
            record(2, b"fault\xff\npartial"),
        ],
        false,
    );
    let before = std::fs::read(fixture.dir.join("daemon.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_serialcap"))
        .args([
            "wait",
            "--interface",
            "console",
            "--epoch",
            "capture-a",
            "--since",
            "1",
            "--timeout-ms",
            "150",
            "--max-bytes",
            "64",
            "--max-output-bytes",
            "4096",
            "--pattern",
            "success",
            "--include-pending",
            "--follow",
        ])
        .env("PANIOLO_RUNTIME_DIR", &fixture.dir)
        .output()
        .unwrap();
    let final_result = result(&output);
    assert_eq!(final_result["status"], "timeout");
    assert_eq!(final_result["records"].as_array().unwrap().len(), 1);
    assert_eq!(
        final_result["records"][0]["bytes"],
        json!(b"fault\xff\npartial".to_vec())
    );
    assert_eq!(final_result["pending"], json!(b"partial".to_vec()));
    let line: Value = serde_json::from_str(
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(line["type"], "line");
    assert_eq!(line["prefix_unknown"], true);
    assert_eq!(line["bytes"], json!(b"fault\xff\n".to_vec()));
    assert!(output.stdout.len() <= 4096);
    assert_eq!(
        std::fs::read(fixture.dir.join("daemon.json")).unwrap(),
        before
    );
}

#[test]
fn output_cap_never_skips_omitted_record() {
    let records: Vec<_> = (1..=100).map(|seq| record(seq, b"x\n")).collect();
    let fixture = Fixture::new("cap", &records, false);
    let output = fixture.run(&["--follow"]);
    let final_result = result(&output);
    assert_eq!(final_result["status"], "cap");
    let accepted = final_result["records"].as_array().unwrap();
    assert!(!accepted.is_empty());
    assert!(accepted.len() < 100);
    assert_eq!(final_result["through_rx_seq"], accepted.len() as u64);
    assert!(output.stdout.len() <= 4096, "{}", output.stdout.len());
}

#[test]
fn daemon_failure_keeps_already_acquired_evidence() {
    let fixture = Fixture::new("dead", &[record(1, b"fault\xff")], true);
    let output = fixture.run(&["--include-pending"]);
    let final_result = result(&output);
    assert_eq!(final_result["status"], "daemon_unavailable");
    assert_eq!(
        final_result["records"][0]["bytes"],
        json!(b"fault\xff".to_vec())
    );
    assert_eq!(final_result["pending"], json!(b"fault\xff".to_vec()));
}
