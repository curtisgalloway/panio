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
    cmd.arg("--lab")
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
#[ignore = "needs Task 7 enforcement"]
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
