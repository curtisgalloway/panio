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

//! Minimal read-only TFTP server for paniolo netboot (port of `_tftp.py`).
//!
//! Read-only (RRQ) per RFC 1350, with the `blksize` (RFC 2348) and `tsize`
//! (RFC 2349) options the Raspberry Pi bootloader negotiates.
//!
//! Delivery model:
//!   * **Listen socket pinning** — the port-69 socket is pinned to the netboot
//!     interface (see `pin`) so requests arriving on any other interface are
//!     never seen; a pin that fails is fatal.
//!   * **Egress pinning** — each reply socket is tied to the netboot interface.
//!     On macOS that's `IP_BOUND_IF` (survives the brief link-flap windows where
//!     the interface IP is momentarily absent). On Linux it is `SO_BINDTODEVICE`,
//!     the same pin the listen sockets use, applied even though the reply
//!     socket is created after the privilege drop (`SO_BINDTODEVICE` has not
//!     needed `CAP_NET_RAW` since kernel 5.7); a kernel older than that gets a
//!     logged `warn!` and falls back to the Python "first fix" of binding the
//!     reply socket to the interface IP alone. Both platforms also bind to the
//!     interface IP so the reply's source address is the one the client
//!     dialled ([`reply_pin_outcome`] is the pure fallback/fatal decision).
//!   * **Send path** — on macOS, once the DHCP handler has learned the client's
//!     MAC, every reply is injected as a raw Ethernet frame via [`BpfSender`]
//!     (we *always* prefer it when available: on Sequoia `send_to` reports
//!     success but silently misdelivers). ACKs are still received on the normal
//!     UDP reply socket. If BPF is unavailable we fall back to `send_to`.
//!
//! Robustness against the other end of the link:
//!   * a file is streamed one block at a time from disk, never read whole;
//!   * each retransmit attempt has a fixed deadline, so a peer that keeps
//!     sending junk cannot hold a transfer open past
//!     `MAX_RETRIES × ACK_TIMEOUT`;
//!   * a second RRQ from a client TID that already has a transfer in flight
//!     replaces that transfer instead of adding a parallel sender;
//!   * when replies go out as raw frames the negotiated `blksize` is capped so
//!     a DATA block always fits one Ethernet frame ([`cap_blksize`]).
//!
//! Two more gates on every inbound request, ahead of all of the above:
//!   * **peer identity** — a request whose source address is not the one IP
//!     DHCP ever leases on this link is not from the netboot client, so it is
//!     ignored outright (no reply of any kind);
//!   * **transfer slots** — concurrent transfers are bounded by a
//!     `MAX_TRANSFERS`-permit [`tokio::sync::Semaphore`], acquired before a
//!     task is spawned and held *inside* that task so it is released however
//!     the transfer ends (completion, error, or `abort()` on replacement). A
//!     request that finds every slot taken is dropped rather than queued —
//!     TFTP has no way to tell a client "try again shortly" — so a flood of
//!     unique-source RRQs cannot grow tasks, sockets, or open files without
//!     bound.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::net::UdpSocket;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};
use tracing::{info, warn};

use crate::bpf::BpfSender;
use crate::pin::pin_socket_to_interface;
use crate::served::{loggable, resolve, warn_rate_limited};

const OP_RRQ: u16 = 1;
const OP_WRQ: u16 = 2;
const OP_DATA: u16 = 3;
const OP_ACK: u16 = 4;
const OP_ERROR: u16 = 5;
const OP_OACK: u16 = 6;

const ERR_NOT_FOUND: u16 = 1;
const ERR_ACCESS: u16 = 2;
const ERR_ILLEGAL: u16 = 4;

const DEFAULT_BLKSIZE: usize = 512;
const ACK_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_RETRIES: usize = 6;
/// The largest DATA payload that fits one untagged Ethernet frame:
/// 1500 (MTU) − 20 (IPv4) − 8 (UDP) − 4 (TFTP opcode + block). The raw-frame
/// sender builds a single frame per packet with DF set (`frame.rs`), so a
/// bigger block could neither be fragmented nor transmitted.
const MAX_RAW_FRAME_BLKSIZE: usize = 1468;

/// How many transfers `run` will hold open at once. The Pi 5 EEPROM's
/// request sequence (`docs/netboot.md`) is sequential — `start.elf` (404),
/// `config.txt`, the `.dtb`, then the kernel — and UEFI PXE/HTTP Boot is
/// similar, so no supported boot flow needs more than a couple of transfers
/// overlapping at once. This bounds the worst case (a flood of RRQs from
/// unique source ports), not the happy path, so it is set a little above
/// that: enough headroom for a boot flow's files plus one in-flight retry
/// during a replacement, without leaving the cap so high that a flood could
/// still pile up a meaningful number of held files and sockets.
const MAX_TRANSFERS: usize = 4;

/// How often a "peer not leased" or "no transfer slots" warning may repeat,
/// so a sustained flood of rejected RRQs produces one log line every few
/// seconds instead of one per packet.
const REJECTED_PEER_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// The `blksize` to serve with. A client may ask for up to 65 464 bytes (RFC
/// 2348); that is fine through the kernel, which fragments, but a DATA block
/// sent as a raw frame must fit one Ethernet frame, so it is capped at
/// [`MAX_RAW_FRAME_BLKSIZE`] whenever raw frames are what will go out. The
/// OACK echoes the capped value, which is how TFTP negotiates down.
fn cap_blksize(requested: usize, raw_frames: bool) -> usize {
    if raw_frames {
        requested.min(MAX_RAW_FRAME_BLKSIZE)
    } else {
        requested
    }
}

/// Per-transfer context shared by the send helpers.
#[derive(Clone)]
struct Xfer {
    host_ip: Ipv4Addr,
    bpf: Arc<BpfSender>,
    client_mac: Option<[u8; 6]>,
    /// How long one attempt waits for its ACK before the block is resent.
    /// [`ACK_TIMEOUT`] in service; the tests shrink it.
    ack_timeout: Duration,
}

impl Xfer {
    /// Whether replies will leave as raw frames (BPF available *and* the
    /// client MAC known) rather than through the kernel.
    fn raw_frames(&self) -> bool {
        self.bpf.available() && self.client_mac.is_some()
    }
}

struct Rrq {
    filename: String,
    mode: String,
    blksize: Option<usize>,
    want_tsize: bool,
}

fn parse_rrq(data: &[u8]) -> Option<Rrq> {
    // Skip the 2-byte opcode; the rest is NUL-separated strings:
    // filename, mode, [opt, value]...
    let body = &data[2..];
    let mut parts = body.split(|&b| b == 0);
    let filename = String::from_utf8_lossy(parts.next()?).to_string();
    let mode = String::from_utf8_lossy(parts.next()?).to_lowercase();

    let mut blksize = None;
    let mut want_tsize = false;
    loop {
        let key = match parts.next() {
            Some(k) if !k.is_empty() => String::from_utf8_lossy(k).to_lowercase(),
            _ => break,
        };
        let Some(val) = parts.next() else { break };
        let val = String::from_utf8_lossy(val);
        match key.as_str() {
            "blksize" => {
                if let Ok(req) = val.parse::<usize>() {
                    blksize = Some(req.clamp(8, 65464));
                }
            }
            "tsize" => want_tsize = true,
            _ => {}
        }
    }
    Some(Rrq {
        filename,
        mode,
        blksize,
        want_tsize,
    })
}

fn error_packet(code: u16, msg: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(5 + msg.len());
    p.extend_from_slice(&OP_ERROR.to_be_bytes());
    p.extend_from_slice(&code.to_be_bytes());
    p.extend_from_slice(msg.as_bytes());
    p.push(0);
    p
}

/// What to do with the result of pinning a Linux TFTP reply socket to the
/// netboot interface (`pin_socket_to_interface`'s `Err`, if any).
///
/// The reply socket is created per transfer, inside `tftp::serve`, which
/// only starts after `privdrop::drop_privileges` has already given up root
/// (see `main.rs`'s startup order). `SO_BINDTODEVICE` has not needed
/// `CAP_NET_RAW` since Linux 5.7 — every kernel paniolo targets (Pi OS
/// Trixie is 6.x, Debian 12 is 6.1) — so the pin still succeeds after the
/// drop there. A kernel older than that answers `EPERM`/`EACCES`, which is
/// the one case worth falling back on rather than failing the transfer: it
/// is an expected, identifiable "too old to pin post-drop" condition, not a
/// sign the interface itself is wrong. Any other error (a bad interface
/// name, `ENODEV`, …) means the interface is wrong and stays fatal, as it
/// already is on macOS.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
enum ReplyPin {
    /// The pin succeeded.
    Pinned,
    /// A permission error: assume a pre-5.7 kernel running after the
    /// privilege drop, and carry on with the pre-fix behavior (bind by IP,
    /// let the kernel route) instead of failing the transfer.
    Fallback(String),
    /// Any other pin failure: the interface itself is wrong. Fatal, as on
    /// macOS.
    Fatal,
}

/// Pure decision for [`ReplyPin`] from the `Err` (if any) of a
/// `pin_socket_to_interface` call. `None` means the pin succeeded.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
fn reply_pin_outcome(err: Option<&io::Error>) -> ReplyPin {
    match err {
        None => ReplyPin::Pinned,
        Some(e) if e.kind() == io::ErrorKind::PermissionDenied => ReplyPin::Fallback(format!(
            "{e} (needs Linux 5.7+ for SO_BINDTODEVICE without CAP_NET_RAW after the \
             privilege drop)"
        )),
        Some(_) => ReplyPin::Fatal,
    }
}

/// Create a reply socket pinned to the netboot interface (`None` — the
/// loopback tests — leaves it unpinned). On Linux, an old kernel that
/// cannot pin post-privilege-drop falls back to binding by IP alone instead
/// of failing the transfer; see [`reply_pin_outcome`].
fn bind_reply_socket(host_ip: Ipv4Addr, interface: Option<&str>) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    #[cfg(target_os = "macos")]
    {
        // Egress is pinned via IP_BOUND_IF, not the bind address, so host_ip is
        // unused here. Bind a wildcard ephemeral port so we do not depend on the
        // interface IP being present at this instant. A pin that fails aborts
        // this transfer: an unpinned reply socket could send the block out of
        // the wrong interface and receive ACKs on any of them.
        let _ = host_ip;
        if let Some(iface) = interface {
            pin_socket_to_interface(&sock, iface)
                .with_context(|| format!("pin TFTP reply socket to {iface}"))?;
        }
        let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        sock.bind(&addr.into())
            .context("bind reply socket (wildcard ephemeral)")?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Pin first, with the same SO_BINDTODEVICE the listen sockets use
        // (see the doc comment on ReplyPin for why this still works after
        // the privilege drop, and what happens when it can't). Then bind to
        // the interface IP regardless of whether the pin took: the pin
        // alone fixes egress, not the *source address* the client's TID is
        // keyed to, so both together are the belt-and-braces combination —
        // the pin keeps the reply on the right wire even when another
        // netboot link's route would otherwise win (the two-links-same-/24
        // case in issue #109), and the IP bind keeps the source address the
        // one the client dialled.
        if let Some(iface) = interface {
            let pin_result = pin_socket_to_interface(&sock, iface);
            match reply_pin_outcome(pin_result.as_ref().err()) {
                ReplyPin::Pinned => {}
                ReplyPin::Fallback(reason) => {
                    warn!(
                        "pin TFTP reply socket to {iface}: {reason}; falling back to \
                         unpinned (source address alone selects the link)"
                    );
                }
                ReplyPin::Fatal => {
                    pin_result.with_context(|| format!("pin TFTP reply socket to {iface}"))?;
                }
            }
        }
        let addr: SocketAddr = SocketAddr::new(host_ip.into(), 0);
        sock.bind(&addr.into())
            .with_context(|| format!("bind reply socket to {host_ip}:0"))?;
    }

    sock.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(sock.into())?)
}

/// Send one packet to `peer`, preferring the BPF raw-frame path on macOS when a
/// client MAC is known, otherwise ordinary `send_to`.
async fn send_pkt(sock: &UdpSocket, packet: &[u8], peer: SocketAddr, xfer: &Xfer) -> Result<()> {
    // Referenced on all platforms so the fields are never "unused" on Linux.
    let _ = (&xfer.bpf, xfer.client_mac, xfer.host_ip);

    #[cfg(target_os = "macos")]
    {
        if xfer.bpf.available() {
            if let (Some(dst_mac), SocketAddr::V4(p)) = (xfer.client_mac, peer) {
                let src_port = sock.local_addr()?.port();
                if xfer
                    .bpf
                    .send_udp(dst_mac, xfer.host_ip, *p.ip(), src_port, p.port(), packet)
                {
                    return Ok(());
                }
                // BPF write failed — fall through to the kernel path.
            }
        }
    }

    sock.send_to(packet, peer).await?;
    Ok(())
}

/// Send a packet and wait for an ACK of `expect_block`, retransmitting on
/// timeout. Returns Ok(true) on ACK, Ok(false) on give-up/peer error.
///
/// Each attempt gets one fixed deadline, `ack_timeout` after its send. A
/// datagram that is not the ACK we want (a duplicate ACK, a stray packet,
/// deliberate junk) is skipped *without* touching that deadline, so the most
/// a peer can make this wait is `MAX_RETRIES × ack_timeout` — it cannot keep
/// a transfer alive by sending anything but the ACK.
async fn send_and_wait_ack(
    sock: &UdpSocket,
    packet: &[u8],
    peer: SocketAddr,
    expect_block: u16,
    xfer: &Xfer,
) -> Result<bool> {
    let mut ackbuf = [0u8; 64];
    for _ in 0..MAX_RETRIES {
        send_pkt(sock, packet, peer, xfer).await?;
        let deadline = Instant::now() + xfer.ack_timeout;
        loop {
            match timeout_at(deadline, sock.recv_from(&mut ackbuf)).await {
                Ok(Ok((n, raddr))) => {
                    if raddr != peer || n < 4 {
                        continue;
                    }
                    let opcode = u16::from_be_bytes([ackbuf[0], ackbuf[1]]);
                    let block = u16::from_be_bytes([ackbuf[2], ackbuf[3]]);
                    if opcode == OP_ACK && block == expect_block {
                        return Ok(true);
                    }
                    if opcode == OP_ERROR {
                        warn!("ERROR from {peer} waiting for ACK of block {expect_block}");
                        return Ok(false);
                    }
                    // Not our ACK; keep waiting out the same deadline.
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break, // deadline passed → retransmit
            }
        }
    }
    Ok(false)
}

/// Read the block at `offset` (up to `blksize` bytes; fewer at end of file)
/// into `buf`, replacing its contents.
async fn read_block(
    file: &mut File,
    offset: u64,
    blksize: usize,
    buf: &mut Vec<u8>,
) -> std::io::Result<()> {
    buf.clear();
    file.seek(SeekFrom::Start(offset)).await?;
    file.take(blksize as u64).read_to_end(buf).await?;
    Ok(())
}

async fn handle_rrq(
    root: PathBuf,
    data: Vec<u8>,
    peer: SocketAddr,
    interface: Option<String>,
    xfer: Xfer,
) {
    let sock = match bind_reply_socket(xfer.host_ip, interface.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            warn!("{e:#}");
            return;
        }
    };
    let Some(rrq) = parse_rrq(&data) else {
        let _ = send_pkt(
            &sock,
            &error_packet(ERR_ILLEGAL, "malformed request"),
            peer,
            &xfer,
        )
        .await;
        return;
    };
    // The filename comes off the wire; only its sanitized form reaches the log.
    let shown = loggable(&rrq.filename);
    if rrq.mode != "octet" {
        let _ = send_pkt(
            &sock,
            &error_packet(ERR_ILLEGAL, "unsupported mode"),
            peer,
            &xfer,
        )
        .await;
        return;
    }

    let path = match resolve(&root, &rrq.filename) {
        Some(p) if p.is_file() => p,
        _ => {
            info!("RRQ {shown} from {peer} -> NOT FOUND");
            let _ = send_pkt(
                &sock,
                &error_packet(ERR_NOT_FOUND, "file not found"),
                peer,
                &xfer,
            )
            .await;
            return;
        }
    };

    // Open once and stream block by block: boot payloads run to tens of MB and
    // there is no reason to hold one in memory per request.
    let (mut file, size) = match File::open(&path).await {
        Ok(f) => match f.metadata().await {
            Ok(m) => (f, m.len()),
            Err(e) => {
                warn!("stat {}: {e}", path.display());
                let _ = send_pkt(
                    &sock,
                    &error_packet(ERR_NOT_FOUND, "read error"),
                    peer,
                    &xfer,
                )
                .await;
                return;
            }
        },
        Err(e) => {
            warn!("open {}: {e}", path.display());
            let _ = send_pkt(
                &sock,
                &error_packet(ERR_NOT_FOUND, "read error"),
                peer,
                &xfer,
            )
            .await;
            return;
        }
    };
    let blksize = cap_blksize(rrq.blksize.unwrap_or(DEFAULT_BLKSIZE), xfer.raw_frames());

    info!("RRQ {shown} from {peer} -> serving {size} bytes (blksize={blksize})");

    // OACK if the client requested any option we honor.
    if rrq.blksize.is_some() || rrq.want_tsize {
        let mut oack = Vec::new();
        oack.extend_from_slice(&OP_OACK.to_be_bytes());
        if rrq.blksize.is_some() {
            oack.extend_from_slice(b"blksize\0");
            oack.extend_from_slice(blksize.to_string().as_bytes());
            oack.push(0);
        }
        if rrq.want_tsize {
            oack.extend_from_slice(b"tsize\0");
            oack.extend_from_slice(size.to_string().as_bytes());
            oack.push(0);
        }
        match send_and_wait_ack(&sock, &oack, peer, 0, &xfer).await {
            Ok(true) => {}
            _ => {
                warn!("no ACK for OACK from {peer}");
                return;
            }
        }
    }

    // DATA/ACK loop. Block numbers wrap at 0xFFFF. Each block is read from the
    // file at its offset just before it is first sent; a retransmit resends
    // those same bytes (a peer whose ACK was lost must get an identical block).
    let mut block: u16 = 1;
    let mut offset = 0u64;
    let mut chunk = Vec::with_capacity(blksize);
    let mut packet = Vec::with_capacity(4 + blksize);
    loop {
        if let Err(e) = read_block(&mut file, offset, blksize, &mut chunk).await {
            warn!("read {} at {offset}: {e}", path.display());
            let _ = send_pkt(
                &sock,
                &error_packet(ERR_NOT_FOUND, "read error"),
                peer,
                &xfer,
            )
            .await;
            return;
        }
        packet.clear();
        packet.extend_from_slice(&OP_DATA.to_be_bytes());
        packet.extend_from_slice(&block.to_be_bytes());
        packet.extend_from_slice(&chunk);

        match send_and_wait_ack(&sock, &packet, peer, block, &xfer).await {
            Ok(true) => {}
            _ => {
                warn!("transfer of {shown} to {peer} failed at block {block}");
                return;
            }
        }
        offset += chunk.len() as u64;
        block = block.wrapping_add(1);
        if chunk.len() < blksize {
            break; // last (possibly empty) block was ACKed
        }
    }
    info!("completed {shown} to {peer}");
}

/// Bind the TFTP listen socket on `0.0.0.0:port`, pinned to the netboot
/// interface. The pin is what keeps the file server off every other interface
/// on the host, so a pin that cannot be applied is fatal.
///
/// No `SO_REUSEADDR`: UDP has no TIME_WAIT to wait out on restart, and on
/// Linux it would let a second netbootd bind the same port on the same
/// interface and silently take some of the requests. Without it a duplicate
/// fails here with EADDRINUSE. macOS uses `SO_REUSEPORT` with
/// a daemon-lifetime interface lock; see [`crate::pin::ListenerInterface`].
pub fn bind_server(port: u16, link: &crate::pin::ListenerInterface) -> Result<UdpSocket> {
    let interface = link.name();
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    link.pin_listener(&sock)
        .with_context(|| format!("pin TFTP socket to interface {interface}"))?;
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    sock.bind(&addr.into()).with_context(|| {
        format!("bind TFTP port {port} (need root/CAP_NET_BIND_SERVICE on Linux)")
    })?;
    sock.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(sock.into())?)
}

/// Run the TFTP server on an already-bound listen socket until the task is
/// cancelled.
pub async fn serve(
    sock: UdpSocket,
    host_ip: Ipv4Addr,
    client_ip: Ipv4Addr,
    root: PathBuf,
    interface: String,
    bpf: Arc<BpfSender>,
    mac_rx: watch::Receiver<Option<[u8; 6]>>,
) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("TFTP root {} does not exist", root.display()))?;
    info!(
        %host_ip, %client_ip,
        root = %root.display(),
        bpf = bpf.available(),
        "TFTP listening on {} via {interface}",
        sock.local_addr()?
    );
    run(
        sock,
        root,
        host_ip,
        client_ip,
        Some(interface),
        bpf,
        mac_rx,
        ACK_TIMEOUT,
    )
    .await
}

/// The request dispatcher: one task per RRQ, keyed by the client's TID (its
/// source address) so a repeated RRQ from a TID with a transfer in flight
/// replaces that transfer — a client that restarted its request gets one
/// sender, not one per attempt.
///
/// Every request is gated twice before that: its source IP must be
/// `client_ip` — the one address DHCP ever leases on this link — and a
/// `MAX_TRANSFERS`-permit semaphore must have a slot free. Either failing
/// drops the request silently (bar a rate-limited `warn!`): TFTP has no NAK,
/// and queuing a rejected request would just build a backlog instead of
/// bounding one.
#[allow(clippy::too_many_arguments)]
async fn run(
    sock: UdpSocket,
    root: PathBuf,
    host_ip: Ipv4Addr,
    client_ip: Ipv4Addr,
    interface: Option<String>,
    bpf: Arc<BpfSender>,
    mac_rx: watch::Receiver<Option<[u8; 6]>>,
    ack_timeout: Duration,
) -> Result<()> {
    let mut buf = vec![0u8; 4096];
    let mut inflight: HashMap<SocketAddr, JoinHandle<()>> = HashMap::new();
    let transfers = Arc::new(Semaphore::new(MAX_TRANSFERS));
    // `served::warn_rate_limited` works in `std::time::Instant` (shared with
    // dhcp.rs, which has no async runtime clock to prefer); everything else
    // in this file uses tokio's `Instant` for `timeout_at` deadlines.
    let mut last_peer_warn: Option<std::time::Instant> = None;
    let mut last_slots_warn: Option<std::time::Instant> = None;
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!("TFTP recv_from: {e}");
                continue;
            }
        };
        if n < 2 {
            continue;
        }
        if peer.ip() != IpAddr::V4(client_ip) {
            warn_rate_limited(
                &mut last_peer_warn,
                REJECTED_PEER_WARN_INTERVAL,
                &format!("TFTP request from {peer}, not the leased client {client_ip}; ignoring"),
            );
            continue;
        }
        let opcode = u16::from_be_bytes([buf[0], buf[1]]);
        // Snapshot the client MAC learned by DHCP (stable for the transfer).
        let xfer = Xfer {
            host_ip,
            bpf: bpf.clone(),
            client_mac: *mac_rx.borrow(),
            ack_timeout,
        };
        match opcode {
            OP_RRQ => {
                inflight.retain(|_, h| !h.is_finished());
                // Acquire the slot before touching any existing transfer for
                // this peer: if the daemon is already at MAX_TRANSFERS, a
                // replacement RRQ is refused exactly like a new one rather
                // than tearing down the old transfer for nothing.
                let Ok(permit) = transfers.clone().try_acquire_owned() else {
                    warn_rate_limited(
                        &mut last_slots_warn,
                        REJECTED_PEER_WARN_INTERVAL,
                        &format!(
                            "TFTP RRQ from {peer} dropped: all {MAX_TRANSFERS} transfer slots \
                             are in use"
                        ),
                    );
                    continue;
                };
                if let Some(old) = inflight.remove(&peer) {
                    info!("RRQ from {peer} while a transfer to it is in flight; replacing it");
                    old.abort();
                }
                let data = buf[..n].to_vec();
                let root = root.clone();
                let interface = interface.clone();
                // The permit lives inside the task, not the dispatcher: it is
                // dropped — and the slot freed — whenever this future stops
                // running, on any exit path (`handle_rrq` returning, or
                // `abort()` above cancelling a future replacement).
                let task = tokio::spawn(async move {
                    let _permit = permit;
                    handle_rrq(root, data, peer, interface, xfer).await;
                });
                inflight.insert(peer, task);
            }
            OP_WRQ => {
                // Read-only server: reject writes.
                if let Ok(err_sock) = bind_reply_socket(host_ip, interface.as_deref()) {
                    let _ = send_pkt(
                        &err_sock,
                        &error_packet(ERR_ACCESS, "read-only server"),
                        peer,
                        &xfer,
                    )
                    .await;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique, freshly-created temp dir (mirrors the sibling crates' pattern —
    /// no `tempfile` dependency).
    fn tmp() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "netbootd-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build an RRQ payload: opcode + filename\0mode\0[opt\0val\0]...
    fn rrq(filename: &str, mode: &str, opts: &[(&str, &str)]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&OP_RRQ.to_be_bytes());
        p.extend_from_slice(filename.as_bytes());
        p.push(0);
        p.extend_from_slice(mode.as_bytes());
        p.push(0);
        for (k, v) in opts {
            p.extend_from_slice(k.as_bytes());
            p.push(0);
            p.extend_from_slice(v.as_bytes());
            p.push(0);
        }
        p
    }

    #[test]
    fn parse_rrq_basic() {
        let r = parse_rrq(&rrq("kernel_2712.img", "octet", &[])).expect("parses");
        assert_eq!(r.filename, "kernel_2712.img");
        assert_eq!(r.mode, "octet");
        assert_eq!(r.blksize, None);
        assert!(!r.want_tsize);
    }

    #[test]
    fn parse_rrq_lowercases_mode() {
        // RFC 1350: the mode string is case-insensitive.
        let r = parse_rrq(&rrq("f", "OCTET", &[])).unwrap();
        assert_eq!(r.mode, "octet");
    }

    #[test]
    fn parse_rrq_honors_blksize_and_tsize() {
        let r = parse_rrq(&rrq("f", "octet", &[("blksize", "1024"), ("tsize", "0")])).unwrap();
        assert_eq!(r.blksize, Some(1024));
        assert!(r.want_tsize);
    }

    #[test]
    fn parse_rrq_clamps_blksize() {
        // Below the RFC 2348 floor (8) and above our ceiling (65464) are clamped.
        assert_eq!(
            parse_rrq(&rrq("f", "octet", &[("blksize", "1")]))
                .unwrap()
                .blksize,
            Some(8)
        );
        assert_eq!(
            parse_rrq(&rrq("f", "octet", &[("blksize", "70000")]))
                .unwrap()
                .blksize,
            Some(65464)
        );
    }

    #[test]
    fn parse_rrq_ignores_unparseable_blksize() {
        let r = parse_rrq(&rrq("f", "octet", &[("blksize", "huge")])).unwrap();
        assert_eq!(
            r.blksize, None,
            "non-numeric blksize is dropped, not defaulted"
        );
    }

    #[test]
    fn parse_rrq_ignores_unknown_options() {
        let r = parse_rrq(&rrq("f", "octet", &[("windowsize", "4"), ("tsize", "0")])).unwrap();
        assert!(r.want_tsize);
        assert_eq!(r.blksize, None);
    }

    #[test]
    fn parse_rrq_rejects_missing_mode() {
        // Only a filename, no NUL-terminated mode field.
        let mut p = Vec::new();
        p.extend_from_slice(&OP_RRQ.to_be_bytes());
        p.extend_from_slice(b"file");
        assert!(parse_rrq(&p).is_none());
    }

    #[test]
    fn error_packet_layout() {
        let p = error_packet(ERR_NOT_FOUND, "file not found");
        assert_eq!(u16::from_be_bytes([p[0], p[1]]), OP_ERROR);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), ERR_NOT_FOUND);
        assert_eq!(&p[4..p.len() - 1], b"file not found");
        assert_eq!(*p.last().unwrap(), 0, "error message is NUL-terminated");
    }

    /// With the raw-frame sender active a block must fit one Ethernet frame;
    /// through the kernel the client's request stands.
    #[test]
    fn blksize_is_capped_only_for_raw_frames() {
        assert_eq!(cap_blksize(4096, true), MAX_RAW_FRAME_BLKSIZE);
        assert_eq!(cap_blksize(65464, true), MAX_RAW_FRAME_BLKSIZE);
        assert_eq!(
            cap_blksize(MAX_RAW_FRAME_BLKSIZE, true),
            MAX_RAW_FRAME_BLKSIZE
        );
        assert_eq!(cap_blksize(1024, true), 1024, "under the cap is untouched");
        assert_eq!(cap_blksize(DEFAULT_BLKSIZE, true), DEFAULT_BLKSIZE);
        assert_eq!(cap_blksize(4096, false), 4096, "kernel path: no cap");
        assert_eq!(cap_blksize(65464, false), 65464);
        // 1468 is exactly what fits: MTU minus IPv4, UDP and TFTP headers.
        assert_eq!(MAX_RAW_FRAME_BLKSIZE, 1500 - 20 - 8 - 4);
    }

    /// The raw-frame decision needs both halves: a sender *and* a MAC to
    /// address. The inert sender used everywhere in these tests never counts.
    #[test]
    fn raw_frames_requires_sender_and_client_mac() {
        let mut x = loopback_xfer();
        assert!(!x.raw_frames());
        x.client_mac = Some([1, 2, 3, 4, 5, 6]);
        assert!(!x.raw_frames(), "no sender: MAC alone is not enough");
    }

    #[test]
    fn reply_pin_outcome_success_is_pinned() {
        assert_eq!(reply_pin_outcome(None), ReplyPin::Pinned);
    }

    fn assert_falls_back(e: &io::Error) {
        match reply_pin_outcome(Some(e)) {
            ReplyPin::Fallback(reason) => {
                assert!(
                    reason.contains("5.7"),
                    "reason should name the kernel requirement: {reason}"
                );
            }
            other => panic!("expected Fallback for {e}, got {other:?}"),
        }
    }

    #[test]
    fn reply_pin_outcome_permission_denied_falls_back() {
        assert_falls_back(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "setsockopt SO_BINDTODEVICE",
        ));
    }

    /// The two errnos an old kernel returns for the unprivileged pin both
    /// classify as `PermissionDenied`. Unix-only: the raw values mean
    /// something else on Windows, where this arm never runs anyway.
    #[cfg(unix)]
    #[test]
    fn reply_pin_outcome_maps_eperm_and_eacces_to_fallback() {
        for errno in [libc::EPERM, libc::EACCES] {
            let e = io::Error::from_raw_os_error(errno);
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert_falls_back(&e);
        }
    }

    #[test]
    fn reply_pin_outcome_other_errors_are_fatal() {
        // A wrong interface (no such device, bad name) is not a permission
        // story: the transfer must not silently continue unpinned.
        #[cfg(unix)]
        {
            let enodev = io::Error::from_raw_os_error(libc::ENODEV);
            assert_eq!(reply_pin_outcome(Some(&enodev)), ReplyPin::Fatal);
        }
        let other = io::Error::new(io::ErrorKind::InvalidInput, "bad interface name");
        assert_eq!(reply_pin_outcome(Some(&other)), ReplyPin::Fatal);
        let not_found = io::Error::new(io::ErrorKind::NotFound, "no such interface");
        assert_eq!(reply_pin_outcome(Some(&not_found)), ReplyPin::Fatal);
    }

    // Path resolution (the shared `resolve`) is tested in the `served` module.

    // ── Loopback transfer tests ──────────────────────────────────────────────
    //
    // These drive the real async DATA/ACK engine (`handle_rrq` →
    // `send_and_wait_ack` → `send_pkt`) over a 127.0.0.1 UDP pair. `BpfSender`
    // is the inert `unavailable()` form on every platform, so `send_pkt` always
    // takes the ordinary `send_to` path — no hardware, no raw frames, no root.

    /// An `Xfer` that always uses the kernel `send_to` path (BPF unavailable).
    fn loopback_xfer() -> Xfer {
        Xfer {
            host_ip: Ipv4Addr::LOCALHOST,
            bpf: Arc::new(BpfSender::unavailable()),
            client_mac: None,
            ack_timeout: ACK_TIMEOUT,
        }
    }

    fn ack(block: u16) -> [u8; 4] {
        let mut a = [0u8; 4];
        a[..2].copy_from_slice(&OP_ACK.to_be_bytes());
        a[2..].copy_from_slice(&block.to_be_bytes());
        a
    }

    /// Drive the read side of a transfer: ACK every OACK/DATA, accumulate the
    /// payload and the block-number sequence. `blksize` is the negotiated size,
    /// used to spot the final (short) block. Returns (bytes, blocks_seen).
    async fn recv_transfer(sock: &UdpSocket, blksize: usize) -> (Vec<u8>, Vec<u16>) {
        let mut data = Vec::new();
        let mut blocks = Vec::new();
        let mut buf = vec![0u8; 2048];
        loop {
            let (n, from) = sock.recv_from(&mut buf).await.unwrap();
            match u16::from_be_bytes([buf[0], buf[1]]) {
                OP_OACK => {
                    sock.send_to(&ack(0), from).await.unwrap();
                }
                OP_DATA => {
                    let blk = u16::from_be_bytes([buf[2], buf[3]]);
                    blocks.push(blk);
                    data.extend_from_slice(&buf[4..n]);
                    sock.send_to(&ack(blk), from).await.unwrap();
                    if n - 4 < blksize {
                        break;
                    }
                }
                other => panic!("unexpected opcode {other} during transfer"),
            }
        }
        (data, blocks)
    }

    /// Bind a loopback client socket and return it with its address (the `peer`
    /// the server sends to).
    async fn client_socket() -> (UdpSocket, SocketAddr) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        (sock, addr)
    }

    #[tokio::test]
    async fn transfer_delivers_multi_block_file() {
        let root = tmp();
        // 1300 bytes over the default 512-byte blksize -> blocks 1(512),2(512),3(276).
        let contents: Vec<u8> = (0..1300u32).map(|i| i as u8).collect();
        fs::write(root.join("k.img"), &contents).unwrap();

        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("k.img", "octet", &[]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, recv_transfer(&sock, DEFAULT_BLKSIZE));

        assert_eq!(got, contents, "reassembled bytes must match the file");
        assert_eq!(blocks, vec![1, 2, 3]);
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn transfer_exact_multiple_sends_trailing_empty_block() {
        let root = tmp();
        // Exactly 2*512: the last data block is full, so RFC 1350 requires one
        // more (empty) block to signal end-of-file.
        let contents = vec![0xABu8; 1024];
        fs::write(root.join("k.img"), &contents).unwrap();

        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("k.img", "octet", &[]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, recv_transfer(&sock, DEFAULT_BLKSIZE));

        assert_eq!(got, contents);
        assert_eq!(blocks, vec![1, 2, 3], "trailing empty block 3 terminates");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn transfer_of_empty_file_is_one_empty_block() {
        let root = tmp();
        fs::write(root.join("empty"), b"").unwrap();

        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("empty", "octet", &[]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, recv_transfer(&sock, DEFAULT_BLKSIZE));

        assert!(got.is_empty());
        assert_eq!(blocks, vec![1], "a single empty DATA block");
        fs::remove_dir_all(&root).ok();
    }

    /// Larger than any read chunk the engine might use internally, with a
    /// non-repeating pattern, so a seek/offset slip anywhere in the streamed
    /// read shows up as a byte mismatch.
    #[tokio::test]
    async fn transfer_streams_a_large_file_block_by_block() {
        let root = tmp();
        let contents: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        fs::write(root.join("big.img"), &contents).unwrap();

        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("big.img", "octet", &[("blksize", "1428")]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, recv_transfer(&sock, 1428));

        assert_eq!(got, contents);
        assert_eq!(blocks.len(), 300_000_usize.div_ceil(1428));
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn transfer_with_oack_negotiates_blksize_and_tsize() {
        let root = tmp();
        let contents = vec![0x5Au8; 40];
        fs::write(root.join("k.img"), &contents).unwrap();

        let (sock, peer) = client_socket().await;
        // Custom client: inspect the OACK before ACKing, then read the transfer.
        let client = async {
            let mut buf = vec![0u8; 2048];
            let (n, from) = sock.recv_from(&mut buf).await.unwrap();
            assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_OACK);
            // OACK body: blksize\016\0tsize\040\0
            let body = &buf[2..n];
            let s = String::from_utf8_lossy(body);
            let fields: Vec<&str> = s.split('\0').collect();
            assert!(fields.contains(&"blksize") && fields.contains(&"16"));
            assert!(fields.contains(&"tsize") && fields.contains(&"40"));
            sock.send_to(&ack(0), from).await.unwrap();
            // Now read DATA at the negotiated 16-byte blksize: 40 -> 16,16,8.
            let mut data = Vec::new();
            let mut blocks = Vec::new();
            loop {
                let (n, from) = sock.recv_from(&mut buf).await.unwrap();
                assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_DATA);
                let blk = u16::from_be_bytes([buf[2], buf[3]]);
                blocks.push(blk);
                data.extend_from_slice(&buf[4..n]);
                sock.send_to(&ack(blk), from).await.unwrap();
                if n - 4 < 16 {
                    break;
                }
            }
            (data, blocks)
        };
        let server = handle_rrq(
            root.clone(),
            rrq("k.img", "octet", &[("blksize", "16"), ("tsize", "0")]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, client);

        assert_eq!(got, contents);
        assert_eq!(blocks, vec![1, 2, 3], "16-byte blocks: 16+16+8");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn retransmits_data_when_first_ack_is_dropped() {
        let root = tmp();
        let contents = vec![0x11u8; 100];
        fs::write(root.join("k.img"), &contents).unwrap();

        let (sock, peer) = client_socket().await;
        // Client ignores the first copy of block 1, forcing an ACK_TIMEOUT-driven
        // retransmit, then behaves normally. We expect to see block 1 twice.
        let client = async {
            let mut buf = vec![0u8; 2048];
            let mut blocks = Vec::new();
            let mut dropped_once = false;
            let mut data = Vec::new();
            loop {
                let (n, from) = sock.recv_from(&mut buf).await.unwrap();
                let blk = u16::from_be_bytes([buf[2], buf[3]]);
                blocks.push(blk);
                if blk == 1 && !dropped_once {
                    dropped_once = true; // swallow this DATA; do NOT ACK
                    continue;
                }
                data.extend_from_slice(&buf[4..n]);
                sock.send_to(&ack(blk), from).await.unwrap();
                if n - 4 < DEFAULT_BLKSIZE {
                    break;
                }
            }
            (data, blocks)
        };
        let server = handle_rrq(
            root.clone(),
            rrq("k.img", "octet", &[]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, client);

        assert_eq!(got, contents, "transfer still completes after a loss");
        assert_eq!(
            blocks.iter().filter(|&&b| b == 1).count(),
            2,
            "block 1 was retransmitted exactly once"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// A peer that never ACKs but keeps sending *something* (here: ACKs for the
    /// wrong block, faster than the ACK timeout) must not keep the transfer
    /// alive. The server sends block 1 exactly MAX_RETRIES times, each attempt
    /// on its own fixed deadline, and gives up.
    #[tokio::test]
    async fn junk_from_the_peer_does_not_extend_the_ack_deadline() {
        let root = tmp();
        fs::write(root.join("k.img"), vec![0x22u8; 100]).unwrap();

        let (sock, peer) = client_socket().await;
        let ack_timeout = Duration::from_millis(150);
        let mut xfer = loopback_xfer();
        xfer.ack_timeout = ack_timeout;
        let mut server = tokio::spawn(handle_rrq(
            root.clone(),
            rrq("k.img", "octet", &[]),
            peer,
            None,
            xfer,
        ));

        // Well past MAX_RETRIES × ack_timeout (900 ms) but far short of forever.
        let give_up_by = Instant::now() + Duration::from_secs(4);
        let mut junk = tokio::time::interval(Duration::from_millis(40));
        let mut copies_of_block_1 = 0usize;
        let mut server_port: Option<SocketAddr> = None;
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                _ = &mut server => break,
                r = sock.recv_from(&mut buf) => {
                    let (n, from) = r.unwrap();
                    assert!(n >= 4);
                    assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_DATA);
                    assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 1);
                    copies_of_block_1 += 1;
                    server_port = Some(from);
                }
                _ = junk.tick() => {
                    if let Some(to) = server_port {
                        // A plausible-looking but wrong ACK (block 7).
                        sock.send_to(&ack(7), to).await.unwrap();
                    }
                }
                _ = tokio::time::sleep_until(give_up_by) => {
                    panic!("server still waiting after {copies_of_block_1} copies of block 1: \
                            junk is re-arming the ACK timer");
                }
            }
        }
        assert_eq!(
            copies_of_block_1, MAX_RETRIES,
            "one send per attempt, then give up"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn missing_file_returns_error_packet() {
        let root = tmp();
        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("nope.img", "octet", &[]),
            peer,
            None,
            loopback_xfer(),
        );
        let client = async {
            let mut buf = vec![0u8; 2048];
            let (n, _) = sock.recv_from(&mut buf).await.unwrap();
            (
                u16::from_be_bytes([buf[0], buf[1]]),
                u16::from_be_bytes([buf[2], buf[3]]),
                n,
            )
        };
        let (_, (opcode, code, _)) = tokio::join!(server, client);
        assert_eq!(opcode, OP_ERROR);
        assert_eq!(code, ERR_NOT_FOUND);
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn non_octet_mode_returns_error_packet() {
        let root = tmp();
        fs::write(root.join("k.img"), b"hello").unwrap();
        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("k.img", "netascii", &[]),
            peer,
            None,
            loopback_xfer(),
        );
        let client = async {
            let mut buf = vec![0u8; 2048];
            let _ = sock.recv_from(&mut buf).await.unwrap();
            (
                u16::from_be_bytes([buf[0], buf[1]]),
                u16::from_be_bytes([buf[2], buf[3]]),
            )
        };
        let (_, (opcode, code)) = tokio::join!(server, client);
        assert_eq!(opcode, OP_ERROR);
        assert_eq!(code, ERR_ILLEGAL);
        fs::remove_dir_all(&root).ok();
    }

    // ── Dispatcher tests ─────────────────────────────────────────────────────
    //
    // These go through `run` — the listen loop that spawns one sender per RRQ —
    // over a loopback UDP pair, exactly as `serve` does minus the port-69 bind
    // and the interface pin.

    /// Two RRQs from the same TID, back to back: the first (a big file the
    /// client never ACKs) must be *replaced* by the second (a small one),
    /// leaving a single sender. With parallel senders the abandoned first one
    /// would keep retransmitting its block 1 after the small file completed.
    #[tokio::test]
    async fn duplicate_rrq_from_the_same_tid_replaces_the_transfer() {
        let root = tmp();
        fs::write(root.join("big.img"), vec![0xB1u8; 200_000]).unwrap();
        let small = b"tiny".to_vec();
        fs::write(root.join("small.img"), &small).unwrap();

        let listen = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listen.local_addr().unwrap();
        let (_tx, rx) = watch::channel::<Option<[u8; 6]>>(None);
        let ack_timeout = Duration::from_millis(200);
        let dispatcher = tokio::spawn(run(
            listen,
            root.clone(),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
            Arc::new(BpfSender::unavailable()),
            rx,
            ack_timeout,
        ));

        let (sock, _) = client_socket().await;
        sock.send_to(&rrq("big.img", "octet", &[]), server_addr)
            .await
            .unwrap();
        sock.send_to(&rrq("small.img", "octet", &[]), server_addr)
            .await
            .unwrap();

        // ACK only the small file's data; leave anything from big unanswered.
        let mut buf = vec![0u8; 2048];
        let done_by = Instant::now() + Duration::from_secs(5);
        loop {
            let (n, from) = timeout_at(done_by, sock.recv_from(&mut buf))
                .await
                .expect("the small file should arrive")
                .unwrap();
            assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_DATA);
            if buf[4..n] == small[..] {
                sock.send_to(&ack(1), from).await.unwrap();
                break;
            }
        }

        // Quiet period longer than two ACK timeouts: a surviving big sender
        // would retransmit its block 1 in here.
        let quiet_until = Instant::now() + ack_timeout * 3;
        match timeout_at(quiet_until, sock.recv_from(&mut buf)).await {
            Err(_) => {} // nothing arrived: one sender, as required
            Ok(r) => {
                let (n, from) = r.unwrap();
                panic!(
                    "unexpected packet from {from} after the replacement transfer \
                     completed ({n} bytes): the first RRQ's sender is still alive"
                );
            }
        }
        dispatcher.abort();
        fs::remove_dir_all(&root).ok();
    }

    // ── Peer gating ──────────────────────────────────────────────────────────
    //
    // TFTP has no concept of source-address identity of its own — these tests
    // drive the real dispatcher (`run`) with different *configured*
    // `client_ip` values against the same real 127.0.0.1 test client, which
    // exercises the actual `peer.ip() != client_ip` check without needing a
    // second loopback address (binding one, e.g. 127.0.0.2, needs an OS-level
    // alias that is not present by default on macOS).

    #[tokio::test]
    async fn rrq_from_a_non_leased_peer_is_ignored() {
        let root = tmp();
        fs::write(root.join("k.img"), b"hello").unwrap();

        let listen = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listen.local_addr().unwrap();
        let (_tx, rx) = watch::channel::<Option<[u8; 6]>>(None);
        // A client IP the real test client (127.0.0.1) can never match.
        let unleased_client_ip = Ipv4Addr::new(203, 0, 113, 9);
        let dispatcher = tokio::spawn(run(
            listen,
            root.clone(),
            Ipv4Addr::LOCALHOST,
            unleased_client_ip,
            None,
            Arc::new(BpfSender::unavailable()),
            rx,
            ACK_TIMEOUT,
        ));

        let (sock, _) = client_socket().await;
        sock.send_to(&rrq("k.img", "octet", &[]), server_addr)
            .await
            .unwrap();

        let mut buf = vec![0u8; 2048];
        let got = timeout_at(
            Instant::now() + Duration::from_millis(300),
            sock.recv_from(&mut buf),
        )
        .await;
        assert!(
            got.is_err(),
            "an RRQ from a peer other than the leased client must get no reply (OACK/DATA/ERROR) \
             at all"
        );

        dispatcher.abort();
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn rrq_from_the_leased_peer_is_served() {
        let root = tmp();
        let contents = b"hello netboot".to_vec();
        fs::write(root.join("k.img"), &contents).unwrap();

        let listen = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listen.local_addr().unwrap();
        let (_tx, rx) = watch::channel::<Option<[u8; 6]>>(None);
        let dispatcher = tokio::spawn(run(
            listen,
            root.clone(),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
            Arc::new(BpfSender::unavailable()),
            rx,
            ACK_TIMEOUT,
        ));

        let (sock, _) = client_socket().await;
        sock.send_to(&rrq("k.img", "octet", &[]), server_addr)
            .await
            .unwrap();
        let (got, _blocks) = recv_transfer(&sock, DEFAULT_BLKSIZE).await;
        assert_eq!(
            got, contents,
            "the leased peer must still be served normally"
        );

        dispatcher.abort();
        fs::remove_dir_all(&root).ok();
    }

    // ── Transfer-slot bound ──────────────────────────────────────────────────

    /// `MAX_TRANSFERS` clients that never ACK anything fill every slot; one
    /// more, from a fresh source port, gets nothing while the daemon is full.
    /// Once the stalled transfers exhaust `MAX_RETRIES` and give up, their
    /// slots free and the same request succeeds.
    #[tokio::test]
    async fn transfers_are_bounded_and_a_slot_frees_after_the_retry_deadline() {
        let root = tmp();
        fs::write(root.join("stall.img"), vec![0x42u8; 4096]).unwrap();
        let small = b"tiny".to_vec();
        fs::write(root.join("small.img"), &small).unwrap();

        let listen = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listen.local_addr().unwrap();
        let (_tx, rx) = watch::channel::<Option<[u8; 6]>>(None);
        let ack_timeout = Duration::from_millis(80);
        let dispatcher = tokio::spawn(run(
            listen,
            root.clone(),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
            Arc::new(BpfSender::unavailable()),
            rx,
            ack_timeout,
        ));

        // Fill every slot with a client that never ACKs.
        let mut stalled = Vec::new();
        for _ in 0..MAX_TRANSFERS {
            let (sock, _) = client_socket().await;
            sock.send_to(&rrq("stall.img", "octet", &[]), server_addr)
                .await
                .unwrap();
            stalled.push(sock);
        }
        // Confirm each was actually spawned (holds a permit) before probing
        // the bound: every one must see its first DATA block.
        let started_by = Instant::now() + Duration::from_secs(2);
        let mut buf = vec![0u8; 2048];
        for sock in &stalled {
            let (_n, _) = timeout_at(started_by, sock.recv_from(&mut buf))
                .await
                .expect("a slotted transfer must send its first DATA block")
                .unwrap();
            assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_DATA);
        }

        // One more, from a fresh source port: every slot is taken, so this
        // must get nothing rather than being queued.
        let (extra, _) = client_socket().await;
        extra
            .send_to(&rrq("small.img", "octet", &[]), server_addr)
            .await
            .unwrap();
        let mut extra_buf = vec![0u8; 2048];
        let denied = timeout_at(
            Instant::now() + ack_timeout * 2,
            extra.recv_from(&mut extra_buf),
        )
        .await;
        assert!(
            denied.is_err(),
            "an RRQ past MAX_TRANSFERS must be dropped, not queued, while every slot is in use"
        );

        // Past MAX_RETRIES * ack_timeout the stalled transfers give up; each
        // held permit drops with its task, freeing the slot.
        tokio::time::sleep(ack_timeout * (MAX_RETRIES as u32) + Duration::from_millis(200)).await;

        extra
            .send_to(&rrq("small.img", "octet", &[]), server_addr)
            .await
            .unwrap();
        let (got, _blocks) = recv_transfer(&extra, DEFAULT_BLKSIZE).await;
        assert_eq!(got, small, "a freed slot serves the next request");

        dispatcher.abort();
        fs::remove_dir_all(&root).ok();
    }

    // ~14 s: 65536 lock-step round trips are the only way to reach the wrap.
    // Excluded from the default run; exercise with `cargo test -- --ignored`.
    #[ignore = "stress test: 65k round trips (~14s); run with --ignored"]
    #[tokio::test]
    async fn block_number_wraps_past_0xffff() {
        // The block counter is a u16 with `wrapping_add`. To exercise the wrap we
        // must transfer more than 65535 blocks; an 8-byte blksize keeps the file
        // small (~512 KiB) while still forcing 65536+ round trips. We assert the
        // transfer completes AND that block 0 (only reachable by wrapping) appears.
        let root = tmp();
        let blksize = 8usize;
        let n_full_blocks = 65536usize; // blocks 1..=65535, then 0
        let contents = vec![0x7Eu8; n_full_blocks * blksize + 4]; // + a final short block
        fs::write(root.join("big.img"), &contents).unwrap();

        let (sock, peer) = client_socket().await;
        let server = handle_rrq(
            root.clone(),
            rrq("big.img", "octet", &[("blksize", "8")]),
            peer,
            None,
            loopback_xfer(),
        );
        let (_, (got, blocks)) = tokio::join!(server, recv_transfer(&sock, blksize));

        assert_eq!(
            got.len(),
            contents.len(),
            "all bytes delivered across the wrap"
        );
        assert_eq!(got, contents);
        assert!(
            blocks.contains(&0),
            "block 0 must appear, proving the 0xFFFF->0 wrap"
        );
        // Sanity: the sequence starts 1,2,3 and contains the wrap boundary 65535,0.
        assert_eq!(&blocks[..3], &[1, 2, 3]);
        let wrap = blocks.windows(2).position(|w| w == [0xFFFF, 0]);
        assert!(wrap.is_some(), "0xFFFF must be immediately followed by 0");
        fs::remove_dir_all(&root).ok();
    }
}
