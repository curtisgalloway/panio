// SPDX-FileCopyrightText: 2026 The Panio Authors
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SPLIT_LAB: &str = r#"
[hosts.bench1]
ssh = "bench1"

[hosts.bench2]
ssh = "bench2"

[targets.dut]
host = "bench1"

[targets.dut.power]
state_cmd = "echo off"
on_cmd = "touch power-ran"

[targets.dut.actions]
host = "bench2"

[targets.dut.actions.strap]
cmd = "exec /bin/sh action.sh"
requires_power = "off"
"#;

const POWER_LAB: &str = r#"
[targets.dut.power]
state_cmd = "echo off"
on_cmd = "touch power-ran"
"#;

const ACTION_LAB: &str = r#"
[targets.dut.actions.strap]
cmd = "exec /bin/sh action.sh"
requires_power = "off"
"#;

// The SSH shim runs the real remote command with an isolated home and runtime.
// In the interruption test it ends its own remote panio subprocess after the
// hook starts. The hook keeps running with its SSH output pipes closed.
const SSH: &str = r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "$#" -eq 3 ] || exit 255
shift
case "$1" in bench1|bench2|bench3) ;; *) exit 255 ;; esac
export HOME="$PANIO_GUARD_TEST_ROOT/$1"
export PANIO_RUNTIME_BASE="$HOME/run" XDG_RUNTIME_DIR="$HOME/run"
cd "$HOME" || exit 255
shift
case "${PANIO_GUARD_TEST_INTERRUPT:-}:$1" in
    partial-release:*guard-step*'{"cancel":'*)
        if ! mkdir "$PANIO_GUARD_TEST_ROOT/first-cancel" 2>/dev/null; then exit 255; fi ;;
    bad-check:*guard-step*'{"check":'*)
        /bin/sh -c "$1" | sed 's/"required":"off"/"required":"on"/'
        exit $? ;;
    empty-success:*guard-step*'{"execute":'*) exit 0 ;;
    1:*guard-step*'{"execute":'*|1:*'action run '*--require-power-lock=*)
        /bin/sh -c "exec $1" &
        remote=$!
        tries=0
        until [ -f started ] || [ "$tries" -ge 500 ]; do
            /bin/sleep 0.02
            tries=$((tries + 1))
        done
        kill -KILL "$remote" 2>/dev/null
        wait "$remote" 2>/dev/null
        exit 255
        ;;
esac
exec /bin/sh -c "$1"
"#;

// Only the quoted put/rm forms Panio sends to the SFTP batch are understood.
// The destination is relative to the same home used by the SSH shim.
const SFTP: &str = r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "$#" -eq 2 ] || exit 1
shift
case "$1" in bench1|bench2|bench3) ;; *) exit 1 ;; esac
cd "$PANIO_GUARD_TEST_ROOT/$1" || exit 1
IFS='"' read -r op source padding destination suffix || exit 1
case "$op" in
    'put ') exec /bin/cp "$source" "$destination" ;;
    'rm ') exec /bin/rm -f -- "$source" ;;
esac
exit 1
"#;

const HOOK: &str = r#"
if [ "${PANIO_GUARD_TEST_INTERRUPT:-}" = 1 ]; then
    exec >/dev/null 2>&1
fi
printf '%s\n' ran >> hook-runs
touch started
tries=0
until [ -f finish ] || [ "$tries" -ge 750 ]; do
    /bin/sleep 0.02
    tries=$((tries + 1))
done
[ -f finish ] || exit 99
echo hook-output
touch stopped
"#;

struct Lab {
    root: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        let lab = Self {
            root: tempfile::tempdir().unwrap(),
        };
        for host in ["driver", "bench1", "bench2", "bench3"] {
            fs::create_dir_all(lab.at(host).join("run")).unwrap();
        }
        fs::create_dir(lab.at("bin")).unwrap();
        symlink(env!("CARGO_BIN_EXE_panio"), lab.at("bin/panio")).unwrap();
        for (name, script) in [("bin/ssh", SSH), ("bin/sftp", SFTP)] {
            fs::write(lab.at(name), script).unwrap();
            fs::set_permissions(lab.at(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(lab.at("driver/lab.toml"), SPLIT_LAB).unwrap();
        fs::write(
            lab.at("driver/wrong.toml"),
            SPLIT_LAB.replacen(r#"ssh = "bench1""#, r#"ssh = "bench3""#, 1),
        )
        .unwrap();
        fs::write(lab.at("bench1/lab.toml"), POWER_LAB).unwrap();
        fs::write(lab.at("bench2/lab.toml"), ACTION_LAB).unwrap();
        fs::write(lab.at("bench2/action.sh"), HOOK).unwrap();
        lab
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.path().join(path)
    }

    fn command(&self, host: &str, lab: &str, args: &[&str]) -> Command {
        let home = self.at(host);
        let run = home.join("run");
        let mut command = Command::new(env!("CARGO_BIN_EXE_panio"));
        command
            .current_dir(&home)
            .args(["--lab", lab])
            .args(args)
            .env("HOME", home)
            .env("XDG_RUNTIME_DIR", &run)
            .env("PANIO_RUNTIME_BASE", &run)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.at("bin").display()),
            )
            .env("PANIO_GUARD_TEST_ROOT", self.root.path())
            .env_remove("PANIO_GUARD_TEST_INTERRUPT")
            .env_remove("PANIO_PLUGIN_PATH")
            .env_remove("PANIO_HOOK_LOCK")
            .env_remove("PANIO_JSON_ERRORS")
            .env_remove("PANIO_LEASE")
            .env_remove("AMT_PASSWORD")
            .stdin(Stdio::null());
        command
    }

    fn start(&self, interrupt: bool) -> Child {
        let mut command = self.command(
            "driver",
            "lab.toml",
            &["action", "run", "-t", "dut", "strap"],
        );
        if interrupt {
            command.env("PANIO_GUARD_TEST_INTERRUPT", "1");
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn power(&self) -> Output {
        self.command("bench1", "lab.toml", &["power", "on", "dut"])
            .output()
            .unwrap()
    }

    fn status(&self) -> Value {
        let out = self
            .command(
                "driver",
                "lab.toml",
                &["action", "guard-status", "-t", "dut"],
            )
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn recover(&self, lab: &str, id: &str, confirm: bool) -> Output {
        let mut command = self.command(
            "driver",
            lab,
            &["action", "recover", "-t", "dut", "--id", id],
        );
        if confirm {
            command.arg("--confirm-stopped");
        }
        command.output().unwrap()
    }

    fn guard_step(&self, host: &str, request: Value) -> Output {
        self.command(
            host,
            "lab.toml",
            &["action", "guard-step", "-t", "dut", &request.to_string()],
        )
        .output()
        .unwrap()
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !path.is_file() {
        assert!(Instant::now() < deadline, "missing fixture marker {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn refused(out: &Output, message: &str) {
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(message),
        "{out:?}"
    );
}

#[test]
fn split_action_reserves_power_across_independent_processes() {
    let lab = Lab::new();
    let child = lab.start(false);
    wait_for(&lab.at("bench2/started"));
    let during = lab.power();
    let ran_early = lab.at("bench1/power-ran").exists();
    fs::write(lab.at("bench2/finish"), "").unwrap();
    let action = child.wait_with_output().unwrap();

    refused(&during, "power is reserved");
    assert!(!ran_early);
    assert!(action.status.success(), "{action:?}");
    assert!(
        String::from_utf8_lossy(&action.stdout).contains("hook-output"),
        "{action:?}"
    );
    assert_eq!(lab.status(), Value::Null);
    let after = lab.power();
    assert!(after.status.success(), "{after:?}");
    assert!(lab.at("bench1/power-ran").exists());
}

#[test]
fn action_rejects_a_power_check_for_a_different_state() {
    let lab = Lab::new();
    fs::write(lab.at("bench2/finish"), "").unwrap();
    let out = lab
        .command(
            "driver",
            "lab.toml",
            &["action", "run", "-t", "dut", "strap"],
        )
        .env("PANIO_GUARD_TEST_INTERRUPT", "bad-check")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("power-state reply does not match"),
        "{out:?}"
    );
    assert!(!lab.at("bench2/hook-runs").exists());
    assert_eq!(lab.status(), Value::Null);
    assert!(lab.power().status.success());
}

#[test]
fn signaled_action_keeps_power_reserved_until_confirmed_stopped() {
    let lab = Lab::new();
    fs::write(lab.at("bench2/action.sh"), "kill -TERM $$\n").unwrap();
    let out = lab.start(false).wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let receipt = lab.status();
    assert_eq!(receipt["phase"], "running");
    refused(&lab.power(), "power is reserved");
    let recovery = lab.recover("lab.toml", receipt["id"].as_str().unwrap(), true);
    assert!(recovery.status.success(), "{recovery:?}");
    assert!(lab.power().status.success());
}

#[test]
fn empty_success_from_transport_does_not_release_an_unfinished_action() {
    let lab = Lab::new();
    let out = lab
        .command(
            "driver",
            "lab.toml",
            &["action", "run", "-t", "dut", "strap"],
        )
        .env("PANIO_GUARD_TEST_INTERRUPT", "empty-success")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not record completion"));
    let receipt = lab.status();
    assert_eq!(receipt["phase"], "ready");
    refused(&lab.power(), "power is reserved");
    let recovery = lab.recover("lab.toml", receipt["id"].as_str().unwrap(), false);
    assert!(recovery.status.success(), "{recovery:?}");
    assert!(lab.power().status.success());
    assert!(!lab.at("bench2/hook-runs").exists());
}

fn configure_button_host(lab: &Lab) {
    let button = r#"
[[targets.dut.serial]]
name = "button"
device = "/dev/ttyUSB-test"
power_button = true
"#;
    fs::write(lab.at("bench3/lab.toml"), button).unwrap();
    let full = format!("{SPLIT_LAB}\n[hosts.bench3]\nssh = 'bench3'\n{button}host = 'bench3'\n");
    fs::write(lab.at("driver/lab.toml"), full).unwrap();
}

#[test]
fn interrupted_release_keeps_the_finished_receipt_until_every_host_acknowledges() {
    let lab = Lab::new();
    configure_button_host(&lab);
    fs::write(lab.at("bench2/finish"), "").unwrap();
    let out = lab
        .command(
            "driver",
            "lab.toml",
            &["action", "run", "-t", "dut", "strap"],
        )
        .env("PANIO_GUARD_TEST_INTERRUPT", "partial-release")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let ticket = lab.status();
    assert_eq!(ticket["phase"], "finished");
    let id = ticket["id"].as_str().unwrap();
    let holders = || {
        ["bench1", "bench3"].map(|host| {
            let bytes = fs::read(
                lab.at(host)
                    .join(".local/share/panio/dut/power-reservation.json"),
            )
            .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["holder"].clone()
        })
    };
    let held = holders();
    assert!(
        held.contains(&json!(id)) && held.contains(&Value::Null),
        "{held:?}"
    );
    let out = lab.recover("lab.toml", id, false);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(lab.status(), Value::Null);
    assert_eq!(holders(), [Value::Null, Value::Null]);
    assert_eq!(
        fs::read_to_string(lab.at("bench2/hook-runs")).unwrap(),
        "ran\n"
    );
}

#[test]
fn split_guard_also_reserves_a_separate_serial_button_host() {
    let lab = Lab::new();
    configure_button_host(&lab);
    let child = lab.start(false);
    wait_for(&lab.at("bench2/started"));
    let receipt = lab.status();
    let pulse = lab
        .command("bench3", "lab.toml", &["serial", "dtr", "dut"])
        .output()
        .unwrap();
    fs::write(lab.at("bench2/finish"), "").unwrap();
    let action = child.wait_with_output().unwrap();

    assert!(receipt["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|peer| peer["host"] == "bench3"));
    refused(&pulse, "power is reserved");
    assert!(action.status.success(), "{action:?}");
    assert_eq!(lab.status(), Value::Null);
}

#[test]
fn interrupted_split_action_requires_confirmation_and_fences_late_requests() {
    let lab = Lab::new();
    let interrupted = lab.start(true).wait_with_output().unwrap();
    assert!(!interrupted.status.success(), "{interrupted:?}");
    assert!(
        String::from_utf8_lossy(&interrupted.stderr).contains("--confirm-stopped"),
        "{interrupted:?}"
    );
    let receipt = lab.status();
    assert_eq!(receipt["phase"], "running");
    assert_eq!(receipt["hosts"][0]["host"], "bench1");
    let id = receipt["id"].as_str().unwrap();
    let peer = &receipt["hosts"][0];

    assert!(!lab.at("bench2/stopped").exists());
    refused(&lab.recover("lab.toml", id, false), "--confirm-stopped");
    refused(&lab.power(), "power is reserved");
    assert!(!lab.at("bench1/power-ran").exists());
    fs::write(lab.at("bench2/finish"), "").unwrap();
    wait_for(&lab.at("bench2/stopped"));

    refused(
        &lab.recover("wrong.toml", id, true),
        "power host no longer matches",
    );
    refused(&lab.power(), "power is reserved");
    let recovery = lab.recover("lab.toml", id, true);
    assert!(recovery.status.success(), "{recovery:?}");
    assert_eq!(lab.status(), Value::Null);
    refused(
        &lab.guard_step("bench2", json!({"execute": {"checked": {"id": id, "peer": peer, "required": "off", "action": "strap"}}})),
        "receipt is missing",
    );
    refused(
        &lab.guard_step("bench1", json!({"reserve": {"id": id, "peer": peer}})),
        "generation",
    );
    assert_eq!(
        fs::read_to_string(lab.at("bench2/hook-runs")).unwrap(),
        "ran\n"
    );
    let after = lab.power();
    assert!(after.status.success(), "{after:?}");
    assert!(lab.at("bench1/power-ran").exists());
}

#[test]
fn split_action_streams_stdio_and_preserves_helper_error() {
    let lab = Lab::new();
    fs::write(
        lab.at("bench2/action.sh"),
        r#"printf 'waiting for input\n'
IFS= read -r payload || exit 97
printf 'received %s\n' "$payload"
printf 'helper diagnostic\n' >&2
exit 7
"#,
    )
    .unwrap();
    let mut child = lab
        .command(
            "driver",
            "lab.toml",
            &["--json-errors", "action", "run", "-t", "dut", "strap"],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let (ready, starting_line) = std::sync::mpsc::channel();
    let out_reader = std::thread::spawn(move || {
        let mut first_line = String::new();
        stdout.read_line(&mut first_line).unwrap();
        ready.send(first_line).unwrap();
        let mut remaining = String::new();
        stdout.read_to_string(&mut remaining).unwrap();
        remaining
    });
    let first_line = starting_line.recv_timeout(Duration::from_secs(8));
    let mut input = child.stdin.take().unwrap();
    let sent = input.write_all(b"from caller\n");
    drop(input);
    if first_line.is_err() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();

    assert_eq!(first_line.unwrap(), "waiting for input\n");
    assert_eq!(out_reader.join().unwrap(), "received from caller\n");
    sent.unwrap();
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.lines().any(|line| line == "helper diagnostic"),
        "{stderr}"
    );
    let errors = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{stderr}");
    let last_line = serde_json::from_str::<Value>(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(last_line, errors[0]);
    assert_eq!(errors[0]["error"]["kind"], "helper_failed", "{stderr}");
    assert_eq!(errors[0]["error"]["child_exit"], 7, "{stderr}");
    assert_eq!(errors[0]["error"]["channel"], "actions", "{stderr}");
    assert_eq!(lab.status(), Value::Null);
    let power = lab.power();
    assert!(power.status.success(), "{power:?}");
}

#[test]
fn interrupted_colocated_ssh_action_recovers_on_the_named_host() {
    let lab = Lab::new();
    fs::write(
        lab.at("driver/lab.toml"),
        SPLIT_LAB.replace(r#"host = "bench2""#, r#"host = "bench1""#),
    )
    .unwrap();
    fs::write(lab.at("bench1/action.sh"), HOOK).unwrap();
    let interrupted = lab.start(true).wait_with_output().unwrap();
    assert_eq!(interrupted.status.code(), Some(4), "{interrupted:?}");

    let receipt = lab.status();
    assert_eq!(receipt["phase"], "running");
    assert_eq!(receipt["hosts"][0]["host"], "bench1");
    let id = receipt["id"].as_str().unwrap();
    refused(&lab.power(), "power is reserved");
    refused(&lab.recover("lab.toml", id, false), "--confirm-stopped");
    fs::write(lab.at("bench1/finish"), "").unwrap();
    wait_for(&lab.at("bench1/stopped"));
    let recovery = lab.recover("lab.toml", id, true);
    assert!(recovery.status.success(), "{recovery:?}");
    assert_eq!(lab.status(), Value::Null);
    let after = lab.power();
    assert!(after.status.success(), "{after:?}");
}
