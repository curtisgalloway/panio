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

//! The table of supported hubs and VINT modules.
//!
//! # Adding a module
//!
//! 1. Add a row to [`MODULES`]: the VINT device id (from the ports
//!    descriptor, shown by `phidget list` as `VINT id 0x..` on an unsupported
//!    module), the SKU, a description, the [`Class`] and the channel count.
//! 2. Check it on hardware: switch one channel and confirm the physical
//!    output with a meter. For a `DigitalOutput` module the `SETDUTYCYCLE`
//!    packet is the same as the REL1100's, so that is usually all it takes.
//!    A new [`Class`] (a different packet set) also needs a variant, a
//!    builder in `proto.rs`, and a CLI verb.
//!
//! Hubs: only the HUB0002 is listed. The HUB0000 (PID 0x3F) and HUB0001
//! (PID 0x43) frame VINT packets differently on old firmware and have not
//! been verified; leave them out until someone tests one.

use crate::proto::{Address, PortsDescriptor};

/// Phidgets, Inc. USB vendor id.
pub const VENDOR_ID: u16 = 0x06C2;

/// What a module does, which decides the packets it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Switchable outputs driven with `SETDUTYCYCLE` (on = full duty).
    DigitalOutput,
}

/// One supported VINT module.
#[derive(Debug, PartialEq, Eq)]
pub struct Module {
    pub vint_id: u16,
    pub sku: &'static str,
    pub description: &'static str,
    pub class: Class,
    pub channels: u8,
}

/// Supported VINT modules.
pub const MODULES: &[Module] = &[Module {
    vint_id: 0x1A,
    sku: "REL1100",
    description: "4-channel solid-state relay",
    class: Class::DigitalOutput,
    channels: 4,
}];

/// A supported hub model.
#[derive(Debug, PartialEq, Eq)]
pub struct HubModel {
    pub pid: u16,
    pub sku: &'static str,
}

/// Supported hubs (USB product ids under [`VENDOR_ID`]).
pub const HUBS: &[HubModel] = &[HubModel {
    pid: 0x0046,
    sku: "HUB0002",
}];

pub fn module_for(vint_id: u16) -> Option<&'static Module> {
    MODULES.iter().find(|m| m.vint_id == vint_id)
}

pub fn hub_model(vendor: u16, pid: u16) -> Option<&'static HubModel> {
    if vendor != VENDOR_ID {
        return None;
    }
    HUBS.iter().find(|h| h.pid == pid)
}

/// Refuse anything but a supported module with that channel. Called before
/// every write: the helper never writes blind.
pub fn check_target(desc: &PortsDescriptor, addr: Address) -> Result<&'static Module, String> {
    let entry = desc.ports[addr.port as usize];
    if entry.id == 0 {
        return Err(format!(
            "port {} is empty (no VINT module attached)",
            addr.port
        ));
    }
    let Some(module) = module_for(entry.id) else {
        return Err(format!(
            "port {} holds VINT id 0x{:02X}, which this helper does not support",
            addr.port, entry.id
        ));
    };
    if addr.channel >= module.channels {
        return Err(format!(
            "channel {} is out of range: the {} on port {} has {} channels (0 to {})",
            addr.channel,
            module.sku,
            addr.port,
            module.channels,
            module.channels - 1
        ));
    }
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(port0: u16, port2: u16) -> PortsDescriptor {
        let mut b = vec![0u8; 42];
        b[0] = 42;
        b[1] = 0x43;
        b[2..4].copy_from_slice(&port0.to_le_bytes());
        b[10..12].copy_from_slice(&port2.to_le_bytes());
        PortsDescriptor::parse(&b).unwrap()
    }

    fn addr(port: u8, channel: u8) -> Address {
        Address { port, channel }
    }

    #[test]
    fn supported_module_and_channel_pass() {
        let d = desc(0x1A, 0);
        let m = check_target(&d, addr(0, 3)).unwrap();
        assert_eq!(m.sku, "REL1100");
        assert_eq!(m.class, Class::DigitalOutput);
    }

    #[test]
    fn empty_port_refused() {
        let e = check_target(&desc(0x1A, 0), addr(1, 0)).unwrap_err();
        assert!(e.contains("port 1 is empty"), "{e}");
    }

    #[test]
    fn unsupported_module_refused() {
        let e = check_target(&desc(0x1A, 0x55), addr(2, 0)).unwrap_err();
        assert!(e.contains("0x55") && e.contains("does not support"), "{e}");
    }

    #[test]
    fn channel_beyond_count_refused() {
        // every address the parser can produce is checked against the module
        let mut d = desc(0x1A, 0);
        d.ports[0].id = 0x1A;
        for ch in 0..4 {
            assert!(check_target(&d, addr(0, ch)).is_ok());
        }
        let e = check_target(&d, addr(0, 4)).unwrap_err();
        assert!(
            e.contains("out of range") && e.contains("4 channels"),
            "{e}"
        );
    }

    #[test]
    fn hub_table() {
        assert_eq!(hub_model(0x06C2, 0x0046).unwrap().sku, "HUB0002");
        assert!(hub_model(0x06C2, 0x003F).is_none());
        assert!(hub_model(0x1234, 0x0046).is_none());
    }
}
