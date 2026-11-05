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

//! USB HID keyboard usage tables and key-name parsing.
//!
//! Two name spaces meet here:
//!
//! - The HID serial protocol (`docs/dev/hid-serial-protocol.md` §3) names keys with
//!   the `adafruit_hid` `Keycode` convention — `A`, `ENTER`, `LEFT_CONTROL`,
//!   `FORWARD_SLASH`, `F1`. Those names are what `key`/`combo`/`down`/`up`
//!   accept, so a `ch9329` injector is a drop-in for the KB2040 `hidrig`.
//! - `type <text>` maps literal characters to usages assuming a **US layout**,
//!   because the CH9329 forwards raw HID usages and the target does the layout.
//!
//! Usage IDs are USB HID Usage Table page 0x07 (Keyboard/Keypad); they are
//! numerically identical to `adafruit_hid.Keycode` values, including the
//! `0xE0..=0xE7` modifier usages.

use anyhow::{anyhow, Result};

/// Modifier bits of the HID boot-keyboard report's first byte.
pub const MOD_LEFT_CONTROL: u8 = 0x01;
pub const MOD_LEFT_SHIFT: u8 = 0x02;
pub const MOD_LEFT_ALT: u8 = 0x04;
pub const MOD_LEFT_GUI: u8 = 0x08;
pub const MOD_RIGHT_CONTROL: u8 = 0x10;
pub const MOD_RIGHT_SHIFT: u8 = 0x20;
pub const MOD_RIGHT_ALT: u8 = 0x40;
pub const MOD_RIGHT_GUI: u8 = 0x80;

/// A resolved key name: either a modifier (contributes a bit to the report's
/// modifier byte) or an ordinary usage (occupies one of the six key slots).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A modifier bit (one of the `MOD_*` constants above).
    Modifier(u8),
    /// A plain HID usage ID.
    Usage(u8),
}

impl Key {
    /// Whether this is a lock key (Caps/Scroll/Num Lock). Hosts treat these
    /// specially — macOS debounces Caps Lock — so taps of them need a longer
    /// hold than ordinary keys.
    pub fn is_lock(self) -> bool {
        matches!(self, Key::Usage(0x39) | Key::Usage(0x47) | Key::Usage(0x53))
    }
}

/// Resolve an `adafruit_hid` Keycode name (case-insensitive) to a [`Key`].
///
/// Accepts the full set the protocol requires plus common aliases. An unknown
/// name is an error (the protocol mandates `ERR`, never a silent guess).
pub fn name_to_key(name: &str) -> Result<Key> {
    let n = name.to_ascii_uppercase();

    // Letters A–Z -> usage 0x04..=0x1D.
    if n.len() == 1 {
        let b = n.as_bytes()[0];
        if b.is_ascii_uppercase() {
            return Ok(Key::Usage(0x04 + (b - b'A')));
        }
    }

    let key = match n.as_str() {
        // Digits (top row). Note ZERO sits *after* NINE in the usage table.
        "ZERO" => Key::Usage(0x27),
        "ONE" => Key::Usage(0x1E),
        "TWO" => Key::Usage(0x1F),
        "THREE" => Key::Usage(0x20),
        "FOUR" => Key::Usage(0x21),
        "FIVE" => Key::Usage(0x22),
        "SIX" => Key::Usage(0x23),
        "SEVEN" => Key::Usage(0x24),
        "EIGHT" => Key::Usage(0x25),
        "NINE" => Key::Usage(0x26),

        // Whitespace / editing.
        "ENTER" | "RETURN" => Key::Usage(0x28),
        "ESCAPE" | "ESC" => Key::Usage(0x29),
        "BACKSPACE" => Key::Usage(0x2A),
        "TAB" => Key::Usage(0x2B),
        "SPACE" | "SPACEBAR" => Key::Usage(0x2C),
        "DELETE" | "DEL" => Key::Usage(0x4C),
        "INSERT" => Key::Usage(0x49),
        "HOME" => Key::Usage(0x4A),
        "END" => Key::Usage(0x4D),
        "PAGE_UP" => Key::Usage(0x4B),
        "PAGE_DOWN" => Key::Usage(0x4E),

        // Arrows.
        "RIGHT_ARROW" => Key::Usage(0x4F),
        "LEFT_ARROW" => Key::Usage(0x50),
        "DOWN_ARROW" => Key::Usage(0x51),
        "UP_ARROW" => Key::Usage(0x52),

        // Symbols (US positions).
        "MINUS" => Key::Usage(0x2D),
        "EQUALS" => Key::Usage(0x2E),
        "LEFT_BRACKET" => Key::Usage(0x2F),
        "RIGHT_BRACKET" => Key::Usage(0x30),
        "BACKSLASH" => Key::Usage(0x31),
        "SEMICOLON" => Key::Usage(0x33),
        "QUOTE" => Key::Usage(0x34),
        "GRAVE_ACCENT" => Key::Usage(0x35),
        "COMMA" => Key::Usage(0x36),
        "PERIOD" => Key::Usage(0x37),
        "FORWARD_SLASH" => Key::Usage(0x38),

        // Locks / system.
        "CAPS_LOCK" => Key::Usage(0x39),
        "PRINT_SCREEN" => Key::Usage(0x46),
        "SCROLL_LOCK" => Key::Usage(0x47),
        "PAUSE" => Key::Usage(0x48),
        "KEYPAD_NUMLOCK" | "NUM_LOCK" => Key::Usage(0x53),
        "APPLICATION" | "MENU" => Key::Usage(0x65),

        // Function keys.
        "F1" => Key::Usage(0x3A),
        "F2" => Key::Usage(0x3B),
        "F3" => Key::Usage(0x3C),
        "F4" => Key::Usage(0x3D),
        "F5" => Key::Usage(0x3E),
        "F6" => Key::Usage(0x3F),
        "F7" => Key::Usage(0x40),
        "F8" => Key::Usage(0x41),
        "F9" => Key::Usage(0x42),
        "F10" => Key::Usage(0x43),
        "F11" => Key::Usage(0x44),
        "F12" => Key::Usage(0x45),

        // Modifiers (set a bit; a bare tap of one is e.g. "press the Win key").
        "LEFT_CONTROL" | "CONTROL" | "CTRL" => Key::Modifier(MOD_LEFT_CONTROL),
        "LEFT_SHIFT" | "SHIFT" => Key::Modifier(MOD_LEFT_SHIFT),
        "LEFT_ALT" | "ALT" => Key::Modifier(MOD_LEFT_ALT),
        "LEFT_GUI" | "GUI" | "WINDOWS" | "COMMAND" => Key::Modifier(MOD_LEFT_GUI),
        "RIGHT_CONTROL" => Key::Modifier(MOD_RIGHT_CONTROL),
        "RIGHT_SHIFT" => Key::Modifier(MOD_RIGHT_SHIFT),
        "RIGHT_ALT" => Key::Modifier(MOD_RIGHT_ALT),
        "RIGHT_GUI" => Key::Modifier(MOD_RIGHT_GUI),

        _ => return Err(anyhow!("unknown key name: {name}")),
    };
    Ok(key)
}

/// Map one character to `(usage, needs_shift)` for a US keyboard layout.
///
/// Ported from marion's `keys.char_to_hid`. Returns an error for characters
/// outside the US layout (the protocol allows `ERR` for unrepresentable text).
pub fn char_to_usage(c: char) -> Result<(u8, bool)> {
    let key = match c {
        '\n' => (0x28, false), // ENTER
        '\t' => (0x2B, false), // TAB
        ' ' => (0x2C, false),  // SPACE
        'a'..='z' => (0x04 + (c as u8 - b'a'), false),
        'A'..='Z' => (0x04 + (c as u8 - b'A'), true),
        '1'..='9' => (0x1E + (c as u8 - b'1'), false),
        '0' => (0x27, false),

        // Unshifted US punctuation.
        '-' => (0x2D, false),
        '=' => (0x2E, false),
        '[' => (0x2F, false),
        ']' => (0x30, false),
        '\\' => (0x31, false),
        ';' => (0x33, false),
        '\'' => (0x34, false),
        '`' => (0x35, false),
        ',' => (0x36, false),
        '.' => (0x37, false),
        '/' => (0x38, false),

        // Shifted US punctuation (digit row).
        '!' => (0x1E, true),
        '@' => (0x1F, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),

        // Shifted US punctuation (symbol keys).
        '_' => (0x2D, true),
        '+' => (0x2E, true),
        '{' => (0x2F, true),
        '}' => (0x30, true),
        '|' => (0x31, true),
        ':' => (0x33, true),
        '"' => (0x34, true),
        '~' => (0x35, true),
        '<' => (0x36, true),
        '>' => (0x37, true),
        '?' => (0x38, true),

        _ => return Err(anyhow!("cannot type character {c:?} (US layout only)")),
    };
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        assert_eq!(name_to_key("A").unwrap(), Key::Usage(0x04));
        assert_eq!(name_to_key("z").unwrap(), Key::Usage(0x1D));
        assert_eq!(name_to_key("ZERO").unwrap(), Key::Usage(0x27));
        assert_eq!(name_to_key("ONE").unwrap(), Key::Usage(0x1E));
    }

    #[test]
    fn modifiers_and_aliases() {
        assert_eq!(name_to_key("LEFT_CONTROL").unwrap(), Key::Modifier(0x01));
        assert_eq!(name_to_key("ctrl").unwrap(), Key::Modifier(0x01));
        assert_eq!(name_to_key("LEFT_GUI").unwrap(), Key::Modifier(0x08));
        assert_eq!(name_to_key("RIGHT_ALT").unwrap(), Key::Modifier(0x40));
    }

    #[test]
    fn named_and_symbol_keys() {
        assert_eq!(name_to_key("ENTER").unwrap(), Key::Usage(0x28));
        assert_eq!(name_to_key("UP_ARROW").unwrap(), Key::Usage(0x52));
        assert_eq!(name_to_key("FORWARD_SLASH").unwrap(), Key::Usage(0x38));
        assert_eq!(name_to_key("F12").unwrap(), Key::Usage(0x45));
        assert!(name_to_key("NOPE").is_err());
    }

    #[test]
    fn lock_keys() {
        assert!(name_to_key("CAPS_LOCK").unwrap().is_lock());
        assert!(name_to_key("SCROLL_LOCK").unwrap().is_lock());
        assert!(name_to_key("NUM_LOCK").unwrap().is_lock());
        assert!(!name_to_key("A").unwrap().is_lock());
        assert!(!name_to_key("LEFT_SHIFT").unwrap().is_lock());
    }

    #[test]
    fn us_char_map() {
        assert_eq!(char_to_usage('a').unwrap(), (0x04, false));
        assert_eq!(char_to_usage('A').unwrap(), (0x04, true));
        assert_eq!(char_to_usage('1').unwrap(), (0x1E, false));
        assert_eq!(char_to_usage('!').unwrap(), (0x1E, true));
        assert_eq!(char_to_usage('/').unwrap(), (0x38, false));
        assert_eq!(char_to_usage('?').unwrap(), (0x38, true));
        assert_eq!(char_to_usage(' ').unwrap(), (0x2C, false));
        assert!(char_to_usage('€').is_err());
    }
}
