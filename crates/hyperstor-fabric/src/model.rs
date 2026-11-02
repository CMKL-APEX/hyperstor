//! Networks, ports' roles and the lending policy (spec/26-networks.md
//! §29.3, §29.8).

use std::net::IpAddr;

/// What a network carries (D206).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkKind {
    /// The map protocol, Raft, the API, mgmt, heartbeats.
    Control,
    /// Client I/O, replication, recovery, backfill.
    Data,
    /// A cluster with one network carries both, as before.
    Both,
}

impl NetworkKind {
    pub fn carries_data(self) -> bool {
        matches!(self, NetworkKind::Data | NetworkKind::Both)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Roce,
    Ib,
}

/// How a port is recognised as on a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetMatch {
    /// A port with an address in this subnet.
    Subnet { net: IpAddr, prefix: u8 },
    /// An InfiniBand port with this subnet prefix (`fe80:0000:0000:0000`).
    IbSubnet(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub kind: NetworkKind,
    /// D25: plaintext after the handshake.
    pub trusted: bool,
    pub transports: Vec<Transport>,
    pub matches: NetMatch,
}

/// A port's role (D209).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// The host's own traffic; hyperstor's control traffic.
    Host,
    /// A VM switch's uplink.
    Uplink,
    Storage,
    /// A GPU rail: storage only when lent or for GPU-direct.
    Compute,
    /// Storage in its own traffic class beside other traffic.
    Converged,
    /// A pool of VFs for VMs; never hyperstor's.
    Vm,
}

impl Role {
    /// Hyperstor's data traffic may use the port without a loan.
    pub fn storage_may_use(self) -> bool {
        matches!(self, Role::Storage | Role::Converged)
    }
}

/// When `compute` ports are lent to storage (D209).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lending {
    Off,
    /// While the port's other traffic stays under the idle threshold for a
    /// dwell and no claim holds it.
    #[default]
    Idle,
    Always,
}

/// Whether `addr` is in `net/prefix`.
pub fn in_subnet(addr: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let p = u32::from(prefix.min(32));
            let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let p = u32::from(prefix.min(128));
            let mask = if p == 0 { 0 } else { u128::MAX << (128 - p) };
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}
