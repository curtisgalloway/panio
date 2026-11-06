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

//! End-to-end: the built binary with shell commands that flip a state file.
//! Unix only (the commands use sh syntax).
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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
fn slow_matching_state_command_times_out_instead_of_reporting_success() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "usbtoggle-e2e-slow-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let pressed = dir.join("pressed");
    let opened = dir.join("opened");
    let state_cmd = format!(
        "if [ -f '{}' ]; then sleep 0.1; echo target; else echo host; fi",
        pressed.display()
    );
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args(["--state-cmd", &state_cmd])
        .args([
            "--close-cmd",
            &format!("echo close > '{}'", pressed.display()),
        ])
        .args(["--open-cmd", &format!("echo open > '{}'", opened.display())])
        .args(["--hold-ms", "0", "--settle-ms", "10", "usb", "target"])
        .output()
        .unwrap();
    assert!(!o.status.success(), "a late target result must not succeed");
    assert!(o.stdout.is_empty());
    assert!(String::from_utf8_lossy(&o.stderr).contains("timed out after 10 ms"));
    assert_eq!(fs::read_to_string(pressed).unwrap(), "close\n");
    assert_eq!(fs::read_to_string(opened).unwrap(), "open\n");
    fs::remove_dir_all(dir).unwrap();
}

fn unique_dir(name: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "usbtoggle-e2e-{name}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn usbtoggle(args: &[&str]) -> (Output, Duration) {
    let start = Instant::now();
    let o = Command::new(env!("CARGO_BIN_EXE_usbtoggle"))
        .args(args)
        .output()
        .unwrap();
    (o, start.elapsed())
}

#[test]
fn a_hung_state_command_fails_and_its_descendants_die() {
    let dir = unique_dir("hung-state");
    let pid_file = dir.join("pid");
    let state_cmd = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
    let (o, took) = usbtoggle(&[
        "--state-cmd",
        &state_cmd,
        "--command-timeout-ms",
        "200",
        "usb",
        "state",
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(o.stdout.is_empty());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("timed out after 200 ms"), "{err}");
    assert!(took < Duration::from_secs(5), "took {took:?}");

    let pid = fs::read_to_string(&pid_file).unwrap();
    let alive = Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!alive.success(), "the background sleep survived");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_hung_close_is_killed_and_open_still_runs_once() {
    let dir = unique_dir("hung-close");
    let opened = dir.join("opened");
    let open_cmd = format!("echo open >> '{}'", opened.display());
    let (o, took) = usbtoggle(&[
        "--state-cmd",
        "echo host",
        "--close-cmd",
        "sleep 30",
        "--open-cmd",
        &open_cmd,
        "--command-timeout-ms",
        "200",
        "usb",
        "target",
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(o.stdout.is_empty());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--close-cmd failed"), "{err}");
    assert!(err.contains("timed out after 200 ms"), "{err}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    assert_eq!(fs::read_to_string(opened).unwrap(), "open\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_hung_open_is_reported_as_possibly_held() {
    let (o, took) = usbtoggle(&[
        "--close-cmd",
        "true",
        "--open-cmd",
        "sleep 30",
        "--hold-ms",
        "0",
        "--command-timeout-ms",
        "200",
        "press",
    ]);
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("may be left held"), "{err}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn a_reading_hung_after_the_press_is_killed_at_the_settling_deadline() {
    let dir = unique_dir("hung-poll");
    let pressed = dir.join("pressed");
    let state_cmd = format!(
        "if [ -f '{}' ]; then sleep 30; else echo host; fi",
        pressed.display()
    );
    let close_cmd = format!("touch '{}'", pressed.display());
    let (o, took) = usbtoggle(&[
        "--state-cmd",
        &state_cmd,
        "--close-cmd",
        &close_cmd,
        "--open-cmd",
        "true",
        "--hold-ms",
        "0",
        "--settle-ms",
        "200",
        "usb",
        "target",
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(o.stdout.is_empty());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("timed out after 200 ms waiting"), "{err}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn something_left_holding_stdout_does_not_block_or_lose_the_answer() {
    let (o, took) = usbtoggle(&[
        "--state-cmd",
        "echo host; sleep 8 2>/dev/null &",
        "usb",
        "state",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "host\n");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn a_chatty_state_command_cannot_stall_on_a_full_pipe() {
    let (o, took) = usbtoggle(&["--state-cmd", "yes host | head -c 1000000", "usb", "state"]);
    assert_eq!(o.status.code(), Some(1), "that output is not a side");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn an_ignored_target_check_is_warned_about_and_does_not_change_the_answer() {
    let (o, _) = usbtoggle(&[
        "--state-cmd",
        "echo target",
        "--target-check-cmd",
        "exit 1",
        "usb",
        "state",
    ]);
    assert!(o.status.success());
    assert_eq!(String::from_utf8_lossy(&o.stdout), "target\n");
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--target-check-cmd is ignored"), "{err}");

    let (o, _) = usbtoggle(&["--state-cmd", "echo target", "usb", "state"]);
    assert!(
        o.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn a_zero_command_timeout_is_a_usage_error() {
    let (o, _) = usbtoggle(&[
        "--state-cmd",
        "echo host",
        "--command-timeout-ms",
        "0",
        "usb",
        "state",
    ]);
    assert_eq!(o.status.code(), Some(2));
}
