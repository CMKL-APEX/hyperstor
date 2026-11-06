//! A node's network ports, as it reports them to the fabric reconciler
//! every 10 s (26-networks.md §29.8, D203, D208): state, addresses, and
//! byte counters, all traffic and hyperstor's own.
//!
//! A port with an RDMA device is counted at its InfiniBand port
//! (`port_xmit_data`, `port_rcv_data`, in 4-byte words), which sees RDMA
//! traffic such as NCCL's as well as IPoIB; its netdev sees only IPoIB.
//! Other ports are counted at their netdev.

use glidex_topology::net::NetPort;
use hyperstor_proto::PortSample;
use std::net::IpAddr;
use std::path::Path;

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// The samples for `ports` under `sys` (`/sys`), with hyperstor's own
/// bytes `own` as (local address, sent, received).
pub fn sample(sys: &Path, ports: &[NetPort], own: &[(IpAddr, u64, u64)]) -> Vec<PortSample> {
    ports
        .iter()
        .map(|p| {
            let addrs: Vec<IpAddr> = p.addresses.iter().map(|a| a.addr).collect();
            let (tx_bytes, rx_bytes) = match &p.rdma {
                Some(r) => {
                    let c = sys.join(format!(
                        "class/infiniband/{}/ports/{}/counters",
                        r.device, r.port
                    ));
                    (
                        read_u64(&c.join("port_xmit_data")).unwrap_or(0) * 4,
                        read_u64(&c.join("port_rcv_data")).unwrap_or(0) * 4,
                    )
                }
                None => {
                    let c = sys.join(format!("class/net/{}/statistics", p.name));
                    (
                        read_u64(&c.join("tx_bytes")).unwrap_or(0),
                        read_u64(&c.join("rx_bytes")).unwrap_or(0),
                    )
                }
            };
            let (own_tx, own_rx) = own
                .iter()
                .filter(|(ip, _, _)| addrs.contains(ip))
                .fold((0, 0), |(t, r), (_, a, b)| (t + a, r + b));
            let speed_mbps = p
                .speed_mbps
                .or_else(|| p.rdma.as_ref()?.rate_gbps.map(|g| g * 1000))
                .unwrap_or(0);
            PortSample {
                name: p.name.clone(),
                up: p.state == "up",
                speed_mbps,
                addrs,
                tx_bytes,
                rx_bytes,
                own_tx,
                own_rx,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glidex_topology::net::{PortAddr, RdmaPort};

    fn write(root: &Path, rel: &str, v: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, v).unwrap();
    }

    /// An RDMA port is counted at its InfiniBand port, in words; another
    /// at its netdev; hyperstor's own bytes are those from its addresses.
    #[test]
    fn ports_are_counted_where_all_their_traffic_shows() {
        let sys = tempfile::tempdir().unwrap();
        let s = sys.path();
        write(
            s,
            "class/infiniband/mlx5_0/ports/1/counters/port_xmit_data",
            "1000\n",
        );
        write(
            s,
            "class/infiniband/mlx5_0/ports/1/counters/port_rcv_data",
            "10\n",
        );
        write(s, "class/net/ibp12s0/statistics/tx_bytes", "1\n");
        write(s, "class/net/eno1/statistics/tx_bytes", "5000\n");
        write(s, "class/net/eno1/statistics/rx_bytes", "6000\n");
        let ib = NetPort {
            name: "ibp12s0".into(),
            state: "up".into(),
            rdma: Some(RdmaPort {
                device: "mlx5_0".into(),
                port: 1,
                rate_gbps: Some(200),
                ..Default::default()
            }),
            addresses: vec![PortAddr {
                addr: "192.0.2.10".parse().unwrap(),
                prefix: 24,
            }],
            ..Default::default()
        };
        let eth = NetPort {
            name: "eno1".into(),
            state: "down".into(),
            speed_mbps: Some(1000),
            ..Default::default()
        };
        let own = [
            ("192.0.2.10".parse().unwrap(), 300, 4),
            ("203.0.113.1".parse().unwrap(), 9, 9),
        ];
        let got = sample(s, &[ib, eth], &own);
        assert_eq!(
            (
                got[0].tx_bytes,
                got[0].rx_bytes,
                got[0].own_tx,
                got[0].own_rx
            ),
            (4000, 40, 300, 4)
        );
        assert_eq!((got[0].speed_mbps, got[0].up), (200_000, true));
        assert_eq!(
            (
                got[1].tx_bytes,
                got[1].rx_bytes,
                got[1].own_tx,
                got[1].speed_mbps,
                got[1].up
            ),
            (5000, 6000, 0, 1000, false)
        );
    }
}
