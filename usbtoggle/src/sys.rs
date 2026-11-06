// Copyright 2026 The Panio Authors
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

use std::io::{self, Read};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
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

/// How often a running command is checked.
const WAIT_POLL: Duration = Duration::from_millis(10);
/// How long to wait for a killed process (or `taskkill`) to finish. A process
/// stuck in the kernel on a dead USB device may never go; it is left behind
/// rather than allowed to block this helper.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long to wait for the rest of a command's stdout once it has exited.
/// Something it left running can hold the pipe open forever.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);
/// The most stdout kept from one command. `host` or `target` is all that is
/// ever parsed; the rest is drained so a chatty command cannot stall.
const MAX_OUTPUT: usize = 64 * 1024;

/// Make `cmd` the leader of its own process group, so [`kill_tree`] reaches
/// the shell and everything it started. Windows needs nothing here: `taskkill
/// /T` walks the tree.
#[cfg(unix)]
fn own_process_group(cmd: &mut Command) {
    std::os::unix::process::CommandExt::process_group(cmd, 0);
}

#[cfg(windows)]
fn own_process_group(_cmd: &mut Command) {}

/// `Some` with the exit status, or `None` if `child` is still running at `limit`.
fn wait_within(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Ok(None);
        }
        thread::sleep(left.min(WAIT_POLL));
    }
}

/// Kill the unreaped `child` and everything it started. The child has not been
/// waited on, so its pid (and process group id) cannot have been reused.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid @ 2..) = i32::try_from(child.id()) {
        // Safe: kill() on a negative pid signals that group; failure is fine.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Ok(mut taskkill) = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if !matches!(wait_within(&mut taskkill, KILL_GRACE), Ok(Some(_))) {
            let _ = taskkill.kill();
        }
    }
    let _ = child.kill();
    let _ = wait_within(child, KILL_GRACE);
}

/// Wait for `child`, killing its whole tree if it outlives `limit`.
fn wait_or_kill(child: &mut Child, cmd: &str, limit: Duration) -> Result<ExitStatus> {
    match wait_within(child, limit) {
        Ok(Some(status)) => Ok(status),
        Ok(None) => {
            kill_tree(child);
            bail!(
                "{cmd:?} timed out after {} ms; it and anything it started were killed",
                limit.as_millis()
            );
        }
        Err(e) => {
            kill_tree(child);
            Err(e).with_context(|| format!("cannot wait for {cmd:?}"))
        }
    }
}

/// Read `out` on its own thread, so a full pipe cannot stall the command while
/// this thread watches the clock. Chunks arrive as they are read, so what a
/// command printed survives something it left running holding the pipe open.
fn read_in_background(mut out: ChildStdout) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut chunk = [0; 4096];
        let mut kept = 0;
        while let Ok(n @ 1..) = out.read(&mut chunk) {
            if kept < MAX_OUTPUT {
                kept += n;
                if tx.send(chunk[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// What the reader has, waiting at most [`OUTPUT_GRACE`] for end of file.
fn collect_output(rx: &Receiver<Vec<u8>>) -> Vec<u8> {
    let start = Instant::now();
    let mut buf = Vec::new();
    while let Ok(chunk) = rx.recv_timeout(OUTPUT_GRACE.saturating_sub(start.elapsed())) {
        buf.extend(chunk);
    }
    buf
}

/// `f`'s result, or `None` if it has not finished within `limit`. For blocking
/// calls that cannot be interrupted: the thread is abandoned and ends with the
/// process.
fn finish_within<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

pub struct ShellRunner;

impl Runner for ShellRunner {
    fn button(&mut self, cmd: &str, limit: Duration) -> Result<()> {
        // stdout is reserved for the side this helper prints, so the
        // command's stdout is sent to our stderr.
        let mut c = shell_command(cmd);
        c.stdin(Stdio::null()).stdout(Stdio::from(io::stderr()));
        own_process_group(&mut c);
        let mut child = c.spawn().with_context(|| format!("cannot start {cmd:?}"))?;
        let status = wait_or_kill(&mut child, cmd, limit)?;
        if !status.success() {
            bail!("{cmd:?} failed ({})", describe_code(status.code()));
        }
        Ok(())
    }

    fn capture(&mut self, cmd: &str, limit: Duration) -> Result<Captured> {
        let mut c = shell_command(cmd);
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        own_process_group(&mut c);
        let mut child = c.spawn().with_context(|| format!("cannot start {cmd:?}"))?;
        let out = child.stdout.take().expect("stdout is piped");
        let rx = read_in_background(out);
        let status = wait_or_kill(&mut child, cmd, limit)?;
        Ok(Captured {
            code: status.code(),
            stdout: String::from_utf8_lossy(&collect_output(&rx)).into_owned(),
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
    fn present(&mut self, limit: Duration) -> Result<bool> {
        let spec = self.spec.clone();
        finish_within(limit, move || {
            let mut devices = nusb::list_devices()
                .wait()
                .map_err(|e| anyhow!("cannot list USB devices: {e}"))?;
            Ok(devices.any(|d| spec.matches(d.vendor_id(), d.product_id(), d.serial_number())))
        })
        .unwrap_or_else(|| {
            bail!(
                "listing USB devices timed out after {} ms",
                limit.as_millis()
            )
        })
    }
}

/// A probe for runs with no `--device`. Reaching it is a bug in argument
/// validation, so it fails loudly.
pub struct NoProbe;

impl Probe for NoProbe {
    fn present(&mut self, _limit: Duration) -> Result<bool> {
        bail!("no --device to probe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("usbtoggle-sys-{name}-{}", std::process::id()))
    }

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
        r.button("exit 0", LONG).unwrap();
        let e = r.button("exit 3", LONG).unwrap_err();
        assert!(e.to_string().contains("exit status 3"), "{e}");
        let cap = r.capture("echo hello", LONG).unwrap();
        assert_eq!(cap.code, Some(0));
        assert_eq!(cap.stdout.trim(), "hello");
        let cap = r.capture("exit 3", LONG).unwrap();
        assert_eq!(cap.code, Some(3));
    }

    /// A command that writes `marker` after about two seconds, from a nested
    /// shell: only killing the whole tree stops the write. Valid in sh and cmd.
    fn nested_late_write(marker: &std::path::Path) -> String {
        let m = marker.display();
        if cfg!(windows) {
            format!("cmd /C \"ping -n 3 127.0.0.1 >nul & echo late> {m}\" & exit 0")
        } else {
            format!("sh -c \"sleep 2; echo late > '{m}'\"; exit 0")
        }
    }

    fn assert_tree_killed(marker: &std::path::Path, e: &anyhow::Error) {
        assert!(e.to_string().contains("timed out after 200 ms"), "{e}");
        thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "a descendant survived the timeout");
    }

    #[test]
    fn a_hung_button_command_is_killed_with_its_descendants() {
        let marker = temp_path("button");
        let e = ShellRunner
            .button(&nested_late_write(&marker), Duration::from_millis(200))
            .unwrap_err();
        assert_tree_killed(&marker, &e);
    }

    #[test]
    fn a_hung_capture_command_is_killed_with_its_descendants() {
        let marker = temp_path("capture");
        let e = ShellRunner
            .capture(&nested_late_write(&marker), Duration::from_millis(200))
            .unwrap_err();
        assert_tree_killed(&marker, &e);
    }

    #[test]
    fn finish_within_gives_up_on_a_blocked_call() {
        assert_eq!(finish_within(LONG, || 7), Some(7));
        let blocked = finish_within(Duration::from_millis(20), || {
            thread::sleep(Duration::from_secs(60));
        });
        assert_eq!(blocked, None);
    }

    #[test]
    fn system_clock_sleeps_and_advances() {
        let mut c = SystemClock::new();
        c.sleep(Duration::from_millis(15));
        assert!(c.now() >= Duration::from_millis(15));
    }
}
