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

//! USB I/O for a Phidgets VINT hub, over raw USB with `nusb`.
//!
//! Finds hubs, opens one, claims interface 0, reads the VINT ports descriptor
//! and exchanges bulk packets: OUT on endpoint 0x02, IN on endpoint 0x81
//! (both bulk, measured). The wire format lives in `proto.rs`.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlType, In, Out, Recipient};
use nusb::{DeviceInfo, ErrorKind, MaybeFuture};

use crate::devices::{hub_model, VENDOR_ID};
use crate::proto::{self, PortsDescriptor, Status, PORTS_DESCRIPTOR_LEN, PORTS_DESCRIPTOR_TYPE};

const EP_OUT: u8 = 0x02;
const EP_IN: u8 = 0x81;
const MAX_PACKET: usize = 64;
/// How long to wait for the hub's ACK, and for each transfer.
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
/// A read this short that times out means nothing stale is queued.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(20);
/// How long to keep retrying a claim that fails with "busy".
const CLAIM_RETRY: Duration = Duration::from_secs(3);

/// What the command logic needs from a hub, so tests can use a fake.
pub trait Link {
    /// Read and parse the VINT ports descriptor.
    fn ports(&mut self) -> Result<PortsDescriptor>;
    /// Write one packet and wait for the hub to ACK `id`.
    fn send_acked(&mut self, packet: &[u8], id: u8) -> Result<()>;
}

/// A supported hub found on the bus.
pub struct HubInfo {
    pub info: DeviceInfo,
    pub serial: String,
    pub sku: &'static str,
    /// Decoded firmware version (from `bcdDevice`).
    pub firmware: u32,
}

/// All supported hubs attached right now.
pub fn find_hubs() -> Result<Vec<HubInfo>> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| anyhow!("cannot list USB devices: {e}"))?;
    let mut hubs: Vec<HubInfo> = devices
        .filter_map(|info| {
            let model = hub_model(info.vendor_id(), info.product_id())?;
            Some(HubInfo {
                serial: info.serial_number().unwrap_or("").to_string(),
                sku: model.sku,
                firmware: proto::decode_version(info.device_version()),
                info,
            })
        })
        .collect();
    hubs.sort_by(|a, b| a.serial.cmp(&b.serial));
    Ok(hubs)
}

/// Choose the index of the hub to use among `serials`. With `want` set, it
/// must match exactly; without it, exactly one hub must be attached.
pub fn select_index(serials: &[&str], want: Option<&str>) -> Result<usize> {
    if serials.is_empty() {
        bail!(
            "no supported Phidgets hub found (looking for a HUB0002, USB {VENDOR_ID:04x}); \
             is it plugged in and powered?"
        );
    }
    match want {
        Some(w) => serials
            .iter()
            .position(|s| *s == w)
            .ok_or_else(|| anyhow!("no hub with serial `{w}`; attached: {}", serials.join(", "))),
        None if serials.len() == 1 => Ok(0),
        None => bail!(
            "{} hubs are attached; choose one with -s/--serial: {}",
            serials.len(),
            serials.join(", ")
        ),
    }
}

/// Find the hub to use, per [`select_index`].
pub fn select_hub(want: Option<&str>) -> Result<HubInfo> {
    let mut hubs = find_hubs()?;
    let serials: Vec<&str> = hubs.iter().map(|h| h.serial.as_str()).collect();
    let i = select_index(&serials, want)?;
    Ok(hubs.swap_remove(i))
}

/// An open, claimed hub.
pub struct UsbLink {
    iface: nusb::Interface,
    out: Option<nusb::Endpoint<Bulk, Out>>,
    inp: Option<nusb::Endpoint<Bulk, In>>,
}

fn open_error(e: nusb::Error, what: &str) -> anyhow::Error {
    match e.kind() {
        ErrorKind::PermissionDenied => anyhow!(
            "permission denied {what}; on Linux install a udev rule that grants your user \
             access to USB vendor 06c2 (idVendor 06c2, see docs/power.md)"
        ),
        ErrorKind::Busy => anyhow!(
            "the hub is busy {what}: another process holds it (a concurrent phidget hook, \
             Phidget Control Panel, or phidget22networkserver)"
        ),
        ErrorKind::Disconnected => anyhow!("the hub was unplugged {what}"),
        _ => anyhow!("cannot open the hub ({what}): {e}"),
    }
}

impl UsbLink {
    /// Open the hub and claim interface 0, retrying a busy claim for up to
    /// three seconds.
    pub fn open(hub: &HubInfo) -> Result<UsbLink> {
        let device = hub
            .info
            .open()
            .wait()
            .map_err(|e| open_error(e, "opening the device"))?;
        let start = Instant::now();
        let iface = loop {
            match device.claim_interface(0).wait() {
                Ok(i) => break i,
                Err(e) if e.kind() == ErrorKind::Busy && start.elapsed() < CLAIM_RETRY => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(open_error(e, "claiming interface 0")),
            }
        };
        Ok(UsbLink {
            iface,
            out: None,
            inp: None,
        })
    }
}

impl Link for UsbLink {
    fn ports(&mut self) -> Result<PortsDescriptor> {
        let bytes = self
            .iface
            .control_in(
                ControlIn {
                    control_type: ControlType::Standard,
                    recipient: Recipient::Device,
                    request: 0x06,
                    value: u16::from(PORTS_DESCRIPTOR_TYPE) << 8,
                    index: 0,
                    length: PORTS_DESCRIPTOR_LEN as u16,
                },
                CONTROL_TIMEOUT,
            )
            .wait()
            .map_err(|e| anyhow!("reading the VINT ports descriptor failed: {e}"))?;
        PortsDescriptor::parse(&bytes).map_err(|e| anyhow!(e))
    }

    fn send_acked(&mut self, packet: &[u8], id: u8) -> Result<()> {
        if self.out.is_none() {
            self.out = Some(
                self.iface
                    .endpoint::<Bulk, Out>(EP_OUT)
                    .map_err(|e| anyhow!("bulk OUT endpoint 0x02: {e}"))?,
            );
        }
        if self.inp.is_none() {
            self.inp = Some(
                self.iface
                    .endpoint::<Bulk, In>(EP_IN)
                    .map_err(|e| anyhow!("bulk IN endpoint 0x81: {e}"))?,
            );
        }
        let out = self.out.as_mut().expect("set above");
        let inp = self.inp.as_mut().expect("set above");

        // Every one-shot run uses the same packet id, so an ACK left unread
        // by an earlier run that timed out would pass for this packet's ACK.
        // An idle hub sends nothing, so read until a short read times out.
        let drain_until = Instant::now() + ACK_TIMEOUT;
        while Instant::now() < drain_until {
            let c = inp.transfer_blocking(Buffer::new(MAX_PACKET), DRAIN_TIMEOUT);
            if c.status.is_err() {
                break;
            }
        }

        let mut buf = Buffer::new(packet.len());
        buf.extend_from_slice(packet);
        out.transfer_blocking(buf, ACK_TIMEOUT)
            .status
            .map_err(|e| anyhow!("writing the packet to the hub failed: {e}"))?;

        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "timed out after {} s waiting for the hub to acknowledge the packet",
                    ACK_TIMEOUT.as_secs()
                );
            }
            let c = inp.transfer_blocking(Buffer::new(MAX_PACKET), left);
            match c.status {
                Ok(()) => {
                    let data = &c.buffer[..c.actual_len.min(c.buffer.len())];
                    match proto::status_for(data, id) {
                        Some(Status::Ack) => return Ok(()),
                        Some(other) => {
                            bail!("the hub refused the packet: {other} (packet id {id})")
                        }
                        None => continue,
                    }
                }
                Err(nusb::transfer::TransferError::Cancelled) => continue,
                Err(e) => bail!("reading the hub's reply failed: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_hub_needs_no_serial() {
        assert_eq!(select_index(&["AA00BB11"], None).unwrap(), 0);
    }

    #[test]
    fn serial_picks_among_several() {
        assert_eq!(
            select_index(&["AA00BB11", "CC22DD33"], Some("CC22DD33")).unwrap(),
            1
        );
    }

    #[test]
    fn several_hubs_without_serial_lists_them() {
        let e = select_index(&["AA00BB11", "CC22DD33"], None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("AA00BB11") && e.contains("CC22DD33") && e.contains("-s"),
            "{e}"
        );
    }

    #[test]
    fn unknown_serial_lists_attached() {
        let e = select_index(&["AA00BB11"], Some("ZZ"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("`ZZ`") && e.contains("AA00BB11"), "{e}");
    }

    #[test]
    fn no_hub_is_an_error() {
        assert!(select_index(&[], None).is_err());
        assert!(select_index(&[], Some("AA00BB11")).is_err());
    }
}
