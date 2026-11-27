//! Which CPUs the bench pins its reactors to (#450).
//!
//! The bench used to pin the client to CPU 1 and the nodes from CPU 3 up.
//! On a glidex host running Pylon, `pylond`'s polling engine spins on its
//! `--engine-cpu` (1 by default) and VMs get its `--cores`, so the client
//! got no CPU time and the bench hung. It now takes the CPUs it may run
//! on, leaves out CPU 0 (the host's housekeeping) and those `pylond`
//! uses, or takes `HS_BENCH_CPUS` as given.

/// Parses a CPU list as the kernel writes it (`0,16,32-33`).
pub fn parse_list(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let num = |x: &str| {
            x.trim()
                .parse::<usize>()
                .map_err(|_| format!("{s:?} is not a CPU list"))
        };
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (num(a)?, num(b)?);
                if a > b {
                    return Err(format!("{s:?} is not a CPU list"));
                }
                out.extend(a..=b);
            }
            None => out.push(num(part)?),
        }
    }
    if out.is_empty() {
        return Err(format!("{s:?} names no CPU"));
    }
    Ok(out)
}

/// The CPUs a `pylond` with these arguments takes: its polling engine's
/// (`--engine-cpu`, 1 when not given) and the tenant cores it hands to
/// VMs (`--cores`).
pub fn pylond_cpus(args: &[String]) -> Vec<usize> {
    let value = |flag: &str| {
        args.iter().enumerate().find_map(|(i, a)| {
            a.strip_prefix(&format!("{flag}="))
                .map(str::to_string)
                .or_else(|| (a == flag).then(|| args.get(i + 1).cloned()).flatten())
        })
    };
    let mut cpus = vec![
        value("--engine-cpu")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
    ];
    if let Some(cores) = value("--cores") {
        cpus.extend(parse_list(&cores).unwrap_or_default());
    }
    cpus
}

/// The CPUs every `pylond` running on this host takes, from its command
/// line in `/proc`.
fn running_pylond_cpus() -> Vec<usize> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut cpus = Vec::new();
    for e in dir.flatten() {
        let p = e.path();
        let is_pylond = std::fs::read_to_string(p.join("comm")).is_ok_and(|c| c.trim() == "pylond");
        if !is_pylond {
            continue;
        }
        if let Ok(raw) = std::fs::read(p.join("cmdline")) {
            let args: Vec<String> = raw
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            cpus.extend(pylond_cpus(&args));
        }
    }
    cpus
}

/// Chooses `need` CPUs. `wanted` (`HS_BENCH_CPUS`) is taken as given;
/// otherwise the allowed CPUs, less CPU 0 and those in `avoid`, in order.
pub fn choose(
    need: usize,
    allowed: &[usize],
    avoid: &[usize],
    wanted: Option<&str>,
) -> Result<Vec<usize>, String> {
    if let Some(w) = wanted {
        let cpus = parse_list(w).map_err(|e| format!("HS_BENCH_CPUS: {e}"))?;
        if cpus.len() < need {
            return Err(format!(
                "HS_BENCH_CPUS names {} CPU(s); this run needs {need}",
                cpus.len()
            ));
        }
        return Ok(cpus[..need].to_vec());
    }
    let free: Vec<usize> = allowed
        .iter()
        .copied()
        .filter(|c| *c != 0 && !avoid.contains(c))
        .collect();
    if free.len() < need {
        return Err(format!(
            "{} free CPU(s) (allowed {allowed:?}, less CPU 0 and pylond's {avoid:?}); this run needs {need}: name them in HS_BENCH_CPUS",
            free.len()
        ));
    }
    Ok(free[..need].to_vec())
}

/// `need` CPUs for this run, the first for the client; exits with a
/// message when there are too few. Call it before any thread is pinned:
/// it reads the calling thread's affinity.
pub fn for_run(need: usize) -> Vec<usize> {
    let allowed = hyperstor_reactor::allowed_cpus().unwrap_or_default();
    let wanted = std::env::var("HS_BENCH_CPUS").ok();
    match choose(need, &allowed, &running_pylond_cpus(), wanted.as_deref()) {
        Ok(cpus) => {
            eprintln!("hyperstor-bench: CPUs {cpus:?} (the client first)");
            cpus
        }
        Err(e) => {
            eprintln!("hyperstor-bench: {e}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse_as_the_kernel_writes_them() {
        assert_eq!(parse_list("0,16,32-33").unwrap(), vec![0, 16, 32, 33]);
        assert_eq!(parse_list("2-4").unwrap(), vec![2, 3, 4]);
        assert!(parse_list("4-2").is_err());
        assert!(parse_list("a").is_err());
        assert!(parse_list("").is_err());
    }

    #[test]
    fn pylond_takes_its_engine_cpu_and_its_cores() {
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            pylond_cpus(&a(
                "pylond --socket /run/pylon/pylond.sock --cores 2-7 --no-scx"
            )),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            pylond_cpus(&a("pylond --engine-cpu=9 --cores=10,12")),
            vec![9, 10, 12]
        );
        assert_eq!(pylond_cpus(&a("pylond --engine-cpu 3")), vec![3]);
    }

    #[test]
    fn the_bench_stays_off_cpu_0_and_pylonds_cpus() {
        let allowed: Vec<usize> = (0..16).collect();
        let pylond = [1, 2, 3, 4, 5, 6, 7];
        assert_eq!(
            choose(4, &allowed, &pylond, None).unwrap(),
            vec![8, 9, 10, 11]
        );
        // Without pylond: from CPU 1 up.
        assert_eq!(choose(2, &allowed, &[], None).unwrap(), vec![1, 2]);
        // Within the affinity it was started with (taskset -c 20-23).
        assert_eq!(
            choose(4, &[20, 21, 22, 23], &pylond, None).unwrap(),
            vec![20, 21, 22, 23]
        );
        // Too few: an error naming HS_BENCH_CPUS, not pinning over pylond.
        let e = choose(10, &allowed, &pylond, None).unwrap_err();
        assert!(e.contains("HS_BENCH_CPUS"), "{e}");
    }

    #[test]
    fn hs_bench_cpus_is_taken_as_given() {
        let allowed: Vec<usize> = (0..16).collect();
        assert_eq!(
            choose(2, &allowed, &[1], Some("0-1,5")).unwrap(),
            vec![0, 1]
        );
        assert!(choose(4, &allowed, &[], Some("3,4")).is_err());
        assert!(choose(1, &allowed, &[], Some("x")).is_err());
    }
}
