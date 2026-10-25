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

//! Pure wire-format code for a Phidgets VINT hub: no I/O, no hardware.
//!
//! The spec is `notes/phidgets-vint.md` (reimplemented from libphidget22's
//! wire format, hardware-verified on a HUB0002 + REL1100). Everything here is
//! plain functions over byte slices so the tests pin the exact bytes.

use std::fmt;

/// VINT ports on a HUB0002.
pub const PORT_COUNT: u8 = 6;

/// Packet ids per hub port (`VINTHUB_PACKETIDS_PER_PORT` in libphidget22).
const PACKET_IDS_PER_PORT: u8 = 21;

/// DigitalOutput `SETDUTYCYCLE` packet type.
pub const PTYPE_SETDUTYCYCLE: u8 = 0x0A;

/// USB descriptor type of the VINT ports descriptor.
pub const PORTS_DESCRIPTOR_TYPE: u8 = 0x43;

/// Length of the VINT ports descriptor of a 6-port hub.
pub const PORTS_DESCRIPTOR_LEN: usize = 42;

/// A `<port>/<channel>` address, both 0-based as printed on the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub port: u8,
    pub channel: u8,
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.port, self.channel)
    }
}

/// Parse `<port>/<channel>`: port 0-5, channel any number. The channel's
/// upper bound depends on the module, so `devices::check_target` checks it
/// against the module found on the port.
pub fn parse_address(s: &str) -> Result<Address, String> {
    let (p, c) = s
        .split_once('/')
        .ok_or_else(|| format!("`{s}` is not a <PORT>/<CHANNEL> address (for example 0/0)"))?;
    let port: u8 = p
        .trim()
        .parse()
        .map_err(|_| format!("port `{p}` is not a number (0 to {})", PORT_COUNT - 1))?;
    let channel: u8 = c
        .trim()
        .parse()
        .map_err(|_| format!("channel `{c}` is not a number"))?;
    if port >= PORT_COUNT {
        return Err(format!(
            "port {port} is out of range (0 to {})",
            PORT_COUNT - 1
        ));
    }
    Ok(Address { port, channel })
}

/// The packet id used for a one-shot write from `port`: the first id of the
/// port's range, 1 + 21 * port.
pub fn packet_id(port: u8) -> u8 {
    port * PACKET_IDS_PER_PORT + 1
}

/// Big-endian payload of `SETDUTYCYCLE`: full duty cycle is on, zero is off.
pub fn duty_payload(on: bool) -> [u8; 2] {
    if on {
        [0x80, 0x00]
    } else {
        [0x00, 0x00]
    }
}

/// Layer A: the VINT device packet. Channel 0 omits the channel byte.
///
/// `[len+1, ptype, payload..]` for channel 0,
/// `[0x40 | (len+2), channel, ptype, payload..]` otherwise.
pub fn vint_packet(channel: u8, ptype: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u8;
    let mut out = Vec::with_capacity(payload.len() + 3);
    if channel == 0 {
        out.push(len + 1);
    } else {
        out.push(0x40 | (len + 2));
        out.push(channel);
    }
    out.push(ptype);
    out.extend_from_slice(payload);
    out
}

/// Layer B: the hub wrapper around a VINT packet. The counter nibble is
/// always 0: the hub does not check it (measured).
pub fn hub_packet(port: u8, vint_id: u16, packet_id: u8, vint: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vint.len() + 4);
    out.push(port & 0x0F);
    out.push((vint_id & 0xFF) as u8);
    out.push(((vint_id >> 4) & 0xF0) as u8);
    out.push(packet_id);
    out.extend_from_slice(vint);
    out
}

/// The complete bulk OUT packet that switches one DigitalOutput channel.
pub fn set_output_packet(port: u8, vint_id: u16, channel: u8, on: bool) -> Vec<u8> {
    let layer_a = vint_packet(channel, PTYPE_SETDUTYCYCLE, &duty_payload(on));
    hub_packet(port, vint_id, packet_id(port), &layer_a)
}

/// A hub status code from an ACK tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ack,
    Nak,
    InvalidArg,
    InvalidPacketType,
    InvalidSequence,
    InvalidCommand,
    Malformed,
    NoSpace,
    Unexpected,
    NotAttached,
    TooBig,
    Failsafe,
    Other(u8),
}

impl Status {
    pub fn from_byte(b: u8) -> Status {
        match b {
            0xE0 => Status::Ack,
            0xE1 => Status::Nak,
            0xE2 => Status::InvalidArg,
            0xE3 => Status::InvalidPacketType,
            0xE4 => Status::InvalidSequence,
            0xE5 => Status::InvalidCommand,
            0xE6 => Status::Malformed,
            0xE7 => Status::NoSpace,
            0xE8 => Status::Unexpected,
            0xE9 => Status::NotAttached,
            0xEA => Status::TooBig,
            0xEB => Status::Failsafe,
            other => Status::Other(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Status::Ack => "ACK".into(),
            Status::Nak => "NAK".into(),
            Status::InvalidArg => "INVALIDARG".into(),
            Status::InvalidPacketType => "INVALIDPACKETTYPE".into(),
            Status::InvalidSequence => "INVALIDSEQUENCE".into(),
            Status::InvalidCommand => "INVALIDCOMMAND".into(),
            Status::Malformed => "MALFORMED".into(),
            Status::NoSpace => "NOSPACE".into(),
            Status::Unexpected => "UNEXPECTED".into(),
            Status::NotAttached => "NOTATTACHED".into(),
            Status::TooBig => "TOOBIG".into(),
            Status::Failsafe => "FAILSAFE".into(),
            Status::Other(b) => format!("status 0x{b:02X}"),
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Parse the ACK tail of one IN packet: the low nibble of byte 0 is the
/// number of trailing bytes. A byte is a packet id (low 7 bits); when its top
/// bit is set the next byte is the status, otherwise the status is ACK.
pub fn parse_acks(packet: &[u8]) -> Vec<(u8, Status)> {
    let Some(&first) = packet.first() else {
        return Vec::new();
    };
    let n = ((first & 0x0F) as usize).min(packet.len() - 1);
    let tail = &packet[packet.len() - n..];
    let mut out = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let b = tail[i];
        let id = b & 0x7F;
        if b & 0x80 != 0 {
            let Some(&s) = tail.get(i + 1) else { break };
            out.push((id, Status::from_byte(s)));
            i += 2;
        } else {
            out.push((id, Status::Ack));
            i += 1;
        }
    }
    out
}

/// The status the hub reported for `id` in this IN packet, if it did.
pub fn status_for(packet: &[u8], id: u8) -> Option<Status> {
    parse_acks(packet)
        .into_iter()
        .find(|(i, _)| *i == id)
        .map(|(_, s)| s)
}

/// Decode a Phidget version word: `(w >> 8) * 100 + (w & 0xFF)`.
pub fn decode_version(w: u16) -> u32 {
    u32::from(w >> 8) * 100 + u32::from(w & 0xFF)
}

/// One port of the VINT ports descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortEntry {
    /// VINT device id; 0 means the port is empty.
    pub id: u16,
    /// Raw firmware version word; see [`decode_version`].
    pub version: u16,
}

/// The VINT ports descriptor (type 0x43): which module is on which port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortsDescriptor {
    pub ports: [PortEntry; PORT_COUNT as usize],
}

impl PortsDescriptor {
    /// Parse the 42-byte little-endian descriptor: `bLength`, type 0x43, six
    /// `{wID, wVersion}` pairs from offset 2, then four port-mode entries
    /// that are not used here.
    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() != PORTS_DESCRIPTOR_LEN {
            return Err(format!(
                "the hub returned {} bytes for the VINT ports descriptor, expected {}",
                b.len(),
                PORTS_DESCRIPTOR_LEN
            ));
        }
        if b[1] != PORTS_DESCRIPTOR_TYPE {
            return Err(format!(
                "descriptor type is 0x{:02X}, expected the VINT ports descriptor 0x{:02X}",
                b[1], PORTS_DESCRIPTOR_TYPE
            ));
        }
        let mut ports = [PortEntry { id: 0, version: 0 }; PORT_COUNT as usize];
        for (i, p) in ports.iter_mut().enumerate() {
            let o = 2 + 4 * i;
            p.id = u16::from_le_bytes([b[o], b[o + 1]]);
            p.version = u16::from_le_bytes([b[o + 2], b[o + 3]]);
        }
        Ok(PortsDescriptor { ports })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REL1100: u16 = 0x1A;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn golden_port0_ch0_on() {
        assert_eq!(
            set_output_packet(0, REL1100, 0, true),
            hex("001a0001030a8000")
        );
    }

    #[test]
    fn golden_port0_ch1_on() {
        assert_eq!(
            set_output_packet(0, REL1100, 1, true),
            hex("001a000144010a8000")
        );
    }

    #[test]
    fn golden_port1_ch0_on_uses_id_22() {
        assert_eq!(
            set_output_packet(1, REL1100, 0, true),
            hex("011a0016030a8000")
        );
    }

    #[test]
    fn off_sends_zero_duty() {
        assert_eq!(
            set_output_packet(0, REL1100, 0, false),
            hex("001a0001030a0000")
        );
        assert_eq!(
            set_output_packet(0, REL1100, 3, false),
            hex("001a000144030a0000")
        );
    }

    #[test]
    fn packet_ids_are_per_port_ranges() {
        assert_eq!(packet_id(0), 1);
        assert_eq!(packet_id(1), 22);
        assert_eq!(packet_id(5), 106);
    }

    #[test]
    fn large_vint_id_spills_into_byte_two() {
        let p = hub_packet(2, 0x01A0, 43, &[0x03]);
        assert_eq!(p, vec![0x02, 0xA0, 0x10, 43, 0x03]);
    }

    #[test]
    fn golden_ack_packet() {
        assert_eq!(parse_acks(&hex("010001")), vec![(1, Status::Ack)]);
        assert_eq!(status_for(&hex("010001"), 1), Some(Status::Ack));
    }

    #[test]
    fn golden_notattached_packet() {
        assert_eq!(
            parse_acks(&hex("320096e9")),
            vec![(22, Status::NotAttached)]
        );
        assert_eq!(status_for(&hex("320096e9"), 22), Some(Status::NotAttached));
        assert_eq!(status_for(&hex("320096e9"), 1), None);
    }

    #[test]
    fn ack_tail_mixes_bare_and_status_ids() {
        // three tail bytes: id 5 (ACK), then 0x96 0xe1 (id 22 NAK)
        let p = [0x03, 0x00, 0x05, 0x96, 0xE1];
        assert_eq!(parse_acks(&p), vec![(5, Status::Ack), (22, Status::Nak)]);
    }

    #[test]
    fn truncated_status_and_empty_packets_do_not_panic() {
        assert!(parse_acks(&[]).is_empty());
        assert!(parse_acks(&[0x01, 0x96]).is_empty());
        assert!(parse_acks(&[0x0F]).is_empty());
        assert!(parse_acks(&[0x00, 0x01]).is_empty());
    }

    #[test]
    fn status_names() {
        let names: Vec<String> = (0xE0..=0xEB).map(|b| Status::from_byte(b).name()).collect();
        assert_eq!(
            names,
            [
                "ACK",
                "NAK",
                "INVALIDARG",
                "INVALIDPACKETTYPE",
                "INVALIDSEQUENCE",
                "INVALIDCOMMAND",
                "MALFORMED",
                "NOSPACE",
                "UNEXPECTED",
                "NOTATTACHED",
                "TOOBIG",
                "FAILSAFE"
            ]
        );
        assert_eq!(Status::from_byte(0xF2).name(), "status 0xF2");
    }

    #[test]
    fn version_decode() {
        assert_eq!(decode_version(0x0078), 120);
        assert_eq!(decode_version(0x0182), 100 + 130);
        assert_eq!(decode_version(0x0101), 101);
    }

    #[test]
    fn address_parsing() {
        assert_eq!(
            parse_address("0/0"),
            Ok(Address {
                port: 0,
                channel: 0
            })
        );
        assert_eq!(
            parse_address("5/3"),
            Ok(Address {
                port: 5,
                channel: 3
            })
        );
        assert_eq!(
            parse_address(" 2 / 1 "),
            Ok(Address {
                port: 2,
                channel: 1
            })
        );
        for bad in [
            "", "0", "6/0", "0/256", "a/0", "0/b", "0/0/0", "-1/0", "0/-1",
        ] {
            assert!(parse_address(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(
            Address {
                port: 3,
                channel: 2
            }
            .to_string(),
            "3/2"
        );
    }

    /// Real descriptor shape: REL1100 (id 0x1A, firmware word 130) on port 0.
    pub(crate) fn sample_descriptor(port0: (u16, u16), port3: (u16, u16)) -> Vec<u8> {
        let mut d = vec![0u8; PORTS_DESCRIPTOR_LEN];
        d[0] = PORTS_DESCRIPTOR_LEN as u8;
        d[1] = PORTS_DESCRIPTOR_TYPE;
        d[2..4].copy_from_slice(&port0.0.to_le_bytes());
        d[4..6].copy_from_slice(&port0.1.to_le_bytes());
        d[14..16].copy_from_slice(&port3.0.to_le_bytes());
        d[16..18].copy_from_slice(&port3.1.to_le_bytes());
        d
    }

    #[test]
    fn ports_descriptor_parses() {
        let d = PortsDescriptor::parse(&sample_descriptor((0x1A, 130), (0x1234, 0x0101))).unwrap();
        assert_eq!(
            d.ports[0],
            PortEntry {
                id: 0x1A,
                version: 130
            }
        );
        assert_eq!(d.ports[1].id, 0);
        assert_eq!(
            d.ports[3],
            PortEntry {
                id: 0x1234,
                version: 0x0101
            }
        );
    }

    #[test]
    fn ports_descriptor_rejects_bad_shapes() {
        assert!(PortsDescriptor::parse(&[0; 10]).is_err());
        let mut d = sample_descriptor((0, 0), (0, 0));
        d[1] = 0x44;
        assert!(PortsDescriptor::parse(&d).is_err());
    }
}
