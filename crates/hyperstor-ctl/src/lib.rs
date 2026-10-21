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

pub use hyperstor_api::reason_of;

/// Every RPC of `hyperstor.v1` and the `hyperstorctl` command that calls
/// it (decisions.md D55): a test fails when the protos have an RPC this
/// table lacks, so the CLI never falls behind the API.
pub const COMMANDS: &[(&str, &str)] = &[
    ("ClusterService.GetStatus", "status, nodes, pools"),
    ("ClusterService.ListPgs", "pgs"),
    ("ClusterService.SetDeviceIn", "out, in"),
    ("ClusterService.ListVolumes", "volumes"),
    ("ClusterService.CreateVolume", "create-volume"),
    ("ClusterService.DeleteVolume", "delete-volume"),
    ("TokenService.CreateToken", "token create"),
    ("TokenService.ListTokens", "token list"),
    ("TokenService.RevokeToken", "token revoke"),
    // Called by hyperstor-install, which joins the host it runs on.
    ("TokenService.Join", "hyperstor-install install --join"),
    // Called by hyperstor-install's daily run.
    ("CaService.RenewCertificate", "hyperstor-install install"),
    ("NodeService.DrainNode", "drain"),
    ("OperationService.GetOperation", "op get, op watch"),
    ("OperationService.ListOperations", "op list"),
    ("OperationService.CancelOperation", "op cancel"),
];

/// `Service.Rpc` for every RPC in the proto files' text.
pub fn rpcs_in(proto: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut service = "";
    for line in proto.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("service ") {
            service = rest.split_whitespace().next().unwrap_or("");
        } else if let Some(rest) = line.strip_prefix("rpc ") {
            let name = rest.split('(').next().unwrap_or("").trim();
            out.push(format!("{service}.{name}"));
        }
    }
    out
}

/// A time as `YYYY-MM-DD HH:MM`, UTC.
pub fn utc(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// `hyperstorctl token list`.
pub fn tokens_text(r: &pb::ListTokensResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<12}  {:<5} {:<16}  {:<8} CREATED BY",
        "TOKEN", "ROLE", "EXPIRES (UTC)", "STATE"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for t in &r.tokens {
        let state = if t.revoked {
            "revoked"
        } else if t.used {
            "used"
        } else if t.expires_unix <= now {
            "expired"
        } else {
            "valid"
        };
        let _ = writeln!(
            o,
            "{:<12}  {:<5} {:<16}  {:<8} {}",
            t.id,
            t.role,
            utc(t.expires_unix),
            state,
            t.created_by
        );
    }
    o
}

fn state_str(op: &pb::Operation) -> &'static str {
    use pb::OperationState as S;
    match op.state.as_known() {
        Some(S::OPERATION_STATE_RUNNING) => "running",
        Some(S::OPERATION_STATE_DONE) => "done",
        Some(S::OPERATION_STATE_FAILED) => "failed",
        Some(S::OPERATION_STATE_CANCELLED) => "cancelled",
        _ => "unknown",
    }
}

/// One line about an operation, for `op watch`.
pub fn op_line(op: &pb::Operation) -> String {
    format!(
        "{} {} {}: {} ({}/{}): {}",
        op.id,
        op.kind,
        op.target,
        state_str(op),
        op.done,
        op.total,
        op.message
    )
}

/// `hyperstorctl op list` and `op get`.
pub fn ops_text(ops: &[pb::Operation]) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<16}  {:<6} {:<10} {:<9} {:>9}  {:<16}  MESSAGE",
        "OPERATION", "KIND", "TARGET", "STATE", "PROGRESS", "STARTED (UTC)"
    );
    for op in ops {
        let _ = writeln!(
            o,
            "{:<16}  {:<6} {:<10} {:<9} {:>9}  {:<16}  {}",
            op.id,
            op.kind,
            op.target,
            state_str(op),
            format!("{}/{}", op.done, op.total),
            utc(op.started_unix),
            op.message
        );
    }
    o
}

/// The PGs a drain moves, as `drain` and `drain --dry-run` print them.
pub fn moves_text(moves: &[pb::PgMove]) -> String {
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
    if moves.is_empty() {
        return o;
    }
    let _ = writeln!(o, "{:<10} {:<24} AFTER", "PG", "BEFORE");
    for m in moves {
        let _ = writeln!(
            o,
            "{:<10} {:<24} {}",
            format!("{}.{}", m.pool, m.pg),
            set(&m.before),
            set(&m.after)
        );
    }
    o
}

/// `v` as YAML (`-o yaml`). Strings are written as JSON strings, which
/// YAML reads as they are, so no value is misread as a number or a bool.
pub fn to_yaml(v: &serde_json::Value) -> String {
    fn scalar(v: &serde_json::Value) -> Option<String> {
        use serde_json::Value as V;
        match v {
            V::Null => Some("null".into()),
            V::Bool(b) => Some(b.to_string()),
            V::Number(n) => Some(n.to_string()),
            V::String(s) => Some(serde_json::to_string(s).expect("json")),
            V::Array(a) if a.is_empty() => Some("[]".into()),
            V::Object(m) if m.is_empty() => Some("{}".into()),
            _ => None,
        }
    }
    fn go(v: &serde_json::Value, indent: usize, out: &mut String) {
        let pad = " ".repeat(indent);
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    match scalar(x) {
                        Some(s) => out.push_str(&format!("{pad}{k}: {s}\n")),
                        None => {
                            out.push_str(&format!("{pad}{k}:\n"));
                            go(x, indent + 2, out);
                        }
                    }
                }
            }
            serde_json::Value::Array(a) => {
                for x in a {
                    match scalar(x) {
                        Some(s) => out.push_str(&format!("{pad}- {s}\n")),
                        None => {
                            // The item's first line after the dash.
                            let mut inner = String::new();
                            go(x, indent + 2, &mut inner);
                            let inner = inner.trim_start_matches(' ');
                            out.push_str(&format!("{pad}- {inner}"));
                        }
                    }
                }
            }
            x => out.push_str(&format!("{pad}{}\n", scalar(x).unwrap_or_default())),
        }
    }
    let mut out = String::new();
    go(v, 0, &mut out);
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

    /// D55: every RPC in the protos has a command (or is called by the
    /// installer), and the table names no RPC the protos lack.
    #[test]
    fn every_rpc_has_a_command() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../hyperstor-api/proto/hyperstor/v1"
        );
        let mut rpcs = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "proto") {
                rpcs.extend(rpcs_in(&std::fs::read_to_string(&p).unwrap()));
            }
        }
        assert!(rpcs.len() >= 15, "{rpcs:?}");
        let known: Vec<&str> = COMMANDS.iter().map(|(r, _)| *r).collect();
        let missing: Vec<&String> = rpcs
            .iter()
            .filter(|r| !known.contains(&r.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "RPCs without a hyperstorctl command (add one, and a row to COMMANDS): {missing:?}"
        );
        let stale: Vec<&&str> = known
            .iter()
            .filter(|r| !rpcs.iter().any(|x| x == *r))
            .collect();
        assert!(
            stale.is_empty(),
            "COMMANDS names RPCs the protos lack: {stale:?}"
        );
    }

    #[test]
    fn yaml_and_times() {
        let v = serde_json::json!({
            "id": "0a1b", "n": 3, "ok": true, "list": [1, 2], "empty": [],
            "ops": [{"id": "x", "done": 2}], "nested": {"a": "007"}
        });
        let y = to_yaml(&v);
        assert!(y.contains("id: \"0a1b\"\n"), "{y}");
        assert!(y.contains("n: 3\n"), "{y}");
        assert!(y.contains("list:\n  - 1\n  - 2\n"), "{y}");
        assert!(y.contains("empty: []\n"), "{y}");
        assert!(y.contains("ops:\n  - done: 2\n    id: \"x\"\n"), "{y}");
        assert!(
            y.contains("nested:\n  a: \"007\"\n"),
            "a string stays a string: {y}"
        );
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(951_782_400 + 3_661), "2000-02-29 01:01");
        assert_eq!(utc(1_792_454_400), "2026-10-20 00:00");
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
