//! Prints this host as the fabric sees it: NUMA nodes and sockets, network
//! ports with their RDMA side and fabric, and for each NVMe drive its
//! endpoints, taking every port that is up as one storage may use, on one
//! network per InfiniBand subnet and per IPv4 subnet.
//!
//! `cargo run --release -p hyperstor-fabric --example host`

use glidex_topology::Host;
use hyperstor_fabric::endpoints::{Device, propose, rank};
use hyperstor_fabric::model::{NetMatch, Network, NetworkKind, Role, Transport};
use std::collections::BTreeMap;

fn main() {
    let host = Host::read();
    println!("NUMA nodes:");
    for n in &host.numa {
        println!("  {} socket {:?}: {} CPUs", n.id, n.socket, n.cpus.len());
    }
    let mut networks: Vec<Network> = Vec::new();
    let mut add = |name: String, matches: NetMatch| {
        if !networks.iter().any(|n| n.matches == matches) {
            networks.push(Network {
                name,
                kind: NetworkKind::Data,
                trusted: true,
                transports: vec![Transport::Tcp],
                matches,
            });
        }
    };
    println!("Ports:");
    for p in &host.ports {
        let numa = host.function(&p.pci).and_then(|f| f.numa_node);
        let switch = host.places.get(&p.pci).and_then(|pl| pl.switch.clone());
        println!(
            "  {:<16} {:?} {} {:?} Mb/s mtu {} numa {:?} switch {:?} master {:?} rdma {:?}",
            p.name,
            p.link_layer,
            p.state,
            p.speed_mbps,
            p.mtu,
            numa,
            switch,
            p.master,
            p.rdma
                .as_ref()
                .map(|r| (&r.device, &r.state, r.rate_gbps, &r.subnet_prefix, r.sm_lid)),
        );
        if let Some(prefix) = p.rdma.as_ref().and_then(|r| r.subnet_prefix.clone()) {
            add(format!("ib-{prefix}"), NetMatch::IbSubnet(prefix));
        }
        for a in p.addresses.iter().filter(|a| a.addr.is_ipv4()) {
            let net = match a.addr {
                std::net::IpAddr::V4(v) => {
                    let mask = u32::MAX.checked_shl(32 - u32::from(a.prefix)).unwrap_or(0);
                    std::net::IpAddr::V4((u32::from(v) & mask).into())
                }
                v6 => v6,
            };
            add(
                format!("ip-{}/{}", net, a.prefix),
                NetMatch::Subnet {
                    net,
                    prefix: a.prefix,
                },
            );
        }
    }
    let roles: BTreeMap<String, Role> = host
        .ports
        .iter()
        .map(|p| (p.name.clone(), Role::Storage))
        .collect();
    for f in host
        .pci
        .iter()
        .filter(|f| f.class_id.trim_start_matches("0x").starts_with("0108"))
    {
        println!("NVMe {} (numa {:?}):", f.address, f.numa_node);
        for e in rank(&host, &f.address, &networks, &roles, 2) {
            println!(
                "  {:<28} {:<16} {:?}{}",
                e.network,
                e.port,
                e.closeness,
                if e.bound { "  bound" } else { "" }
            );
        }
    }
    // What the installer would propose (D215), at port 9100, for a
    // device on each NVMe drive.
    let devices: Vec<Device> = host
        .pci
        .iter()
        .filter(|f| f.class_id.trim_start_matches("0x").starts_with("0108"))
        .enumerate()
        .map(|(i, f)| Device {
            id: i as u32,
            pci: Some(f.address.clone()),
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], 9100)),
        })
        .collect();
    let proposed = propose(&host, &devices, &networks, 2);
    for d in &devices {
        println!("Proposed for NVMe {}:", d.pci.as_deref().unwrap_or("?"));
        for (n, a) in proposed.get(&d.id).into_iter().flatten() {
            let port = host
                .ports
                .iter()
                .find(|p| p.addresses.iter().any(|x| x.addr == a.ip()))
                .map_or("?", |p| p.name.as_str());
            println!("  {n:<28} {a} ({port})");
        }
    }
}
