//! One vhost-user-blk connection: the frontend (Cloud Hypervisor or QEMU)
//! sets up guest memory and the virtqueues through `vhost`'s request
//! handler, and each enabled queue is served by a task on this reactor
//! (04-decisions.md D116): it waits on the queue's kick eventfd, takes the
//! chains the guest made available, does each request against the
//! attached volume, and signals the call eventfd when the guest wants it
//! (`EVENT_IDX`). Requests are tracked in the frontend's in-flight buffer
//! (D145, [`crate::inflight`]), so a restarted service runs again the
//! ones its last process took and never answered.
//!
//! A connection starts on the service's first reactor and moves, with
//! `vhost`'s handler, to its disk's reactor once the frontend's memory
//! table says where the guest's memory is (D292): [`serve_connection`]
//! answers [`Ended::Place`] and [`resume`] carries on there. Its rings
//! start only there, on the [`Disk`] that reactor holds. A guest whose
//! memory isn't bound to one node is placed again once the pages its
//! requests use show where its memory is ([`Watch`]): its rings stop
//! taking requests, those in flight finish, and it moves.

use crate::blk::{self, Bad, Request};
use crate::budget::Budget;
use crate::inflight::Inflight;
use hyperstor_client::ClientError;
use hyperstor_client::volume::Attached;
use hyperstor_reactor::chan::{OneSender, oneshot};
use hyperstor_reactor::fd;
use hyperstor_reactor::time::sleep;
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use vhost::VhostUserMemoryRegionBase;
use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostTransferStatePhase, VhostUserConfigFlags, VhostUserInflight,
    VhostUserLog, VhostUserMemoryRegion, VhostUserProtocolFeatures, VhostUserShMemConfig,
    VhostUserSharedMsg, VhostUserSingleMemoryRegion, VhostUserVirtioFeatures,
    VhostUserVringAddrFlags, VhostUserVringState,
};
use vhost::vhost_user::{
    BackendReqHandler, Error as VError, GpuBackend, Result as VResult,
    VhostUserBackendReqHandlerMut,
};
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{
    Address, Bytes, FileOffset, GuestAddress, GuestMemoryBackend, GuestMemoryMmap,
    GuestMemoryRegion, GuestRegionMmap, MmapRegion,
};

const WRITE_ZEROES: u32 = virtio_bindings::virtio_blk::VIRTIO_BLK_T_WRITE_ZEROES;

/// Descriptors per queue at most (the frontend picks the size, up to this).
pub const QUEUE_MAX: u16 = 1024;

/// A volume as a disk: what every connection to its socket serves.
pub struct Disk {
    pub volume: Rc<Attached>,
    /// What `GET_ID` returns (11-frontends.md §11.1: the glidex disk id).
    pub serial: String,
    /// Queues offered.
    pub queues: u16,
    /// Its rings are polled while busy, not woken by the guest (D293).
    pub poll: bool,
    pub locks: Locks,
    /// How long a write refused for space is held (04-decisions.md D183).
    pub hold: Duration,
    /// Writes held for space now.
    pub held: Cell<u32>,
}

/// One write held for space: the first held, and the last let go, are
/// logged, not each one.
struct Holding<'a>(&'a Disk);

impl<'a> Holding<'a> {
    fn new(disk: &'a Disk) -> Holding<'a> {
        if disk.held.get() == 0 {
            eprintln!(
                "hyperstor-vhost: volume {:032x}: a write refused for space; holding writes up to {} s",
                disk.volume.volume().id,
                disk.hold.as_secs()
            );
        }
        disk.held.set(disk.held.get() + 1);
        Holding(disk)
    }
}

impl Drop for Holding<'_> {
    fn drop(&mut self) {
        let d = self.0;
        d.held.set(d.held.get() - 1);
        if d.held.get() == 0 {
            eprintln!(
                "hyperstor-vhost: volume {:032x}: no write held for space now",
                d.volume.volume().id
            );
        }
    }
}

/// Whether a write failed for want of space (D27): a device past full.
fn no_space(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Server {
            code: hyperstor_proto::Code::NoSpace,
            ..
        }
    )
}

/// Does a write, holding it while it is refused for space (D183):
/// `attempt` is tried again every second for up to the disk's `hold`, so
/// space freed meanwhile (trash purged, a device added) lets it finish.
/// virtio-blk has no status for "no space": past the hold it fails, and
/// the guest sees `IOERR`. `attempt` answers `Err(None)` when the guest's
/// buffers can't be read; it reads the data from guest memory each time,
/// which stays the guest's until the request completes.
async fn held<F, Fut>(disk: &Disk, mut attempt: F) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), Option<ClientError>>>,
{
    match attempt().await {
        Ok(()) => return Ok(()),
        Err(Some(e)) if no_space(&e) => {}
        Err(_) => return Err(()),
    }
    let until = Instant::now() + disk.hold;
    let _holding = Holding::new(disk);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            eprintln!(
                "hyperstor-vhost: volume {:032x}: a write held {} s for space failed: an I/O error to the guest",
                disk.volume.volume().id,
                disk.hold.as_secs()
            );
            return Err(());
        }
        sleep(left.min(Duration::from_secs(1))).await;
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(Some(e)) if no_space(&e) => {}
            Err(e) => {
                eprintln!(
                    "hyperstor-vhost: volume {:032x}: a write held for space failed otherwise: {}",
                    disk.volume.volume().id,
                    e.map_or("guest memory unreadable".into(), |e| e.to_string())
                );
                return Err(());
            }
        }
    }
}

/// The volume blocks writes in flight cover. A write waits for those
/// overlapping its own, so the read-modify-write of a partial block
/// (512e, 04-decisions.md D126) never puts back data another write replaced
/// in between. Guests may reorder overlapping writes anyway; this only
/// keeps each one whole.
#[derive(Default)]
pub struct Locks {
    held: RefCell<Vec<(u64, u64)>>,
    waiting: RefCell<Vec<OneSender<()>>>,
}

impl Locks {
    async fn lock(&self, start: u64, end: u64) -> Held<'_> {
        loop {
            let busy = self
                .held
                .borrow()
                .iter()
                .any(|&(s, e)| s < end && start < e);
            if !busy {
                self.held.borrow_mut().push((start, end));
                return Held {
                    locks: self,
                    range: (start, end),
                };
            }
            let (tx, rx) = oneshot();
            self.waiting.borrow_mut().push(tx);
            rx.recv().await;
        }
    }
}

struct Held<'a> {
    locks: &'a Locks,
    range: (u64, u64),
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut held = self.locks.held.borrow_mut();
        if let Some(i) = held.iter().position(|r| *r == self.range) {
            held.swap_remove(i);
        }
        drop(held);
        // Every waiter looks again; few writes overlap.
        for w in self.locks.waiting.take() {
            let _ = w.send(());
        }
    }
}

/// Reads bytes `off..off + len` (whole sectors) through the volume blocks
/// they are in.
async fn read_at(v: &Attached, off: u64, len: u64) -> Result<Vec<u8>, ()> {
    let (start, end) = blk::blocks(off, len);
    let mut data = v.read(start, end - start).await.map_err(drop)?;
    if start != off || end != off + len {
        let at = (off - start) as usize;
        data.drain(..at);
        data.truncate(len as usize);
    }
    Ok(data)
}

/// Writes `data` (whole sectors) at `off`: whole volume blocks as they
/// are; a partial block at either end read, merged and written with them.
async fn write_at(disk: &Disk, off: u64, data: Vec<u8>) -> Result<(), ClientError> {
    let v = &disk.volume;
    let len = data.len() as u64;
    let (start, end) = blk::blocks(off, len);
    let _held = disk.locks.lock(start, end).await;
    if start == off && end == off + len {
        return v.write(off, data).await;
    }
    let block = blk::BLOCK as usize;
    let mut buf = vec![0u8; (end - start) as usize];
    if off != start {
        let b = v.read(start, blk::BLOCK).await?;
        buf[..block].copy_from_slice(&b);
    }
    if off + len != end && (off == start || end - start > blk::BLOCK) {
        let b = v.read(end - blk::BLOCK, blk::BLOCK).await?;
        let n = buf.len();
        buf[n - block..].copy_from_slice(&b);
    }
    let at = (off - start) as usize;
    buf[at..at + data.len()].copy_from_slice(&data);
    v.write(start, buf).await
}

type Mem = Arc<GuestMemoryMmap>;

struct Vring {
    queue: Queue,
    kick: Option<File>,
    call: Option<File>,
    enabled: bool,
    /// Bumped whenever the ring stops; a task serving an older generation
    /// ends, and its requests' completions are dropped.
    generation: u64,
    running: bool,
    /// Requests taken from this ring so far, for the in-flight buffer's
    /// order.
    counter: u64,
}

struct State {
    /// The disk's volume, which each reactor's [`Disk`] for it is found
    /// by ([`local_disk`]).
    volume: u128,
    /// The volume's size as the connection began, for the guest's
    /// configuration.
    size: u64,
    acked: u64,
    protocol: VhostUserProtocolFeatures,
    mem: Option<Mem>,
    regions: Vec<VhostUserMemoryRegion>,
    vrings: Vec<Vring>,
    /// The frontend's in-flight buffer, once it has given one.
    inflight: Option<Arc<Inflight>>,
    /// Whether the connection is on its disk's reactor (D292); until then
    /// no ring starts.
    placed: bool,
    /// The guest memory's policy, once the first memory table came and
    /// the connection is to be placed.
    place: Option<crate::numa::Policy>,
    /// Requests taken and not yet completed, which a move waits for.
    running: usize,
    /// Rings stopped taking requests for a move: their tasks end, and
    /// [`resume`] starts them again.
    parked: bool,
    /// For a guest whose memory isn't bound: where its pages are, until
    /// it is placed by them.
    watch: Option<Watch>,
    /// Bytes its requests may hold in buffers at once (I375).
    budget: Budget,
}

/// The guest pages a connection's requests used lately, to find the node
/// an unbound guest's memory is on (D292).
pub struct Watch {
    /// The node of the reactor serving the connection.
    node: Option<u32>,
    /// Host addresses of pages requests used, newest last.
    pages: std::collections::VecDeque<usize>,
    last: Instant,
}

/// Pages remembered for [`Watch`].
const WATCH_PAGES: usize = 256;
/// How often an unbound guest's pages are looked at.
pub const WATCH_EVERY: Duration = Duration::from_secs(1);
/// Pages that must be in memory before a node is chosen, and the share
/// of them it must hold.
const WATCH_MIN: usize = 32;

impl Watch {
    fn note(&mut self, mem: &GuestMemoryMmap, segs: &Segments) {
        for &(addr, len) in segs.readable.iter().chain(&segs.writable) {
            // Each page of the buffer, up to 64 of a large one.
            let Ok(p) = mem.get_slice(addr, len as usize) else {
                continue;
            };
            let start = p.ptr_guard().as_ptr() as usize;
            let end = start + len as usize;
            let mut page = start & !4095;
            for _ in 0..64 {
                if page >= end {
                    break;
                }
                if !self.pages.contains(&page) {
                    if self.pages.len() == WATCH_PAGES {
                        self.pages.pop_front();
                    }
                    self.pages.push_back(page);
                }
                page += 4096;
            }
        }
    }

    /// The node holding two thirds or more of the pages looked at, if it
    /// isn't this reactor's.
    fn elsewhere(&self) -> Option<u32> {
        let mut pages: Vec<usize> = self.pages.iter().copied().collect();
        pages.sort_unstable();
        pages.dedup();
        let (node, n, of) = crate::numa::majority(&crate::numa::nodes_of(&pages))?;
        (of >= WATCH_MIN && 3 * n >= 2 * of && Some(node) != self.node).then_some(node)
    }
}

/// A connection's state, which moves with it to its disk's reactor; only
/// one reactor uses it at a time.
type Shared = Arc<Mutex<State>>;

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

thread_local! {
    /// The disks this reactor serves, by volume: each holds its lease
    /// through this reactor's client (D291).
    static DISKS: RefCell<std::collections::HashMap<u128, Rc<Disk>>> = Default::default();
}

/// This reactor's disk for `volume`, if it holds it.
pub fn local_disk(volume: u128) -> Option<Rc<Disk>> {
    DISKS.with(|d| d.borrow().get(&volume).cloned())
}

/// Makes `disk` this reactor's for its volume.
pub fn hold_disk(disk: Rc<Disk>) {
    let v = disk.volume.volume().id;
    DISKS.with(|d| d.borrow_mut().insert(v, disk));
}

/// Lets go of this reactor's disk for `volume`, returning it.
pub fn drop_disk(volume: u128) -> Option<Rc<Disk>> {
    DISKS.with(|d| d.borrow_mut().remove(&volume))
}

impl State {
    fn new(volume: u128, size: u64, queues: u16) -> Self {
        let vrings = (0..queues)
            .map(|_| Vring {
                queue: Queue::new(QUEUE_MAX).expect("a valid queue size"),
                kick: None,
                call: None,
                enabled: false,
                generation: 0,
                running: false,
                counter: 0,
            })
            .collect();
        Self {
            volume,
            size,
            acked: 0,
            protocol: VhostUserProtocolFeatures::empty(),
            mem: None,
            regions: Vec::new(),
            vrings,
            inflight: None,
            placed: false,
            place: None,
            running: 0,
            parked: false,
            watch: None,
            budget: Budget::new(crate::budget::CONNECTION_BYTES),
        }
    }

    fn protocol_acked(&self) -> bool {
        self.acked & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0
    }

    fn event_idx(&self) -> bool {
        self.acked & (1 << virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX) != 0
    }

    /// The frontend's addresses (its own virtual addresses) as guest
    /// physical ones.
    fn gpa(&self, va: u64) -> VResult<u64> {
        self.regions
            .iter()
            .find(|r| va >= r.user_addr && va - r.user_addr < r.memory_size)
            .map(|r| r.guest_phys_addr + (va - r.user_addr))
            .ok_or(VError::InvalidParam)
    }

    fn vring(&mut self, index: u32) -> VResult<&mut Vring> {
        self.vrings
            .get_mut(index as usize)
            .ok_or(VError::InvalidParam)
    }

    /// Stops ring `index`: its task ends at its next wake-up, which this
    /// causes by kicking it.
    fn stop(&mut self, index: usize) {
        let v = &mut self.vrings[index];
        v.generation += 1;
        if v.running {
            v.running = false;
            if let Some(k) = &v.kick {
                kick(k);
            }
        }
    }

    fn stop_all(&mut self) {
        for i in 0..self.vrings.len() {
            self.stop(i);
        }
    }
}

/// Writes 1 to an eventfd (a kick to wake our own task, or a call to the
/// guest). An eventfd write doesn't block unless the counter is about to
/// overflow, which a ring's notifications never reach.
fn kick(f: &File) {
    let one = 1u64.to_ne_bytes();
    // SAFETY: writes 8 bytes from a local array to an eventfd we hold open.
    unsafe { libc::write(f.as_raw_fd(), one.as_ptr().cast(), 8) };
}

/// Starts ring `index` if it can run: memory is set, it has a kick, it is
/// enabled (or the protocol features, which start rings disabled, weren't
/// negotiated), and it isn't running already.
fn start(shared: &Shared, index: usize) {
    let mut s = lock(shared);
    let can = s.placed
        && !s.parked
        && s.mem.is_some()
        && (s.vrings[index].enabled || !s.protocol_acked());
    let event_idx = s.event_idx();
    let mem = s.mem.clone();
    let v = &mut s.vrings[index];
    if !can || v.running || v.kick.is_none() {
        return;
    }
    let Some(mem) = mem else { return };
    v.queue.set_event_idx(event_idx);
    v.queue.set_ready(true);
    if !v.queue.is_valid(&*mem) {
        return;
    }
    // Requests a previous process took and never answered (D145): the
    // ring goes on after them, and they run again first.
    let mut again = Vec::new();
    if let Some(inf) = s.inflight.clone() {
        let v = &mut s.vrings[index];
        let used = v
            .queue
            .used_idx(&*mem, std::sync::atomic::Ordering::Acquire)
            .map(|w| w.0)
            .unwrap_or(0);
        if let Some(r) = inf.recover(index as u16, used) {
            v.queue.set_next_avail(r.next_avail);
            v.queue.set_next_used(used);
            v.counter = r.next_counter;
            again = r.heads;
        }
    }
    let Some(disk) = local_disk(s.volume) else {
        return;
    };
    // Each completes, answered or given back empty.
    s.running += again.len();
    let v = &mut s.vrings[index];
    v.running = true;
    let generation = v.generation;
    let (table, size) = (GuestAddress(v.queue.desc_table()), v.queue.size());
    drop(s);
    for head in again {
        match chain_segments(&mem, table, size, head) {
            Some(segs) => {
                hyperstor_reactor::spawn(serve(
                    shared.clone(),
                    disk.clone(),
                    index,
                    generation,
                    mem.clone(),
                    head,
                    segs,
                ));
            }
            // Not a chain we can read: given back empty, as one can't be
            // answered.
            None => complete(shared, index, generation, &mem, head, 0),
        }
    }
    hyperstor_reactor::spawn(run(shared.clone(), disk, index, generation));
}

/// The readable and writable parts of the chain at `head` of the
/// descriptor table at `table` (`size` entries), followed through an
/// indirect table: for requests run again after a restart, which the ring
/// no longer hands out.
fn chain_segments(
    mem: &GuestMemoryMmap,
    table: GuestAddress,
    size: u16,
    head: u16,
) -> Option<Segments> {
    use virtio_bindings::virtio_ring::{
        VRING_DESC_F_INDIRECT, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
    };
    let mut segs = Segments {
        readable: Vec::new(),
        writable: Vec::new(),
    };
    let (mut table, mut size, mut i) = (table, size as u32, head as u32);
    let mut indirect = false;
    for _ in 0..1024 {
        if i >= size {
            return None;
        }
        let mut d = [0u8; 16];
        mem.read_slice(&mut d, table.checked_add(16 * i as u64)?)
            .ok()?;
        let addr = u64::from_le_bytes(d[0..8].try_into().ok()?);
        let len = u32::from_le_bytes(d[8..12].try_into().ok()?);
        let flags = u16::from_le_bytes(d[12..14].try_into().ok()?) as u32;
        let next = u16::from_le_bytes(d[14..16].try_into().ok()?) as u32;
        if flags & VRING_DESC_F_INDIRECT != 0 {
            if indirect || len % 16 != 0 || len == 0 {
                return None;
            }
            (table, size, i, indirect) = (GuestAddress(addr), len / 16, 0, true);
            continue;
        }
        if flags & VRING_DESC_F_WRITE != 0 {
            segs.writable.push((GuestAddress(addr), len));
        } else {
            segs.readable.push((GuestAddress(addr), len));
        }
        if flags & VRING_DESC_F_NEXT == 0 {
            return Some(segs);
        }
        i = next;
    }
    None
}

/// Serves ring `index` until it stops.
async fn run(shared: Shared, disk: Rc<Disk>, index: usize, generation: u64) {
    let _polling = disk.poll.then(Polling::new);
    let window = poll_window();
    let mut last = Instant::now();
    // Chains made available before the first kick are served at once.
    loop {
        let Some(took) = drain(&shared, &disk, index, generation) else {
            return;
        };
        // D293: a polled ring looks again, its guest not notifying, while
        // requests are running and for a window after the last one.
        if disk.poll {
            if took || lock(&shared).running > 0 {
                last = Instant::now();
            }
            if last.elapsed() < window {
                hyperstor_reactor::yield_now().await;
                continue;
            }
            // Quiet: the guest notifies again, unless one just came.
            if notify_again(&shared, index, generation) {
                continue;
            }
        }
        let fd = {
            let s = lock(&shared);
            let v = &s.vrings[index];
            match &v.kick {
                Some(k) if v.generation == generation => k.as_raw_fd(),
                _ => return,
            }
        };
        let (r, _) = fd::read(fd, vec![0; 8]).await;
        if r.is_err() || lock(&shared).vrings[index].generation != generation {
            return;
        }
    }
}

/// How long a polled ring (D293) keeps looking after it was last busy,
/// and its reactor's completion spin while it has one, in µs
/// (`--poll-us`).
static POLL_WINDOW_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(50);

/// Sets the polled rings' window (D293), for every reactor.
pub fn set_poll_window(d: Duration) {
    POLL_WINDOW_US.store(
        d.as_micros().max(1) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) fn poll_window() -> Duration {
    Duration::from_micros(POLL_WINDOW_US.load(std::sync::atomic::Ordering::Relaxed))
}

thread_local! {
    /// The rings this reactor polls (D293).
    static POLLING: Cell<usize> = const { Cell::new(0) };
}

/// Whether this reactor polls a ring (D293): its completion spin is on.
pub(crate) fn polling() -> bool {
    POLLING.with(|p| p.get() > 0)
}

/// A polled ring's task, counted on its reactor while it runs.
struct Polling;

impl Polling {
    fn new() -> Self {
        POLLING.with(|p| p.set(p.get() + 1));
        crate::reactors::set_spin();
        Polling
    }
}

impl Drop for Polling {
    fn drop(&mut self) {
        POLLING.with(|p| p.set(p.get() - 1));
        crate::reactors::set_spin();
    }
}

/// A polled ring going quiet lets its guest notify again. True if a
/// chain came meanwhile (look again), or the ring has stopped (the next
/// look ends the task).
fn notify_again(shared: &Shared, index: usize, generation: u64) -> bool {
    let mut s = lock(shared);
    let Some(mem) = s.mem.clone() else {
        return true;
    };
    let v = &mut s.vrings[index];
    v.generation != generation || v.queue.enable_notification(&*mem).unwrap_or(true)
}

/// Takes every available chain and starts a task for each: whether it
/// took any, or `None` if the ring has stopped. A polled ring's guest is
/// left not notifying (D293).
fn drain(shared: &Shared, disk: &Rc<Disk>, index: usize, generation: u64) -> Option<bool> {
    let mut took = false;
    loop {
        let mut s = lock(shared);
        let event_idx = s.event_idx();
        let mem = s.mem.clone()?;
        let inflight = s.inflight.clone();
        // Parked for a move: [`resume`] starts the ring again there.
        if s.parked {
            return None;
        }
        let v = &mut s.vrings[index];
        if v.generation != generation {
            return None;
        }
        if event_idx || disk.poll {
            let _ = v.queue.disable_notification(&*mem);
        }
        let mut chains = Vec::new();
        while let Some(c) = v.queue.pop_descriptor_chain(mem.clone()) {
            if let Some(inf) = &inflight {
                inf.take(index as u16, c.head_index(), v.counter);
            }
            v.counter += 1;
            chains.push(c);
        }
        // With EVENT_IDX, chains added while notifications were off are
        // seen here: go round again.
        let more = event_idx && !disk.poll && v.queue.enable_notification(&*mem).unwrap_or(false);
        let taken: Vec<(u16, Segments)> = chains
            .iter()
            .map(|c| (c.head_index(), Segments::of(c)))
            .collect();
        s.running += taken.len();
        took |= !taken.is_empty();
        if let Some(w) = &mut s.watch {
            for (_, segs) in &taken {
                w.note(&mem, segs);
            }
        }
        drop(s);
        for (head, segs) in taken {
            hyperstor_reactor::spawn(serve(
                shared.clone(),
                disk.clone(),
                index,
                generation,
                mem.clone(),
                head,
                segs,
            ));
        }
        if !more {
            return Some(took);
        }
    }
}

/// A chain's readable and writable parts, as (guest address, length).
struct Segments {
    readable: Vec<(GuestAddress, u32)>,
    writable: Vec<(GuestAddress, u32)>,
}

impl Segments {
    fn of(chain: &DescriptorChain<Mem>) -> Self {
        let mut s = Segments {
            readable: Vec::new(),
            writable: Vec::new(),
        };
        for d in chain.clone() {
            if d.is_write_only() {
                s.writable.push((d.addr(), d.len()));
            } else {
                s.readable.push((d.addr(), d.len()));
            }
        }
        s
    }

    fn total(segs: &[(GuestAddress, u32)]) -> u64 {
        segs.iter().map(|s| s.1 as u64).sum()
    }
}

/// Reads `len` bytes from `at` into the stream of `segs`.
fn read_stream(
    mem: &GuestMemoryMmap,
    segs: &[(GuestAddress, u32)],
    mut at: u64,
    len: u64,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len as usize);
    for &(addr, n) in segs {
        let n = n as u64;
        if at >= n {
            at -= n;
            continue;
        }
        let take = (n - at).min(len - out.len() as u64);
        let start = out.len();
        out.resize(start + take as usize, 0);
        mem.read_slice(&mut out[start..], addr.checked_add(at)?)
            .ok()?;
        at = 0;
        if out.len() as u64 == len {
            break;
        }
    }
    (out.len() as u64 == len).then_some(out)
}

/// Writes `data` at `at` of the stream of `segs`.
fn write_stream(
    mem: &GuestMemoryMmap,
    segs: &[(GuestAddress, u32)],
    mut at: u64,
    data: &[u8],
) -> Option<()> {
    let mut done = 0usize;
    for &(addr, n) in segs {
        let n = n as u64;
        if at >= n {
            at -= n;
            continue;
        }
        let take = ((n - at) as usize).min(data.len() - done);
        mem.write_slice(&data[done..done + take], addr.checked_add(at)?)
            .ok()?;
        done += take;
        at = 0;
        if done == data.len() {
            break;
        }
    }
    (done == data.len()).then_some(())
}

/// Does one request (the chain at `head`) and completes it.
async fn serve(
    shared: Shared,
    disk: Rc<Disk>,
    index: usize,
    generation: u64,
    mem: Mem,
    head: u16,
    segs: Segments,
) {
    let mem = &*mem;
    let readable_len = Segments::total(&segs.readable);
    let writable_len = Segments::total(&segs.writable);
    let size = disk.volume.volume().size;
    let descriptors = segs.readable.len() + segs.writable.len();
    let first = read_stream(mem, &segs.readable, 0, readable_len.min(16));
    let req = match first {
        Some(h) if h.len() == 16 && h[0..4] == WRITE_ZEROES.to_le_bytes()[..] => {
            // The ranges follow the header.
            let all = read_stream(mem, &segs.readable, 0, readable_len.min(16 + 16 * 64));
            all.map(|b| blk::decode(&b, readable_len, writable_len, descriptors, size))
        }
        Some(h) => Some(blk::decode(
            &h,
            readable_len,
            writable_len,
            descriptors,
            size,
        )),
        None => None,
    };
    let (status, used) = match req {
        None | Some(Err(Bad::Malformed)) => {
            // No status byte to answer in: give the chain back, empty.
            return complete(&shared, index, generation, mem, head, 0);
        }
        // Past the volume or the limits it advertises: refused before a
        // buffer is allocated or guest memory read.
        Some(Err(Bad::Range | Bad::TooLarge)) => (blk::S_IOERR, 1),
        Some(Ok(r)) => {
            let budget = lock(&shared).budget.clone();
            run_request(&disk, &budget, mem, &segs, r, writable_len).await
        }
    };
    if write_stream(mem, &segs.writable, writable_len - 1, &[status]).is_none() {
        return complete(&shared, index, generation, mem, head, 0);
    }
    complete(&shared, index, generation, mem, head, used);
}

/// The most bytes a read or write of `len` at `off` holds in buffers at
/// once: the volume blocks it covers, read or merged whole ([`read_at`],
/// [`write_at`]).
fn buffer_bytes(off: u64, len: u64) -> u64 {
    let (start, end) = blk::blocks(off, len);
    end - start
}

/// Returns the status and how many bytes were written into the chain.
/// Each buffer is taken from the connection's `budget` while it is held,
/// and only then (I375): a write waiting for space between attempts
/// holds none.
async fn run_request(
    disk: &Disk,
    budget: &Budget,
    mem: &GuestMemoryMmap,
    segs: &Segments,
    req: Request,
    writable_len: u64,
) -> (u8, u32) {
    match req {
        Request::Read { off, len } => {
            let _taken = budget.take(buffer_bytes(off, len)).await;
            match read_at(&disk.volume, off, len).await {
                Ok(data) => match write_stream(mem, &segs.writable, 0, &data) {
                    Some(()) => (blk::S_OK, writable_len as u32),
                    None => (blk::S_IOERR, 1),
                },
                Err(_) => (blk::S_IOERR, 1),
            }
        }
        Request::Write { off, len } => {
            let write = || async {
                let _taken = budget.take(buffer_bytes(off, len)).await;
                let data = read_stream(mem, &segs.readable, 16, len).ok_or(None)?;
                write_at(disk, off, data).await.map_err(Some)
            };
            match held(disk, write).await {
                Ok(()) => (blk::S_OK, 1),
                Err(()) => (blk::S_IOERR, 1),
            }
        }
        // Writes are acknowledged once every copy has them (10-volumes.md
        // §10.1): nothing to flush.
        Request::Flush => (blk::S_OK, 1),
        Request::GetId => match write_stream(mem, &segs.writable, 0, &blk::id(&disk.serial)) {
            Some(()) => (blk::S_OK, writable_len as u32),
            None => (blk::S_IOERR, 1),
        },
        Request::WriteZeroes(ranges) => {
            for (off, len) in ranges {
                // Pieces end on piece boundaries: only the range's own ends
                // can be partial blocks.
                let mut at = off;
                while at < off + len {
                    let next = (at / blk::ZERO_PIECE + 1) * blk::ZERO_PIECE;
                    let n = next.min(off + len) - at;
                    let zero = || async {
                        let _taken = budget.take(buffer_bytes(at, n)).await;
                        write_at(disk, at, vec![0; n as usize]).await.map_err(Some)
                    };
                    if held(disk, zero).await.is_err() {
                        return (blk::S_IOERR, 1);
                    }
                    at += n;
                }
            }
            (blk::S_OK, 1)
        }
        Request::Unsupported(_) => (blk::S_UNSUPP, 1),
    }
}

/// Puts the chain on the used ring, and signals the guest if it wants to
/// know. A chain of a ring that has stopped since is dropped: the
/// frontend has taken the ring back.
fn complete(
    shared: &Shared,
    index: usize,
    generation: u64,
    mem: &GuestMemoryMmap,
    head: u16,
    used: u32,
) {
    let mut s = lock(shared);
    s.running = s.running.saturating_sub(1);
    let inflight = s.inflight.clone();
    let v = &mut s.vrings[index];
    if v.generation != generation {
        return;
    }
    if let Some(inf) = &inflight {
        inf.pre_put(index as u16, head);
    }
    if v.queue.add_used(mem, head, used).is_err() {
        return;
    }
    if let Some(inf) = &inflight {
        inf.post_put(index as u16, head, v.queue.next_used());
    }
    if v.queue.needs_notification(mem).unwrap_or(true)
        && let Some(c) = &v.call
    {
        kick(c);
    }
}

/// The vhost-user requests (`vhost`'s handler calls these as it reads
/// them).
struct Handler(Shared);

impl Handler {
    fn shared(&self) -> MutexGuard<'_, State> {
        lock(&self.0)
    }
}

fn unsupported<T>() -> VResult<T> {
    Err(VError::InvalidOperation("not supported by hyperstor-vhost"))
}

impl VhostUserBackendReqHandlerMut for Handler {
    fn set_owner(&mut self) -> VResult<()> {
        Ok(())
    }

    fn reset_owner(&mut self) -> VResult<()> {
        self.reset_device()
    }

    fn reset_device(&mut self) -> VResult<()> {
        let mut s = self.shared();
        s.stop_all();
        s.acked = 0;
        for v in &mut s.vrings {
            v.queue.reset();
            v.enabled = false;
        }
        Ok(())
    }

    fn get_features(&mut self) -> VResult<u64> {
        Ok(blk::features() | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits())
    }

    fn set_features(&mut self, features: u64) -> VResult<()> {
        if features & !self.get_features()? != 0 {
            return Err(VError::InvalidParam);
        }
        self.shared().acked = features;
        Ok(())
    }

    fn set_mem_table(
        &mut self,
        ctx: &[VhostUserMemoryRegion],
        files: Vec<File>,
        _postcopy_listening: Option<bool>,
    ) -> VResult<Option<Vec<VhostUserMemoryRegionBase>>> {
        let mut regions = Vec::new();
        for (r, f) in ctx.iter().zip(files) {
            let map =
                MmapRegion::from_file(FileOffset::new(f, r.mmap_offset), r.memory_size as usize)
                    .map_err(|_| VError::InvalidParam)?;
            let region = GuestRegionMmap::new(map, GuestAddress(r.guest_phys_addr))
                .ok_or(VError::InvalidParam)?;
            regions.push(region);
        }
        let mem = GuestMemoryMmap::from_regions(regions).map_err(|_| VError::InvalidParam)?;
        let mut s = self.shared();
        // The first table places the connection (D292): by the policy of
        // the guest's largest region, as this process maps it (G59).
        if !s.placed {
            let largest = mem.iter().max_by_key(|r| r.len());
            let policy = largest
                .and_then(|r| crate::numa::policy_at(r.as_ptr() as usize).ok())
                .unwrap_or(crate::numa::Policy::Any);
            // Not bound to one node: placed again by its pages.
            if policy.node().is_none() {
                s.watch = Some(Watch {
                    node: None,
                    pages: Default::default(),
                    last: Instant::now(),
                });
            }
            s.place = Some(policy);
        }
        s.mem = Some(Arc::new(mem));
        s.regions = ctx.to_vec();
        let n = s.vrings.len();
        drop(s);
        for i in 0..n {
            start(&self.0, i);
        }
        Ok(None)
    }

    fn set_vring_num(&mut self, index: u32, num: u32) -> VResult<()> {
        if num == 0 || num > QUEUE_MAX as u32 || !num.is_power_of_two() {
            return Err(VError::InvalidParam);
        }
        self.shared().vring(index)?.queue.set_size(num as u16);
        Ok(())
    }

    fn set_vring_addr(
        &mut self,
        index: u32,
        _flags: VhostUserVringAddrFlags,
        descriptor: u64,
        used: u64,
        available: u64,
        _log: u64,
    ) -> VResult<()> {
        let mut s = self.shared();
        let (d, u, a) = (s.gpa(descriptor)?, s.gpa(used)?, s.gpa(available)?);
        let q = &mut s.vring(index)?.queue;
        q.set_desc_table_address(Some(d as u32), Some((d >> 32) as u32));
        q.set_used_ring_address(Some(u as u32), Some((u >> 32) as u32));
        q.set_avail_ring_address(Some(a as u32), Some((a >> 32) as u32));
        Ok(())
    }

    fn set_vring_base(&mut self, index: u32, base: u32) -> VResult<()> {
        let mut s = self.shared();
        let q = &mut s.vring(index)?.queue;
        q.set_next_avail(base as u16);
        q.set_next_used(base as u16);
        Ok(())
    }

    fn get_vring_base(&mut self, index: u32) -> VResult<VhostUserVringState> {
        let mut s = self.shared();
        s.vring(index)?;
        s.stop(index as usize);
        let v = &mut s.vrings[index as usize];
        // A ring stopped this way is disabled until enabled again.
        v.enabled = false;
        let next = v.queue.next_avail();
        Ok(VhostUserVringState::new(index, next as u32))
    }

    fn set_vring_kick(&mut self, index: u8, fd: Option<File>) -> VResult<()> {
        let mut s = self.shared();
        let i = index as u32;
        s.vring(i)?;
        // A task waiting on the old kick is woken to end.
        s.stop(index as usize);
        s.vrings[index as usize].kick = fd;
        drop(s);
        start(&self.0, index as usize);
        Ok(())
    }

    fn set_vring_call(&mut self, index: u8, fd: Option<File>) -> VResult<()> {
        self.shared().vring(index as u32)?.call = fd;
        Ok(())
    }

    fn set_vring_err(&mut self, index: u8, _fd: Option<File>) -> VResult<()> {
        self.shared().vring(index as u32)?;
        Ok(())
    }

    fn get_protocol_features(&mut self) -> VResult<VhostUserProtocolFeatures> {
        Ok(VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::INFLIGHT_SHMFD
            | VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::RESET_DEVICE)
    }

    fn set_protocol_features(&mut self, features: u64) -> VResult<()> {
        let f = VhostUserProtocolFeatures::from_bits(features).ok_or(VError::InvalidParam)?;
        self.shared().protocol = f;
        Ok(())
    }

    fn get_queue_num(&mut self) -> VResult<u64> {
        Ok(self.shared().vrings.len() as u64)
    }

    fn set_vring_enable(&mut self, index: u32, enable: bool) -> VResult<()> {
        let mut s = self.shared();
        s.vring(index)?.enabled = enable;
        if !enable {
            s.stop(index as usize);
        }
        drop(s);
        if enable {
            start(&self.0, index as usize);
        }
        Ok(())
    }

    fn get_config(
        &mut self,
        offset: u32,
        size: u32,
        _flags: VhostUserConfigFlags,
    ) -> VResult<Vec<u8>> {
        let s = self.shared();
        let c = blk::config(s.size, s.vrings.len() as u16);
        let (o, n) = (offset as usize, size as usize);
        if o.checked_add(n).is_none_or(|e| e > c.len()) {
            return Err(VError::InvalidParam);
        }
        Ok(c[o..o + n].to_vec())
    }

    fn set_config(
        &mut self,
        _offset: u32,
        _buf: &[u8],
        _flags: VhostUserConfigFlags,
    ) -> VResult<()> {
        // The guest may not change the configuration (no writeback toggle).
        Ok(())
    }

    fn set_gpu_socket(&mut self, _gpu_backend: GpuBackend) -> VResult<()> {
        unsupported()
    }

    fn get_shared_object(&mut self, _uuid: VhostUserSharedMsg) -> VResult<File> {
        unsupported()
    }

    /// A fresh in-flight buffer for the frontend to keep (D145).
    fn get_inflight_fd(
        &mut self,
        inflight: &VhostUserInflight,
    ) -> VResult<(VhostUserInflight, File)> {
        let mut s = self.shared();
        if inflight.num_queues as usize > s.vrings.len()
            || inflight.queue_size == 0
            || inflight.queue_size > QUEUE_MAX
        {
            return Err(VError::InvalidParam);
        }
        let inf = Inflight::create(inflight.num_queues, inflight.queue_size)
            .map_err(|_| VError::InvalidParam)?;
        let file = inf.file().try_clone().map_err(|_| VError::InvalidParam)?;
        let answer = VhostUserInflight {
            mmap_size: inf.size(),
            mmap_offset: 0,
            num_queues: inflight.num_queues,
            queue_size: inflight.queue_size,
        };
        s.inflight = Some(Arc::new(inf));
        Ok((answer, file))
    }

    /// The frontend's in-flight buffer: the one it got from this process,
    /// or after a restart from the last (D145).
    fn set_inflight_fd(&mut self, inflight: &VhostUserInflight, file: File) -> VResult<()> {
        let mut s = self.shared();
        if inflight.num_queues as usize > s.vrings.len() || inflight.queue_size > QUEUE_MAX {
            return Err(VError::InvalidParam);
        }
        let inf = Inflight::open(
            file,
            inflight.mmap_offset,
            inflight.mmap_size,
            inflight.num_queues,
            inflight.queue_size,
        )
        .map_err(|_| VError::InvalidParam)?;
        s.inflight = Some(Arc::new(inf));
        Ok(())
    }

    fn get_max_mem_slots(&mut self) -> VResult<u64> {
        unsupported()
    }

    fn add_mem_region(
        &mut self,
        _region: &VhostUserSingleMemoryRegion,
        _fd: File,
        _postcopy_listening: Option<bool>,
    ) -> VResult<Option<VhostUserMemoryRegionBase>> {
        unsupported()
    }

    fn remove_mem_region(&mut self, _region: &VhostUserSingleMemoryRegion) -> VResult<()> {
        unsupported()
    }

    fn set_device_state_fd(
        &mut self,
        _direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        _fd: File,
    ) -> VResult<Option<File>> {
        unsupported()
    }

    fn check_device_state(&mut self) -> VResult<()> {
        unsupported()
    }

    fn get_shmem_config(&mut self) -> VResult<VhostUserShMemConfig> {
        unsupported()
    }

    fn set_log_base(&mut self, _log: &VhostUserLog, _file: File) -> VResult<()> {
        unsupported()
    }
}

/// How a connection's part on one reactor ended.
pub enum Ended {
    /// The frontend hung up, or the protocol failed.
    Closed(Result<(), String>),
    /// The first memory table came (D292): the connection is to go on, by
    /// [`resume`], on the reactor its guest's memory says.
    Place(Moving, crate::numa::Policy),
}

/// A connection between reactors: `vhost`'s handler with its state.
pub struct Moving {
    req: BackendReqHandler<Mutex<Handler>>,
    shared: Shared,
}

/// Serves one frontend connected to `volume`'s socket (`size` bytes,
/// `queues` queues) until it disconnects, the protocol fails, or its
/// memory table comes and it is to be placed.
pub async fn serve_connection(volume: u128, size: u64, queues: u16, stream: UnixStream) -> Ended {
    let shared: Shared = Arc::new(Mutex::new(State::new(volume, size, queues)));
    let handler = Arc::new(Mutex::new(Handler(shared.clone())));
    let req = BackendReqHandler::from_stream(stream, handler);
    run_connection(Moving { req, shared }).await
}

/// Carries on with a connection on the reactor it was placed on, which
/// holds its disk and is on NUMA node `node`: starts its rings and serves
/// it until it disconnects, the protocol fails, or (a guest whose memory
/// isn't bound) its pages turn out to be on another node.
pub async fn resume(m: Moving, node: Option<u32>) -> Ended {
    let n = {
        let mut s = lock(&m.shared);
        s.placed = true;
        s.parked = false;
        if let Some(w) = &mut s.watch {
            w.node = node;
            w.last = Instant::now();
        }
        s.vrings.len()
    };
    for i in 0..n {
        start(&m.shared, i);
    }
    run_connection(m).await
}

/// How long a move waits for the requests in flight; past it the
/// connection stays (a write held for space, say).
const PARK_FOR: Duration = Duration::from_secs(5);

async fn run_connection(mut m: Moving) -> Ended {
    let fd = m.req.as_raw_fd();
    let r = loop {
        // Woken each WATCH_EVERY only while watching an unbound guest.
        let watching = {
            let s = lock(&m.shared);
            s.placed && s.watch.is_some()
        };
        let ready = if watching {
            hyperstor_reactor::time::timeout(WATCH_EVERY, fd::readable(fd)).await
        } else {
            Ok(fd::readable(fd).await)
        };
        match ready {
            Ok(Err(e)) => break Err(e.to_string()),
            Ok(Ok(())) => match m.req.handle_request() {
                Ok(()) => {}
                Err(VError::Disconnected) | Err(VError::PartialMessage) => break Ok(()),
                // A request we refused was answered (or the frontend
                // didn't ask for an answer); carry on.
                Err(VError::InvalidOperation(_)) | Err(VError::InvalidParam) => {}
                Err(e) => break Err(e.to_string()),
            },
            Err(_) => {}
        }
        let place = {
            let mut s = lock(&m.shared);
            if s.placed { None } else { s.place.take() }
        };
        if let Some(p) = place {
            return Ended::Place(m, p);
        }
        if let Some(node) = due(&m.shared)
            && park(&m.shared).await
        {
            return Ended::Place(m, crate::numa::Policy::Nodes(vec![node]));
        }
    };
    lock(&m.shared).stop_all();
    Ended::Closed(r)
}

/// For a placed connection whose guest memory isn't bound, once a
/// [`WATCH_EVERY`]: the node its pages are on, if not its reactor's. It
/// is placed by its pages once (D292), so the watch ends either way once
/// a node has the pages.
fn due(shared: &Shared) -> Option<u32> {
    let mut s = lock(shared);
    if !s.placed {
        return None;
    }
    let w = s.watch.as_mut()?;
    if w.last.elapsed() < WATCH_EVERY {
        return None;
    }
    w.last = Instant::now();
    let node = w.elsewhere();
    if node.is_some() {
        s.watch = None;
    }
    node
}

/// Stops the rings taking requests and waits for those in flight, for a
/// move. False, with the rings started again, if they take longer than
/// [`PARK_FOR`].
async fn park(shared: &Shared) -> bool {
    let kicks: Vec<File> = {
        let mut s = lock(shared);
        s.parked = true;
        s.vrings
            .iter_mut()
            .filter(|v| v.running)
            .filter_map(|v| {
                v.running = false;
                v.kick.as_ref().and_then(|k| k.try_clone().ok())
            })
            .collect()
    };
    // Wakes each ring's task, which sees it is parked and ends; the
    // guest's kick it took is made up for by [`resume`]'s first drain.
    for k in &kicks {
        kick(k);
    }
    let t = Instant::now();
    while lock(shared).running > 0 {
        if t.elapsed() > PARK_FOR {
            lock(shared).parked = false;
            let n = lock(shared).vrings.len();
            for i in 0..n {
                start(shared, i);
            }
            return false;
        }
        sleep(Duration::from_millis(1)).await;
    }
    true
}
