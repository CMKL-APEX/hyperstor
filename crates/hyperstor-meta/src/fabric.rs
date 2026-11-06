//! The fabric reconciler in the leader (26-networks.md §29.8, D203,
//! D208, D209, D237, D238): nodes' port reports kept in memory, turned
//! into rates, and every 10 s `hyperstor_fabric::reconcile` run over the
//! ports that matter to storage, with its leases and weights written
//! into the map.
//!
//! Only ports that carry an endpoint, or that the operator gave a role,
//! are in the map: a cluster without endpoints keeps its map format, so
//! nodes from before format 17 still read it. Idle times stay here, not
//! in the map: a new leader starts them again, which delays a loan by at
//! most a dwell.

use hyperstor_fabric::reconcile::{Change, Observed, Policy, PortState, apply, reconcile};
use hyperstor_map::ClusterMap;
use hyperstor_proto::PortSample;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// How often the reconciler runs (§29.8).
pub const EVERY: Duration = Duration::from_secs(10);

/// A port's last report, and the traffic other than hyperstor's it
/// showed since the one before, in bytes/s per direction, the larger.
#[derive(Debug, Clone)]
pub struct Seen {
    pub at: Instant,
    pub sample: PortSample,
    pub other: Option<f64>,
    /// All its traffic, sent and received, bytes/s.
    pub rates: Option<(f64, f64)>,
}

#[derive(Debug)]
pub struct Fabric {
    pub policy: Policy,
    pub every: Duration,
    seen: HashMap<String, Seen>,
    idle: HashMap<String, u64>,
    last: Option<Instant>,
}

impl Default for Fabric {
    fn default() -> Fabric {
        Fabric {
            policy: Policy::default(),
            every: EVERY,
            seen: HashMap::new(),
            idle: HashMap::new(),
            last: None,
        }
    }
}

impl Fabric {
    /// Notes `node`'s report at `now`.
    pub fn note(&mut self, node: u32, ports: Vec<PortSample>, now: Instant) {
        for p in ports {
            let id = format!("{node}/{}", p.name);
            let prev = self.seen.get(&id);
            let (mut other, mut rates) = (None, None);
            if let Some(prev) = prev {
                let dt = now.duration_since(prev.at).as_secs_f64();
                let q = &prev.sample;
                // Counters reset (a reboot, a restarted node): no rate
                // this time.
                if dt > 0.0
                    && p.tx_bytes >= q.tx_bytes
                    && p.rx_bytes >= q.rx_bytes
                    && p.own_tx >= q.own_tx
                    && p.own_rx >= q.own_rx
                {
                    let (tx, rx) = (p.tx_bytes - q.tx_bytes, p.rx_bytes - q.rx_bytes);
                    let otx = tx.saturating_sub(p.own_tx - q.own_tx);
                    let orx = rx.saturating_sub(p.own_rx - q.own_rx);
                    other = Some(otx.max(orx) as f64 / dt);
                    rates = Some((tx as f64 / dt, rx as f64 / dt));
                }
            }
            self.seen.insert(
                id,
                Seen {
                    at: now,
                    sample: p,
                    other,
                    rates,
                },
            );
        }
    }

    /// Every port reported recently, by `NODE/NETDEV`.
    pub fn seen(&self, now: Instant) -> BTreeMap<String, Seen> {
        self.seen
            .iter()
            .filter(|(_, s)| now.duration_since(s.at) < self.every * 3)
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect()
    }

    /// Whether a pass is due at `now`.
    pub fn due(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < self.every)
        {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// One pass at `now` (`unix_ms` for what the map keeps): ports'
    /// addresses from their reports, then the reconciler's changes. True
    /// if the map changed; its epoch moved.
    pub fn tick(&mut self, map: &mut ClusterMap, now: Instant, unix_ms: u64) -> bool {
        let seen = self.seen(now);
        self.seen.retain(|id, _| seen.contains_key(id));
        let mut changed = false;
        // A port matters when it carries one of its node's endpoints, or
        // the operator gave it a role.
        let matters = |map: &ClusterMap, id: &str, addrs: &[std::net::IpAddr]| {
            let node: Option<u32> = id.split_once('/').and_then(|(n, _)| n.parse().ok());
            map.ports.get(id).is_some_and(|p| p.role.is_some())
                || map.endpoints.iter().any(|(d, eps)| {
                    map.devices.get(d).map(|d| d.node) == node
                        && eps.iter().any(|(_, a)| addrs.contains(&a.ip()))
                })
        };
        for (id, s) in &seen {
            let node: Option<u32> = id.split_once('/').and_then(|(n, _)| n.parse().ok());
            if !node.is_some_and(|n| map.nodes.contains_key(&n)) {
                continue;
            }
            if !matters(map, id, &s.sample.addrs) {
                continue;
            }
            let new = !map.ports.contains_key(id);
            let p = map.ports.entry(id.clone()).or_default();
            if p.addrs != s.sample.addrs {
                p.addrs = s.sample.addrs.clone();
                changed = true;
            }
            // A new port starts at its link's speed where storage may use
            // it, until its rates weigh it: senders don't leave it for a
            // report's time.
            if new && map.port_role(id).storage_may_use() {
                map.ports.get_mut(id).unwrap().weight_mbs = s.sample.speed_mbps / 8;
            }
        }
        // Ports that no longer matter leave the map.
        let gone: Vec<String> = map
            .ports
            .iter()
            .filter(|(id, p)| !matters(map, id, &p.addrs))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            map.ports.remove(&id);
            self.idle.remove(&id);
            changed = true;
        }

        let observed: BTreeMap<String, Observed> = seen
            .iter()
            .filter(|(id, _)| map.ports.contains_key(*id))
            .filter_map(|(id, s)| {
                Some((
                    id.clone(),
                    Observed {
                        role: map.port_role(id),
                        up: s.sample.up,
                        speed: u64::from(s.sample.speed_mbps) * 1_000_000 / 8,
                        other: s.other? as u64,
                    },
                ))
            })
            .collect();
        let mut state: BTreeMap<String, PortState> = map
            .ports
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    PortState {
                        role: observed.get(id).map(|o| o.role),
                        lent: p.lent,
                        revoking: p.revoking,
                        weight: f64::from(p.weight_mbs) / 1000.0,
                        last_change: p.last_change,
                        idle_since: self.idle.get(id).copied(),
                    },
                )
            })
            .collect();
        let changes = reconcile(unix_ms, &self.policy, &observed, &BTreeMap::new(), &state);
        apply(&mut state, &changes);
        for c in &changes {
            match c {
                Change::Lend { port, .. } => {
                    eprintln!("hyperstor-meta: port {port} lent to storage (idle)")
                }
                Change::Revoke { port, until } => eprintln!(
                    "hyperstor-meta: port {port} taken back from storage, in {} s",
                    until.saturating_sub(unix_ms) / 1000
                ),
                Change::Return { port, .. } => {
                    eprintln!("hyperstor-meta: port {port} returned")
                }
                _ => {}
            }
        }
        for (id, s) in state {
            match s.idle_since {
                Some(t) => self.idle.insert(id.clone(), t),
                None => self.idle.remove(&id),
            };
            let Some(p) = map.ports.get_mut(&id) else {
                continue;
            };
            let weight_mbs = (s.weight * 1000.0).round() as u32;
            if (p.lent, p.revoking, p.weight_mbs, p.last_change)
                != (s.lent, s.revoking, weight_mbs, s.last_change)
            {
                p.lent = s.lent;
                p.revoking = s.revoking;
                p.weight_mbs = weight_mbs;
                p.last_change = s.last_change;
                changed = true;
            }
        }
        if changed {
            map.epoch += 1;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_map::{PortRole, parse_cluster};

    const FILE: &str = "cluster 1
network ib data trusted subnet 192.0.2.0/24
node 0 rack 0 203.0.113.10:9100
endpoint 0 ib 192.0.2.10:9100
endpoint 0 ib 192.0.2.11:9100
node 1 rack 0 203.0.113.11:9100
pool 1 v replicated size 2 min 1 pgs 8
";

    /// 200 Gb/s, in bytes/s.
    const HDR: u64 = 25_000_000_000;

    fn port(name: &str, addr: &str, tx: u64, own: u64) -> PortSample {
        PortSample {
            name: name.into(),
            up: true,
            speed_mbps: 200_000,
            addrs: vec![addr.parse().unwrap()],
            tx_bytes: tx,
            rx_bytes: 0,
            own_tx: own,
            own_rx: 0,
        }
    }

    /// Node 0's counters so far: ib0, ib1 (all and hyperstor's), eno1.
    #[derive(Default)]
    struct Node0 {
        ib0: u64,
        ib1: u64,
        ib1_own: u64,
        eno1: u64,
    }

    impl Node0 {
        /// 10 s more, with `other` and `own` bytes/s on its compute port
        /// ib1, and hyperstor filling half of ib0; then its report.
        fn report(&mut self, f: &mut Fabric, at: Instant, other: u64, own: u64) {
            self.ib0 += 10 * HDR / 2;
            self.ib1 += 10 * (other + own);
            self.ib1_own += 10 * own;
            self.eno1 += 1000;
            f.note(
                0,
                vec![
                    port("ib0", "192.0.2.10", self.ib0, self.ib0),
                    port("ib1", "192.0.2.11", self.ib1, self.ib1_own),
                    port("eno1", "203.0.113.10", self.eno1, 0),
                ],
                at,
            );
        }
    }

    /// The leader weights ports that carry endpoints by the bandwidth
    /// other traffic leaves them (hyperstor's own doesn't count), lends a
    /// `compute` port idle for a dwell, takes it back when other traffic
    /// appears, and returns it after the deadline (D209, D237, D238).
    #[test]
    fn ports_are_weighted_lent_while_idle_and_taken_back() {
        let mut map = parse_cluster(FILE).unwrap();
        map.set_port_role("0/ib1", Some(PortRole::Compute)).unwrap();
        let mut f = Fabric::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let ms = |s: u64| 1_700_000_000_000 + s * 1000;

        let mut n0 = Node0::default();
        n0.report(&mut f, at(0), 0, 0);
        assert!(f.tick(&mut map, at(0), ms(0)), "addresses are new");
        assert!(!map.ports.contains_key("0/eno1"), "no endpoint, no role");
        assert_eq!(
            map.ports["0/ib0"].weight_mbs, 25_000,
            "the link, until rates"
        );
        assert_eq!(map.ports["0/ib1"].weight_mbs, 0, "compute: not until lent");

        // Hyperstor fills ib0: its weight is still the whole link.
        n0.report(&mut f, at(10), 0, 0);
        f.tick(&mut map, at(10), ms(10));
        assert_eq!(map.ports["0/ib0"].weight_mbs, 25_000);
        assert_eq!(map.port_role("0/ib0"), PortRole::Storage);
        let ib1 = &map.ports["0/ib1"];
        assert!(!ib1.lent && ib1.weight_mbs == 0, "compute, not yet lent");

        // Idle for the 60 s dwell: lent, and weighted.
        for s in (20..=70).step_by(10) {
            n0.report(&mut f, at(s), 0, 0);
            f.tick(&mut map, at(s), ms(s));
        }
        let ib1 = &map.ports["0/ib1"];
        assert!(ib1.lent, "lent after 60 s idle");
        assert_eq!(ib1.weight_mbs, 25_000);

        // Storage's own traffic on the lent port doesn't make it busy.
        n0.report(&mut f, at(80), 0, HDR / 2);
        f.tick(&mut map, at(80), ms(80));
        assert!(map.ports["0/ib1"].revoking.is_none());

        // A job's traffic: taken back at once, weight 0, 30 s to drain.
        n0.report(&mut f, at(90), HDR / 5, 0);
        f.tick(&mut map, at(90), ms(90));
        let ib1 = &map.ports["0/ib1"];
        assert_eq!(ib1.revoking, Some(ms(120)));
        assert_eq!(ib1.weight_mbs, 0);
        let ep: std::net::SocketAddr = "192.0.2.11:9100".parse().unwrap();
        assert_eq!(map.endpoint_weight(ep), Some(0.0));
        assert_eq!(
            map.weighted_addrs_on(0, &["ib".into()]),
            vec![("192.0.2.10:9100".parse().unwrap(), 25.0)]
        );
        for s in [100, 110, 120] {
            n0.report(&mut f, at(s), HDR / 5, 0);
            f.tick(&mut map, at(s), ms(s));
        }
        let ib1 = &map.ports["0/ib1"];
        assert!(!ib1.lent && ib1.revoking.is_none(), "returned");

        // Back to the default role: it carries an endpoint, so storage.
        map.set_port_role("0/ib1", None).unwrap();
        f.tick(&mut map, at(130), ms(130));
        assert_eq!(map.port_role("0/ib1"), PortRole::Storage);
    }

    /// Without endpoints or roles, reports leave the map as it is.
    #[test]
    fn a_cluster_without_endpoints_keeps_its_map() {
        let mut map = parse_cluster(&FILE.replace("endpoint", "# endpoint")).unwrap();
        let before = map.clone();
        let mut f = Fabric::default();
        let t0 = Instant::now();
        let mut n0 = Node0::default();
        n0.report(&mut f, t0, 0, 0);
        n0.report(&mut f, t0 + Duration::from_secs(10), 0, 0);
        assert!(!f.tick(&mut map, t0 + Duration::from_secs(10), 1));
        assert_eq!(map, before);
    }
}
