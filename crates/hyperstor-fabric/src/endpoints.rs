//! A reactor's endpoints, ranked by closeness to its device (D207,
//! spec/26-networks.md §29.5).

use crate::model::{NetMatch, Network, Role, in_subnet};
use glidex_topology::{Closeness, Host, net::NetPort};
use std::collections::BTreeMap;
use std::net::IpAddr;

/// One port a reactor may listen and send on, for one network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub network: String,
    /// The netdev.
    pub port: String,
    pub addr: IpAddr,
    pub closeness: Closeness,
    /// Among the closest few of its network: the reactor listens on it.
    pub bound: bool,
}

/// The fabric a port is on, for a network: two ports reach each other
/// directly only on the same one (§29.3).
pub fn fabric(port: &NetPort, network: &Network) -> Option<String> {
    match &network.matches {
        NetMatch::IbSubnet(prefix) => {
            let r = port.rdma.as_ref()?;
            (r.subnet_prefix.as_deref() == Some(prefix.as_str())).then(|| {
                format!(
                    "ib:{prefix}:sm{}",
                    r.sm_lid.map(|l| l.to_string()).unwrap_or_default()
                )
            })
        }
        NetMatch::Subnet { net, prefix } => port
            .addresses
            .iter()
            .any(|a| in_subnet(a.addr, *net, *prefix))
            .then(|| format!("ip:{net}/{prefix}")),
    }
}

/// The address a port has on a network: in its subnet, or for an
/// InfiniBand network its first IPv4 address (IPoIB carries the TCP
/// session every connection starts with, 13-protocol.md §13.2).
fn address(port: &NetPort, network: &Network) -> Option<IpAddr> {
    match &network.matches {
        NetMatch::Subnet { net, prefix } => port
            .addresses
            .iter()
            .map(|a| a.addr)
            .find(|a| in_subnet(*a, *net, *prefix)),
        NetMatch::IbSubnet(_) => port
            .addresses
            .iter()
            .map(|a| a.addr)
            .find(IpAddr::is_ipv4)
            .or_else(|| port.addresses.first().map(|a| a.addr)),
    }
}

/// The endpoints for a reactor whose device is the PCI function `device`:
/// every port that is up, has a role storage may use (`roles`, by netdev;
/// a port without one is `Host`), and is on a data network, nearest first;
/// the nearest `bind_per_network` of each network bound.
pub fn rank(
    host: &Host,
    device: &str,
    networks: &[Network],
    roles: &BTreeMap<String, Role>,
    bind_per_network: usize,
) -> Vec<Endpoint> {
    let mut out = Vec::new();
    for net in networks.iter().filter(|n| n.kind.carries_data()) {
        let mut found: Vec<Endpoint> = host
            .ports
            .iter()
            .filter(|p| p.state == "up")
            .filter(|p| {
                roles
                    .get(&p.name)
                    .copied()
                    .unwrap_or(Role::Host)
                    .storage_may_use()
            })
            .filter(|p| fabric(p, net).is_some())
            .filter_map(|p| {
                Some(Endpoint {
                    network: net.name.clone(),
                    port: p.name.clone(),
                    addr: address(p, net)?,
                    closeness: host.closeness(device, &p.pci),
                    bound: false,
                })
            })
            .collect();
        // Nearest first; then by name, so every host ranks alike.
        found.sort_by(|a, b| (a.closeness, &a.port).cmp(&(b.closeness, &b.port)));
        for e in found.iter_mut().take(bind_per_network) {
            e.bound = true;
        }
        out.extend(found);
    }
    out
}
