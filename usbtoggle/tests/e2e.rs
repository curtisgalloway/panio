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

//! End-to-end: the built binary with shell commands that flip a state file.
//! Unix only (the commands use sh syntax).
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn flip_script(dir: &Path) -> (String, String, String) {
    let state = dir.join("state");
    let log = dir.join("log");
    let st = state.display();
    let state_cmd = format!("cat '{st}'");
    let close_cmd = format!("echo close >> '{}'", log.display());
    let open_cmd = format!(
        "echo open >> '{log}'; if [ \"$(cat '{st}')\" = host ]; then echo target > '{st}'; \
         else echo host > '{st}'; fi",
        log = log.display()
    );
    (state_cmd, close_cmd, open_cmd)
}

fn run(dir: &Path, extra: &[&str]) -> Output {
    let (state_cmd, close_cmd, open_cmd) = flip_script(dir);
    Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args(["--state-cmd", &state_cmd])
        .args(["--close-cmd", &close_cmd, "--open-cmd", &open_cmd])
        .args(["--hold-ms", "1", "--settle-ms", "500"])
        .args(extra)
        .output()
        .unwrap()
}

fn log_lines(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn target_then_host_round_trips_and_noop_does_not_press() {
    let dir = std::env::temp_dir().join(format!("usbtoggle-e2e-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("state"), "host\n").unwrap();

    let o = run(&dir, &["usb", "target"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "target\n");
    assert_eq!(log_lines(&dir), ["close", "open"]);

    let o = run(&dir, &["usb", "target"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "target\n");
    assert_eq!(log_lines(&dir).len(), 2, "already there: no press");

    let o = run(&dir, &["usb", "host"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "host\n");
    assert_eq!(log_lines(&dir), ["close", "open", "close", "open"]);

    let o = run(&dir, &["usb", "state"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "host\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_switch_that_never_moves_fails_after_one_press_with_empty_stdout() {
    let dir = std::env::temp_dir().join(format!("usbtoggle-e2e-stuck-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("state"), "host\n").unwrap();
    let log = dir.join("log");
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args([
            "--state-cmd",
            &format!("cat '{}'", dir.join("state").display()),
        ])
        .args(["--close-cmd", &format!("echo close >> '{}'", log.display())])
        .args(["--open-cmd", "true", "--hold-ms", "1", "--settle-ms", "200"])
        .args(["usb", "target"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(o.stdout.is_empty(), "stdout must be empty on failure");
    assert!(String::from_utf8_lossy(&o.stderr).contains("timed out"));
    assert_eq!(log_lines(&dir), ["close"], "exactly one press");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn relay_command_stdout_goes_to_stderr_not_stdout() {
    let dir = std::env::temp_dir().join(format!("usbtoggle-e2e-out-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("state"), "host\n").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args([
            "--state-cmd",
            &format!("cat '{}'", dir.join("state").display()),
        ])
        .args(["--close-cmd", "echo NOISE-FROM-RELAY", "--open-cmd", "true"])
        .args(["--hold-ms", "1", "press"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(o.stdout.is_empty(), "{:?}", o.stdout);
    assert!(String::from_utf8_lossy(&o.stderr).contains("NOISE-FROM-RELAY"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn usb_verbs_require_a_position_source() {
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args(["usb", "state"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(o.stdout.is_empty());
}

#[test]
fn device_and_state_cmd_are_mutually_exclusive() {
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args([
            "--device",
            "046d:c08b",
            "--state-cmd",
            "echo host",
            "usb",
            "state",
        ])
        .output()
        .unwrap();
    assert!(!o.status.success());
}

#[test]
fn doctor_json_exits_zero_and_has_checks() {
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args(["--state-cmd", "echo banana", "doctor", "--json"])
        .output()
        .unwrap();
    assert!(o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.starts_with("{\"checks\": ["), "{out}");
    assert!(out.contains("\"status\": \"fail\""), "{out}");
}

#[test]
fn relay_failures_never_print_a_success_side() {
    for (label, close_status, open_status) in [("close", 3, 0), ("open", 0, 4), ("both", 3, 4)] {
        let dir = std::env::temp_dir().join(format!(
            "usbtoggle-e2e-errors-{label}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
            .args(["--state-cmd", "echo host"])
            .args([
                "--close-cmd",
                &format!("echo close >> '{}'; exit {close_status}", log.display()),
            ])
            .args([
                "--open-cmd",
                &format!("echo open >> '{}'; exit {open_status}", log.display()),
            ])
            .args(["--hold-ms", "0", "usb", "target"])
            .output()
            .unwrap();
        assert!(!o.status.success(), "{label}");
        assert!(o.stdout.is_empty(), "{label}");
        assert_eq!(log_lines(&dir), ["close", "open"]);
        let err = String::from_utf8_lossy(&o.stderr);
        if close_status != 0 {
            assert!(err.contains("--close-cmd failed"), "{err}");
        }
        if open_status != 0 {
            assert!(err.contains("open-cmd failed"), "{err}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn state_command_target_bypasses_a_failing_target_check() {
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args(["--state-cmd", "echo target", "--target-check-cmd", "exit 1"])
        .args(["--close-cmd", "exit 3", "--open-cmd", "exit 4"])
        .args(["usb", "target"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert_eq!(o.stdout, b"target\n");
    assert!(o.stderr.is_empty());
}
