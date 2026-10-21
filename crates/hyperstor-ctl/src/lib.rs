//! What `hyperstorctl` reports about a cluster map: health with its
//! reasons (spec/operations.md §16) and per-pool PG states, as text for
//! people and JSON for scripts (spec/administration.md §23.6).
//!
//! Everything here is computed from one published map, so it shows what
//! clients and storage nodes see, not the map service's working map.

use hyperstor_meta::{ClusterMap, PoolKind};
use hyperstor_placement::Domain;
use std::fmt::Write;

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
            } else if pg.backfilling().next().is_some() {
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

fn kind(k: PoolKind) -> String {
    match k {
        PoolKind::Replicated => "replicated".into(),
        PoolKind::Ec { k, m } => format!("ec{k}+{m}"),
        PoolKind::Cache => "cache".into(),
    }
}

fn domain(d: Domain) -> &'static str {
    match d {
        Domain::Device => "device",
        Domain::Host => "host",
        Domain::Rack => "rack",
    }
}

/// `hyperstorctl status`, for people.
pub fn status_text(map: &ClusterMap, r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "cluster {}, map epoch {}", map.cluster_id, map.epoch);
    let _ = writeln!(s, "health  {}", r.health.as_str());
    for (h, why) in &r.reasons {
        let _ = writeln!(s, "  {:<5} {why}", h.as_str());
    }
    let _ = writeln!(
        s,
        "nodes   {} up of {}; devices {} in of {}",
        r.nodes_up,
        map.nodes.len(),
        r.devices_in,
        map.devices.len()
    );
    s.push_str(&pools_text(map, r));
    s
}

/// `hyperstorctl pools`.
pub fn pools_text(map: &ClusterMap, r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{:<4} {:<12} {:<10} {:>4} {:>3} {:>5} {:>6} {:>6} {:>8} {:>9} {:>8}",
        "POOL",
        "NAME",
        "KIND",
        "SIZE",
        "MIN",
        "PGS",
        "CHUNK",
        "CLEAN",
        "DEGRADED",
        "BACKFILL",
        "INACTIVE"
    );
    for (p, st) in map.pools.values().zip(&r.pools) {
        let _ = writeln!(
            s,
            "{:<4} {:<12} {:<10} {:>4} {:>3} {:>5} {:>5}K {:>6} {:>8} {:>9} {:>8}",
            p.id,
            p.name,
            kind(p.kind),
            p.size,
            p.min_size,
            p.pg_count,
            p.chunk_size >> 10,
            st.clean,
            st.degraded,
            st.backfilling,
            st.inactive
        );
    }
    s
}

/// `hyperstorctl nodes`: each node and its devices.
pub fn nodes_text(map: &ClusterMap) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{:<5} {:<5} {:<5} {:<7} {:<22} {:<6} STATE",
        "NODE", "RACK", "UP", "DEVICE", "ADDRESS", "WEIGHT"
    );
    for n in map.nodes.values() {
        for d in map.devices.values().filter(|d| d.node == n.id) {
            let _ = writeln!(
                s,
                "{:<5} {:<5} {:<5} {:<7} {:<22} {:<6.2} {}",
                n.id,
                n.rack,
                if n.up { "yes" } else { "no" },
                d.id,
                d.addr.to_string(),
                d.weight,
                if d.up_in { "in" } else { "out" }
            );
        }
    }
    s
}

/// `hyperstorctl pgs`: the acting and up sets of one pool's PGs.
pub fn pgs_text(map: &ClusterMap, pool: u32) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{:<10} {:>6}  {:<24} UP", "PG", "EPOCH", "ACTING");
    let set = |v: &[u32]| {
        let parts: Vec<String> = v
            .iter()
            .map(|d| {
                if *d == hyperstor_meta::NO_DEVICE {
                    "-".to_string()
                } else {
                    d.to_string()
                }
            })
            .collect();
        format!("[{}]", parts.join(","))
    };
    for ((_, pg), info) in map.pgs.range((pool, 0)..=(pool, u32::MAX)) {
        let _ = writeln!(
            s,
            "{:<10} {:>6}  {:<24} {}",
            format!("{pool}.{pg}"),
            info.epoch,
            set(&info.acting),
            set(&info.up)
        );
    }
    s
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// `hyperstorctl status --json`: one object with the same content.
pub fn status_json(map: &ClusterMap, r: &Report) -> String {
    let reasons: Vec<String> = r
        .reasons
        .iter()
        .map(|(h, w)| {
            format!(
                "{{\"level\":\"{}\",\"reason\":{}}}",
                h.as_str(),
                json_str(w)
            )
        })
        .collect();
    let nodes: Vec<String> = map
        .nodes
        .values()
        .map(|n| {
            let devs: Vec<String> = map
                .devices
                .values()
                .filter(|d| d.node == n.id)
                .map(|d| {
                    format!(
                        "{{\"id\":{},\"addr\":\"{}\",\"in\":{}}}",
                        d.id, d.addr, d.up_in
                    )
                })
                .collect();
            format!(
                "{{\"id\":{},\"rack\":{},\"up\":{},\"devices\":[{}]}}",
                n.id,
                n.rack,
                n.up,
                devs.join(",")
            )
        })
        .collect();
    let pools: Vec<String> = map
        .pools
        .values()
        .zip(&r.pools)
        .map(|(p, s)| {
            format!(
                "{{\"id\":{},\"name\":{},\"kind\":\"{}\",\"size\":{},\"min_size\":{},\"pg_count\":{},\"chunk_size\":{},\"domain\":\"{}\",\"pgs\":{{\"clean\":{},\"degraded\":{},\"backfilling\":{},\"inactive\":{}}}}}",
                p.id,
                json_str(&p.name),
                kind(p.kind),
                p.size,
                p.min_size,
                p.pg_count,
                p.chunk_size,
                domain(p.domain),
                s.clean,
                s.degraded,
                s.backfilling,
                s.inactive
            )
        })
        .collect();
    format!(
        "{{\"cluster\":{},\"epoch\":{},\"health\":\"{}\",\"reasons\":[{}],\"nodes\":[{}],\"pools\":[{}]}}",
        map.cluster_id,
        map.epoch,
        r.health.as_str(),
        reasons.join(","),
        nodes.join(","),
        pools.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> ClusterMap {
        hyperstor_meta::parse_cluster(
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
        assert_eq!(r.pools[0].clean, 8);
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
        assert_eq!(r.pools[0].degraded + r.pools[0].backfilling, 8);
        assert_eq!(r.pools[0].inactive, 0);
    }

    #[test]
    fn below_min_size_is_an_error() {
        let mut m = map();
        let pg = m.pgs.get_mut(&(1, 3)).unwrap();
        pg.acting = vec![pg.acting[0]];
        pg.up = pg.acting.clone();
        let r = report(&m);
        assert_eq!(r.health, Health::Error);
        assert_eq!(r.pools[0].inactive, 1);
        assert_eq!(r.pools[0].clean, 7);
        // No primary at all is inactive too.
        let pg = m.pgs.get_mut(&(1, 4)).unwrap();
        pg.acting[0] = hyperstor_meta::NO_DEVICE;
        assert_eq!(report(&m).pools[0].inactive, 2);
    }

    #[test]
    fn json_is_escaped_and_complete() {
        let mut m = map();
        m.pools.get_mut(&1).unwrap().name = "a\"b".into();
        let j = status_json(&m, &report(&m));
        assert!(j.contains("\"name\":\"a\\\"b\""), "{j}");
        assert!(j.starts_with("{\"cluster\":9,"));
        assert!(j.contains("\"health\":\"ok\""));
        assert_eq!(j.matches("\"up\":true").count(), 3);
    }
}
