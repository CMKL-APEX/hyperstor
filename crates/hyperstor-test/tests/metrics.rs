//! Storage nodes' metrics (implementation-notes.md I49): what arrives is
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
