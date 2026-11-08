//! Driving `hyperstor-vhost` (11-frontends.md §11.1) as a hypervisor would:
//! the service as a child process, its control socket, and [`Guest`], the
//! hypervisor's side of one disk: guest memory in a memfd, a split
//! virtqueue laid out in it by hand, requests as descriptor chains, kicks
//! and calls through eventfds.

use crate::{Cluster, bin_dir};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use vhost::vhost_user::message::{VhostUserConfigFlags, VhostUserProtocolFeatures};
use vhost::vhost_user::{Frontend, VhostUserFrontend};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use vm_memory::{Bytes, FileOffset, GuestAddress, GuestMemoryMmap, GuestRegionMmap, MmapRegion};
use vmm_sys_util::eventfd::EventFd;

pub const T_IN: u32 = 0;
pub const T_OUT: u32 = 1;
pub const T_FLUSH: u32 = 4;
pub const T_GET_ID: u32 = 8;
pub const T_WRITE_ZEROES: u32 = 13;
pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

/// A running `hyperstor-vhost serve`, stopped with SIGTERM when dropped.
pub struct Service {
    child: Child,
    pub dir: PathBuf,
    args: Vec<String>,
}

impl Service {
    /// Its sockets go in a short directory of their own under the system's
    /// temporary one (removed with it): a Unix socket's path has at most
    /// 107 bytes, which a scratch root's path can take up on its own (CI).
    pub fn start(c: &Cluster, name: &str, extra: &[&str]) -> Service {
        let dir = std::env::temp_dir().join(format!("hsv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut args: Vec<String> = ["serve", "--meta", &c.map, "--dir", dir.to_str().unwrap()]
            .iter()
            .map(|a| a.to_string())
            .collect();
        args.extend(extra.iter().map(|a| a.to_string()));
        Service {
            child: Self::spawn(&args),
            dir,
            args,
        }
    }

    fn spawn(args: &[String]) -> Child {
        let mut child = Command::new(bin_dir().join("hyperstor-vhost"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("hyperstor-vhost");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.contains("control socket"), "{line:?}");
        child
    }

    /// Stops it with SIGTERM and starts it again on the same directory,
    /// as systemd's restart does; returns once it answers again.
    pub fn restart(&mut self) {
        self.pause();
        self.resume();
    }

    /// Stops it with SIGTERM, keeping its directory (and state file).
    pub fn pause(&mut self) {
        // SAFETY: kill on our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let st = self.child.wait().unwrap();
        assert!(st.success(), "hyperstor-vhost exited with {st}");
    }

    /// Starts it again after [`Service::pause`].
    pub fn resume(&mut self) {
        self.child = Self::spawn(&self.args);
    }

    /// The volumes it serves, as `list` answers.
    pub fn listed(&self) -> Vec<String> {
        let r = self.ctl(serde_json::json!({"id": 1, "op": "list"}));
        r["ok"]["disks"]
            .as_array()
            .unwrap_or_else(|| panic!("{r}"))
            .iter()
            .map(|d| d["volume"].as_str().unwrap().to_string())
            .collect()
    }

    /// One control request; the reply.
    pub fn ctl(&self, req: Value) -> Value {
        let mut s = UnixStream::connect(self.dir.join("control.sock")).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    pub fn attach(&self, volume: &str, take: &str) -> Value {
        self.ctl(serde_json::json!({"id": 1, "op": "attach", "args": {
            "volume": volume, "serial": "disk-under-test", "take": take}}))
    }

    pub fn stop(mut self) {
        // SAFETY: kill on our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let st = self.child.wait().unwrap();
        assert!(st.success(), "hyperstor-vhost exited with {st}");
        let _ = std::fs::remove_dir_all(&self.dir);
        self.dir.clear();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        // SAFETY: kill on our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const QSIZE: u16 = 64;
const DESC: u64 = 0x0;
const AVAIL: u64 = 0x1000;
const USED: u64 = 0x2000;
const BUFS: u64 = 0x10000;
const MEM: usize = 16 << 20;

/// The hypervisor's side of one disk: guest memory, one queue, and the
/// vhost-user connection.
pub struct Guest {
    mem: GuestMemoryMmap,
    /// The connection: dropping it disconnects, as a hypervisor exiting.
    _front: Frontend,
    kick: EventFd,
    call: EventFd,
    avail: u16,
    used: u16,
    config: Vec<u8>,
    /// How long a request may take before the test fails (10 s).
    pub wait: Duration,
}

impl Guest {
    /// Connects and sets up queue 0, with or without `EVENT_IDX`.
    pub fn connect(socket: &Path, event_idx: bool) -> Guest {
        // SAFETY: memfd_create with a static name; the descriptor is owned
        // by the File below.
        let fd = unsafe { libc::memfd_create(c"guest".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: a fresh descriptor nothing else owns.
        let memfd = unsafe { File::from_raw_fd(fd) };
        memfd.set_len(MEM as u64).unwrap();
        let region =
            MmapRegion::from_file(FileOffset::new(memfd.try_clone().unwrap(), 0), MEM).unwrap();
        let host = region.as_ptr() as u64;
        let mem = GuestMemoryMmap::from_regions(vec![
            GuestRegionMmap::new(region, GuestAddress(0)).unwrap(),
        ])
        .unwrap();
        let mut front = Frontend::connect(socket, 1).unwrap();
        front.set_owner().unwrap();
        let offered = front.get_features().unwrap();
        // Without EVENT_IDX every completion calls; with it (as Linux
        // guests negotiate), the used ring's avail_event and the avail
        // ring's used_event, left at 0, still ask for a call per request.
        let want = if event_idx {
            offered
        } else {
            offered & !(1 << 29)
        };
        assert!(!event_idx || offered & (1 << 29) != 0);
        front.set_features(want).unwrap();
        let pf = front.get_protocol_features().unwrap();
        assert!(pf.contains(VhostUserProtocolFeatures::CONFIG));
        front
            .set_protocol_features(
                VhostUserProtocolFeatures::MQ
                    | VhostUserProtocolFeatures::CONFIG
                    | VhostUserProtocolFeatures::REPLY_ACK,
            )
            .unwrap();
        assert!(front.get_queue_num().unwrap() >= 1);
        let (_, config) = front
            .get_config(0, 60, VhostUserConfigFlags::WRITABLE, &[0; 60])
            .unwrap();
        front
            .set_mem_table(&[VhostUserMemoryRegionInfo {
                guest_phys_addr: 0,
                memory_size: MEM as u64,
                userspace_addr: host,
                mmap_offset: 0,
                mmap_handle: memfd.as_raw_fd(),
            }])
            .unwrap();
        front.set_vring_num(0, QSIZE).unwrap();
        front
            .set_vring_addr(
                0,
                &VringConfigData {
                    queue_max_size: QSIZE,
                    queue_size: QSIZE,
                    flags: 0,
                    desc_table_addr: host + DESC,
                    used_ring_addr: host + USED,
                    avail_ring_addr: host + AVAIL,
                    log_addr: None,
                },
            )
            .unwrap();
        front.set_vring_base(0, 0).unwrap();
        let kick = EventFd::new(0).unwrap();
        let call = EventFd::new(0).unwrap();
        front.set_vring_call(0, &call).unwrap();
        front.set_vring_kick(0, &kick).unwrap();
        front.set_vring_enable(0, true).unwrap();
        Guest {
            mem,
            _front: front,
            kick,
            call,
            avail: 0,
            used: 0,
            wait: Duration::from_secs(10),
            config: config.to_vec(),
        }
    }

    /// A little-endian field of the device's configuration space.
    pub fn config_u32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.config[at..at + 4].try_into().unwrap())
    }

    pub fn config_u8(&self, at: usize) -> u8 {
        self.config[at]
    }

    pub fn capacity(&self) -> u64 {
        u64::from_le_bytes(self.config[0..8].try_into().unwrap()) * 512
    }

    /// Puts descriptor `i`: (address, length, write-only), chained to i+1.
    fn desc(&self, i: u16, addr: u64, len: u32, write: bool, last: bool) {
        let at = GuestAddress(DESC + 16 * i as u64);
        let mut d = addr.to_le_bytes().to_vec();
        d.extend(len.to_le_bytes());
        let flags: u16 = (!last as u16) | ((write as u16) << 1);
        d.extend(flags.to_le_bytes());
        d.extend((i + 1).to_le_bytes());
        self.mem.write_slice(&d, at).unwrap();
    }

    /// One request: a header, then `out` (data the device reads) or `in_len`
    /// bytes the device writes, then the status. Returns (status, data in,
    /// bytes the device said it wrote).
    pub fn request(
        &mut self,
        kind: u32,
        sector: u64,
        out: &[u8],
        in_len: u32,
    ) -> (u8, Vec<u8>, u32) {
        let hdr = GuestAddress(BUFS);
        let data = BUFS + 0x1000;
        let mut h = kind.to_le_bytes().to_vec();
        h.extend(0u32.to_le_bytes());
        h.extend(sector.to_le_bytes());
        self.mem.write_slice(&h, hdr).unwrap();
        let status = data + (8 << 20);
        self.mem.write_slice(&[0xff], GuestAddress(status)).unwrap();
        // Data in two descriptors, to cross a segment boundary.
        let mut n = 0u16;
        self.desc(n, BUFS, 16, false, false);
        n += 1;
        let (len, write) = if out.is_empty() {
            (in_len, true)
        } else {
            self.mem.write_slice(out, GuestAddress(data)).unwrap();
            (out.len() as u32, false)
        };
        if len > 0 {
            let first = (len / 2).next_multiple_of(512).min(len);
            self.desc(n, data, first, write, false);
            n += 1;
            if len > first {
                self.desc(n, data + first as u64, len - first, write, false);
                n += 1;
            }
        }
        self.desc(n, status, 1, true, true);
        // The avail ring: this chain's head (0), then the index.
        let slot = AVAIL + 4 + 2 * (self.avail % QSIZE) as u64;
        self.mem
            .write_slice(&0u16.to_le_bytes(), GuestAddress(slot))
            .unwrap();
        self.avail = self.avail.wrapping_add(1);
        self.mem
            .write_slice(&self.avail.to_le_bytes(), GuestAddress(AVAIL + 2))
            .unwrap();
        self.kick.write(1).unwrap();
        let t = Instant::now();
        loop {
            let mut idx = [0u8; 2];
            self.mem
                .read_slice(&mut idx, GuestAddress(USED + 2))
                .unwrap();
            if u16::from_le_bytes(idx) != self.used {
                break;
            }
            assert!(t.elapsed() < self.wait, "no completion");
            std::thread::sleep(Duration::from_millis(1));
        }
        let elem = USED + 4 + 8 * (self.used % QSIZE) as u64;
        self.used = self.used.wrapping_add(1);
        let mut e = [0u8; 8];
        self.mem.read_slice(&mut e, GuestAddress(elem)).unwrap();
        assert_eq!(u32::from_le_bytes(e[0..4].try_into().unwrap()), 0, "head");
        let written = u32::from_le_bytes(e[4..8].try_into().unwrap());
        let mut st = [0u8];
        self.mem.read_slice(&mut st, GuestAddress(status)).unwrap();
        let mut back = vec![0; if out.is_empty() { in_len as usize } else { 0 }];
        self.mem.read_slice(&mut back, GuestAddress(data)).unwrap();
        (st[0], back, written)
    }

    /// Writes made available all at once, one chain each (up to 16 of
    /// up to 64 KiB), so the device has them in flight together; waits
    /// for every one and returns their statuses, in completion order.
    pub fn write_together(&mut self, writes: &[(u64, &[u8])]) -> Vec<u8> {
        assert!(writes.len() <= 16);
        let data = BUFS + 0x1000;
        let status = data + (8 << 20);
        for (k, (off, bytes)) in writes.iter().enumerate() {
            assert!(bytes.len() <= 64 << 10);
            let k16 = k as u16;
            let (h, d, st) = (
                BUFS + 16 * k as u64,
                data + (k as u64) * 0x10000,
                status + k as u64,
            );
            let mut hdr = T_OUT.to_le_bytes().to_vec();
            hdr.extend(0u32.to_le_bytes());
            hdr.extend((off / 512).to_le_bytes());
            self.mem.write_slice(&hdr, GuestAddress(h)).unwrap();
            self.mem.write_slice(bytes, GuestAddress(d)).unwrap();
            self.mem.write_slice(&[0xff], GuestAddress(st)).unwrap();
            self.desc(3 * k16, h, 16, false, false);
            self.desc(3 * k16 + 1, d, bytes.len() as u32, false, false);
            self.desc(3 * k16 + 2, st, 1, true, true);
            let slot = AVAIL + 4 + 2 * (self.avail % QSIZE) as u64;
            self.mem
                .write_slice(&(3 * k16).to_le_bytes(), GuestAddress(slot))
                .unwrap();
            self.avail = self.avail.wrapping_add(1);
        }
        self.mem
            .write_slice(&self.avail.to_le_bytes(), GuestAddress(AVAIL + 2))
            .unwrap();
        self.kick.write(1).unwrap();
        let want = self.used.wrapping_add(writes.len() as u16);
        let t = Instant::now();
        loop {
            let mut idx = [0u8; 2];
            self.mem
                .read_slice(&mut idx, GuestAddress(USED + 2))
                .unwrap();
            if u16::from_le_bytes(idx) == want {
                break;
            }
            assert!(t.elapsed() < self.wait, "no completion");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut out = Vec::new();
        while self.used != want {
            let elem = USED + 4 + 8 * (self.used % QSIZE) as u64;
            self.used = self.used.wrapping_add(1);
            let mut e = [0u8; 8];
            self.mem.read_slice(&mut e, GuestAddress(elem)).unwrap();
            let k = u32::from_le_bytes(e[0..4].try_into().unwrap()) as u64 / 3;
            let mut st = [0u8];
            self.mem
                .read_slice(&mut st, GuestAddress(status + k))
                .unwrap();
            out.push(st[0]);
        }
        out
    }

    pub fn read(&mut self, off: u64, len: u32) -> Vec<u8> {
        let (s, d, w) = self.request(T_IN, off / 512, &[], len);
        assert_eq!((s, w), (S_OK, len + 1), "read {len} at {off}");
        d
    }

    pub fn write(&mut self, off: u64, data: &[u8]) {
        let (s, _, _) = self.request(T_OUT, off / 512, data, 0);
        assert_eq!(s, S_OK, "write {} at {off}", data.len());
    }

    /// Whether the device called since last asked.
    pub fn called(&self) -> bool {
        // A non-blocking read of the call eventfd.
        let mut v = 0u64;
        // SAFETY: reads 8 bytes into a local u64 from our eventfd.
        let n = unsafe {
            let fl = libc::fcntl(self.call.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(self.call.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
            libc::read(self.call.as_raw_fd(), (&mut v as *mut u64).cast(), 8)
        };
        n == 8 && v > 0
    }
}

/// Bytes that differ from block to block and seed to seed.
pub fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}
