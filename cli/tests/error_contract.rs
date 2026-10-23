// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! The error contract, end to end: run the built binary and check the exit
//! status and the JSON error object for each kind of failure
//! (`docs/dev/error-contract/design.md`). The inputs are the failures a
//! consumer has to tell apart.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const LAB: &str = r#"
[targets.dut]

[[targets.dut.serial]]
name = "console"
device = "/dev/ttyUSB-test"
baud = 115200
"#;

/// A fresh directory holding `lab.toml` (and an empty runtime dir, so no
/// daemon running on the machine under test can answer).
fn scratch(name: &str, lab: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("error_contract")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("run")).unwrap();
    std::fs::write(dir.join("lab.toml"), lab).unwrap();
    dir
}

fn panio(dir: &Path, json: bool, args: &[&str]) -> Output {
    panio_env(dir, json, args, &[])
}

fn panio_env(dir: &Path, json: bool, args: &[&str], env: &[(&str, String)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_panio"));
    cmd.arg("--lab")
        .arg(dir.join("lab.toml"))
        .args(args)
        .env("PANIO_RUNTIME_BASE", dir.join("run"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env_remove("PANIO_JSON_ERRORS")
        .env_remove("PANIO_LAB");
    if json {
        cmd.env("PANIO_JSON_ERRORS", "1");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// The JSON object: the last line of stderr, parsed.
fn error_object(out: &Output) -> Value {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().last().expect("stderr is empty");
    let v: Value = serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("last stderr line is not JSON ({e}): {last}"));
    v["error"].clone()
}

/// The `category` docs/errors.md gives each kind by default.
fn category_of(kind: &str) -> Value {
    match kind {
        "not_configured" | "unreachable" | "daemon_down" => "infrastructure".into(),
        "timeout" => "transient".into(),
        "helper_failed" => "device_state".into(),
        _ => Value::Null,
    }
}

/// Run `args` with JSON errors on; assert the code, kind, category, target
/// and channel, and that the message is the prose line printed above the object.
fn expect(
    name: &str,
    args: &[&str],
    code: i32,
    kind: &str,
    target: Option<&str>,
    channel: Option<&str>,
) {
    let dir = scratch(name, LAB);
    let out = panio(&dir, true, args);
    assert_eq!(out.status.code(), Some(code), "{args:?}: {out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], kind, "{e}");
    assert_eq!(e["code"], code, "{e}");
    assert_eq!(e["category"], category_of(kind), "{e}");
    assert_eq!(e["target"].as_str(), target, "{e}");
    assert_eq!(e["channel"].as_str(), channel, "{e}");
    assert!(e["host"].is_null() && e["child_exit"].is_null(), "{e}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let prose = stderr.lines().rev().nth(1).unwrap();
    assert_eq!(e["message"], prose, "{e}");
}

#[test]
fn unknown_target_is_not_configured() {
    expect(
        "unknown_pc",
        &["power-cycle", "nosuch"],
        3,
        "not_configured",
        Some("nosuch"),
        None,
    );
    expect(
        "unknown_log",
        &["serial", "log", "-t", "nosuch"],
        3,
        "not_configured",
        Some("nosuch"),
        None,
    );
}

#[test]
fn missing_channel_is_not_configured() {
    expect(
        "no_hid",
        &["hid", "send", "-t", "dut", "releaseall"],
        3,
        "not_configured",
        Some("dut"),
        Some("hid"),
    );
    expect(
        "no_power",
        &["power-cycle", "dut"],
        3,
        "not_configured",
        Some("dut"),
        Some("power"),
    );
}

#[test]
fn missing_netboot_root_is_not_configured_without_touching_runtime_or_lab() {
    for (name, root) in [
        ("no_tftp_root", ""),
        ("empty_tftp_root", "tftp_root = \"\"\n"),
    ] {
        let lab = format!(
            "[hosts.control]\nssh = \"local\"\n\
             [targets.dut]\n\
             [targets.dut.netboot]\nhost = \"control\"\n\
             interface = \"test-netboot-interface\"\n{root}"
        );
        let dir = scratch(name, &lab);
        let out = panio(&dir, true, &["netboot", "tftp-root", "dut"]);
        assert_eq!(out.status.code(), Some(3), "{out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        let e = error_object(&out);
        assert_eq!(e["kind"], "not_configured");
        assert_eq!(e["code"], 3);
        assert_eq!(e["target"], "dut");
        assert_eq!(e["channel"], "netboot");
        assert_eq!(e["host"], "control");
        assert!(e["child_exit"].is_null());
        assert!(e["message"]
            .as_str()
            .unwrap()
            .contains("--tftp-root <path>"));

        // A read-only preflight must flag this before probing the interface.
        let out = panio(&dir, true, &["doctor", "dut"]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let report = String::from_utf8_lossy(&out.stdout);
        assert!(report.contains("dut\tnetboot\t@control\tMISSING\tno tftp_root configured"));
        assert_eq!(std::fs::read_to_string(dir.join("lab.toml")).unwrap(), lab);
        assert_eq!(std::fs::read_dir(dir.join("run")).unwrap().count(), 0);
    }
}

#[test]
fn configured_netboot_root_prints_bare_path_without_starting_a_service() {
    let dir = scratch(
        "configured_tftp_root",
        "[targets.dut.netboot]\ninterface = \"test-netboot-interface\"\n\
         tftp_root = \"/srv/tftp/dut\"\n",
    );
    let out = panio(&dir, true, &["netboot", "tftp-root", "-t", "dut"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(out.stdout, b"/srv/tftp/dut\n");
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(std::fs::read_dir(dir.join("run")).unwrap().count(), 0);
}

#[test]
fn netboot_start_and_netif_netboot_refuse_a_missing_root_as_not_configured() {
    // #283: the verbs that act on the interface must refuse an unset
    // tftp_root as not_configured (exit 3, with context) before touching the
    // interface or spawning netbootd, not fail as an internal error.
    let lab = "[hosts.control]\nssh = \"local\"\n\
               [targets.dut]\n\
               [targets.dut.netboot]\nhost = \"control\"\n\
               interface = \"test-netboot-interface\"\n";
    for (name, args) in [
        ("start_no_root", &["netboot", "start", "dut"][..]),
        (
            "netif_no_root",
            &["netif", "mode", "netboot", "-t", "dut"][..],
        ),
    ] {
        let dir = scratch(name, lab);
        let out = panio(&dir, true, args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        let e = error_object(&out);
        assert_eq!(e["kind"], "not_configured", "{args:?}");
        assert_eq!(e["target"], "dut", "{args:?}");
        assert_eq!(e["channel"], "netboot", "{args:?}");
        assert_eq!(e["host"], "control", "{args:?}");
        assert!(
            e["message"]
                .as_str()
                .unwrap()
                .contains("--tftp-root <path>"),
            "{args:?}: {e}"
        );
        assert_eq!(std::fs::read_dir(dir.join("run")).unwrap().count(), 0);
    }
}

#[test]
fn missing_serial_interface_is_not_configured() {
    expect(
        "no_iface",
        &["serial", "log", "-t", "dut", "-i", "nosuch"],
        3,
        "not_configured",
        Some("dut"),
        Some("serial"),
    );
}

#[test]
fn missing_lab_file_is_not_configured() {
    let dir = scratch("no_lab", LAB);
    std::fs::remove_file(dir.join("lab.toml")).unwrap();
    let out = panio(&dir, true, &["power-cycle", "dut"]);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(error_object(&out)["kind"], "not_configured");
}

#[test]
fn ambiguous_target_is_usage() {
    let dir = scratch("two", "[targets.a]\n[targets.b]\n");
    let out = panio(&dir, true, &["power-cycle"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(error_object(&out)["kind"], "usage");
}

#[test]
fn clap_usage_errors_exit_2_with_a_usage_object() {
    let dir = scratch("clap", LAB);
    let out = panio(&dir, true, &["power-cycle", "--bogus"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], "usage");
    assert_eq!(e["message"], "unexpected argument '--bogus' found");
    // Present and null: a caller's mistake points at nothing in the setup.
    assert_eq!(e.get("category"), Some(&Value::Null), "{e}");
    // The flag form works too, though parsing (which reads it) failed.
    let out = panio(&dir, false, &["--json-errors", "power-cycle", "--bogus"]);
    assert_eq!(error_object(&out)["code"], 2);
    // --help is not a failure: no object.
    let out = panio(&dir, true, &["--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("\"error\""));
}

/// The stopped-daemon check (was the M1 example of `internal`).
#[test]
fn a_stopped_serial_daemon_is_daemon_down() {
    expect(
        "serial_down",
        &["serial", "send", "-t", "dut", "hello"],
        100,
        "daemon_down",
        Some("dut"),
        Some("serial"),
    );
}

/// `serial log` still reads the on-disk log with the daemon stopped, but says
/// so on stderr; `--require-live` refuses instead (design D4).
#[test]
fn serial_log_warns_when_the_daemon_is_stopped_and_can_require_it() {
    expect(
        "log_live",
        &["serial", "log", "-t", "dut", "--require-live"],
        100,
        "daemon_down",
        Some("dut"),
        Some("serial"),
    );
    let dir = scratch("log_warn", LAB);
    let out = panio(&dir, false, &["serial", "log", "-t", "dut"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("warning: serialcap daemon for 'dut' is not running"),
        "{stderr}"
    );
    // Whatever serialcap then does on this machine, it is not refused.
    assert_ne!(out.status.code(), Some(100), "{out:?}");
}

#[test]
fn a_missing_required_argument_names_it() {
    let dir = scratch("missing_arg", LAB);
    let out = panio(&dir, true, &["hid", "set", "-t", "dut"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(
        error_object(&out)["message"],
        "the following required arguments were not provided: --cmd <CMD>"
    );
    // Several missing: all named, not just the first.
    let out = panio(&dir, true, &["serial", "add", "-t", "dut"]);
    assert_eq!(
        error_object(&out)["message"],
        "the following required arguments were not provided: --device <DEVICE>, <NAME>"
    );
}

#[test]
fn no_json_unless_requested_and_the_flag_matches_the_variable() {
    let dir = scratch("opt_in", LAB);
    let plain = panio(&dir, false, &["power-cycle", "nosuch"]);
    assert_eq!(plain.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&plain.stderr),
        "target 'nosuch' not found in lab\n",
        "without the opt-in, stderr is exactly the 0.4 text"
    );
    let flag = panio(&dir, false, &["--json-errors", "power-cycle", "nosuch"]);
    let var = panio(&dir, true, &["power-cycle", "nosuch"]);
    assert_eq!(flag.stderr, var.stderr);
    assert!(flag.stdout.is_empty(), "stdout stays clean");
}

/// Serial interfaces on two hosts and no `-i`: the caller must name one, so
/// this is a usage error (like an ambiguous target), decided before any
/// dispatch is attempted.
#[test]
fn serial_on_two_hosts_without_an_interface_is_usage() {
    let lab = r#"
[hosts.bench1]
ssh = "u@bench1"
[hosts.bench2]
ssh = "u@bench2"
[targets.nuc]
host = "bench1"
[[targets.nuc.serial]]
name = "console"
device = "/dev/ttyUSB0"
[[targets.nuc.serial]]
name = "debug"
device = "/dev/ttyUSB1"
host = "bench2"
"#;
    let dir = scratch("two_hosts", lab);
    let out = panio(&dir, true, &["serial", "log", "-t", "nuc"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], "usage");
    assert_eq!(e["target"], "nuc");
    assert_eq!(e["channel"], "serial");
}

#[test]
fn unknown_dtr_interface_is_not_configured() {
    expect(
        "dtr_iface",
        &["serial", "dtr", "-t", "dut", "-i", "nosuch"],
        3,
        "not_configured",
        Some("dut"),
        Some("serial"),
    );
}

#[test]
fn ambiguous_dtr_interface_is_usage() {
    let lab = r#"
[targets.nuc]
[[targets.nuc.serial]]
name = "a"
device = "/dev/ttyUSB0"
power_button = true
[[targets.nuc.serial]]
name = "b"
device = "/dev/ttyUSB1"
power_button = true
"#;
    let dir = scratch("dtr_two", lab);
    let out = panio(&dir, true, &["serial", "dtr", "-t", "nuc"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(error_object(&out)["kind"], "usage");
}

#[test]
fn serial_watch_without_interfaces_is_not_configured() {
    let dir = scratch("watch_bare", "[targets.bare]\n");
    let out = panio(&dir, true, &["serial", "watch", "-t", "bare"]);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["target"], "bare");
    assert_eq!(e["channel"], "serial");
}

#[test]
fn missing_subcommand_is_usage_with_a_plain_message() {
    let dir = scratch("no_sub", LAB);
    let out = panio(&dir, true, &["power"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(error_object(&out)["message"], "a subcommand is required");
}

#[test]
fn a_repeated_json_errors_flag_is_accepted() {
    let dir = scratch("repeat", LAB);
    let out = panio(
        &dir,
        false,
        &["--json-errors", "--json-errors", "power-cycle", "nosuch"],
    );
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(error_object(&out)["kind"], "not_configured");
}

// ── M2: the child boundary ──────────────────────────────────────────────────

fn power_lab(cycle_cmd: &str) -> String {
    format!("[targets.nuc]\n[targets.nuc.power]\ncycle_cmd = {cycle_cmd:?}\n")
}

#[test]
fn a_failing_power_hook_is_helper_failed_with_its_code() {
    let dir = scratch("hook_7", &power_lab("exit 7"));
    let out = panio(&dir, true, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(101), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], "helper_failed");
    assert_eq!(e["category"], "device_state");
    assert_eq!(e["child_exit"], 7);
    assert_eq!(e["target"], "nuc");
    assert_eq!(e["channel"], "power");
    // A hook exiting 2 no longer reads as a usage error.
    let dir = scratch("hook_2", &power_lab("exit 2"));
    let out = panio(&dir, false, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(101), "{out:?}");
}

/// `sh` reports a missing script as 127. (`cmd.exe` exits 1 for it, the same
/// as a failing script; see the Windows test below.)
#[cfg(unix)]
#[test]
fn a_missing_power_hook_is_not_configured() {
    let dir = scratch("hook_127", &power_lab("/nonexistent/relay-script"));
    let out = panio(&dir, true, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], "not_configured");
    assert_eq!(e["category"], "infrastructure");
    assert_eq!(e["child_exit"], 127);
}

/// `cmd.exe /C` exits 1 for an unknown command (9009 is only `%ERRORLEVEL%`
/// inside a batch file), indistinguishable from a failing script, so on
/// Windows a missing hook is `helper_failed` (documented in docs/errors.md).
#[cfg(windows)]
#[test]
fn a_missing_hook_is_helper_failed_on_windows() {
    let dir = scratch(
        "hook_missing_win",
        &power_lab("panio-no-such-relay-command"),
    );
    let out = panio(&dir, true, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(101), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], "helper_failed");
    assert_eq!(e["child_exit"], 1);
}

#[cfg(unix)]
#[test]
fn a_hook_killed_by_a_signal_is_helper_failed_without_a_code() {
    let dir = scratch("hook_sig", &power_lab("kill -TERM $$"));
    let out = panio(&dir, true, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(101), "{out:?}");
    assert!(error_object(&out)["child_exit"].is_null());
}

/// With a `state_cmd`, `power-cycle` reports success only once power is back
/// on (#259): here the hook returns at once and the state reads off twice
/// before it reads on, as with a relay board that times the off period itself.
#[cfg(unix)]
#[test]
fn power_cycle_waits_for_state_cmd_to_report_on() {
    let dir = scratch("cycle_waits", "");
    let count = dir.join("state-calls");
    let state_cmd = format!(
        "n=$(cat '{c}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{c}'; \
         [ $n -ge 3 ] && echo on || echo off",
        c = count.display()
    );
    let lab = format!(
        "[targets.nuc]\n[targets.nuc.power]\ncycle_cmd = \"true\"\nstate_cmd = {state_cmd:?}\n"
    );
    std::fs::write(dir.join("lab.toml"), lab).unwrap();
    let out = panio(&dir, true, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("back on"), "{stdout}");
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "3");
}

/// Without a `state_cmd`, panio cannot confirm the cycle, and says so
/// instead of claiming it is complete.
#[test]
fn power_cycle_without_state_cmd_does_not_claim_completion() {
    let dir = scratch("cycle_no_state", &power_lab("exit 0"));
    let out = panio(&dir, true, &["power-cycle", "nuc"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("requested"), "{stdout}");
    assert!(!stdout.contains("complete"), "{stdout}");
}

/// A directory with fake `ssh` and `sftp` first on PATH. `sftp` logs its
/// batch commands to `sftp.log` and succeeds (or fails with STUB_SFTP=fail);
/// `ssh` records its argv in `ssh.args` and
/// then behaves per STUB_SSH: `down` is a transport failure (255), `remote3`
/// is a 0.5 remote panio reporting not_configured with its JSON line.
#[cfg(unix)]
fn stub_bin(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = format!(
        r#"#!/bin/sh
printf '%s\n' "$@" > '{args}'
cat >/dev/null
case "$STUB_SSH" in
  down) echo "ssh: connect to host bench1 port 22: Connection refused" >&2; exit 255 ;;
  remote3)
    echo "target 'nuc' not found in lab" >&2
    echo '{{"error":{{"kind":"not_configured","code":3,"message":"target '"'"'nuc'"'"' not found in lab","target":"nuc","channel":null,"host":null,"child_exit":null}}}}' >&2
    exit 3 ;;
esac
exit 0
"#,
        args = dir.join("ssh.args").display()
    );
    let sftp = format!(
        "#!/bin/sh\ncat >> '{log}'\n[ \"$STUB_SFTP\" = fail ] && {{ echo 'Connection closed' >&2; exit 1; }}\nexit 0\n",
        log = dir.join("sftp.log").display()
    );
    for (name, body) in [("ssh", ssh.as_str()), ("sftp", sftp.as_str())] {
        let p = bin.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[cfg(unix)]
const REMOTE_LAB: &str = r#"
[hosts.bench1]
ssh = "u@bench1"
[targets.nuc]
host = "bench1"
[targets.nuc.power]
cycle_cmd = "true"
"#;

#[cfg(unix)]
#[test]
fn ssh_transport_failure_is_unreachable_not_255() {
    let dir = scratch("ssh_down", REMOTE_LAB);
    let path = stub_bin(&dir);
    let env = [("PATH", path), ("STUB_SSH", "down".to_string())];
    let out = panio_env(&dir, true, &["power-cycle", "nuc"], &env);
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let e = error_object(&out);
    assert_eq!(e["kind"], "unreachable");
    assert_eq!(e["host"], "bench1");
    assert_eq!(e["target"], "nuc");
    // A 255 can come from a live host (a remote signal death), so the
    // shipped slice is still removed.
    let log = std::fs::read_to_string(dir.join("sftp.log")).unwrap();
    assert!(log.contains("rm "), "{log}");
}

#[cfg(unix)]
#[test]
fn a_missing_sftp_is_not_configured() {
    let dir = scratch("no_sftp", REMOTE_LAB);
    let path = stub_bin(&dir);
    std::fs::remove_file(dir.join("bin").join("sftp")).unwrap();
    // Only the stub directory: no system sftp to fall back on.
    let bin = path.split(':').next().unwrap().to_string();
    let out = panio_env(&dir, true, &["power-cycle", "nuc"], &[("PATH", bin)]);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(error_object(&out)["kind"], "not_configured");
}

#[cfg(unix)]
#[test]
fn a_failed_slice_copy_is_unreachable() {
    let dir = scratch("sftp_down", REMOTE_LAB);
    let path = stub_bin(&dir);
    let env = [("PATH", path), ("STUB_SFTP", "fail".to_string())];
    let out = panio_env(&dir, true, &["power-cycle", "nuc"], &env);
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert_eq!(error_object(&out)["host"], "bench1");
}

#[cfg(unix)]
#[test]
fn a_remote_failure_arrives_with_its_code_and_one_json_object() {
    let dir = scratch("remote3", REMOTE_LAB);
    let path = stub_bin(&dir);
    let env = [("PATH", path), ("STUB_SSH", "remote3".to_string())];
    let out = panio_env(&dir, true, &["power-cycle", "nuc"], &env);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("\"error\"").count(), 1, "{stderr}");
    assert_eq!(error_object(&out)["kind"], "not_configured");
    // The request crossed the hop as an argument.
    let args = std::fs::read_to_string(dir.join("ssh.args")).unwrap();
    assert!(args.contains("--json-errors"), "{args}");
}

#[cfg(unix)]
#[test]
fn a_passthrough_killed_by_a_signal_exits_128_plus_n() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("editor_sig", LAB);
    let editor = dir.join("editor");
    std::fs::write(&editor, "#!/bin/sh\nkill -TERM $$\n").unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("EDITOR", editor.display().to_string())];
    let out = panio_env(&dir, false, &["config", "edit"], &env);
    assert_eq!(out.status.code(), Some(143), "{out:?}");
    // And the editor's own status is kept (design D3).
    std::fs::write(&editor, "#!/bin/sh\nexit 5\n").unwrap();
    let out = panio_env(&dir, false, &["config", "edit"], &env);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
}

/// Hooks do not inherit the JSON request (design D1, review N1): a hook that
/// ran panio would print its own object ahead of ours. The hook exits 9 if
/// it can see the variable, 7 otherwise.
#[test]
fn hooks_do_not_inherit_the_json_request() {
    // Hooks run under `sh -c`, or `cmd.exe /C` on Windows.
    let hook = if cfg!(windows) {
        "if defined PANIO_JSON_ERRORS (exit 9) else (exit 7)"
    } else {
        r#"[ -n "$PANIO_JSON_ERRORS" ] && exit 9; exit 7"#
    };
    for (name, json, args) in [
        ("inherit_var", true, vec!["power-cycle", "nuc"]),
        (
            "inherit_flag",
            false,
            vec!["--json-errors", "power-cycle", "nuc"],
        ),
    ] {
        let dir = scratch(name, &power_lab(hook));
        let out = panio(&dir, json, &args);
        assert_eq!(out.status.code(), Some(101), "{out:?}");
        assert_eq!(error_object(&out)["child_exit"], 7, "{name}");
    }
}

#[test]
fn serial_health_stopped_snapshot_is_read_only() {
    let dir = scratch("serial_health_stopped", LAB);
    let out = panio(&dir, true, &["serial", "status", "dut"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let snapshot: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(snapshot["process_live"], false);
    assert_eq!(snapshot["daemon_state"], "stopped");
    assert!(snapshot["active"].is_null());
    assert_eq!(snapshot["configured"][0]["device"], "/dev/ttyUSB-test");
    assert_eq!(snapshot["command_execution"], "not_assessed");
    assert_eq!(std::fs::read_to_string(dir.join("lab.toml")).unwrap(), LAB);
    assert_eq!(std::fs::read_dir(dir.join("run")).unwrap().count(), 0);
}

/// A reader that leaves early (`panio … | head -1`) is a quiet success, not
/// a panic and exit 101. The pipe's read end is closed before the child
/// starts, so its first write fails every time rather than by timing.
#[test]
fn a_closed_stdout_exits_zero_without_a_panic() {
    let dir = scratch("closed-stdout", LAB);
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(env!("CARGO_BIN_EXE_panio"))
        .arg("--lab")
        .arg(dir.join("lab.toml"))
        .args(["config", "path"])
        .env("PANIO_RUNTIME_BASE", dir.join("run"))
        .stdout(writer)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(0), "{stderr}");
}

/// A successful state hook's warnings must reach the caller without changing
/// the stdout line consumed by the dashboard and other callers.
#[cfg(unix)]
#[test]
fn power_state_forwards_successful_hook_stderr_and_preserves_stdout() {
    for (verdict, expected) in [("on details", "ON"), ("oFf details", "OFF")] {
        let cmd = format!("printf '%s' '{verdict}'; printf '%s' 'insecure transport warning' >&2");
        let lab = format!("[targets.nuc.power]\nstate_cmd = {cmd:?}\n");
        let dir = scratch(&format!("state_warning_{expected}"), &lab);
        let out = panio(&dir, true, &["power-state", "nuc"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("Power {expected}  (nuc)\n")
        );
        assert_eq!(out.stderr, b"insecure transport warning");
    }
}

/// Failed hooks already carry stderr in their error; don't print it a second
/// time as a standalone diagnostic.
#[cfg(unix)]
#[test]
fn power_state_failed_hook_reports_stderr_once_in_the_error() {
    let cmd = "printf 'on'; printf 'transport failed' >&2; exit 7";
    let lab = format!("[targets.nuc.power]\nstate_cmd = {cmd:?}\n");
    let dir = scratch("state_failure_stderr", &lab);
    let out = panio(&dir, false, &["power-state", "nuc"]);
    assert_eq!(out.status.code(), Some(101), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("stderr: transport failed"), "{stderr}");
    assert!(!stderr.starts_with("transport failed"), "{stderr}");
    assert_eq!(
        stderr.matches("stderr: transport failed").count(),
        1,
        "{stderr}"
    );
    let out = panio(&dir, true, &["power-state", "nuc"]);
    let error = error_object(&out);
    assert_eq!(error["kind"], "helper_failed");
    assert_eq!(error["child_exit"], 7);
    assert_eq!(error["channel"], "power");
}
