//! Each PG's usage (D155): primaries report the bytes they hold, and
//! `pg-grid --usage` shows them as a digit per PG.

use hyperstor_test::vhost::pattern;
use hyperstor_test::*;

#[test]
fn the_grid_shows_where_data_is() {
    // SAFETY: this test binary runs this one test; nothing else reads the
    // environment meanwhile. Primaries report every 300 ms.
    unsafe { std::env::set_var("HS_PG_USAGE_MS", "300") };
    let c = Cluster::all_in_one("usage", 3);
    let grid = || {
        let g = c.ctl(&["pg-grid", "--pool", "1", "--usage"]).ok();
        g.out
            .lines()
            .skip_while(|l| !l.starts_with("usage"))
            .skip(1)
            .flat_map(|l| {
                l.split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .chars()
                    .collect::<Vec<_>>()
            })
            .collect::<String>()
    };
    // Every primary reports, an empty pool at the lowest level.
    let empty = wait_for("every PG's usage", 30, || {
        let g = grid();
        (g.len() == 64 && !g.contains('?')).then_some(g)
    });
    assert_eq!(empty, "0".repeat(64));

    // 16 chunks of 4 MiB, written whole: at most 16 PGs hold them, the
    // fullest at 9, and the rest still empty.
    let made = c
        .ctl(&["create-volume", "--size", "64M", "--name", "full"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let src = c.scratch.path("full.raw");
    std::fs::write(&src, pattern(7, 64 << 20)).unwrap();
    c.ctl(&["write-volume", "--id", &id, "--file", src.to_str().unwrap()])
        .ok();
    let g = wait_for("the written PGs' usage", 30, || {
        let g = grid();
        g.contains('9').then_some(g)
    });
    let held = g.chars().filter(|&d| d != '0').count();
    assert!((1..=16).contains(&held), "{g}");
    let r = c.ctl(&["pg-grid", "--pool", "1", "--usage"]).ok();
    assert!(r.has("the fullest PG, "), "{}", r.out);
    // Without --usage, the states alone.
    assert!(!c.ctl(&["pg-grid", "--pool", "1"]).ok().has("usage"));
}
