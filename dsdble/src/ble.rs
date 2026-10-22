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

//! The btleplug glue: find the board by address, connect, locate the command
//! characteristic, write frames. Blocking wrappers over a tokio runtime so the
//! rest of the crate stays synchronous and testable. Nothing here is covered
//! by unit tests (no hardware in tests); the command logic it serves is.
//!
//! **Only ever write to the command characteristic (`FFE1`).** The board also
//! exposes a TI OAD firmware-update service (`f000ffc0-0451-4000-b000-
//! 000000000000`); a stray write there could brick it. This module never
//! names that service: the characteristic is chosen by its UUID alone
//! ([`proto::COMMAND_SHORT_UUID`]) and nothing else is ever passed to
//! `write`.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use btleplug::api::bleuuid::uuid_from_u16;
use btleplug::api::{
    Central, CentralState, CharPropFlags, Characteristic, Manager as _, Peripheral as _,
    ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use tokio::runtime::Runtime;

use crate::proto::{self, COMMAND_SHORT_UUID, SERVICE_SHORT_UUID};
use crate::{Probe, Relay, Seen};

/// Time allowed to establish the connection once the board is in range.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Time allowed for GATT service discovery after connecting.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on one write-with-response round trip.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the scan loop re-reads the adapter's peripheral list.
const POLL: Duration = Duration::from_millis(250);

fn runtime() -> Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| anyhow!("cannot start the async runtime: {e}"))
}

async fn first_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.map_err(|e| {
        anyhow!("cannot reach the Bluetooth stack: {e} (is the Bluetooth service running?)")
    })?;
    let adapters = manager
        .adapters()
        .await
        .map_err(|e| anyhow!("cannot list Bluetooth adapters: {e}"))?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter found"))?;
    if let Ok(CentralState::PoweredOff) = adapter.adapter_state().await {
        bail!("the Bluetooth adapter is powered off");
    }
    Ok(adapter)
}

fn relay_filter() -> ScanFilter {
    ScanFilter {
        services: vec![uuid_from_u16(SERVICE_SHORT_UUID)],
    }
}

/// Scan for `timeout` and list the peripherals advertising the relay
/// service. Read-only: connects to nothing.
pub fn scan(timeout: Duration) -> Result<Vec<Seen>> {
    let rt = runtime()?;
    rt.block_on(async {
        let adapter = first_adapter().await?;
        adapter
            .start_scan(relay_filter())
            .await
            .map_err(|e| anyhow!("cannot start scanning: {e}"))?;
        tokio::time::sleep(timeout).await;
        let peripherals = adapter.peripherals().await;
        let _ = adapter.stop_scan().await;
        let mut found = Vec::new();
        for p in peripherals.map_err(|e| anyhow!("cannot list peripherals: {e}"))? {
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            if !props.services.contains(&uuid_from_u16(SERVICE_SHORT_UUID)) {
                continue;
            }
            found.push(Seen {
                address: proto::display_address(&p.address().to_string(), &p.id().to_string()),
                name: props
                    .local_name
                    .or(props.advertisement_name)
                    .map(|n| proto::printable_name(&n)),
                rssi: props.rssi,
            });
        }
        found.sort_by(|a, b| a.address.cmp(&b.address));
        Ok(found)
    })
}

/// A session with one board: the steps are run in order, either all at once
/// by [`connect`] or one at a time by `doctor` through [`Probe`].
pub struct Session {
    rt: Runtime,
    address: String,
    find_timeout: Duration,
    adapter: Option<Adapter>,
    peripheral: Option<Peripheral>,
    command: Option<Characteristic>,
    connected: bool,
}

/// Find the board, connect, and locate the command characteristic, ready to
/// write frames.
pub fn connect(address: &str, find_timeout: Duration) -> Result<Session> {
    let mut s = Session::new(address.to_string(), find_timeout);
    s.adapter()?;
    s.locate()?;
    s.connect()?;
    s.characteristic()?;
    Ok(s)
}

impl Session {
    /// A session that has done nothing yet. Scanning and connecting happen in
    /// the step methods.
    pub fn new(address: String, find_timeout: Duration) -> Self {
        Session {
            rt: runtime().expect("tokio runtime"),
            address,
            find_timeout,
            adapter: None,
            peripheral: None,
            command: None,
            connected: false,
        }
    }

    /// Disconnect, ignoring errors (the board does not mind a dropped link).
    pub fn close(&mut self) {
        if !self.connected {
            return;
        }
        if let Some(p) = &self.peripheral {
            let _ = self.rt.block_on(p.disconnect());
        }
        self.connected = false;
    }

    fn peripheral(&self) -> Result<&Peripheral> {
        self.peripheral
            .as_ref()
            .ok_or_else(|| anyhow!("internal: board not located yet"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

impl Probe for Session {
    fn adapter(&mut self) -> Result<String> {
        let adapter = self.rt.block_on(first_adapter())?;
        let info = self
            .rt
            .block_on(adapter.adapter_info())
            .unwrap_or_else(|_| "Bluetooth adapter".to_string());
        self.adapter = Some(adapter);
        Ok(format!("{info}, powered on"))
    }

    fn locate(&mut self) -> Result<String> {
        let adapter = self
            .adapter
            .as_ref()
            .ok_or_else(|| anyhow!("internal: no adapter"))?;
        let query = self.address.clone();
        let wait = self.find_timeout;
        let found = self.rt.block_on(async {
            adapter
                .start_scan(relay_filter())
                .await
                .map_err(|e| anyhow!("cannot start scanning: {e}"))?;
            let deadline = tokio::time::Instant::now() + wait;
            let mut hit = None;
            while hit.is_none() {
                for p in adapter.peripherals().await.unwrap_or_default() {
                    if !proto::address_matches(
                        &query,
                        &p.address().to_string(),
                        &p.id().to_string(),
                    ) {
                        continue;
                    }
                    // Same gate as `scan`: the adapter's list can hold cached
                    // peripherals the service filter did not select.
                    let relay = matches!(p.properties().await, Ok(Some(props))
                        if props.services.contains(&uuid_from_u16(SERVICE_SHORT_UUID)));
                    if relay {
                        hit = Some(p);
                        break;
                    }
                }
                if hit.is_some() || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            let _ = adapter.stop_scan().await;
            Ok::<_, anyhow::Error>(hit)
        })?;
        let p = found.ok_or_else(|| {
            anyhow!(
                "board {} not found within {} s (powered, in range, and not already \
                 connected to another device?)",
                self.address,
                wait.as_secs()
            )
        })?;
        let detail = match self.rt.block_on(p.properties()) {
            Ok(Some(props)) => format!(
                "seen as {}, RSSI {}",
                props
                    .local_name
                    .or(props.advertisement_name)
                    .map_or_else(|| "(unnamed)".to_string(), |n| proto::printable_name(&n)),
                props
                    .rssi
                    .map_or_else(|| "unknown".to_string(), |r| format!("{r} dBm"))
            ),
            _ => "seen".to_string(),
        };
        self.peripheral = Some(p);
        Ok(detail)
    }

    fn connect(&mut self) -> Result<String> {
        let p = self.peripheral()?.clone();
        self.rt.block_on(async {
            tokio::time::timeout(CONNECT_TIMEOUT, p.connect())
                .await
                .map_err(|_| anyhow!("connecting timed out after {} s", CONNECT_TIMEOUT.as_secs()))?
                .map_err(|e| anyhow!("cannot connect: {e}"))?;
            Ok::<_, anyhow::Error>(())
        })?;
        self.connected = true;
        self.rt.block_on(async {
            tokio::time::timeout(DISCOVER_TIMEOUT, p.discover_services())
                .await
                .map_err(|_| anyhow!("service discovery timed out"))?
                .map_err(|e| anyhow!("service discovery failed: {e}"))
        })?;
        Ok("connected, services discovered".to_string())
    }

    fn characteristic(&mut self) -> Result<String> {
        let want = uuid_from_u16(COMMAND_SHORT_UUID);
        let found = self
            .peripheral()?
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == want);
        let Some(c) = found else {
            bail!("command characteristic FFE1 not found (is this a DSD TECH relay board?)");
        };
        if !c.properties.contains(CharPropFlags::WRITE) {
            bail!("command characteristic FFE1 is not writable");
        }
        self.command = Some(c);
        Ok("FFE1 present and writable (the board has no read-back)".to_string())
    }
}

impl Relay for Session {
    fn write_frame(&mut self, frame: [u8; 4]) -> Result<()> {
        let c = self
            .command
            .as_ref()
            .ok_or_else(|| anyhow!("internal: no command characteristic"))?;
        let p = self.peripheral()?;
        self.rt.block_on(async {
            tokio::time::timeout(WRITE_TIMEOUT, p.write(c, &frame, WriteType::WithResponse))
                .await
                .map_err(|_| {
                    anyhow!(
                        "the write was not acknowledged within {} s",
                        WRITE_TIMEOUT.as_secs()
                    )
                })?
                .map_err(|e| anyhow!("write failed: {e}"))
        })
    }
}
