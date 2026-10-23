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

//! The lab model: one git-tracked file describing all hosts and targets.
//!
//! A *lab* is a single TOML file (default `~/.config/panio/lab.toml`, override
//! with `--lab` / `$PANIO_LAB`) that declares every control host panio
//! reaches over SSH and every target, with each channel of a target's hardware
//! bound to a host (its own `host`, else the target's `host`, else `local`).
//!
//! This module is the typed/read side: `serde` structs, [`validate`], the
//! resolved per-channel view ([`Lab::resolved_target`]), the inverse index
//! ([`Lab::channels_on_host`]), and [`Lab::host_slice`] (the single-host
//! flattening that runs locally or is shipped to a control host). Editing lives
//! in [`crate::labfile`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// ssh destination meaning "the dev machine itself — no SSH".
pub const LOCAL: &str = "local";
pub const DEFAULT_HOST_IP: &str = "192.168.99.1";

/// The /24 a netboot link occupies, as its first three octets. netbootd
/// advertises a `255.255.255.0` mask and derives the client lease inside it
/// (`netbootd::dhcp::derive_client_ip`), so the link *is* the host IP's /24.
/// `None` when the address is not IPv4, which [`validate`] reports on its own.
pub fn netboot_subnet(host_ip: &str) -> Option<[u8; 3]> {
    let [a, b, c, _] = host_ip.parse::<std::net::Ipv4Addr>().ok()?.octets();
    Some([a, b, c])
}

/// `192.0.2.0/24`-style rendering of a [`netboot_subnet`].
pub fn subnet_str(subnet: [u8; 3]) -> String {
    format!("{}.{}.{}.0/24", subnet[0], subnet[1], subnet[2])
}

/// A netboot link that shares its /24 with another target's link on the same
/// control host, on a different interface. See [`netboot_subnet_clash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubnetClash {
    /// The control host both links are on.
    pub host: String,
    /// The shared subnet, e.g. `192.0.2.0/24`.
    pub subnet: String,
    /// This target's interface and effective host IP.
    pub interface: String,
    pub host_ip: String,
    /// Whether this target's host IP is [`DEFAULT_HOST_IP`] by omission.
    pub defaulted: bool,
    /// The other target and the interface its link is on.
    pub other_target: String,
    pub other_interface: String,
}
pub const VALID_SENSE_SIGNALS: [&str; 4] = ["cts", "dsr", "dcd", "ri"];
/// The `video.device` value meaning "the RFB feed served by this target's own
/// hid-channel daemon" (any hid helper that serves `GET /rfb`). hdmicap is
/// handed the daemon's discovery file, not a port, so a restarted hid daemon is
/// found again. Needs a hid channel on the same host.
pub const RFB_HID_DEVICE: &str = "rfb+hid:";

/// Whether a `video.device` string names a network RFB source rather than a
/// capture device. The same prefixes hdmicap's own device parser treats as
/// network sources (hdmicap/src/rfb.rs `is_rfb_spec`, plus `rfb+hid:`, which
/// only the CLI understands).
pub fn is_network_device(device: &str) -> bool {
    device == RFB_HID_DEVICE
        || device == "rfb+discovery:"
        || device.starts_with("rfb://")
        || device.starts_with("rfb+ws://")
}

/// The `video.ocr_mode` values (see [`VideoChannel::ocr_mode`]).
pub const VALID_OCR_MODES: [&str; 2] = ["text", "gui"];

/// The lab file is malformed or a mutation would make it invalid.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LabError(pub String);

fn lab_err<T>(msg: impl Into<String>) -> Result<T, LabError> {
    Err(LabError(msg.into()))
}

/// What a target, host, or serial interface name may look like. Names become
/// path components (`<runtime-base>/serialcap/<target>/`,
/// `~/.local/share/panio/<target>/`), daemon query values
/// (`?interface=<name>`), and positional arguments to a re-exec'd `panio`
/// on a control host, so they are confined to what survives all three
/// unquoted — and a leading `-` is refused because that positional would be
/// read as an option.
pub const NAME_RULE: &str =
    "letters, digits, `.`, `_` and `-` only; not `.` or `..`; not starting with `-`";

/// Whether `name` satisfies [`NAME_RULE`].
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Reject a `what` (target/host/serial interface) name that fails
/// [`NAME_RULE`], saying which rule and what the name was for. Applied by the
/// editor when a name is chosen (`add`, `rename`, `set`), not on load, so a
/// hand-written lab with an odd name still reads.
pub fn validate_name(what: &str, name: &str) -> Result<(), LabError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        lab_err(format!("invalid {what} name '{name}': {NAME_RULE}"))
    }
}

/// A fault rule needs a path-safe name and a nonempty pattern of at most
/// [`MAX_FAULT_PATTERN`] bytes.
pub fn validate_fault_rule(name: &str, pattern: &str) -> Result<(), LabError> {
    validate_name("fault rule", name)?;
    if pattern.is_empty() {
        return lab_err(format!("fault rule '{name}': pattern is empty"));
    }
    if pattern.len() > MAX_FAULT_PATTERN {
        return lab_err(format!(
            "fault rule '{name}': pattern is {} bytes (at most {MAX_FAULT_PATTERN})",
            pattern.len()
        ));
    }
    Ok(())
}

fn default_baud() -> i64 {
    115200
}

// ── typed schema (serde) ────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Host {
    pub ssh: String,
    /// Free-text description of this control host (its role, location, etc.),
    /// shown by `host show` / `config show`. Purely informational.
    pub description: Option<String>,
    /// The host's fully-qualified hostname, used to recognize *this* machine
    /// (see [`Host::is_local`]). Distinct from `ssh`, which is only how *other*
    /// machines reach it and may be an `~/.ssh/config` alias.
    pub hostname: Option<String>,
    pub identity: Option<String>,
    pub control_path: Option<String>,
    pub panio_cmd: Option<String>,
    /// Plugin roots for this host (`PANIO_PLUGIN_PATH` syntax, in the
    /// host's own form: `:`-separated on Unix, `;` on Windows). Set on the
    /// remote panio whenever a command is dispatched here, via
    /// `--plugin-path` ([`Host::panio_argv`]); unset leaves the host's own
    /// environment in charge. Not applied when this host is the local one.
    pub plugin_path: Option<String>,
}

impl Host {
    pub fn is_local(&self, name: &str) -> bool {
        if self.ssh == LOCAL || name == LOCAL {
            return true;
        }
        // A host whose declared `hostname` (FQDN) matches this machine's is
        // local — so one shared lab file can be run from any box and each
        // recognizes its own host. `ssh` is intentionally NOT used here: it may
        // be an ~/.ssh/config alias, not the machine's real name.
        match (&self.hostname, local_fqdn()) {
            (Some(h), Some(local)) => h.eq_ignore_ascii_case(local),
            _ => false,
        }
    }

    /// How to invoke panio on this host (bare `panio` unless pinned).
    pub fn panio(&self) -> String {
        self.panio_cmd
            .clone()
            .unwrap_or_else(|| "panio".to_string())
    }

    /// The leading argv for a panio re-exec on this host: [`Host::panio`],
    /// then `--plugin-path=<plugin_path>` when the host has one. Every remote
    /// panio invocation starts here, so the host's plugin roots reach it
    /// whichever transport carries the command (interactive included). The
    /// `=` form keeps any value a value, even one that begins with `-`.
    pub fn panio_argv(&self) -> Vec<String> {
        let mut argv = vec![self.panio()];
        if let Some(p) = &self.plugin_path {
            argv.push(format!("--plugin-path={p}"));
        }
        argv
    }
}

/// This machine's fully-qualified hostname, detected once and cached
/// (lowercased). [`Host::is_local`] compares it against each host's declared
/// `hostname` to decide which host is *this* one. `hostname -f` is the most
/// portable FQDN source across macOS/Linux; fall back to the bare node name,
/// then give up (None → nothing matches by hostname, so only the `local`
/// sentinel marks a host local — i.e. today's driver-machine behavior).
pub fn local_fqdn() -> Option<&'static str> {
    static FQDN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FQDN.get_or_init(|| {
        let run = |args: &[&str]| {
            std::process::Command::new("hostname")
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .to_ascii_lowercase()
                })
                .filter(|s| !s.is_empty())
        };
        run(&["-f"]).or_else(|| run(&[]))
    })
    .as_deref()
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct NetbootChannel {
    pub interface: Option<String>,
    pub host_ip: Option<String>,
    pub tftp_root: Option<String>,
    /// Boot program served to UEFI clients: a bare filename relative to
    /// `tftp_root` (e.g. `grubaa64.efi`). Advertised as a TFTP filename to PXE
    /// clients and wrapped in an `http://` URL for HTTP Boot clients.
    pub boot_file: Option<String>,
    /// HTTP server port, also embedded in the HTTP Boot URL (default 80).
    pub http_port: Option<String>,
    /// `Content-Type` for HTTP responses (default `application/octet-stream`).
    pub content_type: Option<String>,
    /// Shell command run on the netboot channel's host by `netif down-hard`,
    /// before the host-side steps, to guarantee a carrier drop the host cannot
    /// force itself (typically a managed-switch port disable, e.g.
    /// `switchport -d sw1 --mgmt-port 8 port 3 off`). Runs like a power hook.
    pub link_down_cmd: Option<String>,
    /// Shell command run before `netif mode link|netboot|ffx` bring the link
    /// up, undoing `link_down_cmd` (e.g. `switchport -d sw1 --mgmt-port 8
    /// port 3 on`). Must be idempotent: it runs on every such mode change.
    pub link_up_cmd: Option<String>,
    pub host: Option<String>,
}

impl NetbootChannel {
    /// The host IP the link will actually use: the field, else
    /// [`DEFAULT_HOST_IP`]. Every consumer that assigns or displays the
    /// address goes through here so the default is never invisible.
    pub fn effective_host_ip(&self) -> &str {
        self.host_ip.as_deref().unwrap_or(DEFAULT_HOST_IP)
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SerialChannel {
    pub name: String,
    pub device: String,
    #[serde(default = "default_baud")]
    pub baud: i64,
    pub power_sense_signal: Option<String>,
    /// Opt-in: the FTDI DTR line on this interface is wired to the board's J2
    /// power-button header, so `serial dtr` / `serial reset` may drive it.
    /// Off by default — DTR-to-J2 wiring is the rare exception, and toggling an
    /// unwired line silently no-ops. The DTR commands refuse interfaces that
    /// haven't set this. See docs/power.md.
    #[serde(default)]
    pub power_button: bool,
    pub host: Option<String>,
    /// Fixed port for the target's serialcap daemon, which owns every
    /// interface — so set it on any one interface; two interfaces naming
    /// different ports is a validation error. Unset or 0 = OS-assigned.
    pub port: Option<u16>,
    /// Named fault rules: rule name -> literal byte pattern. `serial wait`
    /// ends with `status: "fault"` naming the rule when one appears in the
    /// newly acquired bytes. Literal substrings only, no regex.
    #[serde(default)]
    pub faults: BTreeMap<String, String>,
}

/// Most fault rules one serial interface may carry.
pub const MAX_FAULT_RULES: usize = 32;
/// Longest fault pattern, in bytes.
pub const MAX_FAULT_PATTERN: usize = 1024;

#[derive(Debug, Default, Clone, Deserialize)]
pub struct PowerChannel {
    pub cycle_cmd: Option<String>,
    pub on_cmd: Option<String>,
    pub off_cmd: Option<String>,
    pub state_cmd: Option<String>,
    pub serial_interface: Option<String>,
    pub host: Option<String>,
    /// Opt-in (#304): `panio doctor` also runs the hook program's own
    /// read-only `doctor --json` check (docs/dev/adding-power-helpers.md),
    /// for the default hooks and every named level's.
    #[serde(default)]
    pub doctor: bool,
    /// Named power levels (#302) beside the default hook set above, for a
    /// target with more than one independent power layer: a `soft` PMIC or
    /// board-MCU switch and a `hard` mains/DC cut, say. Every level runs on
    /// the power channel's host. `--level NAME` selects one; without it the
    /// default hooks above apply, so a lab without levels is unchanged.
    #[serde(default)]
    pub levels: BTreeMap<String, PowerLevel>,
}

/// One named power level's hooks: the same four as the default level, each
/// optional. A level never falls back to the default level's hook: a verb
/// whose hook the level lacks is refused, naming the level.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
pub struct PowerLevel {
    pub cycle_cmd: Option<String>,
    pub on_cmd: Option<String>,
    pub off_cmd: Option<String>,
    pub state_cmd: Option<String>,
}

impl PowerLevel {
    /// The level's hooks as `(key, cmd)` pairs, in the order doctor and the
    /// editor use: cycle, on, off, state.
    pub fn hooks(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("cycle_cmd", self.cycle_cmd.as_deref()),
            ("on_cmd", self.on_cmd.as_deref()),
            ("off_cmd", self.off_cmd.as_deref()),
            ("state_cmd", self.state_cmd.as_deref()),
        ]
    }
}

/// The name the default power level is reported under, and so the one name a
/// named level may not take.
pub const DEFAULT_POWER_LEVEL: &str = "default";

impl PowerChannel {
    /// The hooks of `level`: the default hook set for `None`, a named level's
    /// for `Some`, or `None` when no level of that name exists.
    pub fn level(&self, level: Option<&str>) -> Option<PowerLevel> {
        match level {
            None => Some(PowerLevel {
                cycle_cmd: self.cycle_cmd.clone(),
                on_cmd: self.on_cmd.clone(),
                off_cmd: self.off_cmd.clone(),
                state_cmd: self.state_cmd.clone(),
            }),
            Some(name) => self.levels.get(name).cloned(),
        }
    }
}

/// Check one named power level: a valid, non-reserved name, at least one
/// hook, and no empty hook command. `what` prefixes the message. Unlike the
/// older names, a level name is checked on load too: levels are new, so no
/// existing lab can hold an odd one.
pub fn validate_power_level(what: &str, name: &str, l: &PowerLevel) -> Result<(), LabError> {
    validate_name("power level", name).map_err(|e| LabError(format!("{what}: {}", e.0)))?;
    if name == DEFAULT_POWER_LEVEL {
        return lab_err(format!(
            "{what}: '{DEFAULT_POWER_LEVEL}' names the hooks set without --level; \
             pick another level name"
        ));
    }
    let hooks = l.hooks();
    if hooks.iter().all(|(_, c)| c.is_none()) {
        return lab_err(format!(
            "{what}: power level '{name}' sets no hook (cycle_cmd, on_cmd, off_cmd or state_cmd)"
        ));
    }
    for (key, cmd) in hooks {
        if cmd.is_some_and(|c| c.trim().is_empty()) {
            return lab_err(format!("{what}: power level '{name}': empty {key}"));
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct VideoChannel {
    pub device: Option<String>,
    pub host: Option<String>,
    /// Which OCR engine suits this target's screens: `"text"` for firmware,
    /// bootloaders and consoles, `"gui"` for desktops and graphical BIOS pages.
    ///
    /// This exists because on Linux the two need different engines and the
    /// choice cannot be inferred. Measured on a Pi 5 (evals/ocr): PP-OCRv6 via
    /// RapidOCR scores 0.083 token-recall error on GUI screens against
    /// Tesseract's 0.312, while Tesseract edges it on console text (0.019 vs
    /// 0.025 CER) at a third of the latency.
    ///
    /// Confidence cannot decide it at runtime: Apple Vision reports a constant
    /// value, `Windows.Media.Ocr` reports none, and Tesseract's GUI failure is
    /// *silent omission* — it is confident about the rows it did read, so a
    /// low-confidence fallback would never trigger. Hence a config field.
    ///
    /// Unset means the platform default (`visionocr` on macOS, `winocr` on
    /// Windows, `linuxocr` on Linux), which is right for everything except a
    /// Linux host looking at GUI screens.
    pub ocr_mode: Option<String>,
    /// How far (0-255) any single pixel's luma must move from the last
    /// accepted frame before hdmicap's frame hash changes, so smaller
    /// movements (capture noise) do not look like a changed screen to
    /// `video shot --changed-since` and friends. Unset means hdmicap's default
    /// (16); 0 means exact: any differing pixel changes the hash. Measured
    /// noise on an unchanged H.264 (network KVM) screen was at most 8 levels.
    pub change_threshold: Option<u8>,
    /// Fixed port for the hdmicap daemon, so a standing `ssh -L` (or
    /// `LocalForward`) reaches the dashboard across restarts. Unset or 0 =
    /// OS-assigned, which avoids collisions with stale tunnels.
    pub port: Option<u16>,
}

/// USB HID input injection: an opaque helper command (e.g. `hidrig -d <uart>`)
/// that `panio hid send` appends protocol arguments to — the device-specific
/// tool lives outside panio, like the power hooks.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HidChannel {
    pub cmd: Option<String>,
    pub host: Option<String>,
    /// Fixed port for the hid daemon (`<cmd> serve --port N`). Unset or 0 =
    /// OS-assigned.
    pub port: Option<u16>,
    /// Opt-in (#304): `panio doctor` also runs `<cmd> doctor --json`.
    #[serde(default)]
    pub doctor: bool,
}

/// adb transport to an Android target (DUT). The device is named by its
/// `adb -s <serial>` id (omit for the sole attached device); `adb` overrides
/// the binary. Like SSH this is a generic transport, not a device-specific
/// helper, so `panio adb …` shells out to it directly on the bound host.
/// A shared USB device that a KVM can route to either side — the Openterface
/// KVM-Go's onboard microSD reader, or the Mini-KVM's switchable USB-A port.
/// Like `hid`, the device-specific tool lives outside panio in an opaque
/// helper command; unlike `hid send`, panio appends a *fixed* verb rather
/// than passing arguments through, so a remote host only ever sees
/// `usb host`, `usb target`, or `usb state`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct UsbChannel {
    pub cmd: Option<String>,
    pub host: Option<String>,
    /// Opt-in (#304): `panio doctor` also runs `<cmd> doctor --json`.
    #[serde(default)]
    pub doctor: bool,
}

/// One named target action (#299): an opaque hook command for a control
/// that is not power — a reset line, a boot strap, a recovery button combo.
/// Run with the same contract as the power hooks.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Action {
    pub cmd: String,
    pub description: Option<String>,
    /// `"on"` or `"off"`: refuse to run unless `power-state` says so, so a
    /// strap change is never applied to a running board.
    pub requires_power: Option<String>,
}

/// A target's named actions. `host` binds them all to one control host, like
/// any channel; every other key in the table is an action name.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ActionsChannel {
    pub host: Option<String>,
    /// Opt-in (#304): `panio doctor` also runs each action program's own
    /// read-only `doctor --json` check. A table-level key like `host`, so
    /// `doctor` cannot name an action ([`ACTIONS_TABLE_KEYS`]).
    #[serde(default)]
    pub doctor: bool,
    #[serde(flatten)]
    pub actions: BTreeMap<String, Action>,
}

/// Values `requires_power` accepts.
pub const VALID_REQUIRES_POWER: &[&str] = &["on", "off"];

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AdbChannel {
    pub serial: Option<String>,
    pub adb: Option<String>,
    pub host: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Target {
    pub host: Option<String>,
    /// Free-text description of this target, shown by `target show` /
    /// `config show`. The legacy key `note` is accepted as an alias so older
    /// lab files keep parsing.
    #[serde(alias = "note")]
    pub description: Option<String>,
    pub netboot: Option<NetbootChannel>,
    #[serde(default)]
    pub serial: Vec<SerialChannel>,
    pub power: Option<PowerChannel>,
    pub video: Option<VideoChannel>,
    pub hid: Option<HidChannel>,
    pub usb: Option<UsbChannel>,
    pub adb: Option<AdbChannel>,
    pub actions: Option<ActionsChannel>,
}

impl Target {
    pub fn default_host(&self) -> &str {
        self.host.as_deref().unwrap_or(LOCAL)
    }

    /// The serialcap daemon's fixed port: the first interface that sets a
    /// nonzero `port` ([`validate`] refuses interfaces that disagree), else 0
    /// (OS-assigned).
    pub fn serial_port(&self) -> u16 {
        self.serial
            .iter()
            .filter_map(|s| s.port)
            .find(|p| *p != 0)
            .unwrap_or(0)
    }
}

/// A channel's fixed daemon port, 0 (OS-assigned) when unset.
pub fn fixed_port(port: Option<u16>) -> u16 {
    port.unwrap_or(0)
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Lab {
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
}

// ── resolved (read) view ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Netboot,
    Serial,
    Power,
    Video,
    Hid,
    Usb,
    Adb,
    Actions,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Netboot => "netboot",
            ChannelKind::Serial => "serial",
            ChannelKind::Power => "power",
            ChannelKind::Video => "video",
            ChannelKind::Hid => "hid",
            ChannelKind::Usb => "usb",
            ChannelKind::Adb => "adb",
            ChannelKind::Actions => "actions",
        }
    }

    /// Every channel kind, in lab-file order.
    pub const ALL: [ChannelKind; 8] = [
        ChannelKind::Netboot,
        ChannelKind::Serial,
        ChannelKind::Power,
        ChannelKind::Video,
        ChannelKind::Hid,
        ChannelKind::Usb,
        ChannelKind::Adb,
        ChannelKind::Actions,
    ];

    /// The kind whose [`as_str`](Self::as_str) is `name`.
    pub fn from_name(name: &str) -> Option<ChannelKind> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }
}

/// One channel of a target with its physical host resolved.
#[derive(Debug, Clone)]
pub struct ResolvedChannel {
    pub kind: ChannelKind,
    /// Serial interface name, or the kind name for singleton channels.
    pub name: String,
    pub host: String,
    /// Remaining scalar config, in display order (host and name excluded).
    pub fields: Vec<(&'static str, String)>,
}

/// A target's channels with per-channel hosts resolved (no single-host rule).
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub name: String,
    pub default_host: String,
    pub description: Option<String>,
    pub channels: Vec<ResolvedChannel>,
}

impl ResolvedTarget {
    /// The distinct hosts this target's channels live on.
    pub fn hosts(&self) -> Vec<String> {
        let mut s: BTreeSet<String> = self.channels.iter().map(|c| c.host.clone()).collect();
        if s.is_empty() {
            s.insert(self.default_host.clone());
        }
        s.into_iter().collect()
    }
}

impl Lab {
    pub fn target_names(&self) -> Vec<&str> {
        self.targets.keys().map(String::as_str).collect()
    }

    /// Look up a control host by name. `local` (and any undeclared name, which
    /// validation forbids) resolves to a synthetic local host.
    pub fn host(&self, name: &str) -> Host {
        if let Some(h) = self.hosts.get(name) {
            return h.clone();
        }
        Host {
            ssh: LOCAL.to_string(),
            ..Default::default()
        }
    }

    /// Flatten a target to its channels with per-channel hosts resolved.
    pub fn resolved_target(&self, name: &str) -> Option<ResolvedTarget> {
        let t = self.targets.get(name)?;
        let default_host = t.default_host().to_string();
        let host_of = |h: &Option<String>| h.clone().unwrap_or_else(|| default_host.clone());
        let mut channels = Vec::new();

        if let Some(nb) = &t.netboot {
            let mut f = Vec::new();
            push_opt(&mut f, "interface", &nb.interface);
            // Always shown, and marked when it is the default: a link that
            // silently ran at the default while a comment claimed otherwise
            // is how two links on one host ended up sharing a /24.
            f.push((
                "host_ip",
                match &nb.host_ip {
                    Some(ip) => ip.clone(),
                    None => format!("{DEFAULT_HOST_IP} (default)"),
                },
            ));
            push_opt(&mut f, "tftp_root", &nb.tftp_root);
            push_opt(&mut f, "boot_file", &nb.boot_file);
            push_opt(&mut f, "http_port", &nb.http_port);
            push_opt(&mut f, "content_type", &nb.content_type);
            push_opt(&mut f, "link_down_cmd", &nb.link_down_cmd);
            push_opt(&mut f, "link_up_cmd", &nb.link_up_cmd);
            channels.push(ResolvedChannel {
                kind: ChannelKind::Netboot,
                name: "netboot".into(),
                host: host_of(&nb.host),
                fields: f,
            });
        }
        for s in &t.serial {
            let mut f = vec![("device", s.device.clone()), ("baud", s.baud.to_string())];
            push_opt(&mut f, "power_sense_signal", &s.power_sense_signal);
            if s.power_button {
                f.push(("power_button", "true".to_string()));
            }
            push_opt(&mut f, "port", &s.port.map(|n| n.to_string()));
            if !s.faults.is_empty() {
                let rules: Vec<String> =
                    s.faults.iter().map(|(n, p)| format!("{n}={p:?}")).collect();
                f.push(("faults", rules.join(", ")));
            }
            channels.push(ResolvedChannel {
                kind: ChannelKind::Serial,
                name: s.name.clone(),
                host: host_of(&s.host),
                fields: f,
            });
        }
        if let Some(p) = &t.power {
            let mut f = Vec::new();
            push_opt(&mut f, "cycle_cmd", &p.cycle_cmd);
            push_opt(&mut f, "on_cmd", &p.on_cmd);
            push_opt(&mut f, "off_cmd", &p.off_cmd);
            push_opt(&mut f, "state_cmd", &p.state_cmd);
            push_opt(&mut f, "serial_interface", &p.serial_interface);
            if !p.levels.is_empty() {
                let names: Vec<&str> = p.levels.keys().map(String::as_str).collect();
                f.push(("levels", names.join(", ")));
            }
            push_doctor(&mut f, p.doctor);
            channels.push(ResolvedChannel {
                kind: ChannelKind::Power,
                name: "power".into(),
                host: host_of(&p.host),
                fields: f,
            });
        }
        if let Some(v) = &t.video {
            let mut f = Vec::new();
            push_opt(&mut f, "device", &v.device);
            push_opt(&mut f, "ocr_mode", &v.ocr_mode);
            push_opt(
                &mut f,
                "change_threshold",
                &v.change_threshold.map(|n| n.to_string()),
            );
            push_opt(&mut f, "port", &v.port.map(|n| n.to_string()));
            channels.push(ResolvedChannel {
                kind: ChannelKind::Video,
                name: "video".into(),
                host: host_of(&v.host),
                fields: f,
            });
        }
        if let Some(h) = &t.hid {
            let mut f = Vec::new();
            push_opt(&mut f, "cmd", &h.cmd);
            push_opt(&mut f, "port", &h.port.map(|n| n.to_string()));
            push_doctor(&mut f, h.doctor);
            channels.push(ResolvedChannel {
                kind: ChannelKind::Hid,
                name: "hid".into(),
                host: host_of(&h.host),
                fields: f,
            });
        }
        if let Some(u) = &t.usb {
            let mut f = Vec::new();
            push_opt(&mut f, "cmd", &u.cmd);
            push_doctor(&mut f, u.doctor);
            channels.push(ResolvedChannel {
                kind: ChannelKind::Usb,
                name: "usb".into(),
                host: host_of(&u.host),
                fields: f,
            });
        }
        if let Some(a) = &t.adb {
            let mut f = Vec::new();
            push_opt(&mut f, "serial", &a.serial);
            push_opt(&mut f, "adb", &a.adb);
            channels.push(ResolvedChannel {
                kind: ChannelKind::Adb,
                name: "adb".into(),
                host: host_of(&a.host),
                fields: f,
            });
        }
        if let Some(acts) = &t.actions {
            let names: Vec<&str> = acts.actions.keys().map(String::as_str).collect();
            channels.push(ResolvedChannel {
                kind: ChannelKind::Actions,
                name: "actions".into(),
                host: host_of(&acts.host),
                fields: {
                    let mut f = vec![("names", names.join(", "))];
                    push_doctor(&mut f, acts.doctor);
                    f
                },
            });
        }
        Some(ResolvedTarget {
            name: name.to_string(),
            default_host,
            description: t.description.clone(),
            channels,
        })
    }

    /// Every (target, channel) pair whose channel resolves to `host`.
    pub fn channels_on_host(&self, host: &str) -> Vec<(String, ResolvedChannel)> {
        let mut out = Vec::new();
        for name in self.targets.keys() {
            if let Some(rt) = self.resolved_target(name) {
                for ch in rt.channels {
                    if ch.host == host {
                        out.push((name.clone(), ch));
                    }
                }
            }
        }
        out
    }
}

/// Show `doctor = true` in the resolved view; the default (off) is not shown.
fn push_doctor(fields: &mut Vec<(&'static str, String)>, on: bool) {
    if on {
        fields.push(("doctor", "true".to_string()));
    }
}

/// Keys of a target's `actions` table that configure the table itself rather
/// than name an action.
pub const ACTIONS_TABLE_KEYS: &[&str] = &["host", "doctor"];

fn push_opt(fields: &mut Vec<(&'static str, String)>, key: &'static str, v: &Option<String>) {
    if let Some(val) = v {
        fields.push((key, val.clone()));
    }
}

/// Resolve the host a command should run on, given the channel it touches.
///
/// Singleton kinds use that channel's host (else the target default). Serial
/// with a name uses that interface's host — a name the target does not have
/// is an error here, before anything is dispatched, rather than a silent
/// fall-through to the default host (where `console -i typo` used to open a
/// dashboard on the wrong interface); serial without a name uses the common
/// host of all interfaces, erroring if they span hosts (the `serial watch`
/// case, where the daemon owns every interface). A missing channel *kind*
/// falls back to the target's default host so the body can report it.
pub fn channel_host(
    rt: &ResolvedTarget,
    kind: ChannelKind,
    serial_name: Option<&str>,
) -> Result<String, crate::error::PanioError> {
    use crate::error::{channel_missing, Kind, PanioError};
    if kind == ChannelKind::Serial {
        let serials: Vec<&ResolvedChannel> = rt
            .channels
            .iter()
            .filter(|c| c.kind == ChannelKind::Serial)
            .collect();
        if let Some(n) = serial_name {
            return serials
                .iter()
                .find(|c| c.name == n)
                .map(|c| c.host.clone())
                .ok_or_else(|| {
                    let have: Vec<&str> = serials.iter().map(|c| c.name.as_str()).collect();
                    let msg = format!(
                        "target '{}' has no serial interface '{n}' (have: {})",
                        rt.name,
                        if have.is_empty() {
                            "none".to_string()
                        } else {
                            have.join(", ")
                        }
                    );
                    channel_missing(&rt.name, "serial", msg)
                });
        }
        if serials.is_empty() {
            return Ok(rt.default_host.clone());
        }
        let hosts: BTreeSet<&str> = serials.iter().map(|c| c.host.as_str()).collect();
        if hosts.len() > 1 {
            let list: Vec<&str> = hosts.into_iter().collect();
            // Answerable by the caller alone (name an interface): usage, the
            // same as an ambiguous target.
            return Err(PanioError::new(
                Kind::Usage,
                format!(
                    "target '{}' has serial interfaces on multiple hosts ({}); \
                     specify one with --interface",
                    rt.name,
                    list.join(", ")
                ),
            )
            .target(&rt.name)
            .channel("serial"));
        }
        return Ok(serials[0].host.clone());
    }
    for c in &rt.channels {
        if c.kind == kind {
            return Ok(c.host.clone());
        }
    }
    Ok(rt.default_host.clone())
}

// ── validation (shared by load and the editor's save) ───────────────────────

fn check_host_ref(host: &str, declared: &BTreeSet<&str>, ctx: &str) -> Result<(), LabError> {
    if !declared.contains(host) {
        let mut known: Vec<&str> = declared.iter().copied().collect();
        known.sort_unstable();
        return lab_err(format!(
            "{ctx} references unknown host '{host}' (declared: {})",
            known.join(", ")
        ));
    }
    Ok(())
}

/// Raise [`LabError`] if `lab` is not a structurally valid lab.
/// `HOST:PORT` with a nonempty host and a nonzero numeric port.
fn network_addr_ok(addr: &str) -> bool {
    match addr.trim_end_matches('/').rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0),
        None => false,
    }
}

pub fn validate(lab: &Lab) -> Result<(), LabError> {
    let mut declared: BTreeSet<&str> = lab.hosts.keys().map(String::as_str).collect();
    declared.insert(LOCAL);
    for (name, h) in &lab.hosts {
        if name.is_empty() {
            return lab_err("a host has an empty name");
        }
        if h.ssh.trim().is_empty() {
            return lab_err(format!("host '{name}': missing required 'ssh' field"));
        }
        // `local` is the sentinel for this machine and resolves as local
        // whatever its `ssh` says; a declared [hosts.local] with a real
        // destination would silently run its channels here instead.
        if name == LOCAL && h.ssh != LOCAL {
            return lab_err(format!(
                "host '{LOCAL}' means this machine and cannot have ssh = \"{}\"; \
                 give the remote host another name",
                h.ssh
            ));
        }
        // These become ssh arguments; one starting with `-` would be read as
        // an option (`-oProxyCommand=…`) rather than a destination or path.
        for (field, value) in [
            ("ssh", Some(h.ssh.as_str())),
            ("identity", h.identity.as_deref()),
            ("control_path", h.control_path.as_deref()),
        ] {
            if value.is_some_and(|v| v.starts_with('-')) {
                return lab_err(format!(
                    "host '{name}': {field} '{}' must not begin with '-'",
                    value.unwrap_or_default()
                ));
            }
        }
    }
    for (name, t) in &lab.targets {
        if name.is_empty() {
            return lab_err("a target has an empty name");
        }
        let default_host = t.default_host();
        check_host_ref(default_host, &declared, &format!("target '{name}'"))?;
        if let Some(nb) = &t.netboot {
            let h = nb.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' netboot"))?;
            if let Some(ip) = &nb.host_ip {
                if ip.parse::<std::net::Ipv4Addr>().is_err() {
                    return lab_err(format!(
                        "target '{name}' netboot: host_ip '{ip}' is not an IPv4 address"
                    ));
                }
            }
            if let Some(port) = &nb.http_port {
                if !matches!(port.parse::<u16>(), Ok(p) if p != 0) {
                    return lab_err(format!(
                        "target '{name}' netboot: http_port '{port}' is not a port number (1-65535)"
                    ));
                }
            }
        }
        if let Some(p) = &t.power {
            let h = p.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' power"))?;
            for (ln, l) in &p.levels {
                validate_power_level(&format!("target '{name}' power"), ln, l)?;
            }
        }
        if let Some(v) = &t.video {
            let h = v.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' video"))?;
            if let Some(dev) = v.device.as_deref().filter(|d| is_network_device(d)) {
                if dev == RFB_HID_DEVICE {
                    let video_host = v.host.as_deref().unwrap_or(default_host);
                    match &t.hid {
                        None => {
                            return lab_err(format!(
                                "target '{name}' video: device '{RFB_HID_DEVICE}' reads the hid \
                                 daemon's RFB feed, but the target has no hid channel \
                                 (panio hid set -t {name} --cmd ...)"
                            ))
                        }
                        Some(hid) if hid.host.as_deref().unwrap_or(default_host) != video_host => {
                            return lab_err(format!(
                                "target '{name}' video: device '{RFB_HID_DEVICE}' needs the video \
                                 and hid channels on the same host (video on '{video_host}', hid on '{}')",
                                hid.host.as_deref().unwrap_or(default_host)
                            ))
                        }
                        Some(_) => {}
                    }
                } else if !dev.starts_with("rfb://") || !network_addr_ok(&dev["rfb://".len()..]) {
                    return lab_err(format!(
                        "target '{name}' video: unsupported network device '{dev}' \
                         (valid: '{RFB_HID_DEVICE}' or 'rfb://HOST:PORT')"
                    ));
                }
            }
            if let Some(mode) = &v.ocr_mode {
                if !VALID_OCR_MODES.contains(&mode.as_str()) {
                    return lab_err(format!(
                        "target '{name}' video: invalid ocr_mode '{mode}' (valid: {})",
                        VALID_OCR_MODES.join(", ")
                    ));
                }
            }
        }
        if let Some(hid) = &t.hid {
            let h = hid.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' hid"))?;
        }
        if let Some(adb) = &t.adb {
            let h = adb.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' adb"))?;
        }
        if let Some(usb) = &t.usb {
            let h = usb.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' usb"))?;
        }
        if let Some(acts) = &t.actions {
            let h = acts.host.as_deref().unwrap_or(default_host);
            check_host_ref(h, &declared, &format!("target '{name}' actions"))?;
            for (an, a) in &acts.actions {
                if a.cmd.trim().is_empty() {
                    return lab_err(format!("target '{name}' action '{an}': empty cmd"));
                }
                if let Some(rp) = &a.requires_power {
                    if !VALID_REQUIRES_POWER.contains(&rp.as_str()) {
                        return lab_err(format!(
                            "target '{name}' action '{an}': invalid requires_power '{rp}' \
                             (valid: {})",
                            VALID_REQUIRES_POWER.join(", ")
                        ));
                    }
                }
            }
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for s in &t.serial {
            if s.name.is_empty() || s.device.is_empty() {
                return lab_err(format!(
                    "target '{name}': each [[serial]] needs name + device"
                ));
            }
            if !seen.insert(s.name.as_str()) {
                return lab_err(format!(
                    "target '{name}': duplicate serial name '{}'",
                    s.name
                ));
            }
            if let Some(sense) = &s.power_sense_signal {
                if !VALID_SENSE_SIGNALS.contains(&sense.as_str()) {
                    return lab_err(format!(
                        "target '{name}' serial '{}': invalid power_sense_signal '{sense}' \
                         (valid: {})",
                        s.name,
                        VALID_SENSE_SIGNALS.join(", ")
                    ));
                }
            }
            if s.faults.len() > MAX_FAULT_RULES {
                return lab_err(format!(
                    "target '{name}' serial '{}': {} fault rules (at most {MAX_FAULT_RULES})",
                    s.name,
                    s.faults.len()
                ));
            }
            for (rule, pattern) in &s.faults {
                validate_fault_rule(rule, pattern).map_err(|e| {
                    LabError(format!("target '{name}' serial '{}': {}", s.name, e.0))
                })?;
            }
            let h = s.host.as_deref().unwrap_or(default_host);
            check_host_ref(
                h,
                &declared,
                &format!("target '{name}' serial '{}'", s.name),
            )?;
        }
        let serial_ports: BTreeSet<u16> = t
            .serial
            .iter()
            .filter_map(|s| s.port)
            .filter(|p| *p != 0)
            .collect();
        if serial_ports.len() > 1 {
            return lab_err(format!(
                "target '{name}': serial interfaces name different ports {serial_ports:?}, \
                 but one serialcap daemon serves them all; set one port"
            ));
        }
    }
    check_fixed_ports(lab)
}

/// Two daemons on one host cannot listen on the same fixed port: refuse a lab
/// where the video, serial or hid ports of any targets collide on a host.
fn check_fixed_ports(lab: &Lab) -> Result<(), LabError> {
    let mut claimed: BTreeMap<(String, u16), String> = BTreeMap::new();
    for (name, t) in &lab.targets {
        let dh = t.default_host();
        let mut claims: Vec<(&str, &str, u16)> = Vec::new();
        if let Some(v) = &t.video {
            claims.push(("video", v.host.as_deref().unwrap_or(dh), fixed_port(v.port)));
        }
        if let Some(h) = &t.hid {
            claims.push(("hid", h.host.as_deref().unwrap_or(dh), fixed_port(h.port)));
        }
        if let Some(s) = t.serial.iter().find(|s| s.port.is_some_and(|p| p != 0)) {
            claims.push(("serial", s.host.as_deref().unwrap_or(dh), t.serial_port()));
        }
        for (kind, host, port) in claims {
            if port == 0 {
                continue;
            }
            let who = format!("target '{name}' {kind}");
            if let Some(prev) = claimed.insert((host.to_string(), port), who.clone()) {
                return lab_err(format!(
                    "{who} and {prev} both use port {port} on host '{host}'"
                ));
            }
        }
    }
    Ok(())
}

/// The first other target whose netboot link shares `target`'s /24 on the
/// same control host, on a *different* interface.
///
/// Two targets time-sharing one adapter (the same interface) are fine: the
/// cable goes to one of them at a time and `netboot start` keeps one daemon
/// per interface. Two *interfaces* in one /24 are not: netbootd's own sockets
/// are pinned to their interface, but everything else that dials a target
/// address — ssh, AMT, ffx, fastboot — asks the routing table, which cannot
/// tell two identical /24s apart. The effective host IP counts, so a second
/// link left at the default clashes with the first. `None` when either link
/// has no interface yet, or the host IP is not IPv4 ([`validate`] covers that).
pub fn netboot_subnet_clash(lab: &Lab, target: &str) -> Option<SubnetClash> {
    let t = lab.targets.get(target)?;
    let nb = t.netboot.as_ref()?;
    let interface = nb.interface.as_deref()?;
    let host = nb.host.as_deref().unwrap_or(t.default_host());
    let subnet = netboot_subnet(nb.effective_host_ip())?;
    lab.targets.iter().find_map(|(other_name, other)| {
        if other_name == target {
            return None;
        }
        let onb = other.netboot.as_ref()?;
        let oiface = onb.interface.as_deref()?;
        let ohost = onb.host.as_deref().unwrap_or(other.default_host());
        if ohost != host || oiface == interface {
            return None;
        }
        if netboot_subnet(onb.effective_host_ip())? != subnet {
            return None;
        }
        Some(SubnetClash {
            host: host.to_string(),
            subnet: subnet_str(subnet),
            interface: interface.to_string(),
            host_ip: nb.effective_host_ip().to_string(),
            defaulted: nb.host_ip.is_none(),
            other_target: other_name.clone(),
            other_interface: oiface.to_string(),
        })
    })
}

/// The refusal `netboot set` gives for a [`SubnetClash`] on the link it just
/// edited: what is where, and the one flag that fixes it.
pub fn subnet_clash_message(target: &str, c: &SubnetClash) -> String {
    let how = if c.defaulted {
        format!("host_ip {} (the default, since none is set)", c.host_ip)
    } else {
        format!("host_ip {}", c.host_ip)
    };
    format!(
        "target '{target}' netboot: {how} puts {} in {}, which target '{}' already uses on \
         {} of host '{}'. Every netboot link on a host needs its own /24, or every route to \
         a target on either link is ambiguous — give this one a different subnet: \
         `panio netboot set -t {target} --host-ip <addr>`",
        c.interface, c.subnet, c.other_target, c.other_interface, c.host
    )
}

/// [`validate`], plus the cross-references a *write* must not leave dangling:
/// a `power.serial_interface` has to name one of the target's own serial
/// interfaces. Applied by the editor before every save and not on load, so a
/// lab that already carries a stale reference still loads (`doctor` reports
/// it) — but no CLI edit may create one, or remove the interface it names.
///
/// A shared netboot /24 ([`netboot_subnet_clash`]) is deliberately *not*
/// checked here but in `LabFile::set_netboot`, against the one link being
/// edited: a lab with two independent clashing pairs could otherwise never be
/// repaired through the CLI, because each single edit still leaves the other
/// pair clashing and every save would be refused.
pub fn validate_for_save(lab: &Lab) -> Result<(), LabError> {
    validate(lab)?;
    for (name, t) in &lab.targets {
        let Some(si) = t.power.as_ref().and_then(|p| p.serial_interface.as_deref()) else {
            continue;
        };
        if !t.serial.iter().any(|s| s.name == si) {
            let have: Vec<&str> = t.serial.iter().map(|s| s.name.as_str()).collect();
            return lab_err(format!(
                "target '{name}': power.serial_interface '{si}' names no serial interface \
                 of this target (have: {}); point it at one with \
                 `panio power set -t {name} --serial-interface <name>`, or remove the \
                 power channel first",
                if have.is_empty() {
                    "none".to_string()
                } else {
                    have.join(", ")
                }
            ));
        }
    }
    Ok(())
}

// ── parsing & path discovery ────────────────────────────────────────────────

/// Parse and validate a lab from TOML text.
pub fn parse(text: &str) -> Result<Lab, LabError> {
    let lab: Lab = toml::from_str(text).map_err(|e| LabError(e.to_string()))?;
    validate(&lab)?;
    Ok(lab)
}

/// Read and validate the lab at `path`.
pub fn load(path: &Path) -> Result<Lab, LabError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| LabError(format!("{}: {e}", path.display())))?;
    parse(&text)
}

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// The lab file used when neither `--lab` nor `$PANIO_LAB` is given.
pub fn default_lab_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/panio/lab.toml")
}

/// Resolve the active lab path: `--lab`, then `$PANIO_LAB`, then the default
/// path if it exists. Returns None when none resolve.
pub fn resolve_lab_path(flag: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(expand_tilde(p));
    }
    if let Ok(p) = std::env::var("PANIO_LAB") {
        if !p.is_empty() {
            return Some(expand_tilde(&p));
        }
    }
    let d = default_lab_path();
    if d.exists() {
        Some(d)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_local_via_sentinels() {
        let by_ssh = Host {
            ssh: "local".into(),
            ..Default::default()
        };
        assert!(by_ssh.is_local("anything"));
        let by_name = Host {
            ssh: "u@remote".into(),
            ..Default::default()
        };
        assert!(by_name.is_local("local"));
    }

    #[test]
    fn is_local_via_hostname_fqdn() {
        // A non-matching FQDN is remote.
        let remote = Host {
            ssh: "u@x".into(),
            hostname: Some("nope.invalid.example".into()),
            ..Default::default()
        };
        assert!(!remote.is_local("dev"));
        // A host whose declared FQDN equals this machine's resolves as local,
        // case-insensitively. (Skipped when the FQDN can't be detected.)
        if let Some(fqdn) = local_fqdn() {
            let me = Host {
                ssh: "u@x".into(),
                hostname: Some(fqdn.to_ascii_uppercase()),
                ..Default::default()
            };
            assert!(me.is_local("dev"));
        }
    }

    #[test]
    fn description_accepts_legacy_note_alias() {
        // Canonical `description` and the legacy `note` key both land in
        // Target::description, and a host carries its own description.
        let lab = parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            description = "the bench Mac"
            [targets.canon]
            description = "Pi 5 DUT"
            [targets.legacy]
            note = "old-style note"
            "#,
        )
        .unwrap();
        assert_eq!(
            lab.hosts["bench1"].description.as_deref(),
            Some("the bench Mac")
        );
        assert_eq!(
            lab.targets["canon"].description.as_deref(),
            Some("Pi 5 DUT")
        );
        assert_eq!(
            lab.targets["legacy"].description.as_deref(),
            Some("old-style note"),
            "legacy `note` key parses into `description` via the serde alias"
        );
    }

    fn multihost() -> Lab {
        parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [hosts.bench2]
            ssh = "u@bench2"
            [targets.fortune]
            host = "bench1"
            description = "n"
            [targets.fortune.netboot]
            interface = "en0"
            [[targets.fortune.serial]]
            name = "console"
            device = "/dev/ttyUSB0"
            [targets.fortune.video]
            device = "/dev/video0"
            host = "bench2"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_per_channel_host() {
        let rt = multihost().resolved_target("fortune").unwrap();
        let by: BTreeMap<_, _> = rt
            .channels
            .iter()
            .map(|c| ((c.kind.as_str(), c.name.clone()), c.host.clone()))
            .collect();
        assert_eq!(by[&("netboot", "netboot".into())], "bench1");
        assert_eq!(by[&("serial", "console".into())], "bench1");
        assert_eq!(by[&("video", "video".into())], "bench2");
        assert_eq!(rt.hosts(), vec!["bench1", "bench2"]);
    }

    #[test]
    fn channels_on_host_is_the_inverse_index() {
        let lab = multihost();
        let on2 = lab.channels_on_host("bench2");
        assert_eq!(on2.len(), 1);
        assert_eq!(on2[0].0, "fortune");
        assert_eq!(on2[0].1.kind, ChannelKind::Video);
    }

    #[test]
    fn resolves_adb_channel() {
        let lab = parse(
            r#"
            [hosts.bench1]
            ssh = "u@bench1"
            [targets.pixel]
            host = "bench1"
            [targets.pixel.adb]
            serial = "39021FDH200xyz"
            "#,
        )
        .unwrap();
        let rt = lab.resolved_target("pixel").unwrap();
        let adb = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Adb)
            .expect("adb channel");
        assert_eq!(adb.host, "bench1");
        assert_eq!(
            adb.fields.iter().find(|(k, _)| *k == "serial").unwrap().1,
            "39021FDH200xyz"
        );
        // The device id is plumbed via channel_host like any singleton kind.
        assert_eq!(channel_host(&rt, ChannelKind::Adb, None).unwrap(), "bench1");
    }

    /// Power levels (#302): valid names, not the reserved `default`, at
    /// least one hook, no empty hook; the resolved view lists the names; and
    /// `level()` never mixes the default hooks into a named level.
    #[test]
    fn power_levels_validate_and_resolve() {
        let ok = parse(
            "[targets.t.power]\ncycle_cmd = \"soft\"\nstate_cmd = \"st\"\n\
             [targets.t.power.levels.hard]\ncycle_cmd = \"hard\"\n\
             [targets.t.power.levels.pmic]\non_cmd = \"pon\"\n",
        )
        .unwrap();
        let p = ok.targets["t"].power.as_ref().unwrap();
        assert_eq!(p.level(None).unwrap().cycle_cmd.as_deref(), Some("soft"));
        let hard = p.level(Some("hard")).unwrap();
        assert_eq!(hard.cycle_cmd.as_deref(), Some("hard"));
        assert_eq!(hard.state_cmd, None, "no fallback to the default state_cmd");
        assert!(p.level(Some("nope")).is_none());
        let rt = ok.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Power)
            .unwrap();
        assert!(ch.fields.contains(&("levels", "hard, pmic".to_string())));

        for (toml, want) in [
            (
                "[targets.t.power.levels.\"-x\"]\ncycle_cmd = \"x\"\n",
                "invalid power level name '-x'",
            ),
            (
                "[targets.t.power.levels.default]\ncycle_cmd = \"x\"\n",
                "'default' names the hooks set without --level",
            ),
            (
                "[targets.t.power.levels.hard]\n",
                "power level 'hard' sets no hook",
            ),
            (
                "[targets.t.power.levels.hard]\ncycle_cmd = \" \"\n",
                "power level 'hard': empty cycle_cmd",
            ),
        ] {
            let e = parse(toml).unwrap_err();
            assert!(e.0.contains(want), "{toml}: {}", e.0);
        }
    }

    /// Actions (#299): a known host, a non-empty cmd, and requires_power one
    /// of on/off. The resolved view lists them under one `actions` channel.
    #[test]
    fn actions_validate_and_resolve() {
        let ok = parse(
            "[targets.t.actions]\nreset = { cmd = \"boardctl reset\" }\n\
             boot-usb = { cmd = \"boardctl boot usb\", requires_power = \"off\" }\n",
        )
        .unwrap();
        let rt = ok.resolved_target("t").unwrap();
        let ch = rt
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Actions)
            .unwrap();
        assert_eq!(ch.fields, vec![("names", "boot-usb, reset".to_string())]);
        let bad_power =
            parse("[targets.t.actions]\nreset = { cmd = \"x\", requires_power = \"maybe\" }\n")
                .unwrap_err();
        assert!(
            bad_power.0.contains("invalid requires_power 'maybe'"),
            "{}",
            bad_power.0
        );
        let empty = parse("[targets.t.actions]\nreset = { cmd = \" \" }\n").unwrap_err();
        assert!(empty.0.contains("empty cmd"), "{}", empty.0);
        let ghost =
            parse("[targets.t.actions]\nhost = \"ghost\"\nreset = { cmd = \"x\" }\n").unwrap_err();
        assert!(
            ghost.0.contains("actions references unknown host 'ghost'"),
            "{}",
            ghost.0
        );
        assert_eq!(
            ChannelKind::from_name("actions"),
            Some(ChannelKind::Actions)
        );
    }

    #[test]
    fn validate_rejects_unknown_host() {
        let e = parse("[targets.t]\nhost = \"ghost\"\n").unwrap_err();
        assert!(e.0.contains("unknown host 'ghost'"), "{}", e.0);
    }

    #[test]
    fn validate_rejects_unknown_usb_host() {
        // Every channel's host reference is checked, the usb channel included:
        // an unknown name here used to pass validation and then silently
        // resolve to the local host at runtime.
        let e =
            parse("[targets.t.usb]\ncmd = \"ch9329 -d /dev/x\"\nhost = \"ghost\"\n").unwrap_err();
        assert!(e.0.contains("target 't' usb"), "{}", e.0);
        assert!(e.0.contains("unknown host 'ghost'"), "{}", e.0);
    }

    #[test]
    fn fixed_ports_parse_and_resolve() {
        let lab = parse(
            "[targets.t.video]\ndevice = \"cap\"\nport = 8723\n\
             [targets.t.hid]\ncmd = \"hidrig\"\nport = 8725\n\
             [[targets.t.serial]]\nname = \"a\"\ndevice = \"/da\"\n\
             [[targets.t.serial]]\nname = \"b\"\ndevice = \"/db\"\nport = 8724\n",
        )
        .unwrap();
        let t = &lab.targets["t"];
        assert_eq!(fixed_port(t.video.as_ref().unwrap().port), 8723);
        assert_eq!(fixed_port(t.hid.as_ref().unwrap().port), 8725);
        // Set on one interface, it is the daemon's port.
        assert_eq!(t.serial_port(), 8724);
        // Unset means OS-assigned.
        let lab = parse("[targets.u.video]\ndevice = \"cap\"\n").unwrap();
        assert_eq!(fixed_port(lab.targets["u"].video.as_ref().unwrap().port), 0);
        assert_eq!(lab.targets["u"].serial_port(), 0);
    }

    #[test]
    fn validate_rejects_serial_interfaces_with_different_ports() {
        let e = parse(
            "[[targets.t.serial]]\nname = \"a\"\ndevice = \"/da\"\nport = 1\n\
             [[targets.t.serial]]\nname = \"b\"\ndevice = \"/db\"\nport = 2\n",
        )
        .unwrap_err();
        assert!(e.0.contains("different ports"), "{}", e.0);
    }

    #[test]
    fn validate_checks_serial_fault_rules_and_config_show_lists_them() {
        let with = |faults: &str| {
            format!("[[targets.t.serial]]\nname = \"a\"\ndevice = \"/da\"\nfaults = {faults}\n")
        };
        let lab = parse(&with("{ oops = \"Oops:\", panic = \"Kernel panic\" }")).unwrap();
        let rt = lab.resolved_target("t").unwrap();
        let faults = rt.channels[0]
            .fields
            .iter()
            .find(|(k, _)| *k == "faults")
            .unwrap();
        assert_eq!(faults.1, "oops=\"Oops:\", panic=\"Kernel panic\"");
        let e = parse(&with("{ \"bad name\" = \"x\" }")).unwrap_err();
        assert!(e.0.contains("target 't' serial 'a'"), "{}", e.0);
        assert!(e.0.contains("invalid fault rule name"), "{}", e.0);
        let e = parse(&with("{ a = \"\" }")).unwrap_err();
        assert!(e.0.contains("empty"), "{}", e.0);
        let long = "x".repeat(MAX_FAULT_PATTERN + 1);
        let e = parse(&with(&format!("{{ a = \"{long}\" }}"))).unwrap_err();
        assert!(e.0.contains("at most 1024"), "{}", e.0);
        let many: Vec<String> = (0..=MAX_FAULT_RULES)
            .map(|i| format!("r{i} = \"x\""))
            .collect();
        let e = parse(&with(&format!("{{ {} }}", many.join(", ")))).unwrap_err();
        assert!(e.0.contains("at most 32"), "{}", e.0);
    }

    #[test]
    fn validate_rejects_a_port_claimed_twice_on_one_host() {
        // Two targets' daemons on the same host.
        let e = parse(
            "[targets.a.video]\ndevice = \"x\"\nport = 9000\n\
             [targets.b.hid]\ncmd = \"h\"\nport = 9000\n",
        )
        .unwrap_err();
        assert!(e.0.contains("both use port 9000"), "{}", e.0);
        // The same port on different hosts is fine.
        parse(
            "[hosts.h1]\nssh = \"h1\"\n[hosts.h2]\nssh = \"h2\"\n\
             [targets.a]\nhost = \"h1\"\n[targets.a.video]\ndevice = \"x\"\nport = 9000\n\
             [targets.b]\nhost = \"h2\"\n[targets.b.video]\ndevice = \"x\"\nport = 9000\n",
        )
        .unwrap();
    }

    #[test]
    fn validate_rejects_bad_sense() {
        let toml = "[targets.t]\n[[targets.t.serial]]\nname=\"c\"\ndevice=\"/d\"\npower_sense_signal=\"bogus\"\n";
        let e = parse(toml).unwrap_err();
        assert!(e.0.contains("invalid power_sense_signal"), "{}", e.0);
    }

    #[test]
    fn validate_rejects_missing_ssh() {
        // ssh is a required field, so this fails at deserialize time.
        assert!(parse("[hosts.bench1]\n").is_err());
    }

    /// `[hosts.local]` with a real destination would resolve as *this*
    /// machine (the name is the sentinel) while claiming to be remote.
    #[test]
    fn validate_rejects_a_local_host_with_a_remote_ssh() {
        let e = parse("[hosts.local]\nssh = \"u@bench1\"\n").unwrap_err();
        assert!(e.0.contains("host 'local' means this machine"), "{}", e.0);
        // The redundant-but-harmless spelling still parses.
        assert!(parse("[hosts.local]\nssh = \"local\"\n").is_ok());
    }

    #[test]
    fn validate_rejects_empty_host_and_target_names() {
        let e = parse("[hosts.\"\"]\nssh = \"u@b\"\n").unwrap_err();
        assert!(e.0.contains("host has an empty name"), "{}", e.0);
        let e = parse("[targets.\"\"]\n").unwrap_err();
        assert!(e.0.contains("target has an empty name"), "{}", e.0);
    }

    /// `ssh`, `identity` and `control_path` all end up as ssh arguments; a
    /// value starting with `-` would be parsed as an option there.
    #[test]
    fn validate_rejects_host_fields_that_look_like_options() {
        for (field, toml) in [
            ("ssh", "[hosts.b]\nssh = \"-oProxyCommand=x\"\n"),
            ("identity", "[hosts.b]\nssh = \"u@b\"\nidentity = \"-x\"\n"),
            (
                "control_path",
                "[hosts.b]\nssh = \"u@b\"\ncontrol_path = \"-x\"\n",
            ),
        ] {
            let e = parse(toml).unwrap_err();
            assert!(
                e.0.contains(&format!("{field} '-")) && e.0.contains("must not begin with '-'"),
                "{field}: {}",
                e.0
            );
        }
    }

    #[test]
    fn validate_rejects_bad_host_ip_and_http_port() {
        let e = parse("[targets.t]\n[targets.t.netboot]\nhost_ip = \"192.168.99\"\n").unwrap_err();
        assert!(
            e.0.contains("host_ip '192.168.99' is not an IPv4"),
            "{}",
            e.0
        );
        let e = parse("[targets.t]\n[targets.t.netboot]\nhost_ip = \"fe80::1\"\n").unwrap_err();
        assert!(e.0.contains("not an IPv4"), "{}", e.0);
        for bad in ["http", "0", "65536", "-1"] {
            let e = parse(&format!(
                "[targets.t]\n[targets.t.netboot]\nhttp_port = \"{bad}\"\n"
            ))
            .unwrap_err();
            assert!(e.0.contains("is not a port number"), "{bad}: {}", e.0);
        }
        assert!(parse(
            "[targets.t]\n[targets.t.netboot]\nhost_ip = \"192.168.99.1\"\nhttp_port = \"8080\"\n"
        )
        .is_ok());
    }

    #[test]
    fn rfb_hid_video_device_needs_a_colocated_hid_channel() {
        let ok = "[targets.t]\n[targets.t.hid]\ncmd = \"jetkvm\"\n\
                  [targets.t.video]\ndevice = \"rfb+hid:\"\n";
        assert!(parse(ok).is_ok());
        let e = parse("[targets.t]\n[targets.t.video]\ndevice = \"rfb+hid:\"\n").unwrap_err();
        assert!(e.0.contains("no hid channel"), "{}", e.0);
        let split =
            "[hosts.a]\nssh = \"a.example\"\n[hosts.b]\nssh = \"b.example\"\n[targets.t]\n[targets.t.hid]\ncmd = \"jetkvm\"\nhost = \"a\"\n\
                     [targets.t.video]\ndevice = \"rfb+hid:\"\nhost = \"b\"\n";
        let e = parse(split).unwrap_err();
        assert!(e.0.contains("same host"), "{}", e.0);
    }

    #[test]
    fn network_video_devices_are_limited_to_the_supported_forms() {
        for ok in ["rfb://192.0.2.10:5900", "rfb://vm.example:5901/"] {
            let t = format!("[targets.t]\n[targets.t.video]\ndevice = \"{ok}\"\n");
            assert!(parse(&t).is_ok(), "{ok}");
        }
        for bad in [
            "rfb://host",
            "rfb://:5900",
            "rfb+ws://127.0.0.1:1/rfb",
            "rfb+discovery:",
        ] {
            let t = format!("[targets.t]\n[targets.t.video]\ndevice = \"{bad}\"\n");
            let e = parse(&t).unwrap_err();
            assert!(e.0.contains("unsupported network device"), "{bad}: {}", e.0);
        }
        // A capture device that merely starts with the letters is still one.
        assert!(parse("[targets.t]\n[targets.t.video]\ndevice = \"rfbcam\"\n").is_ok());
    }

    #[test]
    fn validate_rejects_unknown_ocr_mode() {
        let e = parse("[targets.t]\n[targets.t.video]\nocr_mode = \"fast\"\n").unwrap_err();
        assert!(e.0.contains("invalid ocr_mode 'fast'"), "{}", e.0);
        for ok in VALID_OCR_MODES {
            assert!(parse(&format!(
                "[targets.t]\n[targets.t.video]\nocr_mode = \"{ok}\"\n"
            ))
            .is_ok());
        }
    }

    /// A dangling `power.serial_interface` still *loads* (so a lab edited by
    /// hand can be repaired with the CLI) but must not be *saved*.
    #[test]
    fn validate_for_save_rejects_a_dangling_serial_interface() {
        let dangling = parse(
            "[targets.t]\n[targets.t.power]\nserial_interface = \"console\"\n\
             [[targets.t.serial]]\nname = \"other\"\ndevice = \"/dev/a\"\n",
        )
        .unwrap();
        let e = validate_for_save(&dangling).unwrap_err();
        assert!(
            e.0.contains("power.serial_interface 'console' names no serial interface"),
            "{}",
            e.0
        );
        assert!(e.0.contains("have: other"), "{}", e.0);
        let fine = parse(
            "[targets.t]\n[targets.t.power]\nserial_interface = \"console\"\n\
             [[targets.t.serial]]\nname = \"console\"\ndevice = \"/dev/a\"\n",
        )
        .unwrap();
        validate_for_save(&fine).unwrap();
    }

    /// Two netboot links on one host: `a` explicit at the default address,
    /// `b` with no host_ip at all (so at the default by omission).
    fn two_links(b_extra: &str) -> Lab {
        parse(&format!(
            "[hosts.bench1]\nssh = \"u@bench1\"\n\
             [targets.a]\nhost = \"bench1\"\n\
             [targets.a.netboot]\ninterface = \"eth1\"\nhost_ip = \"192.168.99.1\"\n\
             [targets.b]\nhost = \"bench1\"\n\
             [targets.b.netboot]\ninterface = \"eth2\"\n{b_extra}"
        ))
        .unwrap()
    }

    #[test]
    fn netboot_subnet_clash_finds_two_interfaces_in_one_slash_24() {
        let lab = two_links("");
        let c = netboot_subnet_clash(&lab, "b").unwrap();
        assert_eq!(c.host, "bench1");
        assert_eq!(c.subnet, "192.168.99.0/24");
        assert_eq!(
            (c.interface.as_str(), c.host_ip.as_str()),
            ("eth2", DEFAULT_HOST_IP)
        );
        assert!(c.defaulted, "b has no host_ip, so it is at the default");
        assert_eq!(
            (c.other_target.as_str(), c.other_interface.as_str()),
            ("a", "eth1")
        );
        // Seen from the other side, the same clash — with the explicit address.
        let c = netboot_subnet_clash(&lab, "a").unwrap();
        assert!(!c.defaulted);
        assert_eq!(c.other_target, "b");
        // A different /24 on the second link, and there is no clash.
        assert!(netboot_subnet_clash(&two_links("host_ip = \"192.168.100.1\"\n"), "b").is_none());
        // The same address on the *same* interface is a time-shared adapter,
        // not a clash; `netboot start` keeps one daemon per interface.
        let shared = parse(
            "[targets.a]\n[targets.a.netboot]\ninterface = \"eth1\"\n\
             [targets.b]\n[targets.b.netboot]\ninterface = \"eth1\"\n",
        )
        .unwrap();
        assert!(netboot_subnet_clash(&shared, "a").is_none());
        // Different hosts never clash, whatever their addresses.
        let apart = parse(
            "[hosts.bench1]\nssh = \"u@bench1\"\n\
             [targets.a]\n[targets.a.netboot]\ninterface = \"eth1\"\n\
             [targets.b]\nhost = \"bench1\"\n[targets.b.netboot]\ninterface = \"eth2\"\n",
        )
        .unwrap();
        assert!(netboot_subnet_clash(&apart, "a").is_none());
        // A channel with no interface yet cannot be placed, so it is not judged.
        let bare = parse(
            "[targets.a]\n[targets.a.netboot]\ninterface = \"eth1\"\n\
             [targets.b]\n[targets.b.netboot]\ntftp_root = \"/x\"\n",
        )
        .unwrap();
        assert!(netboot_subnet_clash(&bare, "a").is_none());
        assert!(netboot_subnet_clash(&bare, "b").is_none());
    }

    /// A shared /24 still *loads* and still *saves* (a lab with two clashing
    /// pairs must stay repairable one edit at a time); the refusal lives in
    /// `LabFile::set_netboot`, and its message says which link is at the
    /// default by omission.
    #[test]
    fn a_shared_slash_24_loads_and_saves_and_the_message_names_the_fix() {
        let clash = two_links("");
        validate_for_save(&clash).unwrap();
        let e = subnet_clash_message("b", &netboot_subnet_clash(&clash, "b").unwrap());
        assert!(
            e.contains(
                "target 'b' netboot: host_ip 192.168.99.1 (the default, since none is set) \
                 puts eth2 in 192.168.99.0/24"
            ),
            "{e}"
        );
        assert!(
            e.contains("target 'a' already uses on eth1 of host 'bench1'"),
            "{e}"
        );
        assert!(e.contains("panio netboot set -t b --host-ip"), "{e}");
        let e = subnet_clash_message("a", &netboot_subnet_clash(&clash, "a").unwrap());
        assert!(
            e.contains("target 'a' netboot: host_ip 192.168.99.1 puts eth1"),
            "{e}"
        );
    }

    /// `target show` must never hide the address a link will run at.
    #[test]
    fn netboot_host_ip_is_always_shown_and_marked_when_defaulted() {
        let lab = two_links("");
        let field = |t: &str| {
            lab.resolved_target(t)
                .unwrap()
                .channels
                .into_iter()
                .find(|c| c.kind == ChannelKind::Netboot)
                .unwrap()
                .fields
                .into_iter()
                .find(|(k, _)| *k == "host_ip")
                .map(|(_, v)| v)
        };
        assert_eq!(field("a").as_deref(), Some("192.168.99.1"));
        assert_eq!(field("b").as_deref(), Some("192.168.99.1 (default)"));
    }

    #[test]
    fn netboot_subnet_is_the_first_three_octets() {
        assert_eq!(netboot_subnet("192.168.99.1"), Some([192, 168, 99]));
        assert_eq!(subnet_str([10, 20, 30]), "10.20.30.0/24");
        assert_eq!(netboot_subnet("fe80::1"), None);
        assert_eq!(netboot_subnet("192.168.99"), None);
    }

    /// Naming an interface the target does not have is an error at
    /// resolution, not a fall-through to the default host.
    #[test]
    fn channel_host_rejects_an_unknown_serial_interface() {
        let rt = multihost().resolved_target("fortune").unwrap();
        assert_eq!(
            channel_host(&rt, ChannelKind::Serial, Some("console")).unwrap(),
            "bench1"
        );
        let e = channel_host(&rt, ChannelKind::Serial, Some("typo")).unwrap_err();
        assert!(
            e.message
                .contains("no serial interface 'typo' (have: console)"),
            "{}",
            e.message
        );
        assert_eq!(e.kind, crate::error::Kind::NotConfigured);
        assert_eq!(e.target.as_deref(), Some("fortune"));
        // No serial at all, none asked for by name: still the default host,
        // so the command body can say what is missing.
        let lab = parse("[targets.bare]\n").unwrap();
        let rt = lab.resolved_target("bare").unwrap();
        assert_eq!(channel_host(&rt, ChannelKind::Serial, None).unwrap(), LOCAL);
    }

    #[test]
    fn name_rule_accepts_plain_names_and_rejects_the_rest() {
        for ok in ["pi5", "lab-nuc-1", "bench_2", "v1.2", "a"] {
            assert!(is_valid_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "-x", "a b", "a/b", "a\tb", "é", "a:b", "x\0"] {
            assert!(!is_valid_name(bad), "{bad:?}");
        }
        let e = validate_name("target", "a b").unwrap_err();
        assert!(e.0.starts_with("invalid target name 'a b':"), "{}", e.0);
    }
}
