//! Placement (spec/07-placement.md §5, D9, D21).
//!
//! - A chunk maps to a PG by a stable hash of its lineage and index
//!   ([`pg_of`]).
//! - A PG maps to an ordered list of devices by weighted rendezvous hashing
//!   over failure domains ([`place`]): the first entry is the preferred
//!   primary (replicated pools) or shard 0 (EC pools).
//!
//! Everything here is a pure function of its inputs and must never change
//! for a given format version: every node and client computes the same
//! placement from the same map. The hash is spelled out below rather than
//! taken from `std`, whose hashers may change between Rust releases.

/// SplitMix64's finalizer: a bijective mix of a 64-bit word.
#[inline]
pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A stable 64-bit hash of a sequence of words.
#[inline]
pub fn hash_words(words: &[u64]) -> u64 {
    let mut h = 0x9e37_79b9_7f4a_7c15u64;
    for &w in words {
        h = mix64(h ^ w).wrapping_add(0x9e37_79b9_7f4a_7c15);
    }
    mix64(h)
}

/// A stable 64-bit hash of bytes (FNV-1a, then mixed).
pub fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix64(h)
}

/// The PG of chunk `chunk` of a lineage (D21). `pg_count` must be a power
/// of two, so that doubling it later splits each PG in two (07-placement.md §5.1).
pub fn pg_of(lineage: u128, chunk: u64, pg_count: u32) -> u32 {
    assert!(
        pg_count.is_power_of_two(),
        "pg_count must be a power of two"
    );
    let h = hash_words(&[lineage as u64, (lineage >> 64) as u64, chunk]);
    (h & (pg_count as u64 - 1)) as u32
}

/// The PG of a named object (S3 data, `.meta` headers, KV ranges).
pub fn pg_of_name(name: &[u8], pg_count: u32) -> u32 {
    assert!(
        pg_count.is_power_of_two(),
        "pg_count must be a power of two"
    );
    (hash_bytes(name) & (pg_count as u64 - 1)) as u32
}

/// A device that can hold PG copies.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: u32,
    pub host: u32,
    pub rack: u32,
    /// Relative capacity; 0 takes nothing.
    pub weight: f64,
    pub class: String,
    /// `in` devices take placements; `out` ones don't.
    pub up_in: bool,
    /// A rotational disk, which a rule with `no_hdd` skips.
    pub hdd: bool,
}

/// The level whose members must be distinct among a PG's copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Device,
    Host,
    Rack,
}

/// How a pool places its PGs.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub pool: u32,
    /// Copies (replicated) or `k + m` (EC).
    pub size: usize,
    pub domain: Domain,
    /// Only devices whose class is one of these (D164); empty: every
    /// device, for pools made before class sets (D171).
    pub classes: Vec<String>,
    /// No rotational disks (`.meta`, 07-placement.md §5.2).
    pub no_hdd: bool,
    /// Devices added to the pool beyond its classes, and devices removed
    /// from it (D180).
    pub added: Vec<u32>,
    pub removed: Vec<u32>,
}

/// Uniform in (0, 1], from a hash.
#[inline]
fn unit(h: u64) -> f64 {
    ((h >> 11) as f64 + 1.0) / (1u64 << 53) as f64
}

/// Weighted rendezvous score: the lowest wins. With weights w_i, candidate
/// i wins with probability w_i / Σw (an exponential race).
#[inline]
fn score(h: u64, weight: f64) -> f64 {
    -unit(h).ln() / weight
}

fn domain_of(d: &Device, level: Domain) -> u64 {
    match level {
        Domain::Device => d.id as u64,
        Domain::Host => d.host as u64,
        Domain::Rack => d.rack as u64,
    }
}

/// Salts that keep the hashes of the different choices independent.
const SALT_RACK: u64 = 0x5241_434b;
const SALT_HOST: u64 = 0x484f_5354;
const SALT_DEV: u64 = 0x4445_5600;

/// Places PG `pg` of `rule.pool` on up to `rule.size` devices, in order,
/// each in a distinct failure domain at `rule.domain`. Returns fewer if
/// there aren't enough domains with eligible devices.
///
/// Choices are hierarchical: distinct domains at the rule's level are
/// chosen by weight, then one host within each chosen rack (by weight),
/// then one device within each host. A device's weight therefore counts
/// for its host and rack, and changing one device moves only the PGs whose
/// choices it wins or loses.
pub fn place(rule: &Rule, pg: u32, devices: &[Device]) -> Vec<u32> {
    Topology::new(rule, devices).place(pg)
}

/// A rule's view of the devices, built once per map so that placing each
/// PG costs time in the number of domains, not devices times domains.
#[derive(Debug, Clone)]
pub struct Topology {
    pool: u32,
    size: usize,
    salt: u64,
    domains: Vec<Dom>,
}

#[derive(Debug, Clone)]
struct Dom {
    id: u64,
    weight: f64,
    /// Hosts in the domain (one, unless the domain is a rack).
    hosts: Vec<HostDevs>,
}

#[derive(Debug, Clone)]
struct HostDevs {
    id: u32,
    weight: f64,
    devices: Vec<(u32, f64)>,
}

impl Topology {
    pub fn new(rule: &Rule, devices: &[Device]) -> Self {
        use std::collections::BTreeMap;
        // domain -> host -> devices, in a stable order.
        let mut tree: BTreeMap<u64, BTreeMap<u32, Vec<(u32, f64)>>> = BTreeMap::new();
        for d in devices.iter().filter(|d| {
            d.up_in
                && d.weight > 0.0
                && !rule.removed.contains(&d.id)
                && (rule.classes.is_empty()
                    || rule.classes.contains(&d.class)
                    || rule.added.contains(&d.id))
                && !(rule.no_hdd && d.hdd)
        }) {
            tree.entry(domain_of(d, rule.domain))
                .or_default()
                .entry(d.host)
                .or_default()
                .push((d.id, d.weight));
        }
        let domains = tree
            .into_iter()
            .map(|(id, hosts)| {
                let hosts: Vec<HostDevs> = hosts
                    .into_iter()
                    .map(|(h, devs)| HostDevs {
                        id: h,
                        weight: devs.iter().map(|d| d.1).sum(),
                        devices: devs,
                    })
                    .collect();
                Dom {
                    id,
                    weight: hosts.iter().map(|h| h.weight).sum(),
                    hosts,
                }
            })
            .collect();
        Self {
            pool: rule.pool,
            size: rule.size,
            salt: match rule.domain {
                Domain::Rack => SALT_RACK,
                Domain::Host => SALT_HOST,
                Domain::Device => SALT_DEV,
            },
            domains,
        }
    }

    /// See [`place`].
    pub fn place(&self, pg: u32) -> Vec<u32> {
        let (p, g) = (self.pool as u64, pg as u64);
        let mut ranked: Vec<(f64, usize)> = self
            .domains
            .iter()
            .enumerate()
            .map(|(i, d)| (score(hash_words(&[p, g, self.salt, d.id]), d.weight), i))
            .collect();
        let n = self.size.min(ranked.len());
        let by = |a: &(f64, usize), b: &(f64, usize)| {
            a.0.total_cmp(&b.0)
                .then(self.domains[a.1].id.cmp(&self.domains[b.1].id))
        };
        if n < ranked.len() && n > 0 {
            ranked.select_nth_unstable_by(n - 1, by);
        }
        ranked.truncate(n);
        ranked.sort_by(by);
        ranked
            .into_iter()
            .map(|(_, i)| {
                let dom = &self.domains[i];
                let host = if dom.hosts.len() == 1 {
                    &dom.hosts[0]
                } else {
                    dom.hosts
                        .iter()
                        .map(|h| {
                            (
                                score(hash_words(&[p, g, SALT_HOST, h.id as u64]), h.weight),
                                h,
                            )
                        })
                        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.id.cmp(&b.1.id)))
                        .unwrap()
                        .1
                };
                host.devices
                    .iter()
                    .map(|&(id, w)| (score(hash_words(&[p, g, SALT_DEV, id as u64]), w), id))
                    .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
                    .unwrap()
                    .1
            })
            .collect()
    }
}

/// Assigns shard positions for an erasure-coded PG (07-placement.md §5.2, §5.3).
///
/// [`place`] decides which devices hold the PG; this decides which shard
/// each one holds. Every member of `prev` (the PG's current acting set,
/// position = shard index) that is still in `members` keeps its position,
/// so its data doesn't move; new members fill the vacated positions in
/// rank order. Positions that can't be filled are `None`. For a new PG,
/// pass a `prev` of `None`s and the members get positions in rank order.
///
/// A stateless positional choice (CRUSH's `indep`) moved about twice the
/// necessary shards in our tests, because each changed position can
/// collide with others; keeping positions in the PG table costs nothing,
/// since the metadata service keeps acting sets anyway.
pub fn assign_positions(prev: &[Option<u32>], members: &[u32]) -> Vec<Option<u32>> {
    let mut out: Vec<Option<u32>> = prev
        .iter()
        .map(|p| p.filter(|d| members.contains(d)))
        .collect();
    let fresh: Vec<u32> = members
        .iter()
        .copied()
        .filter(|m| !out.contains(&Some(*m)))
        .collect();
    let mut fresh = fresh.into_iter();
    for slot in out.iter_mut() {
        if slot.is_none() {
            *slot = fresh.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn cluster(racks: u32, hosts_per_rack: u32, devs_per_host: u32) -> Vec<Device> {
        let mut v = Vec::new();
        let mut id = 0;
        for r in 0..racks {
            for h in 0..hosts_per_rack {
                for _ in 0..devs_per_host {
                    v.push(Device {
                        id,
                        host: r * 100 + h,
                        rack: r,
                        weight: 1.0,
                        class: "nvme".into(),
                        hdd: false,
                        up_in: true,
                    });
                    id += 1;
                }
            }
        }
        v
    }

    fn rule(size: usize, domain: Domain) -> Rule {
        Rule {
            pool: 1,
            size,
            domain,
            classes: Vec::new(),
            no_hdd: false,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    #[test]
    fn hashes_are_stable() {
        // Pinned values: placement must never change for a format version.
        assert_eq!(mix64(0), 0);
        assert_eq!(hash_words(&[1, 2, 3]), 0x7c26_edc8_f54b_2af2);
        assert_eq!(hash_bytes(b"hyperstor"), 0x6e14_92ca_ff53_f0fc);
        assert_eq!(pg_of(42, 7, 1024), 835);
    }

    #[test]
    fn pg_split_keeps_chunks_together() {
        for c in 0..10_000u64 {
            let a = pg_of(99, c, 256);
            let b = pg_of(99, c, 512);
            assert!(b == a || b == a + 256, "chunk {c}: {a} -> {b}");
        }
    }

    #[test]
    fn distinct_domains_and_determinism() {
        let devs = cluster(4, 3, 2);
        for domain in [Domain::Device, Domain::Host, Domain::Rack] {
            for pg in 0..500 {
                let p = place(&rule(3, domain), pg, &devs);
                assert_eq!(p, place(&rule(3, domain), pg, &devs));
                assert_eq!(p.len(), 3);
                let doms: HashSet<u64> = p
                    .iter()
                    .map(|id| domain_of(&devs[*id as usize], domain))
                    .collect();
                assert_eq!(doms.len(), 3, "{domain:?} pg {pg}: {p:?}");
            }
        }
    }

    #[test]
    fn too_few_domains_gives_fewer_copies() {
        let devs = cluster(2, 2, 2);
        assert_eq!(place(&rule(3, Domain::Rack), 5, &devs).len(), 2);
        assert_eq!(place(&rule(3, Domain::Host), 5, &devs).len(), 3);
    }

    #[test]
    fn load_follows_weight() {
        let mut devs = cluster(1, 8, 1);
        devs[0].weight = 2.0; // twice the capacity
        let mut count: HashMap<u32, usize> = HashMap::new();
        let pgs = 40_000;
        for pg in 0..pgs {
            for d in place(&rule(1, Domain::Host), pg, &devs) {
                *count.entry(d).or_default() += 1;
            }
        }
        let total_w: f64 = devs.iter().map(|d| d.weight).sum();
        for d in &devs {
            let want = pgs as f64 * d.weight / total_w;
            let got = count[&d.id] as f64;
            assert!(
                (got - want).abs() / want < 0.05,
                "device {}: {got} vs {want}",
                d.id
            );
        }
    }

    #[test]
    fn three_copies_spread_evenly() {
        let devs = cluster(3, 4, 4);
        let mut count: HashMap<u32, usize> = HashMap::new();
        let pgs = 8192;
        for pg in 0..pgs {
            for d in place(&rule(3, Domain::Rack), pg, &devs) {
                *count.entry(d).or_default() += 1;
            }
        }
        let mean = (pgs * 3) as f64 / devs.len() as f64;
        for (d, c) in &count {
            assert!(
                (*c as f64 - mean).abs() / mean < 0.15,
                "device {d}: {c} vs {mean}"
            );
        }
    }

    #[test]
    fn adding_a_host_moves_only_its_share() {
        let devs = cluster(1, 9, 2);
        let mut more = devs.clone();
        for i in 0..2 {
            more.push(Device {
                id: 1000 + i,
                host: 999,
                rack: 0,
                weight: 1.0,
                class: "nvme".into(),
                hdd: false,
                up_in: true,
            });
        }
        let pgs = 20_000u32;
        let mut moved = 0usize;
        let mut wrongly = 0usize;
        for pg in 0..pgs {
            let a = place(&rule(3, Domain::Host), pg, &devs);
            let b = place(&rule(3, Domain::Host), pg, &more);
            let sa: HashSet<_> = a.iter().collect();
            for d in &b {
                if !sa.contains(d) {
                    moved += 1;
                    if *d < 1000 {
                        wrongly += 1; // a copy moved between old devices
                    }
                }
            }
        }
        // The new host should take its share, 3/10 of copies per PG...
        let frac = moved as f64 / (pgs as f64 * 3.0);
        assert!((frac - 0.1).abs() < 0.02, "moved {frac}");
        // ...and nothing should shuffle between the old ones.
        assert_eq!(wrongly, 0);
    }

    #[test]
    fn removing_a_device_moves_only_pgs_on_its_host() {
        // Device 4 is on host 1 with devices 3 and 5. Removing it lowers
        // host 1's weight, so some PGs that used host 1 through 3 or 5 move
        // off it too (that keeps load proportional to capacity). PGs that
        // never used host 1 must not change at all, and a PG that changes
        // keeps its copies on other hosts.
        let devs = cluster(1, 6, 3);
        let mut less = devs.clone();
        less[4].up_in = false;
        let host = devs[4].host;
        let mut moved = 0;
        for pg in 0..10_000u32 {
            let a = place(&rule(3, Domain::Host), pg, &devs);
            let b = place(&rule(3, Domain::Host), pg, &less);
            assert!(!b.contains(&4));
            let sa: HashSet<&u32> = a.iter().collect();
            let sb: HashSet<&u32> = b.iter().collect();
            if a.iter().all(|d| devs[*d as usize].host != host) {
                assert_eq!(a, b, "pg {pg} changed without involving host {host}");
            } else if sa != sb {
                moved += 1;
                let kept = a.iter().filter(|d| b.contains(d)).count();
                assert!(kept >= 2, "pg {pg}: {a:?} -> {b:?}");
            }
        }
        // The least movement possible: every PG that held device 4 changes,
        // and host 1 must drop out of enough PGs to fall to its new share.
        // Order changes alone don't count: they move no data.
        let on = |set: &dyn Fn(u32) -> Vec<u32>, f: &dyn Fn(u32) -> bool| {
            (0..10_000u32)
                .filter(|&pg| set(pg).iter().any(|&d| f(d)))
                .count()
        };
        let before = |pg| place(&rule(3, Domain::Host), pg, &devs);
        let after = |pg| place(&rule(3, Domain::Host), pg, &less);
        let on_dev4 = on(&before, &|d| d == 4);
        let host_before = on(&before, &|d| devs[d as usize].host == host);
        let host_after = on(&after, &|d| devs[d as usize].host == host);
        let minimal = on_dev4.max(host_before.saturating_sub(host_after));
        let ratio = moved as f64 / minimal as f64;
        eprintln!("replicated: moved {moved} PGs, minimal {minimal}, ratio {ratio:.2}");
        assert!(ratio <= 1.6, "moved {moved}, minimal {minimal}");
    }

    #[test]
    fn ec_positions_are_stable() {
        let devs = cluster(1, 10, 2);
        let r = rule(6, Domain::Host); // 4+2
        let mut less = devs.clone();
        less[4].up_in = false; // host 2 loses one of its two devices
        let mut changed = 0usize;
        let mut on_dev4 = 0usize;
        let mut minimal = 0usize;
        for pg in 0..5000u32 {
            let a = assign_positions(&[None; 6], &place(&r, pg, &devs));
            assert!(a.iter().all(Option::is_some));
            let b = assign_positions(&a, &place(&r, pg, &less));
            // Members kept by the new set keep their shard index.
            for (x, y) in a.iter().zip(&b) {
                if x != y {
                    changed += 1;
                    assert!(!place(&r, pg, &less).contains(&x.unwrap()));
                }
            }
            on_dev4 += a.iter().filter(|d| **d == Some(4)).count();
            let new: HashSet<u32> = place(&r, pg, &less).into_iter().collect();
            minimal += a.iter().filter(|d| !new.contains(&d.unwrap())).count();
        }
        // Every changed shard is one whose device left the set: the least
        // movement for this membership change.
        assert_eq!(changed, minimal);
        eprintln!("ec 4+2: moved {changed} of 30000 shards ({on_dev4} were on the removed device)");
        let short = assign_positions(&[Some(1), Some(2), Some(3)], &[3, 7]);
        assert_eq!(short, vec![Some(7), None, Some(3)]);
    }

    /// D164: a rule with several classes places on any device of them,
    /// and only them; `no_hdd` skips rotational disks (`.meta`).
    #[test]
    fn class_sets_and_no_hdd() {
        let mut devs = cluster(1, 4, 3);
        for d in devs.iter_mut() {
            d.class = ["nvme", "ssd", "hdd"][d.id as usize % 3].into();
            d.hdd = d.class == "hdd";
        }
        let set = Rule {
            pool: 2,
            size: 3,
            domain: Domain::Host,
            classes: vec!["nvme".into(), "ssd".into()],
            no_hdd: false,
            added: Vec::new(),
            removed: Vec::new(),
        };
        let mut seen = HashSet::new();
        for pg in 0..500 {
            for d in place(&set, pg, &devs) {
                assert_ne!(d % 3, 2, "no hdd class");
                seen.insert(d % 3);
            }
        }
        assert_eq!(seen.len(), 2, "both classes used");
        // D180: a device added beyond the classes, one of them removed.
        let edited = Rule {
            added: vec![2],
            removed: vec![0],
            ..set.clone()
        };
        let mut used = HashSet::new();
        for pg in 0..500 {
            used.extend(place(&edited, pg, &devs));
        }
        assert!(used.contains(&2) && !used.contains(&0));
        let meta = Rule {
            classes: Vec::new(),
            no_hdd: true,
            added: Vec::new(),
            removed: Vec::new(),
            ..set
        };
        for pg in 0..500 {
            assert!(place(&meta, pg, &devs).iter().all(|d| d % 3 != 2));
        }
    }

    #[test]
    fn class_filter_and_out_devices() {
        let mut devs = cluster(1, 4, 2);
        for d in devs.iter_mut().filter(|d| d.id % 2 == 1) {
            d.class = "hdd".into();
        }
        let r = Rule {
            pool: 2,
            size: 3,
            domain: Domain::Host,
            classes: vec!["hdd".into()],
            no_hdd: false,
            added: Vec::new(),
            removed: Vec::new(),
        };
        for pg in 0..200 {
            for d in place(&r, pg, &devs) {
                assert_eq!(d % 2, 1);
            }
        }
        for d in devs.iter_mut() {
            d.up_in = false;
        }
        assert!(place(&r, 1, &devs).is_empty());
    }
}
