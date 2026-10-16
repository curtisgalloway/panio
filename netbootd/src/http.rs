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

//! Minimal read-only HTTP/1.1 server for UEFI HTTP Boot.
//!
//! Serves the same rooted directory as the TFTP server, but over TCP. The EDK2
//! `HttpBootDxe` client (and any follow-on fetches its NBP makes — GRUB reading
//! `grub.cfg`, an iPXE script chainloading, …) needs only:
//!   * **GET** and **HEAD** (it HEADs to size its buffer, then GETs);
//!   * an explicit **`Content-Length`** — we always know the file size, so we
//!     always send it and never use chunked transfer;
//!   * a sane **`Content-Type`** (default `application/octet-stream`, accepted as
//!     an EFI application).
//!
//! Unlike the silent Pi bootloader the TFTP path serves, a UEFI client owns a
//! full IP/TCP/ARP stack and answers ARP, so the host kernel delivers normally:
//! **no `/dev/bpf` raw-frame path, no setuid helper, no static ARP pin.** Path
//! resolution reuses [`crate::served::resolve`], so traversal safety is shared
//! with TFTP.
//!
//! The listener is pinned to the netboot interface (see `pin`) — an unpinned
//! wildcard listener would serve the root directory to the host's primary
//! NIC too — and stays IPv4, which is what macOS `IP_BOUND_IF` applies to.
//! Against a peer that is not a well-behaved boot client: a request head (and
//! any body it declares) must arrive within [`HEAD_TIMEOUT`]; at most
//! [`MAX_CONNECTIONS`] connections are served at once; HTTP/1.0 defaults to
//! `Connection: close`; and a body is exactly the `Content-Length` announced
//! for it, whatever the file does underneath us, so keep-alive framing can
//! never desync.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::served::{loggable, resolve};

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
/// Streaming copy buffer for GET bodies (boot payloads can be tens of MB, so we
/// never load the whole file into memory).
const COPY_CHUNK: usize = 64 * 1024;
/// Upper bound on the request head (request line + headers). A boot client's
/// head is a few hundred bytes; this just stops a peer from streaming forever
/// without a terminator.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// How long a connection may take to deliver one request head (and drain the
/// body it declared). A boot client sends its head in one segment; a peer that
/// trickles bytes, or connects and says nothing, is cut off here.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Concurrent connections served at once. One boot client opens a handful;
/// beyond this, new connections wait in the kernel backlog.
pub const MAX_CONNECTIONS: usize = 64;
/// Listen backlog: how many not-yet-accepted connections the kernel queues.
const BACKLOG: i32 = 128;

/// Bind the HTTP listen socket on `0.0.0.0:port`, pinned to the netboot
/// interface. The pin is what keeps the file server off every other interface
/// on the host, so a pin that cannot be applied is fatal. IPv4 only: on macOS
/// `IP_BOUND_IF` is an IPv4 socket option, and the boot URL we advertise is
/// an IPv4 literal anyway.
///
/// `SO_REUSEADDR` stays on for this TCP listener: after a restart the previous
/// instance's connections may still sit in TIME_WAIT on this port, and without
/// it `bind` fails with EADDRINUSE for up to 2×MSL. It does not let a second
/// live listener share the port on Linux. macOS additionally uses
/// `SO_REUSEPORT` under a daemon-lifetime interface lock.
pub fn bind_server(port: u16, link: &crate::pin::ListenerInterface) -> Result<TcpListener> {
    let interface = link.name();
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    link.pin_listener(&sock)
        .with_context(|| format!("pin HTTP socket to interface {interface}"))?;
    let addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
    sock.bind(&addr.into()).with_context(|| {
        format!("bind HTTP port {port} (need root/CAP_NET_BIND_SERVICE on Linux)")
    })?;
    sock.listen(BACKLOG)?;
    sock.set_nonblocking(true)?;
    Ok(TcpListener::from_std(sock.into())?)
}

/// What every connection handler needs.
struct ConnConfig {
    root: PathBuf,
    ctype: String,
    /// [`HEAD_TIMEOUT`] in service; the tests shrink it.
    head_timeout: Duration,
}

/// Run the HTTP server on an already-bound listener until the task is
/// cancelled.
pub async fn serve(
    listener: TcpListener,
    root: PathBuf,
    content_type: Option<String>,
) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("HTTP root {} does not exist", root.display()))?;
    let content_type = content_type.unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string());
    info!(
        root = %root.display(),
        content_type,
        "HTTP listening on {}",
        listener.local_addr()?
    );
    let cfg = Arc::new(ConnConfig {
        root,
        ctype: content_type,
        head_timeout: HEAD_TIMEOUT,
    });
    let limiter = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        // Take the slot before accepting: at the cap, new connections queue in
        // the backlog until a handler finishes (and every handler is bounded
        // by the head timeout, so a slot always comes back).
        let permit = limiter
            .clone()
            .acquire_owned()
            .await
            .expect("connection limiter is never closed");
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("HTTP accept: {e}");
                continue;
            }
        };
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let _slot = permit;
            if let Err(e) = handle_conn(stream, &cfg).await {
                warn!("HTTP {peer}: {e:#}");
            }
        });
    }
}

/// A parsed request head: method, request-target, and the two headers we act on.
struct Head {
    method: String,
    target: String,
    keep_alive: bool,
    content_length: usize,
}

/// Read and parse one request head (up to the blank-line terminator). Returns
/// `None` on a clean EOF before any bytes (the client closed an idle keep-alive
/// connection).
async fn read_head<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Head>> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte).await?;
        if n == 0 {
            // Clean EOF (idle keep-alive close) or a head cut short at EOF —
            // either way there is no complete request to serve.
            return Ok(None);
        }
        raw.push(byte[0]);
        if raw.len() > MAX_HEAD_BYTES {
            anyhow::bail!("request head exceeds {MAX_HEAD_BYTES} bytes without terminator");
        }
        if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
            break;
        }
    }

    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let req_line = lines.next().unwrap_or("");
    let mut it = req_line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("").to_string();
    let version = it.next().unwrap_or("");

    // Persistence is per protocol version: HTTP/1.1 keeps the connection open
    // unless told otherwise, HTTP/1.0 (and anything unrecognised) closes it
    // unless the client asks for keep-alive. `Connection:` then overrides.
    let mut keep_alive = version.eq_ignore_ascii_case("HTTP/1.1");
    let mut content_length = 0usize;
    for h in lines {
        let Some((name, value)) = h.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "connection" => {
                let mut tokens = value.split(',').map(str::trim);
                if tokens.clone().any(|t| t.eq_ignore_ascii_case("close")) {
                    keep_alive = false;
                } else if tokens.any(|t| t.eq_ignore_ascii_case("keep-alive")) {
                    keep_alive = true;
                }
            }
            "content-length" => content_length = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    Ok(Some(Head {
        method,
        target,
        keep_alive,
        content_length,
    }))
}

/// Discard `remaining` bytes of a declared request body. `Ok(false)` if the
/// peer closed before sending them all.
async fn drain_body<R: AsyncRead + Unpin>(r: &mut R, mut remaining: usize) -> Result<bool> {
    let mut sink = [0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(sink.len());
        let got = r.read(&mut sink[..want]).await?;
        if got == 0 {
            return Ok(false);
        }
        remaining -= got;
    }
    Ok(true)
}

async fn handle_conn(stream: TcpStream, cfg: &ConnConfig) -> Result<()> {
    let mut reader = BufReader::new(stream);
    loop {
        let head = match timeout(cfg.head_timeout, read_head(&mut reader)).await {
            Ok(Ok(Some(head))) => head,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!(
                "no complete request head within {:?}; closing",
                cfg.head_timeout
            ),
        };
        // The method and target come off the wire; only their sanitized forms
        // reach the log.
        let method = loggable(&head.method);
        let target = loggable(&head.target);

        // GET/HEAD carry no body, but drain a declared one so the next request
        // on a kept-alive connection stays framed — on the same clock as the
        // head, so a declared-but-never-sent body cannot park the connection.
        match timeout(
            cfg.head_timeout,
            drain_body(&mut reader, head.content_length),
        )
        .await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return Ok(()),
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!(
                "declared {}-byte body not delivered within {:?}; closing",
                head.content_length,
                cfg.head_timeout
            ),
        }

        let head_only = head.method.eq_ignore_ascii_case("HEAD");
        let is_get = head.method.eq_ignore_ascii_case("GET");
        let stream = reader.get_mut();

        if !is_get && !head_only {
            info!("{method} {target} -> 405");
            write_status(
                stream,
                405,
                "Method Not Allowed",
                head.keep_alive,
                head_only,
            )
            .await?;
        } else if let Some(path) = resolve_target(&cfg.root, &head.target) {
            serve_file(
                stream,
                &path,
                &cfg.ctype,
                head.keep_alive,
                head_only,
                &target,
            )
            .await?;
        } else {
            info!("{method} {target} -> 404");
            write_status(stream, 404, "Not Found", head.keep_alive, head_only).await?;
        }

        if !head.keep_alive {
            return Ok(());
        }
    }
}

/// Map an HTTP request-target to a file inside `root`, or `None` (→ 404).
///
/// Accepts origin-form (`/grubaa64.efi`) and absolute-form
/// (`http://host/grubaa64.efi`); strips any query/fragment; percent-decodes; and
/// rejects anything that is not a regular file inside `root` (directories,
/// traversal, missing files).
fn resolve_target(root: &Path, target: &str) -> Option<PathBuf> {
    // absolute-form: drop scheme + authority, keep from the first '/'.
    let path = if let Some(rest) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
    {
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        }
    } else {
        target
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let decoded = percent_decode(path);
    resolve(root, &decoded).filter(|p| p.is_file())
}

/// Decode `%XX` escapes; pass everything else through unchanged.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Send `path` as a 200 response. `target` is the already-sanitized
/// request-target, for the log line only.
///
/// The body is exactly the `Content-Length` announced in the header — the
/// file's size at the moment it was opened. If the file grows meanwhile the
/// extra bytes are not sent (they would be read as the start of the next
/// response on a kept-alive connection); if it shrinks, the announced length
/// cannot be honoured and the connection is dropped, so the client sees a
/// truncated body rather than a mis-framed one.
async fn serve_file<W: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut W,
    path: &Path,
    ctype: &str,
    keep_alive: bool,
    head_only: bool,
    target: &str,
) -> Result<()> {
    let file = match File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            warn!("open {}: {e}", path.display());
            return write_status(stream, 404, "Not Found", keep_alive, head_only).await;
        }
    };
    let len = file.metadata().await?.len();
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nContent-Type: {ctype}\r\n\
         Connection: {conn}\r\n\r\n"
    );
    stream.write_all(header.as_bytes()).await?;
    info!(
        "{target} -> 200 ({len} bytes{})",
        if head_only { ", HEAD" } else { "" }
    );

    if !head_only {
        let mut file = file;
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(COPY_CHUNK as u64) as usize;
            let n = file.read(&mut buf[..want]).await?;
            if n == 0 {
                anyhow::bail!(
                    "{target}: file shrank mid-transfer ({remaining} of {len} bytes unsent); \
                     dropping the connection rather than mis-frame it"
                );
            }
            stream.write_all(&buf[..n]).await?;
            remaining -= n as u64;
        }
    }
    stream.flush().await?;
    Ok(())
}

async fn write_status<W: AsyncWrite + Unpin>(
    stream: &mut W,
    code: u16,
    reason: &str,
    keep_alive: bool,
    head_only: bool,
) -> Result<()> {
    let body = format!("{code} {reason}\n");
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\
         Connection: {conn}\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only {
        stream.write_all(body.as_bytes()).await?;
    }
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "netbootd-http-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn cfg(root: PathBuf, ctype: &str, head_timeout: Duration) -> ConnConfig {
        ConnConfig {
            root,
            ctype: ctype.to_string(),
            head_timeout,
        }
    }

    fn split_response(resp: &[u8]) -> (String, Vec<u8>) {
        let sep = b"\r\n\r\n";
        let pos = resp
            .windows(sep.len())
            .position(|w| w == sep)
            .expect("response has a header terminator");
        let head = String::from_utf8_lossy(&resp[..pos]).into_owned();
        let body = resp[pos + sep.len()..].to_vec();
        (head, body)
    }

    fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    }

    /// Drive one request through `handle_conn` over a real loopback TCP pair and
    /// return the raw response bytes. The request must make the server close
    /// the connection (`Connection: close`, or HTTP/1.0) so the client's
    /// read-to-EOF completes — and it must do so within a few seconds, or the
    /// server is holding the connection open when it should not.
    async fn roundtrip(root: PathBuf, ctype: &str, request: &[u8]) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = cfg(root, ctype, HEAD_TIMEOUT);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_conn(stream, &cfg).await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut resp = Vec::new();
        timeout(Duration::from_secs(5), client.read_to_end(&mut resp))
            .await
            .expect("server closed the connection after the response")
            .unwrap();
        server.await.unwrap();
        resp
    }

    #[tokio::test]
    async fn get_serves_file_with_content_length() {
        let root = tmp();
        let body = vec![0xABu8; 4096];
        fs::write(root.join("boot.efi"), &body).unwrap();

        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"GET /boot.efi HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (head, got) = split_response(&resp);

        assert!(head.starts_with("HTTP/1.1 200 OK"), "head: {head}");
        assert_eq!(header_value(&head, "Content-Length"), Some("4096"));
        assert_eq!(
            header_value(&head, "Content-Type"),
            Some("application/octet-stream")
        );
        assert_eq!(got, body, "body bytes match the file");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn head_returns_length_without_body() {
        let root = tmp();
        fs::write(root.join("boot.efi"), vec![0u8; 1234]).unwrap();

        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"HEAD /boot.efi HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (head, body) = split_response(&resp);

        assert!(head.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(header_value(&head, "Content-Length"), Some("1234"));
        assert!(body.is_empty(), "HEAD must not send a body");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn missing_file_is_404() {
        let root = tmp();
        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"GET /nope.efi HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (head, _) = split_response(&resp);
        assert!(head.starts_with("HTTP/1.1 404"), "head: {head}");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn unsupported_method_is_405() {
        let root = tmp();
        fs::write(root.join("boot.efi"), b"x").unwrap();
        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"POST /boot.efi HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (head, _) = split_response(&resp);
        assert!(head.starts_with("HTTP/1.1 405"), "head: {head}");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let base = tmp();
        let served = base.join("served");
        fs::create_dir_all(&served).unwrap();
        fs::write(base.join("secret"), b"top secret").unwrap();

        let resp = roundtrip(
            served.clone(),
            "application/octet-stream",
            b"GET /../secret HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (head, _) = split_response(&resp);
        assert!(
            head.starts_with("HTTP/1.1 404"),
            "traversal must 404: {head}"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn serves_file_in_subdirectory() {
        let root = tmp();
        let sub = root.join("grub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("grub.cfg"), b"set timeout=0\n").unwrap();

        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"GET /grub/grub.cfg?v=1 HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (head, body) = split_response(&resp);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "head: {head}");
        assert_eq!(
            body, b"set timeout=0\n",
            "query string stripped, file served"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// HTTP/1.0 has no persistent connections unless the client asks: with no
    /// `Connection:` header at all the server must close after the response.
    #[tokio::test]
    async fn http10_without_connection_header_closes() {
        let root = tmp();
        fs::write(root.join("boot.efi"), b"v1.0").unwrap();

        // `roundtrip` itself fails (5 s read-to-EOF timeout) if the server
        // keeps the connection open.
        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"GET /boot.efi HTTP/1.0\r\n\r\n",
        )
        .await;
        let (head, body) = split_response(&resp);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "head: {head}");
        assert_eq!(header_value(&head, "Connection"), Some("close"));
        assert_eq!(body, b"v1.0");
        fs::remove_dir_all(&root).ok();
    }

    /// HTTP/1.1 still defaults to keep-alive: two pipelined requests are both
    /// answered on the one connection, and it closes only when asked.
    #[tokio::test]
    async fn http11_keeps_the_connection_until_told_to_close() {
        let root = tmp();
        fs::write(root.join("a"), b"first").unwrap();
        fs::write(root.join("b"), b"second").unwrap();

        let resp = roundtrip(
            root.clone(),
            "application/octet-stream",
            b"GET /a HTTP/1.1\r\n\r\nGET /b HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        let text = String::from_utf8_lossy(&resp);
        assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 2, "{text}");
        assert!(text.contains("first") && text.contains("second"), "{text}");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn read_head_persistence_follows_version_then_connection_header() {
        async fn keep_alive(req: &[u8]) -> bool {
            read_head(&mut &req[..]).await.unwrap().unwrap().keep_alive
        }
        assert!(keep_alive(b"GET / HTTP/1.1\r\n\r\n").await);
        assert!(!keep_alive(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").await);
        assert!(!keep_alive(b"GET / HTTP/1.0\r\n\r\n").await);
        assert!(keep_alive(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").await);
        assert!(keep_alive(b"GET / HTTP/1.0\r\nConnection: Keep-Alive, TE\r\n\r\n").await);
        assert!(!keep_alive(b"GET / HTTP/1.1\r\nConnection: TE, close\r\n\r\n").await);
        assert!(
            !keep_alive(b"GET / HTTP/0.9\r\n\r\n").await,
            "unknown version closes"
        );
    }

    /// A client that connects and sends only part of a head (or nothing) is
    /// cut off at the head timeout instead of holding a slot forever.
    #[tokio::test]
    async fn slow_head_is_timed_out() {
        let root = tmp();
        fs::write(root.join("boot.efi"), b"x").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = cfg(
            root.clone(),
            "application/octet-stream",
            Duration::from_millis(200),
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_conn(stream, &cfg).await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        // A request line but never the blank-line terminator.
        client
            .write_all(b"GET /boot.efi HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        let started = std::time::Instant::now();
        timeout(Duration::from_secs(3), client.read_to_end(&mut resp))
            .await
            .expect("server must close the connection at the head timeout")
            .unwrap();
        assert!(resp.is_empty(), "no response to an incomplete request");
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "closed only after the timeout, not on the first read"
        );
        let err = server
            .await
            .unwrap()
            .expect_err("handler reports the timeout");
        assert!(
            err.to_string().contains("no complete request head"),
            "{err}"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// Serve a file through `serve_file` over an in-memory pipe so small that
    /// the server blocks on every kilobyte — which lets the test change the
    /// file underneath it, deterministically, before the body is through.
    /// Returns (server result, response bytes).
    async fn serve_while<F>(path: PathBuf, meddle: F) -> (Result<()>, Vec<u8>)
    where
        F: FnOnce(&Path),
    {
        let (mut srv, mut cli) = tokio::io::duplex(1024);
        let p = path.clone();
        let server = tokio::spawn(async move {
            serve_file(&mut srv, &p, "application/octet-stream", false, false, "/f").await
            // `srv` drops here, which is the client's EOF.
        });

        // Read until the header terminator, so the server is committed to a
        // Content-Length but has streamed at most one chunk of the body.
        let mut resp = Vec::new();
        let mut buf = [0u8; 256];
        while !resp.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = cli.read(&mut buf).await.unwrap();
            assert!(n > 0, "server closed before finishing the header");
            resp.extend_from_slice(&buf[..n]);
        }
        meddle(&path);
        cli.read_to_end(&mut resp).await.unwrap();
        (server.await.unwrap(), resp)
    }

    /// A file that grows while it is being served yields exactly the
    /// `Content-Length` announced — never the extra bytes, which on a
    /// kept-alive connection would be parsed as the next response.
    #[tokio::test]
    async fn body_is_bounded_by_content_length_when_the_file_grows() {
        let root = tmp();
        let path = root.join("f");
        let original: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &original).unwrap();

        let (result, resp) = serve_while(path.clone(), |p| {
            let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
            std::io::Write::write_all(&mut f, &vec![0xEEu8; 64 * 1024]).unwrap();
        })
        .await;
        result.unwrap();
        let (head, body) = split_response(&resp);
        assert_eq!(
            header_value(&head, "Content-Length"),
            Some("262144"),
            "length announced up front"
        );
        assert_eq!(
            body.len(),
            original.len(),
            "not one byte past Content-Length"
        );
        assert_eq!(body, original);
        fs::remove_dir_all(&root).ok();
    }

    /// A file that shrinks cannot honour the announced length: the connection
    /// is dropped (an error from the handler) rather than padded or left
    /// short with keep-alive framing intact.
    #[tokio::test]
    async fn shrinking_file_drops_the_connection() {
        let root = tmp();
        let path = root.join("f");
        fs::write(&path, vec![0x5Au8; 256 * 1024]).unwrap();

        let (result, resp) = serve_while(path.clone(), |p| {
            fs::write(p, b"").unwrap();
        })
        .await;
        let err = result.expect_err("cannot honour Content-Length");
        assert!(err.to_string().contains("shrank"), "{err}");
        let (_, body) = split_response(&resp);
        assert!(body.len() < 256 * 1024, "body cut short, not padded");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn percent_decode_handles_escapes_and_literals() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
        // A trailing, incomplete escape is passed through literally.
        assert_eq!(percent_decode("end%2"), "end%2");
    }
}
