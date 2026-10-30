//! ublk devices (04-decisions.md D94, D95): a volume served as /dev/ublkbN by
//! `hyperstor-ublk`, used as a raw device and as a filesystem, detached and
//! served again with its data. Needs root (through `sudo -n`) and the
//! `ublk_drv` module, so it runs only with `HS_UBLK=1`.

use hyperstor_test::{Cluster, bin_dir};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

fn sudo(args: &[&str]) -> (bool, String) {
    let o = Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .expect("sudo");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.success(), text)
}

fn must(args: &[&str]) -> String {
    let (ok, out) = sudo(args);
    assert!(ok, "sudo {args:?} failed:\n{out}");
    out
}

/// A running `hyperstor-ublk attach`, stopped (and its device and mount
/// cleaned up) when dropped, so a failed test leaves nothing behind.
struct Served {
    child: Child,
    dev: String,
    mounted: Option<String>,
}

impl Served {
    fn start(c: &Cluster, id: &str, extra: &[&str]) -> Served {
        let bin = bin_dir().join("hyperstor-ublk");
        let mut child = Command::new("sudo")
            .arg("-n")
            .arg(&bin)
            .args(["attach", "--meta", &c.map, "--id", id])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("hyperstor-ublk");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let dev = line
            .split_whitespace()
            .last()
            .filter(|d| d.starts_with("/dev/ublkb"))
            .unwrap_or_else(|| panic!("no device in {line:?}"))
            .to_string();
        Served {
            child,
            dev,
            mounted: None,
        }
    }

    fn mount(&mut self, dir: &str) {
        must(&["mount", &self.dev, dir]);
        self.mounted = Some(dir.into());
    }

    fn umount(&mut self) {
        if let Some(d) = self.mounted.take() {
            must(&["umount", &d]);
        }
    }

    /// SIGTERM, as an operator stops it; it must exit cleanly.
    fn stop(mut self) {
        self.umount();
        must(&["kill", "-TERM", &self.child.id().to_string()]);
        let st = self.child.wait().unwrap();
        assert!(st.success(), "hyperstor-ublk exited with {st}");
        self.child_done();
    }

    fn child_done(&mut self) {
        self.dev.clear();
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if self.dev.is_empty() {
            return;
        }
        if let Some(d) = self.mounted.take() {
            let _ = sudo(&["umount", "-l", &d]);
        }
        let _ = sudo(&["kill", "-TERM", &self.child.id().to_string()]);
        let _ = self.child.wait();
    }
}

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

#[test]
fn a_volume_is_a_block_device_and_keeps_its_data() {
    if std::env::var("HS_UBLK").is_err() {
        eprintln!("skipped: set HS_UBLK=1 (needs sudo -n and the ublk_drv module)");
        return;
    }
    let c = Cluster::all_in_one("ublk", 3);
    let made = c
        .ctl(&["create-volume", "--size", "256M", "--name", "ublk"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let mut s = Served::start(&c, &id, &[]);
    // Attached: the lease is held, and a second attach is refused.
    let l = c.ctl(&["volumes"]).ok();
    assert!(
        !l.out.lines().any(|x| x.contains(&id) && x.ends_with(" -")),
        "{}",
        l.out
    );
    let bin = bin_dir().join("hyperstor-ublk");
    let (ok, out) = sudo(&[
        bin.to_str().unwrap(),
        "attach",
        "--meta",
        &c.map,
        "--id",
        &id,
    ]);
    assert!(!ok && out.contains("attached by"), "{out}");
    assert_eq!(
        must(&["blockdev", "--getsize64", &s.dev]).trim(),
        (256u64 << 20).to_string()
    );
    // Raw I/O, direct, at an offset across a chunk boundary.
    let data = pattern(7, 8 << 20);
    let file = c.scratch.path("pattern");
    std::fs::write(&file, &data).unwrap();
    let of = format!("of={}", s.dev);
    let iff = format!("if={}", file.display());
    must(&[
        "dd",
        &iff,
        &of,
        "bs=1M",
        "seek=2",
        "oflag=direct",
        "status=none",
    ]);
    let back = c.scratch.path("back");
    let bf = format!("of={}", back.display());
    let dif = format!("if={}", s.dev);
    must(&[
        "dd",
        &dif,
        &bf,
        "bs=1M",
        "skip=2",
        "count=8",
        "iflag=direct",
        "status=none",
    ]);
    must(&["chmod", "a+r", back.to_str().unwrap()]);
    assert!(std::fs::read(&back).unwrap() == data, "read back differs");
    // A filesystem on it.
    must(&["mkfs.ext4", "-q", "-F", &s.dev]);
    let mnt = c.scratch.path("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    s.mount(mnt.to_str().unwrap());
    let inside = mnt.join("hello");
    must(&["cp", file.to_str().unwrap(), inside.to_str().unwrap()]);
    must(&["sync"]);
    s.stop();
    let l = c.ctl(&["volumes"]).ok();
    assert!(
        l.out.lines().any(|x| x.contains(&id) && x.ends_with(" -")),
        "detached: {}",
        l.out
    );
    // Served again: the filesystem and its file are there.
    let mut s = Served::start(&c, &id, &[]);
    s.mount(mnt.to_str().unwrap());
    let again = c.scratch.path("again");
    must(&["cp", inside.to_str().unwrap(), again.to_str().unwrap()]);
    must(&["chmod", "a+r", again.to_str().unwrap()]);
    assert!(
        std::fs::read(&again).unwrap() == data,
        "file differs after re-attach"
    );
    s.stop();
    // Volumes are thin: one larger than the cluster holds fills it, and
    // the device then says so (ENOSPC), not an I/O error.
    let big = c
        .ctl(&["create-volume", "--size", "4G", "--name", "big"])
        .ok();
    let big = big.out.split_whitespace().nth(1).unwrap().to_string();
    let s = Served::start(&c, &big, &[]);
    let of = format!("of={}", s.dev);
    let (ok, out) = sudo(&[
        "dd",
        "if=/dev/zero",
        &of,
        "bs=1M",
        "count=4096",
        "oflag=direct",
        "status=none",
    ]);
    assert!(!ok && out.contains("No space left on device"), "{out}");
    s.stop();
}
