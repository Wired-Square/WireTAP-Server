//! CIDR ranges, for the GVRET listener's `allowed_clients`.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// An IPv4 or IPv6 network, with no host bits set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub const LOOPBACK: [Cidr; 2] = [
        Cidr {
            network: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)),
            prefix: 8,
        },
        Cidr {
            network: IpAddr::V6(Ipv6Addr::LOCALHOST),
            prefix: 128,
        },
    ];

    /// An IPv4-mapped IPv6 peer, as a dual-stack listener sees an IPv4 one,
    /// is matched as its IPv4 address.
    pub fn contains(&self, peer: IpAddr) -> bool {
        let peer = peer.to_canonical();
        let ((peer_bits, width), (network, _)) = (bits(peer), bits(self.network));
        peer.is_ipv4() == self.network.is_ipv4()
            && network_bits(peer_bits, width, self.prefix) == network
    }
}

fn bits(addr: IpAddr) -> (u128, u8) {
    match addr {
        IpAddr::V4(a) => (a.to_bits().into(), 32),
        IpAddr::V6(a) => (a.to_bits(), 128),
    }
}

fn network_bits(bits: u128, width: u8, prefix: u8) -> u128 {
    let host = u32::from(width - prefix);
    bits.checked_shr(host).map_or(0, |b| b << host)
}

impl FromStr for Cidr {
    type Err = String;

    /// `10.0.0.0/8`, or a bare address for one host. Host bits set beyond the
    /// prefix are refused, since `10.1.2.3/8` may mean the host or the network.
    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, prefix) = s.split_once('/').map_or((s, None), |(a, p)| (a, Some(p)));
        let network: IpAddr = addr
            .parse()
            .map_err(|_| format!("{s:?} is not an IP address or CIDR range"))?;
        if matches!(network, IpAddr::V6(a) if a.to_ipv4_mapped().is_some()) {
            return Err(format!("{s:?} is IPv4-mapped; write it as IPv4"));
        }
        let (bits, width) = bits(network);
        let prefix = match prefix {
            None => width,
            Some(p) => p
                .parse()
                .ok()
                .filter(|&p| p <= width)
                .ok_or_else(|| format!("{s:?}: the prefix must be 0 to {width}"))?,
        };
        let masked = network_bits(bits, width, prefix);
        if masked != bits {
            let network = match network {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from_bits(masked as u32)),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from_bits(masked)),
            };
            return Err(format!(
                "{s:?} has host bits set; the network is {}",
                Cidr { network, prefix }
            ));
        }
        Ok(Self { network, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Cidr {
        s.parse().unwrap_or_else(|e| panic!("{e}"))
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_ipv4_range_holds_its_own_addresses_only() {
        let net = cidr("192.168.1.0/24");
        assert!(net.contains(ip("192.168.1.0")));
        assert!(net.contains(ip("192.168.1.255")));
        assert!(!net.contains(ip("192.168.2.1")));
        assert!(!net.contains(ip("192.168.0.255")));
        assert!(!net.contains(ip("::1")), "another family");
    }

    #[test]
    fn an_ipv6_range_holds_its_own_addresses_only() {
        let net = cidr("fd00:1:2::/48");
        assert!(net.contains(ip("fd00:1:2:ffff::1")));
        assert!(!net.contains(ip("fd00:1:3::1")));
        assert!(!net.contains(ip("10.0.0.1")), "another family");
    }

    #[test]
    fn a_zero_prefix_holds_its_whole_family() {
        assert!(cidr("0.0.0.0/0").contains(ip("203.0.113.9")));
        assert!(!cidr("0.0.0.0/0").contains(ip("2001:db8::1")));
        assert!(cidr("::/0").contains(ip("2001:db8::1")));
        assert!(!cidr("::/0").contains(ip("203.0.113.9")));
    }

    #[test]
    fn a_full_prefix_or_a_bare_address_is_one_host() {
        assert_eq!(cidr("10.0.0.1"), cidr("10.0.0.1/32"));
        assert_eq!(cidr("2001:db8::1"), cidr("2001:db8::1/128"));
        assert!(cidr("10.0.0.1/32").contains(ip("10.0.0.1")));
        assert!(!cidr("10.0.0.1/32").contains(ip("10.0.0.2")));
        assert!(cidr("2001:db8::1/128").contains(ip("2001:db8::1")));
        assert!(!cidr("2001:db8::1/128").contains(ip("2001:db8::2")));
    }

    #[test]
    fn an_ipv4_mapped_peer_is_matched_as_ipv4() {
        assert!(cidr("192.168.1.0/24").contains(ip("::ffff:192.168.1.7")));
        assert!(!cidr("192.168.1.0/24").contains(ip("::ffff:192.168.2.7")));
        assert!(Cidr::LOOPBACK
            .iter()
            .any(|n| n.contains(ip("::ffff:127.0.0.1"))));
        assert!(!cidr("::/0").contains(ip("::ffff:192.168.1.7")));
    }

    #[test]
    fn loopback_is_both_families_and_nothing_else() {
        let allowed = |s| Cidr::LOOPBACK.iter().any(|n| n.contains(ip(s)));
        assert!(allowed("127.0.0.1") && allowed("127.255.0.9") && allowed("::1"));
        assert!(!allowed("192.168.1.1") && !allowed("::2") && !allowed("0.0.0.0"));
    }

    #[test]
    fn malformed_ranges_are_refused() {
        for s in [
            "",
            "/24",
            "10.0.0",
            "10.0.0.0/",
            "10.0.0.0/x",
            "10.0.0.0/-1",
            "10.0.0.0/8/8",
            "host.local",
            " 10.0.0.0/8",
            "::ffff:10.0.0.0/104",
        ] {
            assert!(s.parse::<Cidr>().is_err(), "{s:?} parsed");
        }
    }

    #[test]
    fn a_prefix_out_of_range_is_refused() {
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("::/129".parse::<Cidr>().is_err());
        assert!("::/256".parse::<Cidr>().is_err());
    }

    #[test]
    fn host_bits_are_refused_naming_the_network() {
        let err = "192.168.1.5/24".parse::<Cidr>().unwrap_err();
        assert!(err.contains("192.168.1.0/24"), "{err}");
        let err = "2001:db8::1/64".parse::<Cidr>().unwrap_err();
        assert!(err.contains("2001:db8::/64"), "{err}");
    }

    #[test]
    fn it_displays_as_it_is_written() {
        assert_eq!(cidr("10.0.0.0/8").to_string(), "10.0.0.0/8");
        assert_eq!(cidr("::1").to_string(), "::1/128");
    }
}
