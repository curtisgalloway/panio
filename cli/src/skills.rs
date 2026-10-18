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

//! `panio skill` — discover and read the agent-facing skills panio ships.
//!
//! A *skill* is a markdown guide (`<dir>/<name>/SKILL.md`, with a `name:` +
//! `description:` YAML frontmatter) that teaches an agent how to drive panio:
//! the `panio` usage skill and the `kvm-puppeting` GUI doctrine
//! power skill. They live in the source tree under `skills/` and install
//! alongside the CLI; this command is how an agent finds and reads them without
//! the harness having them pre-loaded.
//!
//! Skills resolve from a search path that mirrors
//! [`crate::daemons::helper_dirs`] but under `share/` instead of `libexec/`:
//! the in-repo `skills/` when run from a checkout (so an author's edits show up
//! immediately), then the per-user data dir
//! (`~/.local/share/panio/skills`), then dirs relative to the running CLI
//! (Homebrew keg / prefix install), then the system package dir
//! (`/usr/share/panio/skills`), and last the plugin roots'
//! `<root>/share/panio/skills` (`PANIO_PLUGIN_PATH`, GitHub #303), which
//! can add skills but never shadow a bundled one. The command mirrors
//! `panio helper`: no NAME lists every skill, a NAME prints that skill's
//! `SKILL.md`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Per-user skills dir: `~/.local/share/panio/skills`. The install target
/// for `panio setup`; the first installed location [`skills_dirs`] searches.
pub fn user_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/share/panio/skills"))
}

/// Skills dir of a system package (.deb/tarball): `/usr/share/panio/skills`.
/// Always present in the search path, so [`skills_dirs`] is never empty.
fn system_skills_dir() -> PathBuf {
    PathBuf::from("/usr/share/panio/skills")
}

/// Skills dir relative to the running CLI, after resolving symlinks — the
/// `share/` analogue of [`crate::daemons`]'s libexec lookup. Homebrew links
/// `<prefix>/bin/panio` into the versioned keg, so `<keg>/share/panio/skills`
/// is the keg's bundled skills; an FHS-style prefix install resolves the same
/// way. A relocated install thus finds its skills without enumerating package
/// managers.
///
/// On Windows the portable zip has no `bin/` level: `panio\panio.exe` sits
/// in the install prefix itself, next to `libexec\` (see
/// [`crate::daemons`]'s `exe_relative_dirs`), so `<exe dir>\share\panio\skills`
/// is searched there too. Without it a zip install found no skills at all
/// (`panio skill` on Windows was empty through v0.3.0).
fn exe_relative_skills_dirs() -> Vec<PathBuf> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    skills_dirs_for_exe(&exe)
}

/// The exe-relative candidates for a CLI binary at `exe`: the prefix
/// (grandparent) layout every platform uses, plus the exe's own directory as
/// prefix on Windows. Pure, so it is testable without relocating the test
/// binary.
fn skills_dirs_for_exe(exe: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(prefix) = exe.parent().and_then(|d| d.parent()) {
        dirs.push(prefix.join("share/panio/skills"));
    }
    if cfg!(windows) {
        if let Some(exe_dir) = exe.parent() {
            dirs.push(exe_dir.join("share/panio/skills"));
        }
    }
    dirs
}

/// The skills directories, in resolution order: the in-repo `skills/` when run
/// from a source checkout, then the per-user data dir, the CLI-relative dir
/// (Homebrew keg / prefix), and the system package dir. The first directory
/// that holds a given skill name wins, so a checkout or per-user install
/// shadows the packaged copy.
pub fn skills_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(repo) = crate::setup::find_repo_root() {
        dirs.push(repo.join("skills"));
    }
    dirs.extend(user_skills_dir());
    dirs.extend(exe_relative_skills_dirs());
    dirs.push(system_skills_dir());
    dirs
}

/// A plugin root's skills dir: `<root>/share/panio/skills`.
fn plugin_skills_dir(root: &Path) -> PathBuf {
    root.join("share/panio/skills")
}

/// One directory on the skill search path, and the plugin root it belongs to
/// (`None` for the bundled dirs of [`skills_dirs`]).
struct SearchDir {
    dir: PathBuf,
    plugin_root: Option<PathBuf>,
}

/// The full skill search path: every bundled dir first, then each plugin
/// root's skills dir, in `PANIO_PLUGIN_PATH` order. Pure, so the order
/// (plugins last, so they cannot shadow a bundled skill) is testable.
fn search_path(bundled: Vec<PathBuf>, plugin_roots: &[PathBuf]) -> Vec<SearchDir> {
    let mut path: Vec<SearchDir> = bundled
        .into_iter()
        .map(|dir| SearchDir {
            dir,
            plugin_root: None,
        })
        .collect();
    path.extend(plugin_roots.iter().map(|root| SearchDir {
        dir: plugin_skills_dir(root),
        plugin_root: Some(root.clone()),
    }));
    path
}

/// One discovered skill: its name (the directory name), the `SKILL.md` path,
/// the one-line description pulled from the frontmatter, and the plugin root
/// it came from, if any.
struct Skill {
    name: String,
    path: PathBuf,
    description: String,
    plugin_root: Option<PathBuf>,
}

/// Every skill found across the search path ([`skills_dirs`], then the plugin
/// roots), deduped by name (first dir wins), sorted by name. A "skill" is any
/// `<dir>/<name>/SKILL.md`.
fn discover() -> Vec<Skill> {
    discover_in(&search_path(skills_dirs(), &crate::daemons::plugin_roots()))
}

/// [`discover`] over an explicit search path, so a test can point it at a
/// layout it built rather than at wherever the test binary happens to live.
fn discover_in(dirs: &[SearchDir]) -> Vec<Skill> {
    let mut found: Vec<Skill> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for SearchDir { dir, plugin_root } in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let manifest = entry.path().join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !seen.insert(name.clone()) {
                continue; // an earlier (higher-priority) dir already has it.
            }
            let description = read_description(&manifest);
            found.push(Skill {
                name,
                path: manifest,
                description,
                plugin_root: plugin_root.clone(),
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Pull the `description:` field out of a SKILL.md's YAML frontmatter as a
/// single collapsed line. Handles both an inline value (`description: text`)
/// and a folded/literal block scalar (`description: >` followed by indented
/// lines). Returns an empty string when there is no frontmatter or no field —
/// the skill still lists, just without a summary.
fn read_description(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return String::new();
    }
    let mut collecting = false;
    let mut parts: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break; // end of frontmatter
        }
        if collecting {
            // A block scalar continues while lines stay indented; a new
            // unindented `key:` ends it.
            let indented = line.starts_with(char::is_whitespace);
            if indented && !trimmed.is_empty() {
                parts.push(trimmed.to_string());
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("description:") {
            let rest = rest.trim();
            // `>`, `|`, `>-`, `|+`, … introduce a multi-line block scalar.
            if rest.is_empty() || rest.starts_with('>') || rest.starts_with('|') {
                collecting = true;
            } else {
                return rest.trim_matches(|c| c == '"' || c == '\'').to_string();
            }
        }
    }
    parts.join(" ")
}

/// `panio skill [NAME] [--path]`: list every bundled skill, or print one.
pub fn run(name: Option<&str>, path: bool) -> Result<()> {
    match name {
        None => list(),
        Some(name) => show(name, path),
    }
}

/// List each skill — name, then its frontmatter description — with a hint on
/// how to read one in full.
fn list() -> Result<()> {
    let skills = discover();
    if skills.is_empty() {
        let searched = search_path(skills_dirs(), &crate::daemons::plugin_roots())
            .iter()
            .map(|d| d.dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "No skills found (searched {searched}) — install the panio package \
             or run `panio setup` from a source checkout."
        );
        return Ok(());
    }
    for s in &skills {
        println!("{}", list_heading(s));
        if !s.description.is_empty() {
            println!("    {}", s.description);
        }
        println!();
    }
    println!("Read one with `panio skill <name>` (or --path for its file path).");
    Ok(())
}

/// A skill's heading line in the list: its bare name when bundled, or the
/// name, a tab, and `plugin: <root>` when it came from a plugin root, the same
/// annotation `panio helper` uses. The name stays the first field.
fn list_heading(s: &Skill) -> String {
    match &s.plugin_root {
        None => s.name.clone(),
        Some(root) => format!("{}\tplugin: {}", s.name, root.display()),
    }
}

/// Print a single skill: its `SKILL.md` contents, or (with `path`) just the
/// resolved file path so an agent can `Read` it or a user can open it.
fn show(name: &str, path: bool) -> Result<()> {
    let skills = discover();
    let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
        let have: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        anyhow!(
            "skill '{name}' not found (skills: {}). List them with `panio skill`.",
            if have.is_empty() {
                "(none)".to_string()
            } else {
                have.join(", ")
            }
        )
    })?;
    if path {
        println!("{}", skill.path.display());
        return Ok(());
    }
    let body = std::fs::read_to_string(&skill.path)
        .map_err(|e| anyhow!("reading {}: {e}", skill.path.display()))?;
    print!("{body}");
    Ok(())
}

/// Install the skills bundled in a source checkout into the per-user data dir,
/// so `panio skill` finds them when the installed CLI runs outside the tree.
/// Copies each `skills/<name>/SKILL.md`; returns how many were installed.
pub fn install_bundled(repo: &Path) -> Result<usize> {
    let src = repo.join("skills");
    let dst_root =
        user_skills_dir().ok_or_else(|| anyhow!("could not determine the home directory"))?;
    let entries = std::fs::read_dir(&src).map_err(|e| anyhow!("reading {}: {e}", src.display()))?;
    let mut count = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let manifest = entry.path().join("SKILL.md");
        if !manifest.is_file() {
            continue;
        }
        let name = entry.file_name();
        let dst_dir = dst_root.join(&name);
        std::fs::create_dir_all(&dst_dir)?;
        std::fs::copy(&manifest, dst_dir.join("SKILL.md"))?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out `<root>/<exe rel path>` plus one skill under
    /// `<root>/<skills rel path>/<name>/SKILL.md`, returning the exe path.
    fn layout(root: &Path, exe_rel: &str, skills_rel: &str, name: &str) -> PathBuf {
        let exe = root.join(exe_rel);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        let dir = root.join(skills_rel).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: a test skill\n---\nbody\n"),
        )
        .unwrap();
        exe
    }

    /// The prefix layout every platform ships (`bin/panio` beside
    /// `share/panio/skills`): the keg, the FHS prefix, the tarball.
    #[test]
    fn a_prefix_install_finds_its_skills_one_level_up() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = layout(tmp.path(), "bin/panio", "share/panio/skills", "prefixed");
        let found = discover_in(&search_path(skills_dirs_for_exe(&exe), &[]));
        assert_eq!(
            found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["prefixed"]
        );
        assert_eq!(found[0].description, "a test skill");
    }

    /// The portable Windows zip: `panio\panio.exe` with `share\` beside
    /// it, the exe's own directory being the prefix. This is the layout that
    /// listed nothing through v0.3.0, because only the level-up dir was
    /// searched; the assertion is platform-conditional because that extra
    /// dir is deliberately Windows-only (a Unix `~/.cargo/bin/panio` must
    /// not start scanning `~/.cargo/bin/share`).
    #[test]
    fn the_windows_zip_layout_is_found_only_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = layout(
            tmp.path(),
            "panio/panio.exe",
            "panio/share/panio/skills",
            "zipped",
        );
        let names: Vec<String> = discover_in(&search_path(skills_dirs_for_exe(&exe), &[]))
            .into_iter()
            .map(|s| s.name)
            .collect();
        if cfg!(windows) {
            assert_eq!(names, ["zipped"]);
        } else {
            assert!(
                names.is_empty(),
                "unix must not search the exe dir: {names:?}"
            );
        }
    }

    /// A skill present in two searched dirs lists once, from the first.
    #[test]
    fn an_earlier_dir_shadows_a_later_one() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        for (dir, desc) in [(&first, "from first"), (&second, "from second")] {
            let d = dir.join("dup");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: dup\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        }
        let found = discover_in(&search_path(vec![first, second], &[]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].description, "from first");
    }

    /// Plugin roots' skills dirs come after every bundled dir, in list order.
    #[test]
    fn plugin_skills_dirs_follow_the_bundled_ones() {
        let bundled = vec![PathBuf::from("/a/skills"), PathBuf::from("/b/skills")];
        let roots = [PathBuf::from("/opt/lab-plugins"), PathBuf::from("/srv/p")];
        let path = search_path(bundled.clone(), &roots);
        let dirs: Vec<PathBuf> = path.iter().map(|d| d.dir.clone()).collect();
        assert_eq!(
            dirs,
            vec![
                bundled[0].clone(),
                bundled[1].clone(),
                roots[0].join("share/panio/skills"),
                roots[1].join("share/panio/skills"),
            ]
        );
        assert!(path[..2].iter().all(|d| d.plugin_root.is_none()));
        assert_eq!(path[2].plugin_root.as_deref(), Some(roots[0].as_path()));
    }

    /// A plugin skill lists with its root; one named like a bundled skill
    /// loses to the bundled copy.
    #[test]
    fn a_plugin_adds_skills_but_cannot_shadow_a_bundled_one() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("bundled");
        let root = tmp.path().join("plugins");
        let write = |dir: &Path, name: &str, desc: &str| {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        };
        write(&bundled, "panio", "bundled");
        write(&plugin_skills_dir(&root), "panio", "plugin copy");
        write(&plugin_skills_dir(&root), "lab-targets", "our lab");

        let found = discover_in(&search_path(vec![bundled], std::slice::from_ref(&root)));
        let got: Vec<(&str, &str, Option<&Path>)> = found
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.description.as_str(),
                    s.plugin_root.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("lab-targets", "our lab", Some(root.as_path())),
                ("panio", "bundled", None),
            ]
        );
        assert_eq!(
            list_heading(&found[0]),
            format!("lab-targets\tplugin: {}", root.display())
        );
        assert_eq!(list_heading(&found[1]), "panio");
    }
}
