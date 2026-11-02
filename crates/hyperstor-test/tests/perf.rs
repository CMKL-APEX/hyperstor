//! Load and drives (27-performance.md §30, D218–D222): primaries report
//! each PG's requests and their top volumes, devices their service time,
//! and the leader answers `pg-grid --heat`, `perf`, `top-volumes` and
//! `advise`.

use hyperstor_test::vhost::pattern;
use hyperstor_test::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn the_leader_shows_where_the_load_is() {
    // SAFETY: this test binary runs this one test; nothing else reads the
    // environment meanwhile. Devices report every 300 ms.
    unsafe { std::env::set_var("HS_PERF_MS", "300") };
    let c = Cluster::all_in_one("perf", 3);
    let made = c
        .ctl(&["create-volume", "--size", "64M", "--name", "hot"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let src = c.scratch.path("hot.raw");
    std::fs::write(&src, pattern(3, 64 << 20)).unwrap();
    c.ctl(&["write-volume", "--id", &id, "--file", src.to_str().unwrap()])
        .ok();

    // Reads of the volume go on in the background meanwhile: a window
    // counts for 20 s.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, out) = (stop.clone(), c.scratch.path("back.raw"));
        let (bin, api, pki, id) = (
            c.bin.join("hyperstorctl"),
            c.api.clone(),
            c.pki(),
            id.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::process::Command::new(&bin)
                    .args(["--api", &api, "--tls", pki.to_str().unwrap()])
                    .args([
                        "read-volume",
                        "--id",
                        &id,
                        "--size",
                        "64M",
                        "--out",
                        out.to_str().unwrap(),
                    ])
                    .output();
            }
        })
    };

    // The grid: some PGs read, as digits; most not.
    let g = wait_for("PGs with reads", 30, || {
        let g = c.ctl(&["pg-grid", "--pool", "1", "--heat", "read"]).ok();
        g.out
            .lines()
            .skip(1)
            .any(|l| l.chars().any(|ch| ch.is_ascii_digit() && ch != '0') && l.len() > 8)
            .then_some(g)
    });
    assert!(g.has("reads per PG on its primary"), "{}", g.out);
    // The busiest PG names the volume behind it.
    let cells: String = g
        .out
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    let busiest = cells.find('9').expect("a busiest PG");
    let detail = c
        .ctl(&[
            "pg-grid",
            "--pool",
            "1",
            "--heat",
            "read",
            "--pg",
            &busiest.to_string(),
        ])
        .ok();
    assert!(detail.has("hot") && detail.has(&id), "{}", detail.out);

    // Volumes, devices and the Advisor.
    let top = c.ctl(&["top-volumes"]).ok();
    assert!(
        top.out.lines().nth(1).is_some_and(|l| l.contains(&id)),
        "{}",
        top.out
    );
    let perf = c.ctl(&["perf"]).ok();
    assert!(
        perf.has("READ P99") && perf.out.lines().count() == 4,
        "{}",
        perf.out
    );
    let reads: Vec<f64> = perf
        .out
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(3)?.parse().ok())
        .collect();
    assert!(reads.iter().any(|r| *r > 0.0), "{}", perf.out);
    let one = c.ctl(&["perf", "--device", "1000"]).ok();
    assert!(
        one.has("device 1000 on node 1, file") && one.has("AGO"),
        "{}",
        one.out
    );
    c.ctl(&["perf", "--device", "77"]).expect(1);
    c.ctl(&["advise"]).ok();
    let json = c.ctl(&["advise", "--json"]).ok();
    assert!(json.out.starts_with('{'), "{}", json.out);
    // Flags only where they belong.
    c.ctl(&["volumes", "--heat", "read"]).expect(2);
    c.ctl(&["pg-grid", "--heat", "read", "--usage"]).expect(2);
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
}
