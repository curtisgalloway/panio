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

//! Pin a socket to one interface — the netboot link.
//!
//! netbootd answers every DHCP DISCOVER it hears with a lease and hands out
//! whatever is in its root directory to anyone who asks. Its listen sockets
//! bind the wildcard address (so they work through the link flaps where the
//! interface IP is momentarily gone), which without a pin would make the same
//! sockets reachable on the host's primary NIC: a rogue DHCP server on the
//! office LAN and a file server anyone there can read. So every listen socket
//! — DHCP, TFTP and HTTP — is pinned to the netboot interface before it is
//! bound, and a pin that cannot be applied is fatal: the daemon refuses to
//! start rather than serve unpinned. Reply sockets are pinned the same way so
//! a limited-broadcast DHCP reply or a TFTP DATA block leaves via the netboot
//! link and not the default-route interface.
//!
//! * **macOS:** `IP_BOUND_IF`, which applies to IPv4 sockets (the HTTP
//!   listener stays IPv4 for that reason). No privilege needed.
//! * **Linux:** `SO_BINDTODEVICE`. Kernels before 5.7 require `CAP_NET_RAW`
//!   to set it, so the listen sockets are pinned while the daemon still holds
//!   root — before the privilege drop (see `privdrop`).
//! * **Elsewhere:** there is no per-socket interface binding, so the pin is a
//!   loudly logged no-op; netboot is not a supported feature on those hosts.

use socket2::Socket;

/// Pin `sock` so it only receives on, and only sends via, `iface`.
#[cfg(target_os = "macos")]
pub fn pin_socket_to_interface(sock: &Socket, iface: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let cname = std::ffi::CString::new(iface)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "iface has NUL"))?;
    let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if idx == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let idx: libc::c_uint = idx;
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_BOUND_IF,
            &idx as *const libc::c_uint as *const libc::c_void,
            std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Pin `sock` so it only receives on, and only sends via, `iface`.
#[cfg(target_os = "linux")]
pub fn pin_socket_to_interface(sock: &Socket, iface: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            iface.as_ptr() as *const libc::c_void,
            iface.len() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// No per-socket interface binding exists on this platform: log it and carry
/// on unpinned. Netboot is not a supported feature here, so this is a
/// development convenience, not a silent hole on a bench host.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn pin_socket_to_interface(_sock: &Socket, iface: &str) -> std::io::Result<()> {
    tracing::warn!(
        "this platform cannot pin a socket to {iface}; the socket is reachable on every interface"
    );
    Ok(())
}

/// Own the macOS interface lock for the entire daemon lifetime. Linux keeps
/// relying on SO_BINDTODEVICE's exclusive per-interface bind namespace.
pub struct ListenerInterface {
    name: String,
    #[cfg(target_os = "macos")]
    _lock: std::fs::File,
}

impl ListenerInterface {
    pub fn acquire(name: &str) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        let lock = {
            let cname = std::ffi::CString::new(name)?;
            let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
            if index == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // A fixed, system-wide path: neither UID nor TMPDIR may split the
            // lock namespace. Never unlink it, even at shutdown: a waiter may
            // already have the inode open. flock releases on exit/crash.
            lock_interface_file(std::path::Path::new(&format!(
                "/tmp/panio-netboot-interface-{index}.lock"
            )))?
        };
        Ok(Self {
            name: name.to_owned(),
            #[cfg(target_os = "macos")]
            _lock: lock,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Only listeners with a held interface lock may opt into macOS port
    /// sharing. Reply sockets retain their existing options.
    pub fn pin_listener(&self, sock: &Socket) -> std::io::Result<()> {
        pin_socket_to_interface(sock, &self.name)?;
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let enabled: libc::c_int = 1;
            let rc = unsafe {
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_REUSEPORT,
                    &enabled as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&enabled) as libc::socklen_t,
                )
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn lock_interface_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    // Existing files need only be readable, so another user cannot bypass the
    // lock. Restrictive permissions fail closed. Do not follow a symlink or
    // block on a FIFO planted in the shared temporary directory.
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)?,
        Err(e) => return Err(e),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::other("invalid netboot interface lock file"));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::net::{TcpStream, UdpSocket};
    use tokio::time::timeout;

    // Only high, OS-assigned ports; no DHCP handler, ARP, interface changes,
    // BPF helper, or traffic to a physical target. Exercise the real listener
    // constructors on two existing interfaces and send only via loopback.
    #[tokio::test]
    async fn listeners_share_ports_across_interfaces_and_isolate_ingress() {
        let interfaces = pnet_datalink::interfaces();
        let loopback = interfaces.iter().find(|i| i.is_loopback()).unwrap();
        let other = interfaces
            .iter()
            .find(|i| !i.is_loopback())
            .expect("socket regression needs loopback and one other interface (CI provides both)");
        let first = ListenerInterface::acquire(&loopback.name).unwrap();
        let second = ListenerInterface::acquire(&other.name).unwrap();

        #[cfg(target_os = "macos")]
        assert!(ListenerInterface::acquire(&loopback.name).is_err());

        for bind in [crate::dhcp::bind_server, crate::tftp::bind_server] {
            let a = bind(0, &first).unwrap();
            let port = a.local_addr().unwrap().port();
            let b = bind(port, &second).unwrap();
            #[cfg(target_os = "linux")]
            assert!(bind(port, &first).is_err(), "same-interface UDP duplicate");
            let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            sender
                .send_to(b"probe", (Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            let mut buf = [0; 16];
            let (n, _) = timeout(Duration::from_secs(2), a.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], b"probe");
            assert!(timeout(Duration::from_millis(50), b.recv_from(&mut buf))
                .await
                .is_err());
            drop(a);
            // Restart on one interface while the other is still listening.
            let _restarted = bind(port, &first).unwrap();
        }

        let a = crate::http::bind_server(0, &first).unwrap();
        let port = a.local_addr().unwrap().port();
        let b = crate::http::bind_server(port, &second).unwrap();
        #[cfg(target_os = "linux")]
        assert!(
            crate::http::bind_server(port, &first).is_err(),
            "same-interface TCP duplicate"
        );
        let _client = timeout(
            Duration::from_secs(2),
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
        )
        .await
        .unwrap()
        .unwrap();
        let _accepted = timeout(Duration::from_secs(2), a.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(timeout(Duration::from_millis(50), b.accept())
            .await
            .is_err());
        drop(a);
        let _restarted = crate::http::bind_server(port, &first).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn interface_lock_rejects_duplicates_and_releases_on_close() {
        let path = std::env::temp_dir().join(format!("netbootd-lock-test-{}", std::process::id()));
        let first = lock_interface_file(&path).unwrap();
        let error = lock_interface_file(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        let second = lock_interface_file(&path).unwrap();
        drop(second);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(lock_interface_file(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
