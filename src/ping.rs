//! ICMP echo probing over a raw socket.
//!
//! Entware daemons run as root, so a raw socket is available and no external
//! `ping` binary is involved: the monitor measures RTT and timeouts itself.

use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_HEADER_LEN: usize = 8;
const IP_HEADER_MIN: usize = 20;
const PAYLOAD_LEN: usize = 8;
const PACKET_LEN: usize = ICMP_HEADER_LEN + PAYLOAD_LEN;

#[derive(Debug)]
pub enum PingOutcome {
    Reply(Duration),
    Timeout,
    Error(io::Error),
}

pub struct Pinger {
    fd: RawFd,
    ident: u16,
    sequence: u16,
}

// The raw descriptors are only used from the monitor thread.
unsafe impl Send for Pinger {}

impl Pinger {
    pub fn new() -> io::Result<Pinger> {
        let fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::IPPROTO_ICMP,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pinger {
            fd,
            ident: (std::process::id() & 0xffff) as u16,
            sequence: 0,
        })
    }

    pub fn ping(&mut self, target: Ipv4Addr, timeout: Duration) -> PingOutcome {
        if timeout.is_zero() {
            return PingOutcome::Timeout;
        }
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;
        let started = Instant::now();
        let mut packet = [0u8; PACKET_LEN];
        packet[0] = ICMP_ECHO_REQUEST;
        packet[4..6].copy_from_slice(&self.ident.to_be_bytes());
        packet[6..8].copy_from_slice(&sequence.to_be_bytes());
        let sent_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        packet[ICMP_HEADER_LEN..].copy_from_slice(&sent_at.to_be_bytes());
        let checksum = checksum(&packet);
        packet[2..4].copy_from_slice(&checksum.to_be_bytes());

        let address = sockaddr_in(target, 0);
        let sent = unsafe {
            libc::sendto(
                self.fd,
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&address as *const libc::sockaddr_in).cast(),
                mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return PingOutcome::Error(io::Error::last_os_error());
        }
        if sent as usize != packet.len() {
            return PingOutcome::Error(io::Error::other("short ICMP send"));
        }

        let deadline = started + timeout;
        let mut buffer = [0u8; 512];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return PingOutcome::Timeout;
            }
            let remaining = deadline - now;
            if let Err(err) = self.set_read_timeout(remaining) {
                return PingOutcome::Error(err);
            }
            let mut source: libc::sockaddr_in = unsafe { mem::zeroed() };
            let mut source_len = mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let received = unsafe {
                libc::recvfrom(
                    self.fd,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                    (&mut source as *mut libc::sockaddr_in).cast(),
                    &mut source_len,
                )
            };
            if received < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                        return PingOutcome::Timeout;
                    }
                    io::ErrorKind::Interrupted => continue,
                    _ => return PingOutcome::Error(err),
                }
            }
            if let Some(reply) = parse_reply(&buffer[..received as usize], self.ident, sequence) {
                if reply.source != target {
                    continue;
                }
                return PingOutcome::Reply(started.elapsed());
            }
        }
    }

    fn set_read_timeout(&self, remaining: Duration) -> io::Result<()> {
        // A zero timeval means "block forever" on Linux, so round tiny
        // remainders up to one microsecond.
        let micros = remaining.as_micros().max(1).min(libc::c_long::MAX as u128);
        let timeval = libc::timeval {
            tv_sec: (micros / 1_000_000) as _,
            tv_usec: (micros % 1_000_000) as _,
        };
        let result = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&timeval as *const libc::timeval).cast(),
                mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for Pinger {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

struct ReplyInfo {
    source: Ipv4Addr,
}

fn parse_reply(packet: &[u8], expected_ident: u16, expected_sequence: u16) -> Option<ReplyInfo> {
    if packet.len() < IP_HEADER_MIN {
        return None;
    }
    let version_ihl = packet[0];
    if version_ihl >> 4 != 4 {
        return None;
    }
    let header_len = ((version_ihl & 0x0f) as usize) * 4;
    if header_len < IP_HEADER_MIN || packet.len() < header_len + ICMP_HEADER_LEN {
        return None;
    }
    let icmp = &packet[header_len..];
    if icmp[0] != ICMP_ECHO_REPLY {
        return None;
    }
    if u16::from_be_bytes([icmp[4], icmp[5]]) != expected_ident
        || u16::from_be_bytes([icmp[6], icmp[7]]) != expected_sequence
    {
        return None;
    }
    Some(ReplyInfo {
        source: Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]),
    })
}

fn sockaddr_in(ip: Ipv4Addr, port: u16) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(ip.octets()),
        },
        sin_zero: [0; 8],
    }
}

/// Standard Internet checksum (RFC 1071) used for the ICMP header + payload.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4_header(source: [u8; 4], payload_len: usize) -> Vec<u8> {
        let total_len = (IP_HEADER_MIN + payload_len) as u16;
        let mut header = vec![0u8; IP_HEADER_MIN];
        header[0] = 0x45;
        header[2..4].copy_from_slice(&total_len.to_be_bytes());
        header[9] = ICMP_ECHO_REPLY;
        header[12..16].copy_from_slice(&source);
        header
    }

    #[test]
    fn checksum_of_zeroed_packet_is_invertible() {
        let mut packet = [0u8; PACKET_LEN];
        packet[0] = ICMP_ECHO_REQUEST;
        packet[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
        packet[6..8].copy_from_slice(&7u16.to_be_bytes());
        let sum = checksum(&packet);
        packet[2..4].copy_from_slice(&sum.to_be_bytes());
        // Re-summing a valid packet (checksum included) yields 0xffff / 0.
        assert_eq!(checksum(&packet), 0);
    }

    #[test]
    fn parses_matching_echo_reply() {
        let icmp = {
            let mut icmp = [0u8; ICMP_HEADER_LEN + 4];
            icmp[0] = ICMP_ECHO_REPLY;
            icmp[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
            icmp[6..8].copy_from_slice(&7u16.to_be_bytes());
            icmp.to_vec()
        };
        let mut packet = ipv4_header([192, 168, 1, 10], icmp.len());
        packet.extend_from_slice(&icmp);
        let reply = parse_reply(&packet, 0x1234, 7).expect("reply must parse");
        assert_eq!(reply.source, Ipv4Addr::new(192, 168, 1, 10));
    }

    #[test]
    fn ignores_wrong_sequence_and_request_packets() {
        let mut icmp = [0u8; ICMP_HEADER_LEN];
        icmp[0] = ICMP_ECHO_REPLY;
        icmp[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
        icmp[6..8].copy_from_slice(&8u16.to_be_bytes());
        let mut packet = ipv4_header([192, 168, 1, 10], icmp.len());
        packet.extend_from_slice(&icmp);
        assert!(parse_reply(&packet, 0x1234, 7).is_none());

        icmp[0] = ICMP_ECHO_REQUEST;
        let mut request = ipv4_header([192, 168, 1, 10], icmp.len());
        request.extend_from_slice(&icmp);
        assert!(parse_reply(&request, 0x1234, 8).is_none());
    }

    #[test]
    fn ignores_short_packets() {
        assert!(parse_reply(&[0x45, 0, 0], 1, 1).is_none());
        assert!(parse_reply(&[], 1, 1).is_none());
    }
}
