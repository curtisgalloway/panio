// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;

// Model a helper that accepts stop immediately, while the daemon spends time
// releasing input and leaves a discovery file behind. Reap concurrently so the
// CLI can observe process exit even on systems where zombies remain alive.
#[test]
fn hid_stop_waits_for_each_helper_and_cleans_discovery() {
    for helper in ["ch9329", "hidrig", "jetkvm"] {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let uid = dir.path().metadata().unwrap().uid();
        let base = run.join(format!("panio-{uid}"));
        let runtime = base.join("hid/dut");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = dir.path().join(helper);
        std::fs::write(
            &script,
            "if [ \"$1\" = stop ]; then\n  touch \"$PANIO_RUNTIME_DIR/stopping\"\n  exit 0\nfi\nwhile [ ! -f \"$PANIO_RUNTIME_DIR/stopping\" ]; do sleep 0.05; done\nsleep 0.5\ntouch \"$PANIO_RUNTIME_DIR/released\"\n",
        )
        .unwrap();
        let mut daemon = Command::new("sh")
            .arg(&script)
            .env("PANIO_RUNTIME_DIR", &runtime)
            .spawn()
            .unwrap();
        let discovery = runtime.join("daemon.json");
        std::fs::write(
            &discovery,
            format!("{{\"pid\":{},\"port\":12345}}", daemon.id()),
        )
        .unwrap();
        std::fs::set_permissions(&discovery, std::fs::Permissions::from_mode(0o600)).unwrap();
        let reaper = std::thread::spawn(move || daemon.wait().unwrap());
        let lab = dir.path().join("lab.toml");
        std::fs::write(
            &lab,
            format!("[targets.dut.hid]\ncmd = \"sh '{}'\"\n", script.display()),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_panio"))
            .args(["--lab", lab.to_str().unwrap(), "hid", "stop", "dut"])
            .env("PANIO_RUNTIME_BASE", &run)
            .env_remove("PANIO_LAB")
            .output()
            .unwrap();
        let released_before_return = runtime.join("released").exists();
        let discovery_removed = !discovery.exists();
        // Ensure the test daemon exits even if the CLI failed before invoking stop.
        std::fs::write(runtime.join("stopping"), "").unwrap();
        reaper.join().unwrap();
        assert!(output.status.success(), "{helper}: {output:?}");
        assert!(
            released_before_return,
            "{helper}: stop returned during cleanup"
        );
        assert!(discovery_removed, "{helper}: stale discovery survived stop");
    }
}
