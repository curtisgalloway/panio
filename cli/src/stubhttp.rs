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

//! A one-shot loopback HTTP server for tests that need to see exactly what
//! the CLI puts on the wire.
//!
//! The naive version (accept, one `read`, write the reply, drop the socket)
//! is racy: a request whose body arrives in a second segment is still being
//! sent when the socket closes, and the client then sees a connection reset
//! — which the Windows CI runner reported as "Network Err" about one run in
//! ten. This helper reads the whole request (headers plus `Content-Length`
//! body) before answering, and shuts the socket down instead of dropping it.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::thread::JoinHandle;
use std::time::Duration;

/// Accept one connection on `listener`, read one complete HTTP/1.1 request,
/// send `response` verbatim, close cleanly, and hand back the request text.
pub(crate) fn serve_one(listener: TcpListener, response: &'static [u8]) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut req: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if request_complete(&req) {
                break;
            }
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => req.extend_from_slice(&chunk[..n]),
                Err(e) => panic!("stub read: {e}"),
            }
        }
        let _ = s.write_all(response);
        let _ = s.flush();
        let _ = s.shutdown(Shutdown::Write);
        // Drain anything the client still sends so it never sees a reset.
        s.set_read_timeout(Some(Duration::from_millis(500))).ok();
        while matches!(s.read(&mut chunk), Ok(n) if n > 0) {}
        String::from_utf8_lossy(&req).into_owned()
    })
}

/// True once `buf` holds the header block and the whole `Content-Length` body.
fn request_complete(buf: &[u8]) -> bool {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..end]);
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    buf.len() >= end + 4 + len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completeness_waits_for_the_declared_body() {
        let head = b"POST /x HTTP/1.1\r\nContent-Length: 2\r\n\r\n";
        assert!(!request_complete(head));
        assert!(!request_complete(
            b"POST /x HTTP/1.1\r\nContent-Length: 2\r\n\r\nh"
        ));
        let mut full = head.to_vec();
        full.extend_from_slice(b"hi");
        assert!(request_complete(&full));
        assert!(request_complete(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(!request_complete(b"GET / HTTP/1.1\r\nHost: x"));
    }
}
