//! The leader's view of the cluster's load (27-performance.md §30,
//! D215–D219): each device's reports (`Msg::PerfReport`) kept in memory,
//! the newest window of each PG and 15 minutes of each device at 5-s steps,
//! and the Advisor's rules over them. Nothing is replicated or persisted:
//! a new leader starts empty and fills within a window. History beyond 15
//! minutes is Prometheus's (each node's metrics port).

use hyperstor_proto::perf::{Drive, PerfReport, PgHeat, Service};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Samples kept per device: 15 minutes at 5-s steps.
pub const KEEP: usize = 180;
/// A PG's window counts for this long after it was reported; after that
/// the PG reads as idle (its primary moved, or stopped reporting).
const PG_FRESH: Duration = Duration::from_secs(20);
/// A device's samples are dropped once it has been silent this long.
const DEVICE_GONE: Duration = Duration::from_secs(15 * 60);

/// One window of one device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub unix_ms: u64,
    pub window_ms: u32,
    /// Client requests its PGs served as primary, and their bytes.
    pub reads: u32,
    pub writes: u32,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// The device's own reads and commits.
    pub service: [Service; 2],
    pub inflight_max: u32,
}

impl Sample {
    pub fn per_sec(&self, n: u64) -> f64 {
        if self.window_ms == 0 {
            0.0
        } else {
            n as f64 * 1000.0 / f64::from(self.window_ms)
        }
    }
}

#[derive(Debug, Default, Clone)]
struct DevicePerf {
    samples: VecDeque<Sample>,
    drive: Option<Drive>,
    seen: Option<Instant>,
}

/// A PG's newest window, with the device that served it.
#[derive(Debug, Clone)]
pub struct PgWindow {
    pub device: u32,
    pub window_ms: u32,
    pub heat: PgHeat,
    at: Instant,
}

impl PgWindow {
    pub fn rate(&self, n: u64) -> f64 {
        if self.window_ms == 0 {
            0.0
        } else {
            n as f64 * 1000.0 / f64::from(self.window_ms)
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Perf {
    devices: BTreeMap<u32, DevicePerf>,
    pgs: HashMap<(u32, u32), PgWindow>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Perf {
    /// Takes a device's report.
    pub fn note(&mut self, r: PerfReport) {
        self.note_at(r, Instant::now(), now_ms());
    }

    fn note_at(&mut self, r: PerfReport, at: Instant, unix_ms: u64) {
        let mut s = Sample {
            unix_ms,
            window_ms: r.window_ms,
            inflight_max: r.inflight_max,
            ..Default::default()
        };
        for sv in r.service {
            let i = usize::from(sv.op.min(1));
            s.service[i] = sv;
        }
        s.service[0].op = Service::READ;
        s.service[1].op = Service::COMMIT;
        for p in r.pgs {
            s.reads = s.reads.saturating_add(p.reads);
            s.writes = s.writes.saturating_add(p.writes);
            s.read_bytes += p.read_bytes;
            s.write_bytes += p.write_bytes;
            self.pgs.insert(
                (p.pool, p.pg),
                PgWindow {
                    device: r.device,
                    window_ms: r.window_ms,
                    heat: p,
                    at,
                },
            );
        }
        let d = self.devices.entry(r.device).or_default();
        d.samples.push_back(s);
        while d.samples.len() > KEEP {
            d.samples.pop_front();
        }
        if r.drive.is_some() {
            d.drive = r.drive;
        }
        d.seen = Some(at);
        // Forget what has gone quiet.
        self.pgs.retain(|_, w| at.duration_since(w.at) < PG_FRESH);
        self.devices
            .retain(|_, d| d.seen.is_some_and(|t| at.duration_since(t) < DEVICE_GONE));
    }

    /// The fresh window of each PG of `pool` that served anything.
    pub fn pool(&self, pool: u32) -> Vec<&PgWindow> {
        let now = Instant::now();
        let mut v: Vec<&PgWindow> = self
            .pgs
            .iter()
            .filter(|((p, _), w)| *p == pool && now.duration_since(w.at) < PG_FRESH)
            .map(|(_, w)| w)
            .collect();
        v.sort_by_key(|w| w.heat.pg);
        v
    }

    /// Every fresh PG window.
    pub fn all_pgs(&self) -> Vec<&PgWindow> {
        let now = Instant::now();
        self.pgs
            .values()
            .filter(|w| now.duration_since(w.at) < PG_FRESH)
            .collect()
    }

    /// The devices that reported, with their samples (oldest first) and
    /// their drive.
    pub fn devices(&self) -> Vec<(u32, Vec<Sample>, Option<Drive>)> {
        self.devices
            .iter()
            .map(|(id, d)| (*id, d.samples.iter().cloned().collect(), d.drive.clone()))
            .collect()
    }

    /// Volumes by requests per second over the fresh PG windows, summed
    /// across their PGs: (lineage, branch, requests/s, bytes/s), most
    /// first. Each PG names only its top few, so a volume spread thinly
    /// over many PGs can be missed: a lower bound.
    pub fn top_volumes(&self, limit: usize) -> Vec<(u128, u32, f64, f64)> {
        let mut sum: HashMap<(u128, u32), (f64, f64)> = HashMap::new();
        for w in self.all_pgs() {
            for (l, b, n, by) in &w.heat.top {
                let e = sum.entry((*l, *b)).or_default();
                e.0 += w.rate(u64::from(*n));
                e.1 += w.rate(*by);
            }
        }
        let mut v: Vec<_> = sum
            .into_iter()
            .map(|((l, b), (n, by))| (l, b, n, by))
            .collect();
        v.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    }
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

/// One of the Advisor's findings (D219): what was seen, on what, from
/// which numbers, and what to do. Nothing is done for the operator.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// The rule, stable: what an operator snoozes.
    pub rule: &'static str,
    pub severity: Severity,
    /// "device 1002", "volume …", "pool 1".
    pub subject: String,
    pub title: String,
    pub evidence: String,
    pub action: String,
    /// A command that does it, if one exists.
    pub command: String,
}

/// What the rules look at beyond the reports.
pub struct Context<'a> {
    /// Each device's kind ("nvme", "ssd", "hdd", "file"), for comparing a
    /// device with its peers.
    pub kinds: &'a BTreeMap<u32, String>,
    /// Volumes by (lineage, branch): id, name, pool.
    pub volumes: &'a HashMap<(u128, u32), (u128, String, u32)>,
    /// Data pools: id, name, and whether all its devices are NVMe.
    pub pools: &'a [(u32, String, bool)],
}

/// The Advisor's thresholds (27-performance.md §30.5), from what the drive
/// makers publish and the pilot's first numbers; to be tuned with more.
pub mod limits {
    /// A device is slow when its p99 is this many times its peers' median
    /// p99, and over [`SLOW_FLOOR_US`].
    pub const SLOW_TIMES: u32 = 4;
    pub const SLOW_FLOOR_US: u32 = 2048;
    /// A device carries a hot spot when its primaries serve this share of
    /// the cluster's requests and that is this many times a fair share.
    pub const HOT_SHARE_TIMES: f64 = 2.0;
    /// Requests per second below which load isn't worth a finding.
    pub const BUSY_FLOOR: f64 = 200.0;
    /// A volume is hot when it sends this share of the cluster's requests.
    pub const HOT_VOLUME_SHARE: f64 = 0.25;
    /// Wear: percentage used.
    pub const WEAR_WARN: u8 = 80;
    /// Spare: points above the drive's own threshold.
    pub const SPARE_MARGIN: u8 = 10;
}

fn ms(us: u32) -> String {
    if us >= 1000 {
        format!("{:.1} ms", f64::from(us) / 1000.0)
    } else {
        format!("{us} µs")
    }
}

/// Merges a device's last `n` samples' service times.
fn recent(samples: &[Sample], n: usize) -> (Service, Service, f64, u32) {
    let mut out = [Service::default(), Service::default()];
    let mut ops = 0u64;
    let mut ms_total = 0u64;
    let mut inflight = 0;
    for s in samples.iter().rev().take(n) {
        for (o, x) in out.iter_mut().zip(&s.service) {
            o.count += x.count;
            o.bytes += x.bytes;
            o.max_us = o.max_us.max(x.max_us);
            for (a, b) in o.buckets.iter_mut().zip(x.buckets) {
                *a += b;
            }
        }
        ops += u64::from(s.reads) + u64::from(s.writes);
        ms_total += u64::from(s.window_ms);
        inflight = inflight.max(s.inflight_max);
    }
    let rate = if ms_total == 0 {
        0.0
    } else {
        ops as f64 * 1000.0 / ms_total as f64
    };
    let [r, c] = out;
    (r, c, rate, inflight)
}

/// The Advisor's findings, most serious first (D219).
pub fn findings(p: &Perf, cx: &Context) -> Vec<Finding> {
    use limits::*;
    let mut out = Vec::new();
    let devices = p.devices();
    // The last minute of each device.
    let recent: Vec<(u32, Service, Service, f64, u32)> = devices
        .iter()
        .map(|(d, s, _)| {
            let (r, c, rate, inflight) = recent(s, 12);
            (*d, r, c, rate, inflight)
        })
        .collect();

    // A device much slower than its peers of the same kind.
    for (op, name) in [(0usize, "reads"), (1, "commits")] {
        let mut by_kind: BTreeMap<&str, Vec<(u32, u32)>> = BTreeMap::new();
        for (d, r, c, _, _) in &recent {
            let s = if op == 0 { r } else { c };
            if s.count >= 20 {
                let kind = cx.kinds.get(d).map_or("file", String::as_str);
                by_kind
                    .entry(kind)
                    .or_default()
                    .push((*d, s.quantile_us(0.99)));
            }
        }
        for (kind, mut v) in by_kind {
            if v.len() < 3 {
                continue;
            }
            v.sort_by_key(|x| x.1);
            let median = v[v.len() / 2].1.max(1);
            for (d, p99) in v {
                if p99 >= SLOW_FLOOR_US && p99 >= median.saturating_mul(SLOW_TIMES) {
                    out.push(Finding {
                        rule: "slow-device",
                        severity: Severity::Warn,
                        subject: format!("device {d}"),
                        title: format!("Device {d} is slow at {name}"),
                        evidence: format!(
                            "p99 {} over the last minute, against {} for its {kind} peers",
                            ms(p99),
                            ms(median)
                        ),
                        action: "Check its drive (temperature, errors, link) and its \
                                 node's load; if it stays slow, take it out and replace it"
                            .into(),
                        command: format!("hyperstorctl perf --device {d}"),
                    });
                }
            }
        }
    }

    // Primaries that carry a hot spot.
    let total: f64 = recent.iter().map(|x| x.3).sum();
    let active = recent.iter().filter(|x| x.3 > 0.0).count().max(1);
    if total >= BUSY_FLOOR && active >= 2 {
        let fair = total / active as f64;
        for (d, _, _, rate, _) in &recent {
            if *rate >= fair * HOT_SHARE_TIMES {
                let mut hot: Vec<&PgWindow> =
                    p.all_pgs().into_iter().filter(|w| w.device == *d).collect();
                hot.sort_by(|a, b| {
                    let r =
                        |w: &PgWindow| w.rate(u64::from(w.heat.reads) + u64::from(w.heat.writes));
                    r(b).total_cmp(&r(a))
                });
                let names: Vec<String> = hot
                    .iter()
                    .take(3)
                    .map(|w| format!("{}.{}", w.heat.pool, w.heat.pg))
                    .collect();
                out.push(Finding {
                    rule: "hot-primary",
                    severity: Severity::Info,
                    subject: format!("device {d}"),
                    title: format!(
                        "Device {d} serves {:.0}% of all requests",
                        rate / total * 100.0
                    ),
                    evidence: format!(
                        "{rate:.0} requests/s as primary against a fair share of {fair:.0}; \
                         its busiest PGs: {}",
                        names.join(", ")
                    ),
                    action: "Move the hot volumes to another pool, or spread them over more \
                             PGs; moving primaries is planned (D219)"
                        .into(),
                    command: format!("hyperstorctl top-volumes; hyperstorctl perf --device {d}"),
                });
            }
        }
    }

    // A volume that sends a large share of the load, and a faster pool to
    // move it to.
    let vols = p.top_volumes(5);
    let vol_total: f64 = p
        .all_pgs()
        .iter()
        .map(|w| w.rate(u64::from(w.heat.reads) + u64::from(w.heat.writes)))
        .sum();
    if vol_total >= BUSY_FLOOR {
        for (l, b, rate, bytes) in vols {
            if rate / vol_total < HOT_VOLUME_SHARE {
                continue;
            }
            let Some((id, name, pool)) = cx.volumes.get(&(l, b)) else {
                continue;
            };
            let faster = cx.pools.iter().find(|(p, _, nvme)| {
                *nvme && p != pool && !cx.pools.iter().any(|(q, _, n)| q == pool && *n)
            });
            let (action, command) = match faster {
                Some((fp, fname, _)) => (
                    format!(
                        "Move it to pool {fp} ({fname}), on NVMe only; detach it first, \
                         or copy a snapshot of it there"
                    ),
                    format!("hyperstorctl move-volume --id {id:032x} --pool {fp} --wait 3600"),
                ),
                None => (
                    "Its pool is the fastest there is: spread it (smaller chunks in a new pool) \
                     or limit it once QoS limits exist"
                        .into(),
                    String::new(),
                ),
            };
            out.push(Finding {
                rule: "hot-volume",
                severity: Severity::Info,
                subject: format!("volume {id:032x}"),
                title: format!(
                    "Volume {name} sends {:.0}% of all requests",
                    rate / vol_total * 100.0
                ),
                evidence: format!(
                    "{rate:.0} requests/s, {:.1} MiB/s, in pool {pool}",
                    bytes / f64::from(1 << 20)
                ),
                action,
                command,
            });
        }
    }

    // What the drives say of themselves.
    for (d, samples, drive) in &devices {
        let Some(dr) = drive else { continue };
        if let Some((w, wm, s, sm)) = dr.link
            && (w < wm || s < sm)
        {
            out.push(Finding {
                rule: "pcie-link",
                severity: Severity::Warn,
                subject: format!("device {d}"),
                title: format!("Device {d}'s PCIe link runs below what it can"),
                evidence: format!(
                    "x{w} at {:.1} GT/s, though it can do x{wm} at {:.1} GT/s",
                    f64::from(s) / 10.0,
                    f64::from(sm) / 10.0
                ),
                action: "Reseat the drive or move it to a slot with more lanes; check the \
                         BIOS's link settings"
                    .into(),
                command: String::new(),
            });
        }
        if dr.numa.0 >= 0 && dr.numa.1 >= 0 && dr.numa.0 != dr.numa.1 {
            out.push(Finding {
                rule: "numa",
                severity: Severity::Info,
                subject: format!("device {d}"),
                title: format!("Device {d} is served from another NUMA node"),
                evidence: format!(
                    "the drive hangs off node {}, its reactor runs on node {}",
                    dr.numa.0, dr.numa.1
                ),
                action: "Pin the device's reactor to a core of the drive's node \
                         (hyperstord --cpus)"
                    .into(),
                command: String::new(),
            });
        }
        let Some(s) = &dr.smart else { continue };
        if s.critical_warning != 0 {
            out.push(Finding {
                rule: "drive-critical",
                severity: Severity::Error,
                subject: format!("device {d}"),
                title: format!("Device {d}'s drive raises a critical warning"),
                evidence: format!("critical warning bits {:#04x}", s.critical_warning),
                action: "Read the bits (spare, temperature, reliability, read-only, \
                         volatile backup); plan to take the device out"
                    .into(),
                command: format!("hyperstorctl perf --device {d}"),
            });
        }
        if s.media_errors > 0 {
            out.push(Finding {
                rule: "media-errors",
                severity: Severity::Warn,
                subject: format!("device {d}"),
                title: format!("Device {d}'s drive has media errors"),
                evidence: format!("{} media and data integrity errors", s.media_errors),
                action: "Scrub finds what they damaged; watch whether they grow".into(),
                command: String::new(),
            });
        }
        if s.percentage_used >= WEAR_WARN
            || s.available_spare < s.spare_threshold.saturating_add(SPARE_MARGIN)
        {
            // Days left at the write rate of the samples kept.
            let written: u64 = samples.iter().map(|x| x.write_bytes).sum();
            let span_ms: u64 = samples.iter().map(|x| u64::from(x.window_ms)).sum();
            out.push(Finding {
                rule: "wear",
                severity: Severity::Warn,
                subject: format!("device {d}"),
                title: format!("Device {d}'s drive is wearing out"),
                evidence: format!(
                    "{}% of its rated endurance used, spare {}% (threshold {}%){}",
                    s.percentage_used,
                    s.available_spare,
                    s.spare_threshold,
                    if span_ms > 0 {
                        format!(
                            "; written {:.1} MiB/s lately",
                            written as f64 * 1000.0 / span_ms as f64 / f64::from(1 << 20)
                        )
                    } else {
                        String::new()
                    }
                ),
                action: "Plan its replacement; lower its weight so new data goes elsewhere".into(),
                command: format!("hyperstorctl weight --device {d} --weight 0"),
            });
        }
        if s.throttle_seconds.iter().any(|x| *x > 0) || s.warning_temp_minutes > 0 {
            out.push(Finding {
                rule: "thermal",
                severity: Severity::Warn,
                subject: format!("device {d}"),
                title: format!("Device {d}'s drive has throttled for heat"),
                evidence: format!(
                    "{} s and {} s at the two throttle levels, {} min above its warning \
                     temperature, now {} °C (since the drive was made)",
                    s.throttle_seconds[0],
                    s.throttle_seconds[1],
                    s.warning_temp_minutes,
                    i32::from(s.temperature_k) - 273
                ),
                action: "Check the host's airflow and the drive's heatsink".into(),
                command: String::new(),
            });
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.subject.cmp(&b.subject)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_proto::perf::{BUCKETS, Smart};

    fn service(op: u8, count: u32, bucket: usize) -> Service {
        let mut b = [0; BUCKETS];
        b[bucket] = count;
        Service {
            op,
            count,
            bytes: u64::from(count) * 4096,
            buckets: b,
            max_us: hyperstor_proto::perf::BUCKETS_US
                .get(bucket)
                .copied()
                .unwrap_or(100_000),
        }
    }

    fn report(device: u32, pgs: Vec<PgHeat>, read_bucket: usize) -> PerfReport {
        PerfReport {
            device,
            window_ms: 5000,
            pgs,
            service: vec![
                service(Service::READ, 100, read_bucket),
                service(Service::COMMIT, 50, 4),
            ],
            inflight_max: 4,
            drive: None,
        }
    }

    fn pg(pool: u32, pg: u32, reads: u32, top: Vec<(u128, u32, u32, u64)>) -> PgHeat {
        PgHeat {
            pool,
            pg,
            reads,
            writes: 0,
            read_bytes: u64::from(reads) * 4096,
            write_bytes: 0,
            top,
        }
    }

    #[test]
    fn reports_keep_fifteen_minutes_and_the_newest_pg_windows() {
        let mut p = Perf::default();
        let t0 = Instant::now();
        for i in 0..200u64 {
            let r = report(1000, vec![pg(1, 3, i as u32, vec![])], 3);
            p.note_at(r, t0, i * 5000);
        }
        let d = p.devices();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1.len(), KEEP);
        assert_eq!(d[0].1[0].unix_ms, 20 * 5000, "the oldest dropped");
        let w = p.pool(1);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].heat.reads, 199);
        assert!((w[0].rate(1000) - 200.0).abs() < 1e-9);
        assert!(p.pool(2).is_empty());
    }

    #[test]
    fn volumes_are_summed_across_their_pgs() {
        let mut p = Perf::default();
        p.note(report(
            1000,
            vec![
                pg(
                    1,
                    1,
                    500,
                    vec![(7, 0, 400, 400 << 12), (8, 0, 100, 100 << 12)],
                ),
                pg(
                    1,
                    2,
                    500,
                    vec![(7, 0, 300, 300 << 12), (9, 2, 200, 200 << 12)],
                ),
            ],
            3,
        ));
        let v = p.top_volumes(2);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].0, v[0].1), (7, 0));
        assert!((v[0].2 - 140.0).abs() < 1e-9, "700 requests in 5 s");
        assert_eq!((v[1].0, v[1].1), (9, 2));
    }

    fn cx_parts() -> (
        BTreeMap<u32, String>,
        HashMap<(u128, u32), (u128, String, u32)>,
        Vec<(u32, String, bool)>,
    ) {
        let kinds = [1000, 1001, 2000, 3000]
            .into_iter()
            .map(|d| (d, "nvme".to_string()))
            .collect();
        let volumes = [((7u128, 0u32), (7u128, "db".to_string(), 1u32))]
            .into_iter()
            .collect();
        let pools = vec![(1, "vol".into(), false), (3, "fast".into(), true)];
        (kinds, volumes, pools)
    }

    #[test]
    fn the_advisor_names_a_slow_device_a_hot_primary_and_a_hot_volume() {
        let mut p = Perf::default();
        // Four devices; 3000's reads take 8 ms at p99 against 64 µs, and
        // 1000 serves most requests, most of them for volume 7.
        p.note(report(
            1000,
            vec![pg(1, 1, 9000, vec![(7, 0, 8000, 8000 << 12)])],
            2,
        ));
        p.note(report(1001, vec![pg(1, 2, 300, vec![])], 2));
        p.note(report(2000, vec![pg(1, 3, 300, vec![])], 2));
        p.note(report(3000, vec![pg(1, 4, 300, vec![])], 9));
        let (kinds, volumes, pools) = cx_parts();
        let f = findings(
            &p,
            &Context {
                kinds: &kinds,
                volumes: &volumes,
                pools: &pools,
            },
        );
        let rules: Vec<(&str, &str)> = f.iter().map(|x| (x.rule, x.subject.as_str())).collect();
        assert!(rules.contains(&("slow-device", "device 3000")), "{f:#?}");
        assert!(rules.contains(&("hot-primary", "device 1000")), "{f:#?}");
        let hot = f
            .iter()
            .find(|x| x.rule == "hot-volume")
            .expect("hot volume");
        assert!(hot.command.contains("move-volume") && hot.command.contains("--pool 3"));
        assert!(!rules.iter().any(|r| r.1 == "device 1001"), "{f:#?}");
    }

    #[test]
    fn the_advisor_reads_the_drives() {
        let mut p = Perf::default();
        let mut r = report(1000, vec![], 2);
        r.drive = Some(Drive {
            link: Some((2, 4, 80, 80)),
            numa: (1, 0),
            smart: Some(Smart {
                critical_warning: 0x02,
                percentage_used: 85,
                available_spare: 100,
                spare_threshold: 10,
                throttle_seconds: [30, 0],
                temperature_k: 345,
                ..Default::default()
            }),
            ..Default::default()
        });
        p.note(r);
        let (kinds, volumes, pools) = cx_parts();
        let f = findings(
            &p,
            &Context {
                kinds: &kinds,
                volumes: &volumes,
                pools: &pools,
            },
        );
        let rules: Vec<&str> = f.iter().map(|x| x.rule).collect();
        for r in ["drive-critical", "pcie-link", "numa", "wear", "thermal"] {
            assert!(rules.contains(&r), "{r}: {rules:?}");
        }
        assert_eq!(f[0].severity, Severity::Error, "most serious first");
        // A healthy drive says nothing.
        let mut p = Perf::default();
        let mut r = report(1000, vec![], 2);
        r.drive = Some(Drive {
            link: Some((4, 4, 80, 80)),
            numa: (0, 0),
            smart: Some(Smart {
                available_spare: 100,
                spare_threshold: 10,
                ..Default::default()
            }),
            ..Default::default()
        });
        p.note(r);
        assert!(
            findings(
                &p,
                &Context {
                    kinds: &kinds,
                    volumes: &volumes,
                    pools: &pools,
                },
            )
            .is_empty()
        );
    }
}
