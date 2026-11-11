//! fio in Cloud Hypervisor guests on volumes served by `hyperstor-vhost`:
//! what one VM gets at QD1 and QD32, and how 4 KiB I/O at QD32 scales
//! with the number of VMs and of `hyperstor-vhost` processes
//! (02-benchmarks.md, 2026-11-11). A measurement, not a check: it prints
//! `RESULT` lines and asserts only that the guests finish.
//!
//! Needs what `vm.rs` needs (`/dev/kvm`, a kernel image, a static
//! busybox, `cloud-hypervisor`) and `/usr/bin/fio`, which is copied into
//! the guest's initramfs with its libraries. Environment:
//!
//! - `HS_VMB=1` runs it;
//! - `HS_VMB_JOBS`: the single-VM jobs, `rw-bs-depth` separated by commas
//!   (empty for none);
//! - `HS_VMB_VMS`: the VM counts for scaling (default `1,2,4,8`);
//! - `HS_VMB_SVCS`: `hyperstor-vhost` processes, VMs spread over them
//!   (default 1);
//! - `HS_VMB_RT`: seconds per job (default 10);
//! - `HS_VMB_LABEL`: the first word of each `RESULT` line;
//! - `HS_VHOST_SPIN_US`: passed to `hyperstor-vhost`, the reactor's
//!   completion spin.
//!
//! The test cluster keeps three copies on three 1 GiB nodes: the guests
//! write 128 MiB (one VM) or 64 MiB each (scaling) so it never fills,
//! which would hold writes for `--hold-full`. Put the scratch directory
//! (`target/tmp/e2e`) on tmpfs to measure without the disk.

use hyperstor_test::Cluster;
use hyperstor_test::vhost::Service;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const INIT: &str = r#"#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
export LD_LIBRARY_PATH=/lib
jobs=$(tr ' ' '\n' < /proc/cmdline | grep '^hsjobs=' | cut -d= -f2 | tr ',' ' ')
sz=$(tr ' ' '\n' < /proc/cmdline | grep '^hssize=' | cut -d= -f2)
rt=$(tr ' ' '\n' < /proc/cmdline | grep '^hsrt=' | cut -d= -f2)
f="--filename=/dev/vda --direct=1 --ioengine=libaio --size=${sz} --group_reporting --output-format=terse --terse-version=3"
/lib/ld-linux-x86-64.so.2 /bin/fio $f --name=fill --rw=write --bs=1M --iodepth=8 >/dev/null 2>&1
echo "HS filled"
for j in $jobs; do
  rw=${j%%-*}; rest=${j#*-}; bs=${rest%%-*}; qd=${rest#*-}
  out=$(/lib/ld-linux-x86-64.so.2 /bin/fio $f --name=$j --rw=$rw --bs=$bs --iodepth=$qd --time_based --runtime=$rt --ramp_time=2 2>/dev/null)
  echo "HS fio $j $out"
done
echo "HS done"
poweroff -f
"#;

struct Cpio(Vec<u8>, u32);
impl Cpio {
    fn entry(&mut self, name: &str, mode: u32, rdev: (u32, u32), data: &[u8]) {
        self.1 += 1;
        let fields = [
            self.1,
            mode,
            0,
            0,
            1,
            0,
            data.len() as u32,
            0,
            0,
            rdev.0,
            rdev.1,
            name.len() as u32 + 1,
            0,
        ];
        self.0.extend(b"070701");
        for f in fields {
            self.0.extend(format!("{f:08x}").as_bytes());
        }
        self.0.extend(name.as_bytes());
        self.0.push(0);
        self.pad();
        self.0.extend(data);
        self.pad();
    }
    fn pad(&mut self) {
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
    }
    fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, (0, 0), &[]);
        self.0
    }
}

fn initramfs(path: &Path) {
    let mut c = Cpio(Vec::new(), 0);
    for d in ["bin", "dev", "proc", "sys", "tmp", "lib"] {
        c.entry(d, 0o040755, (0, 0), &[]);
    }
    c.entry("dev/console", 0o020600, (5, 1), &[]);
    c.entry(
        "bin/busybox",
        0o100755,
        (0, 0),
        &std::fs::read("/usr/bin/busybox").unwrap(),
    );
    c.entry(
        "bin/fio",
        0o100755,
        (0, 0),
        &std::fs::read("/usr/bin/fio").unwrap(),
    );
    let ldd = Command::new("ldd").arg("/usr/bin/fio").output().unwrap();
    for l in String::from_utf8(ldd.stdout).unwrap().lines() {
        let p = match l.split("=>").nth(1) {
            Some(r) => r.split_whitespace().next().map(str::to_string),
            None => l
                .split_whitespace()
                .next()
                .filter(|p| p.starts_with('/'))
                .map(str::to_string),
        };
        if let Some(p) = p {
            let name = Path::new(&p)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            c.entry(
                &format!("lib/{name}"),
                0o100755,
                (0, 0),
                &std::fs::read(&p).unwrap(),
            );
        }
    }
    c.entry("init", 0o100755, (0, 0), INIT.as_bytes());
    std::fs::write(path, c.finish()).unwrap();
}

fn machine(kernel: &str, initrd: &str, socket: &str, jobs: &str, rt: u32, sz: &str) -> Vec<String> {
    [
        "cloud-hypervisor",
        "--kernel",
        kernel,
        "--initramfs",
        initrd,
        "--cmdline",
        &format!("console=ttyS0 quiet panic=-1 hsjobs={jobs} hsrt={rt} hssize={sz}"),
        "--cpus",
        "boot=2",
        "--memory",
        "size=1G,shared=on",
        "--disk",
        &format!("vhost_user=true,socket={socket},num_queues=2,queue_size=128"),
        "--serial",
        "tty",
        "--console",
        "off",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Boots `cmd` (at most `limit`); returns the `HS fio` lines.
fn boot(cmd: &[String], limit: Duration) -> Vec<String> {
    let mut child =
        std::os::unix::process::CommandExt::process_group(Command::new(&cmd[0]).args(&cmd[1..]), 0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let t = Instant::now();
    let mut all = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => all.push(l),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
        if t.elapsed() > limit {
            // SAFETY: our own child's process group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            panic!("no poweroff in {limit:?}:\n{}", all.join("\n"));
        }
    }
    let _ = child.wait();
    assert!(
        all.iter().any(|l| l.contains("HS done")),
        "{}",
        all.join("\n")
    );
    all.iter()
        .filter_map(|l| l.find("HS fio ").map(|i| l[i + 7..].trim_end().to_string()))
        .collect()
}

/// (job, IOPS, mean completion latency µs, p99 µs) from a terse v3 line.
fn parse(line: &str) -> (String, f64, f64, f64) {
    let (job, terse) = line.split_once(' ').unwrap();
    let f: Vec<&str> = terse.trim().split(';').collect();
    // terse v3: read block at 5.., write at 46..; iops at +2, clat mean
    // at +10 (µs), clat percentiles from +12 ("99.000000%=N" at +12+12).
    let base = if job.contains("write") { 46 } else { 5 };
    let iops: f64 = f[base + 2].parse().unwrap();
    let lat_mean: f64 = f[base + 34].parse().unwrap();
    let p99 = f[base + 12 + 12]
        .split('=')
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    (job.to_string(), iops, lat_mean, p99)
}

/// User and system time of `pid`, in clock ticks.
fn cpu_ticks(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = &s[s.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

#[test]
fn fio_in_guests() {
    if std::env::var("HS_VMB").is_err() {
        return;
    }
    let kernel = std::env::var("HOME").unwrap() + "/.cache/hyperstor-test/vmlinuz";
    let label = std::env::var("HS_VMB_LABEL").unwrap_or_else(|_| "base".into())
        + &std::env::var("HS_VMB_SVCS")
            .map(|s| format!("-svc{s}"))
            .unwrap_or_default();
    let c = Cluster::all_in_one("vmb", 3);
    let vms: Vec<usize> = std::env::var("HS_VMB_VMS")
        .unwrap_or_else(|_| "1,2,4,8".into())
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    let maxv = *vms.iter().max().unwrap();
    let nsvc: usize = std::env::var("HS_VMB_SVCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut svcs = Vec::new();
    for k in 0..nsvc {
        svcs.push(Service::start(&c, &format!("vmb{k}"), &["--queues", "2"]));
    }
    let pids: Vec<u32> = svcs.iter().map(Service::pid).collect();
    let vhost_ticks = || pids.iter().map(|&p| cpu_ticks(p)).sum::<u64>();
    let mut sockets = Vec::new();
    for i in 0..maxv.max(1) {
        let svc = &mut svcs[i % nsvc];
        let made = c
            .ctl(&[
                "create-volume",
                "--size",
                "300M",
                "--name",
                &format!("b{i}"),
            ])
            .ok();
        let id = made.out.split_whitespace().nth(1).unwrap().to_string();
        let r = svc.attach(&id, "attach");
        sockets.push(r["ok"]["socket"].as_str().unwrap().to_string());
    }
    let initrd = c.scratch.path("initrd.cpio");
    initramfs(&initrd);
    let i = initrd.to_str().unwrap().to_string();
    let rt: u32 = std::env::var("HS_VMB_RT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    // SAFETY: sysconf reads a constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    // One VM: the latency and throughput jobs.
    let single = std::env::var("HS_VMB_JOBS").unwrap_or_else(|_| {
        "randread-4k-1,randwrite-4k-1,randread-4k-32,randwrite-4k-32,read-1M-4,write-1M-4".into()
    });
    if !single.is_empty() {
        let t0 = vhost_ticks();
        let w = Instant::now();
        let lines = boot(
            &machine(&kernel, &i, &sockets[0], &single, rt, "128M"),
            Duration::from_secs(600),
        );
        let cpu = (vhost_ticks() - t0) as f64 / hz;
        for l in &lines {
            let (job, iops, lat, p99) = parse(l);
            println!("RESULT {label} vms=1 {job} iops={iops:.0} lat_us={lat:.1} p99_us={p99:.0}");
        }
        println!(
            "RESULT {label} vms=1 vhost_cpu_s={cpu:.1} wall_s={:.1}",
            w.elapsed().as_secs_f64()
        );
    }
    // Scaling: N VMs at once, each 4 KiB random reads and then writes at
    // depth 32.
    for &n in &vms {
        for job in ["randread-4k-32", "randwrite-4k-32"] {
            let t0 = vhost_ticks();
            let w = Instant::now();
            let hs: Vec<_> = (0..n)
                .map(|v| {
                    let cmd = machine(&kernel, &i, &sockets[v], job, rt, "64M");
                    std::thread::spawn(move || boot(&cmd, Duration::from_secs(600)))
                })
                .collect();
            let mut total = 0.0;
            let mut lat = 0.0;
            for h in hs {
                for l in h.join().unwrap() {
                    let (_, iops, l_us, _) = parse(&l);
                    total += iops;
                    lat += l_us;
                }
            }
            let wall = w.elapsed().as_secs_f64();
            let cpu = (vhost_ticks() - t0) as f64 / hz;
            println!(
                "RESULT {label} vms={n} {job} iops={total:.0} lat_us={:.1} vhost_cpu_s={cpu:.1} wall_s={wall:.1} vhost_cores={:.2}",
                lat / n as f64,
                cpu / wall
            );
        }
    }
    for s in svcs {
        s.stop();
    }
}
