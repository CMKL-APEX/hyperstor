//! What `hyperstorctl` prints (spec/administration.md §23.6): tables for
//! people, made from the cluster API's answers. `--json` prints the
//! answers themselves, in the proto JSON form `curl` also gets.

use hyperstor_api::proto::hyperstor::v1 as pb;
use std::fmt::Write;

pub fn health_str(h: pb::Health) -> &'static str {
    match h {
        pb::Health::HEALTH_OK => "ok",
        pb::Health::HEALTH_WARN => "warn",
        pb::Health::HEALTH_ERROR => "error",
        _ => "unknown",
    }
}

fn health_of(v: &buffa::EnumValue<pb::Health>) -> pb::Health {
    v.as_known().unwrap_or(pb::Health::HEALTH_UNSPECIFIED)
}

/// Whether the cluster reported itself healthy.
pub fn is_ok(s: &pb::GetStatusResponse) -> bool {
    health_of(&s.health) == pb::Health::HEALTH_OK
}

/// `hyperstorctl status`.
pub fn status_text(s: &pb::GetStatusResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "cluster {}, map epoch {}", s.cluster_id, s.epoch);
    let _ = writeln!(o, "health  {}", health_str(health_of(&s.health)));
    for r in &s.reasons {
        let _ = writeln!(o, "  {:<5} {}", health_str(health_of(&r.level)), r.text);
    }
    let devices: Vec<&pb::Device> = s.nodes.iter().flat_map(|n| &n.devices).collect();
    let _ = writeln!(
        o,
        "nodes   {} up of {}; devices {} in of {}",
        s.nodes.iter().filter(|n| n.up).count(),
        s.nodes.len(),
        devices.iter().filter(|d| d.up_in).count(),
        devices.len()
    );
    o.push_str(&pools_text(s));
    o
}

/// `hyperstorctl pools`.
pub fn pools_text(s: &pb::GetStatusResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
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
    for p in &s.pools {
        let _ = writeln!(
            o,
            "{:<4} {:<12} {:<10} {:>4} {:>3} {:>5} {:>5}K {:>6} {:>8} {:>9} {:>8}",
            p.id,
            p.name,
            p.kind,
            p.size,
            p.min_size,
            p.pg_count,
            p.chunk_size >> 10,
            p.pgs.clean,
            p.pgs.degraded,
            p.pgs.backfilling,
            p.pgs.inactive
        );
    }
    o
}

/// `hyperstorctl nodes`: each node and its devices.
pub fn nodes_text(s: &pb::GetStatusResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<5} {:<5} {:<5} {:<7} {:<22} {:<6} STATE",
        "NODE", "RACK", "UP", "DEVICE", "ADDRESS", "WEIGHT"
    );
    for n in &s.nodes {
        for d in &n.devices {
            let _ = writeln!(
                o,
                "{:<5} {:<5} {:<5} {:<7} {:<22} {:<6.2} {}",
                n.id,
                n.rack,
                if n.up { "yes" } else { "no" },
                d.id,
                d.addr,
                d.weight,
                if d.up_in { "in" } else { "out" }
            );
        }
    }
    o
}

/// `hyperstorctl pgs`: acting and up sets.
pub fn pgs_text(r: &pb::ListPgsResponse) -> String {
    let set = |v: &[u32]| {
        let parts: Vec<String> = v
            .iter()
            .map(|d| {
                if *d == u32::MAX {
                    "-".to_string()
                } else {
                    d.to_string()
                }
            })
            .collect();
        format!("[{}]", parts.join(","))
    };
    let mut o = String::new();
    let _ = writeln!(o, "{:<10} {:>6}  {:<24} UP", "PG", "EPOCH", "ACTING");
    for pg in &r.pgs {
        let _ = writeln!(
            o,
            "{:<10} {:>6}  {:<24} {}",
            format!("{}.{}", pg.pool, pg.pg),
            pg.epoch,
            set(&pg.acting),
            set(&pg.up)
        );
    }
    o
}

/// A pool's volumes: id, name, size and state.
pub fn volumes_text(r: &pb::ListVolumesResponse) -> String {
    let mut out = format!("{:<32}  {:<20} {:>14}  STATE\n", "ID", "NAME", "SIZE");
    for v in &r.volumes {
        out.push_str(&format!(
            "{:<32}  {:<20} {:>14}  {}\n",
            v.id, v.name, v.size, v.state
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(degraded: bool) -> pb::GetStatusResponse {
        let mut m = hyperstor_meta::parse_cluster(
            "cluster 9\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             node 2 rack 0 127.0.0.1:9102\npool 1 vol replicated size 3 min 2 pgs 8 domain host\n",
        )
        .unwrap();
        if degraded {
            m.nodes.get_mut(&2).unwrap().up = false;
            m.recompute();
        }
        hyperstor_meta::api::status(&m, None, "127.0.0.1:8849")
    }

    #[test]
    fn status_text_shows_health_reasons_and_pools() {
        let ok = status(false);
        assert!(is_ok(&ok));
        let t = status_text(&ok);
        assert!(t.contains("health  ok"), "{t}");
        assert!(t.contains("nodes   3 up of 3; devices 3 in of 3"), "{t}");
        let bad = status(true);
        assert!(!is_ok(&bad));
        let t = status_text(&bad);
        assert!(t.contains("health  warn"), "{t}");
        assert!(t.contains("warn  1 node(s) down: 2"), "{t}");
    }

    #[test]
    fn tables_list_every_device_and_pool() {
        let s = status(false);
        let n = nodes_text(&s);
        assert_eq!(n.lines().count(), 4, "{n}");
        assert!(n.contains("127.0.0.1:9102"));
        let p = pools_text(&s);
        assert!(p.contains("replicated"), "{p}");
        let pgs = pgs_text(&pb::ListPgsResponse {
            pgs: vec![pb::Pg {
                pool: 1,
                pg: 2,
                epoch: 5,
                acting: vec![3, u32::MAX],
                up: vec![3, 4],
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(
            pgs.contains("1.2") && pgs.contains("[3,-]") && pgs.contains("[3,4]"),
            "{pgs}"
        );
    }
}
