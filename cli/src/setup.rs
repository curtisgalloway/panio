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

//! `panio setup` — build and install panio's binaries from a source clone.
//!
//! The panio CLI installs via `cargo install` into `~/.cargo/bin` — the one
//! user-facing command. The helpers (hdmicap, serialcap, netbootd, cambrionix,
//! hidrig, the OCR helper, zigplug) install into the private libexec dir
//! (`daemons::libexec_dir()`, `~/.local/libexec/panio/bin`) so they stay off
//! PATH; panio resolves them itself and `panio helper <name> …` runs one
//! directly. On macOS setup also setuid-installs the netbootd bpf-helper (the
//! only root component) and compiles the visionocr OCR helper; on Linux it
//! checks dialout/video group membership and installs linuxocr. The legacy
//! `tftp-now` brew step is gone — netbootd serves TFTP.
//!
//! Without a checkout (a packaged install — Homebrew, .deb, tarball),
//! `setup` skips the builds and runs just the platform steps against the
//! installed binaries: see [`run_packaged`].

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Result};

/// The helper crates `setup` builds and installs into libexec, in order. The
/// `cli` crate (the `panio` binary itself) installs separately onto PATH.
const HELPER_CRATES: [&str; 10] = [
    "hdmicap",
    "serialcap",
    "netbootd",
    "cambrionix",
    "dsdrelay",
    "hidrig",
    "ch9329",
    "shellyplug",
    "switchport",
    "amt",
];

/// The Go helpers `setup` builds with `go build` into libexec when a Go
/// toolchain is present. Not Cargo crates, so not in [`HELPER_CRATES`];
/// `scripts/ci-coverage-check.sh` reads this list to check every Go module in
/// the repo is installable from a source clone.
const GO_HELPERS: [&str; 1] = ["jetkvm"];

fn is_repo_root(d: &Path) -> bool {
    d.join("Makefile").is_file()
        && d.join("ocr").is_dir()
        && d.join("cli/Cargo.toml").is_file()
        && d.join("hdmicap/Cargo.toml").is_file()
}

/// Locate the panio source checkout: the current directory and its parents.
/// (The installed binary has no `__file__` to climb from; `make install` and
/// hand-run setups both execute inside the clone.)
pub fn find_repo_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut d: Option<&Path> = Some(cwd.as_path());
    while let Some(p) = d {
        if is_repo_root(p) {
            return Some(p.to_path_buf());
        }
        d = p.parent();
    }
    None
}

fn cargo_bin() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".cargo/bin")
}

fn user_in_group(group: &str) -> bool {
    Command::new("id")
        .arg("-nG")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .any(|g| g == group)
        })
        .unwrap_or(false)
}

fn group_exists(group: &str) -> bool {
    Command::new("getent")
        .args(["group", group])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The account name for `sudo usermod -aG <group> <user>`: `$USER`, required
/// non-empty. Extracted from `ensure_linux_groups` so the failure mode — no
/// sudo invocation with an empty or missing username — is testable without a
/// real system group.
fn linux_group_user(group: &str, reason: &str) -> Result<String> {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "$USER is not set, so panio cannot add this account to the \
             '{group}' group ({reason}); set $USER and re-run `panio setup`, \
             or run `sudo usermod -aG {group} <your-username>` yourself"
            )
        })
}

/// Add the user to dialout/video if needed (Linux). Returns `Ok(true)` if
/// anything changed (a re-login is needed for it to take effect).
///
/// Errors if `$USER` is unset or empty at the point a group actually needs
/// joining. The old code read `$USER` with `unwrap_or_default()` and ran
/// `sudo usermod -aG <group> ""` on an unset one — a real sudo prompt for a
/// command guaranteed to fail, instead of a clear error up front (Review low
/// #11). An agent-invoked shell without a login environment is exactly the
/// case that can hit this; on Windows/macOS `group_exists` is always false
/// here, so `$USER` (which on Windows isn't even the right variable name) is
/// never consulted.
fn ensure_linux_groups() -> Result<bool> {
    let mut changed = false;
    for (group, reason) in [
        ("dialout", "serial port access (/dev/ttyUSB*, /dev/ttyACM*)"),
        ("video", "V4L2 capture device access (/dev/video*)"),
    ] {
        if !group_exists(group) {
            continue;
        }
        if user_in_group(group) {
            println!("  ✓ {group:12} already a member");
            continue;
        }
        let user = linux_group_user(group, reason)?;
        let ok = Command::new("sudo")
            .args(["usermod", "-aG", group, &user])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("  ✓ {group:12} added ({reason})");
            changed = true;
        } else {
            eprintln!("  ✗ {group:12} could not add ({reason})");
        }
    }
    Ok(changed)
}

/// netbootd's macOS raw-frame send path needs a /dev/bpf descriptor, which
/// only root can open. The setuid bpf-helper is the ONLY root component; its
/// sole job is opening /dev/bpf and handing the fd to the unprivileged
/// netbootd. Installs and upgrades (cargo and packages alike) reset the mode,
/// so the setuid bit must be re-applied after each one.
///
/// Mode 4755 (world-executable) is acceptable because the helper gates
/// itself rather than relying on the file mode: it refuses any caller whose
/// real uid is not the owner of the directory it lives in (the installing
/// user), refuses to bind the default-route interface, and hands out only a
/// write-only descriptor with a reject-all filter. Another local user can
/// run it and gets nothing from it.
///
/// Before touching the file, [`helper_safe_to_setuid`] confirms it is what
/// the invoking user installed — a regular file (not a symlink) they own, or
/// the root-owned setuid helper a previous run already produced. Anything
/// else is refused rather than promoted to setuid-root.
fn setuid_bpf_helper(helper: &Path) {
    if let Err(why) = helper_safe_to_setuid(helper) {
        eprintln!(
            "  ! refusing to setuid {}: {why}. Reinstall the helper \
             (`make install` or `brew reinstall panio`) and re-run `panio setup`.",
            helper.display()
        );
        return;
    }
    println!("  … installing netbootd-bpf-helper setuid-root (one-time sudo)");
    let chown = Command::new("sudo")
        .args(["chown", "root:wheel"])
        .arg(helper)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let chmod = Command::new("sudo")
        .args(["chmod", "4755"])
        .arg(helper)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if chown && chmod {
        println!("  ✓ {:12} setuid-root  {}", "bpf-helper", helper.display());
    } else {
        eprintln!(
            "  ! could not setuid netbootd-bpf-helper; the netboot send path \
             falls back to the kernel (broken on macOS 15+). Re-run \
             `panio setup` with sudo access to fix."
        );
    }
}

/// Whether `helper` is a file `panio setup` may promote to setuid-root:
/// a regular file — `symlink_metadata`, so a symlink is seen as a symlink and
/// refused rather than followed — that is either owned by the invoking user
/// (freshly installed by `cargo install` / `make install` / the keg) or
/// already root-owned with the setuid bit (a previous run). A file some other
/// uid placed there is refused: `sudo chown root` + `chmod 4755` on it would
/// hand that uid a root-run binary of their choosing.
#[cfg(unix)]
fn helper_safe_to_setuid(helper: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::symlink_metadata(helper).map_err(|e| anyhow!("cannot stat: {e}"))?;
    if !meta.file_type().is_file() {
        bail!("not a regular file (a symlink?)");
    }
    // Direct getuid rather than platform::current_uid(): that wrapper's
    // contract excludes authorization decisions (it is a hash on Windows), and
    // this compares against a real file owner. Unix-only, so it is exact here.
    let me = unsafe { libc::getuid() };
    // POSIX setuid bit; spelled out because libc::S_ISUID is u16 on macOS and
    // u32 on Linux, so a cast is needed on one and flagged on the other.
    const S_ISUID: u32 = 0o4000;
    let already_setuid_root = meta.uid() == 0 && meta.mode() & S_ISUID != 0;
    if meta.uid() != me && !already_setuid_root {
        bail!(
            "owned by uid {}, not the invoking user (uid {me})",
            meta.uid()
        );
    }
    Ok(())
}

/// setuid is a Unix concept; the macOS-only caller never runs here.
#[cfg(not(unix))]
fn helper_safe_to_setuid(_helper: &Path) -> Result<()> {
    bail!("setuid is not supported on this platform")
}

/// Finish platform setup for a packaged install (Homebrew, .deb, tarball) —
/// Build the venv `ocr/rapidocr` re-execs into, when a lab file asks for it.
///
/// Only built when some target sets `ocr_mode = "gui"`. It is ~317 MB
/// (onnxruntime 58 MB, PP-OCRv6 models 31 MB, numpy/opencv the rest) and most
/// control hosts never look at a GUI screen, so it is opt-in rather than part
/// of every setup.
///
/// A venv rather than a system install because Pi OS is PEP 668-managed and
/// refuses one — better a self-contained directory than asking anyone to reach
/// for `--break-system-packages`.
///
/// `opencv-python-headless` is forced over the `opencv-python` rapidocr pulls
/// in: the full build needs `libGL.so.1`, absent on a headless Pi OS, and the
/// failure is an ImportError at first OCR rather than at install time.
#[cfg(target_os = "linux")]
fn install_rapidocr_venv(libexec: &Path, lab_flag: Option<&str>) {
    if !lab_wants_gui_ocr(lab_flag) {
        println!(
            "  … rapidocr venv: skipped (no target sets video ocr_mode = \"gui\"; \
             it is ~317 MB — set the field and re-run to install)"
        );
        return;
    }
    let venv = libexec.join("ocr-venv");
    if venv.join("bin/python3").is_file() {
        println!("  ✓ {:12} {}", "ocr-venv", venv.display());
        return;
    }
    println!("  … building the rapidocr venv (~317 MB, a few minutes)…");
    let made = Command::new("python3")
        .args(["-m", "venv"])
        .arg(&venv)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        println!("  ! rapidocr venv: `python3 -m venv` failed (is python3-venv installed?)");
        return;
    }
    let pip = venv.join("bin/pip");
    let installed = Command::new(&pip)
        .args(["install", "--quiet", "rapidocr>=3,<4", "onnxruntime"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !installed {
        println!("  ! rapidocr venv: pip install failed; GUI OCR falls back to tesseract");
        return;
    }
    let _ = Command::new(&pip)
        .args(["uninstall", "-y", "-q", "opencv-python"])
        .status();
    let headless = Command::new(&pip)
        .args([
            "install",
            "--quiet",
            "--force-reinstall",
            "--no-deps",
            "opencv-python-headless",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if headless {
        println!("  ✓ {:12} {}", "ocr-venv", venv.display());
    } else {
        println!("  ! rapidocr venv: headless opencv install failed; it will fail on libGL");
    }
}

#[cfg(not(target_os = "linux"))]
fn install_rapidocr_venv(_libexec: &Path, _lab_flag: Option<&str>) {}

/// Does the user's lab file ask for GUI-mode OCR anywhere?
///
/// Read as text rather than parsed: this runs before any lab is loaded, and the
/// only question is whether to spend 317 MB. `lab_flag` resolves the same way
/// every other lab-reading command does (`--lab`, then `$PANIO_LAB`, then
/// the default path) — this used to always read the default path outright,
/// so `panio setup --lab other.toml` (or `$PANIO_LAB` pointed elsewhere)
/// judged the venv against the wrong file (Review low #11).
#[cfg(target_os = "linux")]
fn lab_wants_gui_ocr(lab_flag: Option<&str>) -> bool {
    let Some(path) = crate::model::resolve_lab_path(lab_flag) else {
        return false;
    };
    std::fs::read_to_string(path)
        .map(|s| s.contains("ocr_mode") && s.contains("\"gui\""))
        .unwrap_or(false)
}

/// Verify the portable Windows layout: helpers alongside the CLI in `libexec`.
///
/// This is `panio setup`'s whole job on Windows — see [`run_packaged`]. It
/// reports rather than repairs, because the fix (re-extract the zip, or
/// reinstall via winget) is the user's to make.
#[cfg(windows)]
fn check_windows_layout() {
    let helpers = [
        "hdmicap",
        "serialcap",
        "netbootd",
        "cambrionix",
        "dsdrelay",
        "hidrig",
        "ch9329",
        "shellyplug",
        "switchport",
        "amt",
    ];
    println!("\nChecking the installed helper layout…");
    let mut missing = Vec::new();
    for h in helpers {
        match crate::daemons::find_binary(h) {
            Some(p) => println!("  ✓ {h:12} {}", p.display()),
            None => missing.push(h),
        }
    }
    if missing.is_empty() {
        return;
    }
    println!(
        "  ! missing helpers: {}\n\
         \x20   Expected them in a `libexec` directory beside panio.exe. \
         Re-extract the release zip keeping its directory structure, or \
         reinstall with `winget install CurtisGalloway.Panio`.",
        missing.join(", ")
    );
}

#[cfg(not(windows))]
fn check_windows_layout() {}

/// One platform-specific step of a packaged (no-checkout) setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackagedStep {
    /// macOS: make the installed netbootd-bpf-helper setuid-root.
    SetuidBpfHelper,
    /// Windows: check the portable zip layout is intact.
    WindowsLayout,
    /// Linux: add the user to the device-access groups.
    LinuxGroups,
    /// Linux: hint at installing tesseract for OCR.
    TesseractHint,
}

/// The packaged-setup steps for `os` (a `std::env::consts::OS` value). Kept
/// as a pure function so each CI platform can assert its own steps: the
/// Linux group step once sat under a Windows branch and nothing noticed.
fn packaged_steps(os: &str) -> Vec<PackagedStep> {
    match os {
        "macos" => vec![PackagedStep::SetuidBpfHelper],
        "windows" => vec![PackagedStep::WindowsLayout],
        _ => vec![PackagedStep::LinuxGroups, PackagedStep::TesseractHint],
    }
}

/// no source checkout, so no builds: setuid the installed bpf-helper on
/// macOS (located via `find_binary`, which knows the per-user libexec, the
/// Homebrew keg, and `/usr/libexec/panio/bin`), and check group
/// membership on Linux. Building or refreshing the daemons, OCR helper, and
/// zigplug still needs a clone (`make install`).
pub fn run_packaged() -> Result<()> {
    println!("No source checkout found — finishing setup for the installed panio.");
    for step in packaged_steps(std::env::consts::OS) {
        match step {
            PackagedStep::SetuidBpfHelper => {
                match crate::daemons::find_binary("netbootd-bpf-helper") {
                    Some(helper) => setuid_bpf_helper(&helper),
                    None => {
                        println!("  … netbootd-bpf-helper not found; skipping setuid install")
                    }
                }
            }
            // Nothing to grant on Windows: there is no setuid bit, no dialout
            // group, and the OCR helper has no Windows build. The one thing
            // worth checking is that the portable layout is intact, since a zip
            // extracted without its `libexec` directory yields a CLI that runs
            // and then fails on the first channel it needs a helper for.
            PackagedStep::WindowsLayout => check_windows_layout(),
            PackagedStep::LinuxGroups => {
                if ensure_linux_groups()? {
                    println!(
                        "\nNote: group changes take effect after you log out and back in \
                         (or run `newgrp dialout` in the current shell)."
                    );
                }
            }
            PackagedStep::TesseractHint => {
                if crate::daemons::find_binary("tesseract").is_none() {
                    println!(
                        "  ! tesseract not found — install it for OCR:\n\
                         \x20   sudo apt-get install tesseract-ocr"
                    );
                }
            }
        }
    }
    println!(
        "\nSetup complete. (Rebuilding the daemons, OCR helper, or zigplug \
         needs a source checkout — see `make install` in the repo.)"
    );
    println!("Agent skills shipped with the package — list them with `panio skill`.");
    Ok(())
}

/// A step of the source-checkout setup that [`run`] may perform.
///
/// Every step `run` can skip is gated on membership of [`source_steps`], and
/// the `--rust-only` completion message is derived from the same list, so the
/// message cannot claim one thing while the code does another. That is not
/// hypothetical: the bundled-skills copy sat below the `--rust-only` return
/// while the message named only OCR, setuid and zigplug, so the skills were
/// skipped silently and `panio skill` came up empty with nothing saying why
/// (GitHub #207).
///
/// Same shape, and for the same reason, as [`PackagedStep`] above, whose
/// comment records the Linux group step once sitting under a Windows branch
/// with nothing noticing.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum SourceStep {
    /// Linux: add the user to the device-access groups (needs sudo).
    LinuxGroups,
    /// Copy the bundled `SKILL.md` guides into the per-user data dir.
    InstallSkills,
    /// Drop pre-libexec helper copies from `~/.cargo/bin`.
    DropStaleCopies,
    /// macOS: make the installed netbootd-bpf-helper setuid-root (needs sudo).
    SetuidBpfHelper,
    /// Build or copy the platform's OCR helper.
    OcrHelper,
    /// Install the zigplug power helper (needs uv).
    Zigplug,
    /// Build and install the jetkvm hid helper (Go; needs `go`).
    JetKvm,
}

impl SourceStep {
    /// How the step is named in the `--rust-only` completion message.
    fn label(self) -> &'static str {
        match self {
            Self::LinuxGroups => "group membership",
            Self::InstallSkills => "skills",
            Self::DropStaleCopies => "stale-copy cleanup",
            Self::SetuidBpfHelper => "setuid",
            Self::OcrHelper => "OCR",
            Self::Zigplug => "zigplug",
            Self::JetKvm => "jetkvm",
        }
    }

    /// Whether `--rust-only` keeps this step.
    ///
    /// The fast path exists to skip what needs sudo or a second toolchain
    /// while iterating on the Rust code. Everything else belongs on it:
    /// copying three `SKILL.md` files and deleting stale binaries need
    /// neither, and skipping them only ever produced a half-installed tree.
    fn on_the_fast_path(self) -> bool {
        match self {
            Self::InstallSkills | Self::DropStaleCopies => true,
            Self::LinuxGroups
            | Self::SetuidBpfHelper
            | Self::OcrHelper
            | Self::Zigplug
            | Self::JetKvm => false,
        }
    }
}

/// The skippable steps [`run`] performs on `os`, in order. With `rust_only`,
/// only those needing nothing beyond cargo.
///
/// Parameterized by `os` because the answer differs — there is no setuid bit
/// to set off macOS and no group to join off Linux — and a list that claimed
/// otherwise would mis-report what was skipped on two platforms out of three.
/// The unconditional steps (building the helper crates and the CLI) are not
/// here: nothing can skip them, so there is no decision to record.
fn source_steps(os: &str, rust_only: bool) -> Vec<SourceStep> {
    let mut steps = vec![SourceStep::InstallSkills, SourceStep::DropStaleCopies];
    if os == "linux" {
        steps.insert(0, SourceStep::LinuxGroups);
    }
    if os == "macos" {
        steps.push(SourceStep::SetuidBpfHelper);
    }
    steps.push(SourceStep::OcrHelper);
    steps.push(SourceStep::Zigplug);
    steps.push(SourceStep::JetKvm);
    if rust_only {
        steps.retain(|s| s.on_the_fast_path());
    }
    steps
}

/// What `--rust-only` leaves undone on `os`: the difference between the full
/// list and the fast one, so the message is derived from the list `run` gates
/// on rather than from a second copy of the same prose.
fn rust_only_skips(os: &str) -> Vec<SourceStep> {
    let fast = source_steps(os, true);
    source_steps(os, false)
        .into_iter()
        .filter(|s| !fast.contains(s))
        .collect()
}

/// The line `--rust-only` ends on, naming exactly what it skipped.
///
/// `skills_ok` is whether the skills copy actually landed: claiming it did
/// when it did not would be the same defect as #207 in the other direction.
fn rust_only_done_message(os: &str, skills_ok: bool) -> String {
    let skipped: Vec<&str> = rust_only_skips(os).into_iter().map(|s| s.label()).collect();
    let installed = if skills_ok {
        "Rust crates and skills installed"
    } else {
        "Rust crates installed; the skills copy did not succeed (see above)"
    };
    format!(
        "{installed} (skipped {} — run `panio setup`).",
        skipped.join(", ")
    )
}

/// What [`zigplug_step`] did, so the decision can be driven by a test without
/// a uv install standing by.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ZigplugOutcome {
    /// Off this run's step list — `run` never looked at the checkout or uv.
    Skipped,
    /// No `zigplug/pyproject.toml` in the checkout.
    SourceMissing,
    /// uv is not installed.
    UvMissing,
    Installed,
    Failed,
}

/// zigplug: Python (zigpy-znp) Zigbee smart plug helper, installed as a uv
/// tool. `UV_TOOL_BIN_DIR` points the shim at libexec (the venv stays in uv's
/// tool dir) so the command resolves from power hooks without living on PATH.
/// The uninstall first clears any pre-libexec shim from uv's default bin dir
/// (`~/.local/bin`).
///
/// `will` is membership of [`source_steps`], and the early return on `false`
/// is the whole point: this was the one skippable block in [`run`] written
/// without that gate, so `--rust-only` shelled out to uv — a second
/// toolchain, the very thing the fast path exists to avoid — and then printed
/// a completion line naming zigplug among the steps it had skipped.
fn zigplug_step(will: bool, repo: &Path, libexec: &Path) -> ZigplugOutcome {
    if !will {
        return ZigplugOutcome::Skipped;
    }
    let zigplug_dir = repo.join("zigplug");
    if !zigplug_dir.join("pyproject.toml").is_file() {
        println!("  … zigplug: source not found, skipped");
        return ZigplugOutcome::SourceMissing;
    }
    let Some(uv) = crate::daemons::find_binary("uv") else {
        println!("  … zigplug: uv not found (https://docs.astral.sh/uv), skipped");
        return ZigplugOutcome::UvMissing;
    };
    let _ = Command::new(&uv)
        .args(["tool", "uninstall", "zigplug"])
        .output();
    let ok = Command::new(&uv)
        .env("UV_TOOL_BIN_DIR", libexec)
        .args(["tool", "install", "--force", "--quiet"])
        .arg(&zigplug_dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("  ✓ {:12} {}", "zigplug", libexec.join("zigplug").display());
    } else {
        eprintln!("  ! zigplug: uv tool install failed, skipped");
    }
    // Belt and braces: an orphaned pre-libexec shim survives a lost uv
    // receipt; remove it so PATH can't resolve a stale zigplug.
    if let Some(stale) = dirs::home_dir().map(|h| h.join(".local/bin/zigplug")) {
        if stale.is_file() && std::fs::remove_file(&stale).is_ok() {
            println!("  ✓ removed stale {}", stale.display());
        }
    }
    if ok {
        ZigplugOutcome::Installed
    } else {
        ZigplugOutcome::Failed
    }
}

/// What [`jetkvm_step`] did, so the decision can be driven by a test without a
/// Go toolchain standing by.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum JetKvmOutcome {
    /// Off this run's step list — `run` never looked at the checkout or `go`.
    Skipped,
    /// No `jetkvm/go.mod` in the checkout.
    SourceMissing,
    /// `go` is not installed.
    GoMissing,
    Installed,
    Failed,
}

/// jetkvm: the Go hid helper for JetKVM network KVMs, built from the checkout
/// with `go build` straight into libexec (it has no uv-style shim to place).
/// Not a Cargo crate, so it is not in [`HELPER_CRATES`]; a machine without a Go
/// toolchain simply does not get it, and says so.
///
/// `will` is membership of [`source_steps`]; like [`zigplug_step`], the early
/// return on `false` is what keeps `--rust-only` off a second toolchain.
fn jetkvm_step(will: bool, repo: &Path, libexec: &Path) -> JetKvmOutcome {
    if !will {
        return JetKvmOutcome::Skipped;
    }
    let [name] = GO_HELPERS;
    let dir = repo.join(name);
    if !dir.join("go.mod").is_file() {
        println!("  … jetkvm: source not found, skipped");
        return JetKvmOutcome::SourceMissing;
    }
    let Some(go) = crate::daemons::find_binary("go") else {
        println!("  … jetkvm: go not found (https://go.dev/dl), skipped");
        return JetKvmOutcome::GoMissing;
    };
    let dest = libexec.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let ok = Command::new(&go)
        .current_dir(&dir)
        .args(["build", "-trimpath", "-o"])
        .arg(&dest)
        .arg(".")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("  ✓ {name:12} {}", dest.display());
        JetKvmOutcome::Installed
    } else {
        eprintln!("  ! jetkvm: go build failed, skipped");
        JetKvmOutcome::Failed
    }
}

/// Agent skills: copy the bundled SKILL.md guides into the per-user data dir
/// so `panio skill` finds them when the installed CLI runs outside this
/// tree. From a checkout the repo copy is used directly, so this keeps an
/// installed panio in sync. (Linux packages ship them to /usr/share, so
/// `run_packaged` does not do this; both source-checkout paths do, including
/// `--rust-only`, since it needs no sudo and no toolchain.)
///
/// Returns whether anything was installed, so the caller's completion message
/// can say what actually happened rather than assuming.
fn install_skills(repo: &Path) -> bool {
    match crate::skills::install_bundled(repo) {
        Ok(0) => {
            println!(
                "  … skills: none found under {}",
                repo.join("skills").display()
            );
            false
        }
        Ok(n) => {
            let dst = crate::skills::user_skills_dir().unwrap_or_default();
            println!("  ✓ {:12} {n} installed → {}", "skills", dst.display());
            true
        }
        Err(e) => {
            eprintln!("  ! skills: {e}");
            false
        }
    }
}

/// Run the local setup from a source checkout at `repo`. With `rust_only`,
/// perform only the steps needing nothing beyond cargo — see [`source_steps`],
/// which decides that and which every skippable step below is gated on.
/// `lab_flag` is `--lab`, if given; it decides which lab file `install_rapidocr_venv`
/// checks for `ocr_mode = "gui"`.
pub fn run(repo: &Path, rust_only: bool, lab_flag: Option<&str>) -> Result<()> {
    let bin_dir = cargo_bin();
    let libexec_root = crate::daemons::libexec_root()
        .ok_or_else(|| anyhow!("could not determine the home directory"))?;
    // cargo install --root appends bin/ itself; keep in sync with
    // daemons::libexec_dir().
    let libexec = libexec_root.join("bin");
    std::fs::create_dir_all(&libexec)?;

    // The one list every skippable step below consults, so what `--rust-only`
    // does and what its closing message claims cannot disagree (#207).
    let steps = source_steps(std::env::consts::OS, rust_only);
    let will = |s: SourceStep| steps.contains(&s);

    if !rust_only {
        if cfg!(target_os = "macos") {
            println!("  ℹ macOS: netbootd serves DHCP+TFTP; no system TFTP tool needed.");
        } else {
            println!(
                "  ℹ Linux: before building, ensure system packages are installed:\n\
                 \x20   sudo apt-get install build-essential pkg-config libudev-dev libclang-dev cmake nasm"
            );
        }
    }
    if will(SourceStep::LinuxGroups) {
        println!("\nChecking group membership…");
        if ensure_linux_groups()? {
            println!(
                "\nNote: group changes take effect after you log out and back in \
                 (or run `newgrp dialout` in the current shell)."
            );
        }
    }

    let cargo = crate::daemons::find_binary("cargo")
        .ok_or_else(|| anyhow!("cargo not found — install Rust (https://rustup.rs)"))?;

    // Helpers go to the private libexec dir (--root), keeping them off PATH.
    for crate_name in HELPER_CRATES {
        let crate_dir = repo.join(crate_name);
        if !crate_dir.join("Cargo.toml").is_file() {
            println!(
                "  … {crate_name}: source not found at {}, skipping",
                crate_dir.display()
            );
            continue;
        }
        println!("  building {crate_name} (cargo install — may take a few minutes)…");
        let status = Command::new(&cargo)
            .args(["install", "--path"])
            .arg(&crate_dir)
            .arg("--root")
            .arg(&libexec_root)
            .arg("--force")
            .status()?;
        if !status.success() {
            bail!("{crate_name}: cargo install failed");
        }
        println!("  ✓ {crate_name:12} {}", libexec.join(crate_name).display());
    }

    // The panio CLI itself: the one user-facing binary, installed on PATH.
    println!("  building cli (cargo install — may take a few minutes)…");
    let status = Command::new(&cargo)
        .args(["install", "--path"])
        .arg(repo.join("cli"))
        .arg("--force")
        .status()?;
    if !status.success() {
        bail!("cli: cargo install failed");
    }
    println!("  ✓ {:12} {}", "panio", bin_dir.join("panio").display());

    // On the fast path too: the copy needs no sudo and no toolchain, and an
    // install that stopped short of it used to leave `panio skill` empty
    // with nothing saying why (GitHub #207).
    let skills_ok = will(SourceStep::InstallSkills) && install_skills(repo);

    // One-time migration: drop pre-libexec helper copies from ~/.cargo/bin so
    // a stale binary can't shadow or version-skew against the libexec install.
    // cargo uninstall keeps the install receipts tidy; the direct remove
    // covers receiptless leftovers (and visionocr/linuxocr, never cargo's).
    if will(SourceStep::DropStaleCopies) {
        for crate_name in HELPER_CRATES {
            let installed = bin_dir.join(crate_name);
            if !installed.is_file() {
                continue;
            }
            let _ = Command::new(&cargo)
                .args(["uninstall", crate_name])
                .output();
            if installed.is_file() {
                let _ = std::fs::remove_file(&installed);
            }
            if !installed.is_file() {
                println!("  ✓ removed stale {}", installed.display());
            }
        }
        for loose in ["netbootd-bpf-helper", "visionocr", "linuxocr"] {
            let stale = bin_dir.join(loose);
            if stale.is_file() && std::fs::remove_file(&stale).is_ok() {
                println!("  ✓ removed stale {}", stale.display());
            }
        }
    }

    if will(SourceStep::SetuidBpfHelper) {
        let helper = libexec.join("netbootd-bpf-helper");
        if helper.is_file() {
            setuid_bpf_helper(&helper);
        } else {
            println!("  … netbootd-bpf-helper not found; skipping setuid install");
        }
    }

    // OCR helper, one per platform: visionocr (swiftc) on macOS, winocr (cargo)
    // on Windows, a linuxocr copy on Linux. See docs/dev/ocr.md.
    if will(SourceStep::OcrHelper) {
        if cfg!(windows) {
            let source = repo.join("ocr/winocr");
            if !source.join("Cargo.toml").is_file() {
                println!("  … winocr: source not found, skipped");
            } else {
                let ok = Command::new(&cargo)
                    .args(["build", "--release", "--manifest-path"])
                    .arg(source.join("Cargo.toml"))
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let built = source.join("target/release/winocr.exe");
                if ok && built.is_file() {
                    let dest = libexec.join("winocr.exe");
                    std::fs::copy(&built, &dest)?;
                    println!("  ✓ {:12} {}", "winocr", dest.display());
                } else {
                    println!("  … winocr: build failed, skipped");
                }
            }
        } else if cfg!(target_os = "macos") {
            let source = repo.join("ocr/visionocr.swift");
            let dest = libexec.join("visionocr");
            if !source.is_file() {
                println!("  … visionocr: source not found, skipped");
            } else if crate::daemons::find_binary("swiftc").is_none() {
                println!("  … visionocr: swiftc not found (install Xcode CLT), skipped");
            } else {
                let ok = Command::new("swiftc")
                    .args(["-O", "-o"])
                    .arg(&dest)
                    .arg(&source)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    println!("  ✓ {:12} {}", "visionocr", dest.display());
                } else {
                    println!("  … visionocr: build failed, skipped");
                }
            }
        } else {
            let source = repo.join("ocr/linuxocr");
            let dest = libexec.join("linuxocr");
            if source.is_file() {
                std::fs::copy(&source, &dest)?;
                crate::platform::make_executable(&dest)?;
                println!("  ✓ {:12} {}", "linuxocr", dest.display());
            } else {
                println!("  … linuxocr: source not found, skipped");
            }
            // rapidocr: the GUI-mode engine on Linux. The script is small and always
            // copied; the heavy part is its venv, installed only when a lab file
            // actually asks for GUI OCR.
            let rsrc = repo.join("ocr/rapidocr");
            if rsrc.is_file() {
                let rdest = libexec.join("rapidocr");
                std::fs::copy(&rsrc, &rdest)?;
                crate::platform::make_executable(&rdest)?;
                println!("  ✓ {:12} {}", "rapidocr", rdest.display());
                install_rapidocr_venv(&libexec, lab_flag);
            }
            if crate::daemons::find_binary("tesseract").is_none() {
                println!(
                    "  ! tesseract not found — install it for OCR:\n\
                 \x20   sudo apt-get install tesseract-ocr"
                );
            }
        }
    }

    zigplug_step(will(SourceStep::Zigplug), repo, &libexec);
    jetkvm_step(will(SourceStep::JetKvm), repo, &libexec);

    if rust_only {
        println!(
            "\n{}",
            rust_only_done_message(std::env::consts::OS, skills_ok)
        );
        return Ok(());
    }

    println!("\nSetup complete.");
    println!(
        "Helpers live in {} — list or run them via `panio helper`.",
        libexec.display()
    );
    println!("Agent skills are bundled too — list them with `panio skill`.");
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == bin_dir))
        .unwrap_or(false);
    if !on_path {
        println!(
            "Note: add {} to your PATH so `panio` resolves.",
            bin_dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `setup` promotes the bpf-helper to setuid-root, so it must only do so
    /// to a regular file the invoking user owns — never to a symlink, which
    /// `chown`/`chmod` would follow to wherever it points.
    #[cfg(unix)]
    #[test]
    fn helper_safe_to_setuid_accepts_own_file_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("netbootd-bpf-helper");
        std::fs::write(&real, b"").unwrap();
        helper_safe_to_setuid(&real).expect("own regular file is accepted");

        let link = dir.path().join("netbootd-bpf-helper-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = helper_safe_to_setuid(&link).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");

        let missing = dir.path().join("absent");
        let err = helper_safe_to_setuid(&missing).unwrap_err().to_string();
        assert!(err.contains("cannot stat"), "{err}");
    }

    #[test]
    fn packaged_steps_grant_groups_on_linux_not_windows() {
        // Regression: the Linux group/tesseract steps were nested under the
        // Windows branch, so a .deb install never joined dialout/video.
        let linux = packaged_steps("linux");
        assert!(linux.contains(&PackagedStep::LinuxGroups));
        assert!(linux.contains(&PackagedStep::TesseractHint));
        assert!(!linux.contains(&PackagedStep::WindowsLayout));
        assert!(!linux.contains(&PackagedStep::SetuidBpfHelper));
        assert_eq!(packaged_steps("windows"), vec![PackagedStep::WindowsLayout]);
        assert_eq!(packaged_steps("macos"), vec![PackagedStep::SetuidBpfHelper]);
    }

    /// Regression (#207): `--rust-only` skipped the bundled-skills copy while
    /// its completion message named only OCR, setuid and zigplug, so a
    /// from-source install that stopped there left `panio skill` empty with
    /// no hint why. The copy needs neither sudo nor a toolchain beyond cargo,
    /// so it is on the fast path — on every platform.
    #[test]
    fn rust_only_installs_the_bundled_skills() {
        for os in ["linux", "macos", "windows"] {
            assert!(
                source_steps(os, true).contains(&SourceStep::InstallSkills),
                "{os}: --rust-only must still install the skills"
            );
            assert!(
                !rust_only_skips(os).contains(&SourceStep::InstallSkills),
                "{os}: skills must not be reported as skipped"
            );
        }
    }

    /// Regression: `--rust-only` ran the zigplug install and then said it had
    /// skipped it. The step list and the message were both right — #215 saw
    /// to that — but the install block itself was the one skippable step in
    /// `run` with no `will(...)` gate, so the fast path shelled out to uv on
    /// every from-source setup while reporting the opposite.
    ///
    /// Driven through the step rather than the message: the message was never
    /// the broken half, and a test that only read it passed throughout.
    #[test]
    fn rust_only_never_reaches_uv_for_zigplug() {
        let repo = tempfile::tempdir().unwrap();
        let libexec = tempfile::tempdir().unwrap();

        assert_eq!(
            zigplug_step(false, repo.path(), libexec.path()),
            ZigplugOutcome::Skipped,
            "off the step list, zigplug must return before the checkout or uv"
        );
        // On the list it looks, and says so — this scratch repo has no
        // zigplug/, which is what stops the test touching a real uv.
        assert_eq!(
            zigplug_step(true, repo.path(), libexec.path()),
            ZigplugOutcome::SourceMissing,
        );

        for os in ["linux", "macos", "windows"] {
            assert!(
                !source_steps(os, true).contains(&SourceStep::Zigplug),
                "{os}: zigplug needs uv, so it is off the fast path"
            );
        }
    }

    /// Same contract as zigplug: off the step list `jetkvm_step` must return
    /// before it looks for the checkout or a Go toolchain, so `--rust-only`
    /// never reaches `go`. On the list it reports what it found, and the
    /// scratch repo here has no `jetkvm/`, which is what stops the test from
    /// running a real build.
    #[test]
    fn rust_only_never_reaches_go_for_jetkvm() {
        let repo = tempfile::tempdir().unwrap();
        let libexec = tempfile::tempdir().unwrap();
        assert_eq!(
            jetkvm_step(false, repo.path(), libexec.path()),
            JetKvmOutcome::Skipped
        );
        assert_eq!(
            jetkvm_step(true, repo.path(), libexec.path()),
            JetKvmOutcome::SourceMissing
        );
        for os in ["linux", "macos", "windows"] {
            assert!(
                !source_steps(os, true).contains(&SourceStep::JetKvm),
                "{os}: jetkvm needs Go, so it is off the fast path"
            );
            assert!(source_steps(os, false).contains(&SourceStep::JetKvm));
        }
    }

    /// The step builds the real helper from the real checkout when a Go
    /// toolchain exists, and the binary it leaves in libexec runs. Skipped
    /// (not failed) without `go`, which is the step's own documented outcome.
    #[test]
    fn jetkvm_step_builds_a_runnable_helper_from_the_checkout() {
        if crate::daemons::find_binary("go").is_none() {
            eprintln!("go not installed; skipping");
            return;
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the cli crate always has a parent directory");
        let libexec = tempfile::tempdir().unwrap();
        assert_eq!(
            jetkvm_step(true, repo, libexec.path()),
            JetKvmOutcome::Installed
        );
        let bin = libexec
            .path()
            .join(format!("jetkvm{}", std::env::consts::EXE_SUFFIX));
        let out = Command::new(&bin).arg("version").output().unwrap();
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("jetkvm/"),
            "version reply"
        );
    }

    /// The fast path is the full list minus what needs sudo or a second
    /// toolchain — nothing else. Stated per platform, because the answer
    /// differs and a list that ignored that would mis-report two of three.
    #[test]
    fn the_fast_path_skips_only_sudo_and_second_toolchain_steps() {
        assert_eq!(
            source_steps("linux", true),
            vec![SourceStep::InstallSkills, SourceStep::DropStaleCopies]
        );
        assert_eq!(
            rust_only_skips("linux"),
            vec![
                SourceStep::LinuxGroups,
                SourceStep::OcrHelper,
                SourceStep::Zigplug,
                SourceStep::JetKvm
            ],
            "Linux skips the group step (sudo), not the setuid one (macOS only)"
        );
        assert_eq!(
            rust_only_skips("macos"),
            vec![
                SourceStep::SetuidBpfHelper,
                SourceStep::OcrHelper,
                SourceStep::Zigplug,
                SourceStep::JetKvm
            ],
        );
        assert_eq!(
            rust_only_skips("windows"),
            vec![
                SourceStep::OcrHelper,
                SourceStep::Zigplug,
                SourceStep::JetKvm
            ],
            "there is no setuid bit and no dialout group on Windows"
        );
    }

    /// The message names every step the platform actually skipped, and claims
    /// nothing it did not do. The old message was a literal that named three
    /// steps on every platform while the code skipped a different set.
    #[test]
    fn the_message_names_exactly_what_was_skipped() {
        for os in ["linux", "macos", "windows"] {
            let msg = rust_only_done_message(os, true);
            assert!(msg.contains("skills installed"), "{os}: {msg}");
            for step in rust_only_skips(os) {
                assert!(
                    msg.contains(step.label()),
                    "{os}: {:?} was skipped but is unnamed in: {msg}",
                    step
                );
            }
            for step in source_steps(os, true) {
                assert!(
                    !msg.contains(&format!("skipped {}", step.label())),
                    "{os}: {:?} runs on the fast path but reads as skipped: {msg}",
                    step
                );
            }
        }
        // macOS names setuid; Linux must not, and vice versa for the groups.
        assert!(rust_only_done_message("macos", true).contains("setuid"));
        assert!(!rust_only_done_message("linux", true).contains("setuid"));
        assert!(rust_only_done_message("linux", true).contains("group membership"));
    }

    /// If the skills copy did not land, the message must not say it did —
    /// that is #207 in the other direction, and the release-train source arm
    /// now trusts this line instead of copying the files by hand.
    #[test]
    fn the_message_does_not_claim_skills_it_failed_to_install() {
        let ok = rust_only_done_message("linux", true);
        let failed = rust_only_done_message("linux", false);
        assert!(ok.contains("skills installed"), "{ok}");
        assert!(!failed.contains("and skills installed"), "{failed}");
        assert!(failed.contains("did not succeed"), "{failed}");
    }

    /// The markers `is_repo_root` keys off must match the *real* tree.    /// The markers `is_repo_root` keys off must match the *real* tree.
    ///
    /// A tmpdir-fixture test would be useless here: it would create whatever
    /// files the predicate currently names and keep passing forever after one
    /// of them was deleted from the repo. That is exactly how the
    /// `pyproject.toml` marker outlived the legacy Python CLI removal and
    /// silently broke `panio setup --rust-only` (and, quietly, the
    /// repo-checkout branch of the skill search path).
    /// Assert against the actual checkout so marker drift fails the build.
    #[test]
    fn repo_root_detects_the_real_checkout() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the cli crate always has a parent directory");
        assert!(
            is_repo_root(repo),
            "is_repo_root() no longer recognizes the checkout at {}: a marker \
             it checks for was renamed or removed",
            repo.display()
        );
    }

    #[test]
    fn repo_root_rejects_a_non_repo_directory() {
        assert!(!is_repo_root(Path::new("/")));
        let cli = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!is_repo_root(cli), "the cli crate dir is not the repo root");
    }

    /// The old code read `$USER` with `unwrap_or_default()` and would have
    /// gone on to run `sudo usermod -aG dialout ""` on an unset one; this is
    /// the error that now stops it before any command is run (Review low
    /// #11). Not gated to Linux: the function itself is portable, only its
    /// caller's context (an existing dialout/video group) is Linux-specific.
    #[test]
    fn linux_group_user_errors_on_unset_or_empty_user() {
        // Safe: mutated and restored within one test; no other test in this
        // crate reads or writes $USER.
        let prev = std::env::var_os("USER");
        unsafe { std::env::remove_var("USER") };
        let e = linux_group_user("dialout", "serial port access").unwrap_err();
        assert!(e.to_string().contains("$USER is not set"), "{e}");

        unsafe { std::env::set_var("USER", "") };
        let e = linux_group_user("dialout", "serial port access").unwrap_err();
        assert!(e.to_string().contains("$USER is not set"), "{e}");

        unsafe { std::env::set_var("USER", "alice") };
        assert_eq!(
            linux_group_user("dialout", "serial port access").unwrap(),
            "alice"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("USER", v) },
            None => unsafe { std::env::remove_var("USER") },
        }
    }

    /// `panio setup --lab other.toml` (or `$PANIO_LAB`) must judge the
    /// rapidocr venv against *that* lab, not silently fall back to the
    /// default path the way the old no-argument `lab_wants_gui_ocr()` always
    /// did (Review low #11). Linux-only: the function itself is gated to the
    /// platform the venv is for.
    #[cfg(target_os = "linux")]
    #[test]
    fn lab_wants_gui_ocr_honors_the_lab_flag() {
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui-lab.toml");
        std::fs::write(&gui, "[targets.t]\n[targets.t.video]\nocr_mode = \"gui\"\n").unwrap();
        assert!(lab_wants_gui_ocr(Some(gui.to_str().unwrap())));

        let plain = dir.path().join("plain-lab.toml");
        std::fs::write(&plain, "[targets.t]\n").unwrap();
        assert!(!lab_wants_gui_ocr(Some(plain.to_str().unwrap())));

        // A --lab that doesn't resolve to a real file (typo'd path) is "no",
        // not a panic or a silent read of an unrelated file.
        let missing = dir.path().join("missing.toml");
        assert!(!lab_wants_gui_ocr(Some(missing.to_str().unwrap())));
    }
}
