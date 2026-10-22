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

//! The relay board's BLE command frame, the NC (normally closed) wiring map,
//! channel validation, and address matching. All pure functions: no Bluetooth
//! I/O lives here.
//!
//! Measured on hardware 2026-10-22 (DSD TECH SH-BT01E, device-info model
//! `SH-K09-1.0`, firmware 6.1.2):
//!
//! ```text
//! frame = A0 <channel> <state> <checksum>      checksum = (A0 + channel + state) & FF
//! A0 01 01 A2   energize channel 1's coil
//! A0 01 00 A1   release channel 1's coil
//! ```
//!
//! Frames are written to characteristic `0xFFE1` of service `0xFFE0` with
//! write-with-response. The board never reports state: `FFE1` is not
//! readable and never notifies. Nothing in this crate may pretend otherwise.
//!
//! # Wiring: the target is on the NC contact
//!
//! Targets hang off the relay's **normally closed** contact, so the *coil*
//! state is the inverse of the *target* state: coil released (state byte 0)
//! means the target is powered ("on"); coil energized (state byte 1) cuts it
//! ("off"). All of that inversion lives in [`coil_for`] and nowhere else.

use anyhow::{bail, Result};

/// Short (16-bit) UUID of the relay's GATT service (`0000ffe0-0000-1000-8000-00805f9b34fb`).
pub const SERVICE_SHORT_UUID: u16 = 0xffe0;

/// Short (16-bit) UUID of the command characteristic (`0000ffe1-...`):
/// write / write-without-response / notify. Read is not permitted.
pub const COMMAND_SHORT_UUID: u16 = 0xffe1;

/// First byte of every command frame.
pub const FRAME_HEADER: u8 = 0xa0;

/// Highest channel number accepted. The SH-BT01E has one channel; DSD sells
/// 2- and 4-channel variants that use the same frame, and the board cannot
/// report how many it has.
pub const MAX_CHANNEL: u8 = 4;

/// Build the 4-byte command frame that sets `channel`'s coil to `coil`
/// (`true` = energized, state byte 1).
pub fn frame(channel: u8, coil: bool) -> [u8; 4] {
    let state = u8::from(coil);
    let sum = FRAME_HEADER.wrapping_add(channel).wrapping_add(state);
    [FRAME_HEADER, channel, state, sum]
}

/// The coil state that puts the target in the wanted state, for NC wiring:
/// target on = coil released (`false`), target off = coil energized (`true`).
/// The only place the wiring inversion exists.
pub fn coil_for(target_on: bool) -> bool {
    !target_on
}

/// The frame that switches `channel`'s target on or off (NC wiring applied).
pub fn target_frame(channel: u8, target_on: bool) -> [u8; 4] {
    frame(channel, coil_for(target_on))
}

/// Check that `channel` is in `1..=MAX_CHANNEL`.
pub fn validate_channel(channel: u8) -> Result<u8> {
    if (1..=MAX_CHANNEL).contains(&channel) {
        Ok(channel)
    } else {
        bail!("channel {channel} is out of range (1 to {MAX_CHANNEL})")
    }
}

/// Parse a channel argument for clap.
pub fn parse_channel(s: &str) -> std::result::Result<u8, String> {
    let n: u8 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a channel number (1 to {MAX_CHANNEL})"))?;
    validate_channel(n).map_err(|e| e.to_string())
}

/// Canonical form of an address for comparison, case-insensitive.
///
/// A Bluetooth MAC (`AA:BB:CC:DD:EE:FF`, also written with `-` or `_`
/// separators, or none) becomes lowercase colon-separated. Anything else (a
/// CoreBluetooth UUID, a BlueZ object path) is lowercased and trimmed.
pub fn normalize_address(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    let hex: String = s
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '_'))
        .collect();
    if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let pairs: Vec<&str> = (0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect();
        return pairs.join(":");
    }
    s
}

/// True when the user's `--address` names the peripheral whose address string
/// is `address` and whose platform id string is `id`. Linux matches the MAC;
/// macOS matches the per-host CoreBluetooth UUID carried in the id. A BlueZ
/// object-path id (`.../dev_AA_BB_CC_DD_EE_FF`) also matches by its MAC.
pub fn address_matches(query: &str, address: &str, id: &str) -> bool {
    let q = normalize_address(query);
    // An all-zero query would match every peripheral on macOS, which reports
    // 00:00:00:00:00:00 for all of them.
    if q.is_empty() || is_placeholder_address(&q) {
        return false;
    }
    let tail = id.rsplit('/').next().unwrap_or(id);
    let tail = tail.strip_prefix("dev_").unwrap_or(tail);
    [address, id, tail]
        .iter()
        .any(|c| normalize_address(c) == q)
}

/// True for an address made only of zeros and separators: macOS's stand-in
/// for "no MAC", never an identity.
fn is_placeholder_address(address: &str) -> bool {
    address.chars().all(|c| matches!(c, '0' | ':'))
}

/// An advertised name made safe to print: control characters (a nearby
/// device chooses its own name) become `.`, and the length is capped.
pub fn printable_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { '.' } else { c })
        .take(64)
        .collect()
}

/// The address string `scan` shows for a peripheral: its MAC where the
/// platform exposes one (Linux), else the platform id (macOS reports an
/// all-zero address and identifies a device by a per-host CoreBluetooth UUID).
pub fn display_address(address: &str, id: &str) -> String {
    if is_placeholder_address(address) {
        id.to_string()
    } else {
        address.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_query_matches_nothing() {
        assert!(!address_matches(
            "00:00:00:00:00:00",
            "00:00:00:00:00:00",
            "00000000-0000-4000-8000-000000000001"
        ));
        assert!(!address_matches("000000000000", "00:00:00:00:00:00", "x"));
    }

    #[test]
    fn printable_name_strips_control_characters() {
        assert_eq!(printable_name("DSD\x1b[2J\tTECH\n"), "DSD.[2J.TECH.");
        assert_eq!(printable_name(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn measured_frames() {
        assert_eq!(frame(1, true), [0xa0, 0x01, 0x01, 0xa2]);
        assert_eq!(frame(1, false), [0xa0, 0x01, 0x00, 0xa1]);
    }

    #[test]
    fn checksum_is_the_wrapping_sum() {
        for ch in 1..=MAX_CHANNEL {
            for coil in [false, true] {
                let f = frame(ch, coil);
                assert_eq!(f[3], 0xa0u8.wrapping_add(f[1]).wrapping_add(f[2]));
            }
        }
        assert_eq!(frame(0x70, true)[3], 0x11, "0xa0 + 0x70 + 1 wraps to 0x11");
        assert_eq!(
            frame(0xff, true)[3],
            0xa0,
            "0xa0 + 0xff + 1 = 0x1a0 wraps to 0xa0"
        );
    }

    #[test]
    fn nc_wiring_inverts_the_target() {
        assert!(!coil_for(true), "target on = coil released");
        assert!(coil_for(false), "target off = coil energized");
        assert_eq!(target_frame(1, true), [0xa0, 0x01, 0x00, 0xa1]);
        assert_eq!(target_frame(1, false), [0xa0, 0x01, 0x01, 0xa2]);
        assert_eq!(target_frame(3, false), [0xa0, 0x03, 0x01, 0xa4]);
    }

    #[test]
    fn channel_range() {
        for ch in 1..=MAX_CHANNEL {
            assert_eq!(validate_channel(ch).unwrap(), ch);
        }
        assert!(validate_channel(0).is_err());
        assert!(validate_channel(MAX_CHANNEL + 1).is_err());
        assert_eq!(parse_channel("2"), Ok(2));
        assert!(parse_channel("0").is_err());
        assert!(parse_channel("5").is_err());
        assert!(parse_channel("x").is_err());
        assert!(parse_channel("256").is_err());
    }

    #[test]
    fn mac_normalization() {
        assert_eq!(normalize_address("00:11:22:33:44:55"), "00:11:22:33:44:55");
        assert_eq!(
            normalize_address(" AA-BB-CC-DD-EE-FF "),
            "aa:bb:cc:dd:ee:ff"
        );
        assert_eq!(normalize_address("aa_bb_cc_dd_ee_ff"), "aa:bb:cc:dd:ee:ff");
        assert_eq!(normalize_address("AABBCCDDEEFF"), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn uuid_is_not_mangled_into_a_mac() {
        let u = "00000000-0000-4000-8000-000000000001";
        assert_eq!(normalize_address(u), u);
        assert_eq!(
            normalize_address("00000000-0000-4000-8000-00000000000A"),
            "00000000-0000-4000-8000-00000000000a"
        );
    }

    #[test]
    fn matches_mac_case_insensitively() {
        assert!(address_matches(
            "aa:bb:cc:dd:ee:ff",
            "AA:BB:CC:DD:EE:FF",
            "hci0/dev_x"
        ));
        assert!(address_matches(
            "AA:BB:CC:DD:EE:FF",
            "aa:bb:cc:dd:ee:ff",
            "x"
        ));
        assert!(!address_matches(
            "00:11:22:33:44:55",
            "AA:BB:CC:DD:EE:FF",
            "x"
        ));
    }

    #[test]
    fn matches_corebluetooth_uuid_through_the_id() {
        let id = "00000000-0000-4000-8000-000000000001";
        assert!(address_matches(id, "00:00:00:00:00:00", id));
        assert!(address_matches(&id.to_uppercase(), "00:00:00:00:00:00", id));
        assert!(!address_matches(
            "00000000-0000-4000-8000-000000000002",
            "00:00:00:00:00:00",
            id
        ));
    }

    #[test]
    fn matches_a_bluez_object_path_by_its_mac() {
        assert!(address_matches(
            "00:11:22:33:44:55",
            "00:00:00:00:00:00",
            "/org/bluez/hci0/dev_00_11_22_33_44_55"
        ));
    }

    #[test]
    fn scan_shows_the_mac_or_falls_back_to_the_id() {
        assert_eq!(
            display_address("00:11:22:33:44:55", "x"),
            "00:11:22:33:44:55"
        );
        assert_eq!(display_address("00:00:00:00:00:00", "uuid-1"), "uuid-1");
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(!address_matches("", "", ""));
    }
}
