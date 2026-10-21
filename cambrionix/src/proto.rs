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

//! Cambrionix serial protocol: command framing, response parsing, and the
//! `state` table parser.

use anyhow::{anyhow, bail, Result};
use serialport::SerialPort;
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

/// Wall-clock bound on collecting one command's response. The hub answers
/// `state` in well under a second; this stops a `-d` that points at some
/// other chatty UART from holding a power hook open indefinitely.
pub const RESPONSE_DEADLINE: Duration = Duration::from_secs(3);

/// Byte cap on one command's response, for the same reason: the largest
/// legitimate answer (a 15-port `state` table) is a few hundred bytes.
pub const RESPONSE_CAP: usize = 64 * 1024;

/// A single row from the hub's `state` output.
#[derive(Debug, Clone, PartialEq)]
pub struct PortRow {
    /// Port number as reported by the hub (0 = host/system port).
    pub port: u8,
    /// Voltage in millivolts (hub reports volts×100; we store as-is: raw×10 mV).
    pub volts_raw: u16,
    /// Current in milliamps.
    pub milliamps: u16,
    /// First flag (PDSync firmware): 'A' attached, 'D' detached, 'P' PD
    /// contract established, 'C' Type-C cable with no device.
    pub attach: char,
    /// Second flag (PDSync firmware), case-sensitive: 'I' idle, 'S' host
    /// port connected, 'C' charging, 'F' finished charging, 'c' power on
    /// but no device, 'O' off. See [`vbus_on`].
    pub mode: char,
    /// Remaining raw columns (exactly as received).
    pub rest: String,
}

impl PortRow {
    /// Whether the port is **on** (see [`vbus_on`]). A letter outside the
    /// table is an error carrying the raw value — the `state_cmd` contract
    /// wants a real read-back, not a guess about a mode the hub has never
    /// shown us.
    pub fn is_on(&self) -> Result<bool> {
        vbus_on(self.mode).ok_or_else(|| {
            anyhow!(
                "port {}: hub reports mode {:?}, which this helper does not map to on or off",
                self.port,
                self.mode
            )
        })
    }
}

/// Map a second-flag letter from the PDSync `state` table to whether the
/// port's power is on. Every state the hub can move a port into after `mode
/// c` is on: `I` idle, `C` charging, `F` finished charging (a device that has
/// stopped drawing current, #329), and `c` power enabled with no device;
/// `S` is the connected host port. `O` is off. Flags are case-sensitive per
/// the Cambrionix CLI manual, so anything else (lowercase `o` included) is
/// `None`.
pub fn vbus_on(mode: char) -> Option<bool> {
    match mode {
        'I' | 'S' | 'C' | 'F' | 'c' => Some(true),
        'O' => Some(false),
        _ => None,
    }
}

/// Whether a read-back mode letter confirms a commanded transition: any
/// powered state after `mode c`, `O` after `mode o` (see [`vbus_on`]). What
/// the check must catch is the hub leaving the port `O` (or answering with
/// something unmapped) after an `on`.
pub fn mode_confirms(mode: char, want_on: bool) -> bool {
    vbus_on(mode) == Some(want_on)
}

/// Parse the multi-line text returned by the `state` command into a Vec of
/// `PortRow`. Lines that do not match the expected format are silently skipped
/// (echo lines, prompts, blanks, etc.).
pub fn parse_state(output: &str) -> Vec<PortRow> {
    let mut rows = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(">>") || trimmed.starts_with("state") {
            continue;
        }
        if let Some(row) = parse_state_line(trimmed) {
            rows.push(row);
        }
    }
    rows
}

/// Parse a single state line of the form:
/// ` 0, 0531, 0000, P F -, 121, 6795, 0.00`
///
/// Returns `None` if the line doesn't match the expected shape.
fn parse_state_line(line: &str) -> Option<PortRow> {
    let mut cols = line.splitn(7, ',');

    let port_str = cols.next()?.trim();
    let volts_str = cols.next()?.trim();
    let ma_str = cols.next()?.trim();
    let flags_str = cols.next()?.trim();
    let rest_cols: Vec<&str> = cols.collect();

    let port: u8 = port_str.parse().ok()?;
    let volts_raw: u16 = volts_str.parse().ok()?;
    let milliamps: u16 = ma_str.parse().ok()?;

    // flags_str is three space-separated letters, e.g. "P F -" or "A C -"
    let mut flags = flags_str.split_whitespace();
    let attach = flags.next()?.chars().next()?;
    let mode = flags.next()?.chars().next()?;

    let rest = rest_cols.join(",");

    Some(PortRow {
        port,
        volts_raw,
        milliamps,
        attach,
        mode,
        rest,
    })
}

/// Open a serial port to the hub control UART.
///
/// Returns the port ready for command/response exchanges.  The caller should
/// sleep ~200 ms after calling this to let the hardware settle.
pub fn open_port(device: &str) -> Result<Box<dyn SerialPort>> {
    let port = serialport::new(device, 115_200)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .timeout(Duration::from_millis(1000))
        .open()
        .map_err(|e| anyhow!("cannot open {device}: {e}"))?;
    Ok(port)
}

/// Send a command to the hub and collect the response lines up to (but not
/// including) the `>>` prompt line, bounded by [`RESPONSE_DEADLINE`] and
/// [`RESPONSE_CAP`]. Errors if the hub reports an error (see
/// [`is_error_line`]).
///
/// The caller is responsible for clearing any stale input before calling here.
pub fn run_command(port: &mut Box<dyn SerialPort>, cmd: &str) -> Result<String> {
    // Send the command terminated with CR LF.
    let msg = format!("{cmd}\r\n");
    port.write_all(msg.as_bytes())
        .map_err(|e| anyhow!("write error: {e}"))?;

    let mut reader = BufReader::new(&mut **port);
    collect_response(&mut reader, RESPONSE_DEADLINE, RESPONSE_CAP)
}

/// Whether a response line is the hub reporting an error. The exact form
/// the Cambrionix command-line API uses is not documented in this repo, so
/// this is a deliberately broad heuristic: a line starting with `E` followed
/// by a digit (an error code) or containing "error" in any case. `state`
/// rows start with a port number and command echoes with the command word,
/// so neither can match.
fn is_error_line(line: &str) -> bool {
    let t = line.trim_start();
    let coded = t
        .strip_prefix('E')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    coded || t.to_ascii_lowercase().contains("error")
}

/// Read response lines from `reader` until the `>>` prompt, EOF, or a read
/// timeout, whichever comes first — or fail once `budget` of wall-clock time
/// or `cap` bytes have gone by without a prompt, so a chatty UART that is
/// not a Cambrionix hub cannot hang the hook or grow the buffer unbounded.
/// A partial trailing line (no newline yet) is dropped, as before. Bytes are
/// consumed chunk-wise rather than line-wise so the deadline is checked even
/// when the input never contains a newline.
fn collect_response<R: BufRead>(reader: &mut R, budget: Duration, cap: usize) -> Result<String> {
    let deadline = Instant::now() + budget;
    let mut pending: Vec<u8> = Vec::new();
    let mut output = String::new();
    let mut total = 0usize;
    loop {
        if Instant::now() >= deadline {
            bail!(
                "no `>>` prompt from the hub within {:.1}s — is -d the hub's control UART?",
                budget.as_secs_f64()
            );
        }
        let chunk = match reader.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(anyhow!("read error: {e}")),
        };
        if chunk.is_empty() {
            break; // EOF (shouldn't happen on serial, but handle it)
        }
        let n = chunk.len().min(cap - total);
        pending.extend_from_slice(&chunk[..n]);
        reader.consume(n);
        total += n;

        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line);
            let trimmed = text.trim_end_matches(['\r', '\n']);
            if trimmed.starts_with(">>") {
                return Ok(output);
            }
            if is_error_line(trimmed) {
                bail!("hub reported an error: {trimmed}");
            }
            output.push_str(trimmed);
            output.push('\n');
        }
        if total >= cap {
            bail!("hub sent {cap} bytes without a `>>` prompt — is -d the hub's control UART?");
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    const SAMPLE: &str = "\
 0, 0531, 0000, P F -, 121, 6795, 0.00
 1, 0509, 0245, A C -, 489675, x, 168.67
 2, 0000, 0000, D I -, 0, x, 0.00
 3, 0000, 0000, D I -, 0, x, 0.00
 4, 0509, 0538, A C -, 85285, x, 54.47
";

    #[test]
    fn parse_all_five_rows() {
        let rows = parse_state(SAMPLE);
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn port0_is_host_row() {
        let rows = parse_state(SAMPLE);
        let r = &rows[0];
        assert_eq!(r.port, 0);
        assert_eq!(r.attach, 'P');
        assert_eq!(r.mode, 'F');
        assert_eq!(r.volts_raw, 531);
        assert_eq!(r.milliamps, 0);
    }

    #[test]
    fn port4_is_on() {
        let rows = parse_state(SAMPLE);
        let r = rows.iter().find(|r| r.port == 4).unwrap();
        assert_eq!(r.attach, 'A');
        assert_eq!(r.mode, 'C');
        assert!(r.is_on().unwrap(), "port 4 mode C should be on");
        assert_eq!(r.volts_raw, 509);
        assert_eq!(r.milliamps, 538);
    }

    #[test]
    fn port2_idle_is_on() {
        // Idle ('I') is NOT off — only 'O' means off.
        let rows = parse_state(SAMPLE);
        let r = rows.iter().find(|r| r.port == 2).unwrap();
        assert_eq!(r.mode, 'I');
        assert!(
            r.is_on().unwrap(),
            "port 2 mode I (idle) should be considered on"
        );
    }

    #[test]
    fn port3_idle_is_on() {
        let rows = parse_state(SAMPLE);
        let r = rows.iter().find(|r| r.port == 3).unwrap();
        assert_eq!(r.mode, 'I');
        assert!(r.is_on().unwrap());
    }

    #[test]
    fn off_mode_letter() {
        // A synthetic line with mode 'O' → is_on() == false.
        let row = parse_state_line(" 5, 0000, 0000, D O -, 0, x, 0.00").unwrap();
        assert_eq!(row.mode, 'O');
        assert!(!row.is_on().unwrap(), "mode O should be off");
    }

    /// #329: a charge-mode port whose device has stopped drawing current
    /// reads `F` (finished charging), with `P` (PD contract) in the first
    /// column. VBUS is still on, so `state` and the `on` read-back say on.
    #[test]
    fn charge_finished_port_is_on() {
        let row = parse_state_line(" 4, 0511, 0000, P F -, 0, x, 0.00").unwrap();
        assert_eq!(row.attach, 'P');
        assert_eq!(row.mode, 'F');
        assert!(row.is_on().unwrap(), "mode F (finished charging) is on");
        assert!(mode_confirms(row.mode, true), "F confirms on");
        assert!(!mode_confirms(row.mode, false), "F must not confirm off");
    }

    #[test]
    fn is_on_is_an_allow_list() {
        let with_mode = |mode: char| PortRow {
            port: 7,
            volts_raw: 0,
            milliamps: 0,
            attach: 'D',
            mode,
            rest: String::new(),
        };
        for m in ['I', 'S', 'C', 'F', 'c'] {
            assert!(with_mode(m).is_on().unwrap(), "mode {m}");
        }
        assert!(!with_mode('O').is_on().unwrap(), "mode O");
        // Flags are case-sensitive (lowercase `c` is its own state), so a
        // letter the PDSync table does not list is an error that carries the
        // raw letter, not a guessed `on`.
        for m in ['o', 's', 'i', 'f', 'X', '-', '?'] {
            let err = with_mode(m).is_on().expect_err(&format!("mode {m}"));
            assert!(err.to_string().contains(&format!("{m:?}")), "{err}");
        }
    }

    #[test]
    fn mode_confirms_only_the_commanded_profile() {
        for m in ['I', 'S', 'C', 'F', 'c'] {
            assert!(mode_confirms(m, true), "{m} confirms on");
            assert!(!mode_confirms(m, false), "{m} must not confirm off");
        }
        for m in ['O', 'o', 'X'] {
            assert!(!mode_confirms(m, true), "{m} must not confirm on");
        }
        assert!(mode_confirms('O', false), "O confirms off");
        for m in ['o', 'X'] {
            assert!(!mode_confirms(m, false), "{m} must not confirm off");
        }
    }

    #[test]
    fn tolerates_echo_and_prompt_lines() {
        let noisy = "state\n\
 0, 0531, 0000, P F -, 121, 6795, 0.00\n\
>> \n\
\n\
some garbage line\n\
 1, 0509, 0245, A C -, 489675, x, 168.67\n";
        let rows = parse_state(noisy);
        // Only rows 0 and 1 should parse; "state", ">>", blank, and garbage skip.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].port, 0);
        assert_eq!(rows[1].port, 1);
    }

    #[test]
    fn port1_volts_and_current() {
        let rows = parse_state(SAMPLE);
        let r = rows.iter().find(|r| r.port == 1).unwrap();
        assert_eq!(r.volts_raw, 509);
        assert_eq!(r.milliamps, 245);
        assert!(r.is_on().unwrap());
    }

    #[test]
    fn error_marker_lines() {
        for l in [
            "E01: Unrecognised command",
            "E1",
            " E42 bad port",
            "Error: no",
            "some error here",
            "ERROR",
        ] {
            assert!(is_error_line(l), "{l:?}");
        }
        for l in [
            "mode c 4",
            "state",
            " 1, 0509, 0245, A C -, 489675, x, 168.67",
            "E",
            "Extra",
            "Enable",
            ">> ",
        ] {
            assert!(!is_error_line(l), "{l:?}");
        }
    }

    /// A reader that repeats `pattern` forever, never blocking.
    struct Chatty {
        pattern: &'static [u8],
        pos: usize,
    }

    impl Read for Chatty {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            for b in buf.iter_mut() {
                *b = self.pattern[self.pos];
                self.pos = (self.pos + 1) % self.pattern.len();
            }
            Ok(buf.len())
        }
    }

    /// A reader that yields one byte per call after `delay`, never a newline
    /// or a prompt — the shape of a serial line that is slowly emitting
    /// something that is not a Cambrionix hub.
    struct Slow {
        delay: Duration,
    }

    impl Read for Slow {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.delay);
            buf[0] = b'z';
            Ok(1)
        }
    }

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn collects_lines_up_to_the_prompt() {
        let mut r = Cursor::new(
            b"mode c 4\r\n 4, 0509, 0538, A C -, 85285, x, 54.47\r\n>> \r\ntrailing\r\n",
        );
        let out = collect_response(&mut r, LONG, RESPONSE_CAP).unwrap();
        assert_eq!(out, "mode c 4\n 4, 0509, 0538, A C -, 85285, x, 54.47\n");
    }

    #[test]
    fn eof_without_prompt_returns_what_arrived() {
        let mut r = Cursor::new(b"state\r\n 0, 0531, 0000, P F -, 121, 6795, 0.00\r\npartial");
        let out = collect_response(&mut r, LONG, RESPONSE_CAP).unwrap();
        assert_eq!(out, "state\n 0, 0531, 0000, P F -, 121, 6795, 0.00\n");
    }

    #[test]
    fn hub_error_lines_fail_the_command() {
        let mut r = Cursor::new(b"mode c 99\r\nE01: Unrecognised command\r\n>> ");
        let err = collect_response(&mut r, LONG, RESPONSE_CAP).unwrap_err();
        assert!(err.to_string().contains("E01"), "{err}");
    }

    #[test]
    fn endless_lines_stop_at_the_byte_cap() {
        let mut r = BufReader::new(Chatty {
            pattern: b"garbage line\n",
            pos: 0,
        });
        let start = Instant::now();
        let err = collect_response(&mut r, LONG, 4096).unwrap_err();
        assert!(err.to_string().contains("4096"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn endless_bytes_without_newline_stop_at_the_byte_cap() {
        let mut r = BufReader::new(Chatty {
            pattern: b"x",
            pos: 0,
        });
        let err = collect_response(&mut r, LONG, 4096).unwrap_err();
        assert!(err.to_string().contains("4096"), "{err}");
    }

    #[test]
    fn slow_stream_without_prompt_stops_at_the_deadline() {
        let mut r = BufReader::new(Slow {
            delay: Duration::from_millis(10),
        });
        let start = Instant::now();
        let err = collect_response(&mut r, Duration::from_millis(150), RESPONSE_CAP).unwrap_err();
        assert!(err.to_string().contains("prompt"), "{err}");
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(150) && took < Duration::from_secs(2),
            "took {took:?}"
        );
    }
}
