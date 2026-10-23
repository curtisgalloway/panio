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

//! The relay board's AT dialect, as pure functions over a [`Transport`], plus
//! the NC (normally closed) wiring map and serial-port selection.
//!
//! The board speaks 9600 8N1. A command is plain ASCII and **must** end in
//! `\r\n` (callers pass commands without it; [`exchange`] appends it). Every
//! reply ends in `\n`, and the 4-channel SH-UR04A (older CP2102) follows that
//! with a NUL byte (`\n\x00`); the 1-channel SH-UR01A does not. Readers skip
//! NULs wherever they appear.
//!
//! ```text
//! AT          -> OK
//! AT+CH1=?    -> OK+CH1=0   (coil released) or OK+CH1=1 (coil energized)
//! AT+CH1=1    -> OK+CH1=1   (energize the coil)
//! AT+CH1=0    -> OK+CH1=0   (release the coil)
//! anything else, or a channel the board lacks -> Error
//! ```
//!
//! # Resync: why every command is preceded by `?\r\n`
//!
//! An unterminated command gets no reply, and its bytes stay in the board's
//! input buffer **across port close and reopen**, so the next command is
//! glued onto them and answers `Error`. [`exchange`] therefore first writes
//! [`RESYNC`] (`?\r\n`), waits [`RESYNC_SETTLE`], discards whatever came
//! back, and only then sends the command.
//!
//! The resync must not be a bare `\r\n`: if the leftover is a complete but
//! unterminated command (`AT+CH1=1` from a killed process or an older build),
//! a bare terminator COMPLETES it and the board executes it, so a read-only
//! `state` or `doctor` could cut a target's power (measured on the SH-UR04A).
//! The `?` makes any leftover an invalid command (`AT+CH1=1?` answers
//! `Error`), and on a clean buffer a lone `?` is silent.
//!
//! # Wiring: the target is on the NC contact
//!
//! Targets hang off the relay's **normally closed** contact, so the *coil*
//! state is the inverse of the *target* state: coil released (`=0`) means the
//! target is powered ("on"); coil energized (`=1`) cuts it ("off"). All of
//! that inversion lives in [`coil_for`] and [`target_on`] and nowhere else.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

/// USB vendor ID of the Silicon Labs CP2102/CP2102N the boards use.
pub const USB_VID: u16 = 0x10c4;
/// USB product ID of the Silicon Labs CP2102/CP2102N the boards use.
pub const USB_PID: u16 = 0xea60;

/// Wall-clock bound on one reply (the board answers in about 55 ms).
pub const REPLY_DEADLINE: Duration = Duration::from_secs(1);

/// The resync bytes written before every command.
///
/// The leading `?` turns any leftover in the board's input buffer into an
/// invalid command (`Error`) instead of completing it. A bare `\r\n` would
/// finish a leftover `AT+CH1=1` and energize the relay. A lone `?` on a
/// clean buffer gets no reply.
pub const RESYNC: &[u8] = b"?\r\n";

/// How long to wait after the resync [`RESYNC`] for the board to finish
/// answering it, before the input is cleared and the real command is sent.
pub const RESYNC_SETTLE: Duration = Duration::from_millis(150);

/// Byte cap on one reply, so a `-d` that points at some other chatty UART
/// cannot hold a power hook open or grow without bound.
pub const REPLY_CAP: usize = 256;

/// A byte stream to the board that can also drop stale input.
pub trait Transport: Read + Write {
    /// Discard anything already received, so a late reply to an earlier
    /// command cannot be mistaken for the answer to the next one.
    fn clear_input(&mut self) -> io::Result<()>;

    /// Wait `d`. Real transports sleep; test doubles override this so the
    /// resync settle period costs nothing.
    fn settle(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

impl<T: Transport + ?Sized> Transport for &mut T {
    fn clear_input(&mut self) -> io::Result<()> {
        (**self).clear_input()
    }
    fn settle(&mut self, d: Duration) {
        (**self).settle(d)
    }
}

/// A parsed reply line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `OK`
    Ok,
    /// `OK+CH<n>=<0|1>`: the coil of channel `n` is energized (`true`) or not.
    Channel { channel: u8, coil: bool },
    /// `Error`
    Error,
    /// Anything else, kept verbatim so errors can quote it.
    Other(String),
}

/// Parse one reply line (without its newline). Never guesses: a line this
/// does not recognize exactly is [`Reply::Other`].
pub fn parse_reply(line: &str) -> Reply {
    let line = line.trim();
    if line == "OK" {
        return Reply::Ok;
    }
    if line == "Error" {
        return Reply::Error;
    }
    if let Some(rest) = line.strip_prefix("OK+CH") {
        if let Some((ch, val)) = rest.split_once('=') {
            if let (Ok(channel), Some(coil)) = (
                ch.parse::<u8>(),
                match val {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                },
            ) {
                if channel >= 1 {
                    return Reply::Channel { channel, coil };
                }
            }
        }
    }
    Reply::Other(line.to_string())
}

/// The coil state that puts the target in the wanted state, given NC wiring:
/// target on = coil released.
pub fn coil_for(target_on: bool) -> bool {
    !target_on
}

/// The target state implied by a coil state, given NC wiring.
pub fn target_on(coil: bool) -> bool {
    !coil
}

/// `on` or `off`, the word the `state_cmd` contract wants.
pub fn onoff(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// Send one command (no terminator; `\r\n` is added) and return the reply
/// line, trimmed, without its newline.
///
/// The board is resynchronized first: [`RESYNC`] (`?\r\n`), a [`RESYNC_SETTLE`]
/// wait, then the input is cleared (see the module docs). The reply is read
/// byte by byte until `\n`, within `deadline` and `cap` bytes; NUL bytes are
/// skipped and not counted. Silence and a stream that ends are errors.
pub fn exchange<T: Transport>(
    t: &mut T,
    cmd: &str,
    deadline: Duration,
    cap: usize,
) -> Result<String> {
    t.write_all(RESYNC)
        .and_then(|()| t.flush())
        .map_err(|e| anyhow!("write error: {e}"))?;
    t.settle(RESYNC_SETTLE);
    t.clear_input()
        .map_err(|e| anyhow!("cannot flush input: {e}"))?;
    t.write_all(format!("{cmd}\r\n").as_bytes())
        .and_then(|()| t.flush())
        .map_err(|e| anyhow!("write error: {e}"))?;

    let start = Instant::now();
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if start.elapsed() > deadline {
            bail!("no complete reply to {cmd} within {deadline:?}");
        }
        match t.read(&mut byte) {
            Ok(0) => bail!("device closed the connection while waiting for a reply to {cmd}"),
            Ok(_) => {
                if byte[0] == 0 {
                    continue;
                }
                if byte[0] == b'\n' {
                    return Ok(String::from_utf8_lossy(&line).trim().to_string());
                }
                line.push(byte[0]);
                if line.len() > cap {
                    bail!("reply to {cmd} exceeded {cap} bytes without a newline");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                bail!("no reply to {cmd} (timed out); is this a relay board?")
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => bail!("read error: {e}"),
        }
    }
}

/// [`exchange`] with the standard deadline and cap.
fn ask<T: Transport>(t: &mut T, cmd: &str) -> Result<Reply> {
    Ok(parse_reply(&exchange(t, cmd, REPLY_DEADLINE, REPLY_CAP)?))
}

fn absent(channel: u8) -> anyhow::Error {
    anyhow!("channel {channel} not present on this relay (device replied Error)")
}

fn unexpected(cmd: &str, raw: &Reply) -> anyhow::Error {
    anyhow!("unexpected reply to {cmd}: {}", describe(raw))
}

fn describe(r: &Reply) -> String {
    match r {
        Reply::Ok => "OK".to_string(),
        Reply::Channel { channel, coil } => format!("OK+CH{channel}={}", u8::from(*coil)),
        Reply::Error => "Error".to_string(),
        Reply::Other(s) => format!("{s:?}"),
    }
}

/// `AT` -> `OK`: does a relay board answer on this port?
pub fn ping<T: Transport>(t: &mut T) -> Result<()> {
    match ask(t, "AT")? {
        Reply::Ok => Ok(()),
        other => Err(unexpected("AT", &other)),
    }
}

/// Read a channel's coil state (`true` = energized). Read-only.
pub fn query_coil<T: Transport>(t: &mut T, channel: u8) -> Result<bool> {
    let cmd = format!("AT+CH{channel}=?");
    match ask(t, &cmd)? {
        Reply::Channel { channel: c, coil } if c == channel => Ok(coil),
        Reply::Error => Err(absent(channel)),
        other => Err(unexpected(&cmd, &other)),
    }
}

/// The target's state on `channel` (`true` = powered), through the NC map.
pub fn query_target<T: Transport>(t: &mut T, channel: u8) -> Result<bool> {
    query_coil(t, channel).map(target_on)
}

/// Put the target on `channel` in the wanted state and confirm it twice: the
/// command's own echo, then a fresh `AT+CHn=?` read-back. A board that
/// ignores the command (echo or read-back disagrees) is an error.
pub fn set_target<T: Transport>(t: &mut T, channel: u8, want_on: bool) -> Result<()> {
    let coil = coil_for(want_on);
    let cmd = format!("AT+CH{channel}={}", u8::from(coil));
    match ask(t, &cmd)? {
        Reply::Channel {
            channel: c,
            coil: got,
        } if c == channel && got == coil => {}
        Reply::Error => return Err(absent(channel)),
        other => return Err(unexpected(&cmd, &other)),
    }
    let now = query_target(t, channel)?;
    if now != want_on {
        bail!(
            "channel {channel}: commanded {} but the relay reports the target {}",
            onoff(want_on),
            onoff(now)
        );
    }
    Ok(())
}

/// One serial port, reduced to what selection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub product: Option<String>,
    pub manufacturer: Option<String>,
}

impl Candidate {
    /// Whether this is the CP2102/CP2102N the relay boards use.
    pub fn is_relay_chip(&self) -> bool {
        self.vid == USB_VID && self.pid == USB_PID
    }
}

/// Drop each macOS `/dev/tty.<x>` node whose `/dev/cu.<x>` twin is present:
/// `cu` is the one to open (it does not wait for carrier detect).
pub fn prefer_cu(mut found: Vec<&Candidate>) -> Vec<&Candidate> {
    let cu: Vec<String> = found
        .iter()
        .filter_map(|c| c.path.strip_prefix("/dev/cu.").map(str::to_string))
        .collect();
    found.retain(|c| match c.path.strip_prefix("/dev/tty.") {
        Some(rest) => !cu.iter().any(|x| x == rest),
        None => true,
    });
    found
}

/// Choose the port to open. Exactly one of `device` and `serial` is wanted.
///
/// `device` is taken verbatim. `serial` matches the USB serial number of a
/// relay-chip (VID/PID) port, case-insensitively; zero or several matches
/// are errors that say so.
pub fn select_port(
    ports: &[Candidate],
    device: Option<&str>,
    serial: Option<&str>,
) -> Result<String> {
    match (device, serial) {
        (Some(_), Some(_)) => bail!("give either --device (-d) or --serial (-s), not both"),
        (None, None) => bail!("one of --device (-d) or --serial (-s) is required"),
        (Some(d), None) => Ok(d.to_string()),
        (None, Some(s)) => {
            let found = prefer_cu(
                ports
                    .iter()
                    .filter(|c| c.is_relay_chip())
                    .filter(|c| {
                        c.serial
                            .as_deref()
                            .is_some_and(|x| x.eq_ignore_ascii_case(s))
                    })
                    .collect(),
            );
            match found.as_slice() {
                [] => bail!(
                    "no CP2102N serial port with USB serial number {s} found \
                     (see `dsdrelay list`)"
                ),
                [one] => Ok(one.path.clone()),
                many => {
                    let paths: Vec<&str> = many.iter().map(|c| c.path.as_str()).collect();
                    bail!(
                        "USB serial number {s} matches {} ports ({}); use --device",
                        many.len(),
                        paths.join(", ")
                    )
                }
            }
        }
    }
}

/// The USB serial number the 4-channel board's older CP2102 ships with.
pub const DEFAULT_USB_SERIAL: &str = "0001";

/// A warning for `dsdrelay list` when the USB serial number cannot tell the
/// listed relays apart: two or more share one, or one is the factory default.
/// `ports` should already be reduced to one entry per device ([`prefer_cu`]).
pub fn serial_warning(ports: &[&Candidate]) -> Option<String> {
    let default = ports
        .iter()
        .any(|c| c.serial.as_deref() == Some(DEFAULT_USB_SERIAL));
    let shared = ports
        .iter()
        .any(|a| a.serial.is_some() && ports.iter().filter(|b| b.serial == a.serial).count() > 1);
    if !default && !shared {
        return None;
    }
    Some(format!(
        "warning: the USB serial number is not unique here ({}); --serial \
         (-s) cannot reliably pick a board, use --device (-d). On Linux, \
         /dev/serial/by-path/... names the physical USB port and survives \
         re-plugging into the same port. Or give each board a unique serial, \
         at your own risk (docs/power.md, \"Giving an SH-UR04A a unique \
         serial\").",
        if shared {
            "two or more listed relays share one"
        } else {
            "0001 is the factory default of the 4-channel board"
        }
    ))
}

/// The relay-chip ports, as `dsdrelay list` shows them. Enumerates only; it
/// never opens a port.
pub fn relay_ports() -> Result<Vec<Candidate>> {
    let ports =
        serialport::available_ports().map_err(|e| anyhow!("cannot enumerate serial ports: {e}"))?;
    let mut out: Vec<Candidate> = ports
        .into_iter()
        .filter_map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(u) => Some(Candidate {
                path: p.port_name,
                vid: u.vid,
                pid: u.pid,
                serial: u.serial_number,
                product: u.product,
                manufacturer: u.manufacturer,
            }),
            _ => None,
        })
        .filter(Candidate::is_relay_chip)
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A real serial port as a [`Transport`].
pub struct SerialTransport(pub Box<dyn serialport::SerialPort>);

impl Read for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for SerialTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Transport for SerialTransport {
    fn clear_input(&mut self) -> io::Result<()> {
        self.0
            .clear(serialport::ClearBuffer::Input)
            .map_err(io::Error::other)
    }
}

/// Open `path` at 9600 8N1 with a 1 s read timeout. serialport takes the
/// port exclusively on Unix (`TIOCEXCL`), so two helpers cannot interleave.
/// Opening does not change relay state.
pub fn open(path: &str) -> Result<SerialTransport> {
    let port = serialport::new(path, 9600)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(REPLY_DEADLINE)
        .open()
        .map_err(|e| anyhow!("cannot open {path}: {e}"))?;
    Ok(SerialTransport(port))
}

#[cfg(test)]
pub mod fake {
    //! An in-memory relay board that speaks the AT dialect, for tests.
    //!
    //! It models the SH-UR04A's framing: it acts only on `\r\n`-terminated
    //! lines and keeps a partial line in `inbuf` (as the real board does,
    //! across port close and reopen). A bare empty line and a lone `?` get no
    //! reply (the resync on a clean buffer), while a garbage line, including
    //! leftover + `?` such as `AT+CH1=1?`, gets `Error`. An empty line
    //! completes whatever is in `inbuf`, as on the real board.

    use super::*;
    use std::collections::VecDeque;

    pub struct FakeRelay {
        /// Coil state per channel (index 0 = channel 1). The board has
        /// exactly this many channels; others answer `Error`.
        pub coils: Vec<bool>,
        /// A board that acknowledges `AT+CHn=1` (energize) but does not act.
        pub ignore_energize: bool,
        /// A board that acknowledges every set but never acts.
        pub ignore_all_sets: bool,
        /// When set, every command gets exactly these bytes back instead.
        pub canned: Option<Vec<u8>>,
        /// End every reply with a NUL after the newline (SH-UR04A: true;
        /// SH-UR01A: false).
        pub nul: bool,
        /// Bytes received but not yet terminated by `\n`; seed it to model
        /// garbage left by an earlier session.
        pub inbuf: Vec<u8>,
        pub out: VecDeque<u8>,
        /// Every non-empty line the board acted on, terminator removed.
        pub log: Vec<String>,
    }

    impl FakeRelay {
        /// A board with `channels` channels, replying like the SH-UR04A.
        pub fn new(channels: usize) -> Self {
            FakeRelay {
                coils: vec![false; channels],
                ignore_energize: false,
                ignore_all_sets: false,
                canned: None,
                nul: true,
                inbuf: Vec::new(),
                out: VecDeque::new(),
                log: Vec::new(),
            }
        }

        fn reply(&mut self, cmd: &str) -> String {
            if cmd == "AT" {
                return "OK".into();
            }
            let Some(rest) = cmd.strip_prefix("AT+CH") else {
                return "Error".into();
            };
            let Some((ch, arg)) = rest.split_once('=') else {
                return "Error".into();
            };
            let idx = match ch.parse::<usize>() {
                Ok(n) if n >= 1 && n <= self.coils.len() => n - 1,
                _ => return "Error".into(),
            };
            match arg {
                "?" => format!("OK+CH{ch}={}", u8::from(self.coils[idx])),
                "0" | "1" => {
                    let want = arg == "1";
                    let acts = !(self.ignore_all_sets || want && self.ignore_energize);
                    if acts {
                        self.coils[idx] = want;
                    }
                    format!("OK+CH{ch}={arg}")
                }
                _ => "Error".into(),
            }
        }

        fn line(&mut self, raw: &[u8]) {
            let cmd = String::from_utf8_lossy(raw)
                .trim_end_matches('\r')
                .to_string();
            if cmd.is_empty() || cmd == "?" {
                return;
            }
            self.log.push(cmd.clone());
            match self.canned.clone() {
                Some(bytes) => self.out.extend(bytes),
                None => {
                    let r = self.reply(&cmd);
                    self.out.extend(r.bytes());
                    self.out.push_back(b'\n');
                    if self.nul {
                        self.out.push_back(0);
                    }
                }
            }
        }
    }

    impl Read for FakeRelay {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.out.pop_front() {
                Some(b) => {
                    buf[0] = b;
                    Ok(1)
                }
                None => Err(io::Error::new(io::ErrorKind::TimedOut, "no data")),
            }
        }
    }

    impl Write for FakeRelay {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inbuf.extend_from_slice(buf);
            while let Some(pos) = self.inbuf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.inbuf.drain(..=pos).collect();
                self.line(&raw[..raw.len() - 1]);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Transport for FakeRelay {
        fn clear_input(&mut self) -> io::Result<()> {
            self.out.clear();
            Ok(())
        }
        fn settle(&mut self, _d: Duration) {}
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeRelay;
    use super::*;

    fn cand(path: &str, serial: Option<&str>) -> Candidate {
        Candidate {
            path: path.into(),
            vid: USB_VID,
            pid: USB_PID,
            serial: serial.map(str::to_string),
            product: Some("CP2102N USB to UART Bridge Controller".into()),
            manufacturer: Some("Silicon Labs".into()),
        }
    }

    #[test]
    fn parses_known_replies() {
        assert_eq!(parse_reply("OK"), Reply::Ok);
        assert_eq!(parse_reply("OK\r"), Reply::Ok);
        assert_eq!(parse_reply("Error"), Reply::Error);
        assert_eq!(
            parse_reply("OK+CH1=0"),
            Reply::Channel {
                channel: 1,
                coil: false
            }
        );
        assert_eq!(
            parse_reply("OK+CH12=1\r"),
            Reply::Channel {
                channel: 12,
                coil: true
            }
        );
    }

    #[test]
    fn garbage_is_other_never_a_guess() {
        for g in [
            "",
            "ok",
            "OK+CH1=2",
            "OK+CH1=",
            "OK+CH=1",
            "OK+CH0=1",
            "OK+CH256=1",
            "OK+CHx=1",
            "OK+CH1=0 trailing",
            "\u{fffd}\u{fffd}",
            "ERROR",
        ] {
            assert!(
                matches!(parse_reply(g), Reply::Other(_)),
                "{g:?} parsed as {:?}",
                parse_reply(g)
            );
        }
    }

    #[test]
    fn nc_mapping_inverts_the_coil() {
        assert!(!coil_for(true));
        assert!(coil_for(false));
        assert!(target_on(false));
        assert!(!target_on(true));
    }

    #[test]
    fn on_releases_the_coil_and_off_energizes_it() {
        let mut d = FakeRelay::new(1);
        set_target(&mut d, 1, false).unwrap();
        assert_eq!(d.log[0], "AT+CH1=1", "off must energize the coil");
        assert!(d.coils[0]);
        set_target(&mut d, 1, true).unwrap();
        assert_eq!(d.log[2], "AT+CH1=0", "on must release the coil");
        assert!(!d.coils[0]);
        assert_eq!(d.log, ["AT+CH1=1", "AT+CH1=?", "AT+CH1=0", "AT+CH1=?"]);
    }

    #[test]
    fn query_target_reports_through_the_nc_map() {
        let mut d = FakeRelay::new(2);
        d.coils[1] = true;
        assert!(query_target(&mut d, 1).unwrap());
        assert!(!query_target(&mut d, 2).unwrap());
    }

    #[test]
    fn missing_channel_is_a_clear_error() {
        let mut d = FakeRelay::new(1);
        let e = query_target(&mut d, 2).unwrap_err().to_string();
        assert_eq!(
            e,
            "channel 2 not present on this relay (device replied Error)"
        );
        let e = set_target(&mut d, 3, true).unwrap_err().to_string();
        assert!(e.contains("channel 3 not present"), "{e}");
        assert!(!d.coils[0]);
    }

    #[test]
    fn a_board_that_ignores_off_fails_the_readback() {
        let mut d = FakeRelay::new(1);
        d.ignore_energize = true;
        let e = set_target(&mut d, 1, false).unwrap_err().to_string();
        assert!(
            e.contains("commanded off but the relay reports the target on"),
            "{e}"
        );
    }

    #[test]
    fn echo_for_the_wrong_value_or_channel_is_rejected() {
        let mut d = FakeRelay::new(1);
        d.canned = Some(b"OK+CH1=0\n".to_vec());
        let e = set_target(&mut d, 1, false).unwrap_err().to_string();
        assert!(e.contains("unexpected reply to AT+CH1=1"), "{e}");
        d.canned = Some(b"OK+CH2=1\n".to_vec());
        assert!(set_target(&mut d, 1, false).is_err());
    }

    #[test]
    fn garbage_state_reply_is_an_error_carrying_the_raw_reply() {
        let mut d = FakeRelay::new(1);
        d.canned = Some(b"OK+CH1=7\n".to_vec());
        let e = query_target(&mut d, 1).unwrap_err().to_string();
        assert!(e.contains("OK+CH1=7"), "{e}");
        d.canned = Some(b"hello\n".to_vec());
        let e = ping(&mut d).unwrap_err().to_string();
        assert!(e.contains("\"hello\""), "{e}");
    }

    #[test]
    fn silence_and_runaway_replies_are_bounded() {
        let mut d = FakeRelay::new(1);
        d.canned = Some(Vec::new());
        let e = ping(&mut d).unwrap_err().to_string();
        assert!(e.contains("no reply"), "{e}");
        d.canned = Some(vec![b'x'; REPLY_CAP + 10]);
        let e = ping(&mut d).unwrap_err().to_string();
        assert!(e.contains("without a newline"), "{e}");
    }

    #[test]
    fn stale_input_is_flushed_before_each_command() {
        let mut d = FakeRelay::new(1);
        d.out.extend(b"OK+CH1=1\n".iter().copied());
        assert!(query_target(&mut d, 1).unwrap(), "stale reply was used");
    }

    #[test]
    fn ping_accepts_ok_only() {
        let mut d = FakeRelay::new(1);
        ping(&mut d).unwrap();
        d.canned = Some(b"Error\n".to_vec());
        assert!(ping(&mut d).is_err());
    }

    #[test]
    fn select_by_device_is_verbatim() {
        assert_eq!(
            select_port(&[], Some("/dev/ttyUSB7"), None).unwrap(),
            "/dev/ttyUSB7"
        );
    }

    #[test]
    fn select_needs_exactly_one_selector() {
        assert!(select_port(&[], None, None).is_err());
        assert!(select_port(&[], Some("/dev/x"), Some("AA")).is_err());
    }

    #[test]
    fn select_by_serial_matches_case_insensitively() {
        let ports = [
            cand("/dev/ttyUSB0", Some("AA00BB11CC22DD33")),
            cand("/dev/ttyUSB1", Some("0123456789ABCDEF")),
            cand("/dev/ttyUSB2", None),
        ];
        assert_eq!(
            select_port(&ports, None, Some("aa00bb11cc22dd33")).unwrap(),
            "/dev/ttyUSB0"
        );
    }

    #[test]
    fn select_by_serial_ignores_other_usb_chips() {
        let mut other = cand("/dev/ttyUSB0", Some("AA00BB11CC22DD33"));
        other.vid = 0x0403;
        assert!(select_port(&[other], None, Some("AA00BB11CC22DD33")).is_err());
    }

    #[test]
    fn select_by_serial_no_match_and_ambiguous_both_fail() {
        let ports = [cand("/dev/ttyUSB0", Some("AA"))];
        let e = select_port(&ports, None, Some("BB"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no CP2102N serial port"), "{e}");
        let ports = [
            cand("/dev/ttyUSB0", Some("AA")),
            cand("/dev/ttyUSB1", Some("AA")),
        ];
        let e = select_port(&ports, None, Some("AA"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("matches 2 ports"), "{e}");
    }

    #[test]
    fn select_prefers_the_cu_node_over_tty_on_macos() {
        let ports = [
            cand("/dev/tty.usbserial-0001", Some("AA")),
            cand("/dev/cu.usbserial-0001", Some("AA")),
        ];
        assert_eq!(
            select_port(&ports, None, Some("AA")).unwrap(),
            "/dev/cu.usbserial-0001"
        );
        let lone = [cand("/dev/tty.usbserial-0001", Some("AA"))];
        assert_eq!(
            select_port(&lone, None, Some("AA")).unwrap(),
            "/dev/tty.usbserial-0001"
        );
    }

    #[test]
    fn four_channel_board_channels_are_independent() {
        let mut d = FakeRelay::new(4);
        set_target(&mut d, 2, false).unwrap();
        set_target(&mut d, 4, false).unwrap();
        let on: Vec<bool> = (1..=4).map(|c| query_target(&mut d, c).unwrap()).collect();
        assert_eq!(on, [true, false, true, false]);
        assert_eq!(d.coils, [false, true, false, true]);
        set_target(&mut d, 2, true).unwrap();
        assert_eq!(d.coils, [false, false, false, true]);
        let e = query_target(&mut d, 5).unwrap_err().to_string();
        assert!(e.contains("channel 5 not present"), "{e}");
    }

    #[test]
    fn garbage_left_in_the_board_buffer_does_not_corrupt_the_next_command() {
        for junk in [&b"AT+CH1="[..], b"xyz", b"AT+CH", b"AT+CH1=?"] {
            let mut d = FakeRelay::new(4);
            d.coils[2] = true;
            d.inbuf.extend_from_slice(junk);
            assert!(!query_target(&mut d, 3).unwrap(), "after {junk:?}");
            assert!(query_target(&mut d, 1).unwrap());
            assert!(d.inbuf.is_empty());
        }
        let mut d = FakeRelay::new(4);
        d.inbuf.extend_from_slice(b"AT+CH1=");
        set_target(&mut d, 1, false).unwrap();
        assert!(d.coils[0]);
    }

    #[test]
    fn every_command_is_crlf_terminated_after_a_question_mark_resync() {
        struct Spy(FakeRelay, Vec<Vec<u8>>);
        impl Read for Spy {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Write for Spy {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.1.push(b.to_vec());
                self.0.write(b)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Transport for Spy {
            fn clear_input(&mut self) -> io::Result<()> {
                self.0.clear_input()
            }
            fn settle(&mut self, _d: Duration) {}
        }
        let mut s = Spy(FakeRelay::new(1), Vec::new());
        ping(&mut s).unwrap();
        assert_eq!(s.1, [b"?\r\n".to_vec(), b"AT\r\n".to_vec()]);
    }

    #[test]
    fn nul_bytes_in_replies_are_skipped() {
        let mut d = FakeRelay::new(1);
        d.canned = Some(b"\0OK+CH1=0\n\0".to_vec());
        assert!(query_target(&mut d, 1).unwrap());
        d.canned = Some(b"O\0K\0\n\0".to_vec());
        ping(&mut d).unwrap();
        let mut d = FakeRelay::new(4);
        assert!(d.nul);
        ping(&mut d).unwrap();
        assert_eq!(
            d.out.iter().copied().collect::<Vec<_>>(),
            [0],
            "the reply's trailing NUL is left behind, as on the real board"
        );
        let mut d = FakeRelay::new(1);
        d.canned = Some(vec![0u8; REPLY_CAP + 10]);
        let e = ping(&mut d).unwrap_err().to_string();
        assert!(
            e.contains("no reply"),
            "NULs must not count toward the cap: {e}"
        );
    }

    #[test]
    fn one_channel_board_without_nul_still_works() {
        let mut d = FakeRelay::new(1);
        d.nul = false;
        ping(&mut d).unwrap();
        set_target(&mut d, 1, false).unwrap();
        assert!(!query_target(&mut d, 1).unwrap());
        set_target(&mut d, 1, true).unwrap();
        assert!(query_target(&mut d, 1).unwrap());
    }

    #[test]
    fn leftover_complete_command_is_never_executed_by_reads() {
        for (seed, coil) in [(&b"AT+CH1=1"[..], false), (&b"AT+CH1=0"[..], true)] {
            let mut d = FakeRelay::new(2);
            d.coils[0] = coil;
            d.inbuf.extend_from_slice(seed);
            query_target(&mut d, 1).unwrap();
            assert_eq!(d.coils, [coil, false], "query_target after {seed:?}");
            let mut d = FakeRelay::new(2);
            d.coils[0] = coil;
            d.inbuf.extend_from_slice(seed);
            ping(&mut d).unwrap();
            assert_eq!(d.coils, [coil, false], "ping after {seed:?}");
        }
    }

    #[test]
    fn fake_board_models_the_question_mark_resync() {
        let mut d = FakeRelay::new(1);
        d.write_all(b"?\r\n").unwrap();
        assert!(d.out.is_empty(), "lone ? on a clean buffer is silent");
        d.write_all(b"AT+CH1=1?\r\n").unwrap();
        assert_eq!(d.out.iter().copied().collect::<Vec<_>>(), b"Error\n\0");
        assert!(!d.coils[0], "leftover + ? must not execute");
        d.out.clear();
        d.write_all(b"AT+CH1=1").unwrap();
        d.write_all(b"\r\n").unwrap();
        assert!(d.coils[0], "a bare terminator completes a leftover command");
    }

    #[test]
    fn fake_board_ignores_unterminated_input_and_bare_lines() {
        let mut d = FakeRelay::new(1);
        d.write_all(b"AT").unwrap();
        assert!(d.out.is_empty(), "no reply without a terminator");
        d.write_all(b"\r\n").unwrap();
        assert_eq!(d.out.iter().copied().collect::<Vec<_>>(), b"OK\n\0");
        d.out.clear();
        d.write_all(b"\r\n").unwrap();
        assert!(d.out.is_empty(), "bare line on a clean buffer is silent");
        d.write_all(b"junk").unwrap();
        d.write_all(b"\r\n").unwrap();
        assert_eq!(d.out.iter().copied().collect::<Vec<_>>(), b"Error\n\0");
    }

    #[test]
    fn serial_warning_fires_on_default_or_shared_serials_only() {
        let uniq = [cand("/dev/ttyUSB0", Some("AA00BB11CC22DD33"))];
        let refs: Vec<&Candidate> = uniq.iter().collect();
        assert!(serial_warning(&refs).is_none());
        let two = [
            cand("/dev/ttyUSB0", Some("AA00BB11CC22DD33")),
            cand("/dev/ttyUSB1", Some("0123456789ABCDEF")),
        ];
        let refs: Vec<&Candidate> = two.iter().collect();
        assert!(serial_warning(&refs).is_none());
        let def = [cand("/dev/ttyUSB0", Some("0001"))];
        let refs: Vec<&Candidate> = def.iter().collect();
        let w = serial_warning(&refs).unwrap();
        assert!(w.contains("--device") && w.contains("by-path"), "{w}");
        let shared = [
            cand("/dev/ttyUSB0", Some("AA00BB11CC22DD33")),
            cand("/dev/ttyUSB1", Some("AA00BB11CC22DD33")),
        ];
        let refs: Vec<&Candidate> = shared.iter().collect();
        assert!(serial_warning(&refs).unwrap().contains("share one"));
    }
}
