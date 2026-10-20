# Link mode (netboot · link · ffx · off)

`panio netif` owns the **host side** of the point-to-point USB-Ethernet link
to the target and puts it in one of four **mutually-exclusive** modes.

Never run `ifconfig`/`ip`/`networksetup` on the interface by hand: it desyncs
what `netif status` reports.

| mode | Host interface | For |
|---|---|---|
| **netboot** | IPv4 `host_ip`/24 + DHCP + TFTP + HTTP | TFTP/HTTP-booting the target (NET-first boot order) |
| **link** | IPv4 `host_ip`/24 only — **no** DHCP/TFTP daemon, no ffx LL | bringing the bare link up to test it, without serving anything |
| **ffx** | IPv6 link-local `fe80::1`/64; no DHCP/TFTP | reaching a booted Fuchsia target over `ffx` (`fe80::…%<iface>`) |
| **off** | nothing panio set up (host IP + ffx LL released) | parking the link / testing it down |

LL is an IPv6 link-local (`fe80::`) address. `ffx` is Fuchsia's host tool;
`ffx target list` shows `RCS:Y` when it reaches the device and `RCS:N` when it
doesn't.

| | **netboot mode** | **ffx mode** |
|---|---|---|
| Target boots from | TFTP (NET-first boot order) | SD card (firmware reads the FAT partition) |
| Transport to device | TFTP/netsvc (no shell) | SSH → RemoteControlService over `fe80::…%<iface>` |
| What you run | `panio netboot`, `panio serial`, HDMI capture | `ffx target list/show/shell/log`, `ffx component` |

netif prevents two mistakes:

1. **Forgetting `netboot stop` before an SD boot.** The NET-first target then
   TFTP-boots the wrong image instead of the SD card.
2. **No host-side IPv6 link-local.** `ffx` reaches the device at
   `fe80::<dev-slaac>%<iface>`, but without `fe80::1`/64 on the host it stays
   at `RCS:N`. A control-host reboot removes it.

---

## Commands

```bash
# Switch to netboot (IPv4 + DHCP + TFTP + HTTP). Same as `panio netboot start`,
# but first removes any ffx-mode IPv6 link-local.
panio netif mode netboot <target>

# Bring the bare link up: assign the host IP only, no daemon, no ffx LL.
panio netif mode link <target>

# Switch to ffx: stop netboot, then add the host fe80::1/64.
panio netif mode ffx <target>

# Tear down every mode, soft (release the host IP and the ffx LL).
panio netif mode off <target>

# Take the link down HARD: `mode off` + disable Wake-on-LAN + admin-down the
# interface. Whether the peer sees carrier loss depends on the adapter's
# driver; see "Testing the link up and down".
panio netif down-hard <target>

# Show which mode the link is in, its carrier (physical link) state, its
# addresses, and any discovered ffx peer.
panio netif status <target>
```

Omit `<target>` when exactly one is configured, or pass it as `-t/--target`
(`panio netif mode link -t pi5`). Giving both is refused.

Every mode is **idempotent**. `netif mode ffx` re-adds the link-local after a
reboot, `netif mode netboot` skips a redundant start, and `netif mode off`
removes only what netif set up.

**Interface safety:** every `mode netboot|link|ffx|off` and `down-hard`
**refuses** an interface carrying the system default route, like
`netboot start`:

```
refusing netif mode link on 'en0': it carries the system default route (your
primary network interface). netif would reconfigure it and break host
networking. Use a dedicated USB-Ethernet adapter for the netboot link.
```

On a remote control host, bypassing this would lock you out of SSH. Fix the
target's `interface` in the lab file instead (`panio discover` lists
candidates).

---

## What each mode does

- **netboot** — removes the `fe80::1` link-local, then starts DHCP + TFTP +
  HTTP through the normal netboot path, which configures the IPv4 `host_ip`.
- **link** — stops netboot, removes the `fe80::1` link-local, assigns the
  IPv4 `host_ip`/24 and brings the interface up.
  - **Linux:** `ip addr replace` first, then other IPv4 addresses are removed,
    so a failed assignment never leaves the link without one.
  - **macOS:** sets the service to manual (`networksetup -setmanual`) and
    applies the address with `ifconfig`. A `networksetup` error is only a
    warning.
- **ffx** — stops netboot, then enables IPv6 and adds `fe80::1`/64 (Linux:
  `sysctl net/ipv6/conf/<iface>/disable_ipv6=0` + `ip -6 addr add`; macOS:
  `ifconfig … inet6 … alias`), using netboot's `sudo` path.
- **off** — stops netboot, removes the `fe80::1` link-local, and clears
  `host_ip`/24. On macOS it also returns the service to DHCP and releases the
  `ifconfig` alias.

---

## Testing the link up and down

Toggle between `link` and `off` and read `netif status` each time:

```bash
panio netif mode link <target>     # link up: host IP assigned, no daemon
panio netif status <target>        # mode=link, carrier up, inet host_ip/24
panio netif mode off  <target>     # link down: host IP released
panio netif status <target>        # mode=off, inet (none)
```

**`mode off` is a soft down.** It releases the host IP but does not drop the
physical link. Many NICs keep the PHY (physical-layer chip) powered for
Wake-on-LAN (WoL), so `carrier` can read `up` in `off` mode.

**`netif down-hard` is the harder down.** After `mode off` it:

- **Linux:** disables WoL (`sudo ethtool -s <iface> wol d`), then admin-downs
  the interface (`sudo ip link set <iface> down`).
- **macOS:** admin-downs the interface (`sudo ifconfig <iface> down`); WoL is
  system-wide there (`pmset womp`).

`mode link` or `mode netboot` bring the link back. **WoL stays disabled**
until you run `sudo ethtool -s <iface> wol g` or replug the adapter.

**Admin-down does not reliably drop the physical link.** Whether the PHY powers
off is up to the adapter's driver, and with common USB Ethernet adapters it does
not. Measured with a second adapter cabled to the first and watching its link:

| Host | Adapter (driver) | Peer saw link loss? |
|---|---|---|
| macOS | ASIX AX88179A/AX88179B (Apple CDC-NCM) | no |
| macOS | Realtek RTL8153 (Apple CDC-ECM) | no |
| Linux | Realtek RTL8153 (`r8152`) | no |

Apple's NCM/ECM drivers have no way to power the PHY down, so on macOS no
software step drops the link for these adapters. On Linux, `r8152` parks the
PHY in its out-of-band mode on admin-down rather than powering it off, and
`ax88179_178a` only disables receive (read from the driver source; not
measured).

**`netif status` cannot tell you whether the drop happened.** Once the
interface is admin-down, the host reports `carrier down` regardless of the
PHY: macOS `ifconfig` shows `status: inactive`, and Linux refuses to read
`/sys/class/net/<iface>/carrier`, which panio treats as down. Check from the
other end of the cable: the target's own link state, or the link LED.

For a drop the target is guaranteed to see, take the link away outside the
host: unplug the cable, disable the target's port on a managed switch between
host and target, or cut the adapter's USB power from a hub with per-port power
switching.

---

## Status and finding the device

`panio netif status` **probes** the mode, so it is correct after a reboot:

- netboot daemons running → `netboot`
- else the `fe80::1` host link-local present → `ffx`
- else the static `host_ip` present (no daemon, no LL) → `link`
- else → `off`

It also prints `carrier` (macOS `ifconfig … status: active`, Linux
`/sys/class/net/<iface>/carrier`), independent of the mode.

In ffx mode it reads the IPv6 neighbor table (`ip -6 neigh`, Linux) and prints
a ready-to-paste command for any peer:

```
mode    ffx
carrier up
inet6   fe80::1/64
peer    fe80::fc33:fca2:96e0:6dbe%enx001122334455  (try: ffx target add fe80::fc33:fca2:96e0:6dbe%enx001122334455)
```

If no peer is shown, power-cycle the target and wait for SLAAC (IPv6
autoconfiguration).

---

## Typical ffx-over-network flow

```bash
panio netif mode ffx fortune       # stop netboot + ready the host IPv6 side
panio power-cycle fortune          # boots from SD
panio netif status fortune         # grab the device's fe80::…%<iface>
cd ~/src/fuchsia && ./.jiri_root/bin/ffx target list   # expect RCS:Y
```

To go back to TFTP bring-up: `panio netif mode netboot fortune`.

---

## Gotchas

- **`refusing netif <verb> on '<iface>': it carries the system default route
  …`** means the lab file's `interface` is the real NIC. Point it at the
  USB-Ethernet adapter (see *Interface safety*).
- **`mode off` is the soft down.** Use `down-hard` when the target must see
  link loss.
- **macOS service order.** `mode link`/`mode netboot` set the host IP as the
  service's router (`networksetup -setmanual` requires one). Keep the netboot
  adapter *below* your real NIC in `networksetup -listnetworkserviceorder`, or
  the default route can move onto the link.
- **`mode ffx` reports a failed `ifconfig … inet6` add on macOS**, e.g. a
  missing NOPASSWD sudo rule.

---

## Runtime paths

netif keeps no state; the mode comes from netboot's daemon state and the
interface's addresses. See [Netboot](netboot.md) for those paths.
