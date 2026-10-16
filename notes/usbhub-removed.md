<!--
SPDX-FileCopyrightText: 2026 Curtis Galloway
SPDX-License-Identifier: Apache-2.0
-->

# Removed: `usbhub` (per-port USB hub power)

paniolo shipped a `usbhub` helper that switched VBUS on individual ports of
off-the-shelf hubs via USB hub-class requests — the uhubctl mechanism, in pure
Rust. It was removed in full. Do not rebuild it without reading this.

Why it went:

- **The `learn` procedure was unusable.** Hubs do not report which port maps to
  which silkscreen number, and chips routinely claim per-port switching with no
  VBUS MOSFETs behind it. So a profile could only be built by a human physically
  watching a device lose power, port by port, through a resumable wizard. That is
  a lot of ceremony to configure one power hook, and it had to be redone per hub
  model.
- **The hardware is hard to get and unreliable.** Hubs with genuinely switchable
  per-port VBUS are a shrinking niche, and the ones that work are not
  consistently available.
- **Port state did not survive the hub losing power.** Power-cycling the hub
  turned every port back on, so the helper's state could silently disagree with
  reality.

It was also the only power helper with device-specific support inside `cli/`
(`usbhub_profiles.rs`, a `USBHUB_LIBRARY_PATH` special case in
`daemons::helper_env`, and a bundled profile library in the `.deb`). That
contradicted the rule in **Source code constraints** that device knowledge lives
in helper binaries behind the generic hooks — so removing it took a carve-out
out of the core as well as a crate out of the tree.

**What to use instead:** `shellyplug` (Shelly Gen2+ over local HTTP RPC),
`zigplug` (Zigbee), `amt` (Intel AMT/vPro, with true power-state readback),
`cambrionix` (Cambrionix hub ports), or a relay driven through the generic
hooks — which is what the CI rack actually uses. If per-port USB power is ever
needed again, the git history has the implementation; the mechanism was never
the problem.

