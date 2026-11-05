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

//! Power control: the FTDI DTR line (wired to the target's J2 power-button
//! header) via the serialcap daemon, with a direct-serial fallback; plus
//! power-state sensing from the daemon's `/status`.
//!
//! DTR pulse guidance (Raspberry Pi 5 / DA9091 PMIC): ≤500 ms is a power-button
//! event the OS handles (graceful reboot/halt); ≥3000 ms is a hard PMIC
//! power-off (pulse again to power back on).

use std::time::Duration;

use anyhow::{Context, Result};

use crate::daemons::{self, Endpoint};

/// The request timeout (ms) for a `ms`-long DTR pulse: enough time for the
/// pulse itself plus 5s of daemon/request overhead, floored at 15s so even a
/// short pulse gets a timeout the daemon's own round trip can fit inside.
/// `saturating_add` rather than `+`: `ms` is a caller-supplied duration, and a
/// huge value must clamp to `u64::MAX` (an unreachable timeout — the same
/// practical effect as the intended sum) rather than wrap into a tiny one, or
/// panic outright in a debug build.
fn dtr_timeout_ms(ms: u64) -> u64 {
    std::cmp::max(15_000, ms.saturating_add(5_000))
}

/// Assert DTR for `ms` via the running serialcap daemon (it owns the port).
pub fn dtr_press_daemon(daemon: &Endpoint, interface: &str, ms: u64) -> Result<()> {
    let timeout = dtr_timeout_ms(ms);
    // `interface` is a name from the lab file — percent-encode it so one
    // containing `&` or `=` can't reshape the query string it's spliced into
    // (Review low #3).
    daemon
        .post(&format!(
            "/button?interface={}&ms={ms}",
            daemons::query_escape(interface)
        ))
        .timeout(Duration::from_millis(timeout))
        .send_bytes(&[])
        .map(|_| ())
        .map_err(|e| {
            crate::error::daemon_request_failed(crate::serial::DAEMON, "serialcap /button", e)
                .into()
        })
}

/// Assert DTR for `ms` directly (fallback when the daemon isn't running).
pub fn dtr_press_direct(device: &str, ms: u64) -> Result<()> {
    let mut port = serialport::new(device, 115200)
        .timeout(Duration::from_millis(250))
        .open()
        .with_context(|| format!("opening {device} for DTR control"))?;
    port.write_data_terminal_ready(false)?;
    std::thread::sleep(Duration::from_millis(50)); // settle after open
    port.write_data_terminal_ready(true)?;
    std::thread::sleep(Duration::from_millis(ms));
    port.write_data_terminal_ready(false)?;
    Ok(())
}

/// Current power state from the daemon's sense line: Some(on) or None when the
/// sense signal isn't configured (power_on is null) or the daemon is unreachable.
pub fn read_power_state(daemon: &Endpoint, interface: &str) -> Option<bool> {
    let resp = daemon
        .get(&format!(
            "/status?interface={}",
            daemons::query_escape(interface)
        ))
        .timeout(Duration::from_secs(2))
        .call()
        .ok()?;
    let v: serde_json::Value = resp.into_json().ok()?;
    v.get("power_on")?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The old `ms + 5_000` panics on overflow in a debug build (and wraps to
    /// a too-short timeout in release) for a `ms` near `u64::MAX`; the
    /// saturating version clamps instead (Review low #8).
    #[test]
    fn dtr_timeout_saturates_instead_of_overflowing() {
        assert_eq!(dtr_timeout_ms(1_000), 15_000, "floored at 15s");
        assert_eq!(dtr_timeout_ms(20_000), 25_000);
        assert_eq!(dtr_timeout_ms(u64::MAX), u64::MAX);
    }

    /// The interface name reaches the daemon's query string percent-encoded
    /// — checked against a real loopback listener, not by inspecting the
    /// formatted string (Review low #3): unescaped, `a b&c` would reshape
    /// `?interface=a b&c&ms=10` into two extra, wrong query parameters.
    #[test]
    fn dtr_press_daemon_percent_encodes_the_interface() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = crate::stubhttp::serve_one(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let ep = Endpoint {
            pid: 1,
            port,
            token: None,
        };
        dtr_press_daemon(&ep, "a b&c", 10).unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("POST /button?interface=a%20b%26c&ms=10 HTTP/1.1"),
            "{req}"
        );
    }

    #[test]
    fn read_power_state_percent_encodes_the_interface() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = crate::stubhttp::serve_one(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
              Content-Length: 17\r\nConnection: close\r\n\r\n{\"power_on\":true}",
        );
        let ep = Endpoint {
            pid: 1,
            port,
            token: None,
        };
        let state = read_power_state(&ep, "a b&c");
        let req = server.join().unwrap();
        assert!(
            req.starts_with("GET /status?interface=a%20b%26c HTTP/1.1"),
            "{req}"
        );
        assert_eq!(state, Some(true));
    }
}
