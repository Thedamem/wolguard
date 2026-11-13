//! Wake-on-LAN magic packet construction and delivery.

use std::fmt;
use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::RawFd;

use crate::net;

/// 6 x 0xFF followed by the target MAC address repeated 16 times.
pub fn build_magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac);
    }
    packet
}

/// Where the magic packet for a host is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeTarget {
    pub address: Ipv4Addr,
    pub iface: Option<String>,
    /// Directed broadcast derived from the routing table (false) or an
    /// address explicitly configured by the user (true).
    pub configured: bool,
}

impl fmt::Display for WakeTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.iface {
            Some(iface) => write!(formatter, "{} via {iface}", self.address),
            None => write!(formatter, "{}", self.address),
        }
    }
}

/// Resolve the wake target for `host_ip`. `configured` may be `None`/`"auto"`
/// (derive from the kernel routing table) or an explicit IPv4 broadcast address.
pub fn resolve_wake_target(host_ip: Ipv4Addr, configured: Option<&str>) -> WakeTarget {
    if let Some(text) = configured {
        let text = text.trim();
        if !text.is_empty() && text != "auto" {
            if let Ok(address) = text.parse::<Ipv4Addr>() {
                return WakeTarget {
                    address,
                    iface: None,
                    configured: true,
                };
            }
        }
    }
    match net::lan_route(host_ip) {
        Some(route) => WakeTarget {
            address: route.broadcast,
            iface: Some(route.iface),
            configured: false,
        },
        None => WakeTarget {
            address: Ipv4Addr::BROADCAST,
            iface: None,
            configured: false,
        },
    }
}

/// Send one magic packet to `target` on `port`.
pub fn send_magic(
    mac: [u8; 6],
    target: Ipv4Addr,
    iface: Option<&str>,
    port: u16,
) -> io::Result<()> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = send_magic_fd(fd, mac, target, iface, port);
    unsafe {
        libc::close(fd);
    }
    result
}

fn send_magic_fd(
    fd: RawFd,
    mac: [u8; 6],
    target: Ipv4Addr,
    iface: Option<&str>,
    port: u16,
) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BROADCAST,
            (&enabled as *const libc::c_int).cast(),
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    if let Some(iface) = iface {
        let mut name = iface.as_bytes().to_vec();
        name.push(0);
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_ptr().cast(),
                name.len() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!(
                    "cannot bind Wake-on-LAN socket to {iface}: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
    }

    let packet = build_magic_packet(mac);
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(target.octets()),
        },
        sin_zero: [0; 8],
    };
    let sent = unsafe {
        libc::sendto(
            fd,
            packet.as_ptr().cast(),
            packet.len(),
            0,
            (&address as *const libc::sockaddr_in).cast(),
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != packet.len() {
        return Err(io::Error::other("short Wake-on-LAN send"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let packet = build_magic_packet(mac);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xff; 6]);
        for chunk in packet[6..].chunks_exact(6) {
            assert_eq!(chunk, &mac);
        }
    }

    #[test]
    fn configured_broadcast_is_used_verbatim() {
        let target = resolve_wake_target(Ipv4Addr::new(192, 168, 1, 10), Some("192.168.1.255"));
        assert_eq!(target.address, Ipv4Addr::new(192, 168, 1, 255));
        assert!(target.configured);
        assert_eq!(target.iface, None);
    }

    #[test]
    fn auto_broadcast_is_not_multicast() {
        let target = resolve_wake_target(Ipv4Addr::new(192, 168, 1, 10), Some("auto"));
        assert!(!target.address.is_multicast());
        assert!(!target.address.is_unspecified());
    }
}
