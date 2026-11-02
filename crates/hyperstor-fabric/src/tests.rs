use crate::endpoints::{fabric, rank};
use crate::model::{Lending, NetMatch, Network, NetworkKind, Role, Transport};
use crate::path::{PathLoad, choose};
use crate::reconcile::{Change, Claim, Observed, Policy, PortState, apply, reconcile};
use glidex_topology::Closeness;
use glidex_topology::Host;
use glidex_topology::net::{LinkLayer, NetPort, PortAddr, RdmaPort};
use glidex_topology::numa::Numa;
use glidex_topology::pci::{RawPci, places};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;

const BRIDGE: &str = "0x060400";

fn function(path: &[&str], class: &str, numa: u32) -> RawPci {
    RawPci {
        address: path.last().unwrap().to_string(),
        class_id: class.into(),
        numa_node: Some(numa),
        path: path.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn ib_port(name: &str, pci: &str, ip: &str, sm: u32) -> NetPort {
    NetPort {
        name: name.into(),
        pci: pci.into(),
        link_layer: LinkLayer::Infiniband,
        state: "up".into(),
        speed_mbps: Some(200_000),
        mtu: 2044,
        master: None,
        rdma: Some(RdmaPort {
            device: format!("mlx5_{name}"),
            port: 1,
            link_layer: LinkLayer::Infiniband,
            state: "ACTIVE".into(),
            rate_gbps: Some(200),
            subnet_prefix: Some("fe80:0000:0000:0000".into()),
            sm_lid: Some(sm),
        }),
        addresses: vec![PortAddr {
            addr: ip.parse().unwrap(),
            prefix: 24,
        }],
    }
}

/// Two sockets. Socket 0: a switch with an NVMe drive and two InfiniBand
/// ports. Socket 1: a switch with two more, and a bonded Ethernet port on
/// a root port; a 1 GbE port for management.
fn hgx() -> Host {
    let s0 = ["pci0000:00", "0000:00:01.1", "0000:01:00.0", "0000:02:00.0"];
    let s1 = ["pci0000:80", "0000:80:01.1", "0000:81:00.0", "0000:82:00.0"];
    let mut pci = vec![
        function(&s0[..2], BRIDGE, 0),
        function(&s0[..3], BRIDGE, 0),
        function(&s0, BRIDGE, 0),
        function(&s1[..2], BRIDGE, 2),
        function(&s1[..3], BRIDGE, 2),
        function(&s1, BRIDGE, 2),
    ];
    for (base, devs, numa) in [
        (&s0, ["0000:03:00.0", "0000:04:00.0", "0000:05:00.0"], 0),
        (&s1, ["0000:83:00.0", "0000:84:00.0", "0000:85:00.0"], 2),
    ] {
        for (i, d) in devs.iter().enumerate() {
            let class = if i == 0 { "0x010802" } else { "0x020700" };
            pci.push(function(&[&base[..], &[*d]].concat(), class, numa));
        }
    }
    pci.push(function(&["pci0000:e0", "0000:e0:03.1"], BRIDGE, 3));
    pci.push(function(
        &["pci0000:e0", "0000:e0:03.1", "0000:e1:00.0"],
        "0x020000",
        3,
    ));
    pci.push(function(&["pci0000:e0", "0000:e0:03.2"], "0x020000", 3));
    let ports = vec![
        ib_port("ib0", "0000:04:00.0", "192.0.2.10", 7),
        ib_port("ib1", "0000:05:00.0", "192.0.2.11", 7),
        ib_port("ib2", "0000:84:00.0", "192.0.2.12", 7),
        ib_port("ib3", "0000:85:00.0", "192.0.2.13", 7),
        NetPort {
            name: "eth0".into(),
            pci: "0000:e1:00.0".into(),
            state: "up".into(),
            master: Some("bond0".into()),
            addresses: vec![PortAddr {
                addr: "198.51.100.10".parse().unwrap(),
                prefix: 24,
            }],
            ..Default::default()
        },
        NetPort {
            name: "mgmt0".into(),
            pci: "0000:e0:03.2".into(),
            state: "up".into(),
            addresses: vec![PortAddr {
                addr: "203.0.113.10".parse().unwrap(),
                prefix: 24,
            }],
            ..Default::default()
        },
    ];
    Host {
        numa: (0..4)
            .map(|id| Numa {
                id,
                cpus: vec![id],
                socket: Some(id / 2),
            })
            .collect(),
        places: places(&pci),
        pci,
        ports,
    }
}

fn networks() -> Vec<Network> {
    vec![
        Network {
            name: "mgmt".into(),
            kind: NetworkKind::Control,
            trusted: false,
            transports: vec![Transport::Tcp],
            matches: NetMatch::Subnet {
                net: "203.0.113.0".parse().unwrap(),
                prefix: 24,
            },
        },
        Network {
            name: "ib".into(),
            kind: NetworkKind::Data,
            trusted: true,
            transports: vec![Transport::Tcp, Transport::Ib],
            matches: NetMatch::IbSubnet("fe80:0000:0000:0000".into()),
        },
        Network {
            name: "eth100".into(),
            kind: NetworkKind::Data,
            trusted: false,
            transports: vec![Transport::Tcp],
            matches: NetMatch::Subnet {
                net: "198.51.100.0".parse().unwrap(),
                prefix: 24,
            },
        },
    ]
}

fn roles(pairs: &[(&str, Role)]) -> BTreeMap<String, Role> {
    pairs.iter().map(|(p, r)| (p.to_string(), *r)).collect()
}

#[test]
fn endpoints_nearest_first_two_bound_per_network() {
    let host = hgx();
    let r = roles(&[
        ("ib0", Role::Storage),
        ("ib1", Role::Storage),
        ("ib2", Role::Storage),
        ("ib3", Role::Storage),
        ("eth0", Role::Uplink),
        ("mgmt0", Role::Host),
    ]);
    let eps = rank(&host, "0000:03:00.0", &networks(), &r, 2);
    let got: Vec<(&str, &str, Closeness, bool)> = eps
        .iter()
        .map(|e| (e.network.as_str(), e.port.as_str(), e.closeness, e.bound))
        .collect();
    // The control network and the uplink carry no data endpoints.
    assert_eq!(
        got,
        [
            ("ib", "ib0", Closeness::Switch, true),
            ("ib", "ib1", Closeness::Switch, true),
            ("ib", "ib2", Closeness::Far, false),
            ("ib", "ib3", Closeness::Far, false),
        ]
    );
    // A drive on socket 1 ranks the other pair first.
    let eps = rank(&host, "0000:83:00.0", &networks(), &r, 2);
    assert_eq!(eps[0].port, "ib2");
    assert_eq!(eps[2].closeness, Closeness::Far);
}

#[test]
fn endpoints_only_on_ports_storage_may_use() {
    let host = hgx();
    // GPU rails: no endpoints until lent (their role becomes storage's).
    let r = roles(&[
        ("ib0", Role::Compute),
        ("ib1", Role::Vm),
        ("ib2", Role::Converged),
    ]);
    let eps = rank(&host, "0000:03:00.0", &networks(), &r, 2);
    let ports: Vec<&str> = eps.iter().map(|e| e.port.as_str()).collect();
    assert_eq!(ports, ["ib2"]);
    // A host owner giving the bond a host port for storage.
    let r = roles(&[("eth0", Role::Converged)]);
    let eps = rank(&host, "0000:03:00.0", &networks(), &r, 2);
    assert_eq!(eps.len(), 1);
    assert_eq!(
        (eps[0].network.as_str(), eps[0].closeness),
        ("eth100", Closeness::Far)
    );
    assert_eq!(
        eps[0].addr,
        "198.51.100.10".parse::<std::net::IpAddr>().unwrap()
    );
}

#[test]
fn one_port_hosts_use_it_for_everything() {
    let mut host = hgx();
    host.ports.retain(|p| p.name == "mgmt0");
    let mut nets = networks();
    nets[0].kind = NetworkKind::Both;
    let r = roles(&[("mgmt0", Role::Converged)]);
    let eps = rank(&host, "0000:03:00.0", &nets, &r, 2);
    assert_eq!(eps.len(), 1);
    assert!(eps[0].bound);
}

#[test]
fn fabrics_differ_by_subnet_manager() {
    let nets = networks();
    let a = ib_port("a", "0000:04:00.0", "192.0.2.1", 7);
    let b = ib_port("b", "0000:05:00.0", "192.0.2.2", 9);
    assert_ne!(fabric(&a, &nets[1]), fabric(&b, &nets[1]));
    assert_eq!(fabric(&a, &nets[0]), None);
    let mut c = b.clone();
    c.rdma.as_mut().unwrap().subnet_prefix = Some("fe80:0000:0000:0001".into());
    assert_eq!(fabric(&c, &nets[1]), None);
}

fn path(weight: f64, bytes: u64) -> PathLoad {
    PathLoad {
        weight,
        bytes_in_flight: bytes,
        rtt_us: 10,
    }
}

#[test]
fn a_path_without_weight_is_never_chosen() {
    let mut rng = SmallRng::seed_from_u64(1);
    let paths = [path(0.0, 0), path(1.0, 1 << 30), path(0.0, 0)];
    for _ in 0..1000 {
        assert_eq!(choose(&paths, &mut rng), Some(1));
    }
    assert_eq!(choose(&[path(0.0, 0)], &mut rng), None);
    assert_eq!(choose(&[], &mut rng), None);
}

#[test]
fn equal_load_follows_weights() {
    let mut rng = SmallRng::seed_from_u64(2);
    let paths = [path(1.0, 0), path(3.0, 0)];
    let mut n = [0u32; 2];
    for _ in 0..20_000 {
        n[choose(&paths, &mut rng).unwrap()] += 1;
    }
    // Ties keep the first draw, so with equal load the choice follows the
    // weights.
    let share = n[0] as f64 / 20_000.0;
    assert!((share - 0.25).abs() < 0.01, "{share}");
}

/// Many senders choosing at once, each request adding to its path's bytes
/// in flight and some completing: load stays spread, with no herd on one
/// path.
#[test]
fn load_spreads_without_herding() {
    let mut rng = SmallRng::seed_from_u64(3);
    let mut paths = vec![path(1.0, 0); 8];
    for i in 0..100_000 {
        let p = choose(&paths, &mut rng).unwrap();
        paths[p].bytes_in_flight += 128 << 10;
        if i % 2 == 0 {
            // A completion on a random busy path.
            let q = rng.random_range(0..paths.len());
            paths[q].bytes_in_flight = paths[q].bytes_in_flight.saturating_sub(256 << 10);
        }
    }
    let max = paths.iter().map(|p| p.bytes_in_flight).max().unwrap() as f64;
    let mean = paths.iter().map(|p| p.bytes_in_flight).sum::<u64>() as f64 / 8.0;
    assert!(max <= 1.5 * mean, "{paths:?}");
}

const GBPS_200: u64 = 25_000_000_000;

fn obs(role: Role, up: bool, other: u64) -> Observed {
    Observed {
        role,
        up,
        speed: GBPS_200,
        other,
    }
}

/// What the reconciler sees in one pass: ports and claims.
type Sample = (BTreeMap<String, Observed>, BTreeMap<String, Claim>);

/// Runs the reconciler every 10 s over `samples`; returns each step's
/// changes.
fn run(policy: &Policy, samples: &[Sample]) -> (Vec<Vec<Change>>, BTreeMap<String, PortState>) {
    let mut state = BTreeMap::new();
    let mut all = Vec::new();
    for (i, (o, c)) in samples.iter().enumerate() {
        let now = i as u64 * 10_000;
        let ch = reconcile(now, policy, o, c, &state);
        apply(&mut state, &ch);
        all.push(ch);
    }
    (all, state)
}

fn one(port: &str, o: Observed) -> BTreeMap<String, Observed> {
    [(port.to_string(), o)].into()
}

#[test]
fn an_idle_rail_is_lent_after_the_dwell_and_revoked_on_traffic() {
    let policy = Policy::default();
    let idle = one("h/ib0", obs(Role::Compute, true, 0));
    let busy = one("h/ib0", obs(Role::Compute, true, GBPS_200 / 2));
    let mut samples = vec![(idle.clone(), BTreeMap::new()); 8];
    samples.extend(vec![(busy, BTreeMap::new()); 5]);
    let (steps, state) = run(&policy, &samples);
    let lend_at = steps
        .iter()
        .position(|s| s.iter().any(|c| matches!(c, Change::Lend { .. })))
        .unwrap();
    // Idle from 0 s, lent once 60 s have passed.
    assert_eq!(lend_at, 6);
    assert!(
        steps[6]
            .iter()
            .any(|c| matches!(c, Change::Weight { weight, .. } if *weight > 0.0))
    );
    // Traffic at 80 s: revoked at once, out of use at once, back 30 s on.
    assert!(
        steps[8]
            .iter()
            .any(|c| matches!(c, Change::Revoke { until: 110_000, .. }))
    );
    assert!(
        steps[8]
            .iter()
            .any(|c| matches!(c, Change::Weight { weight, .. } if *weight == 0.0))
    );
    assert!(
        steps[11]
            .iter()
            .any(|c| matches!(c, Change::Return { at: 110_000, .. }))
    );
    assert!(!state["h/ib0"].lent && state["h/ib0"].weight == 0.0);
}

#[test]
fn a_claim_takes_a_rail_back_and_keeps_it() {
    let policy = Policy {
        lending: Lending::Always,
        ..Policy::default()
    };
    let o = one("h/ib0", obs(Role::Compute, true, 0));
    let claim: BTreeMap<String, Claim> = [(
        "h/ib0".to_string(),
        Claim {
            holder: "glidex".into(),
            exclusive: true,
            expires: u64::MAX,
        },
    )]
    .into();
    let mut samples = vec![(o.clone(), BTreeMap::new()); 2];
    samples.extend(vec![(o, claim); 10]);
    let (steps, state) = run(&policy, &samples);
    assert!(steps[0].iter().any(|c| matches!(c, Change::Lend { .. })));
    assert!(steps[2].iter().any(|c| matches!(c, Change::Revoke { .. })));
    assert!(
        steps[3..]
            .iter()
            .flatten()
            .all(|c| !matches!(c, Change::Lend { .. }))
    );
    assert!(!state["h/ib0"].lent);
}

#[test]
fn lending_off_never_lends() {
    let policy = Policy {
        lending: Lending::Off,
        ..Policy::default()
    };
    let samples = vec![(one("h/ib0", obs(Role::Compute, true, 0)), BTreeMap::new()); 30];
    let (steps, state) = run(&policy, &samples);
    assert!(
        steps
            .iter()
            .flatten()
            .all(|c| !matches!(c, Change::Lend { .. }))
    );
    assert_eq!(state["h/ib0"].weight, 0.0);
}

/// Random ports, roles, traffic, links and claims over many steps. After
/// every step: a port under a live exclusive claim, down, or never
/// storage's has weight 0; nothing is lent under `off` or while claimed;
/// a lend comes only after a dwell of idleness and of quiet since the last
/// change; a revoked port returns by its deadline; weights move only past
/// the hysteresis.
#[test]
fn reconciler_invariants_hold_under_random_load() {
    let roles = [
        Role::Host,
        Role::Uplink,
        Role::Storage,
        Role::Compute,
        Role::Compute,
        Role::Converged,
        Role::Vm,
    ];
    for seed in 0..200u64 {
        let mut rng = SmallRng::seed_from_u64(seed);
        let lending = [Lending::Off, Lending::Idle, Lending::Always][seed as usize % 3];
        let policy = Policy {
            lending,
            ..Policy::default()
        };
        let ports: Vec<(String, Role)> = (0..6)
            .map(|i| (format!("h/p{i}"), roles[rng.random_range(0..roles.len())]))
            .collect();
        let mut state: BTreeMap<String, PortState> = BTreeMap::new();
        let mut claims: BTreeMap<String, Claim> = BTreeMap::new();
        let mut busy: BTreeMap<String, bool> = BTreeMap::new();
        for step in 0..300u64 {
            let now = step * 10_000;
            let mut observed = BTreeMap::new();
            for (p, role) in &ports {
                let b = busy.entry(p.clone()).or_default();
                if rng.random_bool(0.1) {
                    *b = !*b;
                }
                let other = if *b {
                    rng.random_range(GBPS_200 / 10..GBPS_200)
                } else {
                    rng.random_range(0..GBPS_200 / 40)
                };
                observed.insert(p.clone(), obs(*role, rng.random_bool(0.97), other));
                if rng.random_bool(0.03) {
                    claims.insert(
                        p.clone(),
                        Claim {
                            holder: "o".into(),
                            exclusive: rng.random_bool(0.7),
                            expires: now + rng.random_range(0..200_000),
                        },
                    );
                }
            }
            let before = state.clone();
            let changes = reconcile(now, &policy, &observed, &claims, &state);
            apply(&mut state, &changes);
            for (p, o) in &observed {
                let s = &state[p];
                let live = claims.get(p).filter(|c| c.expires > now);
                let ctx = format!("seed {seed} step {step} {p} {o:?} {s:?} {live:?}");
                if !o.up || live.is_some_and(|c| c.exclusive) {
                    assert_eq!(s.weight, 0.0, "{ctx}");
                }
                if !(o.role.storage_may_use() || o.role == Role::Compute) {
                    assert_eq!(s.weight, 0.0, "{ctx}");
                }
                if o.role == Role::Compute && !s.lent {
                    assert_eq!(s.weight, 0.0, "{ctx}");
                }
                if let Some(until) = s.revoking {
                    assert_eq!(s.weight, 0.0, "{ctx}");
                    assert!(until + 10_000 > now, "{ctx}");
                }
                let b = before.get(p).cloned().unwrap_or_default();
                for c in changes.iter().filter(|c| match c {
                    Change::Lend { port, .. } | Change::Weight { port, .. } => port == p,
                    _ => false,
                }) {
                    match c {
                        Change::Lend { .. } => {
                            assert_ne!(lending, Lending::Off, "{ctx}");
                            assert!(live.is_none(), "{ctx}");
                            assert!(
                                b.last_change.is_none_or(|t| now - t >= policy.dwell_ms),
                                "{ctx}"
                            );
                            if lending == Lending::Idle {
                                let since = s.idle_since.expect("idle");
                                assert!(now - since >= policy.dwell_ms, "{ctx}");
                            }
                        }
                        Change::Weight { weight, .. } => {
                            if b.weight > 0.0 && *weight > 0.0 {
                                let d = ((weight - b.weight) / b.weight).abs();
                                assert!(d > policy.hysteresis, "{ctx}");
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }
}

#[test]
fn addresses_are_matched_by_subnet() {
    use crate::model::in_subnet;
    let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
    assert!(in_subnet(ip("192.0.2.77"), ip("192.0.2.0"), 24));
    assert!(!in_subnet(ip("192.0.3.1"), ip("192.0.2.0"), 24));
    assert!(in_subnet(ip("2001:db8::1"), ip("2001:db8::"), 32));
    assert!(!in_subnet(ip("192.0.2.1"), ip("2001:db8::"), 32));
    assert!(in_subnet(ip("10.1.2.3"), ip("0.0.0.0"), 0));
}

#[test]
fn several_ports_in_one_subnet_need_rules_by_source() {
    use crate::routing::unrouted;
    let host = hgx();
    let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
    let eps = [ip("192.0.2.10"), ip("192.0.2.11"), ip("203.0.113.10")];
    // Four InfiniBand ports in one subnet, no rules: both endpoints there
    // need one; the management port is alone in its subnet.
    assert_eq!(unrouted(&host.ports, &eps, ""), vec![eps[0], eps[1]]);
    let rules = "0:\tfrom all lookup local\n100:\tfrom 192.0.2.10 lookup 100\n\
                 32766:\tfrom all lookup main\n";
    assert_eq!(unrouted(&host.ports, &eps, rules), vec![eps[1]]);
    // A rule for 192.0.2.1 doesn't cover 192.0.2.10's prefix match.
    let rules = "100:\tfrom 192.0.2.1 lookup 100\n";
    assert_eq!(unrouted(&host.ports, &eps[..1], rules), vec![eps[0]]);
    // One port per subnet: nothing needed.
    let mut one = host.ports.clone();
    one.retain(|p| p.name != "ib1" && p.name != "ib2" && p.name != "ib3");
    assert!(unrouted(&one, &eps, "").is_empty());
    // An address this host doesn't have: not this host's to route.
    assert!(unrouted(&host.ports, &[ip("198.51.100.99")], "").is_empty());
}
