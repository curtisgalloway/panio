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

//! A pseudo-terminal the hid daemon uses to re-export the DUT serial console.
//!
//! panio's `serial` channel opens a *device path* (`serialcap` hands it
//! straight to `tokio_serial`), so to feed the multiplexed `0x03` console
//! through it unchanged we hand the daemon a PTY: the owner thread reads/writes
//! the master end, and panio opens the slave path (via a stable symlink the
//! daemon publishes). A PTY — not a socket — is what also satisfies
//! `panio serial connect`, which `exec`s `tio` against a real terminal.
//!
//! The daemon also keeps its own handle on the slave, open in raw mode, for as
//! long as it runs. Without it the slave sits at the kernel's cooked defaults
//! (`ECHO | ICANON` on Linux) until some reader attaches and sets raw mode, and
//! in that window every byte the daemon writes to the master — the DUT's own
//! boot log — is echoed straight back into the master, where the console pump
//! frames it as `0x03` and the DUT receives its boot log as keyboard input.
//! Holding the slave open also means the master never reports a hang-up
//! between readers, so the pump does not spin on `EIO` after `tio` or
//! `serialcap` detaches.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use anyhow::{anyhow, Result};

/// An allocated PTY: the master end (owned by the daemon), the daemon's own
/// raw-mode handle on the slave, and the slave device path that panio's
/// `serial` channel points its `device =` at.
pub struct Pty {
    /// Master end, in non-blocking mode (so it never stalls the owner loop).
    pub master: File,
    /// Our own handle on the slave, in raw mode, held for the daemon's lifetime
    /// (see the module docs). Nothing reads or writes it.
    pub slave: File,
    /// The slave device node, e.g. `/dev/pts/7` (Linux) or `/dev/ttys003`
    /// (macOS). Dynamic per allocation — the daemon symlinks a stable path to it.
    pub slave_path: String,
}

/// Allocate a PTY master, open its slave in raw mode, and return both with the
/// slave device path.
pub fn open() -> Result<Pty> {
    // Safe: posix_openpt only allocates a master fd; we own it in `master`
    // immediately so any early return below closes it.
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        return Err(anyhow!("posix_openpt: {}", io::Error::last_os_error()));
    }
    let master = unsafe { File::from_raw_fd(master_fd) };
    if unsafe { libc::grantpt(master_fd) } != 0 {
        return Err(anyhow!("grantpt: {}", io::Error::last_os_error()));
    }
    if unsafe { libc::unlockpt(master_fd) } != 0 {
        return Err(anyhow!("unlockpt: {}", io::Error::last_os_error()));
    }
    // ptsname is not reentrant, but the daemon calls open() once at startup
    // before any thread touches the master, so the static buffer is safe here.
    let name_ptr = unsafe { libc::ptsname(master_fd) };
    if name_ptr.is_null() {
        return Err(anyhow!("ptsname: {}", io::Error::last_os_error()));
    }
    let slave_path = unsafe { CStr::from_ptr(name_ptr) }
        .to_string_lossy()
        .into_owned();
    let slave = open_slave_raw(&slave_path)?;
    set_nonblocking(master.as_raw_fd())?;
    Ok(Pty {
        master,
        slave,
        slave_path,
    })
}

/// Open the slave (never as our controlling terminal) and put the line
/// discipline in raw mode: no echo, no canonical line editing, no CR/LF
/// translation, no signal keys — a byte pipe, which is what a serial console
/// is. The mode is a property of the tty, so it holds for every later opener
/// until one changes it.
fn open_slave_raw(path: &str) -> Result<File> {
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .map_err(|e| anyhow!("open {path}: {e}"))?;
    let fd = slave.as_raw_fd();
    // Safe: termios is plain data; tcgetattr fills it before it is read.
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut tio) } != 0 {
        return Err(anyhow!("tcgetattr {path}: {}", io::Error::last_os_error()));
    }
    unsafe { libc::cfmakeraw(&mut tio) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) } != 0 {
        return Err(anyhow!("tcsetattr {path}: {}", io::Error::last_os_error()));
    }
    Ok(slave)
}

/// Put `fd` in non-blocking mode so the owner loop's reads/writes return
/// promptly (`WouldBlock`) instead of stalling the HID relay.
fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(anyhow!("fcntl F_GETFL: {}", io::Error::last_os_error()));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(anyhow!("fcntl F_SETFL: {}", io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn termios_of(path: &str) -> libc::termios {
        // A fresh open of the slave: the mode must be on the tty itself, not
        // only on the daemon's descriptor.
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .unwrap();
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(f.as_raw_fd(), &mut tio) }, 0);
        tio
    }

    /// The slave is raw from the moment it is allocated: no echo, no canonical
    /// mode, no output post-processing — so a reader that attaches later finds
    /// a byte pipe, and nothing the daemon writes bounces back.
    #[test]
    fn slave_is_raw_from_allocation() {
        let p = open().unwrap();
        let tio = termios_of(&p.slave_path);
        assert_eq!(
            tio.c_lflag & (libc::ECHO | libc::ICANON),
            0,
            "lflag {:#x}",
            tio.c_lflag
        );
        assert_eq!(tio.c_lflag & libc::ISIG, 0, "signal keys");
        assert_eq!(tio.c_oflag & libc::OPOST, 0, "output post-processing");
        assert_eq!(
            tio.c_iflag & (libc::ICRNL | libc::IXON),
            0,
            "input translation"
        );
    }

    /// The effect that matters: bytes written to the master (DUT console
    /// output) reach the slave and are not echoed back into the master.
    #[test]
    fn master_writes_are_not_echoed_back() {
        let mut p = open().unwrap();
        p.master.write_all(b"boot log\n").unwrap();
        let mut got = [0u8; 16];
        let n = p.slave.read(&mut got).unwrap();
        assert_eq!(&got[..n], b"boot log\n");
        let mut echo = [0u8; 16];
        match p.master.read(&mut echo) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Ok(n) => panic!("echoed back {:?}", &echo[..n]),
            Err(e) => panic!("master read: {e}"),
        }
    }
}
