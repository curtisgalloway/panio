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

//! CH9329 serial-to-HID protocol session.
//!
//! The WCH CH9329 bridges framed UART commands to a USB HID keyboard + mouse
//! presented to the *target*. Frames are
//! `HEAD(57 AB) ADDR(00) CMD LEN DATA SUM`, `SUM = Σ(all preceding) & 0xFF`;
//! the chip replies `CMD|0x80` (ok) or `CMD|0xC0` (error). See
//! `docs/ch9329-spec.md` for the clean-room protocol reference, restated from
//! the WCH datasheet. The framing, checksum, and GET_INFO paths here are
//! verified against real hardware (chip version 0x38 over a CH340 adapter).
//!
//! A [`Session`] also tracks the *held* report state (modifiers, key slots,
//! mouse buttons, last absolute position) so `combo`/`down`/`mdown` compose
//! within one process — the chip itself only remembers the last report it was
//! given.

use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serialport::SerialPort;

use crate::keys::Key;

const HEAD: [u8; 2] = [0x57, 0xAB];
const ADDR: u8 = 0x00;

const CMD_GET_INFO: u8 = 0x01;
const CMD_KB_GENERAL: u8 = 0x02;
const CMD_MS_ABS: u8 = 0x04;
const CMD_MS_REL: u8 = 0x05;
const CMD_GET_PARA_CFG: u8 = 0x08;
const CMD_SET_PARA_CFG: u8 = 0x09;
const CMD_RESET: u8 = 0x0F;

/// Vendor extension implemented by Openterface MCUs that *emulate* the CH9329
/// (the KVM-Go's CH32V208), not by a real CH9329: drive or query the USB mux
/// that shares the onboard microSD reader between the host and the target.
/// See `notes/openterface-usb-mux-spec.md`.
const CMD_USB_SWITCH: u8 = 0x17;

/// `CMD_USB_SWITCH` payload length. Load-bearing: only the last of the five
/// bytes selects a direction, but the four leading zeros are part of the
/// declared payload. Trimming them shifts where the checksum lands and the
/// device rejects the frame outright.
const USB_SWITCH_LEN: usize = 5;

const USB_SWITCH_TO_HOST: u8 = 0x00;
const USB_SWITCH_TO_TARGET: u8 = 0x01;
const USB_SWITCH_QUERY: u8 = 0x03;

/// Key slots in the boot-protocol keyboard report.
const KEY_SLOTS: usize = 6;

/// Length of the CH9329 parameter-config block (`docs/ch9329-spec.md` §5).
const PARA_CFG_LEN: usize = 50;
/// Byte offset of the 4-byte big-endian baud field within the config block.
const PARA_CFG_BAUD: usize = 3;

/// CH9329 absolute coordinate full-scale (12-bit, in a 4096×4096 grid).
const ABS_FULL: i64 = 4096;
/// panio's `moveabs` logical maximum (`hidrig` ABS_MAX).
const LOGICAL_MAX: i64 = 32_767;

/// Baud rates tried, in order, when none is forced. Openterface units default
/// to 115200; a Sipeed NanoKVM-USB ships at 57600; a factory CH9329 is at 9600
/// (see `docs/ch9329-spec.md` §2).
const BAUD_CANDIDATES: [u32; 3] = [115_200, 57_600, 9_600];

/// The datasheet's supported serial rates for `SET_PARA_CFG`
/// (`docs/ch9329-spec.md` §5). A rate outside this range would be written to
/// flash and leave a chip nothing can talk to.
const BAUD_MIN: u32 = 1_200;
const BAUD_MAX: u32 = 115_200;

/// Ceiling on the text of one `type` command. Each character is a UART round
/// trip plus [`TYPE_GAP`], so an unbounded string holds the wire — and every
/// other client — for as long as the caller likes.
pub const MAX_TYPE_CHARS: usize = 4096;

/// Ceiling on one `move`/`scroll` call's total travel, per axis. The logical
/// space is 0..=32767, so this already crosses the whole screen; each 127-unit
/// report is a round trip plus a 4 ms gap, so an unbounded total would hold
/// the wire for minutes.
const MAX_REL_TOTAL: i32 = 32_767;

/// Quiet period between baud probes — defensive, and host-dependent.
///
/// On some hosts a reopen immediately after a failed probe times out, and only
/// an idle gap lets the next rate answer. Reported (issue #81) on Linux with
/// the CH340 passed through to a VM: an Openterface at the factory 9600 rate
/// (the third candidate, so behind two failed probes) autodetected 1/6, while
/// `-b 9600` answered 6/6 and the same probes as separate processes with a
/// 0.3 s gap answered 4/4.
///
/// This is NOT a property of the CH9329 itself. The same Openterface, same
/// 9600 chip, attached directly to a macOS host autodetects 10/10 *without*
/// any settle — so the delay is compensating for the host's USB-serial path
/// (USB passthrough is the leading suspect), not for the chip.
///
/// Kept because the cost is small and paid once: autodetect runs when the
/// session opens, and one-shots route through a persistent daemon, so this is
/// at most 300 ms per failed candidate once per daemon start — not per
/// command. Measured on macOS against a 9600 chip: 1.14 s → 1.75 s to open.
const PROBE_SETTLE: Duration = Duration::from_millis(300);

/// Filler written to an Openterface CDC port (`1a86:fe0c`) whose GET_INFO
/// timed out on Linux (#238): the longest frame the protocol can advertise
/// (`57 AB`, addr, cmd, len 255, 255 data bytes, sum).
///
/// On a cold open the Linux tty echoes the device's startup output back to it
/// before raw mode takes effect. The firmware parser (`CH9329_DataParser` in
/// TechxArtisanStudio/Openterface_KM) then holds a malformed frame and waits,
/// with no timeout, for the length its header advertised, so every later
/// command is swallowed. This many filler bytes completes any such frame,
/// which then fails its checksum and is skipped; the parser drops leftover
/// filler one byte at a time while hunting for the next `57 AB` header. Zero
/// can never form that header. Measured on a cold-replugged Mini-KVM V2:
/// GET_INFO silent before the filler, answered right after it.
#[cfg(any(target_os = "linux", test))]
const PARSER_FLUSH_LEN: usize = 261;

/// How long a key is held before release on a `tap`/`combo`.
const HOLD: Duration = Duration::from_millis(30);
/// Hold used instead of [`HOLD`] when a tap/combo involves a lock key
/// (Caps/Num/Scroll Lock). macOS debounces Caps Lock — a tap shorter than its
/// threshold is discarded by the host before it changes state, and the chip
/// still ACKs, so the drop is silent. Measured against a macOS 15 target over
/// an Openterface: 30 ms dropped, 60 ms registered; 200 ms toggled 10/10 and
/// keeps margin for slower hosts.
const LOCK_HOLD: Duration = Duration::from_millis(200);
/// How long a mouse button is held during a `click`. Much longer than a
/// keypress: the target's input layer must sample the button-down and
/// button-up as distinct events across the serial→USB→OS chain, and 12 ms was
/// too brief to register a click on a Raspberry Pi OS desktop.
const CLICK_HOLD: Duration = Duration::from_millis(80);
/// Settle delay after positioning the cursor before pressing a button. Lets
/// the target process the absolute-pointer motion first, so the button event
/// is attributed to the new location (without it, motion+press arrive together
/// and libinput drops the click).
const CLICK_SETTLE: Duration = Duration::from_millis(60);
/// Per-character hold/pacing for `type`.
const TYPE_GAP: Duration = Duration::from_millis(15);
/// Longest single sleep inside a multi-report command. A longer wait is taken
/// in slices of this size so a cancellation is noticed within about this long
/// (#328).
const CANCEL_SLICE: Duration = Duration::from_millis(20);

/// Error text of a command stopped because its requester went away. The
/// daemon matches it with [`is_cancelled`]; it is deliberately not shaped like
/// a transport error, so a cancellation never makes the owner reopen the link.
pub const CANCELLED: &str = "cancelled: client went away";

/// Whether an error message is the [`CANCELLED`] outcome.
pub fn is_cancelled(msg: &str) -> bool {
    msg.starts_with("cancelled:")
}

/// A cheap "has the requester gone away?" probe, polled between HID reports
/// and inside long sleeps. The daemon passes one that checks its reply
/// channel; the direct (no-daemon) path passes [`never_cancelled`].
pub type Cancel<'a> = &'a dyn Fn() -> bool;

/// A [`Cancel`] that never fires: the one-shot CLI path has no requester that
/// can go away mid-command.
pub fn never_cancelled() -> bool {
    false
}

/// Sleep for `total` in [`CANCEL_SLICE`] pieces, stopping early when `cancel`
/// fires. Returns true when it was cancelled.
fn sleep_unless_cancelled(total: Duration, cancel: Cancel<'_>) -> bool {
    let mut left = total;
    while !left.is_zero() {
        if cancel() {
            return true;
        }
        let step = left.min(CANCEL_SLICE);
        sleep(step);
        left -= step;
    }
    cancel()
}

fn cancelled_error() -> anyhow::Error {
    anyhow!(CANCELLED)
}

/// Hold duration for a tap/chord: [`LOCK_HOLD`] if any key is a lock key,
/// [`HOLD`] otherwise.
/// Whether a reply's payload begins with real data rather than an ACK status
/// byte.
///
/// Most ACKs lead with a status byte that is non-zero on failure. Three
/// commands do not: GET_INFO and GET_PARA_CFG return data blocks, and
/// USB_SWITCH returns the *resulting* mux position — where `0x01` means "now
/// on the target side", not "error 1". Reading that as a status would make
/// every successful switch to the target report a failure.
fn reply_payload_is_data(cmd: u8) -> bool {
    matches!(cmd, CMD_GET_INFO | CMD_GET_PARA_CFG | CMD_USB_SWITCH)
}

fn hold_for(keys: &[Key]) -> Duration {
    if keys.iter().any(|k| k.is_lock()) {
        LOCK_HOLD
    } else {
        HOLD
    }
}

/// How many of the [`KEY_SLOTS`] key-report slots `chord` would need,
/// counting keys already held (deduplicated) plus the chord's new usages
/// (deduplicated against each other and against what is already held).
/// Modifiers do not count — they have their own report byte.
fn keys_needed(chord: &[Key], held: &[u8]) -> usize {
    let mut new_keys: Vec<u8> = Vec::new();
    for &k in chord {
        if let Key::Usage(u) = k {
            if !new_keys.contains(&u) && !held.contains(&u) {
                new_keys.push(u);
            }
        }
    }
    held.len() + new_keys.len()
}

/// Refuse a rate the chip cannot store, before anything touches it.
pub fn validate_baud(rate: u32) -> Result<()> {
    if !(BAUD_MIN..=BAUD_MAX).contains(&rate) {
        bail!("baud {rate} is outside the CH9329's {BAUD_MIN}..={BAUD_MAX} range");
    }
    Ok(())
}

/// Bound a relative `move`/`scroll` total to [`MAX_REL_TOTAL`] per call.
fn clamp_rel_total(v: i32) -> i32 {
    v.clamp(-MAX_REL_TOTAL, MAX_REL_TOTAL)
}

/// Refuse `type` text longer than [`MAX_TYPE_CHARS`] rather than truncate it.
fn check_type_len(text: &str) -> Result<()> {
    let n = text.chars().count();
    if n > MAX_TYPE_CHARS {
        bail!("type text is {n} characters; the limit is {MAX_TYPE_CHARS}");
    }
    Ok(())
}

/// The probe order for a reopen: `first` (the rate the previous session ran
/// at), then the defaults it is not already among.
fn probe_order(first: Option<u32>) -> Vec<u32> {
    let mut order: Vec<u32> = first.into_iter().collect();
    order.extend(
        BAUD_CANDIDATES
            .iter()
            .copied()
            .filter(|b| Some(*b) != first),
    );
    order
}

/// Whether an error message from [`Session::send`]/[`Session::read_reply`] is
/// the specific "no reply within the timeout" case, as opposed to a NAK, a
/// checksum mismatch, or a write failure. Only this case means "the chip does
/// not implement this command" for [`Session::usb_cmd`]'s reply-timeout gloss,
/// and the only failure that earns an Openterface CDC parser flush on open.
fn is_reply_timeout(msg: &str) -> bool {
    msg.starts_with("timed out")
}

fn button_mask(name: &str) -> Result<u8> {
    match name.to_ascii_lowercase().as_str() {
        "left" => Ok(0x01),
        "right" => Ok(0x02),
        "middle" => Ok(0x04),
        other => Err(anyhow!("unknown mouse button: {other}")),
    }
}

/// GET_INFO reply (`docs/ch9329-spec.md` §3).
#[derive(Debug, Clone, Copy)]
pub struct Info {
    pub chip_version: u8,
    pub target_connected: bool,
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}

/// Which side of the USB mux the shared device is attached to. The mux is
/// exclusive: the microSD reader is visible to the host or the target, never
/// both, and the position does not survive the device losing power (it returns
/// to [`MuxSide::Host`]), so it must be read rather than remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxSide {
    Host,
    Target,
}

impl MuxSide {
    pub fn as_str(self) -> &'static str {
        match self {
            MuxSide::Host => "host",
            MuxSide::Target => "target",
        }
    }

    fn selector(self) -> u8 {
        match self {
            MuxSide::Host => USB_SWITCH_TO_HOST,
            MuxSide::Target => USB_SWITCH_TO_TARGET,
        }
    }

    fn from_status(b: u8) -> Option<MuxSide> {
        match b {
            USB_SWITCH_TO_HOST => Some(MuxSide::Host),
            USB_SWITCH_TO_TARGET => Some(MuxSide::Target),
            _ => None,
        }
    }
}

pub struct Session {
    port: Box<dyn SerialPort>,
    baud: u32,
    // Held report state (the device only remembers its last report). Pointer
    // position is *not* tracked here: clicks go through the relative report
    // and land wherever the OS pointer currently is, set by the last move_abs.
    mods: u8,
    keys: Vec<u8>,
    buttons: u8,
}

/// Identify only the selected USB device, including stable serial-path aliases.
/// Missing metadata is not evidence that an adapter is an Openterface.
#[cfg(any(target_os = "linux", test))]
fn openterface_cdc_port(device: &str, ports: &[serialport::SerialPortInfo]) -> bool {
    let selected = std::fs::canonicalize(device).unwrap_or_else(|_| device.into());
    ports.iter().any(|port| {
        matches!(
            &port.port_type,
            serialport::SerialPortType::UsbPort(usb)
                if usb.vid == 0x1a86 && usb.pid == 0xfe0c
        ) && std::fs::canonicalize(&port.port_name).unwrap_or_else(|_| (&port.port_name).into())
            == selected
    })
}

impl Session {
    /// Open `device` and confirm the CH9329 answers. When `baud` is `None`,
    /// probe [`BAUD_CANDIDATES`] in order; otherwise use the given rate.
    pub fn open(device: &str, baud: Option<u32>) -> Result<Session> {
        let candidates: Vec<u32> = match baud {
            Some(b) => vec![b],
            None => BAUD_CANDIDATES.to_vec(),
        };
        Self::open_candidates(device, &candidates)
    }

    /// [`open`](Self::open) with no forced rate, but probing `first` — the
    /// rate a previous session ran at — before the defaults. After an adapter
    /// replug or a target power cycle the chip is almost always still at it,
    /// so the daemon's reopen skips the failed probes (and their settle
    /// delays) that autodetect from scratch would pay.
    pub fn open_preferring(device: &str, first: Option<u32>) -> Result<Session> {
        Self::open_candidates(device, &probe_order(first))
    }

    fn open_candidates(device: &str, candidates: &[u32]) -> Result<Session> {
        let result = Self::try_open_candidates(device, candidates);
        #[cfg(target_os = "linux")]
        let result = result.map_err(|error| {
            if serialport::available_ports().is_ok_and(|ports| openterface_cdc_port(device, &ports))
            {
                return error.context(
                    "warning: Openterface CDC (1a86:fe0c) has a known Linux startup \
                     compatibility issue: tty echo of startup data can leave its \
                     firmware parser unresponsive. ch9329 already sent its parser \
                     flush and it did not help. Firmware release is unknown; \
                     chip_version is not a reliable release number. See \
                     https://panio.dev/hid/#openterface-cdc-on-linux \
                     for diagnosis and the cold-start/direct-USB workaround; verify \
                     board and firmware compatibility before updating",
                );
            }
            error
        });
        result
    }

    fn try_open_candidates(device: &str, candidates: &[u32]) -> Result<Session> {
        #[cfg(target_os = "linux")]
        let openterface_cdc =
            serialport::available_ports().is_ok_and(|ports| openterface_cdc_port(device, &ports));
        let mut last_err: Option<anyhow::Error> = None;
        let last_index = candidates.len() - 1;
        for (i, &rate) in candidates.iter().enumerate() {
            let mut port = serialport::new(device, rate)
                .data_bits(serialport::DataBits::Eight)
                .parity(serialport::Parity::None)
                .stop_bits(serialport::StopBits::One)
                .timeout(Duration::from_millis(500))
                .dtr_on_open(false)
                .open_native()
                .with_context(|| format!("cannot open {device}"))?;
            #[cfg(target_os = "macos")]
            {
                use std::os::unix::io::AsRawFd;
                set_low_read_latency(port.as_raw_fd());
            }
            // The modem lines are not cosmetic on the devices this helper
            // drives. On the KVM-Go's CH32V208, RTS is a hardware reset of the
            // MCU — the vendor's own reset path asserts it for four seconds —
            // and on the Mini-KVM, DTR floats the switchable port's ground
            // return, so an asserted line unplugs whatever is in that port.
            // Deassert both. A brief assertion during open itself cannot be
            // prevented on Linux without kernel changes; it has not been
            // observed to disturb either device.
            port.write_request_to_send(false).ok();
            port.write_data_terminal_ready(false).ok();
            let mut s = Session {
                port: Box::new(port),
                baud: rate,
                mods: 0,
                keys: Vec::new(),
                buttons: 0,
            };
            let info = s.get_info();
            #[cfg(target_os = "linux")]
            let info = match info {
                Err(e) if openterface_cdc && is_reply_timeout(&e.to_string()) => {
                    s.flush_parser_and_retry()
                }
                other => other,
            };
            match info {
                Ok(_) => {
                    // The chip remembers its last HID report independent of
                    // our process; start every session from a clean slate
                    // rather than trusting that nothing was left held by
                    // whatever last had this link open.
                    s.push_all_clear()?;
                    return Ok(s);
                }
                Err(e) => {
                    last_err = Some(e);
                    // Close the port before the settle window so the chip sees
                    // an idle line rather than a held-open handle. Skipped
                    // after the final candidate: nothing follows it, so the
                    // wait would only delay the error.
                    drop(s);
                    if i != last_index {
                        sleep(PROBE_SETTLE);
                    }
                }
            }
        }
        Err(anyhow!(
            "CH9329 did not respond on {device} at {} baud: {}",
            candidates
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join("/"),
            last_err.map(|e| e.to_string()).unwrap_or_default(),
        ))
    }

    pub fn baud(&self) -> u32 {
        self.baud
    }

    /// Clear a stalled Openterface CDC parser with [`PARSER_FLUSH_LEN`] zero
    /// bytes, then ask GET_INFO once more. Run only after a GET_INFO timeout,
    /// so a healthy port never sees the filler.
    #[cfg(any(target_os = "linux", test))]
    fn flush_parser_and_retry(&mut self) -> Result<Info> {
        self.port
            .write_all(&[0u8; PARSER_FLUSH_LEN])
            .map_err(|e| anyhow!("serial write failed: {e}"))?;
        self.get_info()
    }

    // -- framing -------------------------------------------------------------

    fn frame(cmd: u8, data: &[u8]) -> Vec<u8> {
        let mut body = vec![HEAD[0], HEAD[1], ADDR, cmd, data.len() as u8];
        body.extend_from_slice(data);
        let sum = body.iter().fold(0u32, |a, &b| a + b as u32) as u8;
        body.push(sum);
        body
    }

    /// Send a framed command and return the reply payload (data bytes only).
    fn send(&mut self, cmd: u8, data: &[u8]) -> Result<Vec<u8>> {
        self.port.clear(serialport::ClearBuffer::Input).ok();
        let pkt = Self::frame(cmd, data);
        self.port
            .write_all(&pkt)
            .map_err(|e| anyhow!("serial write failed: {e}"))?;
        self.read_reply(cmd)
    }

    fn read_exact(&mut self, n: usize) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = vec![0u8; n];
        let mut filled = 0;
        while filled < n {
            match self.port.read(&mut buf[filled..]) {
                Ok(0) => bail!("serial port closed mid-reply"),
                Ok(k) => filled += k,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    bail!("timed out waiting for CH9329 reply (check device/baud, target on)")
                }
                Err(e) => bail!("serial read failed: {e}"),
            }
        }
        Ok(buf)
    }

    fn read_reply(&mut self, cmd: u8) -> Result<Vec<u8>> {
        let head = self.read_exact(2)?;
        if head != HEAD {
            bail!("bad reply header {head:02x?} (expected 57 ab)");
        }
        let rest = self.read_exact(3)?;
        let (raddr, rcmd, len) = (rest[0], rest[1], rest[2] as usize);
        let payload = self.read_exact(len)?;
        let sum = self.read_exact(1)?[0];
        let expected = (HEAD[0] as u32
            + HEAD[1] as u32
            + raddr as u32
            + rcmd as u32
            + len as u32
            + payload.iter().map(|&b| b as u32).sum::<u32>()) as u8;
        if sum != expected {
            bail!("reply checksum mismatch (got {sum:#04x}, want {expected:#04x})");
        }
        if raddr != ADDR {
            bail!("reply from address {raddr:#04x}, expected {ADDR:#04x}");
        }
        if rcmd == cmd | 0xC0 {
            let status = payload.first().copied().unwrap_or(0xFF);
            bail!("CH9329 rejected cmd {cmd:#04x}: {}", status_name(status));
        }
        if rcmd != cmd | 0x80 {
            bail!("unexpected reply cmd {rcmd:#04x} to {cmd:#04x}");
        }
        if !reply_payload_is_data(cmd) {
            if let Some(&status) = payload.first() {
                if status != 0x00 {
                    bail!("cmd {cmd:#04x} failed: {}", status_name(status));
                }
            }
        }
        Ok(payload)
    }

    // -- status --------------------------------------------------------------

    pub fn get_info(&mut self) -> Result<Info> {
        let p = self.send(CMD_GET_INFO, &[])?;
        if p.len() < 3 {
            bail!("short GET_INFO reply: {p:02x?}");
        }
        Ok(Info {
            chip_version: p[0],
            target_connected: p[1] != 0,
            num_lock: p[2] & 0x01 != 0,
            caps_lock: p[2] & 0x02 != 0,
            scroll_lock: p[2] & 0x04 != 0,
        })
    }

    // -- USB mux -------------------------------------------------------------

    /// Which side the USB mux is on, without changing it.
    pub fn usb_query(&mut self) -> Result<MuxSide> {
        self.usb_cmd(USB_SWITCH_QUERY)
    }

    /// Drive the USB mux to `side`, and confirm it actually landed there.
    ///
    /// The reply carries the *resulting* position rather than a success code,
    /// so a device that ignored the request still answers with a well-formed
    /// frame stating the old position. Comparing the two is the only success
    /// criterion the protocol offers. The operation is idempotent — switching
    /// to the side it is already on is harmless — so callers may retry.
    pub fn usb_set(&mut self, side: MuxSide) -> Result<MuxSide> {
        let got = self.usb_cmd(side.selector())?;
        if got != side {
            bail!(
                "USB mux did not move: asked for {}, device reports {}",
                side.as_str(),
                got.as_str()
            );
        }
        Ok(got)
    }

    fn usb_cmd(&mut self, selector: u8) -> Result<MuxSide> {
        let mut data = [0u8; USB_SWITCH_LEN];
        data[USB_SWITCH_LEN - 1] = selector;
        let p = self.send(CMD_USB_SWITCH, &data).map_err(|e| {
            let msg = e.to_string();
            if is_reply_timeout(&msg) {
                // A device with no switchable USB mux does not answer this
                // command at all — the protocol has no negative ack for an
                // unknown opcode — so a timeout here means unsupported rather
                // than broken. Reworded (rather than just appending a note to
                // the timeout message) so this expected, benign case does not
                // read as transport loss to the daemon's reopen logic
                // (`is_transport_error` in uart.rs), which would otherwise
                // force a full reopen — and on a KVM-Go, a reopen briefly
                // toggles DTR/RTS, which is an MCU reset — every time a
                // mux-less device is asked for its (nonexistent) mux state.
                anyhow!(
                    "this device does not support USB mux switching (no \
                     reply): the protocol has no negative ack for an unknown \
                     opcode, so a timeout here means unsupported rather than \
                     broken"
                )
            } else {
                // A genuine NAK (bad parameter, checksum mismatch, wrong
                // reply address, …) is a real transport/protocol problem —
                // propagate it as-is, with no "unsupported" gloss.
                e
            }
        })?;
        match (p.len(), p.first().copied().and_then(MuxSide::from_status)) {
            (1, Some(side)) => Ok(side),
            _ => bail!("this device does not support USB mux switching (reply {p:02x?})"),
        }
    }

    /// Persistently change the CH9329's serial baud (`docs/ch9329-spec.md` §5).
    ///
    /// Unlike the HID serial protocol's *transient* `baud` renegotiation, the
    /// CH9329 stores its rate in flash: read the 50-byte parameter block, rewrite
    /// only the 4-byte big-endian baud field, `SET_PARA_CFG` (persist), `RESET`
    /// (activate), then reopen the host port at the new rate and confirm with
    /// `GET_INFO`. The reset clears the chip's HID state, so held keys/buttons
    /// are dropped. The datasheet supported range is 1200..=115200 (the
    /// Openterface default is already 115200; a factory chip is 9600); a rate
    /// outside it is refused before the chip is touched.
    ///
    /// Once `SET_PARA_CFG` has succeeded the new rate is in flash, so the
    /// `RESET` acknowledgement is best-effort: the chip may reboot before the
    /// ack leaves the UART, and a lost ack says nothing about the stored rate.
    /// Returns `Ok(None)` when the chip then answers at the new rate,
    /// `Ok(Some(note))` when it does but the ack was lost, and an error only
    /// when the chip does not answer afterwards (the message says the rate is
    /// persisted and how to reconnect).
    pub fn set_baud(&mut self, rate: u32) -> Result<Option<String>> {
        validate_baud(rate)?;
        let mut cfg = self.send(CMD_GET_PARA_CFG, &[])?;
        if cfg.len() != PARA_CFG_LEN {
            bail!(
                "GET_PARA_CFG returned {} bytes, expected {PARA_CFG_LEN}",
                cfg.len()
            );
        }
        // Rewrite only the baud field; preserve working mode, USB IDs, etc.
        cfg[PARA_CFG_BAUD..PARA_CFG_BAUD + 4].copy_from_slice(&rate.to_be_bytes());
        self.send(CMD_SET_PARA_CFG, &cfg)?; // persist to flash (expect 0x89/0x00)
                                            // From here the rate is in flash: record it so a reopen (the daemon's
                                            // transport-error path) probes the right rate first.
        self.baud = rate;
        let reset_ack = self.send(CMD_RESET, &[]); // activate (expect 0x8F/0x00), best-effort

        // The chip reboots at the new rate with its HID state cleared.
        self.mods = 0;
        self.keys.clear();
        self.buttons = 0;
        sleep(Duration::from_millis(400));

        self.port
            .set_baud_rate(rate)
            .map_err(|e| anyhow!("set host port to {rate} baud: {e}"))?;
        sleep(Duration::from_millis(80));

        // First frames after a reset can be lost; retry GET_INFO a few times.
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..3 {
            match self.get_info() {
                Ok(_) => {
                    return Ok(reset_ack.err().map(|e| {
                        format!("the reset ack was lost ({e}) but the chip answers at {rate} baud")
                    }));
                }
                Err(e) => {
                    last = Some(e);
                    sleep(Duration::from_millis(80));
                }
            }
        }
        Err(anyhow!(
            "CH9329 did not respond at {rate} baud after reset (the rate is \
             persisted; reconnect with -b {rate}, or factory-reset via the DEF \
             pin): {}",
            last.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    // -- keyboard ------------------------------------------------------------

    /// Push the current held report (`self.mods` + `self.keys`) to the chip.
    fn push_keyboard(&mut self) -> Result<()> {
        let mut data = vec![self.mods, 0x00];
        let mut slots = self.keys.clone();
        slots.resize(KEY_SLOTS, 0x00);
        data.extend_from_slice(&slots[..KEY_SLOTS]);
        self.send(CMD_KB_GENERAL, &data)?;
        Ok(())
    }

    /// Push an all-zero keyboard report and a zero-button relative mouse
    /// report. Called once a fresh open's `GET_INFO` confirms the chip is
    /// there: the chip remembers its last HID report independent of *our*
    /// process — a daemon that crashed mid-tap and restarted would otherwise
    /// leave the target with a key or button the new session's local state
    /// (starting at all-zero in memory) has no idea is actually held.
    fn push_all_clear(&mut self) -> Result<()> {
        self.push_keyboard()?;
        self.push_mouse_rel(self.buttons, 0, 0, 0)
    }

    /// Tap a key: add it to the held set, push, hold briefly, then restore the
    /// previously-held report. A modifier taps as a held bit (e.g. the GUI key).
    ///
    /// On any error after the press report was written, this makes a
    /// best-effort attempt to restore the previously-held state before
    /// returning — so a transport hiccup mid-tap does not leave `key` stuck
    /// down on the target.
    ///
    /// The hold is taken in slices; if `cancel` fires during it the key is
    /// released at once and the tap reports [`CANCELLED`].
    pub fn tap(&mut self, key: Key, cancel: Cancel<'_>) -> Result<()> {
        self.apply_down(key);
        let pressed = self.push_keyboard();
        let mut cancelled = false;
        if pressed.is_ok() {
            cancelled = sleep_unless_cancelled(hold_for(&[key]), cancel);
        }
        self.apply_up(key);
        let released = self.push_keyboard();
        self.finish_hold(pressed, released)?;
        if cancelled {
            return Err(cancelled_error());
        }
        Ok(())
    }

    /// Chord: press every key together, hold, then release back to held
    /// state. A report has [`KEY_SLOTS`] key slots; a chord needing more —
    /// counting keys already held with `down` — is refused rather than
    /// silently truncated. Modifiers have their own byte and never take a
    /// slot. Errors after the press are handled like [`Session::tap`].
    pub fn combo(&mut self, chord: &[Key], cancel: Cancel<'_>) -> Result<()> {
        let needed = keys_needed(chord, &self.keys);
        if needed > KEY_SLOTS {
            return Err(anyhow!(
                "combo needs {needed} key slots but a report has {KEY_SLOTS} \
                 ({} already held); modifiers do not count",
                self.keys.len()
            ));
        }
        for &k in chord {
            self.apply_down(k);
        }
        let pressed = self.push_keyboard();
        let mut cancelled = false;
        if pressed.is_ok() {
            cancelled = sleep_unless_cancelled(hold_for(chord), cancel);
        }
        for &k in chord {
            self.apply_up(k);
        }
        let released = self.push_keyboard();
        self.finish_hold(pressed, released)?;
        if cancelled {
            return Err(cancelled_error());
        }
        Ok(())
    }

    pub fn key_down(&mut self, key: Key) -> Result<()> {
        self.apply_down(key);
        self.push_keyboard()
    }

    pub fn key_up(&mut self, key: Key) -> Result<()> {
        self.apply_up(key);
        self.push_keyboard()
    }

    pub fn release_all(&mut self) -> Result<()> {
        self.mods = 0;
        self.keys.clear();
        self.push_keyboard()
    }

    /// Everything released: held keys, modifiers, and mouse buttons. Used at
    /// daemon shutdown so the target is not left with something held after
    /// the process that was holding it exits. Never touches the USB mux.
    pub fn release_everything(&mut self) -> Result<()> {
        self.release_all()?;
        self.buttons = 0;
        self.push_mouse_rel(0, 0, 0, 0)
    }

    fn apply_down(&mut self, key: Key) {
        match key {
            Key::Modifier(bit) => self.mods |= bit,
            Key::Usage(u) => {
                if !self.keys.contains(&u) && self.keys.len() < KEY_SLOTS {
                    self.keys.push(u);
                }
            }
        }
    }

    fn apply_up(&mut self, key: Key) {
        match key {
            Key::Modifier(bit) => self.mods &= !bit,
            Key::Usage(u) => self.keys.retain(|&k| k != u),
        }
    }

    /// Reconcile a press/release pair's results, used by [`Session::tap`] and
    /// [`Session::combo`]: on full success, `Ok(())`. If the press was never
    /// confirmed its error stands — the tap did not happen, whatever the
    /// release write did. If the press landed but the release failed, make
    /// one best-effort retry of the release before surfacing its error, so a
    /// single lost reply does not leave the key or chord stuck down.
    fn finish_hold(&mut self, pressed: Result<()>, released: Result<()>) -> Result<()> {
        match (pressed, released) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => {
                let _ = self.push_keyboard();
                Err(e)
            }
        }
    }

    /// Type literal text (US layout) on top of any held modifiers. Text
    /// longer than [`MAX_TYPE_CHARS`] is refused rather than truncated.
    ///
    /// A character whose usage is already held (via `down`) is released
    /// first: pressing it "again" would compose the exact report already in
    /// effect and the target's HID stack would see no new edge, so the
    /// keystroke would silently not register. On any error after a report has
    /// been written, this makes a best-effort attempt to restore the
    /// previously-held state before returning, like [`Session::tap`].
    ///
    /// `cancel` is polled before every character (nothing of this command is
    /// pressed at that point, since each character is released before the next
    /// begins) and inside the pacing sleeps; when it fires the command stops
    /// with [`CANCELLED`] and the held state is exactly what it was before the
    /// command, keys held earlier with `down` included.
    pub fn type_text(&mut self, text: &str, cancel: Cancel<'_>) -> Result<()> {
        check_type_len(text)?;
        let mut prev: u8 = 0;
        for c in text.chars() {
            if cancel() {
                return Err(cancelled_error());
            }
            let (usage, shift) = crate::keys::char_to_usage(c)?;
            let already_held = self.keys.contains(&usage);
            if already_held {
                self.keys.retain(|&k| k != usage);
                let cleared = self.push_keyboard();
                self.keys.push(usage); // restore bookkeeping either way
                cleared?;
                sleep(TYPE_GAP);
            } else if usage == prev {
                // Same key twice needs the release between presses to register.
                sleep(TYPE_GAP);
            }
            let mods = self.mods
                | if shift {
                    crate::keys::MOD_LEFT_SHIFT
                } else {
                    0
                };
            let mut data = vec![mods, 0x00, usage, 0, 0, 0, 0, 0];
            // Keep any other already-held keys alongside the typed one (it is
            // placed separately above, so it is excluded here even though
            // `self.keys` still lists it).
            for (i, &k) in self
                .keys
                .iter()
                .filter(|&&k| k != usage)
                .take(KEY_SLOTS - 1)
                .enumerate()
            {
                data[3 + i] = k;
            }
            let pressed = self.send(CMD_KB_GENERAL, &data).map(|_| ());
            let mut cancelled = false;
            if pressed.is_ok() {
                cancelled = sleep_unless_cancelled(TYPE_GAP, cancel);
            }
            let released = self.push_keyboard(); // restore held state
            prev = usage;
            self.finish_hold(pressed, released)?;
            if cancelled {
                return Err(cancelled_error());
            }
        }
        Ok(())
    }

    // -- mouse ---------------------------------------------------------------

    fn push_mouse_abs(&mut self, x: u16, y: u16, buttons: u8) -> Result<()> {
        let data = [
            0x02,
            buttons,
            (x & 0xFF) as u8,
            (x >> 8) as u8,
            (y & 0xFF) as u8,
            (y >> 8) as u8,
            0x00,
        ];
        self.send(CMD_MS_ABS, &data)?;
        Ok(())
    }

    /// Absolute move; `x`/`y` are panio logical coords in `0..=32767`.
    ///
    /// The CH9329 absolute device coalesces a report whose coordinates equal
    /// its previous one. If the pointer was since moved by a relative report
    /// (a click, a `move`), re-sending the same absolute coordinate would be a
    /// no-op and the cursor would never snap to the target. So nudge one unit
    /// first, then send the exact target — the second report always differs
    /// from the first and lands the cursor precisely on `(x, y)`.
    pub fn move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        let tx = scale_abs(x);
        let ty = scale_abs(y);
        let ny = if ty >= 1 { ty - 1 } else { ty + 1 };
        self.push_mouse_abs(tx, ny, self.buttons)?;
        self.push_mouse_abs(tx, ty, self.buttons)
    }

    pub fn click(&mut self, button: &str, cancel: Cancel<'_>) -> Result<()> {
        let mask = button_mask(button)?;
        // Let any just-issued positioning move land first. Nothing is pressed
        // yet, so a cancellation here just stops.
        if sleep_unless_cancelled(CLICK_SETTLE, cancel) {
            return Err(cancelled_error());
        }
        // Press/release via the RELATIVE report (zero motion). The absolute
        // report reliably positions the pointer, but a same-coordinate abs
        // button transition gets coalesced by libinput and never registers as
        // a click. A relative BTN report always processes, and clicks wherever
        // the pointer currently is — so a prior `moveabs` (even in a separate
        // process invocation) sets the spot.
        self.push_mouse_rel(self.buttons | mask, 0, 0, 0)?;
        let cancelled = sleep_unless_cancelled(CLICK_HOLD, cancel);
        // The release always goes out, cancelled or not.
        self.push_mouse_rel(self.buttons, 0, 0, 0)?;
        if cancelled {
            return Err(cancelled_error());
        }
        Ok(())
    }

    pub fn mouse_down(&mut self, button: &str) -> Result<()> {
        self.buttons |= button_mask(button)?;
        self.push_mouse_rel(self.buttons, 0, 0, 0)
    }

    pub fn mouse_up(&mut self, button: &str) -> Result<()> {
        self.buttons &= !button_mask(button)?;
        self.push_mouse_rel(self.buttons, 0, 0, 0)
    }

    fn push_mouse_rel(&mut self, buttons: u8, dx: i8, dy: i8, wheel: i8) -> Result<()> {
        let data = [0x01, buttons, dx as u8, dy as u8, wheel as u8];
        self.send(CMD_MS_REL, &data)?;
        Ok(())
    }

    /// Relative move, split into per-report int8 deltas. Each axis's total is
    /// clamped to [`MAX_REL_TOTAL`] per call.
    pub fn move_rel(&mut self, dx: i32, dy: i32, cancel: Cancel<'_>) -> Result<()> {
        let (mut dx, mut dy) = (clamp_rel_total(dx), clamp_rel_total(dy));
        loop {
            if cancel() {
                return Err(cancelled_error());
            }
            let sx = dx.clamp(-127, 127);
            let sy = dy.clamp(-127, 127);
            if sx == 0 && sy == 0 {
                break;
            }
            self.push_mouse_rel(self.buttons, sx as i8, sy as i8, 0)?;
            dx -= sx;
            dy -= sy;
            if dx == 0 && dy == 0 {
                break;
            }
            sleep(Duration::from_millis(4));
        }
        Ok(())
    }

    /// Scroll the wheel; positive is up. Split into per-report int8 steps; the
    /// total is clamped to [`MAX_REL_TOTAL`] per call.
    pub fn scroll(&mut self, amount: i32, cancel: Cancel<'_>) -> Result<()> {
        let mut amount = clamp_rel_total(amount);
        while amount != 0 {
            if cancel() {
                return Err(cancelled_error());
            }
            let step = amount.clamp(-127, 127);
            self.push_mouse_rel(self.buttons, 0, 0, step as i8)?;
            amount -= step;
            sleep(Duration::from_millis(4));
        }
        Ok(())
    }
}

/// Map a panio logical coordinate (`0..=32767`) to a CH9329 12-bit
/// coordinate (`0..=4095`), rounded.
fn scale_abs(v: i32) -> u16 {
    let v = v.clamp(0, LOGICAL_MAX as i32) as i64;
    let scaled = (v * ABS_FULL + LOGICAL_MAX / 2) / LOGICAL_MAX;
    scaled.min(ABS_FULL - 1) as u16
}

fn status_name(code: u8) -> String {
    match code {
        0x00 => "success".into(),
        0xE1 => "serial receive timeout (0xE1)".into(),
        0xE2 => "bad frame header (0xE2)".into(),
        0xE3 => "unknown command (0xE3)".into(),
        0xE4 => "checksum mismatch (0xE4)".into(),
        0xE5 => "bad parameter (0xE5)".into(),
        0xE6 => "execution failed (0xE6)".into(),
        other => format!("status {other:#04x}"),
    }
}

/// macOS buffers serial reads behind a data-latency timer (`IOSSDATALAT`)
/// whose default adds well over 100 ms per round trip; drop it to the floor
/// so per-report HID commands stay responsive. No-op elsewhere.
#[cfg(target_os = "macos")]
fn set_low_read_latency(fd: std::os::unix::io::RawFd) {
    const IOSSDATALAT: libc::c_ulong = 0x8008_5400;
    let latency: libc::c_ulong = 1; // microseconds
    unsafe { libc::ioctl(fd, IOSSDATALAT, &latency) };
}

/// Test-only fake of the CH9329 UART, shared crate-wide (`proto.rs` and
/// `uart.rs` also build a [`Session`] on it) so tests can execute real wire
/// behaviour — what gets written, and how a scripted reply is parsed — without
/// hardware. `Session` only ever calls [`Read`], [`Write`] and `clear()`
/// through the `port: Box<dyn SerialPort>` field; every other `SerialPort`
/// method here is a fixed stub the code under test never reaches.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration as StdDuration;

    /// Every buffer passed to [`FakePort::write`], shared with the test so it
    /// can inspect the wire log after the `Session` (and its `Box<dyn
    /// SerialPort>`) has taken ownership of the `FakePort`. `SerialPort`
    /// requires `Send`, so this is `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>`.
    #[derive(Clone, Default)]
    pub(crate) struct WriteLog(Arc<Mutex<Vec<Vec<u8>>>>);

    impl WriteLog {
        pub(crate) fn snapshot(&self) -> Vec<Vec<u8>> {
            self.0.lock().unwrap().clone()
        }
    }

    pub(crate) struct FakePort {
        inbox: VecDeque<u8>,
        log: WriteLog,
        /// Answer every written frame with a success ACK, so a long command
        /// runs to completion without a reply script.
        auto_ack: bool,
    }

    impl FakePort {
        pub(crate) fn new(log: WriteLog) -> Self {
            FakePort {
                inbox: VecDeque::new(),
                log,
                auto_ack: false,
            }
        }

        /// A port that ACKs every frame written to it.
        pub(crate) fn acking(log: WriteLog) -> Self {
            FakePort {
                auto_ack: true,
                ..FakePort::new(log)
            }
        }

        /// Queue raw reply bytes, consumed FIFO by `read()`. An empty inbox
        /// means a read times out, like a CH9329 that never answers.
        pub(crate) fn queue_reply(&mut self, bytes: &[u8]) {
            self.inbox.extend(bytes.iter().copied());
        }
    }

    impl io::Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.inbox.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fake: no reply queued",
                ));
            }
            let n = buf.len().min(self.inbox.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.inbox.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl io::Write for FakePort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.log.0.lock().unwrap().push(buf.to_vec());
            if self.auto_ack && buf.len() > 3 {
                self.inbox.extend(frame_bytes(ADDR, buf[3] | 0x80, &[0x00]));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for FakePort {
        fn name(&self) -> Option<String> {
            None
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(115_200)
        }
        fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
            Ok(serialport::DataBits::Eight)
        }
        fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
            Ok(serialport::FlowControl::None)
        }
        fn parity(&self) -> serialport::Result<serialport::Parity> {
            Ok(serialport::Parity::None)
        }
        fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
            Ok(serialport::StopBits::One)
        }
        fn timeout(&self) -> StdDuration {
            StdDuration::from_millis(500)
        }
        fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _: serialport::FlowControl) -> serialport::Result<()> {
            Ok(())
        }
        fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, _: StdDuration) -> serialport::Result<()> {
            Ok(())
        }
        fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            Ok(self.inbox.len() as u32)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
            Ok(())
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                "FakePort cannot clone",
            ))
        }
        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }
        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    /// Build a `Session` directly on a `FakePort`, bypassing `open()`'s real
    /// serial-port construction and baud probing entirely.
    pub(crate) fn session_on(port: FakePort) -> Session {
        Session {
            port: Box::new(port),
            baud: 115_200,
            mods: 0,
            keys: Vec::new(),
            buttons: 0,
        }
    }

    /// Build a well-formed `[HEAD][ADDR][cmd][len][payload][sum]` reply frame
    /// — an ACK (`cmd | 0x80`) when `payload` is what a success reply
    /// carries, or any other `rcmd`/payload for a scripted edge case.
    pub(crate) fn frame_bytes(raddr: u8, rcmd: u8, payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u8;
        let mut sum: u32 =
            HEAD[0] as u32 + HEAD[1] as u32 + raddr as u32 + rcmd as u32 + len as u32;
        sum += payload.iter().map(|&b| b as u32).sum::<u32>();
        let mut out = vec![HEAD[0], HEAD[1], raddr, rcmd, len];
        out.extend_from_slice(payload);
        out.push(sum as u8);
        out
    }

    /// A success ACK for `cmd`, with `payload` as its data (empty for a plain
    /// status-`0x00` ack).
    pub(crate) fn ack(cmd: u8, payload: &[u8]) -> Vec<u8> {
        frame_bytes(ADDR, cmd | 0x80, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn usb_port(name: &str, vid: u16, pid: u16) -> serialport::SerialPortInfo {
        serialport::SerialPortInfo {
            port_name: name.into(),
            port_type: serialport::SerialPortType::UsbPort(serialport::UsbPortInfo {
                vid,
                pid,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        }
    }

    #[test]
    fn compatibility_hint_requires_selected_usb_identity() {
        let selected = "/nonexistent/ch9329-selected-port";
        let other = "/nonexistent/ch9329-other-port";
        assert!(openterface_cdc_port(
            selected,
            &[usb_port(selected, 0x1a86, 0xfe0c)]
        ));
        for ports in [
            vec![],
            vec![usb_port(other, 0x1a86, 0xfe0c)],
            vec![usb_port(selected, 0x1a86, 0x7523)],
            vec![usb_port(selected, 0xffff, 0xfe0c)],
            vec![serialport::SerialPortInfo {
                port_name: selected.into(),
                port_type: serialport::SerialPortType::Unknown,
            }],
        ] {
            assert!(!openterface_cdc_port(selected, &ports));
        }
    }

    #[cfg(unix)]
    #[test]
    fn compatibility_hint_resolves_stable_port_symlinks() {
        let dir = std::env::temp_dir().join(format!(
            "ch9329-port-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let device = dir.join("device");
        let alias = dir.join("by-path");
        std::fs::write(&device, []).unwrap();
        std::os::unix::fs::symlink(&device, &alias).unwrap();
        let matched = openterface_cdc_port(
            alias.to_str().unwrap(),
            &[usb_port(device.to_str().unwrap(), 0x1a86, 0xfe0c)],
        );
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_file(device).unwrap();
        std::fs::remove_dir(dir).unwrap();
        assert!(matched);
    }

    #[test]
    fn frame_checksum_matches_spec_example() {
        // docs/ch9329-spec.md §1: press 'A' (usage 0x04).
        // 57 AB 00 02 08 00 00 04 00 00 00 00 00 10
        let data = [0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
        let f = Session::frame(CMD_KB_GENERAL, &data);
        assert_eq!(
            f,
            vec![
                0x57, 0xAB, 0x00, 0x02, 0x08, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10
            ]
        );
    }

    #[test]
    fn get_info_frame() {
        // docs/ch9329-spec.md §3: request 57 AB 00 01 00 03.
        assert_eq!(
            Session::frame(CMD_GET_INFO, &[]),
            vec![0x57, 0xAB, 0x00, 0x01, 0x00, 0x03]
        );
    }

    #[test]
    fn baud_field_encoding_matches_spec() {
        // docs/ch9329-spec.md §5: the 4-byte big-endian baud field is
        // 115200 = 00 01 C2 00, 9600 = 00 00 25 80.
        assert_eq!(115_200u32.to_be_bytes(), [0x00, 0x01, 0xC2, 0x00]);
        assert_eq!(9_600u32.to_be_bytes(), [0x00, 0x00, 0x25, 0x80]);
    }

    #[test]
    fn usb_switch_frames_match_hardware() {
        // Exactly the bytes an Openterface KVM-Go accepted on the bench
        // (notes/openterface-usb-mux-spec.md). The four leading zero payload
        // bytes are load-bearing: trimming them moves the checksum and the
        // device rejects the frame.
        let f = |sel| Session::frame(CMD_USB_SWITCH, &[0, 0, 0, 0, sel]);
        assert_eq!(
            f(USB_SWITCH_TO_HOST),
            vec![0x57, 0xAB, 0x00, 0x17, 0x05, 0, 0, 0, 0, 0x00, 0x1E]
        );
        assert_eq!(
            f(USB_SWITCH_TO_TARGET),
            vec![0x57, 0xAB, 0x00, 0x17, 0x05, 0, 0, 0, 0, 0x01, 0x1F]
        );
        assert_eq!(
            f(USB_SWITCH_QUERY),
            vec![0x57, 0xAB, 0x00, 0x17, 0x05, 0, 0, 0, 0, 0x03, 0x21]
        );
    }

    #[test]
    fn usb_switch_reply_payload_is_data_not_status() {
        // The regression this guards: the mux-switch reply's single byte is
        // the resulting position, so a successful switch to the target answers
        // 0x01. Classified as an ACK status byte, that reads as "error 1" and
        // every switch to the target would be reported as a failure.
        assert!(reply_payload_is_data(CMD_USB_SWITCH));
        assert!(reply_payload_is_data(CMD_GET_INFO));
        assert!(reply_payload_is_data(CMD_GET_PARA_CFG));
        assert!(!reply_payload_is_data(CMD_KB_GENERAL));
        assert!(!reply_payload_is_data(CMD_SET_PARA_CFG));
    }

    #[test]
    fn usb_switch_status_decoding() {
        assert_eq!(MuxSide::from_status(0x00), Some(MuxSide::Host));
        assert_eq!(MuxSide::from_status(0x01), Some(MuxSide::Target));
        // 0x03 selects "query" in a *request*; it is never a reply status,
        // and 0x02 is undefined in both directions. Either means the device
        // does not really implement the command.
        assert_eq!(MuxSide::from_status(0x02), None);
        assert_eq!(MuxSide::from_status(0x03), None);
        assert_eq!(MuxSide::from_status(0xFF), None);
    }

    #[test]
    fn usb_switch_selectors_round_trip() {
        assert_eq!(
            MuxSide::from_status(MuxSide::Host.selector()),
            Some(MuxSide::Host)
        );
        assert_eq!(
            MuxSide::from_status(MuxSide::Target.selector()),
            Some(MuxSide::Target)
        );
        assert_eq!(MuxSide::Host.as_str(), "host");
        assert_eq!(MuxSide::Target.as_str(), "target");
    }

    #[test]
    fn baud_validation_matches_the_datasheet_range() {
        assert!(validate_baud(1_200).is_ok());
        assert!(validate_baud(9_600).is_ok());
        assert!(validate_baud(115_200).is_ok());
        assert!(validate_baud(0).is_err());
        assert!(validate_baud(1_199).is_err());
        assert!(validate_baud(115_201).is_err());
        assert!(validate_baud(230_400).is_err());
    }

    #[test]
    fn preferred_rate_is_probed_first_without_duplicates() {
        assert_eq!(probe_order(None), BAUD_CANDIDATES.to_vec());
        assert_eq!(probe_order(Some(9_600)), vec![9_600, 115_200, 57_600]);
        assert_eq!(
            probe_order(Some(38_400)),
            vec![38_400, 115_200, 57_600, 9_600]
        );
    }

    #[test]
    fn relative_totals_are_clamped_per_call() {
        assert_eq!(clamp_rel_total(100), 100);
        assert_eq!(clamp_rel_total(-100), -100);
        assert_eq!(clamp_rel_total(i32::MAX), MAX_REL_TOTAL);
        assert_eq!(clamp_rel_total(i32::MIN), -MAX_REL_TOTAL);
    }

    #[test]
    fn type_text_has_a_ceiling() {
        assert!(check_type_len(&"a".repeat(MAX_TYPE_CHARS)).is_ok());
        assert!(check_type_len(&"a".repeat(MAX_TYPE_CHARS + 1)).is_err());
        // Characters, not bytes: multi-byte text is measured the same way.
        assert!(check_type_len(&"é".repeat(MAX_TYPE_CHARS)).is_ok());
    }

    #[test]
    fn abs_scaling_endpoints() {
        assert_eq!(scale_abs(0), 0);
        assert_eq!(scale_abs(32_767), 4095);
        assert_eq!(scale_abs(16_384), 2048); // midpoint
        assert_eq!(scale_abs(-5), 0); // clamped
        assert_eq!(scale_abs(40_000), 4095); // clamped
    }

    #[test]
    fn keys_needed_dedupes_against_held_and_within_the_chord() {
        let held = [0x04u8, 0x05]; // A, B already held
        let chord = [Key::Usage(0x06), Key::Usage(0x07), Key::Usage(0x08)];
        assert_eq!(keys_needed(&chord, &held), 5); // 2 held + 3 new
                                                   // A key already held doesn't add a slot again.
        let chord = [Key::Usage(0x04), Key::Usage(0x09)];
        assert_eq!(keys_needed(&chord, &held), 3);
        // Duplicate usages within the chord itself count once.
        let chord = [Key::Usage(0x09), Key::Usage(0x09)];
        assert_eq!(keys_needed(&chord, &held), 3);
        // Modifiers never take a slot.
        let chord = [
            Key::Modifier(crate::keys::MOD_LEFT_CONTROL),
            Key::Usage(0x09),
        ];
        assert_eq!(keys_needed(&chord, &held), 3);
        assert_eq!(keys_needed(&[], &[]), 0);
    }

    #[test]
    fn parser_flush_completes_the_longest_frame_then_asks_again() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log.clone());
        port.queue_reply(&ack(CMD_GET_INFO, &[0x02, 0x01, 0x00]));
        let mut s = session_on(port);
        let info = s.flush_parser_and_retry().unwrap();
        assert_eq!(info.chip_version, 0x02);
        let writes = log.snapshot();
        assert_eq!(writes.len(), 2);
        // 57 AB, addr, cmd, len 255, 255 data bytes, sum: any stalled frame
        // is complete after this many bytes, and zero never forms a header.
        assert_eq!(writes[0], vec![0u8; 5 + 255 + 1]);
        assert_eq!(writes[1], Session::frame(CMD_GET_INFO, &[]));
    }

    #[test]
    fn only_a_missing_reply_triggers_the_parser_flush() {
        let mut s = session_on(FakePort::new(WriteLog::default()));
        assert!(is_reply_timeout(&s.get_info().unwrap_err().to_string()));
        let mut port = FakePort::new(WriteLog::default());
        port.queue_reply(&frame_bytes(ADDR, CMD_GET_INFO | 0x80, &[0x02]));
        let mut s = session_on(port);
        assert!(!is_reply_timeout(&s.get_info().unwrap_err().to_string()));
    }

    #[test]
    fn combo_refuses_before_writing_anything_when_it_needs_too_many_slots() {
        let log = WriteLog::default();
        let port = FakePort::new(log.clone());
        let mut s = session_on(port);
        let seven: Vec<Key> = (0x04..0x0b).map(Key::Usage).collect(); // 7 distinct usages
        let err = s.combo(&seven, &never_cancelled).unwrap_err().to_string();
        assert!(err.contains('6'), "{err}");
        assert!(log.snapshot().is_empty(), "refused before any write");
    }

    #[test]
    fn reply_timeout_classification() {
        assert!(is_reply_timeout(
            "timed out waiting for CH9329 reply (check device/baud, target on)"
        ));
        assert!(!is_reply_timeout(
            "CH9329 rejected cmd 0x17: bad parameter (0xe5)"
        ));
        assert!(!is_reply_timeout(
            "reply checksum mismatch (got 0x01, want 0x02)"
        ));
    }

    /// A reply carrying a different address than ours is a protocol problem,
    /// not silently accepted.
    #[test]
    fn reply_from_the_wrong_address_is_rejected() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log);
        port.queue_reply(&frame_bytes(0x01, CMD_GET_INFO | 0x80, &[0x38, 0x01, 0x00]));
        let mut s = session_on(port);
        let err = s.get_info().unwrap_err().to_string();
        assert!(err.contains("0x01"), "{err}");
        assert!(err.contains("expected"), "{err}");
    }

    /// The regression this guards: a mux-less device's silence on
    /// `CMD_USB_SWITCH` must read as "unsupported", not as the generic
    /// transport-loss wording that would make the daemon's reopen logic
    /// (`is_transport_error` in uart.rs) force a reopen — on a KVM-Go, a
    /// reopen blips DTR/RTS, which is an MCU reset.
    #[test]
    fn usb_query_timeout_is_reworded_as_unsupported_not_transport_loss() {
        let log = WriteLog::default();
        let port = FakePort::new(log); // empty inbox: every read times out
        let mut s = session_on(port);
        let err = s.usb_query().unwrap_err().to_string();
        assert!(err.contains("does not support USB mux switching"), "{err}");
        assert!(!err.starts_with("timed out"), "{err}");
    }

    /// A genuine NAK on the same command is a real protocol error and keeps
    /// its own message — no "unsupported" gloss grafted onto it.
    #[test]
    fn usb_query_nak_is_not_reworded_as_unsupported() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log);
        port.queue_reply(&frame_bytes(ADDR, CMD_USB_SWITCH | 0xC0, &[0xE5]));
        let mut s = session_on(port);
        let err = s.usb_query().unwrap_err().to_string();
        assert!(err.contains("rejected"), "{err}");
        assert!(!err.contains("unsupported"), "{err}");
    }

    #[test]
    fn push_all_clear_writes_a_zero_keyboard_and_zero_mouse_report() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log.clone());
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00]));
        port.queue_reply(&ack(CMD_MS_REL, &[0x00]));
        let mut s = session_on(port);
        s.push_all_clear().unwrap();
        let writes = log.snapshot();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0][3], CMD_KB_GENERAL);
        assert_eq!(&writes[0][5..13], &[0u8; 8]);
        assert_eq!(writes[1][3], CMD_MS_REL);
        assert_eq!(&writes[1][5..10], &[0x01, 0, 0, 0, 0]);
    }

    /// A transport hiccup between the press and the release must not leave
    /// the key stuck: `tap` makes a best-effort second attempt at the release
    /// before surfacing the error.
    #[test]
    fn tap_error_after_the_press_still_attempts_a_release() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log.clone());
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00])); // only the press gets a reply
        let mut s = session_on(port);
        let err = s
            .tap(Key::Usage(0x04), &never_cancelled)
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
        let writes = log.snapshot();
        // press, release attempt, and the best-effort retry of the release.
        assert_eq!(writes.len(), 3);
        assert!(writes.iter().all(|w| w[3] == CMD_KB_GENERAL));
        assert_eq!(&writes[1][5..13], &[0u8; 8], "release carries no key");
        assert_eq!(&writes[2][5..13], &[0u8; 8], "retry carries no key");
        assert!(
            s.keys.is_empty(),
            "local state reflects the key as released"
        );
    }

    /// A usage already held via `down` is released before it is pressed
    /// again: otherwise the press report would be byte-identical to the one
    /// already in effect and the target would see no new edge.
    #[test]
    fn typing_an_already_held_usage_releases_before_pressing_again() {
        let log = WriteLog::default();
        let mut port = FakePort::new(log.clone());
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00])); // release
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00])); // press
        port.queue_reply(&ack(CMD_KB_GENERAL, &[0x00])); // restore
        let mut s = session_on(port);
        s.keys.push(0x04); // A already held via `down`
        s.type_text("a", &never_cancelled).unwrap();
        let writes = log.snapshot();
        assert_eq!(writes.len(), 3);
        assert!(
            !writes[0][5..13].contains(&0x04),
            "release drops the usage: {:?}",
            writes[0]
        );
        assert_eq!(
            writes[1][7], 0x04,
            "press carries the usage: {:?}",
            writes[1]
        );
        assert!(
            writes[2][5..13].contains(&0x04),
            "restore holds the usage again: {:?}",
            writes[2]
        );
        assert_eq!(s.keys, vec![0x04]);
    }

    // -- #328: cancellation --------------------------------------------------

    /// A cancel that fires after `n` polls.
    fn cancel_after(n: usize) -> impl Fn() -> bool {
        let polls = std::cell::Cell::new(0usize);
        move || {
            polls.set(polls.get() + 1);
            polls.get() > n
        }
    }

    /// A `tap` cancelled during its hold releases the key at once and reports
    /// the cancellation; a `click` cancelled during its hold releases the
    /// button.
    #[test]
    fn a_cancel_during_a_hold_releases_what_was_pressed() {
        let log = WriteLog::default();
        let mut s = session_on(FakePort::acking(log.clone()));
        let err = s.tap(Key::Usage(0x04), &cancel_after(0)).unwrap_err();
        assert!(is_cancelled(&err.to_string()), "{err}");
        let writes = log.snapshot();
        assert_eq!(writes.len(), 2, "press then release");
        assert_eq!(writes[0][7], 0x04);
        assert_eq!(&writes[1][5..13], &[0u8; 8]);
        assert!(s.keys.is_empty());

        let log = WriteLog::default();
        let mut s = session_on(FakePort::acking(log.clone()));
        // Survives the settle sleep's polls, then fires during the hold.
        let err = s.click("left", &cancel_after(4)).unwrap_err();
        assert!(is_cancelled(&err.to_string()), "{err}");
        let writes = log.snapshot();
        assert_eq!(writes.len(), 2, "press then release");
        assert_eq!(writes[0][6], 1, "left button down");
        assert_eq!(writes[1][6], 0, "button released");
    }

    /// A cancel before a `click`'s press stops without pressing anything.
    #[test]
    fn a_cancel_before_a_click_presses_nothing() {
        let log = WriteLog::default();
        let mut s = session_on(FakePort::acking(log.clone()));
        assert!(s.click("left", &|| true).is_err());
        assert!(log.snapshot().is_empty());
    }

    /// `move` and `scroll` stop between reports.
    #[test]
    fn a_cancel_stops_move_and_scroll_between_reports() {
        let log = WriteLog::default();
        let mut s = session_on(FakePort::acking(log.clone()));
        assert!(s.move_rel(5000, 0, &cancel_after(2)).is_err());
        assert_eq!(log.snapshot().len(), 2);
        let log = WriteLog::default();
        let mut s = session_on(FakePort::acking(log.clone()));
        assert!(s.scroll(5000, &cancel_after(3)).is_err());
        assert_eq!(log.snapshot().len(), 3);
    }

    /// `sleep_unless_cancelled` notices a cancellation inside a long sleep.
    #[test]
    fn a_long_sleep_is_taken_in_cancellable_slices() {
        let start = std::time::Instant::now();
        assert!(sleep_unless_cancelled(
            Duration::from_secs(10),
            &cancel_after(3)
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
