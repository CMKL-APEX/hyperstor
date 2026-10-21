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
        assert_eq!(t.matches("# TYPE ").count(), 10);
    }
}
