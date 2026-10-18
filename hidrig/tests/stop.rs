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

//! `hidrig stop` shuts the daemon down through its authenticated `POST /stop`
//! and never signals the PID in the discovery file (found by the sweep for
//! issue #147). This drives a real daemon and a real `stop` subprocess: the
//! daemon exits with status 0 and removes its discovery file, exactly as the
//! SIGTERM path did.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn authenticated_stop_exits_and_removes_discovery() {
    let dir = std::env::temp_dir().join(format!("hidrig-stop-integration-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No hardware: the control-board UART is opened lazily on the first
    // command, so a device nothing matches keeps the daemon fully functional
    // over HTTP without ever touching a port. The DUT console PTY bridge is
    // real (a local pty pair), not hardware, so it comes up normally.
    let child = Command::new(env!("CARGO_BIN_EXE_hidrig"))
        .args([
            "--device",
            "/dev/no-such-hidrig-device-zz",
            "serve",
            "--port",
            "0",
        ])
        .env("PANIO_RUNTIME_DIR", &dir)
        // The daemon logs which path stopped it on stderr (where the panio CLI
        // collects daemon.log); captured to prove below that the stop arrived over
        // HTTP rather than as a signal.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, dir };
    let discovery = daemon.dir.join("daemon.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !discovery.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon did not publish discovery"
        );
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited early"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = Command::new(env!("CARGO_BIN_EXE_hidrig"))
        .arg("stop")
        .env("PANIO_RUNTIME_DIR", &daemon.dir)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            assert!(status.success(), "daemon exit status {status}");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!discovery.exists(), "discovery file survived the stop");
    let mut log = String::new();
    std::io::Read::read_to_string(daemon.child.stderr.as_mut().unwrap(), &mut log).unwrap();
    assert!(
        log.contains("stop requested over HTTP"),
        "the daemon was not stopped through POST /stop:\n{log}"
    );
    assert!(
        !log.contains("shutdown signal received"),
        "the daemon was signaled instead of asked:\n{log}"
    );
}
