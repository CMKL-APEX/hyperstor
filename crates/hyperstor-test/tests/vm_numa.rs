//! Host check G59 (D292; 11-frontends.md §11.1): does `hyperstor-vhost`
//! see, through its own mapping of a guest's memory, the NUMA policy the
//! hypervisor bound that memory with, and how do an unbound guest's pages
//! spread over the nodes as they are faulted in? Boots Cloud Hypervisor
//! and QEMU guests, bound to a node and unbound, on a volume served by
//! `hyperstor-vhost`, and samples `/proc/<pid>/numa_maps` of the service
//! (the kernel's `get_vma_policy`, as `get_mempolicy(MPOL_F_ADDR)` uses).
//! A measurement: it prints `G59` lines.
//!
//! `HS_G59=1` runs it; `HS_G59_NODE` is the node to bind to (default 3);
//! `HS_G59_FILL` the MiB the guest writes after boot (default 768).
//! Needs what `vm.rs` needs.

use hyperstor_test::Cluster;
use hyperstor_test::vhost::Service;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Boots, writes `hsfill` MiB into its root filesystem (faulting guest
/// pages in), writes 48 MiB to the disk and reads it back three times,
/// and powers off.
const INIT: &str = r#"#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t devtmpfs dev /dev
fill=$(tr ' ' '\n' < /proc/cmdline | grep '^hsfill=' | cut -d= -f2)
echo "HS up $(cut -d' ' -f1 /proc/uptime)"
dd if=/dev/vda of=/dev/null bs=64k count=16 2>/dev/null
dd if=/dev/zero of=/fill bs=1M count=$fill 2>/dev/null
echo "HS filled $(cut -d' ' -f1 /proc/uptime)"
for r in 1 2 3; do
  dd if=/dev/zero of=/dev/vda bs=1M count=48 oflag=direct 2>/dev/null
  dd if=/dev/vda of=/dev/null bs=1M count=48 iflag=direct 2>/dev/null
  echo "HS io round $r $(cut -d' ' -f1 /proc/uptime)"
  sleep 1
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
}

fn initramfs(path: &Path) {
    let mut c = Cpio(Vec::new(), 0);
    for d in ["bin", "dev", "proc", "tmp"] {
        c.entry(d, 0o040755, (0, 0), &[]);
    }
    c.entry("dev/console", 0o020600, (5, 1), &[]);
    c.entry(
        "bin/busybox",
        0o100755,
        (0, 0),
        &std::fs::read("/usr/bin/busybox").unwrap(),
    );
    c.entry("init", 0o100755, (0, 0), INIT.as_bytes());
    c.entry("TRAILER!!!", 0, (0, 0), &[]);
    std::fs::write(path, c.0).unwrap();
}

/// The guest-memory lines of `pid`'s numa_maps: (policy, pages per node).
fn guest_memory(pid: u32) -> Vec<(String, Vec<(u32, u64)>)> {
    let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/numa_maps")) else {
        return Vec::new();
    };
    maps.lines()
        .filter(|l| l.contains("memfd:"))
        .map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let nodes = w
                .iter()
                .filter_map(|f| {
                    let (n, c) = f.strip_prefix('N')?.split_once('=')?;
                    Some((n.parse().ok()?, c.parse().ok()?))
                })
                .collect();
            (format!("{} {}", w[1], w[2]), nodes)
        })
        .collect()
}

fn summary(m: &[(String, Vec<(u32, u64)>)]) -> String {
    m.iter()
        .map(|(p, n)| {
            let total: u64 = n.iter().map(|x| x.1).sum();
            let per: Vec<String> = n
                .iter()
                .map(|(k, c)| format!("N{k}={}MiB", c * 4 / 1024))
                .collect();
            format!("[{p} {}MiB: {}]", total * 4 / 1024, per.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn guest_memory_policy_seen_by_vhost() {
    if std::env::var("HS_G59").is_err() {
        return;
    }
    let node: u32 = std::env::var("HS_G59_NODE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let fill: u32 = std::env::var("HS_G59_FILL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768);
    let kernel = std::env::var("HOME").unwrap() + "/.cache/hyperstor-test/vmlinuz";
    let c = Cluster::all_in_one("g59", 3);
    let made = c
        .ctl(&["create-volume", "--size", "64M", "--name", "g59"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let svc = Service::start(&c, "g59", &["--queues", "2"]);
    let pid = svc.pid();
    let r = svc.attach(&id, "attach");
    let socket = r["ok"]["socket"].as_str().unwrap().to_string();
    let initrd = c.scratch.path("initrd.cpio");
    initramfs(&initrd);
    let i = initrd.to_str().unwrap();
    let cmdline = format!("console=ttyS0 quiet panic=-1 hsfill={fill}");
    let disk_ch = format!("vhost_user=true,socket={socket},num_queues=2,queue_size=128");
    let ch = |memory: &[String]| -> Vec<String> {
        let mut a: Vec<String> = [
            "cloud-hypervisor",
            "--kernel",
            &kernel,
            "--initramfs",
            i,
            "--cmdline",
            &cmdline,
            "--cpus",
            "boot=2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        a.extend(memory.iter().cloned());
        a.extend(
            ["--disk", &disk_ch, "--serial", "tty", "--console", "off"]
                .iter()
                .map(|s| s.to_string()),
        );
        a
    };
    let qemu = |backend: &str| -> Vec<String> {
        [
            "qemu-system-x86_64",
            "-enable-kvm",
            "-cpu",
            "host",
            "-smp",
            "2",
            "-m",
            "1G",
            "-object",
            backend,
            "-numa",
            "node,memdev=mem",
            "-chardev",
            &format!("socket,id=disk,path={socket}"),
            "-device",
            "vhost-user-blk-pci,chardev=disk,num-queues=2",
            "-kernel",
            &kernel,
            "-initrd",
            i,
            "-append",
            &cmdline,
            "-nographic",
            "-nodefaults",
            "-serial",
            "stdio",
            "-no-reboot",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let cases: Vec<(String, Vec<String>)> = vec![
        (
            format!("ch-bound-{node}"),
            ch(&s(&[
                "--memory",
                "size=0",
                "--memory-zone",
                &format!("id=mem0,size=1G,shared=on,host_numa_node={node}"),
            ])),
        ),
        (
            "ch-unbound".into(),
            ch(&s(&["--memory", "size=1G,shared=on"])),
        ),
        (
            format!("qemu-bound-{node}"),
            qemu(&format!(
                "memory-backend-memfd,id=mem,size=1G,share=on,host-nodes={node},policy=bind"
            )),
        ),
        (
            "qemu-unbound".into(),
            qemu("memory-backend-memfd,id=mem,size=1G,share=on"),
        ),
    ];
    for (name, cmd) in cases {
        let t = Instant::now();
        let mut child = Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<(f64, String)>();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(k) = l.find("HS ") {
                    let _ = tx.send((t.elapsed().as_secs_f64(), l[k..].trim_end().to_string()));
                }
            }
        });
        let mut last = String::new();
        let mut first_seen = None;
        while child.try_wait().unwrap().is_none() {
            assert!(
                t.elapsed() < Duration::from_secs(120),
                "{name}: no poweroff"
            );
            let m = guest_memory(pid);
            let whole = summary(&guest_memory(child.id()));
            if !m.is_empty() && first_seen.is_none() {
                first_seen = Some(t.elapsed().as_secs_f64());
            }
            let now = format!("vhost {} | hypervisor {}", summary(&m), whole);
            if now != last {
                println!("G59 {name} t={:.2}s {now}", t.elapsed().as_secs_f64());
                last = now;
            }
            while let Ok((at, l)) = rx.try_recv() {
                println!("G59 {name} t={at:.2}s guest: {l}");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        while let Ok((at, l)) = rx.try_recv() {
            println!("G59 {name} t={at:.2}s guest: {l}");
        }
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err);
        println!(
            "G59 {name} mapped at t={first_seen:?}; exit {:?}; stderr: {}",
            child.wait().unwrap().code(),
            err.lines().take(3).collect::<Vec<_>>().join(" | ")
        );
    }
    svc.stop();
}
