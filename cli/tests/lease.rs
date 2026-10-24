// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0
#![cfg(unix)]

//! `panio lease acquire|renew|release|status`, end to end against the built
//! binary. `HOME` is the scratch directory, so the lease store lands there.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const LAB: &str = r#"
[targets.dut]

[[targets.dut.serial]]
name = "console"
device = "/dev/ttyUSB-test"
baud = 115200
"#;

const CROSS_HOST_LAB: &str = r#"
[hosts.bench1]
ssh = "u@bench1"
[hosts.bench2]
ssh = "u@bench2"
[targets.dut]
host = "bench1"
[targets.dut.video]
host = "bench2"
device = "0"
"#;

fn scratch(name: &str, lab: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("lease")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("run")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::write(dir.join("lab.toml"), lab).unwrap();
    dir
}

fn panio(dir: &Path, args: &[&str], token: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_panio"));
    cmd.stdin(std::process::Stdio::null())
        .arg("--lab")
        .arg(dir.join("lab.toml"))
        .args(args)
        .env("HOME", dir.join("home"))
        .env("PANIO_RUNTIME_BASE", dir.join("run"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env_remove("PANIO_JSON_ERRORS")
        .env_remove("PANIO_LAB")
        .env_remove("PANIO_LEASE");
    if let Some(t) = token {
        cmd.env("PANIO_LEASE", t);
    }
    cmd.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn acquire(dir: &Path) -> String {
    let o = panio(dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    stdout(&o).trim().to_string()
}

#[test]
fn acquire_prints_token_only() {
    let dir = scratch("acquire_prints_token_only", LAB);
    let o = panio(&dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert_eq!(out.lines().count(), 1, "{out:?}");
    let tok = out.trim();
    assert_eq!(tok.len(), 32, "{out:?}");
    assert!(tok.chars().all(|c| c.is_ascii_hexdigit()), "{out:?}");
}

#[test]
fn acquire_json_shape() {
    let dir = scratch("acquire_json_shape", LAB);
    let o = panio(
        &dir,
        &[
            "lease",
            "acquire",
            "dut",
            "--json",
            "--owner",
            "alice@bench1",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["token"].as_str().unwrap().len(), 32);
    assert_eq!(v["owner"], "alice@bench1");
    assert!(v["expires_at"].as_u64().unwrap() > 0);
    let left = v["expires_in_secs"].as_u64().unwrap();
    assert!((7100..=7200).contains(&left), "{v}");
}

#[test]
fn acquire_held_fails_23() {
    let dir = scratch("acquire_held_fails_23", LAB);
    let o = panio(
        &dir,
        &["lease", "acquire", "dut", "--owner", "alice@bench1"],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o2 = panio(&dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o2.status.code(), Some(23), "{o2:?}");
    assert!(stderr(&o2).contains("alice@bench1"), "{}", stderr(&o2));
}

#[test]
fn acquire_is_idempotent_for_holder() {
    let dir = scratch("acquire_is_idempotent_for_holder", LAB);
    let tok = acquire(&dir);
    let o = panio(&dir, &["lease", "acquire", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stdout(&o).trim(), tok);
}

#[test]
fn acquire_wait_times_out() {
    let dir = scratch("acquire_wait_times_out", LAB);
    acquire(&dir);
    let t0 = Instant::now();
    let o = panio(&dir, &["lease", "acquire", "dut", "--wait", "2s"], None);
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    assert!(t0.elapsed() >= Duration::from_secs(2), "{:?}", t0.elapsed());
}

#[test]
fn acquire_wait_succeeds_after_release() {
    let dir = scratch("acquire_wait_succeeds_after_release", LAB);
    let tok = acquire(&dir);
    let d2 = dir.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        let o = panio(&d2, &["lease", "release", "dut"], Some(&tok));
        assert_eq!(o.status.code(), Some(0), "{o:?}");
    });
    let o = panio(&dir, &["lease", "acquire", "dut", "--wait", "30s"], None);
    t.join().unwrap();
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stdout(&o).trim().len(), 32);
}

#[test]
fn preempt_takes_over_and_old_token_is_told() {
    let dir = scratch("preempt_takes_over", LAB);
    let old = acquire(&dir);
    let o = panio(&dir, &["lease", "acquire", "dut", "--preempt"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let new = stdout(&o).trim().to_string();
    assert_ne!(new, old);
    let o = panio(&dir, &["power", "on", "dut"], Some(&old));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    assert!(stderr(&o).contains("preempted by"), "{}", stderr(&o));
}

#[test]
fn preempt_replaces_record_and_records_history() {
    let dir = scratch("preempt_replaces_record", LAB);
    let old = acquire(&dir);
    let o = panio(
        &dir,
        &[
            "lease",
            "acquire",
            "dut",
            "--preempt",
            "--owner",
            "bob@bench1",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let new = stdout(&o).trim().to_string();
    assert_ne!(new, old);
    let hist = dir.join("home/.local/share/panio/dut/lease-history.jsonl");
    let text = std::fs::read_to_string(hist).unwrap();
    let line: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(line["how"], "preempted");
    assert_eq!(line["previous_token"], old.as_str());
    assert_eq!(line["new_owner"], "bob@bench1");
    let rec = std::fs::read_to_string(dir.join("home/.local/share/panio/dut/lease.json")).unwrap();
    assert!(rec.contains(&new) && !rec.contains(&old));
}

#[test]
fn renew_extends() {
    let dir = scratch("renew_extends", LAB);
    let tok = acquire(&dir);
    let o = panio(
        &dir,
        &["lease", "renew", "dut", "--duration", "4h"],
        Some(&tok),
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let s = panio(&dir, &["lease", "status", "dut", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&stdout(&s)).unwrap();
    let left = v["expires_in_secs"].as_u64().unwrap();
    assert!((14_300..=14_400).contains(&left), "{v}");
}

#[test]
fn renew_without_token_fails_23() {
    let dir = scratch("renew_without_token", LAB);
    acquire(&dir);
    let o = panio(&dir, &["lease", "renew", "dut"], None);
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    let o = panio(&dir, &["lease", "renew", "dut"], Some("deadbeef"));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
}

#[test]
fn release_without_token_fails_23() {
    let dir = scratch("release_without_token", LAB);
    acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], None);
    assert_eq!(o.status.code(), Some(23), "{o:?}");
}

#[test]
fn release_with_token_clears() {
    let dir = scratch("release_with_token", LAB);
    let tok = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let s = panio(&dir, &["lease", "status", "dut", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&stdout(&s)).unwrap();
    assert_eq!(v["held"], false, "{v}");
    let o = panio(&dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
}

#[test]
fn release_preempt_clears_without_token() {
    let dir = scratch("release_preempt", LAB);
    acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut", "--preempt"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
}

#[test]
fn status_never_prints_token() {
    let dir = scratch("status_never_prints_token", LAB);
    let tok = acquire(&dir);
    for args in [
        &["lease", "status", "dut"][..],
        &["lease", "status", "dut", "--json"][..],
        &["lease", "status"][..],
    ] {
        let o = panio(&dir, args, Some(&tok));
        assert_eq!(o.status.code(), Some(0), "{o:?}");
        assert!(!stdout(&o).contains(&tok), "{args:?}");
        assert!(!stderr(&o).contains(&tok), "{args:?}");
    }
    let o = panio(&dir, &["lease", "status", "dut", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["target"], "dut");
    assert_eq!(v["held"], true);
    assert!(v["owner"].is_string());
    assert!(v["idle_left_secs"].as_u64().unwrap() <= 1800);
    assert_eq!(v["procs"], 0);
}

#[test]
fn acquire_cross_host_target_refused() {
    let dir = scratch("acquire_cross_host", CROSS_HOST_LAB);
    let o = panio(&dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(3), "{o:?}");
    let e = stderr(&o);
    assert!(e.contains("bench1") && e.contains("bench2"), "{e}");
}

#[test]
fn duration_cap() {
    let dir = scratch("duration_cap", LAB);
    for args in [
        &["lease", "acquire", "dut", "--duration", "25h"][..],
        &["lease", "acquire", "dut", "--idle", "25h"][..],
        &["lease", "acquire", "dut", "--wait", "25h"][..],
        &["lease", "acquire", "dut", "--duration", "soon"][..],
    ] {
        let o = panio(&dir, args, None);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {o:?}");
    }
    let tok = acquire(&dir);
    let o = panio(
        &dir,
        &["lease", "renew", "dut", "--duration", "25h"],
        Some(&tok),
    );
    assert_eq!(o.status.code(), Some(2), "{o:?}");
}

const FULL_LAB: &str = r#"
[targets.dut]

[[targets.dut.serial]]
name = "console"
device = "/dev/ttyUSB-test"
baud = 115200
power_button = true

[targets.dut.power]
cycle_cmd = "true"
on_cmd = "true"
off_cmd = "true"
state_cmd = "echo on"

[targets.dut.video]
device = "0"

[targets.dut.hid]
cmd = "true"

[targets.dut.usb]
cmd = "true"

[targets.dut.adb]
serial = "ABC123"

[targets.dut.actions.reflash]
cmd = "true"

[targets.dut.netboot]
interface = "lo-test0"
tftp_root = "/nonexistent-tftp"
"#;

const REQUIRED_LAB: &str = r#"
[targets.dut]
lease_required = true

[targets.dut.power]
cycle_cmd = "true"
on_cmd = "true"
off_cmd = "true"
state_cmd = "echo on"
"#;

fn argv(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

/// One argv per mutating row of the spec table (`notes/target-leases.md`).
const MUTATING: &[&str] = &[
    "serial connect dut",
    "serial watch dut",
    "serial stop dut",
    "serial send dut hello",
    "serial transact -t dut --until x hello",
    "serial dtr dut",
    "serial reset dut",
    "video watch dut",
    "video stop dut",
    "video preview dut",
    "netboot start dut",
    "netboot stop dut",
    "netif mode off dut",
    "netif down-hard dut",
    "power on dut",
    "power off dut",
    "power-cycle dut",
    "usb attach-host -t dut",
    "usb attach-target -t dut",
    "action run -t dut reflash",
    "adb shell dut",
    "adb run -t dut devices",
    "adb input -t dut tap 1 1",
    "hid send -t dut x",
    "hid serve dut",
    "hid stop dut",
    "console dut",
];

/// One argv per read row.
const READS: &[&str] = &[
    "serial log dut",
    "serial evidence dut",
    "serial status dut",
    "serial show dut",
    "video show dut",
    "netboot status dut",
    "netboot logs dut",
    "netboot tftp-root dut",
    "netif status dut",
    "power-state dut",
    "usb state -t dut",
    "action list -t dut",
    "adb show dut",
    "doctor dut",
];

fn lease_path(dir: &Path) -> PathBuf {
    dir.join("home/.local/share/panio/dut/lease.json")
}

fn history(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("home/.local/share/panio/dut/lease-history.jsonl"))
        .unwrap_or_default()
}

fn write_lease(dir: &Path, token: &str, expires_at: u64, last_used_at: u64) {
    let p = lease_path(dir);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let rec = serde_json::json!({
        "owner": "alice@bench1",
        "token": token,
        "acquired_at": 1000,
        "expires_at": expires_at,
        "duration_secs": 7200,
        "idle_secs": 1800,
        "last_used_at": last_used_at,
        "procs": [],
    });
    std::fs::write(p, rec.to_string()).unwrap();
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn every_mutating_command_is_refused() {
    let dir = scratch("every_mutating_refused", FULL_LAB);
    write_lease(&dir, "someone-elses-token", now() + 3600, now());
    let mut bad = Vec::new();
    for line in MUTATING {
        let o = panio(&dir, &argv(line), Some("not-the-token"));
        if o.status.code() != Some(23) {
            bad.push(format!("{line}: {:?} {}", o.status.code(), stderr(&o)));
        }
        let o = panio(&dir, &argv(line), None);
        if o.status.code() != Some(23) {
            bad.push(format!("{line} (no token): {:?}", o.status.code()));
        }
    }
    assert!(bad.is_empty(), "not refused with 23:\n{}", bad.join("\n"));
}

#[test]
fn reads_run_on_a_leased_target() {
    let dir = scratch("reads_run_on_leased", FULL_LAB);
    write_lease(&dir, "someone-elses-token", now() + 3600, now());
    let mut bad = Vec::new();
    for line in READS {
        let o = panio(&dir, &argv(line), None);
        if o.status.code() == Some(23) {
            bad.push(format!("{line}: {}", stderr(&o)));
        }
    }
    assert!(bad.is_empty(), "refused reads:\n{}", bad.join("\n"));
}

#[test]
fn holder_runs_mutating() {
    let dir = scratch("holder_runs_mutating", FULL_LAB);
    write_lease(&dir, "mine", now() + 3600, now() - 600);
    let before = now() - 600;
    let o = panio(&dir, &["power", "on", "dut"], Some("mine"));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lease_path(&dir)).unwrap()).unwrap();
    assert!(v["last_used_at"].as_u64().unwrap() > before + 300, "{v}");
    assert_eq!(v["procs"].as_array().unwrap().len(), 1, "{v}");
}

#[test]
fn read_with_holder_token_touches_but_does_not_register() {
    let dir = scratch("read_touches", FULL_LAB);
    write_lease(&dir, "mine", now() + 3600, now() - 600);
    let o = panio(&dir, &["power-state", "dut"], Some("mine"));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lease_path(&dir)).unwrap()).unwrap();
    assert!(v["last_used_at"].as_u64().unwrap() > now() - 300, "{v}");
    assert!(v["procs"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn unleased_lab_unchanged() {
    let dir = scratch("unleased_unchanged", FULL_LAB);
    let o = panio(&dir, &["power", "on", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(!lease_path(&dir).exists());
    let state = dir.join("home/.local/share/panio/dut");
    assert!(
        !state.join("lease.lock").exists(),
        "no lock file for an unleased target"
    );
    let o = panio(&dir, &["power-state", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(!state.join("lease.lock").exists());
}

#[test]
fn lease_required_refuses_unleased() {
    let dir = scratch("lease_required_refuses", REQUIRED_LAB);
    let o = panio(&dir, &["power", "on", "dut"], None);
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    assert!(stderr(&o).contains("lease_required"), "{}", stderr(&o));
    let o = panio(&dir, &["power-state", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let tok = acquire(&dir);
    let o = panio(&dir, &["power", "on", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
}

#[test]
fn expired_lease_cleaned_by_mutating_not_by_read() {
    let dir = scratch("expired_cleaned_by_mutating", FULL_LAB);
    write_lease(&dir, "old", now() - 10, now() - 20);
    let o = panio(&dir, &["power-state", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(lease_path(&dir).exists(), "a read must not clean up");
    assert!(history(&dir).is_empty());
    let o = panio(&dir, &["power", "on", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(!lease_path(&dir).exists());
    assert!(history(&dir).contains("\"expired\""), "{}", history(&dir));
    // Lazy expiry that cleans up cleanly clears the record entirely. (The
    // failing-cleanup twin is `lazy_expiry_during_a_mutation_blocks_and_the_
    // mutation_never_runs`: it used to run the mutation anyway.)
    assert!(
        history(&dir).contains("\"cleanup_errors\":[]"),
        "{}",
        history(&dir)
    );
}

#[test]
fn forced_release_message_when_no_new_owner() {
    let dir = scratch("forced_release_message", FULL_LAB);
    let old = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut", "--preempt"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let _ = panio(
        &dir,
        &["lease", "acquire", "dut", "--owner", "bob@bench1"],
        None,
    );
    let o = panio(&dir, &["power", "on", "dut"], Some(&old));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    assert!(stderr(&o).contains("forced release"), "{}", stderr(&o));
}

#[test]
fn lease_file_is_read_and_deleted() {
    let dir = scratch("lease_file_is_read_and_deleted", REQUIRED_LAB);
    let tok = acquire(&dir);
    let file = dir.join("t");
    std::fs::write(&file, format!("{tok}\n")).unwrap();
    let o = panio(
        &dir,
        &["--lease-file", file.to_str().unwrap(), "power", "on", "dut"],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!file.exists(), "token file must be deleted");
    let o = panio(
        &dir,
        &[
            "--lease-file",
            dir.join("missing").to_str().unwrap(),
            "lease",
            "status",
            "dut",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(109), "{}", stderr(&o));
}

#[test]
fn lease_file_token_reaches_hooks_and_empty_file_is_refused() {
    let name = "lease_file_token_reaches_hooks";
    let dir = scratch(name, REQUIRED_LAB);
    let out = dir.join("seen");
    let lab = REQUIRED_LAB.replace(
        "on_cmd = \"true\"",
        &format!(
            "on_cmd = \"printf %s \\\"$PANIO_LEASE\\\" > {}\"",
            out.display()
        ),
    );
    std::fs::write(dir.join("lab.toml"), lab).unwrap();
    let tok = acquire(&dir);
    let file = dir.join("t");
    std::fs::write(&file, &tok).unwrap();
    let o = panio(
        &dir,
        &["--lease-file", file.to_str().unwrap(), "power", "on", "dut"],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let got = std::fs::read_to_string(&out).unwrap();
    assert!(got == tok, "hook did not see the lease token");

    std::fs::write(&file, "  \n").unwrap();
    let o = panio(
        &dir,
        &["--lease-file", file.to_str().unwrap(), "power", "on", "dut"],
        Some(&tok),
    );
    assert_eq!(o.status.code(), Some(109), "{}", stderr(&o));
}

#[test]
fn ended_lease_token_refuses_mutating() {
    let dir = scratch("ended_token_refuses", FULL_LAB);
    let tok = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["power", "on", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    let err = stderr(&o);
    assert!(err.contains("released"), "{err}");
    assert!(err.contains("PANIO_LEASE"), "{err}");
    assert!(!err.contains(&tok), "token must not be printed: {err}");
}

#[test]
fn expired_lease_token_refuses_after_cleanup() {
    let dir = scratch("expired_token_refuses", FULL_LAB);
    write_lease(&dir, "old-token", now() - 10, now() - 20);
    let o = panio(&dir, &["power", "on", "dut"], Some("old-token"));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    assert!(stderr(&o).contains("expired"), "{}", stderr(&o));
    assert!(!lease_path(&dir).exists());
    assert!(history(&dir).contains("\"expired\""), "{}", history(&dir));
}

#[test]
fn force_released_token_is_told() {
    let dir = scratch("force_released_told", FULL_LAB);
    let old = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut", "--preempt"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["power", "on", "dut"], Some(&old));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    assert!(stderr(&o).contains("forced release"), "{}", stderr(&o));
}

#[test]
fn ended_lease_token_warns_on_read() {
    let dir = scratch("ended_token_warns_read", FULL_LAB);
    let tok = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["power-state", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let err = stderr(&o);
    assert!(err.contains("warning:"), "{err}");
    assert!(err.contains("released"), "{err}");
    assert!(!err.contains(&tok), "{err}");
    assert!(!stdout(&o).contains("warning:"), "{}", stdout(&o));
}

#[test]
fn ended_token_sees_new_holder() {
    let dir = scratch("ended_token_new_holder", FULL_LAB);
    let a = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], Some(&a));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(
        &dir,
        &["lease", "acquire", "dut", "--owner", "bob@bench1"],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["power", "on", "dut"], Some(&a));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    let err = stderr(&o);
    assert!(err.contains("released"), "{err}");
    assert!(err.contains("bob@bench1"), "{err}");
}

#[test]
fn unknown_token_warns_and_runs() {
    let dir = scratch("unknown_token_warns", FULL_LAB);
    for args in [&["power", "on", "dut"][..], &["power-state", "dut"][..]] {
        let o = panio(&dir, args, Some("never-a-lease-here"));
        assert_eq!(o.status.code(), Some(0), "{args:?} {o:?}");
        assert!(
            stderr(&o).contains("warning: PANIO_LEASE is set but 'dut' is not leased"),
            "{}",
            stderr(&o)
        );
    }
    assert!(!lease_path(&dir).exists());
}

#[test]
fn ended_token_can_reacquire() {
    let dir = scratch("ended_token_reacquire", FULL_LAB);
    let tok = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["lease", "acquire", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let new = stdout(&o).trim().to_string();
    assert_eq!(new.len(), 32);
    assert_ne!(new, tok);
}

#[test]
fn preempted_holders_read_warns_and_runs() {
    let dir = scratch("preempted_read_warns", FULL_LAB);
    let old = acquire(&dir);
    let o = panio(
        &dir,
        &[
            "lease",
            "acquire",
            "dut",
            "--preempt",
            "--owner",
            "bob@bench1",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["power-state", "dut"], Some(&old));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let err = stderr(&o);
    assert!(err.contains("warning:"), "{err}");
    assert!(err.contains("preempted by bob@bench1"), "{err}");
    assert!(!err.contains(&old), "{err}");
    assert!(!stdout(&o).contains("warning:"), "{}", stdout(&o));
}

#[test]
fn foreign_token_read_names_the_holder() {
    let dir = scratch("foreign_token_read", FULL_LAB);
    write_lease(&dir, "alices-token", now() + 3600, now());
    let o = panio(&dir, &["power-state", "dut"], Some("some-other-token"));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let err = stderr(&o);
    assert!(
        err.contains("warning: PANIO_LEASE does not match the current holder (alice@bench1)"),
        "{err}"
    );
    assert!(!err.contains("some-other-token"), "{err}");
    let o = panio(&dir, &["power-state", "dut"], None);
    assert!(!stderr(&o).contains("warning:"), "{}", stderr(&o));
}

#[test]
fn release_after_release_succeeds_with_the_history() {
    let dir = scratch("release_twice", FULL_LAB);
    let tok = acquire(&dir);
    let o = panio(&dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stderr(&o).contains("released"), "{}", stderr(&o));
    assert!(!stderr(&o).contains(&tok), "{}", stderr(&o));
}

#[test]
fn release_by_a_preempted_holder_leaves_the_new_holder() {
    let dir = scratch("release_preempted", FULL_LAB);
    let old = acquire(&dir);
    let o = panio(
        &dir,
        &[
            "lease",
            "acquire",
            "dut",
            "--preempt",
            "--owner",
            "bob@bench1",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(&dir, &["lease", "release", "dut"], Some(&old));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(
        stderr(&o).contains("preempted by bob@bench1"),
        "{}",
        stderr(&o)
    );
    let s = panio(&dir, &["lease", "status", "dut", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&stdout(&s)).unwrap();
    assert_eq!(v["held"], true, "bob's lease must survive: {v}");
    assert_eq!(v["owner"], "bob@bench1", "{v}");
}

#[test]
fn release_of_an_unleased_target_is_not_an_error() {
    let dir = scratch("release_unleased", FULL_LAB);
    let o = panio(&dir, &["lease", "release", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("'dut' is not leased"), "{}", stdout(&o));
    let o = panio(&dir, &["lease", "release", "dut"], Some("never-a-lease"));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
}

#[test]
fn renew_of_an_expired_lease_retires_it() {
    let dir = scratch("renew_expired", FULL_LAB);
    write_lease(&dir, "old-token", now() - 10, now() - 20);
    let o = panio(&dir, &["lease", "renew", "dut"], Some("old-token"));
    assert_eq!(o.status.code(), Some(23), "{o:?}");
    let err = stderr(&o);
    assert!(err.contains("found expired"), "{err}");
    assert!(!err.contains("old-token"), "{err}");
    assert!(!lease_path(&dir).exists(), "the expired lease is retired");
    assert!(history(&dir).contains("\"expired\""), "{}", history(&dir));
}

#[test]
fn status_json_carries_the_idle_timeout() {
    let dir = scratch("status_idle_secs", FULL_LAB);
    acquire(&dir);
    let s = panio(&dir, &["lease", "status", "dut", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&stdout(&s)).unwrap();
    assert_eq!(v["idle_secs"], 1800, "{v}");
}

#[test]
fn console_does_not_register_its_pid_under_the_lease() {
    let dir = scratch("console_not_registered", LAB);
    write_lease(&dir, "mine", now() + 3600, now());
    // LAB has no video channel, so the console fails right after the check.
    let o = panio(&dir, &["console", "dut"], Some("mine"));
    assert_ne!(o.status.code(), Some(0), "{o:?}");
    assert_ne!(
        o.status.code(),
        Some(23),
        "the holder is not refused: {o:?}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lease_path(&dir)).unwrap()).unwrap();
    assert!(v["procs"].as_array().unwrap().is_empty(), "{v}");
    let o = panio(&dir, &["serial", "stop", "dut"], Some("mine"));
    assert_ne!(o.status.code(), Some(23), "{o:?}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lease_path(&dir)).unwrap()).unwrap();
    assert_eq!(
        v["procs"].as_array().unwrap().len(),
        1,
        "other commands still register: {v}"
    );
}

// ---- cleanup block (#372) ----
//
// A target whose hid helper cannot be stopped: the lab's hid command is a
// script that fails while `fail-hid` exists, and a live `sleep` stands in for
// the hid daemon (its discovery file is what makes the hid step run).

/// A scratch target with a failing hid stop hook and a marker-writing power
/// hook. Dropping it kills the stand-in daemon.
struct Blockable {
    dir: PathBuf,
    daemon: std::process::Child,
}

impl Drop for Blockable {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl Blockable {
    fn new(name: &str) -> Blockable {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = scratch(name, "");
        let child = Command::new("sleep").arg("300").spawn().unwrap();
        let b = Blockable { dir, daemon: child };
        b.write_lab("");
        let run = b.dir.join("run");
        let uid = run.metadata().unwrap().uid();
        let base = run.join(format!("panio-{uid}"));
        let rt = base.join("hid/dut");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let disc = rt.join("daemon.json");
        std::fs::write(
            &disc,
            format!("{{\"pid\":{},\"port\":12345}}", b.daemon.id()),
        )
        .unwrap();
        std::fs::set_permissions(&disc, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(b.dir.join("fail-hid"), "").unwrap();
        b
    }

    fn write_lab(&self, extra_hid: &str) {
        let d = self.dir.display();
        let lab = format!(
            "[targets.dut]\n\n[targets.dut.power]\ncycle_cmd = \"true\"\non_cmd = \"touch '{d}/ran'\"\n\
             off_cmd = \"true\"\nstate_cmd = \"echo on\"\n\n[targets.dut.hid]\n\
             cmd = \"sh '{d}/hid.sh'\"\n{extra_hid}\n[targets.dut.netboot]\ntftp_root = \"/nonexistent-tftp\"\n"
        );
        std::fs::write(self.dir.join("lab.toml"), lab).unwrap();
        std::fs::write(
            self.dir.join("hid.sh"),
            format!("echo x >> '{d}/stops'\n[ -f '{d}/fail-hid' ] && exit 1\nexit 0\n"),
        )
        .unwrap();
    }

    /// Make the hid stop succeed, and the daemon look already stopped.
    fn fix(&self) {
        let _ = std::fs::remove_file(self.dir.join("fail-hid"));
        let _ = std::fs::remove_dir_all(
            self.dir
                .join("run")
                .join(format!("panio-{}", self.uid()))
                .join("hid"),
        );
    }

    fn uid(&self) -> u32 {
        use std::os::unix::fs::MetadataExt;
        self.dir.join("run").metadata().unwrap().uid()
    }

    fn stops(&self) -> usize {
        std::fs::read_to_string(self.dir.join("stops"))
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    fn ran(&self) -> bool {
        self.dir.join("ran").exists()
    }

    fn status(&self) -> serde_json::Value {
        let o = panio(&self.dir, &["lease", "status", "dut", "--json"], None);
        assert_eq!(o.status.code(), Some(0), "{o:?}");
        serde_json::from_str(&stdout(&o)).unwrap()
    }

    /// Acquire, then release with the token: the lease ends, cleanup fails,
    /// and the target is blocked. Returns the departed token.
    fn block_by_release(&self) -> String {
        let tok = acquire(&self.dir);
        let o = panio(&self.dir, &["lease", "release", "dut"], Some(&tok));
        assert_eq!(o.status.code(), Some(24), "{o:?}");
        tok
    }

    fn cleanup_id(&self) -> String {
        self.status()["cleanup"]["cleanup_id"]
            .as_str()
            .expect("a cleanup id")
            .to_string()
    }

    fn assert_blocked(&self) {
        let s = self.status();
        assert_eq!(s["state"], "blocked", "{s}");
        assert_eq!(s["held"], false, "{s}");
    }
}

fn assert_blocked_refusal(o: &Output) {
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(stderr(o).contains("cleanup-blocked"), "{}", stderr(o));
    assert!(
        stderr(o).contains("lease cleanup dut --id"),
        "{}",
        stderr(o)
    );
}

#[test]
fn release_with_a_failing_cleanup_blocks_the_target() {
    let b = Blockable::new("block_release");
    let tok = acquire(&b.dir);
    let o = panio(&b.dir, &["lease", "release", "dut"], Some(&tok));
    assert_blocked_refusal(&o);
    assert!(stderr(&o).contains("hid_daemon"), "{}", stderr(&o));
    let s = b.status();
    assert_eq!(s["state"], "blocked", "{s}");
    assert_eq!(s["cleanup"]["phase"], "failed", "{s}");
    assert_eq!(s["cleanup"]["how"], "released", "{s}");
    let steps: Vec<(&str, &str)> = s["cleanup"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["step"].as_str().unwrap(), r["status"].as_str().unwrap()))
        .collect();
    // A failure does not stop the later steps: netboot_session still ran.
    assert_eq!(
        steps,
        vec![
            ("registered_processes", "already_satisfied"),
            ("hid_daemon", "failed"),
            ("netboot_session", "already_satisfied"),
        ],
        "{s}"
    );
    assert!(!s.to_string().contains(&tok), "status leaked a token: {s}");
    assert!(
        !s["cleanup"].to_string().contains("token"),
        "status shows no token field: {s}"
    );
    // The record has an owner nobody can name, for CLIs that predate the block.
    let rec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lease_path(&b.dir)).unwrap()).unwrap();
    assert!(
        rec["owner"]
            .as_str()
            .unwrap()
            .starts_with("cleanup-blocked"),
        "{rec}"
    );
    assert_ne!(rec["token"], tok.as_str(), "{rec}");
    let h = history(&b.dir);
    assert!(h.contains("hid_daemon"), "{h}");
}

#[test]
fn expired_acquire_does_not_hand_out_a_blocked_target() {
    let b = Blockable::new("block_expired_acquire");
    write_lease(&b.dir, "old", now() - 10, now() - 20);
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_blocked_refusal(&o);
    assert_eq!(stdout(&o).trim(), "", "no token on a refusal: {o:?}");
    b.assert_blocked();
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_blocked_refusal(&o);
}

#[test]
fn preempt_does_not_hand_out_a_blocked_target() {
    let b = Blockable::new("block_preempt");
    let _holder = acquire(&b.dir);
    let o = panio(
        &b.dir,
        &[
            "lease",
            "acquire",
            "dut",
            "--preempt",
            "--owner",
            "bob@bench1",
        ],
        None,
    );
    assert_blocked_refusal(&o);
    assert_eq!(stdout(&o).trim(), "", "{o:?}");
    b.assert_blocked();
    let again = panio(
        &b.dir,
        &["lease", "acquire", "dut", "--preempt", "--wait", "5s"],
        None,
    );
    assert_blocked_refusal(&again);
}

#[test]
fn acquire_after_a_failed_release_is_refused() {
    let b = Blockable::new("block_release_then_acquire");
    b.block_by_release();
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_blocked_refusal(&o);
    let t0 = Instant::now();
    let o = panio(&b.dir, &["lease", "acquire", "dut", "--wait", "30s"], None);
    assert_blocked_refusal(&o);
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "--wait must fail at once on a block"
    );
}

#[test]
fn lazy_expiry_during_a_mutation_blocks_and_the_mutation_never_runs() {
    let b = Blockable::new("block_lazy_expiry");
    write_lease(&b.dir, "old", now() - 10, now() - 20);
    let o = panio(&b.dir, &["power", "on", "dut"], None);
    assert_blocked_refusal(&o);
    assert!(!b.ran(), "the mutation must not have run");
    b.assert_blocked();
    let o = panio(&b.dir, &["power", "on", "dut"], None);
    assert_blocked_refusal(&o);
    assert!(!b.ran());
}

#[test]
fn the_departed_token_does_not_get_through_a_block() {
    let b = Blockable::new("block_departed_token");
    let tok = b.block_by_release();
    let o = panio(&b.dir, &["power", "on", "dut"], Some(&tok));
    assert_blocked_refusal(&o);
    assert!(!b.ran());
    for args in [
        &["lease", "renew", "dut"][..],
        &["lease", "release", "dut"][..],
    ] {
        let o = panio(&b.dir, args, Some(&tok));
        assert_blocked_refusal(&o);
    }
    let o = panio(&b.dir, &["lease", "release", "dut", "--preempt"], None);
    assert_blocked_refusal(&o);
}

#[test]
fn a_block_binds_a_target_that_does_not_require_leases_and_one_that_does() {
    let b = Blockable::new("block_regardless_of_required");
    b.block_by_release();
    let o = panio(&b.dir, &["power", "on", "dut"], None);
    assert_blocked_refusal(&o);
    let mut lab = std::fs::read_to_string(b.dir.join("lab.toml")).unwrap();
    lab = lab.replace("[targets.dut]\n", "[targets.dut]\nlease_required = true\n");
    std::fs::write(b.dir.join("lab.toml"), lab).unwrap();
    let o = panio(&b.dir, &["power", "on", "dut"], None);
    assert_blocked_refusal(&o);
    assert!(!b.ran());
}

#[test]
fn reads_run_on_a_blocked_target_with_a_warning() {
    let b = Blockable::new("block_reads");
    let tok = b.block_by_release();
    let o = panio(&b.dir, &["power-state", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stderr(&o).contains("cleanup-blocked"), "{}", stderr(&o));
    let stops = b.stops();
    let o = panio(&b.dir, &["power-state", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(b.stops(), stops, "a read must not retry cleanup");
}

#[test]
fn status_triggered_expiry_cleans_up_once_then_only_reports() {
    let b = Blockable::new("block_status_expiry");
    write_lease(&b.dir, "old", now() - 10, now() - 20);
    let s = b.status();
    assert_eq!(s["state"], "blocked", "{s}");
    assert_eq!(s["held"], false, "{s}");
    let stops = b.stops();
    assert_eq!(stops, 1, "cleanup ran once");
    let id = b.cleanup_id();
    let text = panio(&b.dir, &["lease", "status", "dut"], None);
    assert_eq!(text.status.code(), Some(0), "{text:?}");
    let t = stdout(&text);
    assert!(
        t.contains("blocked: cleanup failed (hid_daemon)")
            && t.contains(&format!("cleanup id {id}"))
            && t.contains(&format!("panio lease cleanup dut --id {id}")),
        "{t}"
    );
    assert_eq!(b.stops(), stops, "later status calls only read");
}

#[test]
fn expired_renew_blocks() {
    let b = Blockable::new("block_expired_renew");
    write_lease(&b.dir, "old", now() - 10, now() - 20);
    let o = panio(&b.dir, &["lease", "renew", "dut"], Some("old"));
    assert_blocked_refusal(&o);
    b.assert_blocked();
}

#[test]
fn daemon_operations_on_a_blocked_target_are_refused_or_skipped() {
    let b = Blockable::new("block_daemons");
    b.block_by_release();
    let o = panio(&b.dir, &["hid", "stop", "dut"], None);
    assert_blocked_refusal(&o);
    let o = panio(&b.dir, &["daemons", "stop", "hid[dut]"], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    let o = panio(&b.dir, &["daemons", "stop", "--all"], None);
    assert!(
        stderr(&o).contains("skipped hid/dut: target is cleanup-blocked"),
        "{}",
        stderr(&o)
    );
    assert!(
        process_alive(b.daemon.id()),
        "the blocked target's daemon must be left alone"
    );
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn block_is_visible_from_a_separate_process_and_retry_fails_then_succeeds() {
    let b = Blockable::new("block_retry");
    b.block_by_release();
    let id = b.cleanup_id();
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", &id], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(stderr(&o).contains(&id), "{}", stderr(&o));
    let s = b.status();
    assert_eq!(s["state"], "blocked", "{s}");
    assert_eq!(s["cleanup"]["cleanup_id"], id.as_str(), "the id is stable");
    assert_eq!(b.stops(), 2, "the retry re-ran the failing step");
    b.fix();
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", &id], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("verified"), "{}", stdout(&o));
    let s = b.status();
    assert_eq!(s["state"], "unleased", "{s}");
    let h = history(&b.dir);
    assert!(h.contains("\"resolution\":\"verified\""), "{h}");
    assert!(h.contains("\"how\":\"released\""), "{h}");
    assert!(h.contains(&id), "{h}");
    let tok = acquire(&b.dir);
    assert_eq!(tok.len(), 32);
}

#[test]
fn override_clears_the_block_and_audits_it() {
    let b = Blockable::new("block_override");
    b.block_by_release();
    let id = b.cleanup_id();
    for args in [
        vec!["lease", "override-cleanup", "dut", "--id", &id],
        vec![
            "lease",
            "override-cleanup",
            "dut",
            "--id",
            &id,
            "--reason",
            "  ",
        ],
    ] {
        let o = panio(&b.dir, &args, None);
        assert_eq!(o.status.code(), Some(2), "a reason is required: {o:?}");
        b.assert_blocked();
    }
    let o = panio(
        &b.dir,
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--id",
            &id,
            "--reason",
            "hid dongle unplugged",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("WITHOUT verifying"), "{}", stdout(&o));
    let h = history(&b.dir);
    assert!(h.contains("\"resolution\":\"overridden\""), "{h}");
    assert!(h.contains("hid dongle unplugged"), "{h}");
    assert!(h.contains("hid_daemon"), "the outstanding failure: {h}");
    assert!(h.contains(&id), "{h}");
    assert_eq!(b.status()["state"], "unleased");
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "override does not acquire: {o:?}");
}

#[test]
fn recovery_refuses_a_stale_or_wrong_generation() {
    let b = Blockable::new("block_stale_id");
    b.block_by_release();
    let old = b.cleanup_id();
    // Override, take the target, and let it block again under a new id.
    let o = panio(
        &b.dir,
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--id",
            &old,
            "--reason",
            "r",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let tok = acquire(&b.dir);
    // Held: nothing to recover.
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", &old], None);
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    assert!(stderr(&o).contains("not cleanup-blocked"), "{}", stderr(&o));
    assert_eq!(b.status()["state"], "held");
    let o = panio(
        &b.dir,
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--id",
            &old,
            "--reason",
            "r",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    // Blocked again under a different id.
    let o = panio(&b.dir, &["lease", "release", "dut"], Some(&tok));
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    let new = b.cleanup_id();
    assert_ne!(new, old);
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", &old], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(
        stderr(&o).contains("different cleanup id"),
        "{}",
        stderr(&o)
    );
    b.fix();
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", &old], None);
    assert_eq!(
        o.status.code(),
        Some(24),
        "the old id must not clear it: {o:?}"
    );
    b.assert_blocked();
}

#[test]
fn a_changed_cleanup_scope_forces_an_override() {
    let b = Blockable::new("block_scope");
    b.block_by_release();
    let id = b.cleanup_id();
    b.fix();
    b.write_lab("port = 4242\n");
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", &id], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(stderr(&o).contains("config changed"), "{}", stderr(&o));
    assert!(stderr(&o).contains("override-cleanup"), "{}", stderr(&o));
    b.assert_blocked();
    let o = panio(
        &b.dir,
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--id",
            &id,
            "--reason",
            "r",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
}

#[test]
fn an_interrupted_pending_record_still_blocks_other_processes() {
    let b = Blockable::new("block_pending");
    b.fix();
    let rec = serde_json::json!({
        "owner": "cleanup-blocked (was alice@bench1)",
        "token": "never-printed",
        "acquired_at": 1000,
        "expires_at": now() + 400_000_000,
        "duration_secs": 86400,
        "idle_secs": 86400,
        "last_used_at": now() + 400_000_000,
        "procs": [],
        "cleanup": {
            "cleanup_id": "abc123",
            "phase": "pending",
            "how": "expired",
            "ended_at": 1000,
            "previous_owner": "alice@bench1",
            "previous_token": "old",
            "new_owner": null,
            "procs": [],
            "scope": null,
            "last_attempt_at": null,
            "results": [],
        },
    });
    std::fs::create_dir_all(lease_path(&b.dir).parent().unwrap()).unwrap();
    std::fs::write(lease_path(&b.dir), rec.to_string()).unwrap();
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(stderr(&o).contains("did not finish"), "{}", stderr(&o));
    let o = panio(&b.dir, &["power", "on", "dut"], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(!b.ran());
    let s = b.status();
    assert_eq!(s["cleanup"]["phase"], "pending", "{s}");
    let o = panio(&b.dir, &["lease", "cleanup", "dut", "--id", "abc123"], None);
    assert_eq!(
        o.status.code(),
        Some(24),
        "scope null differs from the lab: {o:?}"
    );
}

#[test]
fn a_history_write_failure_leaves_the_target_blocked_and_mints_no_token() {
    let b = Blockable::new("block_history_failure");
    b.fix();
    write_lease(&b.dir, "old", now() - 10, now() - 20);
    // A directory where the history file belongs: the append cannot succeed.
    let hist = b
        .dir
        .join("home/.local/share/panio/dut/lease-history.jsonl");
    std::fs::create_dir_all(hist.join("x")).unwrap();
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_ne!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stdout(&o).trim(), "", "no token: {o:?}");
    assert!(lease_path(&b.dir).exists(), "the record must survive");
    let rec = std::fs::read_to_string(lease_path(&b.dir)).unwrap();
    assert!(rec.contains("\"cleanup\""), "still blocked: {rec}");
    let o = panio(&b.dir, &["power", "on", "dut"], None);
    assert_eq!(o.status.code(), Some(24), "{o:?}");
    assert!(!b.ran());
}

#[test]
fn a_corrupt_record_refuses_and_shows_unknown_until_moved_aside() {
    let b = Blockable::new("block_corrupt");
    let p = lease_path(&b.dir);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "{not json").unwrap();
    for args in [
        &["lease", "acquire", "dut"][..],
        &["lease", "renew", "dut"][..],
        &["lease", "release", "dut"][..],
        &["power", "on", "dut"][..],
    ] {
        let o = panio(&b.dir, args, None);
        assert_eq!(o.status.code(), Some(24), "{args:?}: {o:?}");
        assert!(stderr(&o).contains("--corrupt"), "{}", stderr(&o));
    }
    assert!(!b.ran());
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
    let s = b.status();
    assert_eq!(s["state"], "blocked", "{s}");
    assert!(s["error"].as_str().unwrap().contains("unreadable"), "{s}");
    let o = panio(&b.dir, &["power-state", "dut"], Some("t"));
    assert_eq!(o.status.code(), Some(0), "reads run: {o:?}");
    let o = panio(
        &b.dir,
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--corrupt",
            "--reason",
            "test",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(!p.exists());
    let moved: Vec<_> = std::fs::read_dir(p.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("lease.json.corrupt-")
        })
        .collect();
    assert_eq!(moved.len(), 1);
    assert!(history(&b.dir).contains("\"how\":\"corrupt\""));
    let o = panio(&b.dir, &["lease", "acquire", "dut"], None);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = panio(
        &b.dir,
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--corrupt",
            "--reason",
            "x",
        ],
        None,
    );
    assert_eq!(
        o.status.code(),
        Some(2),
        "a readable record is not corrupt: {o:?}"
    );
}

#[test]
fn override_takes_exactly_one_of_id_and_corrupt() {
    let b = Blockable::new("block_override_args");
    for args in [
        &["lease", "override-cleanup", "dut", "--reason", "r"][..],
        &[
            "lease",
            "override-cleanup",
            "dut",
            "--id",
            "a",
            "--corrupt",
            "--reason",
            "r",
        ][..],
        &["lease", "override-cleanup", "dut", "--corrupt"][..],
        &["lease", "cleanup", "dut"][..],
    ] {
        let o = panio(&b.dir, args, None);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {o:?}");
    }
}
