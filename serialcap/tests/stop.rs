// Copyright 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

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
    let dir =
        std::env::temp_dir().join(format!("serialcap-stop-integration-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No hardware: a missing device lets the daemon exercise its normal
    // reconnect loop while the HTTP lifecycle remains fully functional.
    let child = Command::new(env!("CARGO_BIN_EXE_serialcap"))
        .args(["daemon", "--port", "0", "--interface"])
        .arg(format!("console={}", dir.join("missing-device").display()))
        .env("PANIO_RUNTIME_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
    let result = Command::new(env!("CARGO_BIN_EXE_serialcap"))
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
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!discovery.exists());
}
