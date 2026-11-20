// SPDX-FileCopyrightText: 2026 The Panio Authors
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn panio(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_panio"));
    command
        .current_dir(dir)
        .args(["--lab", "lab.toml"])
        .args(args)
        .env("PANIO_RUNTIME_BASE", dir.join("run"))
        .env("PANIO_JSON_ERRORS", "1")
        .env("HOME", dir)
        .env_remove("PANIO_HOOK_LOCK")
        .env_remove("PANIO_POWER_MUTATION")
        .env_remove("PANIO_LEASE");
    command
}

const GUARDED_ACTION: &str = r#"
[targets.dut.power]
state_cmd = "sh state.sh"
on_cmd = "touch power-ran"

[targets.dut.actions.strap]
cmd = "sh action.sh"
requires_power = "off"
"#;

fn runtime_base(dir: &Path) -> PathBuf {
    let uid = dir.metadata().unwrap().uid();
    dir.join(format!("run/panio-{uid}"))
}

fn is_locked(dir: &Path) -> bool {
    let path = runtime_base(dir).join("hooks/dut/hook.lock");
    let lock = match File::options().write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(e) => panic!("opening the hook lock: {e}"),
    };
    match lock.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(e) => panic!("probing the hook lock: {e}"),
    }
}

#[test]
fn guarded_action_holds_lock_from_state_read_through_action() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(dir.join("lab.toml"), GUARDED_ACTION).unwrap();
    std::fs::write(
        dir.join("state.sh"),
        r#"if [ "$PANIO_TEST_NESTED" != yes ]; then
    head -c 2097152 /dev/zero | tr '\000' x >&2
fi
printf 'off\n'
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("action.sh"),
        r#"test "$PANIO_HOOK_LOCK" = dut || exit 1
PANIO_TEST_NESTED=yes "$PANIO_TEST_BINARY" --lab lab.toml power-state dut || exit 1
printf 'action started\n'
read gate
test "$gate" = release
"#,
    )
    .unwrap();

    let mut child = panio(dir, &["action", "run", "-t", "dut", "strap"])
        .env("PANIO_TEST_BINARY", env!("CARGO_BIN_EXE_panio"))
        .env_remove("PANIO_TEST_NESTED")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut first_byte = [0];
    let diagnostics_started = stderr.read_exact(&mut first_byte);
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    // Panio is forwarding the successful state hook's stderr at this point.
    // The unread pipe blocks that write, so observing the lock here cannot
    // accidentally hit the state hook or the action hook instead.
    let locked_between_hooks = is_locked(dir);
    let drain = std::thread::spawn(move || {
        let mut diagnostics = Vec::new();
        stderr.read_to_end(&mut diagnostics).unwrap();
        diagnostics
    });

    let mut lines = String::new();
    stdout.read_line(&mut lines).unwrap();
    stdout.read_line(&mut lines).unwrap();
    let locked_during_action = is_locked(dir);
    let released = child.stdin.take().unwrap().write_all(b"release\n");
    let status = child.wait().unwrap();
    let diagnostics = drain.join().unwrap();

    diagnostics_started.unwrap();
    assert_eq!(first_byte, [b'x']);
    assert!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&diagnostics[diagnostics.len().saturating_sub(512)..])
    );
    released.unwrap();
    assert_eq!(lines, "Power OFF  (dut)\naction started\n");
    assert!(locked_between_hooks, "guard dropped after the state read");
    assert!(locked_during_action, "action hook ran outside the guard");
    assert!(!is_locked(dir), "guard outlived the action");
}

#[test]
fn legacy_power_checked_flag_cannot_skip_guard() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(dir.join("lab.toml"), GUARDED_ACTION).unwrap();
    std::fs::write(dir.join("state.sh"), "printf 'on\\n'\n").unwrap();
    std::fs::write(dir.join("action.sh"), "touch action-ran\n").unwrap();

    for flag in [
        None,
        Some("--power-checked"),
        Some("--require-power-lock=local"),
    ] {
        let mut args = vec!["action", "run", "-t", "dut"];
        args.extend(flag);
        args.extend(["--", "strap"]);
        let output = panio(dir, &args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(!dir.join("action-ran").exists(), "{output:?}");
    }

    let unguarded = GUARDED_ACTION.replace("requires_power = \"off\"", "");
    std::fs::write(dir.join("lab.toml"), unguarded).unwrap();
    let output = panio(
        dir,
        &["action", "run", "--require-power-lock=local", "strap"],
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(!dir.join("action-ran").exists(), "{output:?}");
    let output = panio(dir, &["action", "run", "--power-checked", "strap"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(dir.join("action-ran").exists());
}

#[test]
fn serial_power_sense_and_button_share_the_hook_lock() {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        // A sandbox that prohibits listening cannot run the daemon fixture.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(e) => panic!("binding the serialcap fixture: {e}"),
    };
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let base = runtime_base(dir.path());
    let serialcap = base.join("serialcap/dut");
    std::fs::create_dir_all(&serialcap).unwrap();
    std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        serialcap.join("daemon.json"),
        format!(r#"{{"pid": {}, "port": {port}}}"#, std::process::id()),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("lab.toml"),
        r#"
[[targets.dut.serial]]
name = "power"
device = "/dev/ttyUSB-test"
power_sense_signal = "cts"
power_button = true

[targets.dut.power]
serial_interface = "power"

[targets.dut.actions.strap]
cmd = "true"
requires_power = "off"
"#,
    )
    .unwrap();
    let commands: &[(&[&str], &str, &str)] = &[
        (
            &["action", "run", "-t", "dut", "strap"],
            "GET /status?interface=power",
            "200 OK",
        ),
        (
            &["serial", "dtr", "dut"],
            "POST /button?interface=power",
            "200 OK",
        ),
        (
            &["serial", "reset", "dut"],
            "POST /button?interface=power",
            "200 OK",
        ),
        (
            &["serial", "dtr", "dut"],
            "POST /button?interface=power",
            "503 Service Unavailable",
        ),
    ];
    for (args, expected_request, status) in commands {
        let mut child = panio(dir.path(), args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                        let _ = child.kill();
                        panic!(
                            "missing {expected_request} from {:?}",
                            child.wait_with_output().unwrap()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accepting the serialcap request: {e}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut socket);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut header = String::new();
        loop {
            header.clear();
            if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                break;
            }
        }
        let locked_during_request = is_locked(dir.path());
        if expected_request.starts_with("POST") {
            assert!(mutation_status(dir.path())["id"].is_string());
        }
        let response = r#"{"power_on": false}"#;
        write!(
            socket,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
        drop(socket);
        let output = child.wait_with_output().unwrap();
        assert!(request.starts_with(expected_request), "{request}");
        assert!(locked_during_request, "{request} ran without the hook lock");
        assert!(!is_locked(dir.path()));
        let marker = mutation_status(dir.path());
        if *status == "200 OK" {
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert!(marker.is_null(), "{marker}");
        } else {
            assert_eq!(output.status.code(), Some(101), "{args:?}: {output:?}");
            assert_eq!(marker["operation"], "DTR pulse");
            recover_mutation(dir.path(), marker["id"].as_str().unwrap());
        }
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !path.is_file() {
        assert!(Instant::now() < deadline, "missing fixture marker {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn guarded_hook_can_read_power_but_cannot_mutate_it() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(dir.join("lab.toml"), GUARDED_ACTION).unwrap();
    std::fs::write(dir.join("state.sh"), "printf 'off\\n'\n").unwrap();
    std::fs::write(
        dir.join("action.sh"),
        r#""$PANIO_TEST_BINARY" --lab lab.toml power-state dut || exit 1
"$PANIO_TEST_BINARY" --lab lab.toml power on dut
test "$?" = 2
"#,
    )
    .unwrap();

    let output = panio(dir, &["action", "run", "-t", "dut", "strap"])
        .env("PANIO_TEST_BINARY", env!("CARGO_BIN_EXE_panio"))
        .output()
        .unwrap();
    assert!(!dir.join("power-ran").exists(), "{output:?}");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("Power OFF  (dut)"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("power is reserved"));
}

#[test]
fn killed_guard_wrapper_reserves_power_until_hook_stops_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(dir.join("lab.toml"), GUARDED_ACTION).unwrap();
    std::fs::write(dir.join("state.sh"), "printf 'off\\n'\n").unwrap();
    std::fs::write(
        dir.join("action.sh"),
        r#"
touch action-started
tries=0
until [ -f finish-action ] || [ "$tries" -ge 750 ]; do
    sleep 0.02
    tries=$((tries + 1))
done
[ -f finish-action ] || exit 99
touch action-stopped
"#,
    )
    .unwrap();

    let mut wrapper = panio(dir, &["action", "run", "-t", "dut", "strap"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&dir.join("action-started"));
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();

    let power = || panio(dir, &["power", "on", "dut"]).output().unwrap();
    let during = power();
    let stopped_early = dir.join("action-stopped").is_file();
    std::fs::write(dir.join("finish-action"), "").unwrap();
    wait_for_file(&dir.join("action-stopped"));

    assert!(!stopped_early);
    assert_eq!(during.status.code(), Some(2), "{during:?}");
    assert!(String::from_utf8_lossy(&during.stderr).contains("power is reserved"));
    assert!(!dir.join("power-ran").exists());
    let status = panio(dir, &["action", "guard-status", "-t", "dut"])
        .output()
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(receipt["phase"], "running");
    let id = receipt["id"].as_str().unwrap();
    let recover = ["action", "recover", "-t", "dut", "--id", id];
    let refused = panio(dir, &recover).output().unwrap();
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--confirm-stopped"));

    let recovered = panio(dir, &recover)
        .arg("--confirm-stopped")
        .output()
        .unwrap();
    assert!(recovered.status.success(), "{recovered:?}");
    let after = power();
    assert!(after.status.success(), "{after:?}");
    assert!(dir.join("power-ran").exists());
}

fn mutation_status(dir: &Path) -> serde_json::Value {
    let output = panio(
        dir,
        &[
            "action",
            "guard-status",
            "-t",
            "dut",
            "--power-host",
            "local",
        ],
    )
    .output()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn recover_mutation(dir: &Path, id: &str) {
    let args = [
        "action",
        "recover",
        "-t",
        "dut",
        "--power-host",
        "local",
        "--id",
        id,
    ];
    let refused = panio(dir, &args).output().unwrap();
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--confirm-stopped"));
    let recovered = panio(dir, &args).arg("--confirm-stopped").output().unwrap();
    assert!(recovered.status.success(), "{recovered:?}");
    assert_eq!(mutation_status(dir), serde_json::Value::Null);
}

#[test]
fn killed_power_wrapper_blocks_guarded_action_until_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    let lab = GUARDED_ACTION.replace("touch power-ran", "exec sh power.sh");
    std::fs::write(dir.join("lab.toml"), lab).unwrap();
    std::fs::write(dir.join("state.sh"), "printf 'off\\n'\n").unwrap();
    std::fs::write(dir.join("action.sh"), "touch action-ran\n").unwrap();
    std::fs::write(
        dir.join("power.sh"),
        r#"
touch power-started
tries=0
until [ -f finish-power ] || [ "$tries" -ge 750 ]; do
    sleep 0.02
    tries=$((tries + 1))
done
[ -f finish-power ] || exit 99
touch power-stopped
"#,
    )
    .unwrap();

    let mut wrapper = panio(dir, &["power", "on", "dut"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&dir.join("power-started"));
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();

    let action = || panio(dir, &["action", "run", "strap"]).output().unwrap();
    let during = action();
    let stopped_early = dir.join("power-stopped").is_file();
    std::fs::write(dir.join("finish-power"), "").unwrap();
    wait_for_file(&dir.join("power-stopped"));

    assert!(!stopped_early);
    assert_eq!(during.status.code(), Some(2), "{during:?}");
    assert!(!dir.join("action-ran").exists());
    let marker = mutation_status(dir);
    assert_eq!(marker["operation"], "power on");
    let id = marker["id"].as_str().unwrap();
    recover_mutation(dir, id);
    let after = action();
    assert!(after.status.success(), "{after:?}");
    assert!(dir.join("action-ran").exists());
}

fn helper_error(output: &std::process::Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(error["error"]["kind"], "helper_failed", "{stderr}");
    error["error"].clone()
}

#[test]
fn child_signal_retains_marker_but_definite_numeric_failure_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    let lab = dir.join("lab.toml");
    let power_on = || panio(dir, &["power", "on", "dut"]).output().unwrap();
    for (hook, expected_child, uncertain) in [
        ("exit 7", Some(7), false),
        ("kill -TERM $$", None, true),
        ("sh -c 'kill -TERM $$'; exit $?", Some(143), true),
    ] {
        std::fs::write(&lab, GUARDED_ACTION.replace("touch power-ran", hook)).unwrap();
        let err = helper_error(&power_on());
        assert_eq!(err["child_exit"].as_u64(), expected_child);
        let marker = mutation_status(dir);
        if !uncertain {
            assert!(marker.is_null(), "{marker}");
        } else {
            assert_eq!(marker["operation"], "power on");
            recover_mutation(dir, marker["id"].as_str().unwrap());
        }
    }
}

#[test]
fn nested_power_hooks_share_the_marker_and_keep_uncertain_results() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(dir.join("state.sh"), "printf 'off\\n'\n").unwrap();
    std::fs::write(
        dir.join("power.sh"),
        r#"printf '%s\n' "$PANIO_POWER_MUTATION" > mutation-id
"$PANIO_TEST_BINARY" --lab lab.toml power-state dut || exit 91
"$PANIO_TEST_BINARY" --lab lab.toml power off dut || test "$PANIO_TEST_IGNORE_NESTED" = yes
"#,
    )
    .unwrap();
    let lab = GUARDED_ACTION.replace(
        "on_cmd = \"touch power-ran\"",
        "on_cmd = \"sh power.sh\"\noff_cmd = \"touch nested-write\"",
    );
    let power_on = |ignore| {
        panio(dir, &["power", "on", "dut"])
            .env("PANIO_TEST_BINARY", env!("CARGO_BIN_EXE_panio"))
            .env("PANIO_TEST_IGNORE_NESTED", ignore)
            .output()
            .unwrap()
    };

    std::fs::write(dir.join("lab.toml"), &lab).unwrap();
    let known = power_on("no");
    assert!(known.status.success(), "{known:?}");
    assert!(String::from_utf8_lossy(&known.stdout).contains("Power OFF  (dut)"));
    assert!(mutation_status(dir).is_null());
    let old_id = std::fs::read_to_string(dir.join("mutation-id")).unwrap();
    std::fs::remove_file(dir.join("nested-write")).unwrap();
    let stale = panio(dir, &["power", "off", "dut"])
        .env("PANIO_HOOK_LOCK", "dut")
        .env("PANIO_POWER_MUTATION", old_id.trim())
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(2), "{stale:?}");
    assert!(!dir.join("nested-write").exists());
    std::fs::write(dir.join("action.sh"), "touch action-ran\n").unwrap();
    let stale_action = panio(dir, &["action", "run", "strap"])
        .env("PANIO_HOOK_LOCK", "dut")
        .env("PANIO_POWER_MUTATION", old_id.trim())
        .output()
        .unwrap();
    assert_eq!(stale_action.status.code(), Some(2), "{stale_action:?}");
    assert!(!dir.join("action-ran").exists());
    assert!(mutation_status(dir).is_null());

    std::fs::write(
        dir.join("lab.toml"),
        lab.replace("touch nested-write", "kill -TERM $$"),
    )
    .unwrap();
    let uncertain = power_on("yes");
    assert_eq!(uncertain.status.code(), Some(2), "{uncertain:?}");
    let marker = mutation_status(dir);
    assert_eq!(marker["operation"], "power on");
    assert_eq!(marker["pending_children"], 1);
    recover_mutation(dir, marker["id"].as_str().unwrap());
}

#[test]
fn direct_button_error_keeps_the_mutation_marker() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(
        dir.join("lab.toml"),
        r#"
[[targets.dut.serial]]
name = "power"
device = "missing-fixture-device"
power_button = true
"#,
    )
    .unwrap();
    let failed = panio(dir, &["serial", "dtr", "dut"]).output().unwrap();
    assert!(!failed.status.success(), "{failed:?}");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("unknown outcome"));
    let marker = mutation_status(dir);
    assert_eq!(marker["operation"], "DTR pulse");
    recover_mutation(dir, marker["id"].as_str().unwrap());
}

#[test]
fn failed_cycle_confirmation_keeps_the_mutation_marker() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    std::fs::write(
        dir.join("lab.toml"),
        r#"[targets.dut.power]
cycle_cmd = "touch cycle-requested"
state_cmd = "exit 9"
"#,
    )
    .unwrap();
    let failed = panio(dir, &["power-cycle", "dut"]).output().unwrap();
    assert!(dir.join("cycle-requested").is_file());
    assert_eq!(helper_error(&failed)["child_exit"], 9);
    let marker = mutation_status(dir);
    assert_eq!(marker["operation"], "power-cycle");
    recover_mutation(dir, marker["id"].as_str().unwrap());
}
