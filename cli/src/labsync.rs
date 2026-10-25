// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! `panio config sync`: pull the lab file from a git repository or an HTTPS
//! URL and keep the local copy read-only (#375).
//!
//! The lab stays a plain local file that every command reads as before;
//! nothing on the command path touches the network. Sync writes a *sync
//! record* beside it (`<stem>.source.toml`) naming the source, the revision
//! the copy came from (a commit, or an HTTP ETag), the SHA-256 of the bytes
//! written, and when it was fetched. The record's presence is what makes the
//! lab read-only: `set`/`add`/`rm`, `config edit` and `init` refuse while it
//! exists, so a local edit cannot silently diverge from the source.
//! `--detach` deletes it.
//!
//! A git source is fetched by `git` itself (depth 1, into a throwaway
//! repository), so SSH remotes reuse the user's keys. An HTTPS source is a
//! plain GET with an optional bearer token, resolved through the `secret`
//! crate (env `PANIO_LAB_TOKEN`, `--token-file`, `--token-command`); only the
//! file path or command is recorded, never the token. Either way the fetched
//! file is parsed and validated before it replaces anything, so a bad push
//! leaves the working copy in place.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::error::{Kind, PanioError};
use crate::model;

/// A synced lab older than this gets a warning on every command.
pub const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// The largest lab an HTTPS source may return; anything bigger is refused
/// rather than read into memory.
const MAX_LAB_BYTES: u64 = 8 * 1024 * 1024;

/// Connect + read timeout for an HTTPS fetch.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How the HTTPS token is found: env, then `--token-file`, then
/// `--token-command`.
const TOKEN_SPEC: secret::Spec = secret::Spec {
    what: "lab source token",
    env: "PANIO_LAB_TOKEN",
    file_flag: "--token-file",
    command_flag: "--token-command",
};

const RECORD_HEADER: &str =
    "# Written by `panio config sync`: the lab beside this file is a copy of\n\
     # the source below and is read-only here. Edit the source and run\n\
     # `panio config sync`, or run `panio config sync --detach` to stop syncing.\n\n";

/// Where the lab comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Git {
        /// Anything `git fetch` accepts: an SSH or HTTPS remote, or a local path.
        git: String,
        /// The lab file's path inside the repository.
        path: String,
        /// Branch, tag or commit; `None` is the remote's default branch.
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
    },
    Https {
        /// The lab file's URL: `https://`, or `http://` to a loopback address.
        url: String,
        /// A file holding the bearer token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_file: Option<PathBuf>,
        /// A command that prints the bearer token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_command: Option<String>,
    },
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Git { git, path, git_ref } => {
                write!(f, "{git} {path}")?;
                if let Some(r) = git_ref {
                    write!(f, " @ {r}")?;
                }
                Ok(())
            }
            Source::Https { url, .. } => f.write_str(url),
        }
    }
}

/// The sync record written beside a synced lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub source: Source,
    /// The commit (git) or ETag (HTTPS; empty when the server sent none)
    /// the local copy was taken from.
    pub revision: String,
    /// SHA-256 of the bytes written, to detect a hand edit.
    pub sha256: String,
    /// Unix seconds of the last successful sync.
    pub fetched_at: u64,
}

/// `commit 1a2b3c4d5e6f`, `ETag "v7"`, or `no ETag`, for messages.
fn revision_label(source: &Source, revision: &str) -> String {
    match source {
        Source::Git { .. } => format!("commit {}", short(revision)),
        Source::Https { .. } if revision.is_empty() => "no ETag".to_string(),
        Source::Https { .. } => format!("ETag {revision}"),
    }
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
            "{}: unreadable sync record ({e}); run `panio config sync --force` with \
             the source to rewrite it, or `panio config sync --detach` to stop syncing",
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

/// The first 12 characters of a commit id, for messages.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file_sha256(path: &Path) -> Result<String> {
    Ok(sha256_hex(
        &std::fs::read(path).with_context(|| path.display().to_string())?,
    ))
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
                    "lab was last synced {} ago (from {}, {}); run `panio config sync`",
                    format_age(age),
                    rec.source,
                    revision_label(&rec.source, &rec.revision)
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
/// at which revision, how long ago, and whether it was edited since.
pub fn describe(lab: &Path) -> Result<Option<String>> {
    let Some(rec) = read_record(lab)? else {
        return Ok(None);
    };
    let age = format_age(now_secs().saturating_sub(rec.fetched_at));
    let edited = if edited_locally(lab) {
        "; EDITED LOCALLY since the last sync"
    } else {
        ""
    };
    Ok(Some(format!(
        "synced from {}, {}, {age} ago{edited}",
        rec.source,
        revision_label(&rec.source, &rec.revision)
    )))
}

/// True when `lab` has a sync record and its contents differ from what the
/// last sync wrote (a missing lab counts as edited).
pub fn edited_locally(lab: &Path) -> bool {
    match read_record(lab) {
        Ok(Some(rec)) => file_sha256(lab).map(|h| h != rec.sha256).unwrap_or(true),
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
                PanioError::not_configured("a git lab source needs git, which is not on PATH")
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

/// A fetched lab: its bytes and the revision they came from.
struct Fetched {
    revision: String,
    bytes: Vec<u8>,
}

/// Fetch `path` from `remote` at `git_ref` into a throwaway repository.
fn fetch_git(remote: &str, path: &str, git_ref: Option<&str>) -> Result<Fetched> {
    for (what, v) in [("--git", Some(remote)), ("--ref", git_ref)] {
        if v.is_some_and(|v| v.is_empty() || v.starts_with('-')) {
            // `git fetch` would read it as an option: `--upload-pack=…` runs
            // a program, whether it came from the command line or a record.
            return Err(PanioError::new(
                Kind::Usage,
                format!("{what} must not be empty or start with '-'"),
            )
            .into());
        }
    }
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    git(dir, &["init", "-q"])?;
    let want = git_ref.unwrap_or("HEAD");
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
            remote,
            want,
        ],
    )
    .map_err(|e| {
        PanioError::new(
            Kind::Unreachable,
            format!("could not fetch {want} from {remote}: {e:#}"),
        )
    })?;
    let commit = git_text(dir, &["rev-parse", "FETCH_HEAD"])?;
    let spec = format!("FETCH_HEAD:{path}");
    let bytes = git(dir, &["cat-file", "blob", &spec]).map_err(|_| {
        PanioError::not_configured(format!(
            "{remote} has no file '{path}' at commit {}",
            short(&commit)
        ))
    })?;
    Ok(Fetched {
        revision: commit,
        bytes,
    })
}

// ── HTTPS ───────────────────────────────────────────────────────────────────

/// The host of an `http://` URL, if the URL is plain HTTP.
fn http_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    Some(match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host_port.split(':').next().unwrap_or(""),
    })
}

/// Refuse anything but `https://`, except plain HTTP to this machine (a
/// local mirror, or a test), so a token never crosses a network in clear.
fn check_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(host) = http_host(url) {
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if loopback {
            return Ok(());
        }
    }
    Err(PanioError::new(
        Kind::Usage,
        format!("--url must be https:// (plain http:// only to a loopback address), not {url}"),
    )
    .into())
}

/// What an HTTPS fetch returned.
enum HttpFetch {
    /// 304: the server's copy still matches `If-None-Match`.
    NotModified,
    Body(Fetched),
}

/// GET `url`, with `token` as a bearer token and `etag` as `If-None-Match`.
fn fetch_https(url: &str, token: Option<&secret::Secret>, etag: Option<&str>) -> Result<HttpFetch> {
    check_url(url)?;
    // Never forward `Authorization` across a redirect, so a token is never
    // handed to whatever host a redirect points at. (ureq's default, set
    // explicitly so an upgrade cannot change it.)
    let agent = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
        .build();
    let mut req = agent.get(url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {}", t.expose()));
    }
    if let Some(e) = etag {
        req = req.set("If-None-Match", e);
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code @ (401 | 403), _)) => {
            let hint = match token {
                Some(t) => format!("the token from {} was refused", t.origin()),
                None => format!("no token was sent; {}", TOKEN_SPEC.sources_hint()),
            };
            return Err(PanioError::not_configured(format!("{url}: HTTP {code}: {hint}")).into());
        }
        Err(ureq::Error::Status(404, _)) => {
            return Err(PanioError::not_configured(format!("{url}: HTTP 404 not found")).into())
        }
        Err(ureq::Error::Status(code, _)) => {
            return Err(PanioError::new(Kind::Unreachable, format!("{url}: HTTP {code}")).into())
        }
        Err(e) => return Err(PanioError::new(Kind::Unreachable, format!("{url}: {e}")).into()),
    };
    if resp.status() == 304 {
        return Ok(HttpFetch::NotModified);
    }
    let revision = resp.header("ETag").unwrap_or("").to_string();
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_LAB_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| PanioError::new(Kind::Unreachable, format!("{url}: {e}")))?;
    if bytes.len() as u64 > MAX_LAB_BYTES {
        return Err(PanioError::not_configured(format!(
            "{url}: response is larger than {} MiB; that is not a lab file",
            MAX_LAB_BYTES / 1024 / 1024
        ))
        .into());
    }
    Ok(HttpFetch::Body(Fetched { revision, bytes }))
}

// ── sync ────────────────────────────────────────────────────────────────────

/// What `config sync` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The lab was written (created, or changed to the source's new contents).
    Updated { label: String },
    /// The source matched the local copy; only the fetch time was refreshed.
    Unchanged { label: String },
}

/// Sync the lab at `lab` from `source`, or from the recorded source when
/// `source` is `None`. `force` overwrites a hand-edited or never-synced lab.
pub fn sync(lab: &Path, source: Option<Source>, force: bool) -> Result<Outcome> {
    sync_with_token_env(lab, source, force, std::env::var(TOKEN_SPEC.env).ok())
}

/// [`sync`] with the token environment variable injected, so tests need not
/// touch the process environment.
fn sync_with_token_env(
    lab: &Path,
    source: Option<Source>,
    force: bool,
    token_env: Option<String>,
) -> Result<Outcome> {
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
                     `panio config sync --git <remote> --path <file>` or `--url <url>`",
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

    // The record matches the source and the file is as sync left it: the
    // only case where "unchanged upstream" means nothing to write.
    let pristine = record
        .as_ref()
        .filter(|r| r.source == source)
        .filter(|_| !edited_locally(lab));

    let fetched = match &source {
        Source::Git { git, path, git_ref } => fetch_git(git, path, git_ref.as_deref())?,
        Source::Https {
            url,
            token_file,
            token_command,
        } => {
            let token = secret::resolve_with(
                &TOKEN_SPEC,
                token_env,
                &secret::Sources {
                    file: token_file.clone(),
                    command: token_command.clone(),
                },
                secret::COMMAND_TIMEOUT,
            )
            .map_err(|e| PanioError::not_configured(e.0))?;
            let etag = pristine
                .map(|r| r.revision.as_str())
                .filter(|e| !e.is_empty());
            match fetch_https(url, token.as_ref(), etag)? {
                HttpFetch::Body(f) => f,
                HttpFetch::NotModified => {
                    let rec = pristine.expect("If-None-Match is only sent for a pristine copy");
                    let label = revision_label(&source, &rec.revision);
                    write_record(
                        lab,
                        &Record {
                            fetched_at: now_secs(),
                            ..rec.clone()
                        },
                    )?;
                    return Ok(Outcome::Unchanged { label });
                }
            }
        }
    };
    let label = revision_label(&source, &fetched.revision);
    let text = std::str::from_utf8(&fetched.bytes).map_err(|_| {
        PanioError::not_configured(format!(
            "{source} ({label}) is not UTF-8 text; {} was not changed",
            lab.display()
        ))
    })?;
    model::parse(text)
        .and_then(|l| model::validate_for_save(&l))
        .map_err(|e| {
            PanioError::not_configured(format!(
                "{source} ({label}) is not a valid lab: {e}; {} was not changed",
                lab.display()
            ))
        })?;

    let exists = lab.exists();
    let unchanged = exists && std::fs::read(lab)? == fetched.bytes;
    if exists && !unchanged && !force {
        let why = match &record {
            Some(_) if edited_locally(lab) => "was edited locally since the last sync",
            Some(_) => "",
            None => "exists and was not synced before",
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
    write_record(
        lab,
        &Record {
            source,
            revision: fetched.revision,
            sha256: sha256_hex(&fetched.bytes),
            fetched_at: now_secs(),
        },
    )?;
    Ok(if unchanged {
        Outcome::Unchanged { label }
    } else {
        Outcome::Updated { label }
    })
}

fn write_record(lab: &Path, rec: &Record) -> Result<()> {
    let body = format!("{RECORD_HEADER}{}", toml::to_string(rec)?);
    let rpath = record_path(lab);
    crate::platform::write_atomic(&rpath, body.as_bytes())
        .with_context(|| rpath.display().to_string())
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
            Source::Git {
                git: self.dir.to_string_lossy().into_owned(),
                path: "lab/lab.toml".into(),
                git_ref: None,
            }
        }

        fn with(&self, path: Option<&str>, git_ref: Option<&str>) -> Source {
            Source::Git {
                git: self.dir.to_string_lossy().into_owned(),
                path: path.unwrap_or("lab/lab.toml").into(),
                git_ref: git_ref.map(Into::into),
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
    fn sha256_matches_the_standard_test_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
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
        assert_eq!(rec.sha256, sha256_hex(GOOD.as_bytes()));
        assert_eq!(rec.revision.len(), 40, "{rec:?}");
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
        let e = sync(&r.lab, Some(r.with(Some("nope.toml"), None)), false).unwrap_err();
        assert_eq!(kind(&e), Kind::NotConfigured);
        let e = sync(&r.lab, Some(r.with(None, Some("no-such-branch"))), false).unwrap_err();
        assert_eq!(kind(&e), Kind::Unreachable);
        assert!(!r.lab.exists());
    }

    #[test]
    fn option_shaped_remotes_and_refs_are_refused() {
        let r = Repo::new(GOOD);
        let s = Source::Git {
            git: "--upload-pack=touch pwned".into(),
            path: "lab/lab.toml".into(),
            git_ref: None,
        };
        let e = sync(&r.lab, Some(s), false).unwrap_err();
        assert_eq!(kind(&e), Kind::Usage);
        let e = sync(&r.lab, Some(r.with(None, Some("--output=x"))), false).unwrap_err();
        assert_eq!(kind(&e), Kind::Usage);
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

    // ── HTTPS, against a loopback stub server ───────────────────────────────

    use crate::stubhttp::serve_one;
    use std::net::TcpListener;

    const OK_V1: &[u8] =
        b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 14\r\nConnection: close\r\n\r\n[targets.pi5]\n";
    const OK_V2: &[u8] = b"HTTP/1.1 200 OK\r\nETag: \"v2\"\r\nContent-Length: 29\r\nConnection: close\r\n\r\n[targets.pi5]\n\n[targets.nuc]\n";
    const NOT_MODIFIED: &[u8] =
        b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const BAD: &[u8] =
        b"HTTP/1.1 200 OK\r\nETag: \"v3\"\r\nContent-Length: 13\r\nConnection: close\r\n\r\n[targets.pi5\n";
    const UNAUTHORIZED: &[u8] =
        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    /// Serve one response; returns the URL and the handle yielding the request.
    fn stub(response: &'static [u8]) -> (String, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/lab.toml", l.local_addr().unwrap());
        (url, serve_one(l, response))
    }

    fn https(url: &str, token_file: Option<PathBuf>) -> Source {
        Source::Https {
            url: url.to_string(),
            token_file,
            token_command: None,
        }
    }

    fn lab_in(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("lab.toml")
    }

    #[test]
    fn https_sync_sends_the_token_and_records_the_etag() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let (url, req) = stub(OK_V1);
        let out = sync_with_token_env(&lab, Some(https(&url, None)), false, Some("s3cret".into()))
            .unwrap();
        assert_eq!(
            out,
            Outcome::Updated {
                label: "ETag \"v1\"".into()
            }
        );
        let req = req.join().unwrap().to_ascii_lowercase();
        assert!(req.contains("authorization: bearer s3cret"), "{req}");
        assert!(!req.contains("if-none-match"), "{req}");
        assert_eq!(std::fs::read_to_string(&lab).unwrap(), GOOD);
        let rec = read_record(&lab).unwrap().unwrap();
        assert_eq!(rec.revision, "\"v1\"");
        // The token comes from the environment and is never written down.
        let text = std::fs::read_to_string(record_path(&lab)).unwrap();
        assert!(!text.contains("s3cret"), "{text}");
    }

    #[test]
    fn https_resync_is_conditional_and_a_304_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let (url, req) = stub(OK_V1);
        sync_with_token_env(&lab, Some(https(&url, None)), false, None).unwrap();
        req.join().unwrap();
        // Same URL from a fresh listener isn't possible, so re-point the
        // record at the next stub (the source is otherwise unchanged).
        let (url2, req) = stub(NOT_MODIFIED);
        let mut rec = read_record(&lab).unwrap().unwrap();
        rec.source = https(&url2, None);
        rec.fetched_at = 1;
        write_record(&lab, &rec).unwrap();
        let out = sync_with_token_env(&lab, None, false, None).unwrap();
        assert!(matches!(out, Outcome::Unchanged { .. }), "{out:?}");
        let req = req.join().unwrap().to_ascii_lowercase();
        assert!(req.contains("if-none-match: \"v1\""), "{req}");
        assert!(read_record(&lab).unwrap().unwrap().fetched_at > 1);
        assert_eq!(std::fs::read_to_string(&lab).unwrap(), GOOD);
    }

    #[test]
    fn https_new_contents_replace_the_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let (url, req) = stub(OK_V1);
        sync_with_token_env(&lab, Some(https(&url, None)), false, None).unwrap();
        req.join().unwrap();
        let (url2, req) = stub(OK_V2);
        sync_with_token_env(&lab, Some(https(&url2, None)), false, None).unwrap();
        req.join().unwrap();
        assert_eq!(std::fs::read_to_string(&lab).unwrap(), GOOD2);
        assert_eq!(read_record(&lab).unwrap().unwrap().revision, "\"v2\"");
    }

    #[test]
    fn https_an_invalid_body_leaves_the_copy_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let (url, req) = stub(OK_V1);
        sync_with_token_env(&lab, Some(https(&url, None)), false, None).unwrap();
        req.join().unwrap();
        let (url2, req) = stub(BAD);
        let e = sync_with_token_env(&lab, Some(https(&url2, None)), false, None).unwrap_err();
        req.join().unwrap();
        assert_eq!(kind(&e), Kind::NotConfigured);
        assert!(format!("{e:#}").contains("not a valid lab"), "{e:#}");
        assert_eq!(std::fs::read_to_string(&lab).unwrap(), GOOD);
    }

    #[test]
    fn https_a_refused_token_is_not_configured_and_names_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let tok = tmp.path().join("token");
        std::fs::write(&tok, "wrong\n").unwrap();
        let (url, req) = stub(UNAUTHORIZED);
        let e = sync_with_token_env(&lab, Some(https(&url, Some(tok.clone()))), false, None)
            .unwrap_err();
        let sent = req.join().unwrap().to_ascii_lowercase();
        assert!(sent.contains("authorization: bearer wrong\r\n"), "{sent}");
        assert_eq!(kind(&e), Kind::NotConfigured);
        let msg = format!("{e:#}");
        assert!(
            msg.contains("401") && msg.contains(&tok.display().to_string()),
            "{msg}"
        );
        assert!(!msg.contains("wrong"), "{msg}");
        assert!(!lab.exists());
    }

    #[test]
    fn https_token_file_is_recorded_by_path_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let tok = tmp.path().join("token");
        std::fs::write(&tok, "filetok\n").unwrap();
        let (url, req) = stub(OK_V1);
        sync_with_token_env(&lab, Some(https(&url, Some(tok.clone()))), false, None).unwrap();
        req.join().unwrap();
        let rec = read_record(&lab).unwrap().unwrap();
        assert_eq!(rec.source, https(&url, Some(tok)));
        let text = std::fs::read_to_string(record_path(&lab)).unwrap();
        assert!(!text.contains("filetok"), "{text}");
    }

    #[test]
    fn https_an_unreachable_server_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = lab_in(&tmp);
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/lab.toml");
        let e = sync_with_token_env(&lab, Some(https(&url, None)), false, None).unwrap_err();
        assert_eq!(kind(&e), Kind::Unreachable);
    }

    #[test]
    fn only_https_or_loopback_http_is_accepted() {
        for ok in [
            "https://example.com/lab.toml",
            "http://127.0.0.1:8080/lab.toml",
            "http://localhost/lab.toml",
            "http://[::1]:9/x",
        ] {
            check_url(ok).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
        }
        for bad in [
            "http://example.com/lab.toml",
            "http://127.0.0.1.example.com/lab.toml",
            "http://user@192.0.2.10/lab.toml",
            "ftp://example.com/lab.toml",
            "example.com/lab.toml",
        ] {
            let e = check_url(bad).unwrap_err();
            assert_eq!(kind(&e), Kind::Usage, "{bad}");
        }
    }
}
