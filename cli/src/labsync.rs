// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! `panio config sync`: pull the lab file from a git repository and keep the
//! local copy read-only (#375).
//!
//! The lab stays a plain local file that every command reads as before;
//! nothing on the command path touches the network. Sync writes a *sync
//! record* beside it (`<stem>.source.toml`) naming the source, the commit the
//! copy came from, that file's git blob id, and when it was fetched. The
//! record's presence is what makes the lab read-only: `set`/`add`/`rm`,
//! `config edit` and `init` refuse while it exists, so a local edit cannot
//! silently diverge from the source. `--detach` deletes it.
//!
//! Git does the fetching (a depth-1 fetch into a throwaway repository), so
//! SSH remotes reuse the user's keys and there is no token story. The fetched
//! file is parsed and validated before it replaces anything, so a bad push
//! leaves the working copy in place.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::error::{Kind, PanioError};
use crate::model;

/// A synced lab older than this gets a warning on every command.
pub const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

const RECORD_HEADER: &str =
    "# Written by `panio config sync`: the lab beside this file is a copy of\n\
     # the source below and is read-only here. Edit the source and run\n\
     # `panio config sync`, or run `panio config sync --detach` to stop syncing.\n\n";

/// Where the lab comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Anything `git fetch` accepts: an SSH or HTTPS remote, or a local path.
    pub git: String,
    /// The lab file's path inside the repository.
    pub path: String,
    /// Branch, tag or commit; `None` is the remote's default branch.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.git, self.path)?;
        if let Some(r) = &self.git_ref {
            write!(f, " @ {r}")?;
        }
        Ok(())
    }
}

/// The sync record written beside a synced lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub source: Source,
    /// The commit the local copy was taken from.
    pub commit: String,
    /// Git blob id of the bytes written, to detect a hand edit.
    pub blob: String,
    /// Unix seconds of the last successful sync.
    pub fetched_at: u64,
}

/// The sync record path for the lab at `lab`: `lab.toml` → `lab.source.toml`.
pub fn record_path(lab: &Path) -> PathBuf {
    let stem = lab
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "lab".to_string());
    lab.with_file_name(format!("{stem}.source.toml"))
}

/// Read the sync record for `lab`; `Ok(None)` when the lab is not synced.
pub fn read_record(lab: &Path) -> Result<Option<Record>> {
    let path = record_path(lab);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    let rec = toml::from_str(&text).map_err(|e| {
        PanioError::not_configured(format!(
            "{}: unreadable sync record ({e}); run `panio config sync --force` to \
             rewrite it, or `panio config sync --detach` to stop syncing",
            path.display()
        ))
    })?;
    Ok(Some(rec))
}

/// Refuse to modify a synced lab. Called before every lab write.
pub fn ensure_writable(lab: &Path) -> Result<()> {
    if !record_path(lab).exists() {
        return Ok(());
    }
    let from = match read_record(lab) {
        Ok(Some(rec)) => format!(" from {}", rec.source),
        _ => String::new(),
    };
    Err(PanioError::new(
        Kind::Usage,
        format!(
            "{} is synced{from} and read-only here. Edit the source and run \
             `panio config sync`, or run `panio config sync --detach` to edit it locally.",
            lab.display()
        ),
    )
    .into())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `3 days`, `5 hours`, `12 minutes`.
pub fn format_age(secs: u64) -> String {
    let (n, unit) = if secs >= 86_400 {
        (secs / 86_400, "day")
    } else if secs >= 3_600 {
        (secs / 3_600, "hour")
    } else {
        (secs / 60, "minute")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// The first 12 hex digits of a commit id, for messages.
pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// The warning to print when the synced lab at `lab` is stale (or its record
/// is unreadable), as of `now`; `None` for a fresh or unsynced lab.
pub fn stale_warning(lab: &Path, now: u64) -> Option<String> {
    match read_record(lab) {
        Ok(None) => None,
        Err(e) => Some(format!("{e:#}")),
        Ok(Some(rec)) => {
            let age = now.saturating_sub(rec.fetched_at);
            (age > STALE_AFTER_SECS).then(|| {
                format!(
                    "lab was last synced {} ago (from {}, commit {}); run `panio config sync`",
                    format_age(age),
                    rec.source,
                    short(&rec.commit)
                )
            })
        }
    }
}

/// Print [`stale_warning`] for `lab` to stderr, if there is one.
pub fn warn_if_stale(lab: &Path) {
    if let Some(w) = stale_warning(lab, now_secs()) {
        eprintln!("panio: warning: {w}");
    }
}

/// One line for `config show` and `doctor`: where a synced lab came from,
/// at which commit, how long ago, and whether it was edited since.
pub fn describe(lab: &Path) -> Result<Option<String>> {
    let Some(rec) = read_record(lab)? else {
        return Ok(None);
    };
    let age = format_age(now_secs().saturating_sub(rec.fetched_at));
    let edited = match blob_id(lab) {
        Ok(b) if b == rec.blob => "",
        Ok(_) => "; EDITED LOCALLY since the last sync",
        Err(_) => "; could not check for local edits (is git installed?)",
    };
    Ok(Some(format!(
        "synced from {}, commit {}, {age} ago{edited}",
        rec.source,
        short(&rec.commit)
    )))
}

/// True when `lab` has a sync record and its contents differ from what the
/// last sync wrote.
pub fn edited_locally(lab: &Path) -> bool {
    match read_record(lab) {
        Ok(Some(rec)) => blob_id(lab).map(|b| b != rec.blob).unwrap_or(false),
        _ => false,
    }
}

// ── git ─────────────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Never stop to ask for a password: a sync run from an agent or a
        // timer has no one to answer.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| -> anyhow::Error {
            if e.kind() == std::io::ErrorKind::NotFound {
                PanioError::not_configured("`panio config sync` needs git, which is not on PATH")
                    .into()
            } else {
                e.into()
            }
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn git_text(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git(dir, args)?).trim().to_string())
}

/// Git blob id of the file at `path`, without line-ending filters, so it
/// compares with the id of the blob it was written from.
fn blob_id(path: &Path) -> Result<String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().context("lab path has no file name")?;
    git_text(
        dir,
        &["hash-object", "--no-filters", &name.to_string_lossy()],
    )
}

struct Fetched {
    commit: String,
    blob: String,
    bytes: Vec<u8>,
}

/// Refuse a remote or ref that git would read as an option (`--upload-pack=…`
/// runs a program), whether it came from the command line or a record.
fn check_source(source: &Source) -> Result<()> {
    for (what, v) in [
        ("--git", Some(source.git.as_str())),
        ("--ref", source.git_ref.as_deref()),
    ] {
        if v.is_some_and(|v| v.is_empty() || v.starts_with('-')) {
            return Err(PanioError::new(
                Kind::Usage,
                format!("{what} must not be empty or start with '-'"),
            )
            .into());
        }
    }
    Ok(())
}

/// Fetch `source.path` at `source.git_ref` into a throwaway repository.
fn fetch(source: &Source) -> Result<Fetched> {
    check_source(source)?;
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    git(dir, &["init", "-q"])?;
    let want = source.git_ref.as_deref().unwrap_or("HEAD");
    git(
        dir,
        &[
            "-c",
            "protocol.ext.allow=never",
            "fetch",
            "-q",
            "--depth",
            "1",
            "--no-tags",
            "--",
            &source.git,
            want,
        ],
    )
    .map_err(|e| {
        PanioError::new(
            Kind::Unreachable,
            format!("could not fetch {want} from {}: {e:#}", source.git),
        )
    })?;
    let commit = git_text(dir, &["rev-parse", "FETCH_HEAD"])?;
    let spec = format!("FETCH_HEAD:{}", source.path);
    let blob = git_text(dir, &["rev-parse", "--verify", "-q", &spec]).map_err(|_| {
        PanioError::not_configured(format!(
            "{} has no file '{}' at commit {}",
            source.git,
            source.path,
            short(&commit)
        ))
    })?;
    let bytes = git(dir, &["cat-file", "blob", &blob])?;
    Ok(Fetched {
        commit,
        blob,
        bytes,
    })
}

// ── sync ────────────────────────────────────────────────────────────────────

/// What `config sync` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The lab was written (created, or changed to a new commit's contents).
    Updated { commit: String },
    /// The source matched the local copy; only the fetch time was refreshed.
    Unchanged { commit: String },
}

/// Sync the lab at `lab` from `source`, or from the recorded source when
/// `source` is `None`. `force` overwrites a hand-edited or never-synced lab.
pub fn sync(lab: &Path, source: Option<Source>, force: bool) -> Result<Outcome> {
    let record = if force {
        // A damaged record must not block the repair it suggests.
        read_record(lab).ok().flatten()
    } else {
        read_record(lab)?
    };
    let source = match (source, &record) {
        (Some(s), _) => s,
        (None, Some(rec)) => rec.source.clone(),
        (None, None) => {
            return Err(PanioError::new(
                Kind::Usage,
                format!(
                    "{} is not synced yet; name a source with \
                     `panio config sync --git <remote> --path <file>`",
                    lab.display()
                ),
            )
            .into())
        }
    };
    if let Ok(md) = std::fs::symlink_metadata(lab) {
        if md.file_type().is_symlink() {
            return Err(PanioError::new(
                Kind::Usage,
                format!(
                    "{} is a symlink (to {}); it already follows that file, and sync \
                     would write through it. Remove the link to sync a copy instead.",
                    lab.display(),
                    std::fs::read_link(lab)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "?".into())
                ),
            )
            .into());
        }
    }

    let fetched = fetch(&source)?;
    let text = std::str::from_utf8(&fetched.bytes).map_err(|_| {
        PanioError::not_configured(format!(
            "{} at commit {} is not UTF-8 text; {} was not changed",
            source.path,
            short(&fetched.commit),
            lab.display()
        ))
    })?;
    model::parse(text)
        .and_then(|l| model::validate_for_save(&l))
        .map_err(|e| {
            PanioError::not_configured(format!(
                "{} at commit {} is not a valid lab: {e}; {} was not changed",
                source.path,
                short(&fetched.commit),
                lab.display()
            ))
        })?;

    let exists = lab.exists();
    let unchanged = exists && std::fs::read(lab)? == fetched.bytes;
    if exists && !unchanged && !force {
        let why = match &record {
            Some(rec) if blob_id(lab)? != rec.blob => {
                "was edited locally since the last sync".to_string()
            }
            Some(_) => String::new(),
            None => "exists and was not synced before".to_string(),
        };
        if !why.is_empty() {
            return Err(PanioError::new(
                Kind::Usage,
                format!(
                    "{} {why}; syncing would discard those contents. Move the edits to \
                     the source, or rerun with --force to replace the file.",
                    lab.display()
                ),
            )
            .into());
        }
    }

    if !unchanged {
        if let Some(parent) = lab.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        crate::platform::write_atomic(lab, &fetched.bytes)
            .with_context(|| lab.display().to_string())?;
    }
    let rec = Record {
        source,
        commit: fetched.commit.clone(),
        blob: fetched.blob,
        fetched_at: now_secs(),
    };
    let body = format!("{RECORD_HEADER}{}", toml::to_string(&rec)?);
    let rpath = record_path(lab);
    crate::platform::write_atomic(&rpath, body.as_bytes())
        .with_context(|| rpath.display().to_string())?;
    Ok(if unchanged {
        Outcome::Unchanged {
            commit: fetched.commit,
        }
    } else {
        Outcome::Updated {
            commit: fetched.commit,
        }
    })
}

/// Stop syncing `lab`: delete its record. Returns false when it had none.
pub fn detach(lab: &Path) -> Result<bool> {
    let path = record_path(lab);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "[targets.pi5]\n";
    const GOOD2: &str = "[targets.pi5]\n\n[targets.nuc]\n";

    /// A source repository with `lab/lab.toml` committed, driven by real git.
    struct Repo {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        lab: PathBuf,
    }

    impl Repo {
        fn new(lab_text: &str) -> Repo {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("src");
            std::fs::create_dir_all(dir.join("lab")).unwrap();
            git(&dir, &["init", "-q", "-b", "main"]).unwrap();
            let r = Repo {
                lab: tmp.path().join("home/.config/panio/lab.toml"),
                _tmp: tmp,
                dir,
            };
            r.commit(lab_text);
            r
        }

        fn commit(&self, lab_text: &str) {
            std::fs::write(self.dir.join("lab/lab.toml"), lab_text).unwrap();
            git(&self.dir, &["add", "lab/lab.toml"]).unwrap();
            git(
                &self.dir,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "-q",
                    "-m",
                    "lab",
                ],
            )
            .unwrap();
        }

        fn source(&self) -> Source {
            Source {
                git: self.dir.to_string_lossy().into_owned(),
                path: "lab/lab.toml".into(),
                git_ref: None,
            }
        }

        fn local(&self) -> String {
            std::fs::read_to_string(&self.lab).unwrap()
        }
    }

    fn kind(e: &anyhow::Error) -> Kind {
        crate::error::classify(e).kind
    }

    #[test]
    fn record_sits_beside_the_lab() {
        assert_eq!(
            record_path(Path::new("/x/lab.toml")),
            PathBuf::from("/x/lab.source.toml")
        );
    }

    #[test]
    fn first_sync_writes_the_lab_and_a_record() {
        let r = Repo::new(GOOD);
        let out = sync(&r.lab, Some(r.source()), false).unwrap();
        assert!(matches!(out, Outcome::Updated { .. }));
        assert_eq!(r.local(), GOOD);
        let rec = read_record(&r.lab).unwrap().unwrap();
        assert_eq!(rec.source, r.source());
        assert_eq!(rec.blob, blob_id(&r.lab).unwrap());
        assert!(!edited_locally(&r.lab));
    }

    #[test]
    fn resync_uses_the_recorded_source_and_picks_up_new_commits() {
        let r = Repo::new(GOOD);
        sync(&r.lab, Some(r.source()), false).unwrap();
        assert!(matches!(
            sync(&r.lab, None, false).unwrap(),
            Outcome::Unchanged { .. }
        ));
        r.commit(GOOD2);
        assert!(matches!(
            sync(&r.lab, None, false).unwrap(),
            Outcome::Updated { .. }
        ));
        assert_eq!(r.local(), GOOD2);
    }

    #[test]
    fn an_invalid_push_leaves_the_working_copy_in_place() {
        let r = Repo::new(GOOD);
        sync(&r.lab, Some(r.source()), false).unwrap();
        let before = read_record(&r.lab).unwrap().unwrap();
        r.commit("[targets.pi5\n");
        let e = sync(&r.lab, None, false).unwrap_err();
        assert_eq!(kind(&e), Kind::NotConfigured);
        assert!(format!("{e:#}").contains("not a valid lab"), "{e:#}");
        assert_eq!(r.local(), GOOD);
        assert_eq!(read_record(&r.lab).unwrap().unwrap(), before);
    }

    #[test]
    fn a_hand_edit_blocks_sync_until_forced() {
        let r = Repo::new(GOOD);
        sync(&r.lab, Some(r.source()), false).unwrap();
        std::fs::write(&r.lab, GOOD2).unwrap();
        assert!(edited_locally(&r.lab));
        r.commit("[targets.other]\n");
        let e = sync(&r.lab, None, false).unwrap_err();
        assert_eq!(kind(&e), Kind::Usage);
        assert!(format!("{e:#}").contains("edited locally"), "{e:#}");
        assert_eq!(r.local(), GOOD2);
        sync(&r.lab, None, true).unwrap();
        assert_eq!(r.local(), "[targets.other]\n");
    }

    #[test]
    fn an_existing_unsynced_lab_is_not_replaced_without_force() {
        let r = Repo::new(GOOD);
        std::fs::create_dir_all(r.lab.parent().unwrap()).unwrap();
        std::fs::write(&r.lab, GOOD2).unwrap();
        let e = sync(&r.lab, Some(r.source()), false).unwrap_err();
        assert!(format!("{e:#}").contains("not synced before"), "{e:#}");
        assert_eq!(r.local(), GOOD2);
        assert!(read_record(&r.lab).unwrap().is_none());
        // Identical contents adopt the file without --force.
        std::fs::write(&r.lab, GOOD).unwrap();
        sync(&r.lab, Some(r.source()), false).unwrap();
        assert!(read_record(&r.lab).unwrap().is_some());
    }

    #[test]
    fn a_missing_file_or_ref_fails_without_writing() {
        let r = Repo::new(GOOD);
        let mut s = r.source();
        s.path = "nope.toml".into();
        let e = sync(&r.lab, Some(s), false).unwrap_err();
        assert_eq!(kind(&e), Kind::NotConfigured);
        let mut s = r.source();
        s.git_ref = Some("no-such-branch".into());
        let e = sync(&r.lab, Some(s), false).unwrap_err();
        assert_eq!(kind(&e), Kind::Unreachable);
        assert!(!r.lab.exists());
    }

    #[test]
    fn option_shaped_remotes_and_refs_are_refused() {
        let r = Repo::new(GOOD);
        let mut s = r.source();
        s.git = "--upload-pack=touch pwned".into();
        let e = sync(&r.lab, Some(s), false).unwrap_err();
        assert_eq!(kind(&e), Kind::Usage);
        let mut s = r.source();
        s.git_ref = Some("--output=x".into());
        assert_eq!(
            kind(&sync(&r.lab, Some(s), false).unwrap_err()),
            Kind::Usage
        );
        assert!(!r.lab.exists());
    }

    #[test]
    fn sync_without_a_source_or_record_is_a_usage_error() {
        let r = Repo::new(GOOD);
        let e = sync(&r.lab, None, false).unwrap_err();
        assert_eq!(kind(&e), Kind::Usage);
    }

    #[test]
    fn a_synced_lab_is_read_only_until_detached() {
        let r = Repo::new(GOOD);
        ensure_writable(&r.lab).unwrap();
        sync(&r.lab, Some(r.source()), false).unwrap();
        let e = ensure_writable(&r.lab).unwrap_err();
        assert_eq!(kind(&e), Kind::Usage);
        assert!(format!("{e:#}").contains("--detach"), "{e:#}");
        assert!(detach(&r.lab).unwrap());
        ensure_writable(&r.lab).unwrap();
        assert!(!detach(&r.lab).unwrap());
        assert_eq!(r.local(), GOOD);
    }

    #[test]
    fn stale_warning_fires_only_past_the_threshold() {
        let r = Repo::new(GOOD);
        assert!(stale_warning(&r.lab, now_secs()).is_none());
        sync(&r.lab, Some(r.source()), false).unwrap();
        let t = read_record(&r.lab).unwrap().unwrap().fetched_at;
        assert!(stale_warning(&r.lab, t + STALE_AFTER_SECS).is_none());
        let w = stale_warning(&r.lab, t + STALE_AFTER_SECS + 86_400).unwrap();
        assert!(w.contains("8 days ago"), "{w}");
    }

    #[test]
    fn an_unreadable_record_still_makes_the_lab_read_only() {
        let r = Repo::new(GOOD);
        sync(&r.lab, Some(r.source()), false).unwrap();
        std::fs::write(record_path(&r.lab), "garbage = [").unwrap();
        assert!(ensure_writable(&r.lab).is_err());
        assert!(stale_warning(&r.lab, now_secs()).is_some());
        assert!(sync(&r.lab, None, false).is_err());
        // --force with an explicit source repairs it.
        sync(&r.lab, Some(r.source()), true).unwrap();
        assert!(read_record(&r.lab).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_lab_is_refused() {
        let r = Repo::new(GOOD);
        let target = r.dir.join("lab/lab.toml");
        std::fs::create_dir_all(r.lab.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &r.lab).unwrap();
        let e = sync(&r.lab, Some(r.source()), true).unwrap_err();
        assert!(format!("{e:#}").contains("symlink"), "{e:#}");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(format_age(59), "0 minutes");
        assert_eq!(format_age(3_600), "1 hour");
        assert_eq!(format_age(9 * 86_400 + 5), "9 days");
    }
}
