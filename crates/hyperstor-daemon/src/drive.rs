//! What a device's drive is and says of itself (27-performance.md §30.3,
//! D220, D221): model, firmware, temperature, PCIe link and NUMA node from
//! sysfs, which `hyperstord` can read as its own user; and the NVMe SMART
//! / health log, which needs `CAP_SYS_ADMIN`, from `hyperstor-smart`, a
//! root helper on a Unix socket that answers for the devices the
//! installer configured and nothing else.
//!
//! Read on a thread of its own every 30 s, never on a reactor: sysfs reads
//! and the helper's answer may block.

use hyperstor_proto::perf::{Drive, Smart};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where `hyperstor-smart` listens (its socket unit).
pub const SMART_SOCKET: &str = "/run/hyperstor-smart/smart.sock";

/// How often the drive is read.
pub const EVERY: Duration = Duration::from_secs(30);

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

/// "8.0 GT/s PCIe" → 80.
fn gts(s: &str) -> Option<u16> {
    let n: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some((n * 10.0).round() as u16)
}

/// What sysfs (under `sys`, `/sys` but in tests) says of the device at
/// `path`, served by a reactor on `cpu`.
pub fn from_sysfs(sys: &Path, path: &Path, cpu: Option<usize>) -> Drive {
    let mut d = Drive {
        numa: (-1, -1),
        ..Default::default()
    };
    if let Some(c) = cpu
        && let Ok(rd) = std::fs::read_dir(sys.join(format!("devices/system/cpu/cpu{c}")))
    {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(x) = n.strip_prefix("node").and_then(|x| x.parse().ok()) {
                d.numa.1 = x;
            }
        }
    }
    let Some(name) = std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    else {
        return d;
    };
    let block = sys.join("class/block").join(&name);
    if !block.exists() {
        return d;
    }
    d.write_cache = read(&block.join("queue/write_cache")).is_some_and(|w| w == "write back");
    // A namespace's `device` is its controller (NVMe) or its SCSI device.
    let dev = block.join("device");
    d.model = read(&dev.join("model")).unwrap_or_default();
    d.firmware = read(&dev.join("firmware_rev"))
        .or_else(|| read(&dev.join("rev")))
        .unwrap_or_default();
    if let Some(n) = read(&dev.join("numa_node")).and_then(|n| n.parse().ok()) {
        d.numa.0 = n;
    }
    if let Ok(rd) = std::fs::read_dir(&dev) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("hwmon")
                && let Some(t) = read(&e.path().join("temp1_input")).and_then(|t| t.parse().ok())
            {
                d.temperature_mc = Some(t);
            }
        }
    }
    // The controller's PCI function.
    let pci = dev.join("device");
    let link = (
        read(&pci.join("current_link_width")).and_then(|w| w.parse().ok()),
        read(&pci.join("max_link_width")).and_then(|w| w.parse().ok()),
        read(&pci.join("current_link_speed")).and_then(|s| gts(&s)),
        read(&pci.join("max_link_speed")).and_then(|s| gts(&s)),
    );
    if let (Some(w), Some(wm), Some(s), Some(sm)) = link {
        d.link = Some((w, wm, s, sm));
    }
    if d.numa.0 < 0
        && let Some(n) = read(&pci.join("numa_node")).and_then(|n| n.parse().ok())
    {
        d.numa.0 = n;
    }
    d
}

fn le(b: &[u8], at: usize, n: usize) -> u64 {
    // Counters are 128-bit; anything past 64 bits would take longer than
    // the drive's life to reach.
    let mut v = 0u64;
    for i in (0..n.min(8)).rev() {
        v = (v << 8) | u64::from(b[at + i]);
    }
    v
}

/// The SMART / health log page (NVMe base specification 2.0, Get Log Page
/// 02h, 512 bytes) and the Identify Controller warning and critical
/// temperatures (WCTEMP, CCTEMP), kelvin.
pub fn parse_health(log: &[u8], wctemp: u16, cctemp: u16) -> Option<Smart> {
    if log.len() < 232 {
        return None;
    }
    Some(Smart {
        critical_warning: log[0],
        temperature_k: le(log, 1, 2) as u16,
        available_spare: log[3],
        spare_threshold: log[4],
        percentage_used: log[5],
        data_units_read: le(log, 32, 16),
        data_units_written: le(log, 48, 16),
        controller_busy_minutes: le(log, 96, 16),
        power_on_hours: le(log, 128, 16),
        unsafe_shutdowns: le(log, 144, 16),
        media_errors: le(log, 160, 16),
        error_log_entries: le(log, 176, 16),
        warning_temp_minutes: le(log, 192, 4) as u32,
        critical_temp_minutes: le(log, 196, 4) as u32,
        throttle_transitions: [le(log, 216, 4) as u32, le(log, 220, 4) as u32],
        throttle_seconds: [le(log, 224, 4) as u32, le(log, 228, 4) as u32],
        warning_threshold_k: wctemp,
        critical_threshold_k: cctemp,
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Asks `hyperstor-smart` at `socket` for the health of the drive at
/// `path` (as configured): `ok HEX WCTEMP CCTEMP` or `error WHY`.
pub fn ask_helper(socket: &Path, path: &Path) -> Result<Smart, String> {
    let mut s = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("hyperstor-smart isn't answering ({e})"))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    writeln!(s, "health {}", path.display()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s)
        .read_line(&mut line)
        .map_err(|e| format!("hyperstor-smart: {e}"))?;
    let mut w = line.split_whitespace();
    match w.next() {
        Some("ok") => {
            let log = w
                .next()
                .and_then(unhex)
                .ok_or("hyperstor-smart: a bad log")?;
            let wc = w.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let cc = w.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            parse_health(&log, wc, cc).ok_or_else(|| "hyperstor-smart: a short log".into())
        }
        Some("error") => Err(line.trim_start_matches("error").trim().to_string()),
        _ => Err("hyperstor-smart: no answer".into()),
    }
}

/// Reads device `path`'s drive now and every [`EVERY`] on a thread of its
/// own, keeping the latest in the returned cell. Files and memory have
/// nothing to read: the cell stays empty.
pub fn watch(device: u32, path: Option<PathBuf>, cpu: Option<usize>) -> Arc<Mutex<Option<Drive>>> {
    let cell = Arc::new(Mutex::new(None));
    let Some(path) = path else {
        return cell;
    };
    let c = cell.clone();
    let _ = std::thread::Builder::new()
        .name(format!("hs-drive-{device}"))
        .spawn(move || {
            loop {
                let mut d = from_sysfs(Path::new("/sys"), &path, cpu);
                let nvme = std::fs::canonicalize(&path).is_ok_and(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("nvme"))
                });
                if nvme {
                    match ask_helper(Path::new(SMART_SOCKET), &path) {
                        Ok(s) => d.smart = Some(s),
                        Err(e) => d.smart_note = e,
                    }
                } else {
                    d.smart_note = "not an NVMe drive".into();
                }
                *c.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
                std::thread::sleep(EVERY);
            }
        });
    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_health_log_is_parsed() {
        let mut log = vec![0u8; 512];
        log[0] = 0x02; // temperature warning
        log[1..3].copy_from_slice(&318u16.to_le_bytes());
        log[3] = 100;
        log[4] = 10;
        log[5] = 7;
        log[48..56].copy_from_slice(&123_456u64.to_le_bytes());
        log[128..136].copy_from_slice(&9000u64.to_le_bytes());
        log[160] = 2;
        log[192..196].copy_from_slice(&15u32.to_le_bytes());
        log[216..220].copy_from_slice(&4u32.to_le_bytes());
        log[224..228].copy_from_slice(&600u32.to_le_bytes());
        let s = parse_health(&log, 343, 358).unwrap();
        assert_eq!(s.critical_warning, 2);
        assert_eq!(s.temperature_k, 318);
        assert_eq!(
            (s.available_spare, s.spare_threshold, s.percentage_used),
            (100, 10, 7)
        );
        assert_eq!(s.data_units_written, 123_456);
        assert_eq!(s.power_on_hours, 9000);
        assert_eq!(s.media_errors, 2);
        assert_eq!(s.warning_temp_minutes, 15);
        assert_eq!(s.throttle_transitions, [4, 0]);
        assert_eq!(s.throttle_seconds, [600, 0]);
        assert_eq!((s.warning_threshold_k, s.critical_threshold_k), (343, 358));
        assert!(parse_health(&log[..100], 0, 0).is_none());
    }

    /// A made-up sysfs tree for an NVMe namespace: what is read from where.
    #[test]
    fn sysfs_says_what_the_drive_is() {
        let root = std::env::temp_dir().join(format!("hs-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let w = |p: &str, v: &str| {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, format!("{v}\n")).unwrap();
        };
        w("class/block/nvme9n1/queue/write_cache", "write back");
        w("class/block/nvme9n1/device/model", "Fast Drive 3.84TB");
        w("class/block/nvme9n1/device/firmware_rev", "1.2.3");
        w("class/block/nvme9n1/device/numa_node", "1");
        w("class/block/nvme9n1/device/hwmon4/temp1_input", "44850");
        w("class/block/nvme9n1/device/device/current_link_width", "2");
        w("class/block/nvme9n1/device/device/max_link_width", "4");
        w(
            "class/block/nvme9n1/device/device/current_link_speed",
            "8.0 GT/s PCIe",
        );
        w(
            "class/block/nvme9n1/device/device/max_link_speed",
            "16.0 GT/s PCIe",
        );
        std::fs::create_dir_all(root.join("devices/system/cpu/cpu3/node0")).unwrap();
        // The device path names the namespace by its last component.
        let dev = root.join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("nvme9n1"), b"").unwrap();
        let d = from_sysfs(&root, &dev.join("nvme9n1"), Some(3));
        assert_eq!(d.model, "Fast Drive 3.84TB");
        assert_eq!(d.firmware, "1.2.3");
        assert_eq!(d.temperature_mc, Some(44850));
        assert_eq!(d.link, Some((2, 4, 80, 160)));
        assert_eq!(d.numa, (1, 0));
        assert!(d.write_cache);
        // A file has none of it.
        let f = from_sysfs(&root, &root.join("nothing.img"), None);
        assert_eq!((f.model.as_str(), f.link, f.numa), ("", None, (-1, -1)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hex_and_link_speeds_parse() {
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0"), None);
        assert_eq!(gts("2.5 GT/s PCIe"), Some(25));
    }
}
