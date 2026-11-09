//! Fault injection for tests (04-decisions.md D92): partitions between nodes
//! and devices that are slow or fail. The cluster tests drive them; nothing
//! here is used unless a test configures it.

use hyperstor_engine::BlockDev;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Linux's `EIO`.
const EIO: i32 = 5;

/// Pairs of nodes that can't reach each other, both ways. A node's reactors
/// consult it before every request to a peer (`NodeConfig::partitions`).
#[derive(Default)]
pub struct Partitions {
    cut: Mutex<HashSet<(u32, u32)>>,
    /// Endpoints nobody reaches, as a failed port (D210).
    addrs: Mutex<HashSet<std::net::SocketAddr>>,
}

impl Partitions {
    fn pair(a: u32, b: u32) -> (u32, u32) {
        (a.min(b), a.max(b))
    }

    pub fn cut(&self, a: u32, b: u32) {
        self.cut.lock().unwrap().insert(Self::pair(a, b));
    }

    pub fn heal(&self, a: u32, b: u32) {
        self.cut.lock().unwrap().remove(&Self::pair(a, b));
    }

    pub fn heal_all(&self) {
        self.cut.lock().unwrap().clear();
        self.addrs.lock().unwrap().clear();
    }

    /// No node reaches `addr` while `cut`, over TCP or UDP.
    pub fn cut_addr(&self, addr: std::net::SocketAddr, cut: bool) {
        let mut s = self.addrs.lock().unwrap();
        if cut {
            s.insert(addr);
        } else {
            s.remove(&addr);
        }
    }

    pub fn addr_blocked(&self, addr: std::net::SocketAddr) -> bool {
        self.addrs.lock().unwrap().contains(&addr)
    }

    pub fn blocked(&self, a: u32, b: u32) -> bool {
        a != b && self.cut.lock().unwrap().contains(&Self::pair(a, b))
    }
}

/// Pings that fail (D185): TCP pings to some nodes lost, as on a connection
/// stalled behind a lost segment, and nodes started without UDP, as an
/// older build or behind a firewall that drops it. A node's reactors
/// consult it when they bind and when they ping (`NodeConfig::pings`).
#[derive(Default)]
pub struct Pings {
    tcp_lost: Mutex<HashSet<u32>>,
    no_udp: Mutex<HashSet<u32>>,
}

impl Pings {
    /// TCP pings to `node` get no answer while `lost`.
    pub fn lose_tcp(&self, node: u32, lost: bool) {
        let mut s = self.tcp_lost.lock().unwrap();
        if lost {
            s.insert(node);
        } else {
            s.remove(&node);
        }
    }

    pub fn tcp_lost(&self, node: u32) -> bool {
        self.tcp_lost.lock().unwrap().contains(&node)
    }

    /// `node`, when it next starts, binds no UDP socket.
    pub fn start_without_udp(&self, node: u32) {
        self.no_udp.lock().unwrap().insert(node);
    }

    pub fn no_udp(&self, node: u32) -> bool {
        self.no_udp.lock().unwrap().contains(&node)
    }
}

/// Message kinds some nodes don't know, as a build from before them: such
/// a node drops the connection a request of that kind comes on, as it
/// does any frame it can't decode. A node's reactors consult it for every
/// request (`NodeConfig::older`).
#[derive(Default)]
pub struct Older {
    unknown: Mutex<HashSet<(u32, u16)>>,
    /// Nodes that answer `HelloOk` with this protocol version and read
    /// operation lists as that version's builds do.
    versions: Mutex<std::collections::HashMap<u32, u16>>,
}

impl Older {
    /// `node` speaks protocol `version` (`None`: this build's).
    pub fn set_version(&self, node: u32, version: Option<u16>) {
        let mut v = self.versions.lock().unwrap();
        match version {
            Some(x) => v.insert(node, x),
            None => v.remove(&node),
        };
    }

    pub fn version(&self, node: u32) -> Option<u16> {
        self.versions.lock().unwrap().get(&node).copied()
    }

    /// `node` doesn't know message `kind` while `unknown`.
    pub fn set(&self, node: u32, kind: u16, unknown: bool) {
        let mut s = self.unknown.lock().unwrap();
        if unknown {
            s.insert((node, kind));
        } else {
            s.remove(&(node, kind));
        }
    }

    pub fn unknown(&self, node: u32, kind: u16) -> bool {
        self.unknown.lock().unwrap().contains(&(node, kind))
    }
}

/// Devices whose commits take longer without holding their reactor
/// (I199): a slow device written through the reactor's ring, as opposed to
/// [`FaultyDev`]'s delay, which holds it as the device's own calls do. A
/// node's reactors consult it before each commit round's writes
/// (`NodeConfig::commit_delays`); the wait counts toward the I/O timeout.
/// Reads can be made slow the same way ([`CommitDelays::set_reads`], D190),
/// and their wait counts toward the read timeout.
#[derive(Default)]
pub struct CommitDelays {
    by_device: Mutex<std::collections::HashMap<u32, Duration>>,
    reads: Mutex<std::collections::HashMap<u32, Duration>>,
}

impl CommitDelays {
    pub fn set(&self, device: u32, d: Duration) {
        let mut m = self.by_device.lock().unwrap();
        if d.is_zero() {
            m.remove(&device);
        } else {
            m.insert(device, d);
        }
    }

    pub fn get(&self, device: u32) -> Duration {
        self.by_device
            .lock()
            .unwrap()
            .get(&device)
            .copied()
            .unwrap_or_default()
    }

    /// Makes each read on `device` wait `d` more; zero: none.
    pub fn set_reads(&self, device: u32, d: Duration) {
        let mut m = self.reads.lock().unwrap();
        if d.is_zero() {
            m.remove(&device);
        } else {
            m.insert(device, d);
        }
    }

    pub fn reads(&self, device: u32) -> Duration {
        self.reads
            .lock()
            .unwrap()
            .get(&device)
            .copied()
            .unwrap_or_default()
    }
}

/// A device that can be made slow (every read, write and flush takes
/// `delay` more, blocking its reactor as a hung disk does) or fail (every
/// call returns an I/O error, as a dead disk does).
pub struct FaultyDev {
    inner: Arc<dyn BlockDev>,
    delay_ms: AtomicU64,
    fail: AtomicBool,
}

impl FaultyDev {
    pub fn new(inner: Arc<dyn BlockDev>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            delay_ms: AtomicU64::new(0),
            fail: AtomicBool::new(false),
        })
    }

    pub fn set_delay(&self, d: Duration) {
        self.delay_ms.store(d.as_millis() as u64, Relaxed);
    }

    pub fn set_failed(&self, failed: bool) {
        self.fail.store(failed, Relaxed);
    }

    fn fault(&self) -> io::Result<()> {
        if self.fail.load(Relaxed) {
            return Err(io::Error::from_raw_os_error(EIO));
        }
        let ms = self.delay_ms.load(Relaxed);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        Ok(())
    }
}

impl BlockDev for FaultyDev {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.fault()?;
        self.inner.read(off, buf)
    }

    fn write(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.fault()?;
        self.inner.write(off, buf)
    }

    fn flush(&self) -> io::Result<()> {
        self.fault()?;
        self.inner.flush()
    }

    fn write_batch(&self, writes: &[(u64, &[u8])]) -> io::Result<()> {
        self.fault()?;
        self.inner.write_batch(writes)
    }

    fn read_batch(&self, reads: &mut [(u64, &mut [u8])]) -> io::Result<()> {
        self.fault()?;
        self.inner.read_batch(reads)
    }

    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        self.fault()?;
        self.inner.discard(off, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_are_symmetric_and_heal() {
        let p = Partitions::default();
        p.cut(3, 1);
        assert!(p.blocked(1, 3) && p.blocked(3, 1));
        assert!(!p.blocked(1, 2) && !p.blocked(1, 1));
        p.heal(1, 3);
        assert!(!p.blocked(3, 1));
    }

    #[test]
    fn a_faulty_device_fails_and_recovers() {
        let d = FaultyDev::new(Arc::new(hyperstor_engine::MemDev::new(1 << 20)));
        let mut b = vec![0u8; 4096];
        d.write(0, &[7; 4096]).unwrap();
        d.set_failed(true);
        assert_eq!(d.read(0, &mut b).unwrap_err().raw_os_error(), Some(EIO));
        assert!(d.flush().is_err());
        d.set_failed(false);
        d.read(0, &mut b).unwrap();
        assert_eq!(b, [7; 4096]);
    }
}
