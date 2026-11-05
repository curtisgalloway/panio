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

//! Raw Ethernet/IPv4/UDP frame construction for the macOS BPF send path.
//!
//! 1:1 port of `_tftp.py`'s `_inet_checksum` + `_build_udp_frame`. Kept as a
//! standalone, fully unit-tested module because it is pure byte-twiddling and
//! must match the wire format exactly — a single off-by-one in the checksum or
//! header layout silently breaks delivery and is invisible without hardware.

use std::net::Ipv4Addr;

/// Internet checksum (RFC 1071): one's-complement sum of 16-bit big-endian
/// words, with the final fold. An odd trailing byte is treated as the high
/// byte of a zero-padded word (matching Python's `struct.unpack('!H', b+b'\0')`).
// Only reached via the BPF send path (macOS) and the unit tests; dead on a
// Linux non-test build, which is expected.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn inet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let (words, rest) = data.as_chunks::<2>();
    for c in words {
        sum += u16::from_be_bytes(*c) as u32;
    }
    if let [b] = rest {
        sum += (*b as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The largest UDP payload one IPv4 datagram can carry: the 16-bit IP total
/// length, less the 20-byte IPv4 header and the 8-byte UDP header.
pub const MAX_UDP_PAYLOAD: usize = 65_535 - 20 - 8;

/// Build a complete Ethernet (EtherType IPv4) / IPv4 / UDP frame with the given
/// source MAC preserved. The destination MAC is written verbatim — the whole
/// point of the BPF path is to bypass the kernel's (wrong) ARP resolution and
/// address the Pi bootloader's real DHCP MAC directly.
///
/// `None` when `payload` is longer than [`MAX_UDP_PAYLOAD`]: the IP and UDP
/// length fields are 16 bits, and a length that wrapped would describe a
/// datagram other than the one on the wire.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn build_udp_frame(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let src_a = src_ip.octets();
    let dst_a = dst_ip.octets();
    let ip_len = u16::try_from(20 + 8 + payload.len()).ok()?;
    let udp_len = ip_len - 20;

    // --- IPv4 header (20 bytes, no options) ---
    let mut ip_hdr = Vec::with_capacity(20);
    ip_hdr.push(0x45); // version 4, IHL 5
    ip_hdr.push(0x00); // DSCP/ECN
    ip_hdr.extend_from_slice(&ip_len.to_be_bytes());
    ip_hdr.extend_from_slice(&0u16.to_be_bytes()); // identification
    ip_hdr.extend_from_slice(&0x4000u16.to_be_bytes()); // flags=DF, frag=0
    ip_hdr.push(64); // TTL
    ip_hdr.push(17); // protocol = UDP
    ip_hdr.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    ip_hdr.extend_from_slice(&src_a);
    ip_hdr.extend_from_slice(&dst_a);
    let ip_ck = inet_checksum(&ip_hdr);
    ip_hdr[10..12].copy_from_slice(&ip_ck.to_be_bytes());

    // --- UDP header + checksum (over the IPv4 pseudo-header) ---
    let mut ck_input = Vec::with_capacity(12 + 8 + payload.len());
    ck_input.extend_from_slice(&src_a);
    ck_input.extend_from_slice(&dst_a);
    ck_input.push(0); // zero
    ck_input.push(17); // protocol
    ck_input.extend_from_slice(&udp_len.to_be_bytes());
    ck_input.extend_from_slice(&src_port.to_be_bytes());
    ck_input.extend_from_slice(&dst_port.to_be_bytes());
    ck_input.extend_from_slice(&udp_len.to_be_bytes());
    ck_input.extend_from_slice(&0u16.to_be_bytes()); // checksum zero
    ck_input.extend_from_slice(payload);
    let udp_ck = inet_checksum(&ck_input);

    let mut udp = Vec::with_capacity(8 + payload.len());
    udp.extend_from_slice(&src_port.to_be_bytes());
    udp.extend_from_slice(&dst_port.to_be_bytes());
    udp.extend_from_slice(&udp_len.to_be_bytes());
    udp.extend_from_slice(&udp_ck.to_be_bytes());
    udp.extend_from_slice(payload);

    // --- Ethernet II frame ---
    let mut frame = Vec::with_capacity(14 + ip_hdr.len() + udp.len());
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&src_mac);
    frame.extend_from_slice(&[0x08, 0x00]); // EtherType IPv4
    frame.extend_from_slice(&ip_hdr);
    frame.extend_from_slice(&udp);
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    const DST_MAC: [u8; 6] = [0xdc, 0xa6, 0x32, 0x01, 0x02, 0x03];

    fn try_sample(payload: &[u8]) -> Option<Vec<u8>> {
        build_udp_frame(
            SRC_MAC,
            DST_MAC,
            Ipv4Addr::new(192, 168, 99, 1),
            Ipv4Addr::new(192, 168, 99, 100),
            6969,
            45678,
            payload,
        )
    }

    fn sample(payload: &[u8]) -> Vec<u8> {
        try_sample(payload).expect("payload fits a datagram")
    }

    #[test]
    fn checksum_known_vector() {
        // RFC 1071 worked example bytes -> 0x220d.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(inet_checksum(&data), 0x220d);
    }

    #[test]
    fn ethernet_header_layout() {
        let f = sample(b"hi");
        assert_eq!(&f[0..6], &DST_MAC, "dst MAC must come first");
        assert_eq!(&f[6..12], &SRC_MAC, "src MAC preserved (not kernel-filled)");
        assert_eq!(&f[12..14], &[0x08, 0x00], "EtherType IPv4");
    }

    #[test]
    fn ip_header_fields_and_checksum_valid() {
        let payload = b"hello tftp";
        let f = sample(payload);
        let ip = &f[14..34];
        assert_eq!(ip[0], 0x45);
        assert_eq!(ip[9], 17, "protocol UDP");
        let total_len = u16::from_be_bytes([ip[2], ip[3]]);
        assert_eq!(total_len as usize, 20 + 8 + payload.len());
        // A correct IPv4 checksum: summing the whole header (incl. the checksum
        // field) and folding yields 0.
        assert_eq!(inet_checksum(ip), 0, "IP checksum must verify to 0");
    }

    #[test]
    fn udp_checksum_verifies_over_pseudo_header() {
        let payload = b"the quick brown fox jumps"; // odd length exercises padding
        let f = sample(payload);
        let src_a = [192, 168, 99, 1];
        let dst_a = [192, 168, 99, 100];
        let udp = &f[34..];
        let udp_len = udp.len() as u16;
        let mut v = Vec::new();
        v.extend_from_slice(&src_a);
        v.extend_from_slice(&dst_a);
        v.push(0);
        v.push(17);
        v.extend_from_slice(&udp_len.to_be_bytes());
        v.extend_from_slice(udp); // includes the real checksum field
        assert_eq!(inet_checksum(&v), 0, "UDP checksum must verify to 0");
    }

    #[test]
    fn total_length_matches() {
        let payload = vec![0xabu8; 1400];
        let f = sample(&payload);
        assert_eq!(f.len(), 14 + 20 + 8 + payload.len());
    }

    /// The length fields are 16 bits. A payload that would wrap them is refused
    /// rather than silently described as a much shorter datagram.
    #[test]
    fn oversized_payload_is_refused_not_truncated() {
        let max = vec![0u8; MAX_UDP_PAYLOAD];
        let f = try_sample(&max).expect("the largest legal payload builds");
        let ip = &f[14..34];
        assert_eq!(u16::from_be_bytes([ip[2], ip[3]]), 65_535);
        let udp_len = u16::from_be_bytes([f[38], f[39]]);
        assert_eq!(udp_len as usize, 8 + MAX_UDP_PAYLOAD);

        let too_big = vec![0u8; MAX_UDP_PAYLOAD + 1];
        assert!(try_sample(&too_big).is_none(), "one byte over must fail");
        // The old `as u16` cast would have produced a "valid" 8-byte-long UDP
        // header for a 65 536-byte payload; make sure that never comes back.
        let wrap = vec![0u8; 65_536];
        assert!(try_sample(&wrap).is_none());
    }
}
