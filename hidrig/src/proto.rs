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

//! Transport to the dual-board rig's control board: a serial link carrying the
//! binary HID frames composed in [`crate::compose`]. The control board is a
//! USB-CDC device, so the "baud" is nominal (USB sets the real rate); there is
//! no baud negotiation and no ASCII reply protocol — HID frames are
//! fire-and-forget, and only `0x02` control frames (ping/version/power) draw a
//! reply. The board multiplexes the DUT's serial console onto the same stream
//! as `0x03` frames, so a reply is found by demultiplexing, never by assuming
//! the next bytes are it.
//!
//! This module also parses host-side command files (sequencing/timing lives
//! here; the firmware stays a dumb relay).

use anyhow::{anyhow, Result};
use serialport::SerialPort;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::compose::{Composer, F_CTRL};
use crate::uart::split_frames;

/// Nominal open rate — a USB-CDC endpoint ignores it, but `serialport` requires
/// a value.
const NOMINAL_BAUD: u32 = 115_200;
/// How long to wait for a control-frame reply (ping/version/power).
const READ_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Written to the control board on every open, before any frame. `0x00` is not
/// a frame type, so a parser that is in sync skips every byte of it; a parser
/// left holding a partial frame by the previous owner (a daemon killed
/// mid-write) has that frame completed with an all-zero payload — a keyboard
/// report with nothing pressed, a pointer report with no buttons — instead of
/// with the first real frame's bytes, which would press whatever those bytes
/// happened to spell. Three header bytes plus the longest payload (255)
/// complete any partial frame. Documented in docs/dev/hid-dual-board-design.md
/// §5.
pub const RESYNC_PREAMBLE: [u8; 3 + 255] = [0u8; 3 + 255];

/// Longest pause a sequence file may ask for, in seconds.
const MAX_PAUSE_SECS: f64 = 3600.0;

/// macOS buffers serial reads behind a data-latency timer (`IOSSDATALAT`); drop
/// it to its floor so control-frame round trips are prompt. No-op off macOS.
#[cfg(target_os = "macos")]
fn set_low_read_latency(fd: std::os::unix::io::RawFd) {
    // _IOW('T', 0, c_ulong), per <IOKit/serial/ioss.h>.
    const IOSSDATALAT: libc::c_ulong = 0x8008_5400;
    let latency: libc::c_ulong = 1; // microseconds
    unsafe { libc::ioctl(fd, IOSSDATALAT, &latency) };
}

/// Open the control board's data CDC endpoint and put its frame parser in
/// sync (see [`RESYNC_PREAMBLE`]).
pub fn open_port(device: &str) -> Result<Box<dyn SerialPort>> {
    let port = serialport::new(device, NOMINAL_BAUD)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .timeout(READ_TIMEOUT)
        .open_native()
        .map_err(|e| anyhow!("cannot open {device}: {e}"))?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        set_low_read_latency(port.as_raw_fd());
    }
    let mut port: Box<dyn SerialPort> = Box::new(port);
    write_resync_preamble(&mut *port).map_err(|e| anyhow!("cannot resync {device}: {e}"))?;
    Ok(port)
}

/// Write [`RESYNC_PREAMBLE`] so the board's parser starts this session in
/// sync, whatever the previous owner left it holding.
pub fn write_resync_preamble<W: Write + ?Sized>(port: &mut W) -> std::io::Result<()> {
    port.write_all(&RESYNC_PREAMBLE)?;
    port.flush()
}

/// Compose `line` into frames, write them to the control board, and return the
/// reply. HID frames are fire-and-forget so a clean write is the "OK" (empty
/// string); control frames (ping/version/power) draw a reply we read back.
pub fn run_command(
    composer: &mut Composer,
    port: &mut Box<dyn SerialPort>,
    line: &str,
) -> Result<String> {
    let frames = composer.dispatch(line)?;
    let mut wants_reply = false;
    for f in &frames {
        port.write_all(f).map_err(|e| anyhow!("write error: {e}"))?;
        if f.first() == Some(&F_CTRL) {
            wants_reply = true;
        }
    }
    port.flush().map_err(|e| anyhow!("write error: {e}"))?;
    if wants_reply {
        read_control_reply(port, READ_TIMEOUT)
    } else {
        Ok(String::new())
    }
}

/// Wait for one `[0x02][cmd][len][payload]` control reply and return its
/// payload text.
///
/// The board interleaves `0x03` console frames on the same stream whenever the
/// DUT's UART has bytes (a `power cycle` during a boot log is the common case),
/// so inbound bytes are accumulated and demultiplexed with the daemon's
/// [`split_frames`]: console frames — and any stray byte — are discarded, and
/// the first complete control frame is the reply. A read timeout, EOF, or
/// `wait` elapsing with console output still flowing is a failed reply.
fn read_control_reply<R: Read + ?Sized>(port: &mut R, wait: Duration) -> Result<String> {
    let deadline = Instant::now() + wait;
    let mut inbuf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match port.read(&mut chunk) {
            Ok(0) => return Err(anyhow!("no reply from the control board (port closed?)")),
            Ok(n) => {
                inbuf.extend_from_slice(&chunk[..n]);
                let (frames, consumed) = split_frames(&inbuf);
                if let Some((_, payload)) = frames.into_iter().find(|(t, _)| *t == F_CTRL) {
                    return Ok(String::from_utf8_lossy(&payload).into_owned());
                }
                inbuf.drain(..consumed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(anyhow!(
                    "timed out waiting for a control reply — is the control board powered \
                     and the data CDC endpoint wired?"
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(anyhow!("read error: {e}")),
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for a control reply — the control board kept sending \
                 console output but never answered"
            ));
        }
    }
}

/// One step of a command file.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// A protocol command line, composed and sent.
    Cmd(String),
    /// A pause, in seconds.
    Delay(f64),
}

/// Parse a command file into steps.
///
/// Each non-blank, non-`#`-comment line is either a command or a timing
/// directive: `delay <ms>` or `sleep <seconds>`, each a finite value between 0
/// and one hour. Trailing text after the directive's value is ignored for
/// directives; command lines pass through with only their leading whitespace
/// removed (a `type` line's trailing spaces are part of its text), and are
/// composed when they run.
pub fn parse_sequence(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.trim_end().is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
        let value = rest.split_whitespace().next().unwrap_or("");
        match head.to_ascii_lowercase().as_str() {
            "delay" => steps.push(Step::Delay(pause_secs(value, 1000.0, "delay", rest)?)),
            "sleep" => steps.push(Step::Delay(pause_secs(value, 1.0, "sleep", rest)?)),
            _ => steps.push(Step::Cmd(line.to_string())),
        }
    }
    Ok(steps)
}

/// A pause directive's value in seconds: `value` is in units of
/// `1/per_second` s, and must be finite, non-negative and at most
/// [`MAX_PAUSE_SECS`] — `Duration::from_secs_f64` panics on a negative,
/// infinite or NaN value, and an hours-long pause in a sequence is a typo.
fn pause_secs(value: &str, per_second: f64, what: &str, rest: &str) -> Result<f64> {
    let v: f64 = value
        .parse()
        .map_err(|_| anyhow!("invalid {what} value: {rest:?}"))?;
    let secs = v / per_second;
    if !secs.is_finite() || !(0.0..=MAX_PAUSE_SECS).contains(&secs) {
        return Err(anyhow!(
            "{what} must be between 0 and {MAX_PAUSE_SECS} seconds: {rest:?}"
        ));
    }
    // -0.0 is not < 0.0 but is sign-negative; normalise it.
    Ok(if secs == 0.0 { 0.0 } else { secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_directives() {
        let steps = parse_sequence(
            "# boot sequence\n\
             type root\n\
             key ENTER\n\
             delay 500\n\
             \n\
             sleep 1.5\n\
             move 300 -50\n",
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Cmd("type root".into()),
                Step::Cmd("key ENTER".into()),
                Step::Delay(0.5),
                Step::Delay(1.5),
                Step::Cmd("move 300 -50".into()),
            ]
        );
    }

    #[test]
    fn directive_values_tolerate_inline_comments() {
        let steps = parse_sequence("delay 250   # settle\n").unwrap();
        assert_eq!(steps, vec![Step::Delay(0.25)]);
    }

    #[test]
    fn command_lines_pass_through_verbatim() {
        // No inline-comment stripping on commands: `type` text may contain '#'.
        let steps = parse_sequence("type issue #42\n").unwrap();
        assert_eq!(steps, vec![Step::Cmd("type issue #42".into())]);
    }

    #[test]
    fn rejects_bad_delay() {
        assert!(parse_sequence("delay soon\n").is_err());
        assert!(parse_sequence("sleep\n").is_err());
    }

    #[test]
    fn directive_case_insensitive() {
        let steps = parse_sequence("DELAY 1000\nSleep 2\n").unwrap();
        assert_eq!(steps, vec![Step::Delay(1.0), Step::Delay(2.0)]);
    }

    #[test]
    fn moveabs_passes_through_as_a_command() {
        let steps = parse_sequence("moveabs 16000 8000\n").unwrap();
        assert_eq!(steps, vec![Step::Cmd("moveabs 16000 8000".into())]);
    }

    /// A pause must be a finite, non-negative duration of at most an hour:
    /// `Duration::from_secs_f64` panics on a negative, infinite or NaN value,
    /// and a multi-hour sleep in a sequence file is a mistake, not a plan.
    #[test]
    fn delay_and_sleep_are_bounded() {
        for bad in [
            "delay -1",
            "sleep -0.5",
            "sleep inf",
            "delay inf",
            "sleep nan",
            "delay NaN",
            "sleep 3600.5",
            "delay 3600001",
        ] {
            assert!(parse_sequence(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_sequence("sleep 3600\ndelay 3600000\nsleep 0\n").unwrap(),
            vec![Step::Delay(3600.0), Step::Delay(3600.0), Step::Delay(0.0)]
        );
    }

    /// A `type` line's trailing spaces are part of its text; the parser strips
    /// only leading whitespace (blank and comment detection is unchanged).
    #[test]
    fn command_lines_keep_trailing_whitespace() {
        let steps = parse_sequence("  type hi  \n").unwrap();
        assert_eq!(steps, vec![Step::Cmd("type hi  ".into())]);
        assert!(parse_sequence("   \n\t\n").unwrap().is_empty());
    }

    /// A control board's upstream byte stream, handed over one chunk per
    /// read and then silence (a read timeout) — as the CDC port delivers it.
    struct Chunked(std::collections::VecDeque<Vec<u8>>);

    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some(mut c) = self.0.pop_front() else {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "silence"));
            };
            let n = c.len().min(buf.len());
            buf[..n].copy_from_slice(&c[..n]);
            if n < c.len() {
                c.drain(..n);
                self.0.push_front(c);
            }
            Ok(n)
        }
    }

    fn chunked(chunks: &[&[u8]]) -> Chunked {
        Chunked(chunks.iter().map(|c| c.to_vec()).collect())
    }

    /// The reply to a `power cycle` issued while the DUT is booting arrives
    /// behind console frames, and may itself be split across reads. Both are
    /// demultiplexed, not mistaken for "unexpected reply frame type 0x03".
    #[test]
    fn control_reply_is_found_behind_console_output() {
        let mut port = chunked(&[
            &[0x03, 0x00, 0x05, b'b', b'o', b'o', b't', b'\n'],
            &[0x02, 0x03],
            &[0x00],
        ]);
        assert_eq!(read_control_reply(&mut port, READ_TIMEOUT).unwrap(), "");
        // A console payload may itself contain 0x02: it is console bytes, not
        // the reply header.
        let mut port = chunked(&[
            &[0x03, 0x00, 0x03, 0x02, 0x01, 0x00],
            &[0x02, 0x02, 0x0e],
            b"dual-control/1",
        ]);
        assert_eq!(
            read_control_reply(&mut port, READ_TIMEOUT).unwrap(),
            "dual-control/1"
        );
    }

    #[test]
    fn control_reply_times_out_on_console_only_traffic() {
        let mut port = chunked(&[&[0x03, 0x00, 0x01, b'x'], &[0x03, 0x00, 0x01, b'y']]);
        let err = read_control_reply(&mut port, READ_TIMEOUT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    /// Console output that never pauses must not hold the one-shot forever:
    /// the overall wait bounds it even though every read returns bytes.
    #[test]
    fn control_reply_gives_up_when_console_output_never_stops() {
        struct Chatter;
        impl Read for Chatter {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf[..4].copy_from_slice(&[0x03, 0x00, 0x01, b'.']);
                Ok(4)
            }
        }
        let err = read_control_reply(&mut Chatter, Duration::from_millis(50))
            .unwrap_err()
            .to_string();
        assert!(err.contains("console output"), "{err}");
    }

    #[test]
    fn control_reply_reports_a_closed_port() {
        let err = read_control_reply(&mut std::io::empty(), READ_TIMEOUT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed"), "{err}");
    }

    /// The preamble is invisible to a parser in sync, and completes a stale
    /// partial frame with zeros rather than leaving it for the next real frame
    /// to complete. Exercised against the host-side parser, which implements
    /// the same length-prefixed grammar as the firmware.
    #[test]
    fn resync_preamble_is_skipped_in_sync_and_completes_a_stale_frame() {
        assert_eq!(RESYNC_PREAMBLE.len(), 3 + 255);
        assert!(RESYNC_PREAMBLE.iter().all(|&b| b == 0));
        let (frames, consumed) = split_frames(&RESYNC_PREAMBLE);
        assert!(frames.is_empty());
        assert_eq!(consumed, RESYNC_PREAMBLE.len());
        // A stale header promising the longest payload, then the preamble:
        // exactly one all-zero frame, nothing left over.
        let mut stream = vec![0x03, 0x00, 0xff];
        stream.extend_from_slice(&RESYNC_PREAMBLE);
        let (frames, consumed) = split_frames(&stream);
        assert_eq!(frames, vec![(0x03, vec![0u8; 255])]);
        assert_eq!(consumed, stream.len());
        // A stale frame cut after its type byte alone: completed as an empty
        // frame.
        let mut stream = vec![0x02];
        stream.extend_from_slice(&RESYNC_PREAMBLE);
        let (frames, consumed) = split_frames(&stream);
        assert_eq!(frames, vec![(0x02, vec![])]);
        assert_eq!(consumed, stream.len());
    }

    #[test]
    fn preamble_is_what_open_writes() {
        let mut sink = Vec::new();
        write_resync_preamble(&mut sink).unwrap();
        assert_eq!(sink, RESYNC_PREAMBLE.to_vec());
    }
}
