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

//! The real implementations of the `logic` traits: the platform shell, the
//! wall clock, and a `nusb` probe of the control host's USB bus.

use std::io;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nusb::MaybeFuture;

use crate::logic::{describe_code, Captured, Clock, Probe, Runner};

/// A shell command line for the platform (`sh -c` / `cmd /C`).
pub fn shell_command(cmd: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}

pub struct ShellRunner;

impl Runner for ShellRunner {
    fn button(&mut self, cmd: &str) -> Result<()> {
        // stdout is reserved for the side this helper prints, so the
        // command's stdout is sent to our stderr.
        let status = shell_command(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(io::stderr()))
            .status()
            .with_context(|| format!("cannot start {cmd:?}"))?;
        if !status.success() {
            bail!("{cmd:?} failed ({})", describe_code(status.code()));
        }
        Ok(())
    }

    fn capture(&mut self, cmd: &str) -> Result<Captured> {
        let out = shell_command(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("cannot start {cmd:?}"))?;
        Ok(Captured {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }
}

pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> Self {
        SystemClock(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&mut self, d: Duration) {
        thread::sleep(d);
    }
}

/// `VID:PID[:SERIAL]`, with hex VID and PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
}

impl std::str::FromStr for DeviceSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let mut parts = s.splitn(3, ':');
        let hex = |p: Option<&str>, what: &str| -> Result<u16, String> {
            let p = p.filter(|p| !p.is_empty()).ok_or_else(|| {
                format!("expected VID:PID[:SERIAL] in hex (like 046d:c08b), missing {what}")
            })?;
            u16::from_str_radix(p, 16).map_err(|_| format!("{what} {p:?} is not a hex number"))
        };
        let vid = hex(parts.next(), "VID")?;
        let pid = hex(parts.next(), "PID")?;
        let serial = parts.next().filter(|s| !s.is_empty()).map(str::to_string);
        Ok(DeviceSpec { vid, pid, serial })
    }
}

impl DeviceSpec {
    pub fn matches(&self, vid: u16, pid: u16, serial: Option<&str>) -> bool {
        vid == self.vid
            && pid == self.pid
            && self.serial.as_deref().is_none_or(|s| Some(s) == serial)
    }
}

impl std::fmt::Display for DeviceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vid, self.pid)?;
        if let Some(s) = &self.serial {
            write!(f, ":{s}")?;
        }
        Ok(())
    }
}

/// Enumerates the USB bus; never opens a device.
pub struct UsbProbe {
    pub spec: DeviceSpec,
}

impl Probe for UsbProbe {
    fn present(&mut self) -> Result<bool> {
        let mut devices = nusb::list_devices()
            .wait()
            .map_err(|e| anyhow!("cannot list USB devices: {e}"))?;
        Ok(devices.any(|d| {
            self.spec
                .matches(d.vendor_id(), d.product_id(), d.serial_number())
        }))
    }
}

/// A probe for runs with no `--device`. Reaching it is a bug in argument
/// validation, so it fails loudly.
pub struct NoProbe;

impl Probe for NoProbe {
    fn present(&mut self) -> Result<bool> {
        bail!("no --device to probe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_spec_parses_hex_and_optional_serial() {
        let s: DeviceSpec = "046d:c08b".parse().unwrap();
        assert_eq!((s.vid, s.pid, s.serial.clone()), (0x046d, 0xc08b, None));
        let s: DeviceSpec = "046D:C08B:AA00BB11CC22DD33".parse().unwrap();
        assert_eq!(s.serial.as_deref(), Some("AA00BB11CC22DD33"));
        assert!(s.matches(0x046d, 0xc08b, Some("AA00BB11CC22DD33")));
        assert!(!s.matches(0x046d, 0xc08b, Some("other")));
        assert!(!s.matches(0x046d, 0xc08b, None));
        assert!("046d".parse::<DeviceSpec>().is_err());
        assert!("zzzz:c08b".parse::<DeviceSpec>().is_err());
        assert!("046d:".parse::<DeviceSpec>().is_err());
    }

    #[test]
    fn serialless_spec_matches_any_serial() {
        let s: DeviceSpec = "046d:c08b".parse().unwrap();
        assert!(s.matches(0x046d, 0xc08b, Some("x")));
        assert!(s.matches(0x046d, 0xc08b, None));
        assert!(!s.matches(0x046d, 0xc08c, None));
    }

    /// Really spawns the platform shell (`exit N` works in sh and cmd).
    #[test]
    fn shell_runner_reports_exit_codes_and_captures_stdout() {
        let mut r = ShellRunner;
        r.button("exit 0").unwrap();
        let e = r.button("exit 3").unwrap_err();
        assert!(e.to_string().contains("exit status 3"), "{e}");
        let cap = r.capture("echo hello").unwrap();
        assert_eq!(cap.code, Some(0));
        assert_eq!(cap.stdout.trim(), "hello");
        let cap = r.capture("exit 3").unwrap();
        assert_eq!(cap.code, Some(3));
    }

    #[test]
    fn system_clock_sleeps_and_advances() {
        let mut c = SystemClock::new();
        c.sleep(Duration::from_millis(15));
        assert!(c.now() >= Duration::from_millis(15));
    }
}
