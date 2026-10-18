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

//! The editable lab document: surgical, comment-preserving writes via `toml_edit`.
//!
//! The lab file is human-authored, so the CLI edits it *politely* — preserving
//! hand-written comments, key ordering, and formatting, touching only the tables
//! it changes. Reads/resolution live in [`crate::model`]; this is the write side
//! plus a re-run of the shared [`model::validate`] before every save.

use std::path::{Path, PathBuf};

use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::model::{self, Lab, LabError};

const HEADER: &str = "panio lab — managed by the `panio` CLI; hand-edits are preserved";

fn lab_err<T>(msg: impl Into<String>) -> Result<T, LabError> {
    Err(LabError(msg.into()))
}

/// A lab file open for editing, backed by a live `toml_edit` document.
pub struct LabFile {
    pub path: PathBuf,
    pub doc: DocumentMut,
    /// A top-of-file header to write for a freshly created lab (None when the
    /// file already existed — its own leading comments are preserved as-is).
    header: Option<&'static str>,
}

impl LabFile {
    pub fn load(path: &Path) -> Result<Self, LabError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LabError(format!("{}: {e}", path.display())))?;
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|e| LabError(e.to_string()))?;
        model::validate(&lab_of(&doc)?)?;
        Ok(Self {
            path: path.to_path_buf(),
            doc,
            header: None,
        })
    }

    pub fn create(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            doc: DocumentMut::new(),
            header: Some(HEADER),
        }
    }

    /// Validate (with the save-time cross-reference rules, see
    /// [`model::validate_for_save`]) and write the document. The write is
    /// atomic — a sibling temp file renamed into place — so a reader never
    /// sees a half-written lab and a crash leaves the old one intact; and it
    /// resolves the path first, so a lab that is a symlink into a git
    /// checkout keeps being one (see [`crate::platform::write_atomic`]).
    pub fn save(&self) -> Result<(), LabError> {
        model::validate_for_save(&lab_of(&self.doc)?)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LabError(format!("{}: {e}", parent.display())))?;
        }
        // toml_edit stores a leading comment as the first item's prefix, which
        // doesn't exist for a fresh document — so prepend the header to the
        // rendered text rather than into the (item-less) document.
        let mut out = self.doc.to_string();
        if let Some(h) = self.header {
            if !out.trim_start().starts_with('#') {
                out = format!("# {h}\n\n{out}");
            }
        }
        crate::platform::write_atomic(&self.path, out.as_bytes())
            .map_err(|e| LabError(format!("{}: {e}", self.path.display())))
    }

    // ── hosts ───────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn add_host(
        &mut self,
        name: &str,
        ssh: &str,
        description: Option<&str>,
        hostname: Option<&str>,
        identity: Option<&str>,
        control_path: Option<&str>,
        panio_cmd: Option<&str>,
        plugin_path: Option<&str>,
    ) -> Result<(), LabError> {
        model::validate_name("host", name)?;
        let hosts = super_table(&mut self.doc, "hosts")?;
        if hosts.contains_key(name) {
            return lab_err(format!("host '{name}' already exists"));
        }
        let mut t = Table::new();
        t.insert("ssh", value(ssh));
        set_opt(&mut t, "description", description);
        set_opt(&mut t, "hostname", hostname);
        set_opt(&mut t, "identity", identity);
        set_opt(&mut t, "control_path", control_path);
        set_opt(&mut t, "panio_cmd", panio_cmd);
        set_opt(&mut t, "plugin_path", plugin_path);
        hosts.insert(name, Item::Table(t));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_host(
        &mut self,
        name: &str,
        ssh: Option<&str>,
        description: Option<&str>,
        hostname: Option<&str>,
        identity: Option<&str>,
        control_path: Option<&str>,
        panio_cmd: Option<&str>,
        plugin_path: Option<&str>,
    ) -> Result<(), LabError> {
        model::validate_name("host", name)?;
        let t = self
            .doc
            .get_mut("hosts")
            .and_then(|i| i.as_table_mut())
            .and_then(|h| h.get_mut(name))
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("no host '{name}'")))?;
        set_opt(t, "ssh", ssh);
        set_opt(t, "description", description);
        set_opt(t, "hostname", hostname);
        set_opt(t, "identity", identity);
        set_opt(t, "control_path", control_path);
        set_opt(t, "panio_cmd", panio_cmd);
        // An empty value clears the field, handing the host's plugin roots
        // back to its own environment.
        match plugin_path {
            Some("") => {
                t.remove("plugin_path");
            }
            other => set_opt(t, "plugin_path", other),
        }
        Ok(())
    }

    pub fn remove_host(&mut self, name: &str) -> Result<(), LabError> {
        let hosts = self
            .doc
            .get_mut("hosts")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("no host '{name}'")))?;
        if !hosts.contains_key(name) {
            return lab_err(format!("no host '{name}'"));
        }
        let refs = self.host_references(name);
        if !refs.is_empty() {
            return lab_err(format!(
                "host '{name}' is still used by: {}",
                refs.join(", ")
            ));
        }
        self.doc["hosts"].as_table_mut().unwrap().remove(name);
        Ok(())
    }

    fn host_references(&self, host: &str) -> Vec<String> {
        let lab: Lab = toml::from_str(&self.doc.to_string()).unwrap_or_default();
        let mut refs = Vec::new();
        for name in lab.targets.keys() {
            if let Some(rt) = lab.resolved_target(name) {
                if rt.default_host == host || rt.channels.iter().any(|c| c.host == host) {
                    refs.push(name.clone());
                }
            }
        }
        refs
    }

    // ── targets ───────────────────────────────────────────────────────────────

    pub fn add_target(
        &mut self,
        name: &str,
        host: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), LabError> {
        model::validate_name("target", name)?;
        let targets = super_table(&mut self.doc, "targets")?;
        if targets.contains_key(name) {
            return lab_err(format!("target '{name}' already exists"));
        }
        let mut t = Table::new();
        set_opt(&mut t, "host", host);
        set_opt(&mut t, "description", description);
        targets.insert(name, Item::Table(t));
        Ok(())
    }

    pub fn update_target(
        &mut self,
        name: &str,
        host: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), LabError> {
        model::validate_name("target", name)?;
        let t = self.target_mut(name)?;
        set_opt(t, "host", host);
        if description.is_some() {
            // Migrate the legacy `note` key to the canonical `description`:
            // leaving both would deserialize as a duplicate field (they alias).
            t.remove("note");
        }
        set_opt(t, "description", description);
        Ok(())
    }

    pub fn remove_target(&mut self, name: &str) -> Result<(), LabError> {
        let targets = self
            .doc
            .get_mut("targets")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("no target '{name}'")))?;
        if targets.remove(name).is_none() {
            return lab_err(format!("no target '{name}'"));
        }
        Ok(())
    }

    /// Rename a target, carrying every channel table (and any hand-written
    /// comments inside them) to the new name.
    pub fn rename_target(&mut self, old: &str, new: &str) -> Result<(), LabError> {
        model::validate_name("target", new)?;
        let targets = self
            .doc
            .get_mut("targets")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("no target '{old}'")))?;
        if !targets.contains_key(old) {
            return lab_err(format!("no target '{old}'"));
        }
        if targets.contains_key(new) {
            return lab_err(format!("target '{new}' already exists"));
        }
        let item = targets.remove(old).expect("presence checked above");
        targets.insert(new, item);
        Ok(())
    }

    // ── serial channels (collection) ─────────────────────────────────────────

    // Mirrors the `[[serial]]` field set one-for-one; a params struct would just
    // duplicate `SerialChannel` for no clarity gain.
    #[allow(clippy::too_many_arguments)]
    pub fn add_serial(
        &mut self,
        target: &str,
        name: &str,
        device: &str,
        baud: i64,
        sense: Option<&str>,
        power_button: bool,
        host: Option<&str>,
        port: Option<u16>,
    ) -> Result<(), LabError> {
        model::validate_name("serial interface", name)?;
        let t = self.target_mut(target)?;
        if !t.contains_key("serial") {
            t.insert("serial", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let aot = t
            .get_mut("serial")
            .and_then(|i| i.as_array_of_tables_mut())
            .ok_or_else(|| LabError(format!("target '{target}': serial is not [[serial]]")))?;
        if aot
            .iter()
            .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(name))
        {
            return lab_err(format!("target '{target}': serial '{name}' already exists"));
        }
        let mut s = Table::new();
        s.insert("name", value(name));
        s.insert("device", value(device));
        s.insert("baud", value(baud));
        set_opt(&mut s, "power_sense_signal", sense);
        if power_button {
            s.insert("power_button", value(true));
        }
        set_opt(&mut s, "host", host);
        set_port(&mut s, port);
        aot.push(s);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_serial(
        &mut self,
        target: &str,
        name: &str,
        device: Option<&str>,
        baud: Option<i64>,
        sense: Option<&str>,
        power_button: Option<bool>,
        host: Option<&str>,
        port: Option<u16>,
    ) -> Result<(), LabError> {
        model::validate_name("serial interface", name)?;
        let t = self.target_mut(target)?;
        let aot = t
            .get_mut("serial")
            .and_then(|i| i.as_array_of_tables_mut())
            .ok_or_else(|| LabError(format!("target '{target}': no serial '{name}'")))?;
        let s = aot
            .iter_mut()
            .find(|s| s.get("name").and_then(|v| v.as_str()) == Some(name))
            .ok_or_else(|| LabError(format!("target '{target}': no serial '{name}'")))?;
        set_opt(s, "device", device);
        if let Some(b) = baud {
            s.insert("baud", value(b));
        }
        set_opt(s, "power_sense_signal", sense);
        // Tri-state: `Some(true)` opts in, `Some(false)` revokes (drops the key
        // back to the default), `None` leaves it unchanged.
        match power_button {
            Some(true) => {
                s.insert("power_button", value(true));
            }
            Some(false) => {
                s.remove("power_button");
            }
            None => {}
        }
        set_opt(s, "host", host);
        set_port(s, port);
        Ok(())
    }

    pub fn remove_serial(&mut self, target: &str, name: &str) -> Result<(), LabError> {
        let t = self.target_mut(target)?;
        let aot = t
            .get_mut("serial")
            .and_then(|i| i.as_array_of_tables_mut())
            .ok_or_else(|| LabError(format!("target '{target}': no serial '{name}'")))?;
        let idx = aot
            .iter()
            .position(|s| s.get("name").and_then(|v| v.as_str()) == Some(name))
            .ok_or_else(|| LabError(format!("target '{target}': no serial '{name}'")))?;
        aot.remove(idx);
        if aot.is_empty() {
            t.remove("serial");
        }
        Ok(())
    }

    // ── singleton channels (netboot / power / video) ─────────────────────────

    fn set_singleton(
        &mut self,
        target: &str,
        kind: &str,
        fields: &[(&str, Option<&str>)],
    ) -> Result<(), LabError> {
        let t = self.target_mut(target)?;
        if !t.contains_key(kind) {
            t.insert(kind, Item::Table(Table::new()));
        }
        let c = t
            .get_mut(kind)
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': {kind} is not a table")))?;
        for (k, v) in fields {
            set_opt(c, k, *v);
        }
        Ok(())
    }

    fn remove_singleton(&mut self, target: &str, kind: &str) -> Result<(), LabError> {
        let t = self.target_mut(target)?;
        if t.remove(kind).is_none() {
            return lab_err(format!("target '{target}': no {kind} channel"));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_netboot(
        &mut self,
        target: &str,
        interface: Option<&str>,
        host_ip: Option<&str>,
        tftp_root: Option<&str>,
        boot_file: Option<&str>,
        http_port: Option<&str>,
        content_type: Option<&str>,
        host: Option<&str>,
    ) -> Result<(), LabError> {
        self.set_singleton(
            target,
            "netboot",
            &[
                ("interface", interface),
                ("host_ip", host_ip),
                ("tftp_root", tftp_root),
                ("boot_file", boot_file),
                ("http_port", http_port),
                ("content_type", content_type),
                ("host", host),
            ],
        )?;
        // No edit may leave *this* link sharing a /24 with another link on
        // its host — a second link left at the default host_ip included. Only
        // the edited target is judged: a clash between two other targets is
        // `doctor`'s to report and must never block the edit that repairs it.
        if let Some(c) = model::netboot_subnet_clash(&lab_of(&self.doc)?, target) {
            return Err(LabError(model::subnet_clash_message(target, &c)));
        }
        Ok(())
    }

    pub fn remove_netboot(&mut self, target: &str) -> Result<(), LabError> {
        self.remove_singleton(target, "netboot")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_power(
        &mut self,
        target: &str,
        cycle_cmd: Option<&str>,
        on_cmd: Option<&str>,
        off_cmd: Option<&str>,
        state_cmd: Option<&str>,
        serial_interface: Option<&str>,
        host: Option<&str>,
    ) -> Result<(), LabError> {
        self.set_singleton(
            target,
            "power",
            &[
                ("cycle_cmd", cycle_cmd),
                ("on_cmd", on_cmd),
                ("off_cmd", off_cmd),
                ("state_cmd", state_cmd),
                ("serial_interface", serial_interface),
                ("host", host),
            ],
        )
    }

    pub fn remove_power(&mut self, target: &str) -> Result<(), LabError> {
        self.remove_singleton(target, "power")
    }

    /// Create or update the named power level `level` (#302): sets each hook
    /// given, leaving the others untouched. Creating the level needs at least
    /// one hook; the power channel is created when absent. A level written
    /// inline (`hard = { cycle_cmd = "…" }`) is edited in place.
    pub fn set_power_level(
        &mut self,
        target: &str,
        level: &str,
        cycle_cmd: Option<&str>,
        on_cmd: Option<&str>,
        off_cmd: Option<&str>,
        state_cmd: Option<&str>,
    ) -> Result<(), LabError> {
        model::validate_name("power level", level)?;
        if level == model::DEFAULT_POWER_LEVEL {
            return lab_err(format!(
                "'{}' names the hooks set without --level; pick another level name",
                model::DEFAULT_POWER_LEVEL
            ));
        }
        let hooks = [
            ("cycle_cmd", cycle_cmd),
            ("on_cmd", on_cmd),
            ("off_cmd", off_cmd),
            ("state_cmd", state_cmd),
        ];
        self.set_singleton(target, "power", &[])?;
        let power = self
            .target_mut(target)?
            .get_mut("power")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': power is not a table")))?;
        if !power.contains_key("levels") {
            let mut t = Table::new();
            t.set_implicit(true);
            power.insert("levels", Item::Table(t));
        }
        let levels = power
            .get_mut("levels")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| not_a_standard_table(&format!("targets.{target}.power.levels")))?;
        if !levels.contains_key(level) {
            if hooks.iter().all(|(_, v)| v.is_none()) {
                return lab_err(format!(
                    "target '{target}': new power level '{level}' needs at least one of \
                     --cycle-cmd, --on-cmd, --off-cmd, --state-cmd"
                ));
            }
            levels.insert(level, Item::Table(Table::new()));
        }
        let l = levels
            .get_mut(level)
            .and_then(|i| i.as_table_like_mut())
            .ok_or_else(|| {
                LabError(format!(
                    "target '{target}': power level '{level}' is not a table"
                ))
            })?;
        for (k, v) in hooks {
            if let Some(v) = v {
                l.insert(k, value(v));
            }
        }
        if let Some(inline) = levels.get_mut(level).and_then(|i| i.as_inline_table_mut()) {
            inline.fmt();
        }
        Ok(())
    }

    /// Remove the named power level, leaving the default hooks and any other
    /// level alone; the `levels` table goes too when it was the last one.
    pub fn remove_power_level(&mut self, target: &str, level: &str) -> Result<(), LabError> {
        let missing = || LabError(format!("target '{target}': no power level '{level}'"));
        let power = self
            .target_mut(target)?
            .get_mut("power")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': no power channel")))?;
        let levels = power
            .get_mut("levels")
            .and_then(|i| i.as_table_like_mut())
            .ok_or_else(missing)?;
        if levels.remove(level).is_none() {
            return Err(missing());
        }
        if levels.is_empty() {
            power.remove("levels");
        }
        Ok(())
    }

    pub fn set_video(
        &mut self,
        target: &str,
        device: Option<&str>,
        ocr_mode: Option<&str>,
        change_threshold: Option<u8>,
        host: Option<&str>,
        port: Option<u16>,
    ) -> Result<(), LabError> {
        self.set_singleton(
            target,
            "video",
            &[("device", device), ("ocr_mode", ocr_mode), ("host", host)],
        )?;
        let c = self
            .target_mut(target)?
            .get_mut("video")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': video is not a table")))?;
        if let Some(n) = change_threshold {
            c.insert("change_threshold", value(i64::from(n)));
        }
        set_port(c, port);
        Ok(())
    }

    pub fn remove_video(&mut self, target: &str) -> Result<(), LabError> {
        self.remove_singleton(target, "video")
    }

    pub fn set_hid(
        &mut self,
        target: &str,
        cmd: Option<&str>,
        host: Option<&str>,
        port: Option<u16>,
    ) -> Result<(), LabError> {
        self.set_singleton(target, "hid", &[("cmd", cmd), ("host", host)])?;
        let c = self
            .target_mut(target)?
            .get_mut("hid")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': hid is not a table")))?;
        set_port(c, port);
        Ok(())
    }

    pub fn remove_hid(&mut self, target: &str) -> Result<(), LabError> {
        self.remove_singleton(target, "hid")
    }

    pub fn set_usb(
        &mut self,
        target: &str,
        cmd: Option<&str>,
        host: Option<&str>,
    ) -> Result<(), LabError> {
        self.set_singleton(target, "usb", &[("cmd", cmd), ("host", host)])
    }

    pub fn remove_usb(&mut self, target: &str) -> Result<(), LabError> {
        self.remove_singleton(target, "usb")
    }

    pub fn set_adb(
        &mut self,
        target: &str,
        serial: Option<&str>,
        adb: Option<&str>,
        host: Option<&str>,
    ) -> Result<(), LabError> {
        self.set_singleton(
            target,
            "adb",
            &[("serial", serial), ("adb", adb), ("host", host)],
        )
    }

    pub fn remove_adb(&mut self, target: &str) -> Result<(), LabError> {
        self.remove_singleton(target, "adb")
    }

    /// Create or update the named action `name` (#299). `cmd` is required
    /// when creating it. `requires_power` is `"on"`/`"off"`, or `"none"` to
    /// clear it. `host` binds the whole actions table, like any channel. An
    /// action written inline (`reset = { cmd = "…" }`) is edited in place.
    #[allow(clippy::too_many_arguments)]
    pub fn set_action(
        &mut self,
        target: &str,
        name: &str,
        cmd: Option<&str>,
        description: Option<&str>,
        requires_power: Option<&str>,
        host: Option<&str>,
    ) -> Result<(), LabError> {
        model::validate_name("action", name)?;
        if model::ACTIONS_TABLE_KEYS.contains(&name) {
            return lab_err(format!(
                "'{name}' is a setting of the actions table itself; pick another action name"
            ));
        }
        self.set_singleton(target, "actions", &[("host", host)])?;
        let acts = self
            .target_mut(target)?
            .get_mut("actions")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': actions is not a table")))?;
        // Rendered as a bare `[…actions]` header only when it carries a
        // table-level key (host, doctor); otherwise only the
        // `[…actions.<name>]` tables appear.
        set_actions_implicit(acts);
        if !acts.contains_key(name) {
            if cmd.is_none() {
                return lab_err(format!(
                    "target '{target}': new action '{name}' needs --cmd"
                ));
            }
            acts.insert(name, Item::Table(Table::new()));
        }
        let a = acts
            .get_mut(name)
            .and_then(|i| i.as_table_like_mut())
            .ok_or_else(|| {
                LabError(format!("target '{target}': action '{name}' is not a table"))
            })?;
        for (k, v) in [("cmd", cmd), ("description", description)] {
            if let Some(v) = v {
                a.insert(k, value(v));
            }
        }
        match requires_power {
            Some("none") => {
                a.remove("requires_power");
            }
            Some(rp) => {
                a.insert("requires_power", value(rp));
            }
            None => {}
        }
        // An inserted key in an inline table inherits odd spacing (`"x" ,
        // y`); re-space that one table. A standard table needs nothing.
        if let Some(inline) = acts.get_mut(name).and_then(|i| i.as_inline_table_mut()) {
            inline.fmt();
        }
        Ok(())
    }

    /// Remove the named action; the actions table goes too when it was the
    /// last one, as `remove_serial` does for the last interface.
    pub fn remove_action(&mut self, target: &str, name: &str) -> Result<(), LabError> {
        let t = self.target_mut(target)?;
        let acts = t
            .get_mut("actions")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| LabError(format!("target '{target}': no actions channel")))?;
        if model::ACTIONS_TABLE_KEYS.contains(&name) || acts.remove(name).is_none() {
            return lab_err(format!("target '{target}': no action '{name}'"));
        }
        if acts
            .iter()
            .all(|(k, _)| model::ACTIONS_TABLE_KEYS.contains(&k))
        {
            t.remove("actions");
        }
        Ok(())
    }

    /// Turn a channel's opt-in helper doctor check (#304) on or off.
    /// `kind` is `power`, `hid`, `usb`, or `actions` — the channels whose
    /// hooks name a helper program. On writes `doctor = true`; off removes
    /// the key, since off is the default.
    pub fn set_doctor(&mut self, target: &str, kind: &str, on: bool) -> Result<(), LabError> {
        if !DOCTOR_KINDS.contains(&kind) {
            return lab_err(format!("the {kind} channel has no helper doctor check"));
        }
        let c = self
            .target_mut(target)?
            .get_mut(kind)
            .ok_or_else(|| LabError(format!("target '{target}': no {kind} channel")))?
            .as_table_mut()
            .ok_or_else(|| not_a_standard_table(&format!("targets.{target}.{kind}")))?;
        if on {
            c.insert("doctor", value(true));
        } else {
            c.remove("doctor");
        }
        if kind == "actions" {
            set_actions_implicit(c);
        }
        Ok(())
    }
}

/// The channels that can opt in to a helper doctor check.
pub const DOCTOR_KINDS: &[&str] = &["power", "hid", "usb", "actions"];

/// An `actions` table renders as a bare `[…actions]` header only when it
/// carries a table-level key; otherwise only its `[…actions.<name>]` tables
/// appear.
fn set_actions_implicit(acts: &mut Table) {
    let has_own_key = model::ACTIONS_TABLE_KEYS
        .iter()
        .any(|k| acts.contains_key(k));
    acts.set_implicit(!has_own_key);
}

impl LabFile {
    fn target_mut(&mut self, name: &str) -> Result<&mut Table, LabError> {
        let missing = || LabError(format!("no target '{name}'"));
        let targets = self
            .doc
            .get_mut("targets")
            .ok_or_else(missing)?
            .as_table_mut()
            .ok_or_else(|| not_a_standard_table("targets"))?;
        targets
            .get_mut(name)
            .ok_or_else(missing)?
            .as_table_mut()
            .ok_or_else(|| not_a_standard_table(&format!("targets.{name}")))
    }
}

/// The editor works on standard `[key]` tables only. A valid lab may spell
/// the same thing as an inline table (`hosts = { b1 = { ssh = "…" } }`);
/// `toml_edit` represents that as a value, not a `Table`, and rewriting it
/// would destroy the author's layout — so it is reported, never panicked on
/// (this used to be an `expect("super table")`).
fn not_a_standard_table(key: &str) -> LabError {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    LabError(format!(
        "`{key}` is not a standard table — the CLI cannot edit the inline form \
         (`{leaf} = {{ … }}`); rewrite it as a `[{key}]` table by hand"
    ))
}

/// Get or create an implicit super-table so children render as `[key.child]`.
fn super_table<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table, LabError> {
    if doc.get(key).is_none() {
        let mut t = Table::new();
        t.set_implicit(true);
        doc.insert(key, Item::Table(t));
    }
    doc[key]
        .as_table_mut()
        .ok_or_else(|| not_a_standard_table(key))
}

/// Set a key when the value is present; leave it untouched otherwise.
/// A daemon `port`: `Some(0)` drops the key (back to OS-assigned), another
/// `Some` sets it, `None` leaves it unchanged.
fn set_port(t: &mut Table, port: Option<u16>) {
    match port {
        Some(0) => {
            t.remove("port");
        }
        Some(p) => {
            t.insert("port", value(i64::from(p)));
        }
        None => {}
    }
}

fn set_opt(t: &mut Table, key: &str, v: Option<&str>) {
    if let Some(val) = v {
        t.insert(key, value(val));
    }
}

/// The typed view of the live document, for running the shared rulebook
/// ([`model::validate`] on load, [`model::validate_for_save`] on save).
fn lab_of(doc: &DocumentMut) -> Result<Lab, LabError> {
    toml::from_str(&doc.to_string()).map_err(|e| LabError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.toml");
        (dir, path)
    }

    /// Actions (#299): create, update in place, clear requires_power, bind a
    /// host, edit an inline-table action without rewriting it, and drop the
    /// table with its last action.
    #[test]
    fn actions_round_trip() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "[hosts.bench1]\nssh = \"u@bench1\"\n\n[targets.pi5]\n\n\
             [targets.pi5.actions]\n# hand comment\nrecovery = { cmd = \"boardctl mode recovery\" }\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.set_action(
            "pi5",
            "boot-usb",
            Some("boardctl boot usb"),
            Some("Strap USB"),
            Some("off"),
            None,
        )
        .unwrap();
        lf.set_action(
            "pi5",
            "recovery",
            None,
            Some("Reboot into recovery"),
            None,
            Some("bench1"),
        )
        .unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# hand comment"), "{text}");
        assert!(
            text.contains("recovery = { cmd = \"boardctl mode recovery\", description = \"Reboot into recovery\" }"),
            "{text}"
        );
        let lab = model::load(&path).unwrap();
        let acts = lab.targets["pi5"].actions.as_ref().unwrap();
        assert_eq!(acts.host.as_deref(), Some("bench1"));
        assert_eq!(
            acts.actions["boot-usb"].requires_power.as_deref(),
            Some("off")
        );
        assert_eq!(acts.actions["recovery"].cmd, "boardctl mode recovery");

        let mut lf = LabFile::load(&path).unwrap();
        lf.set_action("pi5", "boot-usb", None, None, Some("none"), None)
            .unwrap();
        assert!(lf
            .set_action("pi5", "host", Some("x"), None, None, None)
            .is_err());
        assert!(lf.set_action("pi5", "new", None, None, None, None).is_err());
        assert!(lf
            .set_action("pi5", "-x", Some("x"), None, None, None)
            .is_err());
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert_eq!(
            lab.targets["pi5"].actions.as_ref().unwrap().actions["boot-usb"].requires_power,
            None
        );

        let mut lf = LabFile::load(&path).unwrap();
        lf.remove_action("pi5", "boot-usb").unwrap();
        assert!(lf.remove_action("pi5", "host").is_err());
        assert!(lf.remove_action("pi5", "boot-usb").is_err());
        lf.remove_action("pi5", "recovery").unwrap();
        lf.save().unwrap();
        assert!(model::load(&path).unwrap().targets["pi5"].actions.is_none());
    }

    /// Power levels (#302): create beside the default hooks (keeping a hand
    /// comment), update one hook without touching the others or the default
    /// set, edit an inline level in place, refuse bad names and an empty new
    /// level, and remove one level and then the last.
    #[test]
    fn power_levels_round_trip() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "[targets.pi5]\n\n[targets.pi5.power]\n# soft layer: the board MCU\n\
             cycle_cmd = \"boardctl cycle\"\n\n\
             [targets.pi5.power.levels]\npmic = { on_cmd = \"pmic on\" }\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.set_power_level(
            "pi5",
            "hard",
            Some("shellyplug -d 192.0.2.10 cycle"),
            None,
            None,
            Some("shellyplug -d 192.0.2.10 state"),
        )
        .unwrap();
        lf.set_power_level("pi5", "pmic", None, None, Some("pmic off"), None)
            .unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# soft layer: the board MCU"), "{text}");
        assert!(text.contains("[targets.pi5.power.levels.hard]"), "{text}");
        assert!(
            text.contains("pmic = { on_cmd = \"pmic on\", off_cmd = \"pmic off\" }"),
            "{text}"
        );
        let lab = model::load(&path).unwrap();
        let p = lab.targets["pi5"].power.as_ref().unwrap();
        assert_eq!(p.cycle_cmd.as_deref(), Some("boardctl cycle"));
        assert_eq!(
            p.levels["hard"].state_cmd.as_deref(),
            Some("shellyplug -d 192.0.2.10 state")
        );

        let mut lf = LabFile::load(&path).unwrap();
        lf.set_power_level(
            "pi5",
            "hard",
            None,
            Some("shellyplug -d 192.0.2.10 on"),
            None,
            None,
        )
        .unwrap();
        assert!(lf
            .set_power_level("pi5", "new", None, None, None, None)
            .is_err());
        assert!(lf
            .set_power_level("pi5", "default", Some("x"), None, None, None)
            .is_err());
        assert!(lf
            .set_power_level("pi5", "-x", Some("x"), None, None, None)
            .is_err());
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        let hard = &lab.targets["pi5"].power.as_ref().unwrap().levels["hard"];
        assert_eq!(
            hard.cycle_cmd.as_deref(),
            Some("shellyplug -d 192.0.2.10 cycle")
        );
        assert_eq!(hard.on_cmd.as_deref(), Some("shellyplug -d 192.0.2.10 on"));

        let mut lf = LabFile::load(&path).unwrap();
        lf.remove_power_level("pi5", "hard").unwrap();
        assert!(lf.remove_power_level("pi5", "hard").is_err());
        lf.remove_power_level("pi5", "pmic").unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("levels"), "{text}");
        assert!(text.contains("# soft layer: the board MCU"), "{text}");
        let lab = model::load(&path).unwrap();
        let p = lab.targets["pi5"].power.as_ref().unwrap();
        assert!(p.levels.is_empty());
        assert_eq!(p.cycle_cmd.as_deref(), Some("boardctl cycle"));

        // A level on a target with no power channel creates the channel.
        let mut lf = LabFile::load(&path).unwrap();
        lf.add_target("nuc", None, None).unwrap();
        lf.set_power_level("nuc", "hard", Some("x"), None, None, None)
            .unwrap();
        assert!(lf.remove_power_level("pi5", "nope").is_err());
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        let p = lab.targets["nuc"].power.as_ref().unwrap();
        assert_eq!(p.cycle_cmd, None);
        assert_eq!(p.levels["hard"].cycle_cmd.as_deref(), Some("x"));
    }

    /// The opt-in helper doctor flag (#304): on writes `doctor = true`, off
    /// removes the key, hand comments survive, and `doctor` is reserved in
    /// the actions table the same way `host` is.
    #[test]
    fn doctor_flag_round_trips_on_every_hook_channel() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "[targets.pi5]\n\n[targets.pi5.power]\n# plug on the bench\n\
             cycle_cmd = \"shellyplug -d 192.0.2.10 cycle\"\n\n\
             [targets.pi5.hid]\ncmd = \"ch9329 -d /dev/ttyUSB0\"\n\n\
             [targets.pi5.usb]\ncmd = \"ch9329 -d /dev/ttyUSB0\"\n\n\
             [targets.pi5.actions.reset]\ncmd = \"boardctl reset\"\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        for kind in DOCTOR_KINDS {
            lf.set_doctor("pi5", kind, true).unwrap();
        }
        assert!(lf.set_doctor("pi5", "video", true).is_err());
        assert!(lf.set_doctor("pi5", "adb", true).is_err());
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# plug on the bench"), "{text}");
        assert!(
            text.contains("[targets.pi5.actions]\ndoctor = true"),
            "{text}"
        );
        let lab = model::load(&path).unwrap();
        let t = &lab.targets["pi5"];
        assert!(t.power.as_ref().unwrap().doctor);
        assert!(t.hid.as_ref().unwrap().doctor);
        assert!(t.usb.as_ref().unwrap().doctor);
        let acts = t.actions.as_ref().unwrap();
        assert!(acts.doctor);
        assert_eq!(acts.actions.keys().collect::<Vec<_>>(), ["reset"]);

        let mut lf = LabFile::load(&path).unwrap();
        assert!(lf
            .set_action("pi5", "doctor", Some("x"), None, None, None)
            .is_err());
        assert!(lf.remove_action("pi5", "doctor").is_err());
        for kind in DOCTOR_KINDS {
            lf.set_doctor("pi5", kind, false).unwrap();
        }
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("doctor"), "{text}");
        // Off again: the actions table goes back to an implicit header.
        assert!(!text.contains("[targets.pi5.actions]\n"), "{text}");

        // The last action going takes a table that only says `doctor` with it.
        let mut lf = LabFile::load(&path).unwrap();
        lf.set_doctor("pi5", "actions", true).unwrap();
        lf.remove_action("pi5", "reset").unwrap();
        lf.save().unwrap();
        assert!(model::load(&path).unwrap().targets["pi5"].actions.is_none());

        // A channel that does not exist cannot be opted in.
        let mut lf = LabFile::load(&path).unwrap();
        assert!(lf.set_doctor("pi5", "actions", true).is_err());
        assert!(lf.set_doctor("ghost", "power", true).is_err());
    }

    #[test]
    fn build_round_trips() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_host(
            "bench1",
            "u@bench1",
            None,
            None,
            Some("~/.ssh/id"),
            None,
            None,
            None,
        )
        .unwrap();
        lf.add_target("fortune", Some("bench1"), Some("a pi"))
            .unwrap();
        lf.set_netboot(
            "fortune",
            Some("en0"),
            None,
            Some("/srv/tftp"),
            Some("grubaa64.efi"),
            None,
            None,
            None,
        )
        .unwrap();
        lf.add_serial(
            "fortune",
            "console",
            "/dev/ttyUSB0",
            115200,
            None,
            false,
            None,
            None,
        )
        .unwrap();
        lf.add_serial(
            "fortune",
            "bmc",
            "/dev/ttyUSB1",
            9600,
            Some("cts"),
            false,
            None,
            None,
        )
        .unwrap();
        lf.save().unwrap();

        let lab = model::load(&path).unwrap();
        let t = &lab.targets["fortune"];
        assert_eq!(t.host.as_deref(), Some("bench1"));
        assert_eq!(
            t.netboot.as_ref().unwrap().interface.as_deref(),
            Some("en0")
        );
        assert_eq!(
            t.netboot.as_ref().unwrap().boot_file.as_deref(),
            Some("grubaa64.efi"),
            "boot_file round-trips through save/load"
        );
        assert_eq!(t.serial.len(), 2);
        assert_eq!(t.serial[1].baud, 9600);
    }

    /// `plugin_path` survives add → save → load, `host set` replaces it, and
    /// an empty value removes the key rather than storing "".
    #[test]
    fn host_plugin_path_round_trips_and_clears() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_host(
            "bench1",
            "u@bench1",
            None,
            None,
            None,
            None,
            None,
            Some("/opt/lab-plugins"),
        )
        .unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert_eq!(
            lab.hosts["bench1"].plugin_path.as_deref(),
            Some("/opt/lab-plugins")
        );

        let mut lf = LabFile::load(&path).unwrap();
        let both = "/opt/lab-plugins:/srv/more";
        lf.update_host("bench1", None, None, None, None, None, None, Some(both))
            .unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert_eq!(lab.hosts["bench1"].plugin_path.as_deref(), Some(both));

        let mut lf = LabFile::load(&path).unwrap();
        lf.update_host("bench1", None, None, None, None, None, None, Some(""))
            .unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("plugin_path"), "{text}");
        assert!(model::load(&path).unwrap().hosts["bench1"]
            .plugin_path
            .is_none());
    }

    #[test]
    fn comments_preserved_across_edit() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "# hand-written\n[hosts.bench1]\nssh = \"u@b1\"  # noisy\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.update_host(
            "bench1",
            None,
            None,
            None,
            Some("~/.ssh/id"),
            None,
            None,
            None,
        )
        .unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# hand-written"), "{text}");
        assert!(text.contains("# noisy"), "{text}");
        assert!(text.contains("identity"), "{text}");
    }

    #[test]
    fn rename_target_carries_channels_comments_and_position() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "[targets.old]\nhost = \"b1\"  # pinned\n\n# capture dongle\n[targets.old.video]\ndevice = \"/dev/v0\"\n\n[[targets.old.serial]]\nname = \"console\"\ndevice = \"/dev/a\"\nbaud = 115200\n\n[hosts.b1]\nssh = \"u@b1\"\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.rename_target("old", "new").unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[targets.new]"), "{text}");
        assert!(text.contains("[targets.new.video]"), "{text}");
        assert!(text.contains("[[targets.new.serial]]"), "{text}");
        assert!(!text.contains("targets.old"), "{text}");
        assert!(text.contains("# capture dongle"), "{text}");
        assert!(text.contains("# pinned"), "{text}");
        assert!(
            text.find("[targets.new]").unwrap() < text.find("[hosts.b1]").unwrap(),
            "renamed target should keep its place in the document:\n{text}"
        );
        let lab = model::load(&path).unwrap();
        let t = &lab.targets["new"];
        assert_eq!(t.host.as_deref(), Some("b1"));
        assert_eq!(t.serial.len(), 1);
        assert!(t.video.is_some());
    }

    #[test]
    fn rename_target_refuses_missing_and_collision() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("a", None, None).unwrap();
        lf.add_target("b", None, None).unwrap();
        let e = lf.rename_target("a", "b").unwrap_err();
        assert!(e.0.contains("already exists"), "{}", e.0);
        let e = lf.rename_target("ghost", "c").unwrap_err();
        assert!(e.0.contains("no target 'ghost'"), "{}", e.0);
    }

    #[test]
    fn remove_host_blocked_while_referenced() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_host("bench1", "u@b1", None, None, None, None, None, None)
            .unwrap();
        lf.add_target("fortune", Some("bench1"), None).unwrap();
        let e = lf.remove_host("bench1").unwrap_err();
        assert!(e.0.contains("still used by: fortune"), "{}", e.0);
    }

    #[test]
    fn duplicate_serial_rejected() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.add_serial("t", "console", "/dev/a", 115200, None, false, None, None)
            .unwrap();
        let e = lf
            .add_serial("t", "console", "/dev/b", 115200, None, false, None, None)
            .unwrap_err();
        assert!(e.0.contains("already exists"), "{}", e.0);
    }

    #[test]
    fn unknown_host_ref_fails_on_save() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", Some("ghost"), None).unwrap();
        assert!(lf.save().is_err());
    }

    #[test]
    fn set_power_writes_new_hook_fields() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.set_power(
            "t",
            Some("cycle.sh"),
            Some("on.sh"),
            Some("off.sh"),
            Some("state.sh"),
            None,
            None,
        )
        .unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        let p = lab.targets["t"].power.as_ref().unwrap();
        assert_eq!(p.cycle_cmd.as_deref(), Some("cycle.sh"));
        assert_eq!(p.on_cmd.as_deref(), Some("on.sh"));
        assert_eq!(p.off_cmd.as_deref(), Some("off.sh"));
        assert_eq!(p.state_cmd.as_deref(), Some("state.sh"));
    }

    #[test]
    fn set_power_partial_update_preserves_others() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.set_power("t", Some("cycle.sh"), None, None, None, None, None)
            .unwrap();
        lf.set_power("t", None, Some("on.sh"), None, None, None, None)
            .unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        let p = lab.targets["t"].power.as_ref().unwrap();
        assert_eq!(
            p.cycle_cmd.as_deref(),
            Some("cycle.sh"),
            "cycle_cmd preserved"
        );
        assert_eq!(p.on_cmd.as_deref(), Some("on.sh"), "on_cmd set");
    }

    #[test]
    fn set_hid_round_trips_and_removes() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.set_hid("t", Some("hidrig -d /dev/cu.usbserial-AB12"), None, None)
            .unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        let h = lab.targets["t"].hid.as_ref().unwrap();
        assert_eq!(h.cmd.as_deref(), Some("hidrig -d /dev/cu.usbserial-AB12"));

        lf.remove_hid("t").unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert!(lab.targets["t"].hid.is_none());
    }

    #[test]
    fn set_adb_round_trips_and_removes() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("pixel", None, None).unwrap();
        lf.set_adb("pixel", Some("39021FDH200xyz"), None, None)
            .unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        let a = lab.targets["pixel"].adb.as_ref().unwrap();
        assert_eq!(a.serial.as_deref(), Some("39021FDH200xyz"));
        assert!(a.adb.is_none());

        lf.remove_adb("pixel").unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert!(lab.targets["pixel"].adb.is_none());
    }

    #[test]
    fn update_target_migrates_legacy_note_to_description() {
        // A hand-edited lab using the legacy `note` key must stay mutable:
        // setting a description replaces `note` rather than colliding with it
        // (both would deserialize as a duplicate `description` via the alias).
        let (_d, path) = tmp();
        std::fs::write(&path, "[targets.oldpi]\nnote = \"legacy\"\n").unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.update_target("oldpi", None, Some("modern")).unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("description = \"modern\""), "{text}");
        assert!(!text.contains("note ="), "legacy note key removed: {text}");
        let lab = model::load(&path).unwrap();
        assert_eq!(lab.targets["oldpi"].description.as_deref(), Some("modern"));
    }

    #[test]
    fn remove_last_serial_drops_array() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.add_serial("t", "console", "/dev/a", 115200, None, false, None, None)
            .unwrap();
        lf.remove_serial("t", "console").unwrap();
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert!(lab.targets["t"].serial.is_empty());
    }

    #[test]
    fn set_video_round_trips_ocr_mode_and_keeps_it_across_a_device_change() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.set_video("t", Some("/dev/video0"), Some("gui"), None, None, None)
            .unwrap();
        lf.save().unwrap();
        let v = model::load(&path).unwrap().targets["t"]
            .video
            .clone()
            .unwrap();
        assert_eq!(v.device.as_deref(), Some("/dev/video0"));
        assert_eq!(v.ocr_mode.as_deref(), Some("gui"));
        // Changing only the device leaves the mode alone.
        lf.set_video("t", Some("/dev/video1"), None, None, None, None)
            .unwrap();
        lf.save().unwrap();
        let v = model::load(&path).unwrap().targets["t"]
            .video
            .clone()
            .unwrap();
        assert_eq!(v.device.as_deref(), Some("/dev/video1"));
        assert_eq!(v.ocr_mode.as_deref(), Some("gui"));
        // An unknown mode is refused at save, like any other bad field.
        lf.set_video("t", None, Some("fast"), None, None, None)
            .unwrap();
        assert!(lf.save().unwrap_err().0.contains("invalid ocr_mode 'fast'"));
    }

    /// `save` replaces the file by rename rather than truncating and
    /// rewriting it in place — that is what makes a concurrent reader (or a
    /// crash mid-write) see either the old lab or the new one, never an
    /// empty or partial file. On Unix the replacement is a new inode.
    #[cfg(unix)]
    #[test]
    fn save_replaces_the_file_by_rename() {
        use std::os::unix::fs::MetadataExt;
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.save().unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();
        lf.add_target("u", None, None).unwrap();
        lf.save().unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            before,
            "the lab must be replaced whole, not rewritten in place"
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("[targets.u]"));
        // No temp file left beside it.
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["lab.toml"], "{names:?}");
    }

    /// The lab file is often a symlink into a git checkout. Saving must write
    /// the checkout's file *through* the link and leave the link standing —
    /// a rename onto the link itself would replace it with a plain file and
    /// silently detach the lab from version control.
    #[cfg(unix)]
    #[test]
    fn save_through_a_symlink_keeps_the_link_and_updates_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("checkout").join("lab.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "# in git\n[targets.old]\n").unwrap();
        let link = dir.path().join("lab.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut lf = LabFile::load(&link).unwrap();
        lf.add_target("new", None, None).unwrap();
        lf.save().unwrap();

        let md = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            md.file_type().is_symlink(),
            "the link was replaced by a file"
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        let text = std::fs::read_to_string(&real).unwrap();
        assert!(text.contains("# in git"), "{text}");
        assert!(text.contains("[targets.new]"), "{text}");
    }

    /// `hosts = { … }` is valid TOML the model reads fine, but `toml_edit`
    /// holds it as a value, not a table. Editing it used to hit
    /// `expect("super table")` and abort; now it is a lab error that says
    /// what to rewrite.
    #[test]
    fn inline_hosts_table_is_an_error_not_a_panic() {
        let (_d, path) = tmp();
        std::fs::write(&path, "hosts = { b1 = { ssh = \"u@b1\" } }\n").unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        let e = lf
            .add_host("b2", "u@b2", None, None, None, None, None, None)
            .unwrap_err();
        assert!(e.0.contains("`hosts` is not a standard table"), "{}", e.0);
        assert!(e.0.contains("[hosts]"), "{}", e.0);
    }

    #[test]
    fn inline_target_table_is_an_error_not_a_panic() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "[targets]\nt = { description = \"inline\" }\n[targets.plain]\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        let e = lf
            .set_video("t", Some("/dev/video0"), None, None, None, None)
            .unwrap_err();
        assert!(
            e.0.contains("`targets.t` is not a standard table"),
            "{}",
            e.0
        );
        assert!(e.0.contains("[targets.t]"), "{}", e.0);
        // A sibling written the standard way is still editable.
        lf.set_video("plain", Some("/dev/video0"), None, None, None, None)
            .unwrap();
        // And a whole inline `targets` is reported at that level.
        std::fs::write(&path, "targets = { t = { } }\n").unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        let e = lf.update_target("t", None, Some("x")).unwrap_err();
        assert!(e.0.contains("`targets` is not a standard table"), "{}", e.0);
    }

    /// Names are checked where they are chosen. A bad name never reaches the
    /// document, so the file on disk stays loadable by every path.
    #[test]
    fn add_rename_and_set_reject_invalid_names() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        for bad in ["", ".", "..", "-x", "a b", "a/b"] {
            let e = lf
                .add_host(bad, "u@b", None, None, None, None, None, None)
                .unwrap_err();
            assert!(e.0.starts_with("invalid host name"), "{bad:?}: {}", e.0);
            let e = lf.add_target(bad, None, None).unwrap_err();
            assert!(e.0.starts_with("invalid target name"), "{bad:?}: {}", e.0);
        }
        lf.add_target("ok", None, None).unwrap();
        let e = lf.rename_target("ok", "not ok").unwrap_err();
        assert!(e.0.starts_with("invalid target name 'not ok'"), "{}", e.0);
        let e = lf
            .add_serial("ok", "con sole", "/dev/a", 115200, None, false, None, None)
            .unwrap_err();
        assert!(
            e.0.starts_with("invalid serial interface name 'con sole'"),
            "{}",
            e.0
        );
        let e = lf.update_target("../x", None, Some("d")).unwrap_err();
        assert!(e.0.starts_with("invalid target name"), "{}", e.0);
        // Nothing bad landed in the document.
        lf.save().unwrap();
        let lab = model::load(&path).unwrap();
        assert_eq!(lab.target_names(), vec!["ok"]);
        assert!(lab.hosts.is_empty());
    }

    /// A `power.serial_interface` must name one of the target's interfaces
    /// at save time — whether the reference was just set, or the interface
    /// it names was just removed.
    #[test]
    fn save_refuses_a_dangling_power_serial_interface() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_target("t", None, None).unwrap();
        lf.set_power("t", None, None, None, None, Some("console"), None)
            .unwrap();
        let e = lf.save().unwrap_err();
        assert!(e.0.contains("power.serial_interface 'console'"), "{}", e.0);
        lf.add_serial("t", "console", "/dev/a", 115200, None, false, None, None)
            .unwrap();
        lf.save().unwrap();
        lf.remove_serial("t", "console").unwrap();
        let e = lf.save().unwrap_err();
        assert!(e.0.contains("power.serial_interface 'console'"), "{}", e.0);
    }

    /// A second netboot link on the same host must not be left in the first
    /// link's /24 — not by an explicit `--host-ip`, and not by omitting it.
    /// The edit that gives it its own /24 goes through, as does an edit to a
    /// target that is not part of the clash.
    #[test]
    fn set_netboot_refuses_a_second_link_in_the_first_links_slash_24() {
        let (_d, path) = tmp();
        let mut lf = LabFile::create(&path);
        lf.add_host("bench1", "u@bench1", None, None, None, None, None, None)
            .unwrap();
        for t in ["a", "b", "c"] {
            lf.add_target(t, Some("bench1"), None).unwrap();
        }
        lf.set_netboot("a", Some("eth1"), None, None, None, None, None, None)
            .unwrap();
        // b at the default by omission: refused, and said so.
        let e = lf
            .set_netboot("b", Some("eth2"), None, None, None, None, None, None)
            .unwrap_err();
        assert!(e.0.contains("(the default, since none is set)"), "{}", e.0);
        assert!(e.0.contains("target 'a' already uses on eth1"), "{}", e.0);
        // b explicitly in a's /24: refused too.
        let e = lf
            .set_netboot(
                "b",
                None,
                Some("192.168.99.7"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(e.0.contains("host_ip 192.168.99.7 puts eth2"), "{}", e.0);
        // Its own /24 is fine, and the file saves.
        lf.set_netboot(
            "b",
            None,
            Some("192.168.100.1"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        lf.save().unwrap();
        // The same adapter time-shared by another target is not a clash.
        lf.set_netboot("c", Some("eth1"), None, None, None, None, None, None)
            .unwrap();
        // A clash written by hand still loads and saves: editing an uninvolved
        // target is never blocked by it, and the repair itself goes through.
        std::fs::write(
            &path,
            "[hosts.bench1]\nssh = \"u@bench1\"\n\
             [targets.a]\nhost = \"bench1\"\n[targets.a.netboot]\ninterface = \"eth1\"\n\
             [targets.b]\nhost = \"bench1\"\n[targets.b.netboot]\ninterface = \"eth2\"\n\
             [targets.c]\nhost = \"bench1\"\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.set_netboot(
            "c",
            Some("eth3"),
            Some("192.168.101.1"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        lf.save().unwrap();
        let e = lf
            .set_netboot("b", None, None, Some("/srv/tftp/b"), None, None, None, None)
            .unwrap_err();
        assert!(e.0.contains("target 'b' netboot"), "{}", e.0);
        lf.set_netboot(
            "b",
            None,
            Some("192.168.100.1"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        lf.save().unwrap();
    }

    /// `--port N` writes the key, a later edit without it keeps it, and
    /// `--port 0` drops it back to OS-assigned — on video, hid and serial.
    #[test]
    fn port_round_trips_and_zero_clears_it() {
        let (_d, path) = tmp();
        std::fs::write(&path, "[targets.t]\n").unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.set_video("t", Some("cap"), None, None, None, Some(8723))
            .unwrap();
        lf.set_hid("t", Some("hidrig"), None, Some(8725)).unwrap();
        lf.add_serial("t", "c", "/dev/x", 115200, None, false, None, Some(8724))
            .unwrap();
        lf.save().unwrap();
        let t = model::load(&path).unwrap().targets["t"].clone();
        assert_eq!(t.video.as_ref().unwrap().port, Some(8723));
        assert_eq!(t.hid.as_ref().unwrap().port, Some(8725));
        assert_eq!(t.serial_port(), 8724);

        lf.set_video("t", Some("cap2"), None, None, None, None)
            .unwrap();
        lf.update_serial("t", "c", None, None, None, None, None, Some(0))
            .unwrap();
        lf.set_hid("t", Some("hidrig"), None, Some(0)).unwrap();
        lf.save().unwrap();
        let t = model::load(&path).unwrap().targets["t"].clone();
        assert_eq!(t.video.as_ref().unwrap().port, Some(8723), "kept");
        assert_eq!(t.hid.as_ref().unwrap().port, None, "cleared");
        assert_eq!(t.serial_port(), 0, "cleared");
    }

    #[test]
    fn set_video_round_trips_change_threshold_and_keeps_comments() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            "# lab\n[targets.t]\n[targets.t.video]\n# the capture dongle\ndevice = \"/dev/video0\"\n",
        )
        .unwrap();
        let mut lf = LabFile::load(&path).unwrap();
        lf.set_video("t", None, None, Some(0), None, None).unwrap();
        lf.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# the capture dongle"), "{text}");
        assert!(text.contains("# lab"), "{text}");
        assert!(text.contains("change_threshold = 0"), "{text}");
        let v = model::load(&path).unwrap().targets["t"]
            .video
            .clone()
            .unwrap();
        assert_eq!(v.change_threshold, Some(0));
        assert_eq!(v.device.as_deref(), Some("/dev/video0"));
        // Leaving it out of a later edit keeps it.
        lf.set_video("t", Some("/dev/video1"), None, None, None, None)
            .unwrap();
        lf.save().unwrap();
        let v = model::load(&path).unwrap().targets["t"]
            .video
            .clone()
            .unwrap();
        assert_eq!(v.change_threshold, Some(0));
        // A hand-written out-of-range value is refused on load.
        std::fs::write(
            &path,
            "[targets.t]\n[targets.t.video]\nchange_threshold = 256\n",
        )
        .unwrap();
        assert!(model::load(&path).is_err());
    }
}
