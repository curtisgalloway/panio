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

//! X11 keysyms (what an RFB KeyEvent carries) to the HID serial protocol's
//! key names (`docs/dev/hid-serial-protocol.md`, "Key names").
//!
//! The protocol names physical keys on a US layout, so a keysym that is a
//! shifted character maps to its base key plus a "needs Shift" flag. noVNC
//! sends Shift as its own KeyEvent *and* the shifted keysym ('A', '!') for the
//! key pressed with it, so the session normally just forwards both; the flag
//! lets it add a Shift of its own when a client sends '!' without holding one.

/// A key the HID vocabulary can press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidKey {
    /// Name accepted by `down` / `up`.
    pub name: &'static str,
    /// The keysym is the shifted form of `name` on a US layout.
    pub shifted: bool,
    /// The keysym is itself a Shift key (either side).
    pub is_shift: bool,
}

const fn key(name: &'static str) -> Option<HidKey> {
    Some(HidKey {
        name,
        shifted: false,
        is_shift: false,
    })
}

const fn shifted(name: &'static str) -> Option<HidKey> {
    Some(HidKey {
        name,
        shifted: true,
        is_shift: false,
    })
}

const DIGITS: [&str; 10] = [
    "ZERO", "ONE", "TWO", "THREE", "FOUR", "FIVE", "SIX", "SEVEN", "EIGHT", "NINE",
];

const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];

const FUNCTION: [&str; 12] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
];

/// The HID key for a keysym, or `None` when the vocabulary cannot press it
/// (function keys past F12, non-US characters, media keys, ...).
pub fn lookup(sym: u32) -> Option<HidKey> {
    match sym {
        0x20 => key("SPACE"),
        // Digits and the shifted characters of the number row.
        0x30..=0x39 => key(DIGITS[(sym - 0x30) as usize]),
        0x21 => shifted("ONE"),
        0x40 => shifted("TWO"),
        0x23 => shifted("THREE"),
        0x24 => shifted("FOUR"),
        0x25 => shifted("FIVE"),
        0x5E => shifted("SIX"),
        0x26 => shifted("SEVEN"),
        0x2A => shifted("EIGHT"),
        0x28 => shifted("NINE"),
        0x29 => shifted("ZERO"),
        // Letters.
        0x61..=0x7A => key(LETTERS[(sym - 0x61) as usize]),
        0x41..=0x5A => shifted(LETTERS[(sym - 0x41) as usize]),
        // Punctuation.
        0x2D => key("MINUS"),
        0x5F => shifted("MINUS"),
        0x3D => key("EQUALS"),
        0x2B => shifted("EQUALS"),
        0x5B => key("LEFT_BRACKET"),
        0x7B => shifted("LEFT_BRACKET"),
        0x5D => key("RIGHT_BRACKET"),
        0x7D => shifted("RIGHT_BRACKET"),
        0x5C => key("BACKSLASH"),
        0x7C => shifted("BACKSLASH"),
        0x3B => key("SEMICOLON"),
        0x3A => shifted("SEMICOLON"),
        0x27 => key("QUOTE"),
        0x22 => shifted("QUOTE"),
        0x60 => key("GRAVE_ACCENT"),
        0x7E => shifted("GRAVE_ACCENT"),
        0x2C => key("COMMA"),
        0x3C => shifted("COMMA"),
        0x2E => key("PERIOD"),
        0x3E => shifted("PERIOD"),
        0x2F => key("FORWARD_SLASH"),
        0x3F => shifted("FORWARD_SLASH"),
        // Editing and navigation (X11 keysymdef.h, 0xFFxx).
        0xFF08 => key("BACKSPACE"),
        0xFF09 => key("TAB"),
        0xFE20 => shifted("TAB"), // ISO_Left_Tab
        0xFF0D => key("ENTER"),
        0xFF13 => key("PAUSE"),
        0xFF14 => key("SCROLL_LOCK"),
        0xFF1B => key("ESCAPE"),
        0xFFFF => key("DELETE"),
        0xFF50 => key("HOME"),
        0xFF51 => key("LEFT_ARROW"),
        0xFF52 => key("UP_ARROW"),
        0xFF53 => key("RIGHT_ARROW"),
        0xFF54 => key("DOWN_ARROW"),
        0xFF55 => key("PAGE_UP"),
        0xFF56 => key("PAGE_DOWN"),
        0xFF57 => key("END"),
        0xFF61 => key("PRINT_SCREEN"),
        0xFF63 => key("INSERT"),
        0xFF67 => key("APPLICATION"), // Menu
        0xFF7F => key("NUM_LOCK"),
        0xFFE5 => key("CAPS_LOCK"),
        // Function keys. The vocabulary stops at F12.
        0xFFBE..=0xFFC9 => key(FUNCTION[(sym - 0xFFBE) as usize]),
        // Modifiers. Meta is treated as Alt, the X11 convention.
        0xFFE1 => Some(HidKey {
            name: "LEFT_SHIFT",
            shifted: false,
            is_shift: true,
        }),
        0xFFE2 => Some(HidKey {
            name: "RIGHT_SHIFT",
            shifted: false,
            is_shift: true,
        }),
        0xFFE3 => key("LEFT_CONTROL"),
        0xFFE4 => key("RIGHT_CONTROL"),
        0xFFE7 | 0xFFE9 => key("LEFT_ALT"),
        0xFFE8 | 0xFFEA | 0xFE03 => key("RIGHT_ALT"), // 0xFE03 = ISO_Level3_Shift (AltGr)
        0xFFEB => key("LEFT_GUI"),
        0xFFEC => key("RIGHT_GUI"),
        // Keypad. The vocabulary has no keypad usages, so these press the
        // main-block key that types the same character.
        0xFF89 => key("TAB"),
        0xFF8D => key("ENTER"),
        0xFF95 => key("HOME"),
        0xFF96 => key("LEFT_ARROW"),
        0xFF97 => key("UP_ARROW"),
        0xFF98 => key("RIGHT_ARROW"),
        0xFF99 => key("DOWN_ARROW"),
        0xFF9A => key("PAGE_UP"),
        0xFF9B => key("PAGE_DOWN"),
        0xFF9C => key("END"),
        0xFF9E => key("INSERT"),
        0xFF9F => key("DELETE"),
        0xFFAA => shifted("EIGHT"),
        0xFFAB => shifted("EQUALS"),
        0xFFAC => key("COMMA"),
        0xFFAD => key("MINUS"),
        0xFFAE => key("PERIOD"),
        0xFFAF => key("FORWARD_SLASH"),
        0xFFB0..=0xFFB9 => key(DIGITS[(sym - 0xFFB0) as usize]),
        0xFFBD => key("EQUALS"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_shifted_characters() {
        assert_eq!(lookup('a' as u32).unwrap().name, "A");
        assert!(!lookup('a' as u32).unwrap().shifted);
        assert_eq!(lookup('A' as u32).unwrap().name, "A");
        assert!(lookup('A' as u32).unwrap().shifted);
        assert_eq!(lookup('0' as u32).unwrap().name, "ZERO");
        assert_eq!(lookup('9' as u32).unwrap().name, "NINE");
        assert_eq!(lookup('!' as u32).unwrap().name, "ONE");
        assert!(lookup('!' as u32).unwrap().shifted);
        assert_eq!(lookup(')' as u32).unwrap().name, "ZERO");
    }

    #[test]
    fn every_printable_ascii_character_has_a_key() {
        for c in 0x20u32..0x7F {
            assert!(lookup(c).is_some(), "no key for {:?}", char::from_u32(c));
        }
    }

    #[test]
    fn shift_and_function_keys() {
        let s = lookup(0xFFE1).unwrap();
        assert_eq!((s.name, s.is_shift), ("LEFT_SHIFT", true));
        assert_eq!(lookup(0xFFBE).unwrap().name, "F1");
        assert_eq!(lookup(0xFFC9).unwrap().name, "F12");
        assert_eq!(lookup(0xFFCA), None, "F13 is outside the vocabulary");
        assert_eq!(lookup(0xFF51).unwrap().name, "LEFT_ARROW");
        assert_eq!(lookup(0xFFB5).unwrap().name, "FIVE");
        assert_eq!(lookup(0x00E9), None, "non-US characters are dropped");
    }
}
