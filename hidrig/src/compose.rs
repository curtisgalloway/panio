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

//! HID composition for the dual-board "dumb pipe" rig.
//!
//! The target board no longer interprets HID semantics; it relays raw report
//! bytes to `send_report`. So this module owns what the single-board firmware
//! used to do: turn the v1 ASCII commands into HID report bytes and wrap them
//! in the binary frames the control board relays over I2C.
//!
//! Frame format (matches the paniolo-hardware firmware,
//! <https://github.com/curtisgalloway/paniolo-hardware/tree/main/hidrig-kb2040/firmware>):
//! ```text
//! [type][b1][len][payload .. len bytes]
//!   0x01  rid  N    N report bytes  (rid 1 = keyboard / 8 B, 2 = abs mouse / 6 B)
//!   0x02  cmd  N    N arg bytes     (cmd 1 = ping, 2 = version, 3 = power)
//! ```
//! Reports match the descriptor in paniolo-hardware's
//! `hidrig-kb2040/firmware/target/boot.py`:
//! keyboard report id 1 (`[modifier, 0, k1..k6]`), absolute pointer report id 2
//! (`[buttons, x_lo, x_hi, y_lo, y_hi, wheel]`, axes 0..=32767). A descriptor
//! change there requires a matching change here.

use anyhow::{anyhow, Result};

pub const F_HID: u8 = 0x01;
pub const F_CTRL: u8 = 0x02;
pub const RID_KBD: u8 = 1;
pub const RID_MOUSE: u8 = 2;
pub const CMD_PING: u8 = 1;
pub const CMD_VERSION: u8 = 2;
pub const CMD_POWER: u8 = 3;

/// Power-relay actions, carried as payload byte 0 of a `power` control frame.
const POWER_OFF: u8 = 0;
const POWER_ON: u8 = 1;
const POWER_CYCLE: u8 = 2;

/// Absolute-pointer logical maximum (`moveabs` axis range is `0..=ABS_MAX`).
pub const ABS_MAX: i32 = 32_767;

/// Key slots in the keyboard report (the boot-protocol six).
const KEY_SLOTS: usize = 6;

/// Ceiling on the text of one `type` command. A command line is one line of
/// input, so 4 KiB is generous; the cap keeps one request from queueing hours
/// of keystrokes (two frames per character) ahead of every other client.
pub const MAX_TYPE_CHARS: usize = 4096;

/// Ceiling on one `scroll` call's total travel. The wheel field is one signed
/// byte per report, so larger amounts are split across reports; without a
/// ceiling `scroll 2000000000` would queue sixteen million frames ahead of
/// every other client. Matches ch9329's per-call relative limit.
pub const MAX_SCROLL_TOTAL: i32 = 32_767;

/// A composed wire frame ready to send to the control board.
pub type Frame = Vec<u8>;

fn frame(ftype: u8, b1: u8, payload: &[u8]) -> Frame {
    let mut f = Vec::with_capacity(3 + payload.len());
    f.push(ftype);
    f.push(b1);
    f.push(payload.len() as u8);
    f.extend_from_slice(payload);
    f
}

fn clamp_abs(v: i32) -> i32 {
    v.clamp(0, ABS_MAX)
}

/// A named key resolves to either a modifier bit (byte 0 of the report) or a
/// keycode that occupies one of the six key slots.
enum KeyKind {
    Modifier(u8),
    Key(u8),
}

/// Map an adafruit_hid Keycode name (and common aliases) to a HID usage.
fn key_kind(name: &str) -> Result<KeyKind> {
    let n = name.to_ascii_uppercase();
    // Modifiers occupy the report's modifier byte, not a key slot.
    let modifier = match n.as_str() {
        "LEFT_CONTROL" | "CONTROL" | "CTRL" | "LCTRL" => Some(0x01),
        "LEFT_SHIFT" | "SHIFT" | "LSHIFT" => Some(0x02),
        "LEFT_ALT" | "ALT" | "OPTION" | "LALT" => Some(0x04),
        "LEFT_GUI" | "GUI" | "CMD" | "COMMAND" | "WINDOWS" | "SUPER" | "LGUI" => Some(0x08),
        "RIGHT_CONTROL" | "RCTRL" => Some(0x10),
        "RIGHT_SHIFT" | "RSHIFT" => Some(0x20),
        "RIGHT_ALT" | "ALT_GR" | "RALT" => Some(0x40),
        "RIGHT_GUI" | "RGUI" => Some(0x80),
        _ => None,
    };
    if let Some(bit) = modifier {
        return Ok(KeyKind::Modifier(bit));
    }
    let usage = named_key(&n).ok_or_else(|| anyhow!("unknown key: {name}"))?;
    Ok(KeyKind::Key(usage))
}

/// Non-modifier named keys → HID usage id (Keyboard/Keypad usage page).
fn named_key(n: &str) -> Option<u8> {
    // Letters and the digit-name words (ZERO..NINE) and named keys.
    if n.len() == 1 {
        let c = n.as_bytes()[0];
        if c.is_ascii_uppercase() {
            return Some(0x04 + (c - b'A'));
        }
        if c.is_ascii_digit() {
            return Some(if c == b'0' { 0x27 } else { 0x1e + (c - b'1') });
        }
    }
    let words = [
        ("ZERO", 0x27),
        ("ONE", 0x1e),
        ("TWO", 0x1f),
        ("THREE", 0x20),
        ("FOUR", 0x21),
        ("FIVE", 0x22),
        ("SIX", 0x23),
        ("SEVEN", 0x24),
        ("EIGHT", 0x25),
        ("NINE", 0x26),
        ("ENTER", 0x28),
        ("RETURN", 0x28),
        ("ESCAPE", 0x29),
        ("ESC", 0x29),
        ("BACKSPACE", 0x2a),
        ("TAB", 0x2b),
        ("SPACE", 0x2c),
        ("SPACEBAR", 0x2c),
        ("MINUS", 0x2d),
        ("EQUALS", 0x2e),
        ("LEFT_BRACKET", 0x2f),
        ("RIGHT_BRACKET", 0x30),
        ("BACKSLASH", 0x31),
        ("SEMICOLON", 0x33),
        ("QUOTE", 0x34),
        ("GRAVE_ACCENT", 0x35),
        ("COMMA", 0x36),
        ("PERIOD", 0x37),
        ("FORWARD_SLASH", 0x38),
        ("CAPS_LOCK", 0x39),
        ("F1", 0x3a),
        ("F2", 0x3b),
        ("F3", 0x3c),
        ("F4", 0x3d),
        ("F5", 0x3e),
        ("F6", 0x3f),
        ("F7", 0x40),
        ("F8", 0x41),
        ("F9", 0x42),
        ("F10", 0x43),
        ("F11", 0x44),
        ("F12", 0x45),
        ("INSERT", 0x49),
        ("HOME", 0x4a),
        ("PAGE_UP", 0x4b),
        ("DELETE", 0x4c),
        ("END", 0x4d),
        ("PAGE_DOWN", 0x4e),
        ("RIGHT_ARROW", 0x4f),
        ("LEFT_ARROW", 0x50),
        ("DOWN_ARROW", 0x51),
        ("UP_ARROW", 0x52),
        // Lock, system and menu keys — the same names and usage ids as
        // ch9329/src/keys.rs, so a sequence file runs on either helper.
        ("PRINT_SCREEN", 0x46),
        ("SCROLL_LOCK", 0x47),
        ("PAUSE", 0x48),
        ("NUM_LOCK", 0x53),
        ("KEYPAD_NUMLOCK", 0x53),
        ("APPLICATION", 0x65),
        ("MENU", 0x65),
    ];
    words.iter().find(|(name, _)| *name == n).map(|(_, u)| *u)
}

/// US-keyboard layout: printable ASCII → (usage, needs-shift). A newline is
/// deliberately not here: the command line is the frame, so `type` text can
/// never carry one ([`Composer::dispatch`] refuses it) — `key ENTER` presses
/// Enter.
fn char_to_key(c: char) -> Option<(u8, bool)> {
    Some(match c {
        'a'..='z' => (0x04 + (c as u8 - b'a'), false),
        'A'..='Z' => (0x04 + (c as u8 - b'A'), true),
        '1'..='9' => (0x1e + (c as u8 - b'1'), false),
        '0' => (0x27, false),
        ' ' => (0x2c, false),
        '\t' => (0x2b, false),
        '!' => (0x1e, true),
        '@' => (0x1f, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),
        '-' => (0x2d, false),
        '_' => (0x2d, true),
        '=' => (0x2e, false),
        '+' => (0x2e, true),
        '[' => (0x2f, false),
        '{' => (0x2f, true),
        ']' => (0x30, false),
        '}' => (0x30, true),
        '\\' => (0x31, false),
        '|' => (0x31, true),
        ';' => (0x33, false),
        ':' => (0x33, true),
        '\'' => (0x34, false),
        '"' => (0x34, true),
        '`' => (0x35, false),
        '~' => (0x35, true),
        ',' => (0x36, false),
        '<' => (0x36, true),
        '.' => (0x37, false),
        '>' => (0x37, true),
        '/' => (0x38, false),
        '?' => (0x38, true),
        _ => return None,
    })
}

fn button_bit(name: &str) -> Result<u8> {
    match name.to_ascii_lowercase().as_str() {
        "left" => Ok(1),
        "right" => Ok(2),
        "middle" => Ok(4),
        _ => Err(anyhow!("unknown button: {name}")),
    }
}

/// Stateful HID composer. Tracks held modifiers/keys (for `down`/`up`/`combo`),
/// the virtual absolute cursor (so relative `move` and `click` keep position),
/// and held mouse buttons. One instance lives in the daemon so this state
/// persists across one-shot commands.
pub struct Composer {
    mx: i32,
    my: i32,
    buttons: u8,
    held_mods: u8,
    held_keys: Vec<u8>,
}

impl Default for Composer {
    fn default() -> Self {
        Composer {
            mx: ABS_MAX / 2,
            my: ABS_MAX / 2,
            buttons: 0,
            held_mods: 0,
            held_keys: Vec::new(),
        }
    }
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A keyboard frame from the held state plus transient extra mods/keys.
    fn kbd(&self, extra_mods: u8, extra_keys: &[u8]) -> Frame {
        let mut report = [0u8; 8];
        report[0] = self.held_mods | extra_mods;
        for (slot, &k) in self
            .held_keys
            .iter()
            .chain(extra_keys)
            .take(KEY_SLOTS)
            .enumerate()
        {
            report[2 + slot] = k;
        }
        frame(F_HID, RID_KBD, &report)
    }

    /// An absolute-pointer frame at the current cursor + button state.
    fn mouse(&self, wheel: i8) -> Frame {
        let x = clamp_abs(self.mx) as u16;
        let y = clamp_abs(self.my) as u16;
        let payload = [
            self.buttons & 0x07,
            (x & 0xff) as u8,
            (x >> 8) as u8,
            (y & 0xff) as u8,
            (y >> 8) as u8,
            wheel as u8,
        ];
        frame(F_HID, RID_MOUSE, &payload)
    }

    /// `type <text>`: tap each character (press then release). Text longer
    /// than [`MAX_TYPE_CHARS`] is refused rather than truncated, and text the
    /// US layout cannot produce is refused rather than typed with holes —
    /// nothing is composed for a refused string, so nothing partial is typed.
    pub fn type_text(&self, text: &str) -> Result<Vec<Frame>> {
        let n = text.chars().count();
        if n > MAX_TYPE_CHARS {
            return Err(anyhow!(
                "type text is {n} characters; the limit is {MAX_TYPE_CHARS}"
            ));
        }
        let mut out = Vec::with_capacity(2 * n);
        for c in text.chars() {
            let (usage, shift) = char_to_key(c).ok_or_else(|| {
                anyhow!("cannot type character {c:?} (US layout only; use `key <NAME>` for a named key)")
            })?;
            let m = if shift { 0x02 } else { 0 };
            out.push(self.kbd(m, &[usage]));
            out.push(self.kbd(0, &[]));
        }
        Ok(out)
    }

    /// `key <NAME>`: tap one key on top of whatever is held.
    pub fn key(&self, name: &str) -> Result<Vec<Frame>> {
        Ok(match key_kind(name)? {
            KeyKind::Modifier(bit) => vec![self.kbd(bit, &[]), self.kbd(0, &[])],
            KeyKind::Key(u) => vec![self.kbd(0, &[u]), self.kbd(0, &[])],
        })
    }

    /// `combo <NAME>...`: press all the named keys together, then release.
    /// A report has [`KEY_SLOTS`] key slots; a chord needing more (counting
    /// keys already held with `down`) is refused rather than silently
    /// truncated. Modifiers have their own byte and never take a slot.
    pub fn combo(&self, names: &[String]) -> Result<Vec<Frame>> {
        let mut mods = 0u8;
        let mut keys: Vec<u8> = Vec::new();
        for n in names {
            match key_kind(n)? {
                KeyKind::Modifier(bit) => mods |= bit,
                KeyKind::Key(u) => {
                    if !keys.contains(&u) && !self.held_keys.contains(&u) {
                        keys.push(u);
                    }
                }
            }
        }
        let needed = self.held_keys.len() + keys.len();
        if needed > KEY_SLOTS {
            return Err(anyhow!(
                "combo needs {needed} key slots but a report has {KEY_SLOTS} \
                 ({} already held); modifiers do not count",
                self.held_keys.len()
            ));
        }
        Ok(vec![self.kbd(mods, &keys), self.kbd(0, &[])])
    }

    /// `down <NAME>`: press and hold.
    pub fn down(&mut self, name: &str) -> Result<Vec<Frame>> {
        match key_kind(name)? {
            KeyKind::Modifier(bit) => self.held_mods |= bit,
            KeyKind::Key(u) => {
                if !self.held_keys.contains(&u) {
                    self.held_keys.push(u);
                }
            }
        }
        Ok(vec![self.kbd(0, &[])])
    }

    /// `up <NAME>`: release a held key.
    pub fn up(&mut self, name: &str) -> Result<Vec<Frame>> {
        match key_kind(name)? {
            KeyKind::Modifier(bit) => self.held_mods &= !bit,
            KeyKind::Key(u) => self.held_keys.retain(|&k| k != u),
        }
        Ok(vec![self.kbd(0, &[])])
    }

    /// `releaseall`: drop all held keys and modifiers.
    pub fn releaseall(&mut self) -> Vec<Frame> {
        self.held_mods = 0;
        self.held_keys.clear();
        vec![self.kbd(0, &[])]
    }

    /// `moveabs <x> <y>`: set the cursor to an absolute logical position.
    pub fn moveabs(&mut self, x: i32, y: i32) -> Vec<Frame> {
        self.mx = clamp_abs(x);
        self.my = clamp_abs(y);
        vec![self.mouse(0)]
    }

    /// `move <dx> <dy>`: accumulate a relative move into the virtual cursor.
    /// The sum saturates: a delta near `i32::MAX` clamps to the edge instead
    /// of overflowing (a panic in a debug build, a wrap to the far corner in
    /// release).
    pub fn move_rel(&mut self, dx: i32, dy: i32) -> Vec<Frame> {
        self.mx = clamp_abs(self.mx.saturating_add(dx));
        self.my = clamp_abs(self.my.saturating_add(dy));
        vec![self.mouse(0)]
    }

    /// `click <button>`: press then release a button at the current position.
    pub fn click(&mut self, button: &str) -> Result<Vec<Frame>> {
        let b = button_bit(button)?;
        self.buttons |= b;
        let press = self.mouse(0);
        self.buttons &= !b;
        let release = self.mouse(0);
        Ok(vec![press, release])
    }

    /// `mdown <button>`: press and hold a button.
    pub fn mdown(&mut self, button: &str) -> Result<Vec<Frame>> {
        self.buttons |= button_bit(button)?;
        Ok(vec![self.mouse(0)])
    }

    /// `mup <button>`: release a held button.
    pub fn mup(&mut self, button: &str) -> Result<Vec<Frame>> {
        self.buttons &= !button_bit(button)?;
        Ok(vec![self.mouse(0)])
    }

    /// `scroll <amount>`: wheel notches at the current position. The wheel
    /// field is one signed byte, so the amount is split across reports
    /// (protocol §3); the per-call total is capped at [`MAX_SCROLL_TOTAL`].
    pub fn scroll(&self, amount: i32) -> Vec<Frame> {
        let mut left = amount.clamp(-MAX_SCROLL_TOTAL, MAX_SCROLL_TOTAL);
        let mut out = Vec::new();
        while left != 0 {
            let step = left.clamp(-127, 127);
            out.push(self.mouse(step as i8));
            left -= step;
        }
        out
    }

    /// Everything released: no keys, no modifiers, no buttons. The daemon
    /// sends this at shutdown so the target is not left with a key or button
    /// down after the process that was holding it exits.
    pub fn release_everything(&mut self) -> Vec<Frame> {
        self.held_mods = 0;
        self.held_keys.clear();
        self.buttons = 0;
        vec![self.kbd(0, &[]), self.mouse(0)]
    }

    /// `ping`: a control frame (the control board answers).
    pub fn ping(&self) -> Frame {
        frame(F_CTRL, CMD_PING, &[])
    }

    /// `version`: a control frame (the control board answers with its id).
    pub fn version(&self) -> Frame {
        frame(F_CTRL, CMD_VERSION, &[])
    }

    /// `power off|on|cycle [secs]`: a control frame the control board acts on by
    /// driving the DUT power relay (a hard-cut load switch). `secs` applies only
    /// to `cycle` — the off-time before power returns; omitted/0 uses the
    /// firmware default. The board answers with an ack.
    pub fn power(&self, action: &str, secs: Option<u8>) -> Result<Frame> {
        let a = match action.to_ascii_lowercase().as_str() {
            "off" => POWER_OFF,
            "on" => POWER_ON,
            "cycle" => POWER_CYCLE,
            other => return Err(anyhow!("unknown power action: {other} (use off|on|cycle)")),
        };
        let payload: Vec<u8> = match secs {
            Some(s) if a == POWER_CYCLE => vec![a, s],
            _ => vec![a],
        };
        Ok(frame(F_CTRL, CMD_POWER, &payload))
    }

    /// Compose one v1 ASCII command line into the frames to send. The single
    /// composition entry point shared by the one-shot CLI and the daemon (it
    /// replaces the firmware's `handle_line`).
    ///
    /// The line is the frame: one trailing line terminator is stripped, and a
    /// CR/LF anywhere else is refused rather than typed as Enter — the daemon's
    /// queue refuses it too, so the direct path agrees. `type` text is the
    /// remainder after the one separator, verbatim (trailing spaces included);
    /// every other verb takes whitespace-separated tokens.
    pub fn dispatch(&mut self, line: &str) -> Result<Vec<Frame>> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.contains(['\r', '\n']) {
            return Err(anyhow!("command contains a newline: {line:?}"));
        }
        let line = line.trim_start();
        let (head, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let args = rest.trim();
        let button = || if args.is_empty() { "left" } else { args };
        match head.to_ascii_lowercase().as_str() {
            "type" => self.type_text(rest),
            "key" => self.key(args),
            "combo" => self.combo(
                &args
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            ),
            "down" => self.down(args),
            "up" => self.up(args),
            "releaseall" => Ok(self.releaseall()),
            "move" => {
                let (dx, dy) = two_ints(args)?;
                Ok(self.move_rel(dx, dy))
            }
            "moveabs" => {
                let (x, y) = two_ints(args)?;
                Ok(self.moveabs(x, y))
            }
            "click" => self.click(button()),
            "mdown" => self.mdown(button()),
            "mup" => self.mup(button()),
            "scroll" => {
                let n = args
                    .parse()
                    .map_err(|_| anyhow!("scroll needs an integer: {args:?}"))?;
                Ok(self.scroll(n))
            }
            "ping" => Ok(vec![self.ping()]),
            "version" => Ok(vec![self.version()]),
            "power" => {
                let mut it = args.split_whitespace();
                let action = it
                    .next()
                    .ok_or_else(|| anyhow!("power needs an action: off|on|cycle"))?;
                let secs = it
                    .next()
                    .map(|t| t.parse::<u8>())
                    .transpose()
                    .map_err(|_| anyhow!("power cycle seconds must be 0..=255"))?;
                Ok(vec![self.power(action, secs)?])
            }
            "" => Ok(Vec::new()),
            other => Err(anyhow!("unknown command: {other}")),
        }
    }
}

fn two_ints(s: &str) -> Result<(i32, i32)> {
    let mut it = s.split_whitespace();
    match (
        it.next().and_then(|t| t.parse().ok()),
        it.next().and_then(|t| t.parse().ok()),
    ) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => Err(anyhow!("expected two integers: {s:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_frame_encodes_axes_little_endian() {
        let mut c = Composer::new();
        // 0x1234 = 4660; lo=0x34 hi=0x12.
        let f = c.moveabs(0x1234, 0x0056).pop().unwrap();
        assert_eq!(
            f,
            vec![0x01, 0x02, 0x06, 0x00, 0x34, 0x12, 0x56, 0x00, 0x00]
        );
    }

    #[test]
    fn moveabs_clamps_to_range() {
        let mut c = Composer::new();
        let f = c.moveabs(99_999, -5).pop().unwrap();
        // x clamps to 32767 = 0x7fff, y clamps to 0.
        assert_eq!(&f[3..], &[0x00, 0xff, 0x7f, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn type_uppercase_uses_shift() {
        let c = Composer::new();
        let frames = c.type_text("A").unwrap();
        // press: shift modifier + 'a' usage 0x04; release: all zero.
        assert_eq!(
            frames[0],
            vec![0x01, 0x01, 0x08, 0x02, 0, 0x04, 0, 0, 0, 0, 0]
        );
        assert_eq!(frames[1], vec![0x01, 0x01, 0x08, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn type_lowercase_no_shift() {
        let c = Composer::new();
        let press = &c.type_text("a").unwrap()[0];
        assert_eq!(press, &vec![0x01, 0x01, 0x08, 0x00, 0, 0x04, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn combo_ctrl_c() {
        let c = Composer::new();
        let frames = c.combo(&["LEFT_CONTROL".into(), "C".into()]).unwrap();
        // press: ctrl bit 0x01, key 'c' = 0x06.
        assert_eq!(
            frames[0],
            vec![0x01, 0x01, 0x08, 0x01, 0, 0x06, 0, 0, 0, 0, 0]
        );
        assert_eq!(frames[1], vec![0x01, 0x01, 0x08, 0x00, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn held_key_persists_across_taps() {
        let mut c = Composer::new();
        c.down("LEFT_SHIFT").unwrap();
        // A subsequent `key A` carries the held shift in the modifier byte.
        let press = &c.key("A").unwrap()[0];
        assert_eq!(press[3], 0x02); // modifier byte = held shift
        assert_eq!(press[5], 0x04); // 'a' usage in first key slot
    }

    #[test]
    fn click_presses_then_releases_at_position() {
        let mut c = Composer::new();
        c.moveabs(1000, 2000);
        let frames = c.click("left").unwrap();
        assert_eq!(frames[0][3], 0x01); // button bit set
        assert_eq!(frames[1][3], 0x00); // released
                                        // both at the same position (x lo/hi)
        assert_eq!(&frames[0][4..6], &frames[1][4..6]);
    }

    #[test]
    fn control_frames() {
        let c = Composer::new();
        assert_eq!(c.ping(), vec![0x02, 0x01, 0x00]);
        assert_eq!(c.version(), vec![0x02, 0x02, 0x00]);
    }

    /// The shutdown release drops every held key, modifier and button in one
    /// keyboard report and one pointer report, and leaves the composer clear.
    #[test]
    fn release_everything_clears_keys_and_buttons() {
        let mut c = Composer::new();
        c.down("LEFT_SHIFT").unwrap();
        c.down("A").unwrap();
        c.mdown("left").unwrap();
        c.mdown("right").unwrap();
        let frames = c.release_everything();
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][..3], &[0x01, 0x01, 0x08]);
        assert!(frames[0][3..].iter().all(|&b| b == 0), "{:?}", frames[0]);
        assert_eq!(&frames[1][..3], &[0x01, 0x02, 0x06]);
        assert_eq!(frames[1][3], 0, "buttons byte");
        // And a later tap carries no leftover modifier or button.
        assert_eq!(c.key("B").unwrap()[0][3], 0);
        assert_eq!(c.click("left").unwrap()[1][3], 0);
    }

    #[test]
    fn power_frames() {
        let mut c = Composer::new();
        // [0x02][CMD_POWER=3][len][action] — off=0, on=1, cycle=2.
        assert_eq!(
            c.dispatch("power off").unwrap(),
            vec![vec![0x02, 0x03, 0x01, 0x00]]
        );
        assert_eq!(
            c.dispatch("power on").unwrap(),
            vec![vec![0x02, 0x03, 0x01, 0x01]]
        );
        // cycle with an explicit off-time carries a second payload byte.
        assert_eq!(
            c.dispatch("power cycle 5").unwrap(),
            vec![vec![0x02, 0x03, 0x02, 0x02, 0x05]]
        );
        // bare cycle omits the off-time (firmware default).
        assert_eq!(
            c.dispatch("power cycle").unwrap(),
            vec![vec![0x02, 0x03, 0x01, 0x02]]
        );
        assert!(c.dispatch("power sideways").is_err());
    }

    #[test]
    fn type_text_has_a_ceiling() {
        let mut c = Composer::new();
        let ok = "a".repeat(MAX_TYPE_CHARS);
        assert_eq!(c.type_text(&ok).unwrap().len(), 2 * MAX_TYPE_CHARS);
        let over = "a".repeat(MAX_TYPE_CHARS + 1);
        assert!(c.type_text(&over).is_err());
        assert!(c.dispatch(&format!("type {over}")).is_err());
    }

    #[test]
    fn move_rel_saturates_instead_of_overflowing() {
        let mut c = Composer::new();
        // From the centre, i32::MAX overflows a plain add; it must clamp to
        // the far edge (x = 0x7fff) and the near edge (y = 0).
        let f = c.move_rel(i32::MAX, i32::MIN).pop().unwrap();
        assert_eq!(&f[3..], &[0x00, 0xff, 0x7f, 0x00, 0x00, 0x00]);
        // And the cursor keeps working from the clamped position.
        let f = c.move_rel(-1, 1).pop().unwrap();
        assert_eq!(&f[3..], &[0x00, 0xfe, 0x7f, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn scroll_encodes_signed_wheel() {
        let c = Composer::new();
        let f = &c.scroll(-3)[0];
        assert_eq!(f[8], 0xfd); // -3 as int8 two's complement
    }

    /// The wheel field is one signed byte per report, so a larger amount must
    /// be split across reports (protocol §3), with a per-call ceiling so an
    /// absurd amount cannot queue millions of frames.
    #[test]
    fn scroll_splits_into_int8_reports_and_caps_the_total() {
        let c = Composer::new();
        let wheels =
            |frames: Vec<Frame>| -> Vec<i8> { frames.iter().map(|f| f[8] as i8).collect() };
        assert_eq!(wheels(c.scroll(300)), vec![127, 127, 46]);
        assert_eq!(wheels(c.scroll(-200)), vec![-127, -73]);
        assert_eq!(wheels(c.scroll(5)), vec![5]);
        assert!(c.scroll(0).is_empty());
        let total: i64 = wheels(c.scroll(i32::MAX)).iter().map(|&w| w as i64).sum();
        assert_eq!(total, MAX_SCROLL_TOTAL as i64);
        let total: i64 = wheels(c.scroll(i32::MIN)).iter().map(|&w| w as i64).sum();
        assert_eq!(total, -(MAX_SCROLL_TOTAL as i64));
    }

    /// The lock, system and menu keys ch9329 already names (its keys.rs) —
    /// same usage ids here so a sequence file works on either helper.
    #[test]
    fn lock_and_system_keys_are_named() {
        let c = Composer::new();
        for (name, usage) in [
            ("PRINT_SCREEN", 0x46u8),
            ("SCROLL_LOCK", 0x47),
            ("PAUSE", 0x48),
            ("NUM_LOCK", 0x53),
            ("KEYPAD_NUMLOCK", 0x53),
            ("APPLICATION", 0x65),
            ("MENU", 0x65),
        ] {
            let press = &c.key(name).unwrap()[0];
            assert_eq!(press[5], usage, "{name}");
        }
    }

    /// A keyboard report carries six key slots. A chord that needs more —
    /// counting keys already held with `down` — is refused rather than
    /// silently truncated; modifiers live in their own byte and never count.
    #[test]
    fn combo_refuses_more_than_six_keys() {
        let mut c = Composer::new();
        let six: Vec<String> = ["A", "B", "C", "D", "E", "F"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(c.combo(&six).is_ok());
        let mut seven = six.clone();
        seven.push("G".into());
        assert!(c.combo(&seven).is_err());
        let mut with_mods = six.clone();
        with_mods.push("LEFT_CONTROL".into());
        assert!(c.combo(&with_mods).is_ok());
        c.down("H").unwrap();
        assert!(c.combo(&six).is_err(), "a held key takes a slot too");
    }

    /// Text the US layout cannot produce is an `ERR`, not a silently shorter
    /// string; nothing is composed for it, so nothing partial is typed.
    #[test]
    fn type_refuses_unmappable_characters() {
        let c = Composer::new();
        assert!(c.type_text("caf\u{e9}").is_err());
        assert!(c.type_text("a\u{1f600}b").is_err());
        assert!(c.type_text("plain ascii ok").is_ok());
    }

    /// `type` text is the remainder of the line after one separator,
    /// verbatim: trailing spaces are typed, leading ones too. Only a line
    /// terminator is stripped, and a CR/LF *inside* the text is refused (the
    /// daemon path already refuses it; the direct path must agree).
    #[test]
    fn type_text_is_verbatim_after_the_separator() {
        let mut c = Composer::new();
        // "hi " is three characters: two frames each.
        assert_eq!(c.dispatch("type hi ").unwrap().len(), 6);
        assert_eq!(c.dispatch("type hi \n").unwrap().len(), 6);
        assert_eq!(c.dispatch("type hi \r\n").unwrap().len(), 6);
        assert_eq!(
            c.dispatch("type  x").unwrap().len(),
            4,
            "leading space kept"
        );
        assert!(c.dispatch("type a\nb").is_err());
        assert!(c.dispatch("type a\rb").is_err());
        assert!(c.dispatch("key\nENTER").is_err());
    }
}
