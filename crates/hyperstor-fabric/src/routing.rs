//! Whether a host routes by source address where it must (D207, G47).
//! With several ports in one subnet, Linux picks the outgoing port by the
//! destination alone: one route per port, the first wins, so traffic
//! bound to any of their addresses leaves by one port. Each address then
//! needs a rule of its own (`ip rule add from ADDR lookup TABLE`).

use crate::model::in_subnet;
use glidex_topology::net::NetPort;
use std::net::IpAddr;

/// The endpoint addresses in `endpoints` that need a rule of their own and
/// have none in `rules` (the output of `ip rule show`, both families): those
/// on a port that shares its subnet with another port of `ports`.
pub fn unrouted(ports: &[NetPort], endpoints: &[IpAddr], rules: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for ip in endpoints {
        let Some((port, prefix)) = ports.iter().find_map(|p| {
            p.addresses
                .iter()
                .find(|a| a.addr == *ip)
                .map(|a| (p, a.prefix))
        }) else {
            continue;
        };
        let shared = ports.iter().any(|q| {
            q.name != port.name
                && q.addresses
                    .iter()
                    .any(|a| a.addr.is_ipv4() == ip.is_ipv4() && in_subnet(a.addr, *ip, prefix))
        });
        let from = format!("from {ip} ");
        let ruled = rules
            .lines()
            .any(|l| format!("{} ", l.trim_end()).contains(&from));
        if shared && !ruled {
            out.push(*ip);
        }
    }
    out
}
