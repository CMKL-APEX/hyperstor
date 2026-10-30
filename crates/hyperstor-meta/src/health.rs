//! Cluster health (spec/15-operations.md §16): `error` when some PG can't
//! take writes, `warn` while anything is down, degraded or backfilling,
//! `ok` otherwise, each with its reasons. Computed from one map, so the
//! cluster API reports what clients and storage nodes see.

use crate::{ClusterMap, PoolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    Ok,
    Warn,
    Error,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Warn => "warn",
            Health::Error => "error",
        }
    }
}

/// PG states of one pool. A PG is counted once, under its worst state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolPgs {
    pub pool: u32,
    pub total: u32,
    /// Every copy in `acting`, nothing to backfill.
    pub clean: u32,
    /// Fewer copies than `size`, but at least `min_size`: writes go on.
    pub degraded: u32,
    /// Copies being backfilled onto new members of `up`.
    pub backfilling: u32,
    /// Fewer than `min_size` copies: writes stop (no primary, or too few).
    pub inactive: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub health: Health,
    pub reasons: Vec<(Health, String)>,
    pub pools: Vec<PoolPgs>,
    pub nodes_up: usize,
    pub devices_in: usize,
}

/// Health of `map`: `error` when some PG can't take writes, `warn` while
/// anything is down, degraded or backfilling, `ok` otherwise.
pub fn report(map: &ClusterMap) -> Report {
    let mut reasons = Vec::new();
    let nodes_up = map.nodes.values().filter(|n| n.up).count();
    let devices_in = map.devices.values().filter(|d| d.up_in).count();
    let down = map.nodes.len() - nodes_up;
    if down > 0 {
        let ids: Vec<String> = map
            .nodes
            .values()
            .filter(|n| !n.up)
            .map(|n| n.id.to_string())
            .collect();
        reasons.push((
            Health::Warn,
            format!("{down} node(s) down: {}", ids.join(", ")),
        ));
    }
    let out = map.devices.len() - devices_in;
    if out > 0 {
        reasons.push((Health::Warn, format!("{out} device(s) down or out")));
    }
    let mut pools = Vec::new();
    for p in map.pools.values() {
        let mut s = PoolPgs {
            pool: p.id,
            ..Default::default()
        };
        // An EC PG needs k shards to read; min_size is what writes need.
        for ((pool, _), pg) in map.pgs.range((p.id, 0)..=(p.id, u32::MAX)) {
            debug_assert_eq!(*pool, p.id);
            s.total += 1;
            let have = pg.acting_count();
            if pg.primary().is_none() || have < p.min_size as usize {
                s.inactive += 1;
            } else if !map.filling(pg).is_empty() {
                s.backfilling += 1;
            } else if have < p.size as usize {
                s.degraded += 1;
            } else {
                s.clean += 1;
            }
        }
        if s.inactive > 0 {
            let what = if p.kind == PoolKind::Cache {
                // Cache pools lose data instead of recovering it (D45).
                "without copies (cache data lost until rewritten)"
            } else {
                "below min_size, not taking writes"
            };
            reasons.push((
                Health::Error,
                format!("pool {}: {} PG(s) {what}", p.name, s.inactive),
            ));
        }
        if s.degraded > 0 {
            reasons.push((
                Health::Warn,
                format!("pool {}: {} PG(s) degraded", p.name, s.degraded),
            ));
        }
        if s.backfilling > 0 {
            reasons.push((
                Health::Warn,
                format!("pool {}: {} PG(s) backfilling", p.name, s.backfilling),
            ));
        }
        pools.push(s);
    }
    let health = reasons.iter().map(|(h, _)| *h).max().unwrap_or(Health::Ok);
    Report {
        health,
        reasons,
        pools,
        nodes_up,
        devices_in,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool's counts by id (`.meta` comes first).
    fn pool(r: &Report, id: u32) -> &PoolPgs {
        r.pools.iter().find(|p| p.pool == id).unwrap()
    }

    fn map() -> ClusterMap {
        crate::parse_cluster(
            "cluster 9\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             node 2 rack 0 127.0.0.1:9102\npool 1 vol replicated size 3 min 2 pgs 8 domain host\n",
        )
        .unwrap()
    }

    #[test]
    fn healthy_cluster_is_ok() {
        let m = map();
        let r = report(&m);
        assert_eq!(r.health, Health::Ok, "{:?}", r.reasons);
        assert_eq!(pool(&r, 1).clean, 8);
        assert_eq!(r.nodes_up, 3);
    }

    #[test]
    fn a_down_node_degrades_and_warns() {
        let mut m = map();
        m.nodes.get_mut(&2).unwrap().up = false;
        for d in m.devices.values_mut().filter(|d| d.node == 2) {
            d.up_in = false;
        }
        m.recompute();
        let r = report(&m);
        assert_eq!(r.health, Health::Warn, "{:?}", r.reasons);
        assert!(r.reasons.iter().any(|(_, w)| w.contains("node(s) down: 2")));
        // With 3 copies over 3 hosts and one host gone, every PG keeps 2.
        assert_eq!(pool(&r, 1).degraded + pool(&r, 1).backfilling, 8);
        assert_eq!(pool(&r, 1).inactive, 0);
    }

    #[test]
    fn below_min_size_is_an_error() {
        let mut m = map();
        let pg = m.pgs.get_mut(&(1, 3)).unwrap();
        pg.acting = vec![pg.acting[0]];
        pg.up = pg.acting.clone();
        let r = report(&m);
        assert_eq!(r.health, Health::Error);
        assert_eq!(pool(&r, 1).inactive, 1);
        assert_eq!(pool(&r, 1).clean, 7);
        // No primary at all is inactive too.
        let pg = m.pgs.get_mut(&(1, 4)).unwrap();
        pg.acting[0] = crate::NO_DEVICE;
        assert_eq!(pool(&report(&m), 1).inactive, 2);
    }
}
