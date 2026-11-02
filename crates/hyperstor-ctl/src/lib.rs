//! What `hyperstorctl` prints (spec/19-administration.md §23.6): tables for
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
    let _ = writeln!(o, "space   {}", space_text(&devices));
    o.push_str(&pools_text(s));
    o
}

/// Raw space over the devices in placement, from their reports to the
/// leader (D156): what is free before copies or erasure coding, which
/// `pools` then gives per pool.
fn space_text(devices: &[&pb::Device]) -> String {
    let (mut free, mut total, mut missing) = (0u64, 0u64, 0usize);
    for d in devices.iter().filter(|d| d.up_in) {
        match (d.free_bytes, d.total_bytes) {
            (Some(f), Some(t)) => {
                free += f;
                total += t;
            }
            _ => missing += 1,
        }
    }
    if total == 0 {
        return "- (devices report their space to the leader)".into();
    }
    let mut t = format!(
        "{} free of {} raw ({:.0}% used)",
        gib(Some(free)),
        gib(Some(total)),
        (total - free) as f64 * 100.0 / total as f64
    );
    if missing > 0 {
        let _ = write!(t, "; {missing} device(s) not reported");
    }
    t
}

/// `hyperstorctl pools`.
/// Bytes in GiB with a decimal, or `-` when not reported (D156).
fn gib(b: Option<u64>) -> String {
    b.map_or("-".into(), |b| {
        format!("{:.1}G", b as f64 / (1u64 << 30) as f64)
    })
}

/// A device's space after a removal: used of total, and the share.
pub fn fill_text(used: u64, total: u64) -> String {
    format!(
        "{} of {} used ({:.0}%)",
        gib(Some(used)),
        gib(Some(total)),
        used as f64 * 100.0 / total.max(1) as f64
    )
}

pub fn pools_text(s: &pb::GetStatusResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<4} {:<12} {:<10} {:>4} {:>3} {:>5} {:>6} {:>6} {:>8} {:>9} {:>8} {:>10} {:>10} CLASSES",
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
        "INACTIVE",
        "FREE",
        "TOTAL"
    );
    // Usable space (D156): `-` until every device has reported.
    let mut mixed = false;
    for p in &s.pools {
        // Every device, or the pool's classes (D164), then the devices
        // added and removed (`+D`, `-D`, D180); `!` when they mix hard
        // disks and flash (D172).
        mixed |= p.mixed_kinds;
        let mut classes = if p.classes.is_empty() {
            "any".to_string()
        } else {
            p.classes.join(",")
        };
        for d in &p.added_devices {
            let _ = write!(classes, "+{d}");
        }
        for d in &p.removed_devices {
            let _ = write!(classes, "-{d}");
        }
        if p.mixed_kinds {
            classes.push('!');
        }
        // Nearfull or full, and the device that made it so (D27).
        if !p.fullness.is_empty() && p.fullness != "ok" {
            let _ = write!(classes, "  {}", p.fullness);
            if let Some(d) = p.fullest_device {
                let _ = write!(classes, " (device {d})");
            }
        }
        let _ = writeln!(
            o,
            "{:<4} {:<12} {:<10} {:>4} {:>3} {:>5} {:>5}K {:>6} {:>8} {:>9} {:>8} {:>10} {:>10} {}",
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
            p.pgs.inactive,
            gib(p.free_bytes),
            gib(p.total_bytes),
            classes
        );
    }
    if mixed {
        let _ = writeln!(
            o,
            "! classes hold hard disks and flash (set with --force): writes wait for the slowest copy"
        );
    }
    // Splits (D173, D187), and the autoscaler (D184; not for .meta).
    for p in &s.pools {
        if !p.autoscale && p.id != 0 {
            let _ = writeln!(o, "pool {}: autoscale off", p.id);
        }
        // A cache pool's grace other than the default, 60 s (D189).
        if p.kind == "cache" && p.cache_grace != 60 {
            let _ = writeln!(
                o,
                "pool {}: cache PGs wait {} s for a down member",
                p.id, p.cache_grace
            );
        }
        if p.splitting {
            let _ = writeln!(
                o,
                "pool {} splitting to {} PGs: {} of {} split",
                p.id,
                p.pg_count * 2,
                p.split_done,
                p.pg_count
            );
        } else if p.placement_count != 0 && p.placement_count < p.pg_count {
            let _ = writeln!(
                o,
                "pool {}: its {} PGs are placed as {}: the new ones are on their parents' devices",
                p.id, p.pg_count, p.placement_count
            );
        }
    }
    o
}

/// `hyperstorctl nodes`: each node and its devices.
pub fn nodes_text(s: &pb::GetStatusResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<5} {:<5} {:<5} {:<7} {:<22} {:<8} {:<12} {:>10} {:>10} STATE",
        "NODE", "RACK", "UP", "DEVICE", "ADDRESS", "WEIGHT", "CLASS", "FREE", "TOTAL"
    );
    let mut fixed = false;
    let mut class_set = false;
    for n in &s.nodes {
        for d in &n.devices {
            // In TiB from the device's capacity, unless set (D158).
            fixed |= d.weight_fixed;
            let weight = format!("{:.4}{}", d.weight, if d.weight_fixed { "*" } else { "" });
            // The device's kind, unless set (D178).
            class_set |= d.class_fixed;
            let class = format!("{}{}", d.class, if d.class_fixed { "+" } else { "" });
            let _ = writeln!(
                o,
                "{:<5} {:<5} {:<5} {:<7} {:<22} {:<8} {:<12} {:>10} {:>10} {}",
                n.id,
                n.rack,
                if n.up { "yes" } else { "no" },
                d.id,
                d.addr,
                weight,
                class,
                gib(d.free_bytes),
                gib(d.total_bytes),
                // And how full, past a threshold (D27, D182).
                match d.fullness.as_str() {
                    "" | "ok" => (if d.up_in { "in" } else { "out" }).to_string(),
                    f => format!("{} {f}", if d.up_in { "in" } else { "out" }),
                }
            );
        }
    }
    if fixed {
        let _ = writeln!(
            o,
            "* weight set with hyperstorctl weight; the others follow capacity"
        );
    }
    if class_set {
        let _ = writeln!(
            o,
            "+ class set with hyperstorctl device class; the others are their kind"
        );
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

/// `hyperstorctl pg-grid`: a character per PG, 64 to a line, each line
/// led by its first PG (D154); with usage (D155), a second grid of digits,
/// 0 for the emptiest level to 9 for the fullest.
pub fn pg_grid_text(pool: u32, r: &pb::GetPgGridResponse) -> String {
    let mut o = format!(
        "pool {pool}, map epoch {}: . clean  d degraded  b backfilling  X inactive\n",
        r.epoch
    );
    for (i, row) in r.states.chunks(64).enumerate() {
        let cells: String = row
            .iter()
            .map(|s| match *s {
                1 => '.',
                2 => 'd',
                3 => 'b',
                4 => 'X',
                _ => '?',
            })
            .collect();
        let _ = writeln!(o, "{:>6}  {cells}", i * 64);
    }
    if !r.usage.is_empty() {
        let _ = writeln!(
            o,
            "usage as a share of the fullest PG, {} bytes: 0 least to 9 most  ? not reported",
            r.max_bytes
        );
        for (i, row) in r.usage.chunks(64).enumerate() {
            let cells: String = row
                .iter()
                .map(|&l| match l {
                    0 => '?',
                    l => char::from(b'0' + ((l as u32 - 1) * 10 / 255) as u8),
                })
                .collect();
            let _ = writeln!(o, "{:>6}  {cells}", i * 64);
        }
    }
    o
}

/// `hyperstorctl pg-grid --heat read|write` (D219): a digit per PG for
/// its requests per second, 1 for the quietest PG that served any to 9
/// for the busiest, linear; `.` for none.
pub fn pg_heat_text(pool: u32, write: bool, r: &pb::GetPgHeatResponse) -> String {
    let ops = if write { &r.write_ops } else { &r.read_ops };
    let max = ops.iter().copied().fold(0f32, f32::max);
    let total: f32 = ops.iter().sum();
    let mut o = format!(
        "pool {pool}, {} per PG on its primary: 1 least to 9 most (the busiest {max:.0}/s, all {total:.0}/s)  . none\n",
        if write { "writes" } else { "reads" },
    );
    for (i, row) in ops.chunks(64).enumerate() {
        let cells: String = row
            .iter()
            .map(|&x| {
                if x <= 0.0 || max <= 0.0 {
                    '.'
                } else {
                    char::from(b'1' + ((x / max * 8.0).round() as u8).min(8))
                }
            })
            .collect();
        let _ = writeln!(o, "{:>6}  {cells}", i * 64);
    }
    if !r.top.is_empty() {
        o.push_str(&volume_heat_text(&r.top));
    }
    o
}

/// Volumes by their load: requests and MiB per second.
pub fn volume_heat_text(v: &[pb::VolumeHeat]) -> String {
    let mut o = format!(
        "{:<32}  {:<20} {:>4} {:>10} {:>8}\n",
        "ID", "NAME", "POOL", "REQUESTS/S", "MIB/S"
    );
    for x in v {
        let _ = writeln!(
            o,
            "{:<32}  {:<20} {:>4} {:>10.0} {:>8.1}",
            if x.id.is_empty() { "-" } else { &x.id },
            x.name,
            x.pool,
            x.ops,
            x.bytes / f32::from(1u16 << 10) / 1024.0
        );
    }
    o
}

/// The time under which a share `q` of I/Os completed, from bucket counts
/// (under 16, 32, ... 65536 µs, then slower: `max`).
pub fn quantile_us(buckets: &[u32], max: u32, q: f64) -> u32 {
    const B: [u32; 13] = [
        16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ];
    let n: u64 = buckets.iter().map(|x| u64::from(*x)).sum();
    if n == 0 {
        return 0;
    }
    let want = ((n as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, c) in buckets.iter().enumerate() {
        seen += u64::from(*c);
        if seen >= want {
            return B.get(i).copied().unwrap_or(max).min(max.max(1));
        }
    }
    max
}

/// A device's last `n` samples merged: requests/s, bytes/s, the reads' and
/// commits' p99 and most in flight.
fn last(d: &pb::DevicePerf, n: usize) -> (f32, f32, f32, u32, u32, u32) {
    let s: Vec<&pb::PerfSample> = d.samples.iter().rev().take(n).collect();
    if s.is_empty() {
        return (0.0, 0.0, 0.0, 0, 0, 0);
    }
    let k = s.len() as f32;
    let avg = |f: &dyn Fn(&pb::PerfSample) -> f32| s.iter().map(|x| f(x)).sum::<f32>() / k;
    let merge = |f: &dyn Fn(&pb::PerfSample) -> Option<&pb::ServiceTime>| {
        let mut b = vec![0u32; 14];
        let mut max = 0;
        for x in &s {
            if let Some(t) = f(x) {
                for (a, c) in b.iter_mut().zip(&t.buckets) {
                    *a += c;
                }
                max = max.max(t.max_us);
            }
        }
        quantile_us(&b, max, 0.99)
    };
    (
        avg(&|x| x.read_ops),
        avg(&|x| x.write_ops),
        avg(&|x| x.read_bytes + x.write_bytes),
        merge(&|x| x.reads.as_option()),
        merge(&|x| x.commits.as_option()),
        s.iter().map(|x| x.inflight_max).max().unwrap_or(0),
    )
}

fn us(v: u32) -> String {
    if v == 0 {
        "-".into()
    } else if v >= 1000 {
        format!("{:.1}ms", f64::from(v) / 1000.0)
    } else {
        format!("{v}us")
    }
}

/// `hyperstorctl perf`: each device over the last minute.
pub fn perf_text(r: &pb::ListDevicePerfResponse) -> String {
    let mut o = format!(
        "{:<6} {:>4} {:<5} {:>8} {:>8} {:>7} {:>9} {:>9} {:>8} {:>6} {:>5}\n",
        "DEVICE",
        "NODE",
        "KIND",
        "READS/S",
        "WRITES/S",
        "MIB/S",
        "READ P99",
        "SYNC P99",
        "INFLIGHT",
        "TEMP",
        "WEAR"
    );
    for d in &r.devices {
        let (rd, wr, by, rp, cp, inf) = last(d, 12);
        let dr = d.drive.as_option();
        let temp = dr
            .and_then(|x| {
                x.temperature_c
                    .or(x.smart.as_option().map(|s| s.temperature_c))
            })
            .map_or("-".into(), |t| format!("{t:.0}C"));
        let wear = dr
            .and_then(|x| x.smart.as_option())
            .map_or("-".into(), |s| format!("{}%", s.percentage_used));
        let _ = writeln!(
            o,
            "{:<6} {:>4} {:<5} {:>8.0} {:>8.0} {:>7.1} {:>9} {:>9} {:>8} {:>6} {:>5}",
            d.device,
            d.node,
            d.kind,
            rd,
            wr,
            by / 1048576.0,
            us(rp),
            us(cp),
            inf,
            temp,
            wear
        );
    }
    o
}

/// `hyperstorctl perf --device D`: its drive, then each 5-s window.
pub fn device_perf_text(d: &pb::DevicePerf) -> String {
    let mut o = format!("device {} on node {}, {}\n", d.device, d.node, d.kind);
    if let Some(x) = d.drive.as_option() {
        if !x.model.is_empty() {
            let _ = writeln!(o, "drive     {} (firmware {})", x.model, x.firmware);
        }
        if let (Some(w), Some(wm), Some(s), Some(sm)) = (
            x.link_width,
            x.link_width_max,
            x.link_speed,
            x.link_speed_max,
        ) {
            let _ = writeln!(
                o,
                "link      x{w} at {s:.1} GT/s (can do x{wm} at {sm:.1} GT/s)"
            );
        }
        if x.numa_node >= 0 {
            let _ = writeln!(
                o,
                "numa      drive on node {}, reactor on node {}",
                x.numa_node, x.reactor_numa_node
            );
        }
        let _ = writeln!(
            o,
            "cache     {}",
            if x.write_cache {
                "volatile write cache"
            } else {
                "no volatile write cache"
            }
        );
        match x.smart.as_option() {
            Some(s) => {
                let _ = writeln!(
                    o,
                    "smart     {:.0} C (warning {:.0}, critical {:.0}); {}% used, spare {}% (threshold {}%); {} media errors; critical warning {:#04x}",
                    s.temperature_c,
                    s.warning_threshold_c,
                    s.critical_threshold_c,
                    s.percentage_used,
                    s.available_spare,
                    s.spare_threshold,
                    s.media_errors,
                    s.critical_warning
                );
                let _ = writeln!(
                    o,
                    "          {:.1} TB written, {:.1} TB read, {} h powered on, {} unsafe shutdowns; throttled {} s and {} s",
                    s.bytes_written as f64 / 1e12,
                    s.bytes_read as f64 / 1e12,
                    s.power_on_hours,
                    s.unsafe_shutdowns,
                    s.throttle_seconds.first().copied().unwrap_or(0),
                    s.throttle_seconds.get(1).copied().unwrap_or(0)
                );
            }
            None if !x.smart_note.is_empty() => {
                let _ = writeln!(o, "smart     none: {}", x.smart_note);
            }
            None => {}
        }
        if let Some(t) = x.temperature_c {
            let _ = writeln!(o, "temp      {t:.1} C (hwmon)");
        }
    }
    let _ = writeln!(
        o,
        "{:<8} {:>8} {:>8} {:>7} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "AGO",
        "READS/S",
        "WRITES/S",
        "MIB/S",
        "READ P50",
        "READ P99",
        "SYNC P50",
        "SYNC P99",
        "INFLIGHT"
    );
    let newest = d.samples.last().map_or(0, |x| x.unix_ms);
    for x in d.samples.iter().rev().take(24) {
        let (r, c) = (x.reads.as_option(), x.commits.as_option());
        let _ = writeln!(
            o,
            "{:<8} {:>8.0} {:>8.0} {:>7.1} {:>9} {:>9} {:>9} {:>9} {:>8}",
            format!("{}s", (newest - x.unix_ms) / 1000),
            x.read_ops,
            x.write_ops,
            (x.read_bytes + x.write_bytes) / 1048576.0,
            us(r.map_or(0, |t| t.p50_us)),
            us(r.map_or(0, |t| t.p99_us)),
            us(c.map_or(0, |t| t.p50_us)),
            us(c.map_or(0, |t| t.p99_us)),
            x.inflight_max
        );
    }
    o
}

/// `hyperstorctl advise`: the Advisor's findings.
pub fn findings_text(r: &pb::ListFindingsResponse) -> String {
    if r.findings.is_empty() {
        return "nothing to suggest: no device, drive or volume stands out\n".into();
    }
    let mut o = String::new();
    for f in &r.findings {
        let _ = writeln!(
            o,
            "[{}] {}  ({}, rule {})",
            f.severity, f.title, f.subject, f.rule
        );
        let _ = writeln!(o, "    seen: {}", f.evidence);
        let _ = writeln!(o, "    do:   {}", f.action);
        if !f.command.is_empty() {
            let _ = writeln!(o, "    $ {}", f.command);
        }
    }
    o
}

/// A pool's volumes: id, name, size and state.
pub fn volumes_text(r: &pb::ListVolumesResponse) -> String {
    let mut out = format!(
        "{:<32}  {:<20} {:>14}  {:<8}  ATTACHED BY\n",
        "ID", "NAME", "SIZE", "STATE"
    );
    for v in &r.volumes {
        out.push_str(&format!(
            "{:<32}  {:<20} {:>14}  {:<8}  {}\n",
            v.id,
            v.name,
            v.size,
            v.state,
            if v.attached_by.is_empty() {
                "-"
            } else {
                &v.attached_by
            }
        ));
    }
    out
}

/// A volume's snapshots: epoch, name, group and state.
pub fn snapshots_text(r: &pb::ListSnapshotsResponse) -> String {
    let mut out = format!("{:>8}  {:<20} {:<32}  STATE\n", "EPOCH", "NAME", "GROUP");
    for s in &r.snapshots {
        out.push_str(&format!(
            "{:>8}  {:<20} {:<32}  {}\n",
            s.epoch,
            s.name,
            if s.group.is_empty() { "-" } else { &s.group },
            s.state
        ));
    }
    out
}

pub use hyperstor_api::reason_of;

/// Every RPC of `hyperstor.v1` and the `hyperstorctl` command that calls
/// it (04-decisions.md D55): a test fails when the protos have an RPC this
/// table lacks, so the CLI never falls behind the API.
pub const COMMANDS: &[(&str, &str)] = &[
    ("ClusterService.GetStatus", "status, nodes, pools"),
    ("ClusterService.ListPgs", "pgs"),
    ("ClusterService.GetPgGrid", "pg-grid"),
    ("ClusterService.GetPgHeat", "pg-grid --heat"),
    ("ClusterService.ListDevicePerf", "perf"),
    ("ClusterService.ListTopVolumes", "top-volumes"),
    ("ClusterService.ListFindings", "advise"),
    ("ClusterService.SetDeviceIn", "out, in"),
    ("ClusterService.SetDeviceWeight", "weight"),
    ("ClusterService.SetFullRatios", "full-ratios"),
    ("ClusterService.AddDevice", "device add"),
    ("ClusterService.RemoveDevice", "device remove"),
    ("ClusterService.SetDeviceClass", "device class"),
    ("ClusterService.CreatePool", "pool create"),
    ("ClusterService.SetPool", "pool set"),
    (
        "ClusterService.SetPoolDevice",
        "pool add-device, pool remove-device",
    ),
    ("ClusterService.DeletePool", "pool delete"),
    ("ClusterService.ListVolumes", "volumes"),
    ("ClusterService.CreateVolume", "create-volume"),
    ("ClusterService.DeleteVolume", "delete-volume"),
    ("ClusterService.ExpandVolume", "expand-volume"),
    ("ClusterService.ListSnapshots", "snapshot list"),
    ("ClusterService.CreateSnapshot", "snapshot create"),
    ("ClusterService.DeleteSnapshot", "snapshot delete"),
    ("ClusterService.CloneVolume", "clone-volume"),
    ("ClusterService.RollbackVolume", "rollback-volume"),
    ("ClusterService.FlattenVolume", "flatten-volume"),
    ("ClusterService.CopyVolume", "copy-volume"),
    ("ClusterService.MoveVolume", "move-volume"),
    ("ClusterService.CreateGroupSnapshot", "snapshot group"),
    (
        "ClusterService.DeleteGroupSnapshot",
        "snapshot group-delete",
    ),
    ("TokenService.CreateToken", "token create"),
    ("TokenService.ListTokens", "token list"),
    ("TokenService.RevokeToken", "token revoke"),
    // Called by hyperstor-install, which joins the host it runs on.
    ("TokenService.Join", "hyperstor-install install --join"),
    // Called by hyperstor-install's daily run.
    ("CaService.RenewCertificate", "hyperstor-install install"),
    ("CaService.ListCertificates", "cert list"),
    ("CaService.RevokeCertificate", "cert revoke"),
    ("NodeService.DrainNode", "drain"),
    ("MapService.GetMapBackup", "map backup"),
    ("MemberService.ListMembers", "member list"),
    ("MemberService.AddMember", "member add"),
    ("MemberService.RemoveMember", "member remove"),
    // hyperstor-mgmt's own rows: no command (people's state, D98).
    ("MgmtStoreService.GetRow", "hyperstor-mgmt"),
    ("MgmtStoreService.PutRow", "hyperstor-mgmt"),
    ("MgmtStoreService.DeleteRow", "hyperstor-mgmt"),
    ("MgmtStoreService.ListRows", "hyperstor-mgmt"),
    (
        "OperationService.GetOperation",
        "op get, op watch through hyperstor-mgmt",
    ),
    ("OperationService.ListOperations", "op list"),
    ("OperationService.WatchOperation", "op watch, --wait"),
    ("OperationService.CancelOperation", "op cancel"),
    ("AuditService.ListAudit", "audit list"),
    // hyperstor-mgmt's own records: no command.
    ("AuditService.AppendAudit", "hyperstor-mgmt"),
    ("AuditService.GetAuditSettings", "audit settings"),
    (
        "AuditService.SetAuditSettings",
        "audit settings --retention-days N",
    ),
    ("TrashService.ListTrash", "trash list"),
    ("TrashService.RestoreFromTrash", "trash restore"),
    ("TrashService.PurgeFromTrash", "trash purge"),
    ("TrashService.GetTrashSettings", "trash settings"),
    (
        "TrashService.SetTrashSettings",
        "trash settings --grace S --on-full keep|purge",
    ),
];

/// `30s`, `30m`, `2h`, `7d` (or bare seconds): seconds.
pub fn parse_ago(s: &str) -> Option<u64> {
    let (n, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: u64 = n.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// `audit list` as lines: time, principal, operation, target, result.
pub fn audit_text(r: &pb::ListAuditResponse) -> String {
    if r.records.is_empty() {
        return "No audit records.\n".into();
    }
    let mut t = String::new();
    for x in &r.records {
        let _ = writeln!(
            t,
            "{}.{:03}  {}  {}  {}  {}",
            utc(x.time_ms / 1000),
            x.time_ms % 1000,
            x.principal,
            x.op,
            x.target,
            x.result
        );
    }
    t
}

/// `trash list` as a table: what, which volume, name, pool, when it went,
/// when it's purged.
pub fn trash_text(r: &pb::ListTrashResponse) -> String {
    if r.items.is_empty() {
        return "The trash is empty.\n".into();
    }
    let mut t = format!(
        "{:<9} {:<32} {:>5} {:<16} {:>4} {:<16} {:<16}\n",
        "KIND", "VOLUME", "EPOCH", "NAME", "POOL", "TRASHED", "PURGED"
    );
    for i in &r.items {
        let epoch = if i.kind == "snapshot" {
            i.epoch.to_string()
        } else {
            "-".into()
        };
        t.push_str(&format!(
            "{:<9} {:<32} {:>5} {:<16} {:>4} {:<16} {:<16}\n",
            i.kind,
            i.id,
            epoch,
            if i.name.is_empty() { "-" } else { &i.name },
            i.pool,
            utc(i.trashed_unix),
            utc(i.purge_unix)
        ));
    }
    t
}

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

/// `hyperstorctl member list`, and what `member add` and `remove` print.
pub fn members_text(members: &[pb::Member]) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{:<6} {:<8} {:<22} MAP", "MEMBER", "ROLE", "RAFT");
    for m in members {
        let role = match (m.leader, m.voter) {
            (true, _) => "leader",
            (false, true) => "voter",
            (false, false) => "learner",
        };
        let _ = writeln!(
            o,
            "{:<6} {:<8} {:<22} {}",
            m.id, role, m.raft_address, m.map_address
        );
    }
    o
}

/// `hyperstorctl cert list`.
pub fn certs_text(r: &pb::ListCertificatesResponse) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<34} {:<36} {:<16}  {:<7} ISSUED TO",
        "SERIAL", "IDENTITY", "EXPIRES (UTC)", "STATE"
    );
    for c in &r.certificates {
        let _ = writeln!(
            o,
            "{:<34} {:<36} {:<16}  {:<7} {}",
            c.serial,
            c.identity,
            utc(c.not_after_unix),
            if c.revoked { "revoked" } else { "valid" },
            c.issued_by
        );
    }
    // Revoked serials the API didn't issue (the map service host's own).
    for x in r
        .revoked
        .iter()
        .filter(|x| !r.certificates.iter().any(|c| c.serial == x.serial))
    {
        let _ = writeln!(
            o,
            "{:<34} {:<36} {:<16}  revoked {}",
            x.serial,
            "-",
            utc(x.until_unix),
            x.revoked_by
        );
    }
    o
}

/// `hyperstorctl cert show`: a certificate file's identity, serial and
/// expiry.
pub fn cert_text(pem: &str) -> Option<String> {
    let serial = hyperstor_pki::serial_in(pem)?;
    let who = hyperstor_pki::identity_in(pem).map_or("-".to_string(), |i| i.uri());
    let end = hyperstor_pki::not_after(pem).ok()?;
    let end = end
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Some(format!(
        "identity {who}\nserial   {serial}\nexpires  {} (UTC)\n",
        utc(end)
    ))
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
        "{:<16}  {:<7} {:<10} {:<9} {:>9}  {:<16}  MESSAGE",
        "OPERATION", "KIND", "TARGET", "STATE", "PROGRESS", "STARTED (UTC)"
    );
    for op in ops {
        let _ = writeln!(
            o,
            "{:<16}  {:<7} {:<10} {:<9} {:>9}  {:<16}  {}",
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
                // Keys in order, whatever serde_json's map keeps (a
                // dependency may turn on `preserve_order`).
                let mut keys: Vec<_> = m.iter().collect();
                keys.sort_by(|a, b| a.0.cmp(b.0));
                for (k, x) in keys {
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
    #[test]
    fn heat_shows_a_digit_per_pg_and_the_busiest_volumes() {
        let r = pb::GetPgHeatResponse {
            read_ops: vec![0.0, 10.0, 80.0, 40.0],
            write_ops: vec![0.0; 4],
            top: vec![pb::VolumeHeat {
                id: "ab".into(),
                name: "db".into(),
                pool: 1,
                ops: 70.0,
                bytes: 2.0 * 1048576.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let t = pg_heat_text(1, false, &r);
        assert!(t.contains("     0  .295\n"), "{t}");
        assert!(t.contains("the busiest 80/s, all 130/s"), "{t}");
        assert!(t.contains("db") && t.contains("2.0"), "{t}");
        assert!(pg_heat_text(1, true, &r).contains("     0  ....\n"));
    }

    #[test]
    fn quantiles_merge_buckets() {
        let mut b = vec![0u32; 14];
        b[2] = 98;
        b[9] = 2;
        assert_eq!(quantile_us(&b, 5000, 0.5), 64);
        assert_eq!(quantile_us(&b, 5000, 0.99), 5000, "capped at the max");
        assert_eq!(quantile_us(&[0; 14], 0, 0.99), 0);
    }

    #[test]
    fn perf_and_findings_read_as_tables() {
        let svc = |b: usize| {
            let mut buckets = vec![0u32; 14];
            buckets[b] = 10;
            buckets
        };
        let d = pb::DevicePerf {
            device: 1002,
            node: 1,
            kind: "nvme".into(),
            samples: vec![pb::PerfSample {
                unix_ms: 5000,
                window_ms: 5000,
                read_ops: 120.0,
                write_ops: 30.0,
                read_bytes: 1048576.0,
                write_bytes: 0.0,
                reads: buffa::MessageField::some(pb::ServiceTime {
                    count: 10,
                    p50_us: 64,
                    p99_us: 64,
                    max_us: 60,
                    buckets: svc(2),
                    ..Default::default()
                }),
                commits: buffa::MessageField::some(pb::ServiceTime {
                    count: 10,
                    p50_us: 2048,
                    p99_us: 2048,
                    max_us: 1900,
                    buckets: svc(7),
                    ..Default::default()
                }),
                inflight_max: 7,
                ..Default::default()
            }],
            drive: buffa::MessageField::some(pb::Drive {
                model: "M".into(),
                temperature_c: Some(41.0),
                link_width: Some(2),
                link_width_max: Some(4),
                link_speed: Some(8.0),
                link_speed_max: Some(16.0),
                numa_node: 1,
                smart: buffa::MessageField::some(pb::Smart {
                    percentage_used: 3,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = pb::ListDevicePerfResponse {
            devices: vec![d.clone()],
            ..Default::default()
        };
        let t = perf_text(&r);
        assert!(
            t.contains("1002") && t.contains("nvme") && t.contains("120"),
            "{t}"
        );
        assert!(
            t.contains("60us") && t.contains("1.9ms") && t.contains("41C") && t.contains("3%"),
            "{t}"
        );
        let t = device_perf_text(&d);
        assert!(
            t.contains("link      x2 at 8.0 GT/s (can do x4 at 16.0 GT/s)"),
            "{t}"
        );
        assert!(t.contains("3% used"), "{t}");
        let f = pb::ListFindingsResponse {
            findings: vec![pb::Finding {
                rule: "pcie-link".into(),
                severity: "warn".into(),
                subject: "device 1002".into(),
                title: "T".into(),
                evidence: "E".into(),
                action: "A".into(),
                command: "hyperstorctl perf --device 1002".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let t = findings_text(&f);
        assert!(
            t.starts_with("[warn] T  (device 1002, rule pcie-link)\n"),
            "{t}"
        );
        assert!(t.contains("    $ hyperstorctl perf --device 1002"), "{t}");
        assert!(
            findings_text(&pb::ListFindingsResponse::default()).starts_with("nothing to suggest")
        );
    }

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
        hyperstor_meta::api::status(
            &m,
            None,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
        )
    }

    #[test]
    fn status_text_shows_health_reasons_and_pools() {
        let ok = status(false);
        assert!(is_ok(&ok));
        let t = status_text(&ok);
        assert!(t.contains("health  ok"), "{t}");
        assert!(t.contains("nodes   3 up of 3; devices 3 in of 3"), "{t}");
        assert!(t.contains("space   - (devices report"), "{t}");
        let bad = status(true);
        assert!(!is_ok(&bad));
        let t = status_text(&bad);
        assert!(t.contains("health  warn"), "{t}");
        assert!(t.contains("warn  1 node(s) down: 2"), "{t}");
    }

    #[test]
    fn status_text_sums_the_space_of_devices_in() {
        let mut s = status(false);
        let gib = 1u64 << 30;
        for (i, d) in s.nodes.iter_mut().flat_map(|n| &mut n.devices).enumerate() {
            match i {
                0 => {
                    d.total_bytes = Some(100 * gib);
                    d.free_bytes = Some(75 * gib);
                }
                // Out: its space doesn't count.
                1 => {
                    d.up_in = false;
                    d.total_bytes = Some(100 * gib);
                    d.free_bytes = Some(100 * gib);
                }
                _ => {}
            }
        }
        let t = status_text(&s);
        assert!(
            t.contains("space   75.0G free of 100.0G raw (25% used); 1 device(s) not reported"),
            "{t}"
        );
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
    fn ago_durations() {
        assert_eq!(parse_ago("90"), Some(90));
        assert_eq!(parse_ago("30m"), Some(1800));
        assert_eq!(parse_ago("2h"), Some(7200));
        assert_eq!(parse_ago("7d"), Some(604_800));
        assert_eq!(parse_ago("1w"), None);
        assert_eq!(parse_ago("h"), None);
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
        assert!(
            n.lines().nth(1).unwrap().contains(" - "),
            "no space reported: {n}"
        );
        let mut with = s.clone();
        with.nodes[0].devices[0].total_bytes = Some(4 << 30);
        with.nodes[0].devices[0].free_bytes = Some(1 << 29);
        let n = nodes_text(&with);
        assert!(n.contains("0.5G       4.0G in"), "{n}");
        // A weight an operator set is marked (D158).
        with.nodes[0].devices[0].weight = 0.25;
        with.nodes[0].devices[0].weight_fixed = true;
        let n = nodes_text(&with);
        assert!(n.contains(" 0.2500* ") && n.lines().count() == 5, "{n}");
        let p = pools_text(&s);
        assert!(p.contains("replicated"), "{p}");
        // A pool's classes, then its devices added and removed (D180).
        let mut edited = s.clone();
        edited.pools[0].classes = vec!["nvme".into()];
        edited.pools[0].added_devices = vec![7];
        edited.pools[0].removed_devices = vec![2, 3];
        edited.pools[0].mixed_kinds = true;
        let p = pools_text(&edited);
        assert!(p.contains(" nvme+7-2-3!\n"), "{p}");
        // Past a threshold, and which device (D27).
        edited.pools[0].fullness = "full".into();
        edited.pools[0].fullest_device = Some(2);
        assert!(pools_text(&edited).contains(" nvme+7-2-3!  full (device 2)\n"));
        with.nodes[0].devices[0].fullness = "backfillfull".into();
        assert!(nodes_text(&with).contains("4.0G in backfillfull"));
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
        let mut states = vec![1; 70];
        states[65] = 4;
        let g = pg_grid_text(
            1,
            &pb::GetPgGridResponse {
                epoch: 9,
                states,
                ..Default::default()
            },
        );
        let lines: Vec<&str> = g.lines().collect();
        assert!(lines[0].starts_with("pool 1, map epoch 9"), "{g}");
        assert_eq!(lines[1], format!("     0  {}", ".".repeat(64)));
        assert_eq!(lines[2], "    64  .X....");
        let g = pg_grid_text(
            1,
            &pb::GetPgGridResponse {
                epoch: 9,
                states: vec![1; 4],
                usage: vec![0, 1, 128, 255],
                max_bytes: 4096,
                ..Default::default()
            },
        );
        let lines: Vec<&str> = g.lines().collect();
        assert!(lines[2].contains("the fullest PG, 4096 bytes"), "{g}");
        assert_eq!(lines[3], "     0  ?049");
    }
}
