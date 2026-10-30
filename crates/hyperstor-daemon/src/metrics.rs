//! Storage node metrics on the admin port (spec/operations.md §16,
//! administration.md §23.3, D58, D59).
//!
//! Each reactor counts into its own [`ShardMetrics`] with relaxed atomic
//! adds, which cost no system call or lock on the I/O path, and copies its
//! engine's counters into gauges once a second. A plain thread serves them
//! in Prometheus text format (`GET /metrics`) with a liveness answer
//! (`GET /health`); nothing on a reactor waits on it.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Upper bounds of the latency buckets, in µs: powers of two from 16 µs
/// to 65 ms, then +Inf.
pub const BOUNDS_US: [u64; 13] = [
    16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];

/// A latency histogram with fixed buckets: one relaxed add per bucket hit,
/// one for the sum and one for the count.
#[derive(Default)]
pub struct Histogram {
    buckets: [AtomicU64; 14],
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, d: std::time::Duration) {
        let us = d.as_micros() as u64;
        let i = BOUNDS_US
            .iter()
            .position(|b| us <= *b)
            .unwrap_or(BOUNDS_US.len());
        self.buckets[i].fetch_add(1, Relaxed);
        self.sum_us.fetch_add(us, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }
}

/// Counters and gauges of one reactor and its device.
#[derive(Default)]
pub struct ShardMetrics {
    pub writes: AtomicU64,
    pub write_bytes: AtomicU64,
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
    pub repls: AtomicU64,
    pub repl_bytes: AtomicU64,
    pub pushes: AtomicU64,
    pub push_bytes: AtomicU64,
    /// Client writes answered with an error.
    pub write_errors: AtomicU64,
    /// Light scrub (replication.md §8.5): objects compared, objects whose
    /// copies differed, repaired, and left unresolved.
    pub scrub_objects: AtomicU64,
    pub scrub_inconsistent: AtomicU64,
    pub scrub_repaired: AtomicU64,
    pub scrub_unresolved: AtomicU64,
    /// Objects a deep scrub found with a block that failed its CRC.
    pub scrub_damaged: AtomicU64,
    /// Objects a new primary pushed to members while peering (I61).
    pub peer_pushes: AtomicU64,
    /// Times the reactor's thread was held for [`crate::STALL`] or more,
    /// which delays every ping it answers (I188).
    pub stalls: AtomicU64,
    /// From a client write's arrival to its answer, after every replica.
    pub write_latency: Histogram,
    /// From a client read's arrival to its answer.
    pub read_latency: Histogram,
    // Gauges, from the engine once a second.
    pub objects: AtomicU64,
    pub free_bytes: AtomicU64,
    pub data_bytes: AtomicU64,
    pub log_used_bytes: AtomicU64,
    pub log_bytes: AtomicU64,
    pub commits: AtomicU64,
    pub checkpoints: AtomicU64,
    pub map_epoch: AtomicU64,
}

impl ShardMetrics {
    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Relaxed);
    }
}

/// The metrics of every shard of `node`, in Prometheus text format.
pub fn render(node: u32, shards: &[(u32, Arc<ShardMetrics>)]) -> String {
    let mut o = String::new();
    let mut family = |name: &str,
                      kind: &str,
                      help: &str,
                      f: &dyn Fn(&ShardMetrics) -> Vec<(&'static str, u64)>| {
        o.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        for (dev, m) in shards {
            for (label, v) in f(m) {
                let extra = if label.is_empty() {
                    String::new()
                } else {
                    format!(",kind=\"{label}\"")
                };
                o.push_str(&format!(
                    "{name}{{node=\"{node}\",device=\"{dev}\"{extra}}} {v}\n"
                ));
            }
        }
    };
    let g = |a: &AtomicU64| a.load(Relaxed);
    family(
        "hyperstor_requests_total",
        "counter",
        "Requests served, by kind: client writes and reads, replication from primaries, backfill pushes.",
        &|m| {
            vec![
                ("write", g(&m.writes)),
                ("read", g(&m.reads)),
                ("repl", g(&m.repls)),
                ("push", g(&m.pushes)),
            ]
        },
    );
    family(
        "hyperstor_request_bytes_total",
        "counter",
        "Bytes of data in requests served, by kind.",
        &|m| {
            vec![
                ("write", g(&m.write_bytes)),
                ("read", g(&m.read_bytes)),
                ("repl", g(&m.repl_bytes)),
                ("push", g(&m.push_bytes)),
            ]
        },
    );
    family(
        "hyperstor_write_errors_total",
        "counter",
        "Client writes answered with an error.",
        &|m| vec![("", g(&m.write_errors))],
    );
    family(
        "hyperstor_reactor_stalls_total",
        "counter",
        "Times the reactor's thread was held 100 ms or more, delaying every request and ping it serves.",
        &|m| vec![("", g(&m.stalls))],
    );
    family(
        "hyperstor_scrub_objects_total",
        "counter",
        "Scrub and peering: objects compared across copies, those whose copies differed by outcome, those a deep scrub found damaged, and those a new primary pushed while peering.",
        &|m| {
            vec![
                ("compared", g(&m.scrub_objects)),
                ("inconsistent", g(&m.scrub_inconsistent)),
                ("repaired", g(&m.scrub_repaired)),
                ("unresolved", g(&m.scrub_unresolved)),
                ("damaged", g(&m.scrub_damaged)),
                ("peering", g(&m.peer_pushes)),
            ]
        },
    );
    family(
        "hyperstor_device_objects",
        "gauge",
        "Objects stored on the device.",
        &|m| vec![("", g(&m.objects))],
    );
    family(
        "hyperstor_device_free_bytes",
        "gauge",
        "Free space on the device.",
        &|m| vec![("", g(&m.free_bytes))],
    );
    family(
        "hyperstor_device_data_bytes",
        "gauge",
        "Data area of the device.",
        &|m| vec![("", g(&m.data_bytes))],
    );
    family(
        "hyperstor_log_used_bytes",
        "gauge",
        "Log in use since the last checkpoint.",
        &|m| vec![("", g(&m.log_used_bytes))],
    );
    family("hyperstor_log_bytes", "gauge", "Size of the log.", &|m| {
        vec![("", g(&m.log_bytes))]
    });
    family(
        "hyperstor_commits_total",
        "counter",
        "Engine commit rounds.",
        &|m| vec![("", g(&m.commits))],
    );
    family(
        "hyperstor_checkpoints_total",
        "counter",
        "Engine checkpoints finished.",
        &|m| vec![("", g(&m.checkpoints))],
    );
    family(
        "hyperstor_map_epoch",
        "gauge",
        "Epoch of the cluster map this reactor holds.",
        &|m| vec![("", g(&m.map_epoch))],
    );
    let mut hist = String::new();
    hist.push_str(
        "# HELP hyperstor_request_duration_seconds Time from a client request's arrival to its answer; writes include every replica.\n# TYPE hyperstor_request_duration_seconds histogram\n",
    );
    for (dev, m) in shards {
        for (kind, h) in [("write", &m.write_latency), ("read", &m.read_latency)] {
            let labels = format!("node=\"{node}\",device=\"{dev}\",kind=\"{kind}\"");
            let mut cum = 0;
            for (i, b) in h.buckets.iter().enumerate() {
                cum += b.load(Relaxed);
                let le = BOUNDS_US
                    .get(i)
                    .map_or("+Inf".to_string(), |us| format!("{}", *us as f64 / 1e6));
                hist.push_str(&format!(
                    "hyperstor_request_duration_seconds_bucket{{{labels},le=\"{le}\"}} {cum}\n"
                ));
            }
            hist.push_str(&format!(
                "hyperstor_request_duration_seconds_sum{{{labels}}} {}\nhyperstor_request_duration_seconds_count{{{labels}}} {}\n",
                h.sum_us.load(Relaxed) as f64 / 1e6,
                h.count.load(Relaxed)
            ));
        }
    }
    o.push_str(&hist);
    o
}

/// Serves `GET /metrics` and `GET /health` on `listener` from a new
/// thread, one connection at a time: scrapers are few and answers small.
pub fn serve(
    listener: std::net::TcpListener,
    node: u32,
    shards: Vec<(u32, Arc<ShardMetrics>)>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hyperstord-admin".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new(&s).read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("");
                let (status, ctype, body) = match path {
                    "/metrics" => ("200 OK", "text/plain; version=0.0.4", render(node, &shards)),
                    "/health" => ("200 OK", "text/plain", "ok\n".to_string()),
                    _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        })
        .expect("spawn the admin thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_labels_every_shard_and_kind() {
        let a = Arc::new(ShardMetrics::default());
        ShardMetrics::add(&a.writes, 3);
        ShardMetrics::add(&a.write_bytes, 12288);
        let b = Arc::new(ShardMetrics::default());
        b.free_bytes.store(1 << 30, Relaxed);
        let t = render(4, &[(4000, a), (4001, b)]);
        assert!(
            t.contains("hyperstor_requests_total{node=\"4\",device=\"4000\",kind=\"write\"} 3\n"),
            "{t}"
        );
        assert!(t.contains(
            "hyperstor_request_bytes_total{node=\"4\",device=\"4000\",kind=\"write\"} 12288\n"
        ));
        assert!(t.contains("hyperstor_device_free_bytes{node=\"4\",device=\"4001\"} 1073741824\n"));
        assert!(t.contains("hyperstor_reactor_stalls_total{node=\"4\",device=\"4001\"} 0\n"));
        assert_eq!(t.matches("# TYPE ").count(), 14);
        // Histograms are cumulative and end at +Inf with the count.
        let c = Arc::new(ShardMetrics::default());
        for us in [10, 20, 100, 100_000] {
            c.write_latency
                .observe(std::time::Duration::from_micros(us));
        }
        let t = render(1, &[(1000, c)]);
        let l = "node=\"1\",device=\"1000\",kind=\"write\"";
        assert!(
            t.contains(&format!("_bucket{{{l},le=\"0.000016\"}} 1\n")),
            "{t}"
        );
        assert!(
            t.contains(&format!("_bucket{{{l},le=\"0.000128\"}} 3\n")),
            "{t}"
        );
        assert!(
            t.contains(&format!("_bucket{{{l},le=\"0.065536\"}} 3\n")),
            "{t}"
        );
        assert!(
            t.contains(&format!("_bucket{{{l},le=\"+Inf\"}} 4\n")),
            "{t}"
        );
        assert!(t.contains(&format!("_count{{{l}}} 4\n")), "{t}");
    }
}
