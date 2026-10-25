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

//! Shared plumbing for panio's per-subsystem daemons (serialcap, hdmicap).
//!
//! Every daemon follows the same contract: it is an installed binary (the
//! panio libexec dir, PATH, or legacy `~/.cargo/bin` — see [`find_binary`]),
//! binds localhost (port 0 = OS-assigned), and writes a discovery file
//! `{pid, port, token, …}` under `<runtime>/<name>[/<target>]/daemon.json`.
//! `token` is the bearer secret every request to that daemon must carry (see
//! [`Endpoint`]); the file is owner-only, so only the operator's own processes
//! can read it. The
//! optional `<target>` segment lets per-target capture daemons (serialcap,
//! hdmicap, hid) coexist on one host; host-singleton daemons (zigplug,
//! cambrionix, netbootd) omit it (see [`runtime_rel`]). `<runtime>` is
//! `<base>/panio-<uid>` where `<base>` honors `$PANIO_RUNTIME_BASE`
//! (default `/tmp`; see [`runtime_root`]). Liveness is "the recorded pid
//! still exists".

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

/// The cargo-install root for panio's helper binaries. Binaries land in
/// `<root>/bin` (cargo appends `bin/` itself) — see [`libexec_dir`].
pub fn libexec_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/libexec/panio"))
}

/// Private helper dir (libexec): `~/.local/libexec/panio/bin`. The helpers
/// (hdmicap, serialcap, netbootd, cambrionix, hidrig, zigplug, visionocr, …)
/// live here rather than on PATH — they are panio's plumbing, invoked by
/// panio (or explicitly via `panio helper <name> …`), not user commands.
pub fn libexec_dir() -> Option<PathBuf> {
    libexec_root().map(|r| r.join("bin"))
}

/// Helper dir used by the Linux system packages (.deb/tarball installs to a
/// system prefix): `/usr/libexec/panio/bin`. Searched after the per-user
/// libexec dir, so a `make install` build overrides an installed package.
pub fn system_libexec_dir() -> PathBuf {
    PathBuf::from("/usr/libexec/panio/bin")
}

/// Candidate file names for a helper in a directory.
///
/// Windows requires the `.exe` suffix to execute a file and the packaged
/// helpers carry it, but every caller names helpers bare (`"hdmicap"`), so the
/// suffixed name is tried first and the bare one kept as a fallback for a
/// cross-built or extension-less binary. On Unix there is only ever one name.
fn binary_names(name: &str) -> Vec<String> {
    if cfg!(windows) && !name.ends_with(".exe") {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    }
}

/// Helper dirs relative to the running CLI binary, after resolving symlinks
/// (Homebrew links `<prefix>/bin/panio` into the versioned keg):
/// `../libexec/bin` (Homebrew keg layout) and `../libexec/panio/bin`
/// (FHS-style prefix). A relocated install is self-locating without
/// enumerating package managers. Deliberately NOT the exe's own dir: for a
/// `make install` CLI that is `~/.cargo/bin`, the legacy location that must
/// stay a last-resort fallback.
fn exe_relative_dirs() -> Vec<PathBuf> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let Some(prefix) = exe.parent().and_then(|d| d.parent()) else {
        return Vec::new();
    };
    let mut dirs = vec![prefix.join("libexec/bin"), prefix.join("libexec/panio/bin")];
    // The portable Windows layout is `panio\panio.exe` with the helpers in
    // `panio\libexec` beside it — the exe's own directory is the install
    // prefix there, since Windows has no bin/libexec split to hang them off.
    if cfg!(windows) {
        if let Some(exe_dir) = exe.parent() {
            dirs.push(exe_dir.join("libexec"));
        }
    }
    dirs
}

/// The panio helper directories, in resolution order: the per-user libexec
/// dir (`~/.local/libexec/panio/bin`), then dirs relative to the running CLI
/// (Homebrew keg / prefix install — see [`exe_relative_dirs`]), then the
/// system package dir (`/usr/libexec/panio/bin`). These are the only places
/// panio ships helpers into: [`find_binary`] searches them (before falling
/// back to `$PATH` and `~/.cargo/bin`), [`hook_path`] prepends them, and
/// `panio helper` lists them. A per-user `make install` thus shadows an
/// installed system package. The system dir is always present, so this is
/// never empty. Plugin roots (`PANIO_PLUGIN_PATH`) are deliberately not
/// here: they come after all of these, see [`all_helper_dirs`].
pub fn helper_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = libexec_dir().into_iter().collect();
    dirs.extend(exe_relative_dirs());
    dirs.push(system_libexec_dir());
    dirs
}

/// Environment variable naming extra **plugin roots**: install prefixes for
/// out-of-tree helpers and skills (GitHub #303). A list in the platform's
/// `PATH` syntax (`:`-separated on Unix, `;` on Windows). Each root
/// contributes `<root>/libexec/panio/bin` to the helper search
/// ([`plugin_helper_dirs`]) and `<root>/share/panio/skills` to the skill
/// search (`skills.rs`). Read on whichever host runs the command; dispatch to
/// a control host sets it there from the host's `plugin_path` lab field, via
/// the global `--plugin-path` flag.
pub const PLUGIN_PATH_ENV: &str = "PANIO_PLUGIN_PATH";

/// Split a `PANIO_PLUGIN_PATH` value into usable roots and ignored entries.
///
/// Empty components (a leading, trailing or doubled separator) are skipped
/// silently, like an empty `PATH` component. A relative entry is returned in
/// the second list: it would resolve against whatever directory panio
/// happens to be started from (the user's home over SSH, a daemon's cwd), so
/// it is never searched, and the caller warns about it. No other validation:
/// the variable names directories, not lab names.
pub fn parse_plugin_path(raw: &std::ffi::OsStr) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut roots = Vec::new();
    let mut ignored = Vec::new();
    for p in std::env::split_paths(raw) {
        if p.as_os_str().is_empty() {
            continue;
        }
        if p.is_absolute() {
            roots.push(p);
        } else {
            ignored.push(p);
        }
    }
    (roots, ignored)
}

/// The plugin roots from [`PLUGIN_PATH_ENV`] in this process, in list order.
/// The first call that finds an entry [`parse_plugin_path`] ignored warns
/// about it once, on stderr.
pub fn plugin_roots() -> Vec<PathBuf> {
    let raw = std::env::var_os(PLUGIN_PATH_ENV).unwrap_or_default();
    let (roots, ignored) = parse_plugin_path(&raw);
    static WARNED: std::sync::Once = std::sync::Once::new();
    if !ignored.is_empty() {
        WARNED.call_once(|| {
            for p in &ignored {
                eprintln!(
                    "warning: {PLUGIN_PATH_ENV}: ignoring '{}' (not an absolute path)",
                    p.display()
                );
            }
        });
    }
    roots
}

/// A plugin root's helper dir: `<root>/libexec/panio/bin`.
pub fn plugin_helper_dir(root: &Path) -> PathBuf {
    root.join("libexec/panio/bin")
}

/// The helper dirs of every plugin root ([`plugin_roots`]), in order.
pub fn plugin_helper_dirs() -> Vec<PathBuf> {
    plugin_roots()
        .iter()
        .map(|r| plugin_helper_dir(r))
        .collect()
}

/// Every directory panio resolves a helper from before `$PATH`: the
/// bundled [`helper_dirs`] first, then the plugin dirs. The order is the
/// guarantee #303 keeps from #206: a plugin can **add** a helper, but one
/// with the same name as a bundled helper never wins, so a daemon still runs
/// the binary panio installed. There is no opt-in to shadow a bundled
/// helper.
pub fn all_helper_dirs() -> Vec<PathBuf> {
    with_plugins(helper_dirs(), plugin_helper_dirs())
}

/// `bundled` followed by `plugins`. Pure, so the order is testable.
fn with_plugins(bundled: Vec<PathBuf>, plugins: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = bundled;
    dirs.extend(plugins);
    dirs
}

/// One entry of `panio helper` (list): a helper's file name and, when it
/// resolves from a plugin dir, the plugin root it came from (`None` = one of
/// the bundled [`helper_dirs`]).
#[derive(Debug, PartialEq, Eq)]
pub struct HelperEntry {
    pub name: String,
    pub plugin_root: Option<PathBuf>,
}

/// The helpers `panio helper` lists, sorted by name, each attributed to the
/// first dir that has it (the one [`find_binary`] would run). Also returns the
/// plugin helpers that lost to an earlier dir, as `(name, root)`, for
/// `doctor`.
pub fn helper_entries() -> (Vec<HelperEntry>, Vec<(String, PathBuf)>) {
    helper_entries_in(&helper_dirs(), &plugin_roots())
}

/// [`helper_entries`] over explicit dirs, so a test can build the layout.
pub fn helper_entries_in(
    bundled: &[PathBuf],
    plugin_roots: &[PathBuf],
) -> (Vec<HelperEntry>, Vec<(String, PathBuf)>) {
    let files_in = |dir: &Path| -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    };
    let mut found: std::collections::BTreeMap<String, Option<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut shadowed = Vec::new();
    for dir in bundled {
        for n in files_in(dir) {
            found.entry(n).or_insert(None);
        }
    }
    for root in plugin_roots {
        for n in files_in(&plugin_helper_dir(root)) {
            match found.entry(n) {
                std::collections::btree_map::Entry::Occupied(e) => {
                    shadowed.push((e.key().clone(), root.clone()));
                }
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(Some(root.clone()));
                }
            }
        }
    }
    let entries = found
        .into_iter()
        .map(|(name, plugin_root)| HelperEntry { name, plugin_root })
        .collect();
    (entries, shadowed)
}

/// One `panio helper` list line: the bare name for a bundled helper, or the
/// name, a tab, and `plugin: <root>` for one from a plugin root. The name is
/// always the first whitespace-separated field, so a script that read the
/// one-name-per-line output keeps working.
pub fn helper_list_line(e: &HelperEntry) -> String {
    match &e.plugin_root {
        None => e.name.clone(),
        Some(root) => format!("{}\tplugin: {}", e.name, root.display()),
    }
}

/// Find an installed binary: the panio helper dirs first (per-user, then
/// relative to the running CLI — Homebrew keg or other prefix install — then
/// the system package's `/usr/libexec/panio/bin`; see [`helper_dirs`]), then
/// the plugin roots' helper dirs ([`plugin_helper_dirs`]), then $PATH, then
/// ~/.cargo/bin (the pre-libexec install location, kept as a transitional
/// fallback). Never the in-repo build tree, so a running daemon can't point at
/// an ephemeral build artifact.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let cargo_bin = dirs::home_dir().map(|h| h.join(".cargo/bin"));
    let dirs = search_dirs(all_helper_dirs(), path_dirs, cargo_bin);
    first_match(&dirs, &binary_names(name), |p| p.is_file())
}

/// The directories [`find_binary`] searches, in order: the panio helper dirs
/// (bundled, then plugin: [`all_helper_dirs`]), then `$PATH`, then
/// `~/.cargo/bin`.
///
/// Pure, so the **order** is testable without a host that happens to have a
/// helper installed. The order is the whole point: a packaged helper in
/// `/usr/libexec/panio/bin` must win over a stale one on the operator's
/// `$PATH`, so a daemon always runs the binary panio installed. That
/// guarantee had no test of its own until GitHub #206 removed the one that
/// exercised it by accident.
fn search_dirs(
    helper: Vec<PathBuf>,
    path: Vec<PathBuf>,
    cargo_bin: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs = helper;
    dirs.extend(path);
    dirs.extend(cargo_bin);
    dirs
}

/// The first `<dir>/<name>` that `exists` accepts, scanning `dirs` in order and
/// `names` within each. `exists` is a parameter so the search can be tested
/// against a set of paths rather than against the filesystem.
fn first_match(
    dirs: &[PathBuf],
    names: &[String],
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    dirs.iter()
        .find_map(|dir| names.iter().map(|n| dir.join(n)).find(|p| exists(p)))
}

/// PATH value with the helper dirs prepended, for `sh -c` hook commands
/// (power on/off/cycle/state, hid cmd, actions). Lab files reference helpers
/// by bare name (`zigplug …`, `cambrionix …`); prepending libexec keeps those
/// names resolving without the helpers being user-visible on PATH. The plugin
/// helper dirs follow the bundled ones ([`all_helper_dirs`]), so a hook can
/// name a plugin helper bare too, and a bundled one still wins a name clash.
pub fn hook_path() -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = all_helper_dirs();
    paths.extend(std::env::split_paths(&current));
    std::env::join_paths(paths).unwrap_or(current)
}

/// The temp root beneath which panio's per-uid runtime base lives. Honors
/// `$PANIO_RUNTIME_BASE` (default `/tmp`), so the location is configurable
/// without resorting to `$TMPDIR`: macOS hands each environment a different
/// TMPDIR (GUI terminal vs SSH vs sandboxed agent shells), which would make a
/// daemon started in one environment invisible to the others — the bug the
/// hardcoded `/tmp` originally fixed. `$XDG_RUNTIME_DIR` is likewise avoided
/// (systemd removes `/run/user/<uid>` when the user's last session ends,
/// breaking daemons that outlive the SSH session that started them).
pub fn runtime_root() -> PathBuf {
    std::env::var_os("PANIO_RUNTIME_BASE")
        .map(PathBuf::from)
        .unwrap_or_else(crate::platform::default_runtime_root)
}

/// Stable per-user runtime base: `<root>/panio-<uid>`, identical in every
/// environment of the same user. The per-uid namespace and its 0700 ownership
/// check (see [`ensure_runtime_dir`]) are always applied beneath the root.
/// Keep in sync with `runtime_dir()` in hdmicap/src/daemon.rs and
/// serialcap/src/daemon.rs.
fn runtime_base() -> PathBuf {
    let uid = crate::platform::current_uid();
    runtime_root().join(format!("panio-{uid}"))
}

/// Sanitize a user-chosen key (a target name, a hook program's basename, …)
/// into a single safe path component: keep alphanumerics, `-`, `_`, `.`;
/// collapse anything else to `_`. Mirrors serialcap's interface-name
/// sanitizer. An empty result, or one that collapses to exactly `.` or `..`
/// — both still legal under the character allowlist above, and both a path
/// traversal rather than a real component — falls back to `_`.
///
/// Reused everywhere a name that ultimately reached the lab file (a target
/// name that predates [`crate::model::validate_name`], a power hook's
/// program) is turned into a filesystem path component: [`runtime_rel`],
/// [`hook_helper_name`], the `panio helper <name>` host-singleton branch in
/// main.rs, and `state::target_dir`.
pub(crate) fn sanitize_component(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        "_".to_string()
    } else {
        out
    }
}

/// True if `tok` looks like a POSIX shell environment assignment
/// (`VAR=value`) rather than a program name — the shape a hook or helper
/// `cmd` can legitimately be prefixed with, e.g.
/// `AMT_PASSWORD=... amt-tool cycle` (docs/power.md "Credentials").
pub(crate) fn is_env_assignment(tok: &str) -> bool {
    let Some((var, _)) = tok.split_once('=') else {
        return false;
    };
    !var.is_empty()
        && var.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The first token of a shell command line that is not an environment
/// assignment — the program a hook or helper `cmd` actually invokes, whether
/// or not it is preceded by one or more `VAR=value` assignments. Used both to
/// name the helper ([`hook_helper_name`]) and to probe it (`doctor.rs`); a
/// probe or a state/runtime dir keyed on the assignment itself, as a plain
/// `split_whitespace().next()` would do, is simply wrong.
pub fn first_program_token(cmd: &str) -> Option<&str> {
    cmd.split_whitespace().find(|tok| !is_env_assignment(tok))
}

/// The runtime subdir for a daemon, relative to the per-uid base: `<name>` for
/// a single-instance daemon, or `<name>/<sanitized-instance>` for a per-target
/// (multi-instance) daemon. Both the local path helpers and the remote
/// discovery lookup (`dispatch::remote_daemon_endpoint`) build paths through this,
/// so a daemon's writer and reader always agree on the location.
pub fn runtime_rel(name: &str, instance: Option<&str>) -> String {
    match instance {
        Some(i) => format!("{name}/{}", sanitize_component(i)),
        None => name.to_string(),
    }
}

/// Create (0700) and validate the runtime base, then `<base>/<name>[/<inst>]`.
/// The ownership check guards against a squatter pre-creating the /tmp path.
/// `instance` is `Some(target)` for per-target capture daemons (serialcap,
/// hdmicap, hid), `None` for host-singleton daemons (zigplug, cambrionix, …).
pub fn ensure_runtime_dir(name: &str, instance: Option<&str>) -> Result<PathBuf> {
    let base = runtime_base();
    crate::platform::ensure_private_dir(&base)?;
    let dir = base.join(runtime_rel(name, instance));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Where a spawned daemon's stderr is captured (truncated on each start).
pub fn log_path(name: &str, instance: Option<&str>) -> PathBuf {
    runtime_base()
        .join(runtime_rel(name, instance))
        .join("daemon.log")
}

/// Create (truncating) the daemon's stderr log at [`log_path`], private to
/// this user, with the runtime dir ensured first. The log is the daemon's
/// tracing output — device paths, remote hostnames, whatever a hook printed —
/// so on Unix it is created 0600 rather than umask-default, the same as
/// serialcap's capture files.
pub fn create_log(name: &str, instance: Option<&str>) -> Result<std::fs::File> {
    let dir = ensure_runtime_dir(name, instance)?;
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    Ok(o.open(dir.join("daemon.log"))?)
}

/// The runtime base, only if it can be trusted: it exists, is a real
/// directory (not a symlink), is owned by this user, and is closed to
/// everyone else — the same conditions [`ensure_runtime_dir`] establishes
/// when it creates the base. `Ok(None)` when it does not exist yet (nothing
/// has ever started on this host); `Err` names the problem. Every reader of
/// discovery files goes through this, so a `daemon.json` planted under a
/// squatted or world-accessible base can never hand the CLI a port to
/// connect to or a pid to signal. A base that is ours but was left too open
/// is tightened to 0700 here, as the writer does; only a symlink or another
/// owner is refused outright.
fn trusted_runtime_base() -> std::result::Result<Option<PathBuf>, String> {
    let base = runtime_base();
    if std::fs::symlink_metadata(&base).is_err() {
        return Ok(None);
    }
    if crate::platform::is_private_dir(&base) {
        return Ok(Some(base));
    }
    // Our own directory left too open — created by an older panio, or by
    // the ssh ControlMaster path before it went through the private-dir
    // check. Tighten it exactly as the write path does rather than pretend
    // nothing is running; only a symlink or another owner is refused.
    if crate::platform::ensure_private_dir(&base).is_ok() && crate::platform::is_private_dir(&base)
    {
        Ok(Some(base))
    } else {
        Err(format!(
            "{} is not a private directory owned by this user (a symlink, another \
             owner, or group/world-accessible); ignoring the discovery files under \
             it — `chmod 700` it if it is yours",
            base.display()
        ))
    }
}

// ── helper state/runtime-dir API ────────────────────────────────────────────
//
// Helpers must not invent their own paths (a helper writing unnamespaced
// state into ~/.config/panio/ collides with the lab file and each other),
// and must not re-implement the runtime-base logic above. Panio is the
// single source of truth: every invocation of a helper — `panio helper`,
// hook commands, daemon spawns — carries two environment variables:
//
//   PANIO_STATE_DIR    ~/.config/panio/helpers/<name>   durable state
//   PANIO_RUNTIME_DIR  /tmp/panio-<uid>/<name>          discovery, locks,
//                                                           logs (wiped on boot)
//
// Both directories exist by the time the helper runs. Helpers should prefer
// these over hand-rolled paths, falling back to the same literal locations
// when run standalone (documented in docs/dev/adding-power-helpers.md).

/// Durable per-helper state base: `~/.config/panio/helpers`.
pub fn state_base() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/panio/helpers"))
}

/// The `(var, value)` environment pairs for invoking helper `name`, with both
/// directories created. `instance` is `Some(target)` for per-target capture
/// daemons (so each target gets its own runtime + state dir) and `None` for
/// host-singleton helpers. Failures degrade to omitting the affected var — the
/// helper's own fallback then applies.
pub fn helper_env(name: &str, instance: Option<&str>) -> Vec<(&'static str, PathBuf)> {
    let mut env = Vec::new();
    if let Some(state) = state_base().map(|b| b.join(runtime_rel(name, instance))) {
        if std::fs::create_dir_all(&state).is_ok() {
            env.push(("PANIO_STATE_DIR", state));
        }
    }
    if let Ok(runtime) = ensure_runtime_dir(name, instance) {
        env.push(("PANIO_RUNTIME_DIR", runtime));
    }
    env
}

/// The helper name for an opaque hook command: the basename of its first
/// non-assignment shell token (`zigplug -d … on …` → `zigplug`,
/// `/path/to/script.sh …` → `script.sh`, `AMT_PASSWORD=... amt-tool cycle` →
/// `amt-tool`, skipping the assignment). Hooks are opaque strings, so this is
/// a convention, not an inspection — documented in docs/dev/adding-power-helpers.md.
///
/// Sanitized before it comes back: the name becomes a path component
/// (`daemons::helper_env` → [`runtime_rel`]), and a hook string is
/// hand-written in the lab file, not validated the way a target/host/serial
/// name is at `add`/`set` time.
pub fn hook_helper_name(cmd: &str) -> Option<String> {
    let first = first_program_token(cmd)?;
    let name = first.rsplit('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(sanitize_component(name))
    }
}

/// Error for a daemon that didn't publish discovery in time, carrying the
/// tail of its stderr log so the failure is diagnosable. With the `spawned`
/// process in hand it tells the two cases apart: one that has already exited
/// (lost a lock to an old daemon, could not open its device — #194) will not
/// come up, so it is `helper_failed` with its exit code; one still running
/// may yet, so it is `timeout`. (Its pid alone would not do: an exited child
/// stays a zombie, alive to a pid probe, until it is waited on.)
pub fn start_failure(
    name: &str,
    instance: Option<&str>,
    timeout: Duration,
    spawned: Option<&mut std::process::Child>,
) -> anyhow::Error {
    let log = std::fs::read_to_string(log_path(name, instance)).unwrap_or_default();
    let mut tail: Vec<&str> = log.lines().rev().take(5).collect();
    tail.reverse();
    // A clean exit is not a failure: a helper may fork its daemon and let the
    // parent exit 0 (the hid `cmd` contract allows it), and that daemon may
    // still come up. Only a non-zero exit or a signal says it will not.
    let exited = spawned
        .and_then(|c| c.try_wait().ok().flatten())
        .filter(|status| !status.success());
    let what = match exited {
        Some(status) => format!(
            "{name} daemon exited during startup with code {}",
            crate::error::shell_code(status)
        ),
        None => format!("{name} daemon did not start within {} s", timeout.as_secs()),
    };
    let message = if tail.is_empty() {
        format!("{what} (no stderr captured)")
    } else {
        format!("{what}; last stderr:\n  {}", tail.join("\n  "))
    };
    let mut e = match exited {
        Some(status) => {
            crate::error::hook_failed(status, message).channel(crate::error::daemon_channel(name))
        }
        None => crate::error::daemon_start_timeout(name, message),
    };
    if let Some(t) = instance {
        e = e.target(t);
    }
    e.into()
}

fn pid_alive(pid: i32) -> bool {
    crate::platform::pid_alive(pid)
}

// ── binary staleness ─────────────────────────────────────────────────────────

/// Identity recorded at spawn. Legacy records without a digest still load.
#[derive(serde::Serialize, serde::Deserialize)]
struct BinMeta {
    bin: String,
    path: String,
    mtime_ns: u128,
    size: u64,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
}

/// Content comparison for a running helper, including explicit uncertainty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryStatus {
    Current,
    Changed,
    Unknown(&'static str),
}

impl BinaryStatus {
    pub fn is_stale(self) -> Option<bool> {
        match self {
            Self::Current => Some(false),
            Self::Changed => Some(true),
            Self::Unknown(_) => None,
        }
    }

    pub fn note(self) -> String {
        match self {
            Self::Current => " (binary content matches installed)".into(),
            Self::Changed => " (stale: binary content changed; `panio daemons restart`)".into(),
            Self::Unknown(reason) => format!(" (binary identity unknown: {reason})"),
        }
    }
}

fn stat_identity(path: &Path) -> Option<(u128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, md.len()))
}

/// Best-effort SHA-256 using trusted OS tool paths rather than helper PATH.
/// Unix tools read an already-open file on stdin, including deleted /proc exes.
/// An unavailable tool leaves identity unknown; it never prevents capture.
fn content_digest(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("/usr/bin/sha256sum");
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = std::process::Command::new("/usr/bin/shasum");
        cmd.args(["-a", "256"]);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let Some(system_root) = std::env::var_os("SystemRoot") else {
            #[cfg(test)]
            report_hash_failure("SystemRoot is absent", None);
            return None;
        };
        let powershell =
            PathBuf::from(system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        if !powershell.is_absolute() {
            #[cfg(test)]
            report_hash_failure("SystemRoot does not resolve to an absolute tool path", None);
            return None;
        }
        let mut cmd = std::process::Command::new(powershell);
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            concat!(
                "$ErrorActionPreference='Stop'; $stream=$null; $hash=$null; ",
                "try { $stream=[System.IO.File]::OpenRead($env:PANIO_HASH_FILE); ",
                "$hash=[System.Security.Cryptography.SHA256]::Create(); ",
                "[System.BitConverter]::ToString($hash.ComputeHash($stream))",
                ".Replace('-','').ToLowerInvariant() } ",
                "finally { if ($null -ne $hash) { $hash.Dispose() }; ",
                "if ($null -ne $stream) { $stream.Dispose() } }",
            ),
        ]);
        cmd.env("PANIO_HASH_FILE", path);
        cmd
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = path;
        return None;
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        #[cfg(unix)]
        cmd.stdin(std::fs::File::open(path).ok()?);
        #[cfg(windows)]
        cmd.stdin(std::process::Stdio::null());
        #[cfg(test)]
        cmd.stderr(std::process::Stdio::piped());
        #[cfg(not(test))]
        cmd.stderr(std::process::Stdio::null());
        let output = match cmd.output() {
            Ok(output) => output,
            Err(error) => {
                #[cfg(test)]
                report_hash_failure(&format!("tool spawn failed: {error}"), None);
                #[cfg(not(test))]
                let _ = error;
                return None;
            }
        };
        if !output.status.success() {
            #[cfg(test)]
            report_hash_failure("tool returned a failing status", Some(&output));
            return None;
        }
        let Ok(text) = std::str::from_utf8(&output.stdout) else {
            #[cfg(test)]
            report_hash_failure("stdout was not UTF-8", Some(&output));
            return None;
        };
        let Some(digest) = text.split_whitespace().next() else {
            #[cfg(test)]
            report_hash_failure("stdout contained no digest", Some(&output));
            return None;
        };
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            #[cfg(test)]
            report_hash_failure("stdout contained an invalid SHA-256 digest", Some(&output));
            return None;
        }
        Some(digest.to_ascii_lowercase())
    }
}

/// Bounded, test-only evidence; never prints command arguments or environment.
#[cfg(test)]
fn report_hash_failure(reason: &str, output: Option<&std::process::Output>) {
    eprintln!("content_digest failure: {reason}");
    if let Some(output) = output {
        eprintln!(
            "hash status={} stdout_bytes(first 512)={:?} stderr_bytes(first 512)={:?}",
            output.status,
            &output.stdout[..output.stdout.len().min(512)],
            &output.stderr[..output.stderr.len().min(512)],
        );
    }
}

/// On Linux this is the actual running executable, even after an atomic install.
fn running_digest(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        content_digest(Path::new(&format!("/proc/{pid}/exe")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Called after successful spawn, so Linux can stamp the executable actually run.
/// A pid binds the digest to this start, preventing reuse of a previous record.
pub fn record_binmeta(bin: &Path, name: &str, instance: Option<&str>, pid: u32) {
    let Some((mtime_ns, size)) = stat_identity(bin) else {
        return;
    };
    let sha256 = if cfg!(target_os = "linux") {
        running_digest(pid)
    } else {
        content_digest(bin)
    };
    let meta = BinMeta {
        bin: bin
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: bin.to_string_lossy().into_owned(),
        mtime_ns,
        size,
        sha256,
        pid: Some(pid),
    };
    if let Ok(dir) = ensure_runtime_dir(name, instance) {
        if let Ok(text) = serde_json::to_vec(&meta) {
            if let Ok(mut file) = tempfile::NamedTempFile::new_in(&dir) {
                use std::io::Write;
                if file.write_all(&text).is_ok() {
                    let _ = file.persist(dir.join("binmeta.json"));
                }
            }
        }
    }
}

fn compare_binary(
    meta: &BinMeta,
    current: &Path,
    pid: u32,
    running: Option<String>,
) -> BinaryStatus {
    let Some(current_digest) = content_digest(current) else {
        return BinaryStatus::Unknown("installed executable unavailable or hashing failed");
    };
    let recorded = (meta.pid == Some(pid))
        .then_some(meta.sha256.as_ref())
        .flatten()
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|digest| digest.to_ascii_lowercase());
    let Some(start_digest) = running.or(recorded) else {
        return if current.to_string_lossy() != meta.path
            || stat_identity(current) != Some((meta.mtime_ns, meta.size))
        {
            BinaryStatus::Unknown("metadata changed; running content unavailable")
        } else {
            BinaryStatus::Unknown("running executable content unavailable")
        };
    };
    if current_digest == start_digest {
        BinaryStatus::Current
    } else {
        BinaryStatus::Changed
    }
}

pub fn binary_status(name: &str, instance: Option<&str>) -> BinaryStatus {
    let dir = runtime_base().join(runtime_rel(name, instance));
    let Some(pid) = std::fs::read_to_string(dir.join("daemon.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("pid")?.as_u64())
        .and_then(|p| u32::try_from(p).ok())
    else {
        return BinaryStatus::Unknown("discovery identity unavailable");
    };
    let Some(meta) = std::fs::read_to_string(dir.join("binmeta.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<BinMeta>(&s).ok())
    else {
        return BinaryStatus::Unknown("no recorded binary identity");
    };
    let current = find_binary(&meta.bin).unwrap_or_else(|| PathBuf::from(&meta.path));
    compare_binary(&meta, &current, pid, running_digest(pid))
}

/// Only proven content differences permit automatic or --stale restarts.
pub fn binary_is_stale(name: &str, instance: Option<&str>) -> Option<bool> {
    binary_status(name, instance).is_stale()
}

/// One live daemon found via its discovery file under the runtime base.
pub struct DaemonInfo {
    /// Discovery dir name (serialcap, hdmicap, hid, zigplug, …).
    pub name: String,
    /// Target name for per-target (multi-instance) daemons; `None` for
    /// host-singleton daemons.
    pub instance: Option<String>,
    pub pid: i32,
    pub port: Option<u16>,
    /// Daemon-specific detail (e.g. zigplug's serial device), if published.
    pub detail: String,
    /// Content comparison with the currently installed helper.
    pub binary_status: BinaryStatus,
}

/// Parse `<dir>/daemon.json` into a live [`DaemonInfo`], or `None` if the file
/// is absent, unparseable, or names a dead pid (stale).
fn read_discovery(
    dir: &std::path::Path,
    name: &str,
    instance: Option<String>,
) -> Option<DaemonInfo> {
    let text = std::fs::read_to_string(dir.join("daemon.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pid = v.get("pid")?.as_i64()?;
    if !pid_alive(pid as i32) {
        return None;
    }
    let binary_status = binary_status(name, instance.as_deref());
    Some(DaemonInfo {
        name: name.to_string(),
        instance,
        pid: pid as i32,
        port: v.get("port").and_then(|p| p.as_u64()).map(|p| p as u16),
        detail: v
            .get("device")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string(),
        binary_status,
    })
}

/// Every daemon currently publishing a live discovery file. Stale files
/// (dead pid) are skipped, mirroring [`daemon_endpoint`]'s liveness rule. Handles
/// both layouts: `<name>/daemon.json` (host-singleton: zigplug, cambrionix,
/// netbootd) and `<name>/<target>/daemon.json` (per-target: serialcap,
/// hdmicap, hid).
pub fn list_discovered() -> Vec<DaemonInfo> {
    let mut out = Vec::new();
    let base = match trusted_runtime_base() {
        Ok(Some(base)) => base,
        Ok(None) => return out,
        Err(why) => {
            // The human-facing inventory is the one place this is said aloud;
            // the per-daemon readers just answer "not running".
            eprintln!("warning: {why}");
            return out;
        }
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        // Host-singleton: a discovery file sits directly in <name>/.
        if let Some(info) = read_discovery(&dir, &name, None) {
            out.push(info);
            continue;
        }
        // Otherwise look one level down for per-target instances.
        let Ok(subs) = std::fs::read_dir(&dir) else {
            continue;
        };
        for sub in subs.flatten() {
            let inst = sub.file_name().to_string_lossy().into_owned();
            if let Some(info) = read_discovery(&sub.path(), &name, Some(inst)) {
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.instance.cmp(&b.instance))
    });
    out
}

/// Processes launched from a panio helper dir that are neither in
/// `exclude_pids` nor descended from one of them — stray helper invocations
/// (e.g. a wedged one-shot holding a serial port).
///
/// "Launched from" means the process's program path (argv[0]) sits directly
/// in one of [`helper_dirs`], the bundled dirs only: a process running out of
/// a plugin root (#303) is not panio's to signal, so `daemons stop --all`
/// leaves it alone. The rest of the command line is not consulted,
/// so the `cargo install --root …/libexec/panio …` that `make install`
/// runs, an editor, or an `ls` that merely *mentions* the dir is not a stray
/// and is not TERMed by `daemons stop --all`. Descendants of an excluded pid
/// are excluded too, because a recorded pid is not always the helper itself:
/// on Linux netbootd runs under `sudo`, so the recorded pid is sudo's and the
/// real netbootd is its child, and hdmicap spawns its OCR helper the same way.
pub fn list_stray_helpers(exclude_pids: &[i32]) -> Vec<(i32, String)> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,args="])
        .output()
    else {
        return Vec::new();
    };
    strays_in_ps(
        &String::from_utf8_lossy(&out.stdout),
        &helper_dirs(),
        std::process::id() as i32,
        exclude_pids,
    )
}

/// One row of `ps -axo pid=,ppid=,args=`: `(pid, ppid, command line)`. The
/// numeric columns are right-aligned, so each is taken up to the next run of
/// blanks rather than a single space.
fn parse_ps_row(line: &str) -> Option<(i32, i32, &str)> {
    let rest = line.trim_start();
    let (pid, rest) = rest.split_once(' ')?;
    let rest = rest.trim_start();
    let (ppid, rest) = rest.split_once(' ')?;
    Some((pid.parse().ok()?, ppid.parse().ok()?, rest.trim()))
}

/// True when argv[0] of `args` is a file directly inside one of `helper_dirs`.
fn launched_from(args: &str, helper_dirs: &[PathBuf]) -> bool {
    let Some(program) = args.split_whitespace().next() else {
        return false;
    };
    Path::new(program)
        .parent()
        .is_some_and(|dir| helper_dirs.iter().any(|d| d.as_path() == dir))
}

/// The classification behind [`list_stray_helpers`], over the text of a
/// `ps` listing: pure, so it is unit-testable with sample rows.
fn strays_in_ps(
    ps: &str,
    helper_dirs: &[PathBuf],
    me: i32,
    exclude_pids: &[i32],
) -> Vec<(i32, String)> {
    let rows: Vec<(i32, i32, &str)> = ps.lines().filter_map(parse_ps_row).collect();
    // Close the excluded set over parent → child, so a helper's own children
    // (sudo's netbootd, hdmicap's OCR helper) go with it.
    let mut excluded: std::collections::HashSet<i32> = exclude_pids.iter().copied().collect();
    loop {
        let before = excluded.len();
        for (pid, ppid, _) in &rows {
            if excluded.contains(ppid) {
                excluded.insert(*pid);
            }
        }
        if excluded.len() == before {
            break;
        }
    }
    rows.into_iter()
        .filter(|(pid, _, args)| {
            *pid != me && !excluded.contains(pid) && launched_from(args, helper_dirs)
        })
        .map(|(pid, _, args)| (pid, args.to_string()))
        .collect()
}

// ── untracked daemons ────────────────────────────────────────────────────────
//
// A daemon's only record is its discovery file under the runtime base, and on
// Linux that base is under /tmp, which systemd-tmpfiles ages out: Debian's
// stock policy is `q /tmp 1777 root root 10d`, so every file under it that has
// not been touched for ten days is deleted — including the discovery file of a
// daemon that has simply been *running* that long without a command being run
// against it. Nothing tells the daemon; it keeps running and keeps its capture
// device. Panio, whose only handle on it was the deleted file, reports the
// channel `stopped`, and the next `watch` spawns a replacement that dies on
// the orphan's advisory lock — one command says stopped, the next says already
// running, and neither can fix it. Measured on a Pi 5 control host: three
// hdmicap daemons, 13 days old, ~95 CPU-hours, invisible (GitHub issue #187).
//
// The .deb now ships a tmpfiles.d exclusion so this stops happening on a
// packaged install, but a `make install` host, a different distro policy, or a
// hand-cleaned /tmp all produce the same orphan — and an operator looking at
// one needs a way out of it regardless of how it got there.
//
// The way out is to recognize the *process*: panio's daemons are spawned
// from a helper dir, with a `daemon` subcommand and a `--device`, all of which
// `ps` still shows long after the file is gone.

/// A helper process running as a daemon that no live discovery file accounts
/// for — see the note above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untracked {
    /// Helper basename as invoked (`hdmicap`, `serialcap`, …).
    pub name: String,
    pub pid: i32,
    /// Every device it holds, however its command line names them: hdmicap
    /// takes one `--device`, serialcap takes a repeatable `--interface`
    /// carrying a device per entry. Empty when it names none.
    pub devices: Vec<String>,
    /// The full command line, for display.
    pub args: String,
}

impl Untracked {
    /// The devices for a status line: comma-separated, `-` when it names none.
    pub fn devices_display(&self) -> String {
        if self.devices.is_empty() {
            "-".to_string()
        } else {
            self.devices.join(", ")
        }
    }
}

/// The device inside one `--interface` value, which serialcap spells
/// `NAME=DEVICE@BAUD[:SENSE]` (see `serial::interface_arg`). The baud is cut at
/// the *last* `@` and the name at the first `=`, so a device path containing
/// either character still comes back whole. A value with no `@` is taken as
/// `NAME=DEVICE`, and one with no `=` as a bare device.
fn interface_device(value: &str) -> Option<String> {
    let without_baud = value.rsplit_once('@').map_or(value, |(head, _)| head);
    let device = without_baud
        .split_once('=')
        .map_or(without_baud, |(_, dev)| dev);
    (!device.is_empty()).then(|| device.to_string())
}

/// The value of `flag` at `i`, whether written `--flag value` or `--flag=value`.
fn flag_value<'a>(toks: &[&'a str], i: usize, flag: &str) -> Option<&'a str> {
    let tok = toks.get(i)?;
    if let Some(v) = tok.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
        return Some(v);
    }
    if *tok == flag {
        return toks.get(i + 1).copied();
    }
    None
}

/// Classify one `ps` command line as a panio daemon invocation, returning the
/// helper's basename and every device it holds. `None` for anything not running
/// the `daemon` subcommand: a wedged one-shot (`zigplug … on 1`), or a daemon's
/// own OCR child. Pure, so the classification is unit-testable without a
/// process to look at.
fn daemon_invocation(args: &str) -> Option<(String, Vec<String>)> {
    let mut toks = args.split_whitespace();
    let program = toks.next()?;
    let name = program.rsplit(['/', '\\']).next()?;
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    let rest: Vec<&str> = toks.collect();
    // The subcommand is the first word after the program: `hdmicap daemon …`.
    if rest.first() != Some(&"daemon") {
        return None;
    }
    // hdmicap names one capture device; serialcap repeats `--interface`, one
    // per serial port it owns, and an orphan of either kind is identified by
    // the devices it is sitting on.
    let mut devices = Vec::new();
    for i in 0..rest.len() {
        if let Some(v) = flag_value(&rest, i, "--device") {
            devices.push(v.to_string());
        } else if let Some(v) = flag_value(&rest, i, "--interface") {
            devices.extend(interface_device(v));
        }
    }
    Some((name.to_string(), devices))
}

/// One [`list_stray_helpers`] row as an untracked daemon, or `None` when the
/// process is a stray of some other kind. Callers that already have a stray
/// listing (the `panio daemons` inventory) use this to split it rather than
/// walking `ps` a second time.
pub fn untracked_of(pid: i32, args: &str) -> Option<Untracked> {
    let (name, devices) = daemon_invocation(args)?;
    Some(Untracked {
        name,
        pid,
        devices,
        args: args.to_string(),
    })
}

/// Every untracked daemon on this host: a process launched from a panio
/// helper dir, running its `daemon` subcommand, whose pid no live discovery
/// file names. Tracked daemons and their children are excluded by
/// [`list_stray_helpers`], so a healthy daemon never appears here.
pub fn list_untracked() -> Vec<Untracked> {
    let tracked: Vec<i32> = list_discovered().iter().map(|d| d.pid).collect();
    list_stray_helpers(&tracked)
        .into_iter()
        .filter_map(|(pid, args)| untracked_of(pid, &args))
        .collect()
}

/// True when two device arguments name the same node. Compared as written
/// first, then through `canonicalize`: the lab names a capture device by its
/// stable `/dev/v4l/by-path/…` symlink, and an orphan could have been started
/// from a different spelling of the same device.
pub fn same_device(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The untracked daemons named `name` holding any of `devices` — the orphan
/// standing between a channel and a daemon it can start. A channel can name
/// several devices (a target's serial interfaces), and one daemon can hold
/// several; an overlap of one is enough, because that one device is what the
/// replacement will fail to open.
pub fn untracked_on_devices(name: &str, devices: &[String]) -> Vec<Untracked> {
    list_untracked()
        .into_iter()
        .filter(|u| u.name == name)
        .filter(|u| {
            u.devices
                .iter()
                .any(|held| devices.iter().any(|want| same_device(held, want)))
        })
        .collect()
}

/// Percent-encode a query-parameter value: everything but RFC 3986's
/// unreserved characters. Used for every value spliced into a daemon URL or
/// API path that didn't come from this process's own fixed vocabulary —
/// interface names (user-chosen, land in `?interface=`), a dashboard ticket —
/// so a name containing `&`, `=`, `#`, or a space can't reshape the query
/// string it's placed into.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A running daemon's HTTP endpoint, as read from its discovery file: the
/// loopback port and the bearer token every request must carry. `token` is
/// `None` for a daemon started by a panio older than the token — it accepts
/// unauthenticated requests; a named `panio daemons restart` replaces it.
/// `--stale` excludes instances whose content identity cannot be compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub pid: i32,
    pub port: u16,
    pub token: Option<String>,
}

impl Endpoint {
    /// Parse a `daemon.json` body (read locally, or fetched over SSH by
    /// `dispatch::remote_daemon_endpoint`). `None` without a pid and a port.
    /// Liveness is the caller's concern.
    pub fn from_json(text: &str) -> Option<Endpoint> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let pid = i32::try_from(v.get("pid")?.as_i64()?).ok()?;
        let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
        let token = v
            .get("token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        Some(Endpoint { pid, port, token })
    }

    /// `http://127.0.0.1:<port>` — the base for API calls.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A GET of `path` (which may carry a query) with the token attached as
    /// `Authorization: Bearer`.
    pub fn get(&self, path: &str) -> ureq::Request {
        self.authorize(ureq::get(&format!("{}{path}", self.base_url())))
    }

    /// A POST to `path`, likewise.
    pub fn post(&self, path: &str) -> ureq::Request {
        self.authorize(ureq::post(&format!("{}{path}", self.base_url())))
    }

    fn authorize(&self, req: ureq::Request) -> ureq::Request {
        match &self.token {
            Some(t) => req.set("Authorization", &format!("Bearer {t}")),
            None => req,
        }
    }

    /// `scheme://127.0.0.1:<port><path>` with the token as a `token=` query
    /// parameter — the form a browser needs, since a page load, an `<img>`
    /// or a WebSocket upgrade cannot set a header. `path` may already carry
    /// a query.
    fn url_with_token(&self, scheme: &str, path: &str) -> String {
        let mut url = format!("{scheme}://127.0.0.1:{}{path}", self.port);
        if let Some(t) = &self.token {
            url.push(if path.contains('?') { '&' } else { '?' });
            url.push_str("token=");
            url.push_str(t);
        }
        url
    }

    /// The URL a human opens in a browser (the dashboard is `GET /`).
    pub fn http_url(&self, path: &str) -> String {
        self.url_with_token("http", path)
    }
}

/// `<dir>/daemon.json` as a live [`Endpoint`], or None if the file is absent,
/// unparseable, or names a dead pid.
fn endpoint_at(dir: &Path) -> Option<Endpoint> {
    let text = std::fs::read_to_string(dir.join("daemon.json")).ok()?;
    let ep = Endpoint::from_json(&text)?;
    pid_alive(ep.pid).then_some(ep)
}

/// The named running daemon instance's endpoint (port + token), or None if
/// it isn't running. `instance` selects a per-target daemon (`None` =
/// host-singleton).
pub fn daemon_endpoint(name: &str, instance: Option<&str>) -> Option<Endpoint> {
    let base = trusted_runtime_base().ok().flatten()?;
    endpoint_at(&base.join(runtime_rel(name, instance)))
}

/// PID of the named running daemon instance, or None if it isn't running.
pub fn daemon_pid(name: &str, instance: Option<&str>) -> Option<i32> {
    daemon_endpoint(name, instance).map(|ep| ep.pid)
}

/// Base URL of the named running daemon instance, or None if it isn't running.
pub fn daemon_url(name: &str, instance: Option<&str>) -> Option<String> {
    daemon_endpoint(name, instance).map(|ep| ep.base_url())
}

/// Block until the named daemon instance answers discovery, or time out.
pub fn wait_for_daemon(name: &str, instance: Option<&str>, timeout: Duration) -> Option<String> {
    wait_for_endpoint(name, instance, timeout, None).map(|ep| ep.base_url())
}

/// [`wait_for_daemon`] for a daemon that is *replacing* the one at `old_pid`:
/// a discovery file still naming that pid is never accepted as the new
/// daemon's.
///
/// A daemon that is SIGKILLed cannot remove its own discovery file, and the
/// file outlives it by however long the kernel takes to finish with the pid —
/// during which `daemon_url` happily returns the dead daemon's port. That is
/// how `daemons restart --stale` came to report a restart that had actually
/// failed, quoting the old daemon's port back as the new one's (GitHub #193).
/// The replacement always gets a fresh pid, so refusing the one being replaced
/// cannot hide a real success.
pub fn wait_for_replacement(
    name: &str,
    instance: Option<&str>,
    timeout: Duration,
    old_pid: i32,
) -> Option<String> {
    wait_for_endpoint(name, instance, timeout, Some(old_pid)).map(|ep| ep.base_url())
}

fn wait_for_endpoint(
    name: &str,
    instance: Option<&str>,
    timeout: Duration,
    reject_pid: Option<i32>,
) -> Option<Endpoint> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(ep) = daemon_endpoint(name, instance) {
            if Some(ep.pid) != reject_pid {
                return Some(ep);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Remove `<runtime>/<name>[/<instance>]/daemon.json` if — and only if — it
/// still names `pid`. A daemon removes its own discovery file on a graceful
/// shutdown; one that was SIGKILLed cannot, and the file it leaves behind is
/// what a replacement's startup wait would otherwise mistake for its own
/// (GitHub #193). The pid guard is what makes this safe to call after the
/// kill: if a new daemon has already published, the file is its, not the dead
/// one's, and is left alone. Returns whether a file was removed.
pub fn remove_discovery_for_pid(name: &str, instance: Option<&str>, pid: i32) -> bool {
    let Ok(Some(base)) = trusted_runtime_base() else {
        return false;
    };
    let path = base.join(runtime_rel(name, instance)).join("daemon.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    match Endpoint::from_json(&text) {
        Some(ep) if ep.pid == pid => std::fs::remove_file(&path).is_ok(),
        _ => false,
    }
}

/// `PANIO_RUNTIME_BASE` is process-global and `cargo test` runs the unit
/// tests as threads of one process, so every test that points it somewhere
/// holds this — **including the ones in other modules**.
///
/// It lives out here, `pub(crate)`, rather than inside `mod tests` because
/// that is the difference between the two. `main.rs`'s console-fallback test
/// (#196) set the variable with a comment calling it "safe: mutated and
/// restored within this test", could not reach a lock it could not name, and
/// swapped the base out from under `daemons::tests` about once in a dozen
/// suite runs.
#[cfg(test)]
pub(crate) static RUNTIME_BASE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_helper_name_takes_first_token_basename() {
        assert_eq!(
            hook_helper_name("zigplug -d /dev/x on 1").as_deref(),
            Some("zigplug")
        );
        assert_eq!(
            hook_helper_name("/usr/local/bin/script.sh --flag").as_deref(),
            Some("script.sh")
        );
        assert_eq!(
            hook_helper_name("  cambrionix state 4").as_deref(),
            Some("cambrionix")
        );
        assert_eq!(hook_helper_name(""), None);
        assert_eq!(hook_helper_name("   "), None);
    }

    /// A hook may be prefixed with one or more `VAR=value` assignments (the
    /// AMT credential convention, docs/power.md "Credentials"); the helper
    /// name is the program after them, not the assignment itself.
    #[test]
    fn hook_helper_name_skips_leading_env_assignments() {
        assert_eq!(
            hook_helper_name("AMT_PASSWORD=hunter2 amt-tool cycle").as_deref(),
            Some("amt-tool")
        );
        assert_eq!(
            hook_helper_name("A=1 B=2 /opt/bin/relay.sh on").as_deref(),
            Some("relay.sh")
        );
        // A value containing `=` doesn't confuse the split.
        assert_eq!(
            hook_helper_name("AMT_PASSWORD=a=b amt-tool cycle").as_deref(),
            Some("amt-tool")
        );
        // All assignments, no program: nothing to name.
        assert_eq!(hook_helper_name("AMT_PASSWORD=hunter2"), None);
    }

    /// The returned name always lands as a single path component under the
    /// runtime/state base — a hook string is hand-written, not validated the
    /// way `add`/`set` names are (Review lows #3, #10).
    #[test]
    fn hook_helper_name_sanitizes_the_result() {
        // A basename is always taken first, so a directory traversal in the
        // path collapses on its own...
        assert_eq!(
            hook_helper_name("../../etc/passwd --flag").as_deref(),
            Some("passwd")
        );
        // ...but a bare `..` (no `/`) is the basename, and stays a traversal
        // component unless sanitize_component catches it too.
        assert_eq!(hook_helper_name("..").as_deref(), Some("_"));
        assert_eq!(
            hook_helper_name("weird!name* --flag").as_deref(),
            Some("weird_name_")
        );
    }

    /// The orphan-recovery path keys entirely on the command line, because the
    /// discovery file that named the daemon is what went missing (#187). A
    /// daemon invocation is a helper running `daemon`, and the device it holds
    /// is the `--device` it was handed.
    #[test]
    fn daemon_invocation_reads_the_helper_and_its_device() {
        let args = "/usr/libexec/panio/bin/hdmicap daemon \
                    --device /dev/v4l/by-path/x-video-index0 --port 0";
        let (name, devices) = daemon_invocation(args).expect("this is a daemon");
        assert_eq!(name, "hdmicap");
        assert_eq!(devices, ["/dev/v4l/by-path/x-video-index0"]);

        // `--device=<path>` is the same argument spelled the other way.
        let (_, devices) =
            daemon_invocation("/usr/libexec/panio/bin/hdmicap daemon --device=/dev/video0")
                .expect("this is a daemon");
        assert_eq!(devices, ["/dev/video0"]);

        // A daemon that names no device is still a daemon.
        let (name, devices) =
            daemon_invocation("/usr/libexec/panio/bin/zigplug daemon").expect("still a daemon");
        assert_eq!(name, "zigplug");
        assert!(devices.is_empty());
    }

    /// Only the `daemon` subcommand counts. A wedged one-shot holding a serial
    /// port, and a daemon's own OCR child, are both strays — reaping them as
    /// if they were the channel's daemon would kill the wrong process.
    #[test]
    fn daemon_invocation_rejects_anything_that_is_not_a_daemon() {
        assert_eq!(
            daemon_invocation("/usr/libexec/panio/bin/zigplug -d /dev/x on 1"),
            None
        );
        assert_eq!(
            daemon_invocation("/usr/libexec/panio/bin/linuxocr --json /tmp/frame.png"),
            None
        );
        // `--device` alone, with no subcommand, is not a daemon either.
        assert_eq!(
            daemon_invocation("/usr/libexec/panio/bin/hdmicap shot --device /dev/video0"),
            None
        );
        assert_eq!(daemon_invocation(""), None);
    }

    /// The Windows helper name carries `.exe`; the daemon it is compared
    /// against is named bare everywhere in the CLI.
    #[test]
    fn daemon_invocation_strips_the_windows_suffix() {
        let (name, _) =
            daemon_invocation(r"C:\panio\libexec\hdmicap.exe daemon --device 0").unwrap();
        assert_eq!(name, "hdmicap");
    }

    /// serialcap does not take `--device`: it repeats `--interface`, whose
    /// value is `NAME=DEVICE@BAUD[:SENSE]` (`serial::interface_arg`). Reading
    /// only `--device` left every serial orphan with no device to match a
    /// channel against, so `serial watch` could not recognize the process
    /// holding its port.
    #[test]
    fn daemon_invocation_reads_every_serial_interface_device() {
        let args = "/usr/libexec/panio/bin/serialcap daemon --port 0 \
                    --interface console=/dev/ttyUSB0@115200 \
                    --interface aux=/dev/serial/by-id/usb-FTDI_A-if00-port0@9600:cts";
        let (name, devices) = daemon_invocation(args).expect("this is a daemon");
        assert_eq!(name, "serialcap");
        assert_eq!(
            devices,
            ["/dev/ttyUSB0", "/dev/serial/by-id/usb-FTDI_A-if00-port0"],
            "the baud, the sense suffix and the interface name are not the device"
        );
    }

    /// The pieces of an `--interface` value, at the edges: the baud is cut at
    /// the last `@` and the name at the first `=`, so a by-id device path
    /// carrying either character still comes back whole.
    #[test]
    fn interface_device_takes_the_device_and_nothing_else() {
        assert_eq!(
            interface_device("console=/dev/ttyUSB0@115200").as_deref(),
            Some("/dev/ttyUSB0")
        );
        assert_eq!(
            interface_device("console=/dev/ttyUSB0@115200:cts").as_deref(),
            Some("/dev/ttyUSB0")
        );
        assert_eq!(
            interface_device("a=b=c@115200").as_deref(),
            Some("b=c"),
            "only the first `=` separates the name"
        );
        assert_eq!(
            interface_device("n=/dev/by-id/usb-A@B-if00@9600").as_deref(),
            Some("/dev/by-id/usb-A@B-if00"),
            "only the last `@` separates the baud"
        );
        // Degenerate forms rather than a panic.
        assert_eq!(
            interface_device("/dev/ttyUSB0").as_deref(),
            Some("/dev/ttyUSB0")
        );
        assert_eq!(interface_device("console=@115200"), None);
    }

    /// A device argument is matched as written before anything is resolved, so
    /// the comparison works for a path that does not exist on this machine
    /// (every non-Linux host running these tests, for one).
    #[test]
    fn same_device_matches_identical_paths_without_touching_the_filesystem() {
        assert!(same_device(
            "/dev/v4l/by-path/platform-xhci-hcd.0-usbv2-0:1.1.3.3.2:1.0-video-index0",
            "/dev/v4l/by-path/platform-xhci-hcd.0-usbv2-0:1.1.3.3.2:1.0-video-index0"
        ));
        assert!(!same_device("/dev/video0", "/dev/video1"));
    }

    /// The whole recovery pipeline over a realistic `ps` listing: the stray
    /// walk excludes the tracked daemon and its OCR child, and the
    /// classification then separates the orphaned daemon from a wedged
    /// one-shot. This is the exact split `panio daemons` prints, and the set
    /// `video watch` searches before it starts a replacement (#187).
    #[test]
    fn an_orphaned_daemon_is_separated_from_a_tracked_one_and_from_a_one_shot() {
        let libexec = PathBuf::from("/usr/libexec/panio/bin");
        let ps = "\
  100   1 /usr/libexec/panio/bin/hdmicap daemon --device /dev/video0 --port 0
  101 100 /usr/libexec/panio/bin/linuxocr --json /tmp/frame.png
  200   1 /usr/libexec/panio/bin/hdmicap daemon --device /dev/video1 --port 0
  300   1 /usr/libexec/panio/bin/zigplug -d /dev/tty.usb on 1
  400   1 /usr/bin/vim daemon --device /dev/video9
";
        // pid 100 is the one daemon with a live discovery file.
        let strays = strays_in_ps(ps, std::slice::from_ref(&libexec), 999, &[100]);

        let mut untracked = Vec::new();
        let mut rest = Vec::new();
        for (pid, args) in strays {
            match untracked_of(pid, &args) {
                Some(u) => untracked.push(u),
                None => rest.push(pid),
            }
        }

        assert_eq!(
            untracked.iter().map(|u| u.pid).collect::<Vec<_>>(),
            vec![200],
            "only the daemon with no discovery file is untracked: 100 is tracked, \
             101 is its child, 300 is a one-shot, 400 is not a panio helper"
        );
        assert_eq!(untracked[0].name, "hdmicap");
        assert_eq!(untracked[0].devices, ["/dev/video1"]);
        assert_eq!(rest, vec![300], "the wedged one-shot stays a plain stray");
    }

    /// The row `panio daemons` prints for an orphan, end to end from the
    /// command line `ps` shows to the text an operator reads. On waldo,
    /// upgraded to 0.3.0, the two serialcap orphans holding serial ports
    /// printed as
    ///
    ///   serialcap    pid 14488    port ?    -
    ///
    /// because the inventory read only `--device` and serialcap names its
    /// ports with a repeatable `--interface` (#195). The `-` is what left an
    /// operator with no way to tell which target an orphan belonged to, so the
    /// devices reaching the row — all of them, for a daemon holding several —
    /// is the fix, not an incidental of it.
    #[test]
    fn the_inventory_row_names_every_port_an_orphan_is_sitting_on() {
        let serialcap = "/usr/libexec/panio/bin/serialcap daemon --port 0 \
                         --interface console=/dev/ttyUSB0@115200 \
                         --interface aux=/dev/ttyUSB1@9600:cts";
        let u = untracked_of(14488, serialcap).expect("this is a daemon");
        assert_eq!(
            u.devices_display(),
            "/dev/ttyUSB0, /dev/ttyUSB1",
            "both ports, and no baud or sense suffix"
        );

        // The video orphan that already worked, so the serial case did not buy
        // itself at the other's expense.
        let u = untracked_of(
            200,
            "/usr/libexec/panio/bin/hdmicap daemon --device /dev/video1",
        )
        .expect("this is a daemon");
        assert_eq!(u.devices_display(), "/dev/video1");

        // A daemon that holds no device still gets a row; `-` is right here.
        let u = untracked_of(300, "/usr/libexec/panio/bin/zigplug daemon").expect("still a daemon");
        assert_eq!(u.devices_display(), "-");
    }

    #[test]
    fn query_escape_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(query_escape("abc-_.~09"), "abc-_.~09");
        assert_eq!(
            query_escape("ws://127.0.0.1:5/stream?token=a&b"),
            "ws%3A%2F%2F127.0.0.1%3A5%2Fstream%3Ftoken%3Da%26b"
        );
        // Exactly the characters an interface name could carry that would
        // otherwise reshape a `?interface=` query string.
        assert_eq!(query_escape("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn sanitize_component_refuses_dot_and_dotdot() {
        assert_eq!(sanitize_component("."), "_");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component(""), "_");
        // Three dots is a harmless literal filename, not a traversal.
        assert_eq!(sanitize_component("..."), "...");
        assert_eq!(sanitize_component("pi5"), "pi5");
    }

    #[test]
    fn runtime_rel_singleton_vs_per_target() {
        assert_eq!(runtime_rel("zigplug", None), "zigplug");
        assert_eq!(runtime_rel("serialcap", Some("pi5")), "serialcap/pi5");
    }

    #[test]
    fn runtime_rel_sanitizes_instance() {
        // Path separators and other unsafe chars in a target name collapse to
        // `_`, so the instance is always a single path component.
        assert_eq!(runtime_rel("hdmicap", Some("a/b")), "hdmicap/a_b");
        assert_eq!(runtime_rel("hdmicap", Some("../x")), "hdmicap/.._x");
        assert_eq!(runtime_rel("hid", Some("")), "hid/_");
        assert_eq!(runtime_rel("hid", Some("nova-1.2")), "hid/nova-1.2");
    }

    fn fixture_meta(path: &Path, pid: u32) -> BinMeta {
        let (mtime_ns, size) = stat_identity(path).unwrap();
        BinMeta {
            bin: "test-bin".into(),
            path: path.to_string_lossy().into_owned(),
            mtime_ns,
            size,
            sha256: content_digest(path),
            pid: Some(pid),
        }
    }

    #[test]
    fn content_digest_matches_sha256_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("digest input with spaces");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            content_digest(&path).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            "the OS hash adapter must produce the standard SHA-256 abc vector"
        );
    }

    #[test]
    fn binary_content_comparison_ignores_metadata_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper");
        std::fs::write(&path, b"v1").unwrap();
        let meta = fixture_meta(&path, 42);
        assert!(
            meta.sha256.is_some(),
            "platform SHA-256 tool must be available"
        );
        assert_eq!(
            compare_binary(&meta, &path, 42, None),
            BinaryStatus::Current
        );
        let other = dir.path().join("other");
        std::fs::write(&other, b"v1").unwrap();
        assert_eq!(
            compare_binary(&meta, &other, 42, None),
            BinaryStatus::Current
        );
        std::fs::rename(&other, &path).unwrap();
        assert_eq!(
            compare_binary(&meta, &path, 42, None),
            BinaryStatus::Current
        );
        std::fs::write(&path, b"v2").unwrap();
        assert_eq!(
            compare_binary(&meta, &path, 42, None),
            BinaryStatus::Changed
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(compare_binary(&meta, &path, 42, None).is_stale(), None);
    }

    #[test]
    fn unknown_identity_never_recommends_or_selects_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper");
        std::fs::write(&path, b"v1").unwrap();
        let mut meta = fixture_meta(&path, 42);
        let status = compare_binary(&meta, &path, 43, None);
        assert_eq!(status.is_stale(), None, "digest belongs to another start");
        meta.sha256 = Some("not-a-digest".into());
        assert_eq!(compare_binary(&meta, &path, 42, None).is_stale(), None);
        meta.sha256 = None;
        assert_eq!(compare_binary(&meta, &path, 42, None).is_stale(), None);
        std::fs::write(&path, b"v2-larger").unwrap();
        let status = compare_binary(&meta, &path, 42, None);
        assert_eq!(
            status,
            BinaryStatus::Unknown("metadata changed; running content unavailable")
        );
        assert_eq!(status.is_stale(), None);
        assert!(!status.note().contains("restart"));
        assert!(BinaryStatus::Unknown("no recorded binary identity")
            .note()
            .contains("unknown"));
        let legacy: BinMeta = serde_json::from_value(serde_json::json!({
            "bin": "helper", "path": "helper", "mtime_ns": 0, "size": 2
        }))
        .unwrap();
        assert!(legacy.sha256.is_none());
        assert!(legacy.pid.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_executable_compares_content_after_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("sleep");
        std::fs::copy("/bin/sleep", &installed).unwrap();
        // Overlay filesystems and concurrent build subprocesses can briefly
        // retain a write handle after the copy. Wait only for ETXTBSY readiness;
        // other spawn errors fail immediately, and content assertions still run.
        let mut command = std::process::Command::new(&installed);
        command.arg("30");
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let mut child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.raw_os_error() == Some(libc::ETXTBSY)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(
                        Duration::from_millis(10)
                            .min(deadline.saturating_duration_since(std::time::Instant::now())),
                    );
                }
                Err(error) => panic!("copied executable fixture failed to spawn: {error}"),
            }
        };
        let pid = child.id();
        let mut meta = fixture_meta(&installed, pid);
        meta.sha256 = None;
        meta.pid = None;
        let replacement = dir.path().join("replacement");
        std::fs::copy("/bin/sleep", &replacement).unwrap();
        std::fs::rename(&replacement, &installed).unwrap();
        let equal = compare_binary(&meta, &installed, pid, running_digest(pid));
        std::fs::write(&installed, b"different executable").unwrap();
        let changed = compare_binary(&meta, &installed, pid, running_digest(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(equal, BinaryStatus::Current);
        assert_eq!(changed, BinaryStatus::Changed);
        assert!(
            running_digest(pid).is_none(),
            "exited executable is unavailable"
        );
    }

    /// Helper lookup has to try `<name>.exe` on Windows: every caller names
    /// helpers bare (`"hdmicap"`), and the packaged binaries carry the suffix,
    /// so without this no helper resolves at all on a Windows install.
    #[test]
    fn binary_names_adds_the_windows_suffix() {
        let names = binary_names("hdmicap");
        if cfg!(windows) {
            assert_eq!(names, vec!["hdmicap.exe", "hdmicap"]);
        } else {
            assert_eq!(names, vec!["hdmicap"]);
        }
        // An explicit .exe is never doubled up.
        assert_eq!(binary_names("hdmicap.exe"), vec!["hdmicap.exe"]);
    }

    /// A packaged helper must beat a stale one on the operator's `$PATH`.
    ///
    /// This is what makes "the daemon always runs the binary panio
    /// installed" true, and it had no test of its own: the one test that
    /// exercised the ordering did so by accident, and only on a host that
    /// happened to have a packaged helper (GitHub #206). Reversing the two
    /// loops in `find_binary` used to leave the whole suite green.
    #[test]
    fn helper_dirs_are_searched_before_path_and_cargo_bin() {
        let libexec = PathBuf::from("/usr/libexec/panio/bin");
        let on_path = PathBuf::from("/usr/local/bin");
        let cargo_bin = PathBuf::from("/home/u/.cargo/bin");
        let dirs = search_dirs(
            vec![libexec.clone()],
            vec![on_path.clone()],
            Some(cargo_bin.clone()),
        );
        assert_eq!(
            dirs,
            vec![libexec.clone(), on_path.clone(), cargo_bin.clone()]
        );

        // With the same name in all three, the helper dir wins.
        let names = vec!["hdmicap".to_string()];
        assert_eq!(
            first_match(&dirs, &names, |_| true),
            Some(libexec.join("hdmicap")),
            "a packaged helper must win over $PATH"
        );
        // With it missing from the helper dir, $PATH is next, then cargo bin.
        assert_eq!(
            first_match(&dirs, &names, |p| !p.starts_with(&libexec)),
            Some(on_path.join("hdmicap")),
        );
        assert_eq!(
            first_match(&dirs, &names, |p| p.starts_with(&cargo_bin)),
            Some(cargo_bin.join("hdmicap")),
        );
        assert_eq!(first_match(&dirs, &names, |_| false), None);
    }

    /// An absolute path in the platform's own syntax, for the plugin tests:
    /// `/opt/x` is not absolute on Windows.
    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", p.replace('/', "\\")))
        } else {
            PathBuf::from(p)
        }
    }

    /// Plugin helper dirs come after every bundled dir and before `$PATH`, so
    /// a same-name plugin helper never wins over the one panio installed
    /// (#303 keeps #206's guarantee), but still beats `$PATH`.
    #[test]
    fn plugin_dirs_follow_bundled_dirs_and_precede_path() {
        let user = abs("/home/u/.local/libexec/panio/bin");
        let system = abs("/usr/libexec/panio/bin");
        let plugin = plugin_helper_dir(&abs("/opt/lab-plugins"));
        let on_path = abs("/usr/local/bin");
        let helper = with_plugins(vec![user.clone(), system.clone()], vec![plugin.clone()]);
        assert_eq!(helper, vec![user.clone(), system.clone(), plugin.clone()]);
        let dirs = search_dirs(helper, vec![on_path.clone()], None);
        assert_eq!(
            dirs,
            vec![user, system.clone(), plugin.clone(), on_path.clone()]
        );

        let names = vec!["hdmicap".to_string()];
        assert_eq!(
            first_match(&dirs, &names, |p| p.starts_with(&system)
                || p.starts_with(&plugin)),
            Some(system.join("hdmicap")),
            "a plugin helper must not shadow a bundled one"
        );
        assert_eq!(
            first_match(&dirs, &names, |p| !p.starts_with(&system)
                && (p.starts_with(&plugin) || p.starts_with(&on_path))),
            Some(plugin.join("hdmicap")),
            "a plugin helper wins over $PATH"
        );
    }

    #[test]
    fn plugin_path_parsing_skips_empty_and_sets_aside_relative_entries() {
        let a = abs("/opt/lab-plugins");
        let b = abs("/srv/more");
        let raw = std::env::join_paths([
            PathBuf::new(),
            a.clone(),
            PathBuf::from("rel/dir"),
            b.clone(),
        ])
        .unwrap();
        let (roots, ignored) = parse_plugin_path(&raw);
        assert_eq!(roots, vec![a, b]);
        assert_eq!(ignored, vec![PathBuf::from("rel/dir")]);
        assert_eq!(
            parse_plugin_path(std::ffi::OsStr::new("")),
            (Vec::new(), Vec::new())
        );
    }

    /// The list attributes each helper to the dir `find_binary` would run it
    /// from, annotates only plugin entries, and reports a plugin helper that
    /// lost to a bundled one of the same name.
    #[test]
    fn helper_list_annotates_plugin_entries_and_reports_shadowed_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("libexec");
        let root = tmp.path().join("plugins");
        let pdir = plugin_helper_dir(&root);
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(bundled.join("hdmicap"), b"").unwrap();
        std::fs::write(pdir.join("hdmicap"), b"").unwrap();
        std::fs::write(pdir.join("labctl"), b"").unwrap();

        let (entries, shadowed) =
            helper_entries_in(std::slice::from_ref(&bundled), std::slice::from_ref(&root));
        assert_eq!(
            entries,
            vec![
                HelperEntry {
                    name: "hdmicap".into(),
                    plugin_root: None
                },
                HelperEntry {
                    name: "labctl".into(),
                    plugin_root: Some(root.clone())
                },
            ]
        );
        assert_eq!(shadowed, vec![("hdmicap".to_string(), root.clone())]);
        assert_eq!(helper_list_line(&entries[0]), "hdmicap");
        assert_eq!(
            helper_list_line(&entries[1]),
            format!("labctl\tplugin: {}", root.display())
        );
        assert_eq!(
            helper_list_line(&entries[1]).split_whitespace().next(),
            Some("labctl"),
            "the name stays the first field"
        );
    }

    /// Within one directory the Windows-suffixed name is tried first, so a
    /// packaged `hdmicap.exe` is found before an extension-less sibling.
    #[test]
    fn first_match_tries_every_candidate_name_in_order() {
        let dir = PathBuf::from("/opt/panio/libexec");
        let names = binary_names("hdmicap");
        let found = first_match(std::slice::from_ref(&dir), &names, |_| true).unwrap();
        assert_eq!(found, dir.join(&names[0]));
        if cfg!(windows) {
            assert_eq!(found, dir.join("hdmicap.exe"));
        } else {
            assert_eq!(found, dir.join("hdmicap"));
        }
    }

    /// The lookup is exercised through $PATH, which `find_binary` searches on
    /// every platform — so this runs the real resolution code rather than
    /// asserting on the shape of a name.
    #[test]
    fn find_binary_resolves_a_helper_through_path() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "panio-fake-helper.exe"
        } else {
            "panio-fake-helper"
        };
        let path = dir.path().join(name);
        std::fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let prev = std::env::var_os("PATH");
        let mut paths = vec![dir.path().to_path_buf()];
        if let Some(p) = &prev {
            paths.extend(std::env::split_paths(p));
        }
        // Safe: single-threaded test process; PATH is restored below.
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };

        // Named bare, as every caller names helpers.
        let found = find_binary("panio-fake-helper");

        match prev {
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(
            found.as_deref(),
            Some(path.as_path()),
            "a bare helper name must resolve to the real file"
        );
    }

    #[test]
    fn endpoint_parses_the_token_and_tolerates_its_absence() {
        let ep = Endpoint::from_json(r#"{"pid":42,"port":8724,"token":"abc"}"#).unwrap();
        assert_eq!(
            ep,
            Endpoint {
                pid: 42,
                port: 8724,
                token: Some("abc".into())
            }
        );
        // A daemon older than the token: still discoverable, no credential.
        let old = Endpoint::from_json(r#"{"pid":42,"port":8724}"#).unwrap();
        assert_eq!(old.token, None);
        assert!(Endpoint::from_json(r#"{"pid":42}"#).is_none());
        assert!(Endpoint::from_json("not json").is_none());
    }

    #[test]
    fn endpoint_urls_carry_the_token_only_where_a_browser_needs_it() {
        let ep = Endpoint {
            pid: 1,
            port: 5555,
            token: Some("t0k".into()),
        };
        assert_eq!(ep.base_url(), "http://127.0.0.1:5555");
        assert_eq!(ep.http_url("/"), "http://127.0.0.1:5555/?token=t0k");
        assert_eq!(
            ep.http_url("/?interface=console"),
            "http://127.0.0.1:5555/?interface=console&token=t0k"
        );
        let old = Endpoint {
            token: None,
            ..ep.clone()
        };
        assert_eq!(old.http_url("/"), "http://127.0.0.1:5555/");
    }

    /// The liveness rule, executed against a real file: the current process's
    /// pid is alive, pid 0 never is.
    #[test]
    fn endpoint_at_applies_the_liveness_rule() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        std::fs::write(
            dir.path().join("daemon.json"),
            format!(r#"{{"pid":{me},"port":7,"token":"x"}}"#),
        )
        .unwrap();
        let ep = endpoint_at(dir.path()).unwrap();
        assert_eq!((ep.port, ep.token.as_deref()), (7, Some("x")));
        std::fs::write(
            dir.path().join("daemon.json"),
            r#"{"pid":0,"port":7,"token":"x"}"#,
        )
        .unwrap();
        assert!(endpoint_at(dir.path()).is_none());
    }

    /// The request builders put the bearer header on the wire — checked
    /// against a real loopback listener, not by inspecting a string.
    #[test]
    fn endpoint_requests_carry_the_bearer_header() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = crate::stubhttp::serve_one(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let ep = Endpoint {
            pid: 1,
            port,
            token: Some("t0k".into()),
        };
        let resp = ep
            .get("/status?interface=console")
            .timeout(Duration::from_secs(5))
            .call()
            .unwrap();
        assert_eq!(resp.status(), 200);
        let req = server.join().unwrap();
        assert!(
            req.starts_with("GET /status?interface=console HTTP/1.1"),
            "{req}"
        );
        assert!(
            req.lines()
                .any(|l| l.eq_ignore_ascii_case("authorization: Bearer t0k")),
            "no bearer header in:\n{req}"
        );
    }

    #[test]
    fn runtime_root_honors_env_default_tmp() {
        // With no override, the root is the platform default: the hardcoded
        // /tmp on Unix, the per-user temp dir on Windows. The override is read
        // live, so only assert the shape (the env var is process-global in
        // tests; the lock keeps `with_runtime_root` from flipping it mid-way).
        let _guard = RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if std::env::var_os("PANIO_RUNTIME_BASE").is_none() {
            assert_eq!(runtime_root(), crate::platform::default_runtime_root());
            if cfg!(unix) {
                assert_eq!(runtime_root(), PathBuf::from("/tmp"));
            }
        }
    }

    /// Run `f` with `PANIO_RUNTIME_BASE` pointed at a fresh scratch root
    /// (so `runtime_base()` is `<root>/panio-<uid>`), restoring the
    /// previous value afterwards.
    fn with_runtime_root<R>(f: impl FnOnce(&Path) -> R) -> R {
        let _guard = RUNTIME_BASE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PANIO_RUNTIME_BASE");
        // Safe: serialized by the lock above; restored below.
        unsafe { std::env::set_var("PANIO_RUNTIME_BASE", root.path()) };
        let r = f(root.path());
        match prev {
            Some(p) => unsafe { std::env::set_var("PANIO_RUNTIME_BASE", p) },
            None => unsafe { std::env::remove_var("PANIO_RUNTIME_BASE") },
        }
        r
    }

    fn expected_base(root: &Path) -> PathBuf {
        root.join(format!("panio-{}", crate::platform::current_uid()))
    }

    /// A discovery file naming *this* process, so the liveness check passes
    /// and only the base check decides whether the readers trust it.
    fn plant_discovery(base: &Path, rel: &str, port: u16) {
        let dir = base.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("daemon.json"),
            format!(r#"{{"pid":{},"port":{port}}}"#, std::process::id()),
        )
        .unwrap();
    }

    /// A daemon that is SIGKILLed cannot remove its own discovery file, and
    /// the file it leaves behind names a pid that is briefly still alive — so
    /// the replacement's startup wait accepts it and reports a restart that
    /// never happened, quoting the dead daemon's port (GitHub #193). The
    /// replacement always gets a new pid, so refusing the one being replaced
    /// separates the two.
    #[test]
    fn wait_for_replacement_refuses_the_discovery_file_of_the_pid_it_replaces() {
        with_runtime_root(|root| {
            ensure_runtime_dir("hdmicap", Some("pi5")).unwrap();
            // A live pid (ours), exactly as a just-killed daemon's leftover
            // file looks while the kernel is still finishing with it.
            plant_discovery(&expected_base(root), "hdmicap/pi5", 36989);
            let me = std::process::id() as i32;

            // The unguarded wait is what reported the false success.
            assert_eq!(
                wait_for_daemon("hdmicap", Some("pi5"), Duration::from_millis(300)),
                Some("http://127.0.0.1:36989".to_string()),
                "the plain wait accepts any live discovery file"
            );

            // The guarded one does not, and says so by timing out.
            assert_eq!(
                wait_for_replacement("hdmicap", Some("pi5"), Duration::from_millis(300), me),
                None,
                "the pid being replaced must never answer for its replacement"
            );

            // A different pid in the file is somebody else's daemon, which is
            // a real answer: the guard must not reject everything.
            assert_eq!(
                wait_for_replacement(
                    "hdmicap",
                    Some("pi5"),
                    Duration::from_millis(300),
                    me.wrapping_add(1)
                ),
                Some("http://127.0.0.1:36989".to_string())
            );
        });
    }

    /// The other half of #193: the leftover file is removed once the killed
    /// process is gone, since it is not around to do it. Guarded by pid, so a
    /// replacement that has already published is never the one deleted.
    #[test]
    fn remove_discovery_for_pid_removes_only_the_file_naming_that_pid() {
        with_runtime_root(|root| {
            ensure_runtime_dir("hdmicap", Some("pi5")).unwrap();
            plant_discovery(&expected_base(root), "hdmicap/pi5", 36989);
            let path = expected_base(root).join("hdmicap/pi5").join("daemon.json");
            let me = std::process::id() as i32;

            assert!(
                !remove_discovery_for_pid("hdmicap", Some("pi5"), me.wrapping_add(1)),
                "a file naming another pid is not ours to delete"
            );
            assert!(path.exists(), "and must still be there");

            assert!(remove_discovery_for_pid("hdmicap", Some("pi5"), me));
            assert!(!path.exists(), "the killed daemon's file is reaped");
            // Idempotent: nothing left to remove is not a failure to report.
            assert!(!remove_discovery_for_pid("hdmicap", Some("pi5"), me));
        });
    }

    /// The *call site* of the guard above. `wait_for_replacement` only helps
    /// where a restart actually asks for it, and every path that replaces a
    /// capture daemon — `daemons restart`, the stale-binary restart in
    /// `serial watch` and `video watch` — picks between the two waits in one
    /// place, `crate::wait_for_started_daemon`. Choosing the unguarded wait
    /// there is the whole of GitHub #193: `daemons restart --stale` printed
    ///
    ///   hdmicap[lab-optiplex-1] restarted — http://127.0.0.1:36989
    ///
    /// while the replacement had died on the old daemon's advisory lock and
    /// 36989 was the *old* daemon's port. The two tests above pin the
    /// mechanism; this pins the wiring, so the branch cannot be dropped
    /// without a failure — the mechanism's own tests keep passing when it is.
    #[test]
    fn a_restart_never_reports_the_replaced_daemons_port_as_the_new_ones() {
        with_runtime_root(|root| {
            ensure_runtime_dir("hdmicap", Some("lab-optiplex-1")).unwrap();
            // #193's exact shape: the killed daemon's leftover file, naming a
            // pid the kernel has not finished with (ours) and the port the
            // restart wrongly reported back.
            plant_discovery(&expected_base(root), "hdmicap/lab-optiplex-1", 36989);
            let old = std::process::id() as i32;

            let err =
                crate::wait_for_started_daemon("hdmicap", Some("lab-optiplex-1"), Some(old), None)
                    .expect_err("a replacement must not be answered for by the pid it replaced");
            assert!(
                !err.to_string().contains("36989"),
                "the replaced daemon's port must never reach the operator as the \
                 new one's: {err:#}"
            );

            // A cold start replaces nothing, so the same file is a real answer:
            // the guard must not refuse every startup wait.
            assert_eq!(
                crate::wait_for_started_daemon("hdmicap", Some("lab-optiplex-1"), None, None)
                    .unwrap(),
                "http://127.0.0.1:36989"
            );
        });
    }

    /// The readers see a daemon when the base is the private directory the
    /// writer creates — the everyday path, which must keep working with the
    /// trust check in front of it.
    #[test]
    fn readers_trust_a_private_base_we_own() {
        with_runtime_root(|root| {
            ensure_runtime_dir("serialcap", Some("pi5")).unwrap();
            plant_discovery(&expected_base(root), "serialcap/pi5", 4321);

            assert_eq!(
                daemon_endpoint("serialcap", Some("pi5")).map(|ep| ep.port),
                Some(4321)
            );
            assert_eq!(
                daemon_pid("serialcap", Some("pi5")),
                Some(std::process::id() as i32)
            );
            let listed = list_discovered();
            assert_eq!(listed.len(), 1, "one live daemon");
            assert_eq!(listed[0].name, "serialcap");
            assert_eq!(listed[0].instance.as_deref(), Some("pi5"));
            assert_eq!(listed[0].port, Some(4321));
        });
    }

    /// A symlink where the base should be — a squatter's, pointing wherever
    /// they like — makes every reader answer "not running", even though the
    /// discovery file behind it names a live pid.
    #[cfg(unix)]
    #[test]
    fn readers_ignore_a_symlinked_base() {
        use std::os::unix::fs::PermissionsExt;
        with_runtime_root(|root| {
            let real = root.join("elsewhere");
            std::fs::create_dir(&real).unwrap();
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
            plant_discovery(&real, "serialcap/pi5", 4321);
            std::os::unix::fs::symlink(&real, expected_base(root)).unwrap();

            assert_eq!(
                daemon_endpoint("serialcap", Some("pi5")).map(|ep| ep.port),
                None
            );
            assert_eq!(daemon_pid("serialcap", Some("pi5")), None);
            assert!(list_discovered().is_empty());
        });
    }

    /// A base we own but left group/world-accessible is refused by the
    /// readers: a base we own that was left too open (an older panio, or
    /// the ssh ControlMaster path) is tightened to 0700 and then trusted, so
    /// an upgrade never makes a running daemon look stopped.
    #[cfg(unix)]
    #[test]
    fn readers_tighten_an_open_base_we_own() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        with_runtime_root(|root| {
            let base = expected_base(root);
            std::fs::create_dir(&base).unwrap();
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
            plant_discovery(&base, "serialcap/pi5", 4321);

            assert_eq!(
                daemon_endpoint("serialcap", Some("pi5")).map(|ep| ep.port),
                Some(4321)
            );
            assert_eq!(
                std::fs::metadata(&base).unwrap().mode() & 0o777,
                0o700,
                "reader tightened the base"
            );
            assert!(daemon_pid("serialcap", Some("pi5")).is_some());
        });
    }

    /// The daemon's stderr log is created readable by nobody else.
    #[cfg(unix)]
    #[test]
    fn daemon_log_is_created_private() {
        use std::os::unix::fs::MetadataExt;
        with_runtime_root(|_| {
            let log = create_log("serialcap", Some("pi5")).unwrap();
            assert_eq!(log.metadata().unwrap().mode() & 0o777, 0o600);
            assert!(log_path("serialcap", Some("pi5")).is_file());
        });
    }

    /// Only argv[0] decides whether a process is a stray helper. Rows whose
    /// *arguments* mention the helper dir — the `cargo install --root` that
    /// `make install` runs, an `ls`, an editor — used to match, and
    /// `daemons stop --all` then TERMed them.
    #[test]
    fn strays_match_the_program_path_not_the_arguments() {
        let dirs = vec![PathBuf::from("/opt/panio/libexec/bin")];
        let ps = "\
  100     1 /opt/panio/libexec/bin/zigplug -d /dev/ttyUSB0 on 1
  101     1 cargo install --path zigplug --root /opt/panio/libexec
  102     1 ls -l /opt/panio/libexec/bin
  103     1 vim /opt/panio/libexec/bin/notes.txt
  104     1 /opt/panio/libexec/binx/other
  105     1 /opt/panio/libexec/bin/sub/deeper
";
        let pids: Vec<i32> = strays_in_ps(ps, &dirs, 999, &[])
            .into_iter()
            .map(|(pid, _)| pid)
            .collect();
        assert_eq!(pids, vec![100]);
    }

    /// Our own pid, the known daemons, and anything descended from a known
    /// daemon are not strays: on Linux the recorded netboot pid is `sudo`'s
    /// and the real netbootd is its child; hdmicap's OCR helper is likewise
    /// a child of a known daemon. The wedged one-shot is what remains.
    #[test]
    fn strays_exclude_us_known_pids_and_their_descendants() {
        let dirs = vec![PathBuf::from("/usr/libexec/panio/bin")];
        let ps = "\
  200     1 sudo env NO_COLOR=1 /usr/libexec/panio/bin/netbootd --host-ip 192.168.99.1
  201   200 /usr/libexec/panio/bin/netbootd --host-ip 192.168.99.1
  202   201 /usr/libexec/panio/bin/bpf-helper
  300     1 /usr/libexec/panio/bin/hdmicap daemon --device /dev/video0
  301   300 /usr/libexec/panio/bin/linuxocr
  400     1 /usr/libexec/panio/bin/zigplug -d /dev/ttyUSB1 on 1
  500     1 /usr/libexec/panio/bin/serialcap stop
";
        let strays = strays_in_ps(ps, &dirs, 500, &[200, 300]);
        assert_eq!(
            strays,
            vec![(
                400,
                "/usr/libexec/panio/bin/zigplug -d /dev/ttyUSB1 on 1".to_string()
            )]
        );
    }

    /// A daemon that already exited during startup will not come up, so it is
    /// `helper_failed` with its code; one still running is a `timeout`
    /// (review finding F10, #194).
    #[cfg(unix)]
    #[test]
    fn start_failure_tells_an_exited_daemon_from_a_slow_one() {
        use crate::error::{Kind, PanioError};
        let kind_of = |e: anyhow::Error| {
            let pe = e.downcast::<PanioError>().expect("classified");
            (pe.kind, pe.child_exit)
        };
        // Wait for the exit without a fixed sleep; std caches the status, so
        // start_failure's own try_wait still sees it.
        let mut fresh = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fresh.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let e = start_failure(
            "panio-test-daemon",
            None,
            Duration::from_secs(5),
            Some(&mut fresh),
        );
        assert_eq!(kind_of(e), (Kind::HelperFailed, Some(3)));

        let mut slow = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let e = start_failure(
            "panio-test-daemon",
            None,
            Duration::from_secs(5),
            Some(&mut slow),
        );
        assert_eq!(kind_of(e), (Kind::Timeout, None));
        let _ = slow.kill();
        let _ = slow.wait();

        let e = start_failure("panio-test-daemon", None, Duration::from_secs(5), None);
        assert_eq!(kind_of(e), (Kind::Timeout, None));

        // A helper that forked and exited 0 may still bring its daemon up.
        let mut forked = std::process::Command::new("true").spawn().unwrap();
        let _ = forked.wait();
        let e = start_failure(
            "panio-test-daemon",
            None,
            Duration::from_secs(5),
            Some(&mut forked),
        );
        assert_eq!(kind_of(e), (Kind::Timeout, None));
    }
}
