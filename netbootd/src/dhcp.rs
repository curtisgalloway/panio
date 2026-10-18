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

//! Minimal DHCP server for panio netboot (PoC port of `_dhcp.py`).
//!
//! Broadcast-only replies — no raw sockets, no BPF. Handles
//! DISCOVER→OFFER and REQUEST→ACK for a single netboot client, advertising
//! the host as both TFTP server (option 66 / `siaddr`) and bootfile source.
//!
//! One lease, always the same: the client IP is derived from the host IP
//! inside the host's /24 ([`derive_client_ip`]) unless overridden, and a
//! REQUEST for any other address is NAKed rather than ACKed with a different
//! `yiaddr` ([`request_verdict`]). The listen socket is pinned to the netboot
//! interface (see `pin`), and a pin that fails is fatal — an unpinned DHCP
//! server on the wildcard address would answer DISCOVERs on the host's
//! primary NIC too.
//!
//! One *client*, too, enforced by hardware address rather than by address
//! alone: the lease above says which IP is handed out, [`accept_client`]
//! says who it is handed to. The first DISCOVER — or first REQUEST this
//! server actually answers — locks in that MAC as the active client for the
//! life of the process; a later request from a different MAC is not the
//! machine on the other end of this dedicated link, so it gets no OFFER,
//! ACK, or NAK at all — just a rate-limited `warn!` — rather than being
//! allowed to steal the lease, the ARP pin, or the MAC handed to TFTP's
//! raw-frame sender. A retransmission from the already-active MAC is
//! unaffected (`accept_client` is idempotent for it).
//!
//! What locks the client in is decided *after* the message type is, in
//! [`disposition`]: a message this server never answers — DHCPINFORM,
//! DHCPDECLINE, DHCPRELEASE, or a REQUEST addressed to another server
//! (option 54) — takes no lock, whoever sends it, so an unrelated host on
//! the link cannot claim the session with a packet that would never have
//! been served (#166). There is no lease timer to expire the lock: `panio
//! netboot start`/`stop` restarts this process per boot session, so swapping
//! the netboot target means restarting netbootd, clearing the lock along
//! with everything else this process held in memory.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::netcfg;
use crate::served::warn_rate_limited;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const HTYPE_ETHERNET: u8 = 1;
/// Hardware-address length of an Ethernet client. Anything else is not the
/// board on the other end of the link.
const HLEN_ETHERNET: u8 = 6;
const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

const OPT_SUBNET: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_VENDOR_SPECIFIC: u8 = 43;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_VENDOR_CLASS: u8 = 60;
const OPT_TFTP_SERVER: u8 = 66;
const OPT_BOOTFILE: u8 = 67;
const OPT_CLIENT_ARCH: u8 = 93;
const OPT_END: u8 = 255;

/// Vendor-class prefix a UEFI HTTP Boot client sends (option 60), e.g.
/// `HTTPClient:Arch:00019:UNDI:003000`. We branch on the prefix, not the full
/// string. The reply must echo a class beginning with this or EDK2 rejects the
/// offer as "not a valid HTTP boot offer".
const HTTP_CLIENT_CLASS: &str = "HTTPClient";

/// Vendor-class prefix a UEFI/legacy PXE client sends (option 60), e.g.
/// `PXEClient:Arch:00011:UNDI:003000` (arch 11 = ARM64 UEFI). Echoing it back is
/// often required for the UEFI PXE driver to accept a single-server offer.
const PXE_CLIENT_CLASS: &str = "PXEClient";

const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_NAK: u8 = 6;

const LEASE_SECONDS: u32 = 12 * 3600;
/// The lease's subnet mask. The whole netboot link is a fixed /24: the host
/// interface is configured with this mask (by `panio netboot start` and by
/// `netcfg::monitor_interface`), and the client is leased an address inside
/// it so the advertised router — the host — is reachable.
const SUBNET_MASK: [u8; 4] = [255, 255, 255, 0];
/// Last octet of the derived client IP (`_dhcp.py` leased `.100` on the
/// default `192.168.99.0/24`); `.101` when the host itself is `.100`.
const DEFAULT_CLIENT_OCTET: u8 = 100;

/// How often a "second client ignored" warning may repeat. A device that
/// keeps hammering DISCOVER on a link it does not hold the lease for should
/// leave one line every few seconds, not one per packet.
const REJECTED_CLIENT_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// The IP leased to the netboot client, derived from the host IP: the same
/// /24 with the last octet replaced by [`DEFAULT_CLIENT_OCTET`] — or one more
/// when the host is already that address. `192.168.99.1` → `192.168.99.100`,
/// so the default lease is unchanged from the Python server's.
pub fn derive_client_ip(host_ip: Ipv4Addr) -> Ipv4Addr {
    let [a, b, c, d] = host_ip.octets();
    let last = if d == DEFAULT_CLIENT_OCTET {
        DEFAULT_CLIENT_OCTET + 1
    } else {
        DEFAULT_CLIENT_OCTET
    };
    Ipv4Addr::new(a, b, c, last)
}

/// Refuse a client IP the lease could not make bootable: the reply carries a
/// `255.255.255.0` mask and `host_ip` as the router and TFTP/HTTP server, so
/// the client must sit in the host's /24, must not *be* the host, and must
/// not be the network or broadcast address.
pub fn validate_client_ip(host_ip: Ipv4Addr, client_ip: Ipv4Addr) -> Result<()> {
    let h = host_ip.octets();
    let c = client_ip.octets();
    if h[..3] != c[..3] {
        bail!(
            "client IP {client_ip} is not in the host's /24 ({}.{}.{}.0/24): the lease \
             advertises a 255.255.255.0 mask and {host_ip} as router and TFTP/HTTP \
             server, so the client could never reach them",
            h[0],
            h[1],
            h[2]
        );
    }
    if client_ip == host_ip {
        bail!("client IP {client_ip} is the host IP itself");
    }
    if c[3] == 0 || c[3] == 255 {
        bail!("client IP {client_ip} is the network or broadcast address of the /24");
    }
    Ok(())
}

/// Parsed view of the fields we care about from an inbound BOOTP/DHCP packet.
struct Request {
    xid: [u8; 4],
    chaddr: [u8; 16],
    /// `ciaddr` — the address a client in RENEWING/REBINDING (or INIT-REBOOT
    /// with some stacks) claims to already have. Zero for a fresh boot.
    ciaddr: Ipv4Addr,
    msg_type: u8,
    /// Option 50, the address a REQUEST asks for (SELECTING / INIT-REBOOT).
    requested_ip: Option<Ipv4Addr>,
    /// Option 54, the server a REQUEST is addressed to (SELECTING). A REQUEST
    /// naming another server is that server's business, not ours.
    server_id: Option<Ipv4Addr>,
    /// Option 60 vendor class identifier (e.g. `HTTPClient:Arch:00019:…`), used
    /// to distinguish a UEFI HTTP Boot client from the legacy Pi bootloader.
    vendor_class: Option<String>,
    /// Option 93 client system architecture (e.g. 19 = ARM64 UEFI HTTP). Parsed
    /// for logging / future multi-arch use; the reply branches on `vendor_class`.
    arch: Option<u16>,
}

impl Request {
    /// True when this is a UEFI HTTP Boot client (option 60 begins `HTTPClient`).
    fn is_http_client(&self) -> bool {
        self.vendor_class
            .as_deref()
            .is_some_and(|c| c.starts_with(HTTP_CLIENT_CLASS))
    }

    /// True when this is a PXE client (option 60 begins `PXEClient`).
    fn is_pxe_client(&self) -> bool {
        self.vendor_class
            .as_deref()
            .is_some_and(|c| c.starts_with(PXE_CLIENT_CLASS))
    }
}

fn ipv4_at(data: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::new(data[at], data[at + 1], data[at + 2], data[at + 3])
}

fn parse_request(data: &[u8]) -> Option<Request> {
    // Fixed BOOTP header is 236 bytes; options (magic + TLVs) follow.
    if data.len() < 240 || data[0] != BOOTREQUEST {
        return None;
    }
    // Only Ethernet clients with a 6-byte hardware address: `chaddr[..6]` is
    // what we ARP-pin and address raw frames to, so anything else would be
    // pinned to garbage.
    if data[1] != HTYPE_ETHERNET || data[2] != HLEN_ETHERNET {
        return None;
    }
    let mut xid = [0u8; 4];
    xid.copy_from_slice(&data[4..8]);
    let ciaddr = ipv4_at(data, 12);
    let mut chaddr = [0u8; 16];
    chaddr.copy_from_slice(&data[28..44]);

    let opts = &data[236..];
    if opts.len() < 4 || opts[0..4] != MAGIC {
        return None;
    }
    let mut msg_type = 0u8;
    let mut requested_ip = None;
    let mut server_id = None;
    let mut vendor_class = None;
    let mut arch = None;
    let mut i = 4;
    while i < opts.len() {
        let tag = opts[i];
        if tag == OPT_END {
            break;
        }
        if tag == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= opts.len() {
            break;
        }
        let len = opts[i + 1] as usize;
        let val_start = i + 2;
        let val_end = val_start + len;
        if val_end > opts.len() {
            break;
        }
        let val = &opts[val_start..val_end];
        match tag {
            OPT_MSG_TYPE if len >= 1 => msg_type = val[0],
            OPT_REQUESTED_IP if len == 4 => requested_ip = Some(ipv4_at(val, 0)),
            OPT_SERVER_ID if len == 4 => server_id = Some(ipv4_at(val, 0)),
            OPT_VENDOR_CLASS if len >= 1 => {
                vendor_class = Some(String::from_utf8_lossy(val).into_owned());
            }
            OPT_CLIENT_ARCH if len >= 2 => arch = Some(u16::from_be_bytes([val[0], val[1]])),
            _ => {}
        }
        i = val_end;
    }
    if msg_type == 0 {
        return None;
    }
    Some(Request {
        xid,
        chaddr,
        ciaddr,
        msg_type,
        requested_ip,
        server_id,
        vendor_class,
        arch,
    })
}

/// What to do with a DHCPREQUEST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The client wants our lease (or names no address): ACK it.
    Ack,
    /// The client asked us for an address we do not lease: NAK, so it goes
    /// back to DISCOVER instead of booting on a stale address we never ACKed.
    Nak,
    /// The REQUEST is addressed to another server (option 54): not ours.
    Ignore,
}

/// Decide a REQUEST's fate. Only `client_ip` is ever leased here, so a REQUEST
/// that names another address — in option 50 or by claiming it in `ciaddr` —
/// is refused rather than answered with an ACK whose `yiaddr` differs from
/// what the client asked for (RFC 2131 §4.3.2 says NAK).
fn request_verdict(req: &Request, server_ip: Ipv4Addr, client_ip: Ipv4Addr) -> Verdict {
    if req.server_id.is_some_and(|s| s != server_ip) {
        return Verdict::Ignore;
    }
    if req.requested_ip.is_some_and(|r| r != client_ip) {
        return Verdict::Nak;
    }
    if !req.ciaddr.is_unspecified() && req.ciaddr != client_ip {
        return Verdict::Nak;
    }
    Verdict::Ack
}

fn encode_option(buf: &mut Vec<u8>, tag: u8, value: &[u8]) {
    // A DHCP option length is one byte. Pushing `value.len() as u8` for a
    // longer value wraps mod 256 while the whole value still gets appended, so
    // every following TLV — including OPT_END — is parsed at the wrong
    // offset and the client sees a garbled options block. Option 67 is the
    // reachable case: it carries the operator's boot_file, or the generated
    // `http://host:port/file` URL on the HTTP Boot path. Truncate so the
    // stream stays well-formed, and say so loudly — a bootfile that does not
    // fit is an operator error, and a clear warning plus a short filename
    // beats a silently corrupt offer.
    let value = match value.len() > u8::MAX as usize {
        true => {
            warn!(
                "DHCP option {tag}: {} bytes exceeds the 255-byte option limit, truncating",
                value.len()
            );
            &value[..u8::MAX as usize]
        }
        false => value,
    };
    buf.push(tag);
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
}

/// What to advertise as the boot source in a reply. The legacy Pi/TFTP path and
/// the UEFI HTTP Boot path differ only here.
struct BootAdvert<'a> {
    /// Option 67 value (and, for `tftp`, the BOOTP `file` field): a bare filename
    /// for TFTP, a full `http://…` URL for HTTP Boot.
    boot_file: &'a str,
    /// Advertise the host as the TFTP next-server — option 66 (dotted-quad
    /// string) + a non-zero `siaddr` + the bootfile copied into the fixed `file`
    /// field. False for HTTP Boot (the URL in option 67 is self-contained, and
    /// it may overrun the 128-byte `file` field).
    tftp: bool,
    /// Echo a vendor class in option 60. UEFI HTTP Boot **requires** the reply's
    /// class to begin `HTTPClient` or EDK2 rejects the offer. None on the legacy
    /// Pi path.
    vendor_class: Option<&'a str>,
}

impl<'a> BootAdvert<'a> {
    /// Legacy Raspberry Pi / generic TFTP advertisement.
    fn tftp(boot_file: &'a str) -> Self {
        Self {
            boot_file,
            tftp: true,
            vendor_class: None,
        }
    }

    /// UEFI HTTP Boot: a `http://…` URL in option 67, `HTTPClient` echoed in
    /// option 60, no TFTP next-server.
    fn http(url: &'a str) -> Self {
        Self {
            boot_file: url,
            tftp: false,
            vendor_class: Some(HTTP_CLIENT_CLASS),
        }
    }

    /// PXE: the legacy TFTP advertisement plus the `PXEClient` option-60 echo.
    fn pxe(boot_file: &'a str) -> Self {
        Self {
            boot_file,
            tftp: true,
            vendor_class: Some(PXE_CLIENT_CLASS),
        }
    }
}

/// Construct the `http://host[:port]/file` URL advertised to a UEFI HTTP Boot
/// client in option 67. The port is omitted when it is the default 80, keeping
/// the common URL clean.
fn http_boot_url(host_ip: Ipv4Addr, port: u16, boot_file: &str) -> String {
    let file = boot_file.trim_start_matches('/');
    if port == 80 {
        format!("http://{host_ip}/{file}")
    } else {
        format!("http://{host_ip}:{port}/{file}")
    }
}

/// How `serve()` should answer one client, decided from its vendor class and
/// whether an HTTP endpoint is actually bound. Kept as a pure, unit-testable
/// step separate from the packet I/O.
enum BootChoice {
    /// Legacy Pi / generic TFTP.
    Tftp,
    /// UEFI PXE.
    Pxe,
    /// UEFI HTTP Boot, carrying the full `http://…` URL to advertise.
    Http(String),
    /// An `HTTPClient` asked for HTTP Boot, but no HTTP endpoint is
    /// available (the listener failed to bind, or was never started). Two
    /// bad answers were considered and rejected (issue #144): building an
    /// `http://` URL anyway offers a server that isn't there, so the fetch
    /// always fails after DHCP has already succeeded; answering with a plain
    /// TFTP/PXE reply doesn't help either, because EDK2's `HttpBootDxe`
    /// requires the reply's option 60 to echo `HTTPClient` before it accepts
    /// the offer at all — a non-HTTP reply is just rejected as invalid.
    /// Ignoring the request is the only answer that doesn't mislead the
    /// client; PXE and plain-TFTP clients on the same link never reach this
    /// arm, so they keep working exactly as before.
    Ignore,
}

/// Decide how to answer `req`, given the boot file and the HTTP endpoint
/// actually bound (`None` when HTTP is unavailable, `Some(port)` — the real,
/// OS-confirmed port, never the literal `0` a caller may have requested).
fn choose_boot(
    req: &Request,
    host_ip: Ipv4Addr,
    http_endpoint: Option<u16>,
    boot_file: &str,
) -> BootChoice {
    if req.is_http_client() {
        return match http_endpoint {
            Some(port) => BootChoice::Http(http_boot_url(host_ip, port, boot_file)),
            None => BootChoice::Ignore,
        };
    }
    if req.is_pxe_client() {
        return BootChoice::Pxe;
    }
    BootChoice::Tftp
}

/// The fixed BOOTP header of a reply to `req`: everything up to the options,
/// with the given `yiaddr` / `siaddr` and the client's `chaddr` echoed. The
/// broadcast flag is always set — our client has no address to unicast to.
fn reply_header(req: &Request, yiaddr: [u8; 4], siaddr: [u8; 4], file: &[u8; 128]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(236 + 64);
    pkt.extend_from_slice(&[BOOTREPLY, HTYPE_ETHERNET, HLEN_ETHERNET, 0]); // op, htype, hlen, hops
    pkt.extend_from_slice(&req.xid);
    pkt.extend_from_slice(&[0, 0]); // secs
    pkt.extend_from_slice(&[0x80, 0x00]); // flags: broadcast
    pkt.extend_from_slice(&[0; 4]); // ciaddr
    pkt.extend_from_slice(&yiaddr); // yiaddr
    pkt.extend_from_slice(&siaddr); // siaddr (next-server = TFTP, else 0)
    pkt.extend_from_slice(&[0; 4]); // giaddr
    pkt.extend_from_slice(&req.chaddr); // chaddr (16)
    pkt.extend_from_slice(&[0; 64]); // sname
    pkt.extend_from_slice(file); // file (null-padded)
    pkt
}

fn build_reply(
    req: &Request,
    msg_type: u8,
    server_ip: Ipv4Addr,
    client_ip: Ipv4Addr,
    advert: &BootAdvert,
) -> Vec<u8> {
    let server_b = server_ip.octets();
    let client_b = client_ip.octets();

    let mut opts = Vec::with_capacity(64);
    opts.extend_from_slice(&MAGIC);
    encode_option(&mut opts, OPT_MSG_TYPE, &[msg_type]);
    encode_option(&mut opts, OPT_SERVER_ID, &server_b);
    encode_option(&mut opts, OPT_LEASE, &LEASE_SECONDS.to_be_bytes());
    encode_option(&mut opts, OPT_SUBNET, &SUBNET_MASK);
    encode_option(&mut opts, OPT_ROUTER, &server_b);
    if advert.tftp {
        encode_option(&mut opts, OPT_TFTP_SERVER, server_ip.to_string().as_bytes());
    }
    if let Some(vc) = advert.vendor_class {
        encode_option(&mut opts, OPT_VENDOR_CLASS, vc.as_bytes());
    }
    // PXE: option 43 (vendor-specific) carrying PXE_DISCOVERY_CONTROL (tag 6) with
    // bit 3 (0x08) set tells the UEFI PXE client to download the bootfile named in
    // this very offer and skip boot-server (BINL) discovery. Without it EDK2 hunts
    // for a boot server on port 4011, gets no answer, and reports "no valid offer
    // returned". (`0xff` ends the option-43 sub-option list.)
    if advert.vendor_class == Some(PXE_CLIENT_CLASS) {
        encode_option(&mut opts, OPT_VENDOR_SPECIFIC, &[6, 1, 8, 0xff]);
    }
    encode_option(&mut opts, OPT_BOOTFILE, advert.boot_file.as_bytes());
    opts.push(OPT_END);

    // siaddr (next-server) only for the TFTP path; HTTP carries the full URL.
    let siaddr = if advert.tftp { server_b } else { [0u8; 4] };
    let mut file = [0u8; 128];
    if advert.tftp {
        let fb = advert.boot_file.as_bytes();
        let n = fb.len().min(127);
        file[..n].copy_from_slice(&fb[..n]);
    }

    let mut pkt = reply_header(req, client_b, siaddr, &file);
    pkt.extend_from_slice(&opts);
    pkt
}

/// A DHCPNAK for `req`: no address, no lease, just "not from this server"
/// (RFC 2131 table 3 — message type and server identifier are all a NAK
/// carries; `yiaddr`/`siaddr`/`file` are zero).
fn build_nak(req: &Request, server_ip: Ipv4Addr) -> Vec<u8> {
    let mut opts = Vec::with_capacity(16);
    opts.extend_from_slice(&MAGIC);
    encode_option(&mut opts, OPT_MSG_TYPE, &[DHCP_NAK]);
    encode_option(&mut opts, OPT_SERVER_ID, &server_ip.octets());
    opts.push(OPT_END);

    let mut pkt = reply_header(req, [0; 4], [0; 4], &[0u8; 128]);
    pkt.extend_from_slice(&opts);
    pkt
}

/// Format the first six bytes of `mac` (a `chaddr` or a bare hardware
/// address) as colon-separated hex.
fn mac_string(mac: &[u8]) -> String {
    mac[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The single-client gate: decide whether `mac` is the client this process
/// answers, updating `active` the first time it is called.
///
/// `active` starts `None` (no client seen yet). The first call locks `mac`
/// in as `Some(mac)` and returns `true`. Every later call with the *same*
/// `mac` (a retransmission) is idempotent — `active` is left unchanged and
/// it still returns `true`. A call with a *different* `mac` — a second
/// device on the link — returns `false` and leaves `active` alone: that MAC
/// never becomes the client, no matter how many times it asks.
fn accept_client(active: &mut Option<[u8; 6]>, mac: [u8; 6]) -> bool {
    match active {
        Some(a) => *a == mac,
        None => {
            *active = Some(mac);
            true
        }
    }
}

/// What `serve` does with one parsed BOOTREQUEST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// Answer with this DHCP message type; `label` names the request in the
    /// log line.
    Reply { msg_type: u8, label: &'static str },
    /// A DHCPREQUEST addressed to another server (option 54) — that server's
    /// business, not ours ([`Verdict::Ignore`]).
    OtherServer,
    /// A message type this server never answers: DHCPINFORM, DHCPDECLINE,
    /// DHCPRELEASE, or anything else that is not a DISCOVER or a REQUEST.
    Unserved,
    /// A message we would have answered, from a MAC that is not this
    /// session's client — which is carried here for the warning.
    NotTheClient { active: [u8; 6] },
}

/// Decide what to do with `req`, consulting (and, on the first served
/// request, setting) the single-client lock in `active`.
///
/// The order is the point. `parse_request` accepts any option-53 value, so a
/// DHCPINFORM, DHCPDECLINE, DHCPRELEASE, or a DHCPREQUEST addressed to some
/// other server can arrive from an unrelated host sharing the link. Taking
/// the lock before the message type was known let such a packet install a MAC
/// that this server would then never answer, and with no lease timer the real
/// target's DISCOVER was refused until netbootd restarted (#166). So the
/// message type is settled first: only a DISCOVER, or a REQUEST whose
/// [`Verdict`] is `Ack` or `Nak` — one this server does answer — ever reaches
/// [`accept_client`]. A NAKed REQUEST counts: a NAK is a reply, and sending
/// one commits this process to that client just as an OFFER does.
fn disposition(
    active: &mut Option<[u8; 6]>,
    req: &Request,
    server_ip: Ipv4Addr,
    client_ip: Ipv4Addr,
) -> Disposition {
    let (msg_type, label) = match req.msg_type {
        DHCP_DISCOVER => (DHCP_OFFER, "DHCPDISCOVER"),
        DHCP_REQUEST => match request_verdict(req, server_ip, client_ip) {
            Verdict::Ack => (DHCP_ACK, "DHCPREQUEST"),
            Verdict::Nak => (DHCP_NAK, "DHCPREQUEST"),
            Verdict::Ignore => return Disposition::OtherServer,
        },
        _ => return Disposition::Unserved,
    };
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&req.chaddr[..6]);
    if !accept_client(active, mac) {
        // `active` is always Some here: accept_client only refuses once a
        // client is locked in.
        return Disposition::NotTheClient {
            active: active.unwrap_or_default(),
        };
    }
    Disposition::Reply { msg_type, label }
}

/// Bind the broadcast-capable DHCP listen socket on `0.0.0.0:port`, pinned to
/// the netboot interface.
///
/// The wildcard bind keeps this rootless on macOS 14+ (and working through the
/// link flaps where the interface IP is momentarily gone); on Linux port 67
/// still needs root or `CAP_NET_BIND_SERVICE`. The pin is what confines the
/// server to the netboot link — it makes the limited-broadcast
/// `255.255.255.255` replies leave via that link instead of the default-route
/// interface, and it stops DISCOVERs arriving on any other interface from
/// being answered — so a pin that cannot be applied is fatal.
///
/// No `SO_REUSEADDR`: UDP has no TIME_WAIT, so a restart never needs it, and
/// on Linux it would let a second netbootd bind the same port on the same
/// interface and silently split the DISCOVERs with this one. With it off, a
/// duplicate fails here with EADDRINUSE. macOS uses `SO_REUSEPORT` with
/// a daemon-lifetime interface lock; see [`crate::pin::ListenerInterface`].
pub fn bind_server(port: u16, link: &crate::pin::ListenerInterface) -> Result<UdpSocket> {
    let interface = link.name();
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_broadcast(true)?;
    link.pin_listener(&sock)
        .with_context(|| format!("pin DHCP socket to interface {interface}"))?;
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    sock.bind(&addr.into()).with_context(|| {
        format!("bind DHCP port {port} (need root/CAP_NET_BIND_SERVICE on Linux)")
    })?;
    sock.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(sock.into())?)
}

/// Minimum gap between "ignoring HTTPClient, no HTTP endpoint" log warnings
/// for the same daemon, so a firmware that retries every few seconds doesn't
/// spam the log with one line per retry.
const HTTP_UNAVAILABLE_WARN_INTERVAL: Duration = Duration::from_secs(30);

/// Run the DHCP server on an already-bound listen socket until the task is
/// cancelled.
///
/// `mac_tx` publishes the client's hardware address (from `chaddr`) to the TFTP
/// task in-process, so the BPF send path can address frames to the Pi's real
/// DHCP MAC without the on-disk `client-mac` file the Python version needs.
///
/// `http_endpoint` is the port actually bound for HTTP, read back from the
/// listener by the caller — `None` when no HTTP listener exists at all (bind
/// failed). This must never be a caller-requested port that was not verified
/// to be live: see [`choose_boot`], which is what keeps an `HTTPClient`
/// request from ever being answered with an `http://` URL pointing at
/// nothing, or at port `0` (issue #144).
pub async fn serve(
    sock: UdpSocket,
    host_ip: Ipv4Addr,
    client_ip: Ipv4Addr,
    boot_file: String,
    interface: String,
    http_endpoint: Option<u16>,
    mac_tx: watch::Sender<Option<[u8; 6]>>,
) -> Result<()> {
    info!(
        %host_ip, %client_ip, boot_file,
        "DHCP listening on {} via {interface} (replies broadcast to 255.255.255.255)",
        sock.local_addr()?
    );

    // The single client this process will ever answer, locked in by
    // `disposition` on the first request it decides to answer — see the
    // module doc comment. There is no in-process reset; a new client means
    // restarting netbootd.
    let mut active_mac: Option<[u8; 6]> = None;
    let mut last_rejected_warn: Option<Instant> = None;

    let mut buf = vec![0u8; 4096];
    // Rate-limit the "ignoring HTTPClient, no HTTP endpoint" warning: a
    // firmware that gets no offer typically retries every few seconds, and
    // without this the log would fill with one warning per retry.
    let mut http_unavailable_warned_at: Option<Instant> = None;
    loop {
        let (n, _peer) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!("DHCP recv_from: {e}");
                continue;
            }
        };
        let Some(req) = parse_request(&buf[..n]) else {
            continue;
        };
        let mut mac_bytes = [0u8; 6];
        mac_bytes.copy_from_slice(&req.chaddr[..6]);
        let mac = mac_string(&mac_bytes);

        // The message type decides the reply, and only a request we are
        // going to answer takes (or has to match) the single-client lock.
        let (reply_type, label) = match disposition(&mut active_mac, &req, host_ip, client_ip) {
            Disposition::Reply { msg_type, label } => (msg_type, label),
            Disposition::OtherServer => {
                info!(
                    "DHCPREQUEST from {mac} is addressed to server {}; ignoring",
                    req.server_id.unwrap_or(Ipv4Addr::UNSPECIFIED)
                );
                continue;
            }
            Disposition::Unserved => continue,
            Disposition::NotTheClient { active } => {
                let active = mac_string(&active);
                warn_rate_limited(
                    &mut last_rejected_warn,
                    REJECTED_CLIENT_WARN_INTERVAL,
                    &format!(
                        "ignoring DHCP request from {mac}: the netboot lease is held by {active} \
                         for this session; restart netbootd to switch clients"
                    ),
                );
                continue;
            }
        };
        info!("{label} from {mac}");

        if reply_type == DHCP_NAK {
            let nak = build_nak(&req, host_ip);
            if let Err(e) = sock.send_to(&nak, (Ipv4Addr::BROADCAST, 68)).await {
                warn!("DHCP send_to 255.255.255.255:68 failed: {e}");
                continue;
            }
            info!(
                "DHCPNAK -> {mac}  requested={}  ciaddr={}  (only {client_ip} is leased here)",
                req.requested_ip
                    .map_or_else(|| "-".to_string(), |ip| ip.to_string()),
                req.ciaddr
            );
            continue;
        }

        // Decide what to answer before any side effect (MAC publish, ARP
        // pin) that only matters if we are actually going to reply. Branches
        // on the client's vendor class (option 60):
        //   * HTTPClient → self-contained http:// URL in option 67 + the required
        //     HTTPClient echo, served over HTTP — or, with no HTTP endpoint
        //     bound, ignored (see `BootChoice::Ignore`);
        //   * PXEClient  → the legacy TFTP reply plus a PXEClient option-60 echo;
        //   * neither (the silent Pi, generic TFTP) → the legacy reply unchanged.
        let boot = choose_boot(&req, host_ip, http_endpoint, &boot_file);
        let (advert, style) = match &boot {
            BootChoice::Http(url) => (BootAdvert::http(url), "http-boot"),
            BootChoice::Pxe => (BootAdvert::pxe(&boot_file), "pxe"),
            BootChoice::Tftp => (BootAdvert::tftp(&boot_file), "tftp"),
            BootChoice::Ignore => {
                let now = Instant::now();
                let should_warn = http_unavailable_warned_at
                    .is_none_or(|t| now.duration_since(t) >= HTTP_UNAVAILABLE_WARN_INTERVAL);
                if should_warn {
                    warn!(
                        "{label} from {mac} is HTTPClient but no HTTP listener is bound; \
                         ignoring rather than offering an http:// URL that points at \
                         nothing. Free --http-port or use PXE/TFTP instead."
                    );
                    http_unavailable_warned_at = Some(now);
                }
                continue;
            }
        };

        // Publish the client MAC to the TFTP task (for the macOS BPF send path).
        let _ = mac_tx.send_replace(Some(mac_bytes));

        // Pin the client's MAC so the host kernel can deliver to the silent
        // Pi bootloader (it never answers ARP). On macOS 15+ the kernel path is
        // unreliable for TFTP regardless — that's what the BPF send path covers.
        netcfg::set_arp(client_ip, &mac, &interface).await;

        let reply = build_reply(&req, reply_type, host_ip, client_ip, &advert);
        // RFC 2131: with the broadcast flag set and giaddr=0 (our single-client
        // direct link), reply to the limited broadcast 255.255.255.255 — not the
        // subnet-directed broadcast. A strict UEFI client (EDK2) at 0.0.0.0 drops
        // a packet addressed to a subnet it has no address on; the lenient Pi
        // firmware accepts either. The socket is pinned to the netboot interface
        // so this still egresses the netboot link.
        if let Err(e) = sock.send_to(&reply, (Ipv4Addr::BROADCAST, 68)).await {
            warn!("DHCP send_to 255.255.255.255:68 failed: {e}");
            continue;
        }
        let what = match reply_type {
            DHCP_OFFER => "DHCPOFFER",
            _ => "DHCPACK",
        };
        info!(
            "{what} -> {mac}  ip={client_ip}  {style}  arch={:?}  boot={}",
            req.arch, advert.boot_file
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XID: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
    const MAC6: [u8; 6] = [0xdc, 0xa6, 0x32, 0x01, 0x02, 0x03];
    /// The lease the tests hand out (what `derive_client_ip(192.168.99.1)` gives).
    const CLIENT: Ipv4Addr = Ipv4Addr::new(192, 168, 99, 100);

    /// Build a minimal but well-formed BOOTREQUEST/DHCP packet with the given
    /// message type. `trailing_opts` are appended (TLV bytes) before OPT_END.
    fn request_packet(op: u8, msg_type: Option<u8>, trailing_opts: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0u8; 236];
        pkt[0] = op;
        pkt[1] = HTYPE_ETHERNET;
        pkt[2] = 6; // hlen
        pkt[4..8].copy_from_slice(&XID);
        pkt[28..34].copy_from_slice(&MAC6); // chaddr (first 6 = MAC)
        pkt.extend_from_slice(&MAGIC);
        if let Some(mt) = msg_type {
            pkt.extend_from_slice(&[OPT_MSG_TYPE, 1, mt]);
        }
        pkt.extend_from_slice(trailing_opts);
        pkt.push(OPT_END);
        pkt
    }

    /// A DHCPREQUEST with the given option 50 / option 54 / `ciaddr`.
    fn request_for(
        requested: Option<Ipv4Addr>,
        server: Option<Ipv4Addr>,
        ciaddr: Ipv4Addr,
    ) -> Request {
        let mut opts = Vec::new();
        if let Some(r) = requested {
            opts.push(OPT_REQUESTED_IP);
            opts.push(4);
            opts.extend_from_slice(&r.octets());
        }
        if let Some(s) = server {
            opts.push(OPT_SERVER_ID);
            opts.push(4);
            opts.extend_from_slice(&s.octets());
        }
        let mut pkt = request_packet(BOOTREQUEST, Some(DHCP_REQUEST), &opts);
        pkt[12..16].copy_from_slice(&ciaddr.octets());
        parse_request(&pkt).expect("REQUEST parses")
    }

    /// Collect the options TLVs from a reply packet into (tag -> value) pairs,
    /// in wire order (so duplicates would be visible).
    fn reply_options(pkt: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let opts = &pkt[236..];
        assert_eq!(&opts[0..4], &MAGIC, "reply must start options with magic");
        let mut out = Vec::new();
        let mut i = 4;
        while i < opts.len() {
            let tag = opts[i];
            if tag == OPT_END {
                break;
            }
            let len = opts[i + 1] as usize;
            out.push((tag, opts[i + 2..i + 2 + len].to_vec()));
            i += 2 + len;
        }
        out
    }

    #[test]
    fn parse_discover_extracts_xid_mac_and_type() {
        let pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[]);
        let req = parse_request(&pkt).expect("valid DISCOVER should parse");
        assert_eq!(req.xid, XID);
        assert_eq!(&req.chaddr[..6], &MAC6);
        assert_eq!(req.msg_type, DHCP_DISCOVER);
        assert!(req.ciaddr.is_unspecified());
        assert_eq!(req.requested_ip, None);
        assert_eq!(req.server_id, None);
    }

    #[test]
    fn parse_skips_pad_options_before_msg_type() {
        // A leading run of pad (0) options must not derail the TLV walk: the
        // msg_type option sits after them.
        let mut p = vec![0u8; 236];
        p[0] = BOOTREQUEST;
        p[1] = HTYPE_ETHERNET;
        p[2] = 6;
        p[28..34].copy_from_slice(&MAC6);
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[0u8, 0, 0]); // three pad options
        p.extend_from_slice(&[OPT_MSG_TYPE, 1, DHCP_REQUEST]);
        p.push(OPT_END);
        let req = parse_request(&p).expect("pad-prefixed packet parses");
        assert_eq!(req.msg_type, DHCP_REQUEST);
    }

    #[test]
    fn parse_rejects_too_short() {
        assert!(parse_request(&[0u8; 239]).is_none(), "under 240 bytes");
    }

    #[test]
    fn parse_rejects_non_bootrequest() {
        let pkt = request_packet(BOOTREPLY, Some(DHCP_DISCOVER), &[]);
        assert!(parse_request(&pkt).is_none(), "op must be BOOTREQUEST");
    }

    #[test]
    fn parse_rejects_non_ethernet_htype() {
        let mut pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[]);
        pkt[1] = 6; // IEEE 802
        assert!(parse_request(&pkt).is_none(), "htype must be Ethernet (1)");
        pkt[1] = 0;
        assert!(parse_request(&pkt).is_none());
    }

    #[test]
    fn parse_rejects_hardware_address_length_other_than_six() {
        let mut pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[]);
        pkt[2] = 8; // an EUI-64 would not fit our 6-byte ARP pin / frame dst
        assert!(parse_request(&pkt).is_none(), "hlen must be 6");
        pkt[2] = 0;
        assert!(parse_request(&pkt).is_none());
        pkt[2] = 6;
        assert!(parse_request(&pkt).is_some(), "hlen 6 is the accepted case");
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[]);
        pkt[236] ^= 0xff; // corrupt the magic cookie
        assert!(
            parse_request(&pkt).is_none(),
            "wrong magic must be rejected"
        );
    }

    #[test]
    fn parse_rejects_missing_msg_type() {
        let pkt = request_packet(BOOTREQUEST, None, &[]);
        assert!(parse_request(&pkt).is_none(), "no option 53 -> reject");
    }

    #[test]
    fn parse_stops_at_truncated_tlv() {
        // A TLV whose length runs past the buffer must not panic or over-read;
        // the walk breaks and (with no msg_type seen) the packet is rejected.
        let bogus = [OPT_TFTP_SERVER, 200]; // claims 200 bytes, none follow
        let mut p = vec![0u8; 236];
        p[0] = BOOTREQUEST;
        p[1] = HTYPE_ETHERNET;
        p[2] = 6;
        p[28..34].copy_from_slice(&MAC6);
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&bogus);
        // no OPT_END, no msg_type
        assert!(parse_request(&p).is_none());
    }

    #[test]
    fn parse_extracts_ciaddr_requested_ip_and_server_id() {
        let req = request_for(
            Some(Ipv4Addr::new(10, 0, 0, 7)),
            Some(Ipv4Addr::new(10, 0, 0, 1)),
            Ipv4Addr::new(10, 0, 0, 9),
        );
        assert_eq!(req.requested_ip, Some(Ipv4Addr::new(10, 0, 0, 7)));
        assert_eq!(req.server_id, Some(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(req.ciaddr, Ipv4Addr::new(10, 0, 0, 9));
    }

    #[test]
    fn parse_ignores_malformed_requested_ip_and_server_id() {
        // Option 50 / 54 with a length other than 4 carry no address.
        let opts = [OPT_REQUESTED_IP, 2, 1, 2, OPT_SERVER_ID, 5, 1, 2, 3, 4, 5];
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_REQUEST), &opts)).unwrap();
        assert_eq!(req.requested_ip, None);
        assert_eq!(req.server_id, None);
    }

    // ── REQUEST verdict ──────────────────────────────────────────────────────

    const SERVER: Ipv4Addr = Ipv4Addr::new(192, 168, 99, 1);

    #[test]
    fn bare_request_is_acked() {
        let req = request_for(None, None, Ipv4Addr::UNSPECIFIED);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Ack);
    }

    #[test]
    fn request_for_our_lease_from_us_is_acked() {
        let req = request_for(Some(CLIENT), Some(SERVER), Ipv4Addr::UNSPECIFIED);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Ack);
        // Renewing with ciaddr = the lease is fine too.
        let req = request_for(None, None, CLIENT);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Ack);
    }

    #[test]
    fn request_addressed_to_another_server_is_ignored() {
        let other = Ipv4Addr::new(192, 168, 99, 2);
        let req = request_for(Some(CLIENT), Some(other), Ipv4Addr::UNSPECIFIED);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Ignore);
        // Even when the address it wants is one we would NAK: not our REQUEST.
        let req = request_for(
            Some(Ipv4Addr::new(10, 1, 1, 1)),
            Some(other),
            Ipv4Addr::UNSPECIFIED,
        );
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Ignore);
    }

    #[test]
    fn request_for_a_different_address_is_naked() {
        let stale = Ipv4Addr::new(192, 168, 99, 50);
        let req = request_for(Some(stale), None, Ipv4Addr::UNSPECIFIED);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Nak);
        let req = request_for(Some(stale), Some(SERVER), Ipv4Addr::UNSPECIFIED);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Nak);
    }

    #[test]
    fn request_claiming_a_different_ciaddr_is_naked() {
        let stale = Ipv4Addr::new(10, 0, 0, 5);
        let req = request_for(None, None, stale);
        assert_eq!(request_verdict(&req, SERVER, CLIENT), Verdict::Nak);
    }

    #[test]
    fn nak_carries_no_address_and_names_us() {
        let req = request_for(
            Some(Ipv4Addr::new(10, 0, 0, 5)),
            None,
            Ipv4Addr::UNSPECIFIED,
        );
        let nak = build_nak(&req, SERVER);
        assert_eq!(nak[0], BOOTREPLY);
        assert_eq!(&nak[4..8], &XID, "xid echoed");
        assert_eq!(&nak[10..12], &[0x80, 0x00], "NAKs are broadcast");
        assert_eq!(&nak[16..20], &[0, 0, 0, 0], "yiaddr zero");
        assert_eq!(&nak[20..24], &[0, 0, 0, 0], "siaddr zero");
        assert_eq!(&nak[28..34], &MAC6, "chaddr echoed");
        assert!(nak[108..236].iter().all(|&b| b == 0), "file field empty");
        let opts = reply_options(&nak);
        assert_eq!(
            opts,
            vec![
                (OPT_MSG_TYPE, vec![DHCP_NAK]),
                (OPT_SERVER_ID, SERVER.octets().to_vec()),
            ],
            "a NAK carries only message type + server id"
        );
    }

    // ── Single-client gate ───────────────────────────────────────────────────

    const MAC_A: [u8; 6] = [0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa];
    const MAC_B: [u8; 6] = [0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb];

    #[test]
    fn accept_client_locks_in_the_first_mac_seen() {
        let mut active = None;
        assert!(accept_client(&mut active, MAC_A));
        assert_eq!(active, Some(MAC_A));
    }

    #[test]
    fn accept_client_is_idempotent_for_a_retransmission_from_the_active_mac() {
        let mut active = Some(MAC_A);
        // Any number of repeats from the already-active MAC keep succeeding
        // and never disturb `active` — this is what keeps a retransmitted
        // DISCOVER/REQUEST from the real client working.
        for _ in 0..3 {
            assert!(accept_client(&mut active, MAC_A));
            assert_eq!(active, Some(MAC_A));
        }
    }

    #[test]
    fn accept_client_rejects_a_second_distinct_mac() {
        let mut active = Some(MAC_A);
        assert!(
            !accept_client(&mut active, MAC_B),
            "a different MAC must not be accepted"
        );
        // Rejecting it must not steal or clear the lock.
        assert_eq!(active, Some(MAC_A));
        // ...and the real client can still be served afterward.
        assert!(accept_client(&mut active, MAC_A));
        assert_eq!(active, Some(MAC_A));
    }

    /// A DHCPINFORM: parsed like any other BOOTREQUEST (option 53 = 8), never
    /// answered by this server. Declared here rather than beside the served
    /// message types because nothing outside these tests names it.
    const DHCP_INFORM: u8 = 8;
    /// Some other DHCP server on the link.
    const OTHER_SERVER: Ipv4Addr = Ipv4Addr::new(192, 168, 99, 2);

    /// A parsed BOOTREQUEST of `msg_type` from `mac`, carrying `trailing_opts`
    /// after the message-type option — the same shape `serve` hands
    /// `disposition` for every packet off the wire.
    fn request_from(mac: [u8; 6], msg_type: u8, trailing_opts: &[u8]) -> Request {
        let mut pkt = request_packet(BOOTREQUEST, Some(msg_type), trailing_opts);
        pkt[28..34].copy_from_slice(&mac);
        parse_request(&pkt).expect("request parses")
    }

    /// An option 54 (server identifier) TLV.
    fn server_id_opt(ip: Ipv4Addr) -> Vec<u8> {
        let mut opt = vec![OPT_SERVER_ID, 4];
        opt.extend_from_slice(&ip.octets());
        opt
    }

    /// An option 50 (requested address) TLV.
    fn requested_ip_opt(ip: Ipv4Addr) -> Vec<u8> {
        let mut opt = vec![OPT_REQUESTED_IP, 4];
        opt.extend_from_slice(&ip.octets());
        opt
    }

    /// An address this server does not lease, so a REQUEST naming it is NAKed.
    const STALE: Ipv4Addr = Ipv4Addr::new(192, 168, 99, 50);

    #[test]
    fn an_unanswered_message_type_does_not_lock_in_its_sender() {
        let mut active = None;

        // Some other host on the link speaks first, with a message this
        // server never answers.
        let inform = request_from(MAC_B, DHCP_INFORM, &[]);
        assert_eq!(
            disposition(&mut active, &inform, SERVER, CLIENT),
            Disposition::Unserved
        );
        assert_eq!(
            active, None,
            "a message we never answer must not take the lock"
        );

        // The real target then boots and must still be served.
        let discover = request_from(MAC_A, DHCP_DISCOVER, &[]);
        assert_eq!(
            disposition(&mut active, &discover, SERVER, CLIENT),
            Disposition::Reply {
                msg_type: DHCP_OFFER,
                label: "DHCPDISCOVER",
            }
        );
        assert_eq!(active, Some(MAC_A));
    }

    #[test]
    fn a_request_addressed_to_another_server_does_not_lock_in_its_sender() {
        let mut active = None;

        // Another host on the link accepting another server's offer: option 54
        // names that server, so this REQUEST is none of our business.
        let mut opts = requested_ip_opt(CLIENT);
        opts.extend_from_slice(&server_id_opt(OTHER_SERVER));
        let elsewhere = request_from(MAC_B, DHCP_REQUEST, &opts);
        assert_eq!(
            disposition(&mut active, &elsewhere, SERVER, CLIENT),
            Disposition::OtherServer
        );
        assert_eq!(
            active, None,
            "a REQUEST we do not answer must not take the lock"
        );

        let discover = request_from(MAC_A, DHCP_DISCOVER, &[]);
        assert_eq!(
            disposition(&mut active, &discover, SERVER, CLIENT),
            Disposition::Reply {
                msg_type: DHCP_OFFER,
                label: "DHCPDISCOVER",
            }
        );
        assert_eq!(active, Some(MAC_A));
    }

    #[test]
    fn a_served_message_locks_in_the_client_and_a_second_mac_gets_no_reply() {
        let mut active = None;
        let discover_a = request_from(MAC_A, DHCP_DISCOVER, &[]);
        assert_eq!(
            disposition(&mut active, &discover_a, SERVER, CLIENT),
            Disposition::Reply {
                msg_type: DHCP_OFFER,
                label: "DHCPDISCOVER",
            }
        );
        assert_eq!(active, Some(MAC_A));

        // A second device on the link gets nothing at all — not even the NAK
        // its REQUEST for an address we do not lease would otherwise earn.
        let discover_b = request_from(MAC_B, DHCP_DISCOVER, &[]);
        assert_eq!(
            disposition(&mut active, &discover_b, SERVER, CLIENT),
            Disposition::NotTheClient { active: MAC_A }
        );
        let request_b = request_from(MAC_B, DHCP_REQUEST, &requested_ip_opt(STALE));
        assert_eq!(
            disposition(&mut active, &request_b, SERVER, CLIENT),
            Disposition::NotTheClient { active: MAC_A }
        );
        assert_eq!(
            active,
            Some(MAC_A),
            "a rejected MAC must not steal or clear the lock"
        );

        // ...and the client's retransmission is answered exactly as before.
        assert_eq!(
            disposition(&mut active, &discover_a, SERVER, CLIENT),
            Disposition::Reply {
                msg_type: DHCP_OFFER,
                label: "DHCPDISCOVER",
            }
        );
        assert_eq!(active, Some(MAC_A));
    }

    #[test]
    fn a_request_we_nak_still_locks_in_the_client() {
        let mut active = None;
        let stale_request = request_from(MAC_A, DHCP_REQUEST, &requested_ip_opt(STALE));
        assert_eq!(
            disposition(&mut active, &stale_request, SERVER, CLIENT),
            Disposition::Reply {
                msg_type: DHCP_NAK,
                label: "DHCPREQUEST",
            }
        );
        assert_eq!(
            active,
            Some(MAC_A),
            "a NAK is a reply: sending one commits this process to that client"
        );
    }

    // ── Client IP derivation ─────────────────────────────────────────────────

    #[test]
    fn derive_client_ip_takes_dot_100_in_the_hosts_slash_24() {
        assert_eq!(derive_client_ip(Ipv4Addr::new(192, 168, 99, 1)), CLIENT);
        assert_eq!(
            derive_client_ip(Ipv4Addr::new(10, 20, 30, 1)),
            Ipv4Addr::new(10, 20, 30, 100)
        );
        assert_eq!(
            derive_client_ip(Ipv4Addr::new(172, 16, 5, 254)),
            Ipv4Addr::new(172, 16, 5, 100)
        );
    }

    #[test]
    fn derive_client_ip_steps_to_101_when_the_host_is_100() {
        assert_eq!(
            derive_client_ip(Ipv4Addr::new(192, 168, 99, 100)),
            Ipv4Addr::new(192, 168, 99, 101)
        );
    }

    #[test]
    fn derived_client_ip_always_validates() {
        for host in [
            Ipv4Addr::new(192, 168, 99, 1),
            Ipv4Addr::new(192, 168, 99, 100),
            Ipv4Addr::new(10, 0, 0, 254),
        ] {
            validate_client_ip(host, derive_client_ip(host)).unwrap();
        }
    }

    #[test]
    fn validate_client_ip_rejects_other_subnets() {
        let err = validate_client_ip(Ipv4Addr::new(10, 1, 2, 1), Ipv4Addr::new(10, 1, 3, 100))
            .unwrap_err();
        assert!(err.to_string().contains("10.1.2.0/24"), "{err}");
        assert!(validate_client_ip(
            Ipv4Addr::new(192, 168, 99, 1),
            Ipv4Addr::new(192, 168, 98, 100)
        )
        .is_err());
    }

    #[test]
    fn validate_client_ip_rejects_host_network_and_broadcast() {
        let host = Ipv4Addr::new(192, 168, 99, 1);
        assert!(validate_client_ip(host, host).is_err(), "client == host");
        assert!(validate_client_ip(host, Ipv4Addr::new(192, 168, 99, 0)).is_err());
        assert!(validate_client_ip(host, Ipv4Addr::new(192, 168, 99, 255)).is_err());
        validate_client_ip(host, Ipv4Addr::new(192, 168, 99, 2)).unwrap();
        validate_client_ip(host, Ipv4Addr::new(192, 168, 99, 254)).unwrap();
    }

    // ── Reply building ───────────────────────────────────────────────────────

    #[test]
    fn build_offer_header_fields() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        let server = Ipv4Addr::new(192, 168, 99, 1);
        let reply = build_reply(
            &req,
            DHCP_OFFER,
            server,
            CLIENT,
            &BootAdvert::tftp("kernel_2712.img"),
        );

        assert_eq!(reply[0], BOOTREPLY, "op = BOOTREPLY");
        assert_eq!(reply[1], HTYPE_ETHERNET);
        assert_eq!(reply[2], 6, "hlen");
        assert_eq!(&reply[4..8], &XID, "xid echoed back");
        assert_eq!(&reply[10..12], &[0x80, 0x00], "broadcast flag set");
        assert_eq!(&reply[16..20], &CLIENT.octets(), "yiaddr = assigned");
        assert_eq!(&reply[20..24], &server.octets(), "siaddr = next-server");
        assert_eq!(&reply[28..34], &MAC6, "chaddr echoed");
        // file field (offset 108, 128 bytes) carries the bootfile, null-padded.
        assert_eq!(&reply[108..123], b"kernel_2712.img");
        assert_eq!(reply[123], 0, "file field null-padded");
    }

    #[test]
    fn build_reply_leases_the_given_client_ip() {
        // A non-default host gets a lease in its own /24, not 192.168.99.100.
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        let host = Ipv4Addr::new(10, 20, 30, 1);
        let client = derive_client_ip(host);
        let reply = build_reply(&req, DHCP_OFFER, host, client, &BootAdvert::tftp("k.img"));
        assert_eq!(
            &reply[16..20],
            &[10, 20, 30, 100],
            "yiaddr in the host's /24"
        );
        let opts = reply_options(&reply);
        let get = |tag: u8| opts.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone());
        assert_eq!(get(OPT_ROUTER), Some(host.octets().to_vec()));
        assert_eq!(get(OPT_SUBNET), Some(vec![255, 255, 255, 0]));
    }

    #[test]
    fn build_offer_options_match_spec() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        let server = Ipv4Addr::new(10, 0, 0, 5);
        let reply = build_reply(
            &req,
            DHCP_OFFER,
            server,
            Ipv4Addr::new(10, 0, 0, 100),
            &BootAdvert::tftp("boot.img"),
        );
        let opts = reply_options(&reply);

        let get = |tag: u8| opts.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone());
        assert_eq!(get(OPT_MSG_TYPE), Some(vec![DHCP_OFFER]));
        assert_eq!(get(OPT_SERVER_ID), Some(server.octets().to_vec()));
        assert_eq!(get(OPT_LEASE), Some(LEASE_SECONDS.to_be_bytes().to_vec()));
        assert_eq!(get(OPT_SUBNET), Some(vec![255, 255, 255, 0]));
        assert_eq!(get(OPT_ROUTER), Some(server.octets().to_vec()));
        // Option 66 advertises the TFTP server as a dotted-quad STRING (not raw
        // bytes) — the Pi bootloader expects the text form.
        assert_eq!(get(OPT_TFTP_SERVER), Some(b"10.0.0.5".to_vec()));
        assert_eq!(get(OPT_BOOTFILE), Some(b"boot.img".to_vec()));
    }

    #[test]
    fn request_yields_ack_type() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_REQUEST), &[])).unwrap();
        let reply = build_reply(
            &req,
            DHCP_ACK,
            Ipv4Addr::new(192, 168, 99, 1),
            CLIENT,
            &BootAdvert::tftp("k.img"),
        );
        let opts = reply_options(&reply);
        let mt = opts.iter().find(|(t, _)| *t == OPT_MSG_TYPE).unwrap();
        assert_eq!(mt.1, vec![DHCP_ACK]);
    }

    #[test]
    fn build_reply_truncates_overlong_bootfile_in_file_field() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        let long = "a".repeat(200);
        let reply = build_reply(
            &req,
            DHCP_OFFER,
            Ipv4Addr::new(1, 2, 3, 4),
            Ipv4Addr::new(1, 2, 3, 100),
            &BootAdvert::tftp(&long),
        );
        // The fixed 128-byte file field holds at most 127 chars + a NUL terminator.
        assert_eq!(reply[108..108 + 127], long.as_bytes()[..127]);
        assert_eq!(reply[108 + 127], 0, "127th byte reserved for the NUL");
    }

    #[test]
    fn mac_string_formats_first_six_octets() {
        let mut chaddr = [0u8; 16];
        chaddr[..6].copy_from_slice(&MAC6);
        assert_eq!(mac_string(&chaddr), "dc:a6:32:01:02:03");
    }

    #[test]
    fn encode_option_writes_tag_len_value() {
        let mut buf = Vec::new();
        encode_option(&mut buf, OPT_LEASE, &[0, 0, 0x0e, 0x10]);
        assert_eq!(buf, vec![OPT_LEASE, 4, 0, 0, 0x0e, 0x10]);
    }

    /// A value past the one-byte length field must not wrap it: the length
    /// byte and the appended bytes have to agree, or every later TLV (OPT_END
    /// included) is read at the wrong offset and the offer is garbage.
    #[test]
    fn encode_option_truncates_oversized_value_keeping_tlv_consistent() {
        let mut buf = Vec::new();
        let long = vec![b'x'; 300];
        encode_option(&mut buf, OPT_BOOTFILE, &long);
        assert_eq!(buf[0], OPT_BOOTFILE);
        assert_eq!(
            buf[1], 255,
            "length byte saturates instead of wrapping to 44"
        );
        assert_eq!(
            buf.len(),
            2 + 255,
            "appended exactly as many bytes as the length byte claims"
        );
    }

    // ── UEFI HTTP Boot path ──────────────────────────────────────────────────

    /// Trailing option TLVs for an ARM64 UEFI HTTP Boot client: option 60
    /// vendor class + option 93 arch = 0x0013.
    fn http_client_opts() -> Vec<u8> {
        let class = b"HTTPClient:Arch:00019:UNDI:003000";
        let mut t = vec![OPT_VENDOR_CLASS, class.len() as u8];
        t.extend_from_slice(class);
        t.extend_from_slice(&[OPT_CLIENT_ARCH, 2, 0x00, 0x13]);
        t
    }

    #[test]
    fn parse_extracts_vendor_class_and_arch() {
        let pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &http_client_opts());
        let req = parse_request(&pkt).expect("HTTPClient DISCOVER parses");
        assert_eq!(
            req.vendor_class.as_deref(),
            Some("HTTPClient:Arch:00019:UNDI:003000")
        );
        assert_eq!(req.arch, Some(0x0013));
        assert!(req.is_http_client());
    }

    #[test]
    fn classless_request_is_not_http_client() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        assert_eq!(req.vendor_class, None);
        assert_eq!(req.arch, None);
        assert!(!req.is_http_client(), "the Pi/legacy path is not HTTP");
    }

    #[test]
    fn http_reply_echoes_class_and_url_without_tftp_fields() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        let server = Ipv4Addr::new(192, 168, 99, 1);
        let url = http_boot_url(server, 80, "grubaa64.efi");
        let reply = build_reply(&req, DHCP_OFFER, server, CLIENT, &BootAdvert::http(&url));
        let opts = reply_options(&reply);
        let get = |tag: u8| opts.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone());

        // The mandatory HTTPClient echo (EDK2 rejects the offer without it) and
        // the full URL as the bootfile.
        assert_eq!(get(OPT_VENDOR_CLASS), Some(b"HTTPClient".to_vec()));
        assert_eq!(
            get(OPT_BOOTFILE),
            Some(b"http://192.168.99.1/grubaa64.efi".to_vec())
        );
        // No TFTP next-server: option 66 absent, siaddr zeroed, file field empty.
        assert_eq!(
            get(OPT_TFTP_SERVER),
            None,
            "HTTP path must not set option 66"
        );
        assert_eq!(&reply[20..24], &[0, 0, 0, 0], "siaddr zero on HTTP path");
        assert_eq!(reply[108], 0, "file field empty for HTTP boot");
    }

    #[test]
    fn http_boot_url_formats() {
        let ip = Ipv4Addr::new(192, 168, 99, 1);
        assert_eq!(
            http_boot_url(ip, 80, "boot.efi"),
            "http://192.168.99.1/boot.efi",
            "port 80 omitted"
        );
        assert_eq!(
            http_boot_url(ip, 80, "/boot.efi"),
            "http://192.168.99.1/boot.efi",
            "leading slash trimmed"
        );
        assert_eq!(
            http_boot_url(ip, 8080, "boot.efi"),
            "http://192.168.99.1:8080/boot.efi",
            "non-default port kept"
        );
    }

    // ── HTTP endpoint plumbing (issue #144) ─────────────────────────────────

    /// With no HTTP endpoint bound, an `HTTPClient` request must not be
    /// answered with an `http://` URL — that would offer a server that isn't
    /// there. On the pre-fix code, `choose_boot` did not exist and the branch
    /// in `serve()` built `http_boot_url(host_ip, http_port, &boot_file)`
    /// unconditionally, so this is exactly the case that regresses without
    /// the fix.
    #[test]
    fn choose_boot_ignores_http_client_with_no_http_endpoint() {
        let pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &http_client_opts());
        let req = parse_request(&pkt).expect("HTTPClient DISCOVER parses");
        let server = Ipv4Addr::new(192, 168, 99, 1);

        let boot = choose_boot(&req, server, None, "grubaa64.efi");

        assert!(
            matches!(boot, BootChoice::Ignore),
            "no HTTP listener means no http:// URL to offer"
        );
    }

    /// A bound HTTP endpoint on a nonstandard port must be the port that
    /// actually appears in option 67 — not the default 80, and not whatever
    /// port was originally requested on the command line (which, for
    /// `--http-port 0`, is never the port that ends up bound).
    #[test]
    fn choose_boot_advertises_the_actual_bound_port_in_option_67() {
        let pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &http_client_opts());
        let req = parse_request(&pkt).expect("HTTPClient DISCOVER parses");
        let server = Ipv4Addr::new(192, 168, 99, 1);

        let boot = choose_boot(&req, server, Some(8123), "grubaa64.efi");
        let BootChoice::Http(url) = &boot else {
            panic!("expected BootChoice::Http, got a different choice");
        };
        assert_eq!(url, "http://192.168.99.1:8123/grubaa64.efi");

        let advert = BootAdvert::http(url);
        let reply = build_reply(&req, DHCP_OFFER, server, CLIENT, &advert);
        let opts = reply_options(&reply);
        let get = |tag: u8| opts.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone());

        assert_eq!(
            get(OPT_BOOTFILE),
            Some(b"http://192.168.99.1:8123/grubaa64.efi".to_vec()),
            "option 67 must carry the real bound port, not 80 or 0"
        );
        assert_eq!(get(OPT_VENDOR_CLASS), Some(b"HTTPClient".to_vec()));
    }

    /// A PXE client is unaffected by HTTP availability at all: `choose_boot`
    /// takes the PXE branch regardless of `http_endpoint`, so PXE/TFTP keep
    /// working when HTTP is intentionally unavailable.
    #[test]
    fn choose_boot_pxe_client_ignores_http_endpoint_state() {
        let pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &pxe_client_opts());
        let req = parse_request(&pkt).expect("PXEClient DISCOVER parses");
        let server = Ipv4Addr::new(192, 168, 99, 1);

        assert!(matches!(
            choose_boot(&req, server, None, "ipxe.efi"),
            BootChoice::Pxe
        ));
        assert!(matches!(
            choose_boot(&req, server, Some(8080), "ipxe.efi"),
            BootChoice::Pxe
        ));
    }

    // ── PXE path ─────────────────────────────────────────────────────────────

    /// Trailing option TLVs for an ARM64 UEFI PXE client: option 60 vendor class
    /// + option 93 arch = 0x000B (11).
    fn pxe_client_opts() -> Vec<u8> {
        let class = b"PXEClient:Arch:00011:UNDI:003000";
        let mut t = vec![OPT_VENDOR_CLASS, class.len() as u8];
        t.extend_from_slice(class);
        t.extend_from_slice(&[OPT_CLIENT_ARCH, 2, 0x00, 0x0B]);
        t
    }

    #[test]
    fn parse_classifies_pxe_client() {
        let pkt = request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &pxe_client_opts());
        let req = parse_request(&pkt).expect("PXEClient DISCOVER parses");
        assert_eq!(req.arch, Some(0x000B));
        assert!(req.is_pxe_client());
        assert!(!req.is_http_client(), "PXE is not HTTP");
    }

    #[test]
    fn pxe_reply_echoes_class_and_keeps_tftp_fields() {
        let req = parse_request(&request_packet(BOOTREQUEST, Some(DHCP_DISCOVER), &[])).unwrap();
        let server = Ipv4Addr::new(192, 168, 99, 1);
        let reply = build_reply(
            &req,
            DHCP_OFFER,
            server,
            CLIENT,
            &BootAdvert::pxe("ipxe.efi"),
        );
        let opts = reply_options(&reply);
        let get = |tag: u8| opts.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone());

        // PXEClient echo, alongside the legacy TFTP next-server fields.
        assert_eq!(get(OPT_VENDOR_CLASS), Some(b"PXEClient".to_vec()));
        // Option 43: PXE_DISCOVERY_CONTROL=0x08 (use this offer's bootfile, skip
        // boot-server discovery), terminated by 0xff.
        assert_eq!(get(OPT_VENDOR_SPECIFIC), Some(vec![6, 1, 8, 0xff]));
        assert_eq!(get(OPT_TFTP_SERVER), Some(b"192.168.99.1".to_vec()));
        assert_eq!(get(OPT_BOOTFILE), Some(b"ipxe.efi".to_vec()));
        assert_eq!(&reply[20..24], &server.octets(), "siaddr = next-server");
        assert_eq!(&reply[108..116], b"ipxe.efi", "bootfile in the file field");
    }
}
