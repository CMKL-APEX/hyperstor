//! One vhost-user-blk connection: the frontend (Cloud Hypervisor or QEMU)
//! sets up guest memory and the virtqueues through `vhost`'s request
//! handler, and each enabled queue is served by a task on this reactor
//! (decisions.md D111): it waits on the queue's kick eventfd, takes the
//! chains the guest made available, does each request against the
//! attached volume, and signals the call eventfd when the guest wants it
//! (`EVENT_IDX`).

use crate::blk::{self, Bad, Request};
use hyperstor_client::volume::Attached;
use hyperstor_reactor::fd;
use std::cell::RefCell;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
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
    Address, Bytes, FileOffset, GuestAddress, GuestMemoryMmap, GuestRegionMmap, MmapRegion,
};

const WRITE_ZEROES: u32 = virtio_bindings::virtio_blk::VIRTIO_BLK_T_WRITE_ZEROES;

/// Descriptors per queue at most (the frontend picks the size, up to this).
pub const QUEUE_MAX: u16 = 1024;

/// A volume as a disk: what every connection to its socket serves.
pub struct Disk {
    pub volume: Rc<Attached>,
    /// What `GET_ID` returns (frontends.md §11.1: the glidex disk id).
    pub serial: String,
    /// Queues offered.
    pub queues: u16,
}

type Mem = Rc<GuestMemoryMmap>;

struct Vring {
    queue: Queue,
    kick: Option<File>,
    call: Option<File>,
    enabled: bool,
    /// Bumped whenever the ring stops; a task serving an older generation
    /// ends, and its requests' completions are dropped.
    generation: u64,
    running: bool,
}

struct State {
    disk: Rc<Disk>,
    acked: u64,
    protocol: VhostUserProtocolFeatures,
    mem: Option<Mem>,
    regions: Vec<VhostUserMemoryRegion>,
    vrings: Vec<Vring>,
}

type Shared = Rc<RefCell<State>>;

impl State {
    fn new(disk: Rc<Disk>) -> Self {
        let vrings = (0..disk.queues)
            .map(|_| Vring {
                queue: Queue::new(QUEUE_MAX).expect("a valid queue size"),
                kick: None,
                call: None,
                enabled: false,
                generation: 0,
                running: false,
            })
            .collect();
        Self {
            disk,
            acked: 0,
            protocol: VhostUserProtocolFeatures::empty(),
            mem: None,
            regions: Vec::new(),
            vrings,
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
    let mut s = shared.borrow_mut();
    let can = s.mem.is_some() && (s.vrings[index].enabled || !s.protocol_acked());
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
    v.running = true;
    let generation = v.generation;
    drop(s);
    hyperstor_reactor::spawn(run(shared.clone(), index, generation));
}

/// Serves ring `index` until it stops.
async fn run(shared: Shared, index: usize, generation: u64) {
    // Chains made available before the first kick are served at once.
    loop {
        if !drain(&shared, index, generation) {
            return;
        }
        let fd = {
            let s = shared.borrow();
            let v = &s.vrings[index];
            match &v.kick {
                Some(k) if v.generation == generation => k.as_raw_fd(),
                _ => return,
            }
        };
        let (r, _) = fd::read(fd, vec![0; 8]).await;
        if r.is_err() || shared.borrow().vrings[index].generation != generation {
            return;
        }
    }
}

/// Takes every available chain and starts a task for each. Returns false
/// if the ring has stopped.
fn drain(shared: &Shared, index: usize, generation: u64) -> bool {
    loop {
        let mut s = shared.borrow_mut();
        let event_idx = s.event_idx();
        let Some(mem) = s.mem.clone() else {
            return false;
        };
        let v = &mut s.vrings[index];
        if v.generation != generation {
            return false;
        }
        if event_idx {
            let _ = v.queue.disable_notification(&*mem);
        }
        let mut chains = Vec::new();
        while let Some(c) = v.queue.pop_descriptor_chain(mem.clone()) {
            chains.push(c);
        }
        // With EVENT_IDX, chains added while notifications were off are
        // seen here: go round again.
        let more = event_idx && v.queue.enable_notification(&*mem).unwrap_or(false);
        let disk = s.disk.clone();
        drop(s);
        for c in chains {
            hyperstor_reactor::spawn(serve(shared.clone(), disk.clone(), index, generation, c));
        }
        if !more {
            return true;
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

/// Does one request and completes its chain.
async fn serve(
    shared: Shared,
    disk: Rc<Disk>,
    index: usize,
    generation: u64,
    chain: DescriptorChain<Mem>,
) {
    let mem = chain.memory();
    let head = chain.head_index();
    let segs = Segments::of(&chain);
    let readable_len = Segments::total(&segs.readable);
    let writable_len = Segments::total(&segs.writable);
    let size = disk.volume.volume().size;
    let first = read_stream(mem, &segs.readable, 0, readable_len.min(16));
    let req = match first {
        Some(h) if h.len() == 16 && h[0..4] == WRITE_ZEROES.to_le_bytes()[..] => {
            // The ranges follow the header.
            let all = read_stream(mem, &segs.readable, 0, readable_len.min(16 + 16 * 64));
            all.map(|b| blk::decode(&b, readable_len, writable_len, size))
        }
        Some(h) => Some(blk::decode(&h, readable_len, writable_len, size)),
        None => None,
    };
    let (status, used) = match req {
        None | Some(Err(Bad::Malformed)) => {
            // No status byte to answer in: give the chain back, empty.
            return complete(&shared, index, generation, mem, head, 0);
        }
        Some(Err(Bad::Range)) => (blk::S_IOERR, 1),
        Some(Ok(r)) => run_request(&disk, mem, &segs, r, writable_len).await,
    };
    if write_stream(mem, &segs.writable, writable_len - 1, &[status]).is_none() {
        return complete(&shared, index, generation, mem, head, 0);
    }
    complete(&shared, index, generation, mem, head, used);
}

/// Returns the status and how many bytes were written into the chain.
async fn run_request(
    disk: &Disk,
    mem: &GuestMemoryMmap,
    segs: &Segments,
    req: Request,
    writable_len: u64,
) -> (u8, u32) {
    let v = &disk.volume;
    match req {
        Request::Read { off, len } => match v.read(off, len).await {
            Ok(data) => match write_stream(mem, &segs.writable, 0, &data) {
                Some(()) => (blk::S_OK, writable_len as u32),
                None => (blk::S_IOERR, 1),
            },
            Err(_) => (blk::S_IOERR, 1),
        },
        Request::Write { off, len } => {
            let Some(data) = read_stream(mem, &segs.readable, 16, len) else {
                return (blk::S_IOERR, 1);
            };
            match v.write(off, data).await {
                Ok(()) => (blk::S_OK, 1),
                Err(_) => (blk::S_IOERR, 1),
            }
        }
        // Writes are acknowledged once every copy has them (volumes.md
        // §10.1): nothing to flush.
        Request::Flush => (blk::S_OK, 1),
        Request::GetId => match write_stream(mem, &segs.writable, 0, &blk::id(&disk.serial)) {
            Some(()) => (blk::S_OK, writable_len as u32),
            None => (blk::S_IOERR, 1),
        },
        Request::WriteZeroes(ranges) => {
            for (off, len) in ranges {
                let mut at = off;
                while at < off + len {
                    let n = blk::ZERO_PIECE.min(off + len - at);
                    if v.write(at, vec![0; n as usize]).await.is_err() {
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
    let mut s = shared.borrow_mut();
    let v = &mut s.vrings[index];
    if v.generation != generation {
        return;
    }
    if v.queue.add_used(mem, head, used).is_err() {
        return;
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
    fn shared(&self) -> std::cell::RefMut<'_, State> {
        self.0.borrow_mut()
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
        s.mem = Some(Rc::new(mem));
        s.regions = ctx.to_vec();
        drop(s);
        let n = self.0.borrow().vrings.len();
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
        let c = blk::config(s.disk.volume.volume().size, s.disk.queues);
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

    fn get_inflight_fd(
        &mut self,
        _inflight: &VhostUserInflight,
    ) -> VResult<(VhostUserInflight, File)> {
        unsupported()
    }

    fn set_inflight_fd(&mut self, _inflight: &VhostUserInflight, _file: File) -> VResult<()> {
        unsupported()
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

/// Serves one frontend connected to a disk's socket until it disconnects
/// (or the protocol fails). Its rings stop with it; the volume stays
/// attached for the next connection (a hypervisor reconnecting).
pub async fn serve_connection(disk: Rc<Disk>, stream: UnixStream) -> Result<(), String> {
    let shared: Shared = Rc::new(RefCell::new(State::new(disk)));
    // `vhost` wants its handler in an Arc; it never leaves this thread.
    #[allow(clippy::arc_with_non_send_sync)]
    let handler = Arc::new(Mutex::new(Handler(shared.clone())));
    let mut req = BackendReqHandler::from_stream(stream, handler);
    let fd = req.as_raw_fd();
    let r = loop {
        if let Err(e) = fd::readable(fd).await {
            break Err(e.to_string());
        }
        match req.handle_request() {
            Ok(()) => {}
            Err(VError::Disconnected) | Err(VError::PartialMessage) => break Ok(()),
            // A request we refused was answered (or the frontend didn't
            // ask for an answer); carry on.
            Err(VError::InvalidOperation(_)) | Err(VError::InvalidParam) => {}
            Err(e) => break Err(e.to_string()),
        }
    };
    shared.borrow_mut().stop_all();
    r
}
