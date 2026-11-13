//! Kernel routing helpers.
//!
//! On a router the default route points to the WAN interface, so sending a
//! magic packet to 255.255.255.255 may leave through the uplink. The monitor
//! therefore derives the directed broadcast address of the LAN that actually
//! serves the target host and binds the magic packet to that interface.

use std::io;
use std::mem;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    pub iface: String,
    pub destination: Ipv4Addr,
    pub mask: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanRoute {
    pub iface: String,
    pub broadcast: Ipv4Addr,
    pub prefix_len: u8,
}

pub fn read_routes() -> io::Result<Vec<RouteEntry>> {
    let text = std::fs::read_to_string("/proc/net/route")?;
    Ok(parse_routes(&text))
}

fn parse_routes(text: &str) -> Vec<RouteEntry> {
    let mut routes = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let iface = fields[0];
        let flags = u16::from_str_radix(fields[3], 16).unwrap_or(0);
        if flags & 0x0001 == 0 {
            // RTF_UP is required
            continue;
        }
        let Some(destination) = parse_hex_addr(fields[1]) else {
            continue;
        };
        let Some(mask) = parse_hex_addr(fields[7]) else {
            continue;
        };
        routes.push(RouteEntry {
            iface: iface.to_string(),
            destination,
            mask,
        });
    }
    routes
}

/// /proc/net/route prints the u32 address as it is stored in memory
/// (little-endian on MIPS and x86), so the hex value is byte-swapped.
fn parse_hex_addr(hex: &str) -> Option<Ipv4Addr> {
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(value.swap_bytes()))
}

/// Longest-prefix match among non-default routes.
fn longest_match(dest: Ipv4Addr, routes: &[RouteEntry]) -> Option<(u8, &RouteEntry)> {
    let dest_bits = u32::from(dest);
    let mut best: Option<(u8, &RouteEntry)> = None;
    for route in routes {
        let mask = u32::from(route.mask);
        let prefix = mask.count_ones() as u8;
        if prefix == 0 || prefix > 32 {
            continue;
        }
        if dest_bits & mask != u32::from(route.destination) & mask {
            continue;
        }
        if best
            .map(|(best_prefix, _)| prefix > best_prefix)
            .unwrap_or(true)
        {
            best = Some((prefix, route));
        }
    }
    best
}

/// Find the LAN interface and directed broadcast address that serves `dest`.
pub fn lan_route(dest: Ipv4Addr) -> Option<LanRoute> {
    let routes = read_routes().ok()?;
    let (prefix_len, route) = longest_match(dest, &routes)?;
    let netmask = iface_netmask(&route.iface).unwrap_or(route.mask);
    let broadcast = broadcast_address(dest, netmask);
    if broadcast.is_unspecified() || broadcast.is_multicast() {
        return None;
    }
    Some(LanRoute {
        iface: route.iface.clone(),
        broadcast,
        prefix_len,
    })
}

/// `(ip & netmask) | !netmask`
pub fn broadcast_address(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    let mask = u32::from(netmask);
    Ipv4Addr::from((u32::from(ip) & mask) | !mask)
}

fn iface_netmask(iface: &str) -> Option<Ipv4Addr> {
    if iface.is_empty() || iface.len() >= libc::IFNAMSIZ {
        return None;
    }
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    let mut request: libc::ifreq = unsafe { mem::zeroed() };
    for (slot, byte) in request.ifr_name.iter_mut().zip(iface.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    // libc::Ioctl is c_ulong on linux-gnu and c_int on linux-musl (the router).
    let result = unsafe { libc::ioctl(fd, libc::SIOCGIFNETMASK as libc::Ioctl, &mut request) };
    unsafe {
        libc::close(fd);
    }
    if result < 0 {
        return None;
    }
    let netmask: libc::sockaddr = unsafe { request.ifr_ifru.ifru_netmask };
    let sockaddr_in = unsafe { &*(&netmask as *const libc::sockaddr as *const libc::sockaddr_in) };
    // sin_addr.s_addr keeps the address in network byte order in memory.
    Some(Ipv4Addr::from(sockaddr_in.sin_addr.s_addr.to_ne_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
br0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
eth3\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0
";

    #[test]
    fn parses_proc_net_route() {
        let routes = parse_routes(ROUTES);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].iface, "br0");
        assert_eq!(routes[0].destination, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(routes[0].mask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(routes[1].destination, Ipv4Addr::new(0, 0, 0, 0));
    }

    #[test]
    fn selects_lan_route_not_default_route() {
        let routes = parse_routes(ROUTES);
        let (prefix, route) =
            longest_match(Ipv4Addr::new(192, 168, 1, 10), &routes).expect("LAN route must match");
        assert_eq!(prefix, 24);
        assert_eq!(route.iface, "br0");
        assert!(longest_match(Ipv4Addr::new(8, 8, 8, 8), &routes).is_none());
    }

    #[test]
    fn computes_directed_broadcast() {
        assert_eq!(
            broadcast_address(
                Ipv4Addr::new(192, 168, 1, 10),
                Ipv4Addr::new(255, 255, 255, 0)
            ),
            Ipv4Addr::new(192, 168, 1, 255)
        );
        assert_eq!(
            broadcast_address(Ipv4Addr::new(10, 0, 5, 4), Ipv4Addr::new(255, 255, 254, 0)),
            Ipv4Addr::new(10, 0, 5, 255)
        );
    }

    #[test]
    fn real_route_table_is_readable_on_linux() {
        // The dev host has /proc/net/route; just make sure parsing does not panic.
        let _ = read_routes();
    }
}
