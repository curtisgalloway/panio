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

//! netbootd — single-client netboot daemon: DHCP + read-only TFTP (+ HTTP) in
//! one process.
//!
//! Proof-of-concept port of paniolo's `_dhcp.py` + `_tftp.py`. Unlike the
//! Python version (two `sudo python -m …` subprocesses coordinating through an
//! on-disk `client-mac` file), this is a single binary running both servers as
//! tokio tasks — no inter-process file handshake needed.
//!
//! Delivery on macOS uses a raw-frame ([`bpf`]) send path so TFTP reaches the
//! Pi bootloader on macOS 15+ (where the kernel misdelivers despite a static
//! ARP entry). The DHCP handler learns the client MAC and hands it to TFTP
//! in-process — no `client-mac` file. On Linux (and when BPF is unavailable)
//! TFTP uses ordinary `send_to`, matching the Python behavior.
//!
//! Every listen socket is pinned to the one netboot interface (`--interface`,
//! required — see [`pin`]) before anything is served, so the DHCP server and
//! the file servers are unreachable from any other interface on the host.
//! Startup is: validate → bind and pin all listeners → drop root (Linux, see
//! [`privdrop`]) → serve. Privileged ports 67/69 still require root or
//! `CAP_NET_BIND_SERVICE` on Linux, which is why `paniolo netboot start`
//! spawns netbootd through `sudo` there; the daemon gives that root back as
//! soon as the sockets are bound. The BPF path on macOS needs no privilege in
//! the daemon at all (the setuid helper opens the descriptor).

mod bpf;
mod dhcp;
mod http;
mod netcfg;
mod pin;
mod privdrop;
mod served;
mod tftp;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "netbootd", version, about)]
struct Cli {
    /// Interface IP — assigned to the netboot interface and advertised as the
    /// TFTP/HTTP server (DHCP option 66 / siaddr) and router. The client's
    /// lease is derived from it unless `--client-ip` says otherwise.
    #[arg(long)]
    host_ip: Ipv4Addr,

    /// IP leased to the (single) netboot client. Must be in the same /24 as
    /// `--host-ip`: the lease carries a 255.255.255.0 mask and `--host-ip` as
    /// router and boot server. Defaults to the host's /24 with a last octet of
    /// 100 (101 when the host itself is .100).
    #[arg(long)]
    client_ip: Option<Ipv4Addr>,

    /// TFTP root directory (must exist).
    #[arg(long)]
    tftp_root: PathBuf,

    /// Bootfile advertised in DHCP option 67.
    #[arg(long, default_value = "kernel_2712.img")]
    boot_file: String,

    /// Interface device name (e.g. en11 / eth0) — the dedicated netboot link.
    /// Every listen socket (DHCP, TFTP, HTTP) is pinned to it, so nothing is
    /// served on any other interface; it is also where the ARP pin and the IP
    /// monitor act. Required; must not carry the system default route.
    /// On macOS, only one netbootd may own an interface at a time; separate
    /// interfaces can serve concurrently using the same ports.
    #[arg(long)]
    interface: String,

    #[arg(long, default_value_t = 67)]
    dhcp_port: u16,

    #[arg(long, default_value_t = 69)]
    tftp_port: u16,

    /// HTTP server port, also embedded in the UEFI HTTP Boot URL advertised in
    /// DHCP option 67. Defaults to 80 (omitted from the URL); choose an
    /// unprivileged high port to avoid needing root for the bind. `0` asks
    /// the OS for an ephemeral port — DHCP advertises whatever port the bind
    /// actually returns, read back from the listener, never the literal `0`
    /// that was requested. If the port cannot be bound, netbootd logs a
    /// warning and runs without HTTP Boot: DHCP + TFTP (and PXE) keep
    /// working, but an `HTTPClient` DHCP request gets no offer at all,
    /// rather than one pointing at a server that isn't there.
    #[arg(long, default_value_t = 80)]
    http_port: u16,

    /// `Content-Type` for HTTP responses. UEFI HTTP Boot treats
    /// `application/octet-stream` as an EFI application (the default).
    #[arg(long)]
    content_type: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).context("install tracing subscriber")?;

    let cli = Cli::parse();

    if !cli.tftp_root.is_dir() {
        anyhow::bail!("TFTP root {} does not exist", cli.tftp_root.display());
    }

    // The one lease we hand out has to be reachable from the host IP, or the
    // client boots into nothing. Derive it, or check the override.
    let client_ip = cli
        .client_ip
        .unwrap_or_else(|| dhcp::derive_client_ip(cli.host_ip));
    dhcp::validate_client_ip(cli.host_ip, client_ip).context("--client-ip")?;

    // Refuse to run on a primary NIC: netbootd reconfigures its interface to the
    // static host IP, which would clobber the host's real networking. The
    // netboot link must be a dedicated secondary interface.
    if netcfg::is_primary_interface(&cli.interface) {
        anyhow::bail!(
            "refusing to run on {}: it carries the system default route \
             (a primary NIC). netboot would force {} onto it and break host \
             networking. Use a dedicated USB-Ethernet adapter.",
            cli.interface,
            cli.host_ip
        );
    }

    info!(
        host_ip = %cli.host_ip,
        %client_ip,
        tftp_root = %cli.tftp_root.display(),
        boot_file = cli.boot_file,
        interface = cli.interface,
        "netbootd starting"
    );

    // Bind and pin every listener up front, before anything runs concurrently
    // and while we still hold whatever privilege the low ports and the pin
    // need. A DHCP or TFTP socket that cannot be bound or pinned is fatal.
    let listener_interface =
        pin::ListenerInterface::acquire(&cli.interface).with_context(|| {
            format!(
                "claim netboot interface {} (another netbootd may own it)",
                cli.interface
            )
        })?;
    let listener_interface = Arc::new(listener_interface);
    let dhcp_sock = dhcp::bind_server(cli.dhcp_port, &listener_interface)?;
    let tftp_sock = tftp::bind_server(cli.tftp_port, &listener_interface)?;
    // HTTP is the one optional transport: a port-80 clash must not take DHCP
    // and TFTP (the Pi and PXE paths) down with it.
    let http_listener = match http::bind_server(cli.http_port, &listener_interface) {
        Ok(l) => Some(l),
        Err(e) => {
            warn!(
                "HTTP listener on port {} unavailable ({e:#}); continuing with DHCP + TFTP \
                 only. HTTP Boot is unavailable: an HTTPClient DHCP request gets no offer \
                 at all (rate-limited warning in the DHCP log), rather than one pointing at \
                 a server that isn't there. Free the port or set --http-port to an unused \
                 one.",
                cli.http_port
            );
            None
        }
    };
    // DHCP must advertise the port actually bound, not the one requested:
    // `--http-port 0` asks the OS for an ephemeral port, and only the
    // listener knows what it got back. `None` (no listener at all) is what
    // tells `dhcp::serve` to stop building `http://` URLs entirely (#144).
    let bound_http_port = http_endpoint(http_listener.as_ref());

    // Raw-frame sender: on macOS the bound /dev/bpf descriptor is obtained from
    // the setuid-root helper via SCM_RIGHTS (netbootd itself stays unprivileged).
    // Linux uses the kernel send path, matching the Python servers. Constructed
    // unconditionally as a type, so the TFTP call sites stay compiled and checked
    // on every platform.
    let bpf = Arc::new(build_bpf_sender(&cli.interface));

    // Everything that needed root is done: give it back (Linux; a no-op where
    // the daemon was never privileged).
    privdrop::drop_privileges()?;

    // Interface-IP enforcement (matches _dhcp.py's monitor thread).
    {
        let iface = cli.interface.clone();
        let host_ip = cli.host_ip;
        tokio::spawn(async move { netcfg::monitor_interface(iface, host_ip).await });
    }

    // In-process DHCP→TFTP client-MAC handoff (replaces the on-disk file).
    let (mac_tx, mac_rx) = tokio::sync::watch::channel::<Option<[u8; 6]>>(None);

    let dhcp = spawn_listener_task(
        listener_interface.clone(),
        dhcp::serve(
            dhcp_sock,
            cli.host_ip,
            client_ip,
            cli.boot_file.clone(),
            cli.interface.clone(),
            bound_http_port,
            mac_tx,
        ),
    );
    let tftp = spawn_listener_task(
        listener_interface.clone(),
        tftp::serve(
            tftp_sock,
            cli.host_ip,
            client_ip,
            cli.tftp_root.clone(),
            cli.interface.clone(),
            bpf,
            mac_rx,
        ),
    );
    // HTTP serves UEFI HTTP Boot clients over ordinary kernel TCP — no BPF, no
    // ARP pin (a UEFI client answers ARP, unlike the silent Pi). The client
    // picks TFTP vs HTTP by how it DHCPs. Without a listener the task simply
    // never completes, so the select below is unchanged.
    let http = match http_listener {
        Some(listener) => spawn_listener_task(
            listener_interface.clone(),
            http::serve(listener, cli.tftp_root.clone(), cli.content_type.clone()),
        ),
        None => tokio::spawn(std::future::pending()),
    };

    // Any server task exiting (always an error — they loop forever) or a
    // shutdown signal brings the whole daemon down.
    tokio::select! {
        r = dhcp => match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => { error!("DHCP server failed: {e:#}"); return Err(e); }
            Err(e) => return Err(e).context("DHCP task panicked"),
        },
        r = tftp => match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => { error!("TFTP server failed: {e:#}"); return Err(e); }
            Err(e) => return Err(e).context("TFTP task panicked"),
        },
        r = http => match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => { error!("HTTP server failed: {e:#}"); return Err(e); }
            Err(e) => return Err(e).context("HTTP task panicked"),
        },
        sig = shutdown_signal() => {
            info!("netbootd shutting down ({sig})");
        }
    }
    Ok(())
}

/// A spawned task can outlive async main during runtime teardown. Keep its
/// interface claim alive until its listener has been dropped as well.
fn spawn_listener_task(
    interface: Arc<pin::ListenerInterface>,
    server: impl std::future::Future<Output = Result<()>> + Send + 'static,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let _interface = interface;
        server.await
    })
}

/// Resolve when the daemon is asked to stop: Ctrl-C (SIGINT) or, on Unix,
/// SIGTERM — which is what `paniolo netboot stop` sends. Yields the name of
/// the signal for the log.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            }
            Err(e) => {
                warn!("cannot listen for SIGTERM ({e}); only Ctrl-C shuts down cleanly");
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// Construct the raw-frame sender. On macOS, request a bound `/dev/bpf`
/// descriptor from the privileged helper and pair it with the interface's MAC
/// (read here, unprivileged). Any failure — helper missing, helper error — is
/// non-fatal: we log it and return an inert sender so TFTP falls back to the
/// kernel `send_to` path, exactly as on Linux.
fn build_bpf_sender(interface: &str) -> bpf::BpfSender {
    #[cfg(target_os = "macos")]
    {
        let Some(src_mac) = mac_of(interface) else {
            error!("no MAC address for {interface}; BPF disabled, using kernel send_to");
            return bpf::BpfSender::unavailable();
        };
        match netbootd::handoff::request_bpf_fd(interface) {
            Ok(fd) => bpf::BpfSender::from_handoff(fd, src_mac),
            Err(e) => {
                error!(
                    "BPF handoff failed ({e}); falling back to kernel send_to. \
                     Install the privileged helper with `paniolo setup`."
                );
                bpf::BpfSender::unavailable()
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = interface;
        bpf::BpfSender::unavailable()
    }
}

/// Read an interface's hardware (MAC) address. Unprivileged — no `/dev/bpf`
/// involved.
#[cfg(target_os = "macos")]
fn mac_of(iface: &str) -> Option<[u8; 6]> {
    pnet_datalink::interfaces()
        .into_iter()
        .find(|i| i.name == iface)
        .and_then(|i| i.mac)
        .map(|m| [m.0, m.1, m.2, m.3, m.4, m.5])
}

/// The port DHCP should advertise for HTTP, read back from the listener that
/// was actually bound rather than trusted from the command line.
///
/// `None` means no HTTP listener exists at all (the bind failed), which is
/// what tells `dhcp::serve` to stop building `http://` URLs for `HTTPClient`
/// requests entirely. `Some(port)` is the real, OS-confirmed port — for
/// `--http-port 0` this is the ephemeral port the OS actually assigned, never
/// the literal `0` that was requested (issue #144).
fn http_endpoint(listener: Option<&tokio::net::TcpListener>) -> Option<u16> {
    let listener = listener?;
    match listener.local_addr() {
        Ok(addr) => Some(addr.port()),
        Err(e) => {
            warn!("HTTP listener has no local address ({e:#}); treating HTTP as unavailable");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_endpoint_is_none_without_a_listener() {
        assert_eq!(http_endpoint(None), None);
    }

    /// The bind-to-port-0 plumbing this whole fix depends on: an ephemeral
    /// bind hands back a real port from the OS, and `http_endpoint` must
    /// report that real port — never `0`, and never something read from the
    /// original (unbound) request.
    #[tokio::test]
    async fn http_endpoint_reads_back_the_os_assigned_ephemeral_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let bound_port = listener
            .local_addr()
            .expect("bound listener has a local address")
            .port();
        assert_ne!(bound_port, 0, "the OS must have assigned a real port");

        assert_eq!(http_endpoint(Some(&listener)), Some(bound_port));
    }
}
