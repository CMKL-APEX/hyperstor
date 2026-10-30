//! Storage nodes' metrics (01-implementation-notes.md I49): what arrives is
//! counted, and every write is answered once, timed or counted as an
//! error.

use hyperstor_test::*;

#[test]
fn requests_are_counted_and_timed() {
    let c = Cluster::all_in_one("metrics", 3);
    c.ctl(&["io-test", "--mib", "8"]).ok();
    // At least the 8 MiB written; more if a write was refused and retried.
    let bytes = c.metric_sum("hyperstor_request_bytes_total", "kind=\"write\"");
    assert!(bytes >= 8 << 20, "{bytes}");
    let writes = c.metric_sum("hyperstor_requests_total", "kind=\"write\"");
    assert!(writes >= 8, "1 MiB requests: {writes}");
    assert_eq!(
        c.metric_sum("hyperstor_request_duration_seconds_count", "kind=\"write\"")
            + c.metric_sum("hyperstor_write_errors_total", ""),
        writes
    );
    // Reads too, and replicas saw two copies of each write.
    assert!(c.metric_sum("hyperstor_requests_total", "kind=\"read\"") >= 8);
    assert!(c.metric_sum("hyperstor_requests_total", "kind=\"repl\"") >= 2 * 8);
    // Gauges arrive within a second; every node holds the same map epoch.
    wait_for("gauges", 5, || {
        (c.metric_sum("hyperstor_device_objects", "") > 0).then_some(())
    });
    let epochs: Vec<String> = (0..3)
        .map(|n| {
            c.metrics(n)
                .lines()
                .find(|l| l.starts_with("hyperstor_map_epoch{"))
                .unwrap()
                .rsplit(' ')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(epochs.windows(2).all(|w| w[0] == w[1]), "{epochs:?}");
}

/// Every metric the shipped Grafana dashboard queries is one the storage
/// nodes really serve, and the installer's Prometheus targets list every
/// node's metrics port: the dashboard can't drift from the daemon.
#[test]
fn grafana_dashboard_and_prometheus_targets_match_the_daemon() {
    let c = Cluster::all_in_one("grafana", 2);
    c.ctl(&["io-test", "--mib", "2"]).ok();
    let served = c.metrics(0);
    let dash = std::fs::read_to_string(workspace().join("deploy/grafana/hyperstor.json")).unwrap();
    let mut names: Vec<&str> = dash
        .match_indices("hyperstor_")
        .map(|(i, _)| {
            let rest = &dash[i..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .filter(|n| n.len() > "hyperstor_".len())
        .collect();
    names.sort();
    names.dedup();
    assert!(names.len() >= 10, "{names:?}");
    for n in &names {
        assert!(
            served.contains(&format!("\n{n}{{")),
            "the dashboard queries {n}, which no node serves"
        );
    }
    let targets =
        std::fs::read_to_string(c.root.join("etc/hyperstor/prometheus-targets.json")).unwrap();
    for n in 0..2 {
        assert!(
            targets.contains(&format!("\"127.0.0.1:{}\"", c.admin_port + n)),
            "{targets}"
        );
    }
    assert!(
        targets.contains(&format!("\"cluster\": \"{}\"", c.cluster_id)),
        "{targets}"
    );
}
