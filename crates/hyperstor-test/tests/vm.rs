//! Virtual machines boot with a hyperstor volume as their disk, through
//! `hyperstor-vhost` (11-frontends.md §11.1, 04-decisions.md D118): Cloud
//! Hypervisor writes it from a guest, then QEMU reads it back. The guest
//! is the host's kernel with a busybox initramfs built here, whose init
//! checks the disk and prints `HS` lines on the serial console. Then each
//! hypervisor again, on a fresh volume, while `hyperstor-vhost` restarts
//! under it (D143, host check G6). Last, a guest's write held while the
//! cluster is full (D183, host check G46): the guest writes a file on
//! ext4 while new blocks are refused, space is freed after a while, and
//! the write lands.
//!
//! Needs `HS_VM=1`, `/dev/kvm` (directly, or through `sg kvm`), a kernel
//! image the test can read (`HS_VM_KERNEL`, default
//! `~/.cache/hyperstor-test/vmlinuz`; `/boot`'s is root-only on Ubuntu),
//! a static `/usr/bin/busybox`, `cloud-hypervisor` and
//! `qemu-system-x86_64`. `HS_G46=30,60,120,300` holds the write that long
//! instead of 5 s (minutes each, under both hypervisors) and lets one
//! more go past `--hold-full`; the test prints what each guest's kernel
//! logged.

use hyperstor_test::Cluster;
use hyperstor_test::vhost::Service;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the guest runs: report the disk, then (with `hs=write`) four
/// writers at once at 4, 8, 12 and 16 MiB, then each region's MD5 read
/// past the page cache.
const INIT: &str = r#"#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mode=$(tr ' ' '\n' < /proc/cmdline | grep '^hs=' | cut -d= -f2)
echo "HS serial $(cat /sys/block/vda/serial)"
echo "HS size $(cat /sys/block/vda/size)"
echo "HS queues $(ls /sys/block/vda/mq | wc -l)"
echo "HS blocks $(cat /sys/block/vda/queue/logical_block_size) $(cat /sys/block/vda/queue/physical_block_size)"
if [ "$mode" = restart ]; then
  # The test restarts hyperstor-vhost when it sees "HS paused"; the
  # second write runs across that, and both are read back after.
  yes "hyperstor 1" | head -c 4194304 | dd of=/dev/vda bs=64k seek=64 conv=fsync 2>/dev/null
  # Straight to the device, 4 KiB a request, already under way when the
  # service goes: some requests are taken and not yet answered.
  (yes "hyperstor 2" | head -c 16777216 | dd of=/dev/vda bs=4k seek=2048 oflag=direct 2>/dev/null) &
  sleep 0.3
  echo "HS paused"
  wait
  echo "HS wrote"
  echo "HS big $(dd if=/dev/vda bs=64k skip=128 count=256 iflag=direct 2>/dev/null | md5sum | cut -d' ' -f1)"
fi
if [ "$mode" = write ]; then
  for i in 1 2 3 4; do
    (yes "hyperstor $i" | head -c 4194304 | dd of=/dev/vda bs=64k seek=$((i * 64)) conv=fsync 2>/dev/null) &
  done
  wait
  echo "HS wrote"
fi
if [ "$mode" = hold ]; then
  # The cluster refuses new blocks: the file's blocks and the journal's
  # are held by hyperstor-vhost until the test frees space.
  # Warnings too on the console: hung tasks, timeouts, ext4's errors.
  echo 7 > /proc/sys/kernel/printk
  # Present only when the driver has a timeout handler.
  echo "HS io_timeout $(cat /sys/block/vda/queue/io_timeout 2>/dev/null || echo none)"
  echo "HS hung_task $(cat /proc/sys/kernel/hung_task_timeout_secs)"
  mkdir /mnt
  mount -t ext4 /dev/vda /mnt && echo "HS mounted"
  yes "hyperstor 6" | head -c 1048576 > /tmp/f
  echo "HS writing $(cut -d' ' -f1 /proc/uptime)"
  if dd if=/tmp/f of=/mnt/f bs=64k conv=fsync 2>/dev/null; then
    echo "HS synced $(cut -d' ' -f1 /proc/uptime)"
  else
    echo "HS failed $(cut -d' ' -f1 /proc/uptime)"
  fi
  grep -q ' /mnt ext4 ro' /proc/mounts && echo "HS read-only"
  echo 3 > /proc/sys/vm/drop_caches
  echo "HS file $(md5sum < /mnt/f | cut -d' ' -f1)"
  umount /mnt && echo "HS unmounted"
fi
echo 3 > /proc/sys/vm/drop_caches
for i in 1 2 3 4; do
  echo "HS sum $i $(dd if=/dev/vda bs=64k skip=$((i * 64)) count=64 2>/dev/null | md5sum | cut -d' ' -f1)"
done
echo "HS done"
poweroff -f
"#;

/// A newc cpio archive: what the kernel unpacks as its initramfs.
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
    let busybox = std::fs::read("/usr/bin/busybox").expect("a static /usr/bin/busybox");
    let mut c = Cpio(Vec::new(), 0);
    for d in ["bin", "dev", "proc", "sys", "tmp"] {
        c.entry(d, 0o040755, (0, 0), &[]);
    }
    // The console the kernel opens for init's output, before devtmpfs.
    c.entry("dev/console", 0o020600, (5, 1), &[]);
    c.entry("bin/busybox", 0o100755, (0, 0), &busybox);
    c.entry("init", 0o100755, (0, 0), INIT.as_bytes());
    std::fs::write(path, c.finish()).unwrap();
}

/// Runs `cmd` with access to /dev/kvm (through `sg kvm` if this process
/// lacks it) for at most 120 s; returns the `HS` lines of its console.
fn boot(name: &str, cmd: &[String]) -> Vec<String> {
    boot_with(name, cmd, |_| {})
}

/// [`boot`], calling `on_line` with each `HS` line as it arrives.
fn boot_with(name: &str, cmd: &[String], on_line: impl FnMut(&str)) -> Vec<String> {
    let all = boot_for(name, cmd, Duration::from_secs(120), on_line);
    all.iter()
        .filter_map(|l| l.find("HS ").map(|i| l[i..].trim_end().to_string()))
        .collect()
}

/// [`boot_with`] for at most `limit`, returning every console line.
fn boot_for(
    name: &str,
    cmd: &[String],
    limit: Duration,
    mut on_line: impl FnMut(&str),
) -> Vec<String> {
    let direct = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    let mut c = if direct {
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        c
    } else {
        let mut c = Command::new("sg");
        let quoted: Vec<String> = cmd.iter().map(|a| format!("'{a}'")).collect();
        c.args(["kvm", "-c", &quoted.join(" ")]);
        c
    };
    // A group of its own: `sg` runs the hypervisor as its child, and a
    // hung one is killed with it.
    let mut child = std::os::unix::process::CommandExt::process_group(&mut c, 0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut err_pipe = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut err_pipe, &mut err);
        err
    });
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let t = Instant::now();
    let mut all = Vec::new();
    let mut take = |all: &mut Vec<String>, l: String| {
        if let Some(i) = l.find("HS ") {
            on_line(l[i..].trim_end());
        }
        all.push(l);
    };
    while child.try_wait().unwrap().is_none() {
        if t.elapsed() > limit {
            // SAFETY: kill on our own child's process group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            let err = stderr.join().unwrap();
            while let Ok(l) = rx.try_recv() {
                all.push(l);
            }
            panic!(
                "{name}: no poweroff within {limit:?}\n--- console\n{}\n--- stderr\n{err}",
                all.join("\n")
            );
        }
        while let Ok(l) = rx.recv_timeout(Duration::from_millis(100)) {
            take(&mut all, l);
        }
    }
    reader.join().unwrap();
    while let Ok(l) = rx.try_recv() {
        take(&mut all, l);
    }
    let err = stderr.join().unwrap();
    if !all.iter().any(|l| l.trim_end().ends_with("HS done")) {
        panic!(
            "{name}: the guest didn't finish\n--- console\n{}\n--- stderr\n{err}",
            all.join("\n")
        );
    }
    all
}

/// The MD5 of what the guest's writer `i` writes.
fn expected(i: u32) -> String {
    expected_of(i, 4 << 20)
}

/// The MD5 of the first `len` bytes of writer `i`'s pattern.
fn expected_of(i: u32, len: u64) -> String {
    let o = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "yes 'hyperstor {i}' | head -c {len} | md5sum | cut -d' ' -f1"
        ))
        .output()
        .unwrap();
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

/// The command line booting the test's guest under `hypervisor`
/// (`cloud-hypervisor` or `qemu`), from the volume served at `socket`
/// over two queues, with `hs=mode`.
fn machine(hypervisor: &str, kernel: &str, initrd: &str, socket: &str, mode: &str) -> Vec<String> {
    let cmdline = format!("console=ttyS0 quiet panic=-1 hs={mode}");
    let args: Vec<String> = if hypervisor == "cloud-hypervisor" {
        vec![
            "cloud-hypervisor".into(),
            "--kernel".into(),
            kernel.into(),
            "--initramfs".into(),
            initrd.into(),
            "--cmdline".into(),
            cmdline,
            "--cpus".into(),
            "boot=2".into(),
            "--memory".into(),
            "size=512M,shared=on".into(),
            "--disk".into(),
            format!("vhost_user=true,socket={socket},num_queues=2,queue_size=128"),
            "--serial".into(),
            "tty".into(),
            "--console".into(),
            "off".into(),
        ]
    } else {
        vec![
            "qemu-system-x86_64".into(),
            "-enable-kvm".into(),
            "-cpu".into(),
            "host".into(),
            "-smp".into(),
            "2".into(),
            "-m".into(),
            "512M".into(),
            "-object".into(),
            "memory-backend-memfd,id=mem,size=512M,share=on".into(),
            "-numa".into(),
            "node,memdev=mem".into(),
            "-chardev".into(),
            format!("socket,id=disk,path={socket},reconnect=1"),
            "-device".into(),
            "vhost-user-blk-pci,chardev=disk,num-queues=2".into(),
            "-kernel".into(),
            kernel.into(),
            "-initrd".into(),
            initrd.into(),
            "-append".into(),
            cmdline,
            "-nographic".into(),
            "-nodefaults".into(),
            "-serial".into(),
            "stdio".into(),
            "-no-reboot".into(),
        ]
    };
    args
}

fn kernel() -> Option<PathBuf> {
    let k = std::env::var("HS_VM_KERNEL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/hyperstor-test/vmlinuz")
        });
    std::fs::File::open(&k).is_ok().then_some(k)
}

#[test]
fn virtual_machines_boot_with_a_volume_as_their_disk() {
    if std::env::var("HS_VM").is_err() {
        eprintln!(
            "skipped: set HS_VM=1 (needs /dev/kvm, a kernel image, busybox and both hypervisors)"
        );
        return;
    }
    let kernel = kernel().expect("a readable kernel image (HS_VM_KERNEL)");
    let c = Cluster::all_in_one("vm", 3);
    let made = c
        .ctl(&["create-volume", "--size", "256M", "--name", "vm"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let mut svc = Service::start(&c, "vhost-vm", &["--queues", "2"]);
    let r = svc.attach(&id, "attach");
    let socket = r["ok"]["socket"]
        .as_str()
        .unwrap_or_else(|| panic!("{r}"))
        .to_string();
    let initrd = c.scratch.path("initrd.cpio");
    initramfs(&initrd);
    let (k, i) = (
        kernel.to_str().unwrap().to_string(),
        initrd.to_str().unwrap().to_string(),
    );
    let ch = machine("cloud-hypervisor", &k, &i, &socket, "write");
    let wrote = boot("cloud-hypervisor", &ch);
    eprintln!("cloud-hypervisor: {wrote:?}");
    let has = |lines: &[String], l: &str| {
        assert!(lines.iter().any(|x| x == l), "no {l:?} in {lines:#?}");
    };
    has(&wrote, "HS serial disk-under-test");
    has(&wrote, &format!("HS size {}", (256u64 << 20) / 512));
    has(&wrote, "HS queues 2");
    // 512e (D126): sectors as logical blocks, the volume's 4 KiB blocks as
    // physical ones.
    has(&wrote, "HS blocks 512 4096");
    has(&wrote, "HS wrote");
    for n in 1..=4 {
        has(&wrote, &format!("HS sum {n} {}", expected(n)));
    }
    // The volume stayed attached; QEMU connects to the same socket and
    // reads what Cloud Hypervisor's guest wrote.
    let qemu = machine("qemu", &k, &i, &socket, "read");
    let read = boot("qemu", &qemu);
    eprintln!("qemu: {read:?}");
    has(&read, "HS queues 2");
    for n in 1..=4 {
        has(&read, &format!("HS sum {n} {}", expected(n)));
    }
    // hyperstor-vhost restarts under a running VM (D143; host check G6):
    // the guest's write across the restart lands, and so does the one
    // before it. A fresh volume for each, so nothing earlier hides a
    // lost write.
    for (name, cmd) in [("cloud-hypervisor", &ch), ("qemu", &qemu)] {
        let made = c
            .ctl(&["create-volume", "--size", "64M", "--name", "restart"])
            .ok();
        let vol = made.out.split_whitespace().nth(1).unwrap().to_string();
        let r = svc.attach(&vol, "attach");
        let fresh = r["ok"]["socket"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_string();
        let cmd: Vec<String> = cmd
            .iter()
            .map(|a| {
                a.replace(&socket, &fresh)
                    .replace("hs=write", "hs=restart")
                    .replace("hs=read", "hs=restart")
            })
            .collect();
        let mut restarted = false;
        let lines = boot_with(name, &cmd, |l| {
            if l == "HS paused" {
                svc.restart();
                restarted = true;
            }
        });
        eprintln!("{name} across a restart: {lines:?}");
        assert!(restarted, "{name}: the guest never paused");
        has(&lines, "HS wrote");
        has(&lines, &format!("HS sum 1 {}", expected(1)));
        has(&lines, &format!("HS big {}", expected_of(2, 16 << 20)));
    }
    svc.stop();
}

/// What a guest's kernel logged about a held write: hung tasks, block
/// timeouts and resets, I/O errors and ext4's complaints.
fn kernel_said(console: &[String]) -> Vec<String> {
    const SIGNS: [&str; 9] = [
        "blocked for more than",
        "hung_task",
        "timed out",
        "timeout",
        "reset",
        "I/O error",
        "EXT4-fs",
        "Buffer I/O",
        "Aborting journal",
    ];
    console
        .iter()
        .filter(|l| !l.contains("HS ") && SIGNS.iter().any(|s| l.contains(s)))
        .map(|l| l.trim_end().to_string())
        .collect()
}

#[test]
fn a_guest_waits_on_a_write_held_while_full() {
    if std::env::var("HS_VM").is_err() {
        eprintln!(
            "skipped: set HS_VM=1 (needs /dev/kvm, a kernel image, busybox and both hypervisors)"
        );
        return;
    }
    let kernel = kernel().expect("a readable kernel image (HS_VM_KERNEL)");
    // How long each write is held before space is freed, in seconds.
    let g46 = std::env::var("HS_G46").ok();
    let holds: Vec<u64> = g46
        .as_deref()
        .map(|v| v.split(',').map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![5]);
    let c = Cluster::all_in_one("vmhold", 3);
    let volume = |name: &str, size: &str| {
        let made = c
            .ctl(&["create-volume", "--size", size, "--name", name])
            .ok();
        made.out.split_whitespace().nth(1).unwrap().to_string()
    };
    let write = |id: &str, file: &Path, at: u64| {
        c.ctl(&[
            "write-volume",
            "--id",
            id,
            "--file",
            file.to_str().unwrap(),
            "--offset",
            &at.to_string(),
            "--sparse",
            "--map",
            &c.map,
        ])
    };
    // An ext4 volume for each guest, written while there is space: only
    // the blocks mkfs wrote something into, so the journal and the free
    // blocks are new to the cluster, as on a disk that was never full.
    let image = c.scratch.path("ext4.img");
    let mkfs = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-E", "lazy_itable_init=0,lazy_journal_init=0"])
        .arg(&image)
        .arg("128M")
        .status()
        .unwrap();
    assert!(mkfs.success(), "mkfs.ext4");
    let mut runs = Vec::new();
    for hold in holds
        .iter()
        .map(|h| Some(*h))
        .chain(g46.as_ref().map(|_| None))
    {
        for hv in ["cloud-hypervisor", "qemu"] {
            let id = volume("held", "128M");
            write(&id, &image, 0).ok();
            runs.push((hv, hold, id));
        }
    }
    // Every device past 52 %, then full at 51 %: new blocks are refused.
    let used = || -> f64 {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        let n = |d: &serde_json::Value, f: &str| {
            d[f].as_str()
                .and_then(|t| t.parse::<f64>().ok())
                .or(d[f].as_f64())
        };
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|n| n["devices"].as_array().unwrap().clone())
            .filter_map(|d| Some(100.0 * (1.0 - n(&d, "free_bytes")? / n(&d, "total_bytes")?)))
            .fold(0.0, f64::max)
    };
    let filler = volume("filler", "1G");
    let chunk = c.scratch.path("chunk.raw");
    let pattern: Vec<u8> = (0..32u32 << 20).map(|i| (i % 251) as u8 + 1).collect();
    std::fs::write(&chunk, &pattern).unwrap();
    let mut next = 0u64;
    while used() < 52.0 {
        let had = used();
        for _ in 0..4 {
            write(&filler, &chunk, next << 25).ok();
            next += 1;
        }
        assert!(next << 25 < 1 << 30, "the volume filled first");
        hyperstor_test::wait_for("a space report", 15, || (used() > had).then_some(()));
    }
    let full = |pct: &str| {
        c.ctl(&[
            "full-ratios",
            "--nearfull",
            "50",
            "--backfillfull",
            "50.5",
            "--full",
            pct,
        ])
        .ok();
    };
    let probe = volume("probe", "1G");
    let probes = std::cell::Cell::new(0u64);
    let refused = || {
        full("51");
        hyperstor_test::wait_for("new blocks refused", 20, || {
            probes.set(probes.get() + 1);
            (write(&probe, &chunk, probes.get() << 25).code != 0).then_some(())
        });
    };
    let initrd = c.scratch.path("initrd.cpio");
    initramfs(&initrd);
    let (k, i) = (
        kernel.to_str().unwrap().to_string(),
        initrd.to_str().unwrap().to_string(),
    );
    let svc = Service::start(&c, "vhost-hold", &["--queues", "2", "--hold-full", "3600"]);
    let short = Service::start(&c, "vhost-short", &["--queues", "2", "--hold-full", "30"]);
    let mut report = Vec::new();
    for (hv, hold, id) in &runs {
        refused();
        let r = match hold {
            Some(_) => svc.attach(id, "attach"),
            None => short.attach(id, "attach"),
        };
        let socket = r["ok"]["socket"].as_str().unwrap_or_else(|| panic!("{r}"));
        let cmd = machine(hv, &k, &i, socket, "hold");
        let limit = Duration::from_secs(hold.unwrap_or(30) + 120);
        let console = std::thread::scope(|s| {
            boot_for(hv, &cmd, limit, |l| {
                if l.starts_with("HS writing")
                    && let Some(h) = *hold
                {
                    s.spawn(move || {
                        std::thread::sleep(Duration::from_secs(h));
                        full("95");
                    });
                }
            })
        });
        let hs: Vec<&str> = console
            .iter()
            .filter_map(|l| l.find("HS ").map(|i| l[i..].trim_end()))
            .collect();
        let at = |what: &str| -> Option<f64> {
            hs.iter()
                .find_map(|l| l.strip_prefix(what)?.trim().parse().ok())
        };
        let waited = at("HS synced ")
            .or(at("HS failed "))
            .zip(at("HS writing "))
            .map(|(b, a)| b - a);
        let said = kernel_said(&console);
        let held = match hold {
            Some(h) => format!("freed after {h} s"),
            None => "past --hold-full 30".into(),
        };
        eprintln!("{hv}, {held}: waited {waited:?} s; {hs:?}; kernel: {said:#?}");
        report.push(format!(
            "{hv}, {held}: waited {waited:?} s, kernel: {said:?}"
        ));
        if hold.is_some() {
            assert!(hs.contains(&"HS mounted"), "{hv}: {hs:?}");
            assert!(
                hs.iter().any(|l| l.starts_with("HS synced")),
                "{hv}: {hs:?}"
            );
            assert!(!hs.contains(&"HS read-only"), "{hv}: {hs:?}");
            assert!(
                hs.contains(&format!("HS file {}", expected_of(6, 1 << 20)).as_str()),
                "{hv}: {hs:?}"
            );
            assert!(
                waited.unwrap() >= hold.unwrap() as f64 - 1.0,
                "{hv}: {waited:?}"
            );
        }
    }
    eprintln!("G46:\n{}", report.join("\n"));
    svc.stop();
    short.stop();
}
