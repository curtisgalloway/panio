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

//! adb transport for an Android target (DUT).
//!
//! The `[targets.X.adb]` channel names a device (`adb -s <serial>`) and the
//! control host it is plugged into; panio shells out to that host's `adb`
//! binary for console (`adb shell`), screen capture (`adb exec-out
//! screencap`), and input injection (`adb shell input`). adb is a generic
//! transport like SSH — not a device-specific helper — so it lives in core
//! rather than a libexec helper. Reaching the host (local vs SSH) is the
//! existing per-channel dispatch; this module is only the local `adb` exec.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

pub const DEFAULT_ADB: &str = "adb";

/// Full argv for an adb invocation: the binary, an optional `-s <serial>`
/// device selector, then `rest`.
pub fn argv(adb_cmd: Option<&str>, serial: Option<&str>, rest: &[String]) -> Vec<String> {
    let mut a = vec![adb_cmd.unwrap_or(DEFAULT_ADB).to_string()];
    if let Some(s) = serial {
        a.push("-s".into());
        a.push(s.to_string());
    }
    a.extend(rest.iter().cloned());
    a
}

/// Friendlier error when the adb binary itself can't be run.
fn spawn_err(adb_cmd: Option<&str>, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        crate::error::PanioError::not_configured(format!(
            "'{}' not found — install the Android platform-tools (adb) on this host",
            adb_cmd.unwrap_or(DEFAULT_ADB)
        ))
        .channel("adb")
        .into()
    } else {
        anyhow!("failed to run adb: {e}")
    }
}

fn command(av: &[String]) -> Command {
    let mut c = Command::new(&av[0]);
    c.args(&av[1..]);
    c
}

/// Replace this process with an interactive `adb shell` (never returns on
/// success). The console analog of `serial connect`.
pub fn exec_shell(adb_cmd: Option<&str>, serial: Option<&str>) -> Result<()> {
    let av = argv(adb_cmd, serial, &["shell".to_string()]);
    let err = crate::platform::exec_replace(&mut command(&av));
    Err(spawn_err(adb_cmd, err))
}

/// Run `adb [-s …] <rest…>` with this process's stdio inherited; return the
/// child's exit code. Backs `run` (`shell <cmd>`), `input`, and `devices`.
pub fn run_passthrough(
    adb_cmd: Option<&str>,
    serial: Option<&str>,
    rest: &[String],
) -> Result<i32> {
    let av = argv(adb_cmd, serial, rest);
    let status = command(&av).status().map_err(|e| spawn_err(adb_cmd, e))?;
    Ok(crate::error::shell_code(status))
}

/// Capture one PNG via `adb exec-out screencap -p` and write it to `out`
/// ("-" = stdout). `exec-out` is binary-clean (no CRLF mangling that the old
/// `adb shell screencap` path suffered).
pub fn screencap(adb_cmd: Option<&str>, serial: Option<&str>, out: &str) -> Result<()> {
    let rest = ["exec-out", "screencap", "-p"]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let av = argv(adb_cmd, serial, &rest);
    let output = command(&av)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| spawn_err(adb_cmd, e))?;
    if !output.status.success() {
        bail!(
            "adb screencap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if out == "-" {
        std::io::stdout().write_all(&output.stdout)?;
    } else {
        std::fs::write(out, &output.stdout)
            .with_context(|| format!("writing screenshot to {out}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argv_inserts_serial_selector() {
        assert_eq!(
            argv(None, Some("ABC123"), &rest(&["shell"])),
            vec!["adb", "-s", "ABC123", "shell"]
        );
    }

    #[test]
    fn argv_omits_selector_for_sole_device() {
        assert_eq!(
            argv(None, None, &rest(&["devices", "-l"])),
            vec!["adb", "devices", "-l"]
        );
    }

    #[test]
    fn argv_honors_custom_binary() {
        assert_eq!(
            argv(Some("/opt/platform-tools/adb"), None, &rest(&["get-state"])),
            vec!["/opt/platform-tools/adb", "get-state"]
        );
    }
}
