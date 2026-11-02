//! Whether a host routes by source address where it must (D207, G47).
//! With several ports in one subnet, Linux picks the outgoing port by the
//! destination alone: one route per port, the first wins, so traffic
//! bound to any of their addresses leaves by one port. Each address then
//! needs a rule of its own (`ip rule add from ADDR lookup TABLE`).

use crate::model::in_subnet;
use glidex_topology::net::NetPort;
use std::net::IpAddr;
use std::path::Path;

/// Whether reverse-path filtering is strict on `port` (D215): the larger
/// of `all`'s and the port's own `rp_filter` under `proc` (`/proc` on a
/// host) is 1. Strict filtering drops packets that arrive on a port other
/// than the one the reply would leave by, so an endpoint without its own
/// rule loses connections, not just bandwidth. Loose (2) or off (0) only
/// loses the spread over ports.
pub fn rp_filter_strict(proc: &Path, port: &str) -> bool {
    let read = |name: &str| {
        std::fs::read_to_string(proc.join(format!("sys/net/ipv4/conf/{name}/rp_filter")))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0)
    };
    read("all").max(read(port)) == 1
}

/// The port an address is on, among `ports`.
pub fn port_of(ports: &[NetPort], ip: IpAddr) -> Option<&NetPort> {
    ports
        .iter()
        .find(|p| p.addresses.iter().any(|a| a.addr == ip))
}

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
