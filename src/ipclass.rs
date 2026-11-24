//! IPv4 classification: is a PPPoE address "white" (globally routable) or
//! "grey" (private, CGNAT, link-local, documentation, etc.)?
//!
//! The built-in table follows the IANA special-purpose address registry. Users
//! can add explicit allow/deny CIDRs; an explicit deny always wins, and an
//! explicit allow can override the built-in table.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IpClass {
    White,
    Grey,
}

impl IpClass {
    pub fn label(self) -> &'static str {
        match self {
            IpClass::White => "white",
            IpClass::Grey => "grey",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: u32,
    mask: u32,
    prefix: u8,
    text: String,
}

impl Cidr {
    pub fn parse(input: &str) -> Result<Cidr, String> {
        let text = input.trim();
        let (address, prefix) = text
            .split_once('/')
            .ok_or_else(|| format!("invalid CIDR {text:?}: expected a.b.c.d/len"))?;
        let address: Ipv4Addr = address
            .parse()
            .map_err(|_| format!("invalid CIDR {text:?}: bad IPv4 address"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("invalid CIDR {text:?}: bad prefix length"))?;
        if prefix > 32 {
            return Err(format!(
                "invalid CIDR {text:?}: prefix length must be 0..=32"
            ));
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Ok(Cidr {
            network: u32::from(address) & mask,
            mask,
            prefix,
            text: format!("{}/{}", Ipv4Addr::from(u32::from(address) & mask), prefix),
        })
    }

    pub fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask == self.network
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Debug)]
pub struct Classification {
    pub class: IpClass,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct Classifier {
    white: Vec<Cidr>,
    grey: Vec<Cidr>,
}

impl Classifier {
    pub fn new(white: &[String], grey: &[String]) -> Result<Classifier, String> {
        let mut parsed_white = Vec::with_capacity(white.len());
        for entry in white {
            if entry.trim().is_empty() {
                continue;
            }
            parsed_white.push(Cidr::parse(entry)?);
        }
        let mut parsed_grey = Vec::with_capacity(grey.len());
        for entry in grey {
            if entry.trim().is_empty() {
                continue;
            }
            parsed_grey.push(Cidr::parse(entry)?);
        }
        Ok(Classifier {
            white: parsed_white,
            grey: parsed_grey,
        })
    }

    pub fn classify(&self, address: Ipv4Addr) -> Classification {
        if let Some(cidr) = self.grey.iter().find(|cidr| cidr.contains(address)) {
            return Classification {
                class: IpClass::Grey,
                reason: format!("matches grey_cidrs {}", cidr.text()),
            };
        }
        if let Some(cidr) = self.white.iter().find(|cidr| cidr.contains(address)) {
            return Classification {
                class: IpClass::White,
                reason: format!("matches white_cidrs {}", cidr.text()),
            };
        }
        match builtin_grey(address) {
            Some(reason) => Classification {
                class: IpClass::Grey,
                reason: reason.to_string(),
            },
            None => Classification {
                class: IpClass::White,
                reason: "public".to_string(),
            },
        }
    }
}

/// Built-in special-purpose ranges that are not globally routable.
fn builtin_grey(address: Ipv4Addr) -> Option<&'static str> {
    const RANGES: &[(u32, u32, &str)] = &[
        (0x0000_0000, 0xff00_0000, "this network 0.0.0.0/8"),
        (0x0a00_0000, 0xff00_0000, "private 10.0.0.0/8"),
        (0x6440_0000, 0xffc0_0000, "CGNAT 100.64.0.0/10"),
        (0x7f00_0000, 0xff00_0000, "loopback 127.0.0.0/8"),
        (0xa9fe_0000, 0xffff_0000, "link-local 169.254.0.0/16"),
        (0xac10_0000, 0xfff0_0000, "private 172.16.0.0/12"),
        (0xc000_0000, 0xffff_ff00, "IETF protocol 192.0.0.0/24"),
        (0xc000_0200, 0xffff_ff00, "documentation 192.0.2.0/24"),
        (0xc058_6300, 0xffff_ff00, "6to4 relay 192.88.99.0/24"),
        (0xc0a8_0000, 0xffff_0000, "private 192.168.0.0/16"),
        (0xc612_0000, 0xfffe_0000, "benchmark 198.18.0.0/15"),
        (0xc633_6400, 0xffff_ff00, "documentation 198.51.100.0/24"),
        (0xcb00_7100, 0xffff_ff00, "documentation 203.0.113.0/24"),
        (0xe000_0000, 0xf000_0000, "multicast 224.0.0.0/4"),
        (0xf000_0000, 0xf000_0000, "reserved 240.0.0.0/4"),
        (0xffff_ffff, 0xffff_ffff, "broadcast 255.255.255.255/32"),
    ];
    let bits = u32::from(address);
    RANGES
        .iter()
        .find(|(network, mask, _)| bits & *mask == *network)
        .map(|(_, _, reason)| *reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> Classifier {
        Classifier::new(&[], &[]).expect("default classifier")
    }

    fn class(address: &str) -> IpClass {
        classifier()
            .classify(address.parse().expect("test address"))
            .class
    }

    #[test]
    fn public_addresses_are_white() {
        for address in [
            "8.8.8.8",
            "1.1.1.1",
            "95.31.12.7",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
        ] {
            assert_eq!(class(address), IpClass::White, "{address}");
        }
    }

    #[test]
    fn special_purpose_addresses_are_grey() {
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.0",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.10",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.254",
            "255.255.255.255",
        ] {
            assert_eq!(class(address), IpClass::Grey, "{address}");
        }
    }

    #[test]
    fn explicit_grey_wins_even_for_public_ranges() {
        let classifier = Classifier::new(&[], &["8.8.8.0/24".to_string()]).unwrap();
        let result = classifier.classify("8.8.8.8".parse().unwrap());
        assert_eq!(result.class, IpClass::Grey);
        assert!(result.reason.contains("grey_cidrs"), "{}", result.reason);
        assert_eq!(
            classifier.classify("8.8.4.4".parse().unwrap()).class,
            IpClass::White
        );
    }

    #[test]
    fn explicit_white_can_override_the_builtin_table() {
        let classifier = Classifier::new(&["10.0.0.0/8".to_string()], &[]).unwrap();
        let result = classifier.classify("10.1.2.3".parse().unwrap());
        assert_eq!(result.class, IpClass::White);
        assert!(result.reason.contains("white_cidrs"), "{}", result.reason);
    }

    #[test]
    fn grey_wins_over_white_when_both_match() {
        let classifier =
            Classifier::new(&["10.0.0.0/8".to_string()], &["10.1.0.0/16".to_string()]).unwrap();
        assert_eq!(
            classifier.classify("10.1.2.3".parse().unwrap()).class,
            IpClass::Grey
        );
        assert_eq!(
            classifier.classify("10.2.2.3".parse().unwrap()).class,
            IpClass::White
        );
    }

    #[test]
    fn cidr_normalizes_host_bits_and_matches_boundaries() {
        let cidr = Cidr::parse("192.168.1.77/24").unwrap();
        assert_eq!(cidr.text(), "192.168.1.0/24");
        assert!(cidr.contains("192.168.1.255".parse().unwrap()));
        assert!(!cidr.contains("192.168.2.1".parse().unwrap()));

        let all = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(all.contains("255.255.255.255".parse().unwrap()));
        let host = Cidr::parse("8.8.8.8/32").unwrap();
        assert!(host.contains("8.8.8.8".parse().unwrap()));
        assert!(!host.contains("8.8.8.9".parse().unwrap()));
    }

    #[test]
    fn rejects_invalid_cidrs() {
        for text in [
            "192.168.1.1",
            "192.168.1.1/33",
            "192.168.1.1/",
            "192.168.1.999/24",
            "not-an-address/24",
        ] {
            assert!(Cidr::parse(text).is_err(), "{text}");
        }
    }
}
