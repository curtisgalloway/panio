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

//! Shelly Gen2+ local HTTP RPC client.
//!
//! Gen2/3/4 devices (Plus, Pro, Gen3, Gen4) expose a JSON-RPC API over plain
//! HTTP. We use the REST-style GET form — `GET /rpc/<Method>?<params>` returns
//! the result JSON directly — which keeps each call a single stateless
//! request, exactly what the one-shot power-hook contract wants.
//!
//! Auth: the helper currently targets devices with authentication disabled
//! (`auth_en: false`). When a device demands auth it answers HTTP 401 with a
//! digest challenge; [`Client::call`] is the single chokepoint where that
//! retry (Shelly's SHA-256 HTTP digest) would be added.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Per-request timeout. Generous enough for a sleepy Wi-Fi device to answer,
/// short enough that a wrong/dead address fails a hook promptly.
const CALL_TIMEOUT: Duration = Duration::from_secs(6);

/// A Shelly Gen2+ device addressed over its local HTTP RPC API.
pub struct Client {
    base: String,
}

/// Subset of `Shelly.GetDeviceInfo` we surface.
#[derive(Debug, Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    pub name: Option<String>,
    pub id: String,
    pub model: String,
    #[serde(rename = "gen")]
    pub generation: u32,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub ver: Option<String>,
    #[serde(default)]
    pub auth_en: bool,
}

/// Internal temperature block of a switch status.
#[derive(Debug, Deserialize)]
pub struct Temperature {
    #[serde(rename = "tC")]
    pub t_c: Option<f64>,
}

/// Accumulated-energy block of a switch status (`total` is in watt-hours).
#[derive(Debug, Deserialize)]
pub struct Energy {
    pub total: Option<f64>,
}

/// Subset of `Switch.GetStatus` we surface. `output` is the relay state — the
/// single field the on/off/state/cycle contract turns on; the rest is metering
/// shown by `status`.
#[derive(Debug, Deserialize)]
pub struct SwitchStatus {
    pub output: bool,
    #[serde(default)]
    pub apower: Option<f64>,
    #[serde(default)]
    pub voltage: Option<f64>,
    #[serde(default)]
    pub current: Option<f64>,
    #[serde(default)]
    pub temperature: Option<Temperature>,
    #[serde(default)]
    pub aenergy: Option<Energy>,
}

impl Client {
    /// Build a client from a user-supplied address. Accepts a bare IP or
    /// hostname (`10.0.0.5`, `shelly.local`), an explicit scheme
    /// (`http://10.0.0.5`), and/or a port (`10.0.0.5:8080`).
    pub fn new(device: &str) -> Self {
        let d = device.trim().trim_end_matches('/');
        let base = if d.starts_with("http://") || d.starts_with("https://") {
            d.to_string()
        } else {
            format!("http://{d}")
        };
        Client { base }
    }

    /// The one place every RPC request is issued — and the hook point for a
    /// future HTTP-401 digest-auth retry. Returns the parsed JSON result,
    /// mapping transport failures and Shelly-level errors to clear messages.
    fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let url = format!("{}/rpc/{method}", self.base);
        let mut req = ureq::get(&url).timeout(CALL_TIMEOUT);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json::<serde_json::Value>()
                .with_context(|| format!("decoding {method} response")),
            Err(ureq::Error::Status(code, resp)) => {
                // Shelly reports RPC errors as a JSON body, either
                // {"code":..,"message":".."} or {"error":{"message":".."}}.
                let body = resp.into_json::<serde_json::Value>().ok();
                let detail = body
                    .as_ref()
                    .and_then(|b| {
                        b.get("message")
                            .or_else(|| b.get("error").and_then(|e| e.get("message")))
                    })
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                if code == 401 {
                    bail!(
                        "{method}: device requires authentication (HTTP 401){} — \
                         shellyplug does not yet support password-protected devices",
                        detail.map(|d| format!(": {d}")).unwrap_or_default()
                    );
                }
                match detail {
                    Some(d) => bail!("{method}: {d} (HTTP {code})"),
                    None => bail!("{method}: HTTP {code}"),
                }
            }
            Err(ureq::Error::Transport(t)) => {
                Err(anyhow!("cannot reach Shelly at {}: {t}", self.base))
            }
        }
    }

    /// `Shelly.GetDeviceInfo` — identity, generation, firmware, auth flag.
    pub fn device_info(&self) -> Result<DeviceInfo> {
        let v = self.call("Shelly.GetDeviceInfo", &[])?;
        serde_json::from_value(v).context("parsing Shelly.GetDeviceInfo")
    }

    /// `Switch.GetStatus` for one switch component (`id`).
    pub fn switch_status(&self, id: u32) -> Result<SwitchStatus> {
        let ids = id.to_string();
        let v = self.call("Switch.GetStatus", &[("id", ids.as_str())])?;
        serde_json::from_value(v).with_context(|| format!("parsing Switch.GetStatus id={id}"))
    }

    /// `Shelly.GetStatus` — every component's status, keyed `<type>:<id>`
    /// (`switch:0`, `sys`, `wifi`, …). Read-only; `doctor` uses the keys to
    /// list the switches a device actually has.
    pub fn all_status(&self) -> Result<serde_json::Value> {
        self.call("Shelly.GetStatus", &[])
    }

    /// `Switch.Set` — command the relay on or off. The caller confirms by
    /// re-reading [`Client::switch_status`] (the contract wants read-back).
    pub fn switch_set(&self, id: u32, on: bool) -> Result<()> {
        let ids = id.to_string();
        let ons = if on { "true" } else { "false" };
        self.call("Switch.Set", &[("id", ids.as_str()), ("on", ons)])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalization() {
        assert_eq!(Client::new("10.0.0.5").base, "http://10.0.0.5");
        assert_eq!(Client::new("shelly.local").base, "http://shelly.local");
        assert_eq!(Client::new("10.0.0.5:8080").base, "http://10.0.0.5:8080");
        assert_eq!(Client::new("http://10.0.0.5").base, "http://10.0.0.5");
        assert_eq!(Client::new("https://10.0.0.5/").base, "https://10.0.0.5");
        assert_eq!(Client::new("  10.0.0.5/  ").base, "http://10.0.0.5");
    }
}
