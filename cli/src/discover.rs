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

//! Hardware discovery for lab authoring.
//!
//! `panio discover` lists this host's lab-relevant hardware (USB-Ethernet,
//! serial, capture devices, adb devices); `panio configure` runs it over SSH on a lab host
//! and renders a proposed `[targets.<name>]` block for review. The proposal is
//! never written — the human approves it by adding it to the lab and committing.

use serde_json::{json, Value};

use crate::{daemons, netif, serial, video};

/// Capture-device names that are built-in cameras, not HDMI capture.
const BUILTIN_CAPTURE: [&str; 5] = ["FaceTime", "Capture screen", "iSight", "iPhone", "iPad"];

/// This host's lab-relevant hardware, in the same JSON shape the Python CLI
/// emits (so mixed-version labs interoperate during the migration; the video
/// entries' `id` field and the `adb` array are Rust-side additions, ignored by
/// older consumers).
pub fn local_inventory() -> Value {
    let ethernet: Vec<Value> = netif::list_usb_ethernet_interfaces()
        .iter()
        .map(|e| json!({"port": e.port, "device": e.device, "active": e.active}))
        .collect();
    let serial: Vec<Value> = serial::list_devices()
        .into_iter()
        .map(Value::String)
        .collect();
    json!({
        "ethernet": ethernet,
        "serial": serial,
        "video": list_capture_devices(),
        "adb": list_adb_devices(),
    })
}

/// Capture devices from `hdmicap devices --json`:
/// `[{index, name, misc, id}, ...]` — `id` is the stable, port-derived
/// identifier (AVFoundation uniqueID on macOS, /dev/v4l/by-path on Linux).
fn list_capture_devices() -> Vec<Value> {
    let Some(binary) = daemons::find_binary(video::DAEMON) else {
        return Vec::new();
    };
    let Ok(out) = std::process::Command::new(binary)
        .args(["devices", "--json"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    serde_json::from_slice(&out.stdout).unwrap_or_default()
}

/// Authorized adb devices as `[{serial, model}, ...]` from `adb devices -l`.
/// Empty when adb isn't installed/answering. Devices not in the `device` state
/// (unauthorized/offline) are skipped — they can't be driven yet.
fn list_adb_devices() -> Vec<Value> {
    let Ok(out) = std::process::Command::new("adb")
        .args(["devices", "-l"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_adb_devices(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `adb devices -l` text into `[{serial, model?}]`, keeping only devices
/// in the `device` state. Pure (no I/O) so it can be unit-tested.
fn parse_adb_devices(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // Skip the header and adb-server startup chatter.
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        if state != "device" {
            continue;
        }
        // `-l` adds `model:Pixel_6a` etc.; underscores stand in for spaces.
        match parts.find_map(|p| p.strip_prefix("model:")) {
            Some(model) => out.push(json!({"serial": serial, "model": model.replace('_', " ")})),
            None => out.push(json!({"serial": serial})),
        }
    }
    out
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Split a `/dev/serial/by-id/` name ending in `-if<NN>` or `-if<NN>-port<N>`
/// into the adapter's name and the USB interface number `NN`. udev names every
/// interface of a composite adapter this way (`-port<N>` for `ftdi_sio`, e.g.
/// FT2232 `0403:6010` and FT4232 `0403:6011`; bare `-if<NN>` for CDC ACM), so
/// the prefix identifies the physical adapter.
fn usb_interface(path: &str) -> Option<(&str, &str)> {
    if !path.contains("/by-id/") {
        return None;
    }
    let digits = |s: &str, hex: bool| {
        !s.is_empty()
            && s.bytes().all(|b| {
                if hex {
                    b.is_ascii_hexdigit()
                } else {
                    b.is_ascii_digit()
                }
            })
    };
    let rest = match path.rsplit_once("-port") {
        Some((rest, port)) if digits(port, false) => rest,
        _ => path,
    };
    let (adapter, iface) = rest.rsplit_once("-if")?;
    digits(iface, true).then_some((adapter, iface))
}

/// Group serial paths by physical adapter, keeping first-seen order. A path
/// that is not one interface of a by-id adapter is a group of its own.
fn group_serial_adapters<'a>(serials: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut groups: Vec<(Option<&str>, Vec<&'a str>)> = Vec::new();
    for &path in serials {
        let adapter = usb_interface(path).map(|(a, _)| a);
        match groups
            .iter_mut()
            .find(|(a, _)| adapter.is_some() && *a == adapter)
        {
            Some((_, ports)) => ports.push(path),
            None => groups.push((adapter, vec![path])),
        }
    }
    groups.into_iter().map(|(_, ports)| ports).collect()
}

/// A trailing comment naming a multi-interface adapter port's interface, with
/// the FTDI convention that interface 00 (channel A) is usually JTAG/MPSSE.
fn interface_note(path: &str) -> String {
    match usb_interface(path) {
        Some((_, "00")) => "  # interface 00: often JTAG/MPSSE on FT2232/FT4232 boards".to_string(),
        Some((_, iface)) => format!("  # interface {iface}"),
        None => String::new(),
    }
}

/// Render a proposed `[targets.<name>]` lab block from a host's inventory.
/// Best-guesses one value per channel; alternatives become comments. Meant to
/// be reviewed and pasted into the lab — panio never writes it.
pub fn propose_target_block(name: &str, host: &str, inv: &Value) -> String {
    let mut out: Vec<String> = Vec::new();
    out.push(format!("[targets.{name}]"));
    out.push(format!("host = \"{host}\""));
    out.push(String::new());

    // netboot: prefer the carrier-up interface (the list is sorted actives-first).
    let eths: Vec<&Value> = inv
        .get("ethernet")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    out.push(format!("[targets.{name}.netboot]"));
    if let Some(first) = eths.first() {
        let dev = str_at(first, "device").unwrap_or("");
        let note = if first
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "  # carrier up"
        } else {
            ""
        };
        out.push(format!("interface = \"{dev}\"{note}"));
        for e in &eths[1..] {
            let dev = str_at(e, "device").unwrap_or("");
            out.push(format!("# interface = \"{dev}\"  # alternative"));
        }
    } else {
        out.push("# interface = \"\"  # no USB-Ethernet interface discovered".to_string());
    }
    out.push("# tftp_root = \"/path/to/tftp\"  # set to enable netboot".to_string());
    out.push(String::new());

    // serial: the first single-interface device as the console; the rest as
    // comments. Interfaces of one multi-interface adapter (FT2232/FT4232
    // JTAG+UART boards) are never guessed between: they become a pick-one
    // comment block, since channel A is often JTAG/MPSSE, not the console.
    let serials: Vec<&str> = inv
        .get("serial")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let adapters = group_serial_adapters(&serials);
    let console = adapters
        .iter()
        .find_map(|ports| (ports.len() == 1).then_some(ports[0]));
    let mut extras: Vec<String> = Vec::new();
    for ports in &adapters {
        if let [port] = ports.as_slice() {
            if Some(*port) != console {
                extras.push(format!("# another serial device: {port}"));
            }
        } else {
            extras.push("# one adapter, several interfaces — pick the UART channel:".to_string());
            for port in ports {
                extras.push(format!("# device = \"{port}\"{}", interface_note(port)));
            }
        }
    }
    if let Some(first) = console {
        out.push(format!("[[targets.{name}.serial]]"));
        out.push("name = \"console\"".to_string());
        out.push(format!("device = \"{first}\""));
        out.push("baud = 115200".to_string());
    } else if extras.is_empty() {
        out.push(format!(
            "# [[targets.{name}.serial]]  # no serial devices discovered"
        ));
    } else {
        out.push(format!("# [[targets.{name}.serial]]"));
        out.push("# name = \"console\"".to_string());
        out.push("# baud = 115200".to_string());
    }
    out.extend(extras);
    out.push(String::new());

    // video: propose the one non-built-in capture device, if unambiguous.
    // Prefer the stable id (port-derived, survives enumeration-order shifts)
    // with the human name as a comment; fall back to the name when the
    // discovering hdmicap reported no id.
    let device_ref = |name: &str, id: &str| {
        if id.is_empty() {
            format!("device = \"{name}\"")
        } else {
            format!("device = \"{id}\"  # {name}")
        }
    };
    let captures: Vec<(&str, &str)> = inv
        .get("video")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| Some((str_at(d, "name")?, str_at(d, "id").unwrap_or(""))))
                .filter(|(n, _)| !BUILTIN_CAPTURE.iter().any(|b| n.contains(b)))
                .collect()
        })
        .unwrap_or_default();
    if let [(cap_name, cap_id)] = captures.as_slice() {
        out.push(format!("[targets.{name}.video]"));
        out.push(device_ref(cap_name, cap_id));
    } else if captures.is_empty() {
        out.push(format!(
            "# [targets.{name}.video]  # no capture device discovered"
        ));
    } else {
        out.push(format!(
            "# [targets.{name}.video]  # multiple capture devices — pick one:"
        ));
        for (cap_name, cap_id) in &captures {
            out.push(format!("# {}", device_ref(cap_name, cap_id)));
        }
    }
    out.push(String::new());

    // adb: propose the one authorized device; multiple → pick-one comments.
    let adb_line = |serial: &str, model: &str| {
        let note = if model.is_empty() {
            String::new()
        } else {
            format!("  # {model}")
        };
        format!("serial = \"{serial}\"{note}")
    };
    let adbs: Vec<(&str, &str)> = inv
        .get("adb")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| Some((str_at(d, "serial")?, str_at(d, "model").unwrap_or(""))))
                .collect()
        })
        .unwrap_or_default();
    if let [(serial, model)] = adbs.as_slice() {
        out.push(format!("[targets.{name}.adb]"));
        out.push(adb_line(serial, model));
    } else if adbs.is_empty() {
        out.push(format!(
            "# [targets.{name}.adb]  # no adb devices discovered"
        ));
    } else {
        out.push(format!(
            "# [targets.{name}.adb]  # multiple adb devices — pick one:"
        ));
        for (serial, model) in &adbs {
            out.push(format!("# {}", adb_line(serial, model)));
        }
    }
    out.push(String::new());

    out.push(format!("# [targets.{name}.power]"));
    out.push("# cycle_cmd = \"/path/to/power-cycle.sh\"  # not discoverable".to_string());

    let mut s = out.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const FT2232_A: &str = "/dev/serial/by-id/usb-FTDI_Dual_RS232-HS_AA00BB11-if00-port0";
    const FT2232_B: &str = "/dev/serial/by-id/usb-FTDI_Dual_RS232-HS_AA00BB11-if01-port0";

    #[test]
    fn usb_interface_parses_by_id_interface_suffix() {
        assert_eq!(
            usb_interface(FT2232_B),
            Some(("/dev/serial/by-id/usb-FTDI_Dual_RS232-HS_AA00BB11", "01"))
        );
        assert_eq!(
            usb_interface("/dev/serial/by-id/usb-Raspberry_Pi_Debugprobe_AA00BB11CC22DD33-if01"),
            Some((
                "/dev/serial/by-id/usb-Raspberry_Pi_Debugprobe_AA00BB11CC22DD33",
                "01"
            ))
        );
        assert_eq!(
            usb_interface("/dev/serial/by-id/usb-Prolific_PL2303-port0"),
            None
        );
        assert_eq!(usb_interface("/dev/tty.usbserial-A"), None);
        // by-path names carry the interface differently; not grouped.
        assert_eq!(
            usb_interface("/dev/serial/by-path/pci-0000:00:14.0-usb-0:2:1.0-port0"),
            None
        );
    }

    #[test]
    fn propose_never_guesses_between_interfaces_of_one_adapter() {
        let inv = json!({
            "ethernet": [], "video": [],
            "serial": [FT2232_A, FT2232_B],
        });
        let block = propose_target_block("t", "bench1", &inv);
        // Neither channel is proposed live: interface 00 is usually JTAG.
        assert!(!block.contains("\n[[targets.t.serial]]"), "{block}");
        assert!(!block.contains("\ndevice = "), "{block}");
        assert!(block.contains("# [[targets.t.serial]]\n"), "{block}");
        assert!(
            block.contains("# one adapter, several interfaces — pick the UART channel:"),
            "{block}"
        );
        assert!(
            block.contains(&format!(
                "# device = \"{FT2232_A}\"  # interface 00: often JTAG/MPSSE"
            )),
            "{block}"
        );
        assert!(
            block.contains(&format!("# device = \"{FT2232_B}\"  # interface 01")),
            "{block}"
        );
        assert!(!block.contains("another serial device"), "{block}");
    }

    #[test]
    fn propose_prefers_single_interface_adapter_over_composite() {
        // Sorted order puts the FT2232 first; the plain adapter still wins.
        let plain = "/dev/serial/by-id/usb-Prolific_PL2303-port0";
        let inv = json!({
            "ethernet": [], "video": [],
            "serial": [FT2232_A, FT2232_B, plain],
        });
        let block = propose_target_block("t", "bench1", &inv);
        assert!(block.contains("\n[[targets.t.serial]]\n"), "{block}");
        assert!(
            block.contains(&format!("\ndevice = \"{plain}\"")),
            "{block}"
        );
        assert!(
            block.contains(&format!("# device = \"{FT2232_A}\"  # interface 00")),
            "{block}"
        );
        assert!(!block.contains("another serial device"), "{block}");
    }

    #[test]
    fn propose_prefers_active_eth_and_first_serial() {
        let inv = json!({
            "ethernet": [
                {"port": "AX88179A", "device": "en16", "active": true},
                {"port": "Ethernet Adapter", "device": "en8", "active": false},
            ],
            "serial": ["/dev/tty.usbserial-A", "/dev/tty.usbserial-B"],
            "video": [
                {"index": 0, "name": "USB Video", "misc": "", "id": "0x8300000534d2109"},
                {"index": 1, "name": "FaceTime HD Camera", "misc": "", "id": "0x11000005ac8514"},
            ],
        });
        let block = propose_target_block("pi5", "bench1", &inv);
        assert!(block.contains("[targets.pi5]"), "{block}");
        assert!(block.contains("host = \"bench1\""), "{block}");
        assert!(
            block.contains("interface = \"en16\"  # carrier up"),
            "{block}"
        );
        assert!(
            block.contains("# interface = \"en8\"  # alternative"),
            "{block}"
        );
        assert!(
            block.contains("device = \"/dev/tty.usbserial-A\""),
            "{block}"
        );
        assert!(
            block.contains("# another serial device: /dev/tty.usbserial-B"),
            "{block}"
        );
        // FaceTime filtered as built-in → USB Video is the unambiguous capture,
        // proposed by stable id with the name as a comment.
        assert!(block.contains("[targets.pi5.video]"), "{block}");
        assert!(
            block.contains("device = \"0x8300000534d2109\"  # USB Video"),
            "{block}"
        );
    }

    #[test]
    fn propose_falls_back_to_name_without_id() {
        let inv = json!({
            "ethernet": [],
            "serial": [],
            "video": [{"index": 0, "name": "USB Video", "misc": ""}],
        });
        let block = propose_target_block("t", "local", &inv);
        assert!(block.contains("device = \"USB Video\""), "{block}");
    }

    #[test]
    fn propose_lists_duplicate_dongles_as_id_alternatives() {
        let inv = json!({
            "ethernet": [],
            "serial": [],
            "video": [
                {"index": 0, "name": "USB Video", "misc": "", "id": "0x8300000534d2109"},
                {"index": 1, "name": "USB Video", "misc": "", "id": "0x8200000534d2109"},
            ],
        });
        let block = propose_target_block("t", "local", &inv);
        assert!(block.contains("multiple capture devices"), "{block}");
        assert!(
            block.contains("# device = \"0x8300000534d2109\"  # USB Video"),
            "{block}"
        );
        assert!(
            block.contains("# device = \"0x8200000534d2109\"  # USB Video"),
            "{block}"
        );
    }

    #[test]
    fn propose_with_empty_inventory_is_all_stubs() {
        // No `adb` key at all — the missing-key path must still stub it out.
        let inv = json!({"ethernet": [], "serial": [], "video": []});
        let block = propose_target_block("t", "local", &inv);
        assert!(block.contains("# interface = \"\""), "{block}");
        assert!(block.contains("# [[targets.t.serial]]"), "{block}");
        assert!(block.contains("# [targets.t.video]"), "{block}");
        assert!(
            block.contains("# [targets.t.adb]  # no adb devices discovered"),
            "{block}"
        );
    }

    #[test]
    fn propose_includes_single_adb_device() {
        let inv = json!({
            "ethernet": [], "serial": [], "video": [],
            "adb": [{"serial": "33271JEGR02033", "model": "Pixel 6a"}],
        });
        let block = propose_target_block("pixel", "bench1", &inv);
        assert!(block.contains("[targets.pixel.adb]"), "{block}");
        assert!(
            block.contains("serial = \"33271JEGR02033\"  # Pixel 6a"),
            "{block}"
        );
    }

    #[test]
    fn propose_lists_multiple_adb_devices_as_alternatives() {
        let inv = json!({
            "ethernet": [], "serial": [], "video": [],
            "adb": [{"serial": "AAA", "model": "Pixel 6a"}, {"serial": "BBB"}],
        });
        let block = propose_target_block("t", "local", &inv);
        assert!(block.contains("multiple adb devices"), "{block}");
        assert!(block.contains("# serial = \"AAA\"  # Pixel 6a"), "{block}");
        assert!(block.contains("# serial = \"BBB\""), "{block}");
    }

    #[test]
    fn parse_adb_devices_keeps_only_authorized() {
        let text = "List of devices attached\n\
             * daemon not running; starting now at tcp:5037\n\
             33271JEGR02033         device usb:8-3.4 product:bluejay model:Pixel_6a transport_id:1\n\
             EMULATOR30            offline\n\
             FA6population          unauthorized\n";
        let devs = parse_adb_devices(text);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0]["serial"], "33271JEGR02033");
        assert_eq!(devs[0]["model"], "Pixel 6a");
    }
}
