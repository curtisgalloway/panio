// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! `panio config sync` end to end against the built binary and a real git
//! source repository (#375): a synced lab refuses edits with a usage error,
//! reports its source, and becomes editable again after `--detach`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("lab_sync")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("lab.toml"), "[targets.dut]\n").unwrap();
    git(&src, &["add", "lab.toml"]);
    git(&src, &["commit", "-q", "-m", "lab"]);
    dir
}

fn panio(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_panio"))
        .stdin(std::process::Stdio::null())
        .arg("--lab")
        .arg(dir.join("home/lab.toml"))
        .args(args)
        .env("HOME", dir.join("home"))
        .env_remove("PANIO_JSON_ERRORS")
        .env_remove("PANIO_LAB")
        .env_remove("PANIO_LEASE")
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn a_synced_lab_is_read_only_until_detached() {
    let dir = scratch("read_only");
    let src = dir.join("src");
    let src = src.to_str().unwrap();

    let out = panio(
        &dir,
        &["config", "sync", "--git", src, "--path", "lab.toml"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains("Synced"), "{out:?}");
    assert!(dir.join("home/lab.source.toml").exists());

    let out = panio(&dir, &["target", "add", "nuc"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(text(&out.stderr).contains("read-only"), "{out:?}");

    let out = panio(&dir, &["config", "show"]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains("synced from"), "{out:?}");

    let out = panio(&dir, &["config", "sync"]);
    assert!(text(&out.stdout).contains("up to date"), "{out:?}");

    let out = panio(&dir, &["config", "sync", "--detach"]);
    assert!(out.status.success(), "{out:?}");
    let out = panio(&dir, &["target", "add", "nuc"]);
    assert!(out.status.success(), "{out:?}");
}
