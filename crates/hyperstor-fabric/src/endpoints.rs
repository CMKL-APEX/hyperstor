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

/// A PCI address (`0000:0c:00.0`).
fn is_pci_address(s: &str) -> bool {
    let b = s.as_bytes();
    let hex = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_hexdigit);
    b.len() == 12
        && hex(0..4)
        && b[4] == b':'
        && hex(5..7)
        && b[7] == b':'
        && hex(8..10)
        && b[10] == b'.'
        && b[11].is_ascii_digit()
}

/// The block device (major, minor) `path` is, or the file system it is on
/// lives on.
pub fn block_of_path(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = std::fs::metadata(path).ok()?;
    let dev = if m.file_type().is_block_device() {
        m.rdev()
    } else {
        m.dev()
    };
    // Linux's dev_t: 12 bits of major at 8, the rest at 32; minor around it.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    Some((major as u32, minor as u32))
}

/// The PCI function behind block device `major:minor` under `sys`: the
/// last PCI address in its sysfs path; for a virtual one (md, dm), that
/// of its first member (`slaves/`), followed down.
pub fn pci_of_block(sys: &std::path::Path, major: u32, minor: u32) -> Option<String> {
    let mut dir = std::fs::canonicalize(sys.join(format!("dev/block/{major}:{minor}"))).ok()?;
    for _ in 0..8 {
        let found = dir
            .components()
            .rev()
            .filter_map(|c| c.as_os_str().to_str())
            .find(|c| is_pci_address(c))
            .map(str::to_string);
        if found.is_some() {
            return found;
        }
        let first = std::fs::read_dir(dir.join("slaves"))
            .ok()?
            .flatten()
            .map(|e| e.file_name())
            .min()?;
        dir = std::fs::canonicalize(sys.join("class/block").join(first)).ok()?;
    }
    None
}

/// A device for [`propose`]: its id, the PCI function it is on (if known)
/// and the address its reactor already listens on.
pub struct Device {
    pub id: u32,
    pub pci: Option<String>,
    pub addr: std::net::SocketAddr,
}

/// Endpoints the installer proposes (D214): for each device, its nearest
/// `per_network` up ports on each data network ([`rank`], every port that
/// isn't a bond's member taken as storage's), at the device's own port
/// number, leaving out its own address. A bond's members share its
/// address, so the bond counts once.
pub fn propose(
    host: &Host,
    devices: &[Device],
    networks: &[Network],
    per_network: usize,
) -> BTreeMap<u32, Vec<(String, std::net::SocketAddr)>> {
    let roles: BTreeMap<String, Role> = host
        .ports
        .iter()
        .map(|p| (p.name.clone(), Role::Storage))
        .collect();
    let mut out = BTreeMap::new();
    for d in devices {
        let mut eps: Vec<(String, std::net::SocketAddr)> = Vec::new();
        for net in networks.iter().filter(|n| n.kind.carries_data()) {
            let mut taken = 0;
            for e in rank(
                host,
                d.pci.as_deref().unwrap_or(""),
                std::slice::from_ref(net),
                &roles,
                usize::MAX,
            ) {
                let a = std::net::SocketAddr::new(e.addr, d.addr.port());
                if taken == per_network
                    || e.addr == d.addr.ip()
                    || eps.iter().any(|(n, x)| *n == net.name && *x == a)
                {
                    continue;
                }
                eps.push((net.name.clone(), a));
                taken += 1;
            }
        }
        if !eps.is_empty() {
            out.insert(d.id, eps);
        }
    }
    out
}
