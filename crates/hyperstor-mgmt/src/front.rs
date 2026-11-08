//! mgmt behind a reverse proxy or a CDN such as Cloudflare (04-decisions.md
//! D277). A connection comes from a proxy when it presents a client
//! certificate from `--proxy-ca` (Cloudflare's Authenticated Origin Pulls
//! CA, or the organization's own), or comes from an address in
//! `--trusted-proxies`. Only then is the person's address taken from
//! `CF-Connecting-IP`, else the last `X-Forwarded-For` entry; from anyone
//! else those headers are ignored, as anyone can send them. With
//! `--proxy-only`, other connections are refused, except from loopback
//! (`hyperstorctl --mgmt` on the host).

use std::net::{IpAddr, SocketAddr};

/// An address range, `ADDR/BITS` or one address.
#[derive(Debug, Clone, PartialEq)]
pub struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Cidr> {
        let (a, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
            None => (s, None),
        };
        let net: IpAddr = a.parse().ok()?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        (bits <= max).then_some(Cidr { net, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let m = u32::MAX.checked_shl(32 - self.bits as u32).unwrap_or(0);
                u32::from(n) & m == u32::from(a) & m
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let m = u128::MAX.checked_shl(128 - self.bits as u32).unwrap_or(0);
                u128::from(n) & m == u128::from(a) & m
            }
            _ => false,
        }
    }
}

/// An IPv4 address mapped into IPv6 (a dual-stack listener's) as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// `--trusted-proxies`' list, or why it isn't one.
pub fn parse_cidrs(list: &str) -> Result<Vec<Cidr>, String> {
    list.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| Cidr::parse(s.trim()).ok_or_else(|| format!("{s}: not ADDR or ADDR/BITS")))
        .collect()
}

#[derive(Default)]
pub struct Front {
    /// `--trusted-proxies`.
    pub trusted: Vec<Cidr>,
    /// `--proxy-only`.
    pub only: bool,
}

impl Front {
    /// Whether a connection from `peer` comes from a proxy; `cert`: it
    /// presented a client certificate `--proxy-ca` verified.
    pub fn via_proxy(&self, peer: IpAddr, cert: bool) -> bool {
        cert || self.trusted.iter().any(|c| c.contains(peer))
    }

    /// Whether a connection from `peer` may be served at all.
    pub fn admits(&self, peer: IpAddr, via_proxy: bool) -> bool {
        !self.only || via_proxy || canonical(peer).is_loopback()
    }
}

/// The person's address: from a proxy, what it says
/// (`CF-Connecting-IP`, else the last `X-Forwarded-For` entry, the one
/// the proxy added); otherwise, or when it says nothing usable, `peer`.
pub fn client(headers: &http::HeaderMap, peer: SocketAddr, via_proxy: bool) -> SocketAddr {
    if !via_proxy {
        return peer;
    }
    let said = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .or_else(|| {
            headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .next_back()
                .and_then(|v| v.trim().parse::<IpAddr>().ok())
        });
    said.map_or(peer, |ip| SocketAddr::new(ip, peer.port()))
}

/// Whether `name` (lowercase) is one of a certificate's DNS `names`: the
/// same, or under a wildcard's one label (`*.example.com`).
pub fn on_cert(name: &str, names: &[String]) -> bool {
    let name = name.to_ascii_lowercase();
    names.iter().any(|n| match n.strip_prefix("*.") {
        Some(base) => name
            .split_once('.')
            .is_some_and(|(label, rest)| !label.is_empty() && rest == base),
        None => *n == name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ranges_hold_what_they_should() {
        let c = parse_cidrs("10.1.0.0/16,2001:db8::/32,192.0.2.7").unwrap();
        assert!(c[0].contains(ip("10.1.200.3")));
        assert!(!c[0].contains(ip("10.2.0.1")));
        // A dual-stack listener's mapped IPv4 address.
        assert!(c[0].contains(ip("::ffff:10.1.0.9")));
        assert!(c[1].contains(ip("2001:db8:5::1")));
        assert!(!c[1].contains(ip("10.1.0.1")));
        assert!(c[2].contains(ip("192.0.2.7")) && !c[2].contains(ip("192.0.2.8")));
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains(ip("203.0.113.1"))
        );
        assert!(parse_cidrs("10.0.0.0/33").is_err());
        assert!(parse_cidrs("proxy.example").is_err());
    }

    #[test]
    fn only_a_proxy_says_who_is_calling() {
        let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let mut h = http::HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            "198.51.100.9, 203.0.113.5".parse().unwrap(),
        );
        assert_eq!(client(&h, peer, false), peer);
        // The last entry: the one the proxy itself added.
        assert_eq!(client(&h, peer, true).ip(), ip("203.0.113.5"));
        h.insert("cf-connecting-ip", "2001:db8::7".parse().unwrap());
        assert_eq!(client(&h, peer, true).ip(), ip("2001:db8::7"));
        h.insert("cf-connecting-ip", "nonsense".parse().unwrap());
        assert_eq!(client(&h, peer, true).ip(), ip("203.0.113.5"));
        assert_eq!(client(&http::HeaderMap::new(), peer, true), peer);
    }

    #[test]
    fn proxy_only_still_lets_the_host_in() {
        let f = Front {
            trusted: parse_cidrs("10.0.0.0/8").unwrap(),
            only: true,
        };
        assert!(f.via_proxy(ip("10.3.4.5"), false));
        assert!(f.via_proxy(ip("203.0.113.1"), true));
        assert!(!f.via_proxy(ip("203.0.113.1"), false));
        assert!(f.admits(ip("127.0.0.1"), false) && f.admits(ip("::1"), false));
        assert!(!f.admits(ip("203.0.113.1"), false));
        assert!(Front::default().admits(ip("203.0.113.1"), false));
    }

    #[test]
    fn names_on_the_certificate() {
        let names = vec![
            "mgmt.example.com".to_string(),
            "*.ops.example.com".to_string(),
        ];
        assert!(on_cert("mgmt.example.com", &names));
        assert!(on_cert("MGMT.example.com", &names));
        assert!(on_cert("a.ops.example.com", &names));
        assert!(!on_cert("ops.example.com", &names));
        assert!(!on_cert("a.b.ops.example.com", &names));
        assert!(!on_cert("evil.example", &names));
    }
}
