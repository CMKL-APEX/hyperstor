//! Block devices under the engine (spec/08-engine.md §6.2, D29).
//!
//! Every I/O is block-aligned in offset and length. Implementations:
//! - [`MemDev`]: memory, for tests;
//! - [`FileDev`]: a file or block device through `pread`/`pwrite`,
//!   optionally `O_DIRECT`;
//! - [`UringDev`]: a file or block device through io_uring, `O_DIRECT`,
//!   with whole batches submitted at once (the block back end of D29);
//! - [`FaultDev`]: memory with a volatile write cache, power loss and
//!   torn writes, for crash tests.
//!
//! The NVMe passthrough back end (`uring_cmd` on `/dev/ngXnY`) needs a
//! dedicated namespace to test on and comes later (see
//! spec/01-implementation-notes.md).

use crate::align::BLOCK;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Mutex;

/// A block device. All offsets and lengths are multiples of [`BLOCK`].
pub trait BlockDev: Send + Sync {
    /// Size in bytes.
    fn len(&self) -> u64;
    fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write(&self, off: u64, buf: &[u8]) -> io::Result<()>;
    /// Makes every completed write durable.
    fn flush(&self) -> io::Result<()>;

    /// The descriptor a caller with an io_uring of its own may write and
    /// flush through instead of waiting in these calls (I199). `None`
    /// (memory, test devices) keeps the caller to the calls here.
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    /// Writes several ranges; implementations may submit them together.
    fn write_batch(&self, writes: &[(u64, &[u8])]) -> io::Result<()> {
        for (off, buf) in writes {
            self.write(*off, buf)?;
        }
        Ok(())
    }

    /// Reads several ranges; implementations may submit them together.
    fn read_batch(&self, reads: &mut [(u64, &mut [u8])]) -> io::Result<()> {
        for (off, buf) in reads.iter_mut() {
            self.read(*off, buf)?;
        }
        Ok(())
    }

    /// Tells the device a range holds nothing it needs (D275): a
    /// deallocate on a drive, a hole in a file. What reads there
    /// afterwards is undefined until written again. `Unsupported` where
    /// there is nothing to tell.
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        let _ = (off, len);
        Err(io::ErrorKind::Unsupported.into())
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `BLKDISCARD`, `_IO(0x12, 119)` (linux/fs.h); `libc` doesn't name it.
const BLKDISCARD: libc::Ioctl = 0x1277;

/// Discards `[off, off + len)` of `file` (D275): `BLKDISCARD` on a block
/// device, a punched hole in a regular file.
fn discard_fd(file: &File, off: u64, len: u64) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let r = if file.metadata()?.file_type().is_block_device() {
        let range = [off, len];
        // SAFETY: BLKDISCARD reads two u64s (start, length) from the
        // pointer, which points at `range`, alive for the call.
        unsafe { libc::ioctl(fd, BLKDISCARD, range.as_ptr()) }
    } else {
        // SAFETY: fallocate takes plain integers; `fd` is open for writing.
        unsafe {
            libc::fallocate(
                fd,
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                off as libc::off_t,
                len as libc::off_t,
            )
        }
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn check_aligned(off: u64, len: usize, dev_len: u64) -> io::Result<()> {
    if !off.is_multiple_of(BLOCK as u64) || !len.is_multiple_of(BLOCK) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unaligned I/O"));
    }
    if off + len as u64 > dev_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("I/O past end of device: {off}+{len} > {dev_len}"),
        ));
    }
    Ok(())
}

/// A device in memory.
pub struct MemDev {
    data: Mutex<Vec<u8>>,
}

impl MemDev {
    pub fn new(len: u64) -> Self {
        assert_eq!(len % BLOCK as u64, 0);
        Self {
            data: Mutex::new(vec![0; len as usize]),
        }
    }

    pub fn from_image(img: Vec<u8>) -> Self {
        assert_eq!(img.len() % BLOCK, 0);
        Self {
            data: Mutex::new(img),
        }
    }

    pub fn image(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }
}

impl BlockDev for MemDev {
    fn len(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
    fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let d = self.data.lock().unwrap();
        check_aligned(off, buf.len(), d.len() as u64)?;
        buf.copy_from_slice(&d[off as usize..off as usize + buf.len()]);
        Ok(())
    }
    fn write(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        let mut d = self.data.lock().unwrap();
        check_aligned(off, buf.len(), d.len() as u64)?;
        d[off as usize..off as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    /// Zeros, so that a test reading a block discarded while it was in
    /// use sees it.
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        let mut d = self.data.lock().unwrap();
        check_aligned(off, len as usize, d.len() as u64)?;
        d[off as usize..(off + len) as usize].fill(0);
        Ok(())
    }
}

fn open_dev(path: &Path, direct: bool, create_len: Option<u64>) -> io::Result<(File, u64)> {
    let mut o = OpenOptions::new();
    o.read(true).write(true);
    if create_len.is_some() {
        o.create(true);
    }
    if direct {
        o.custom_flags(libc::O_DIRECT);
    }
    let f = o.open(path)?;
    if let Some(len) = create_len
        && f.metadata()?.len() < len
    {
        f.set_len(len)?;
    }
    let len = dev_len(&f)?;
    Ok((f, len - len % BLOCK as u64))
}

/// Size of a regular file, or of a block device (whose metadata says 0).
fn dev_len(f: &File) -> io::Result<u64> {
    let m = f.metadata()?;
    use std::os::unix::fs::FileTypeExt;
    if m.file_type().is_block_device() {
        let mut size: u64 = 0;
        // BLKGETSIZE64 = _IOR(0x12, 114, size_t)
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        // SAFETY: the ioctl writes one u64 into `size`, which outlives the call.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64, &mut size as *mut u64) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(size)
    } else {
        Ok(m.len())
    }
}

/// A file or block device through `pread`/`pwrite`.
pub struct FileDev {
    file: File,
    len: u64,
}

impl FileDev {
    /// Opens `path`. With `create_len`, creates or extends a regular file to
    /// that size. With `direct`, bypasses the page cache (`O_DIRECT`), and
    /// buffers must then be aligned ([`crate::AlignedBuf`]).
    pub fn open(path: &Path, direct: bool, create_len: Option<u64>) -> io::Result<Self> {
        let (file, len) = open_dev(path, direct, create_len)?;
        Ok(Self { file, len })
    }
}

impl BlockDev for FileDev {
    fn len(&self) -> u64 {
        self.len
    }
    fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        check_aligned(off, buf.len(), self.len)?;
        self.file.read_exact_at(buf, off)
    }
    fn write(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        check_aligned(off, buf.len(), self.len)?;
        self.file.write_all_at(buf, off)
    }
    fn flush(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.file.as_raw_fd())
    }
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        check_aligned(off, len as usize, self.len)?;
        discard_fd(&self.file, off, len)
    }
}

/// A file or block device through io_uring. A batch of reads or writes is
/// submitted with one system call and reaped together.
pub struct UringDev {
    file: File,
    len: u64,
    ring: Mutex<io_uring::IoUring>,
    depth: u32,
}

impl UringDev {
    /// Opens `path` as [`FileDev::open`] does, with a ring of `depth` entries.
    pub fn open(
        path: &Path,
        direct: bool,
        create_len: Option<u64>,
        depth: u32,
    ) -> io::Result<Self> {
        let (file, len) = open_dev(path, direct, create_len)?;
        let ring = io_uring::IoUring::new(depth)?;
        Ok(Self {
            file,
            len,
            ring: Mutex::new(ring),
            depth,
        })
    }

    /// Submits `ops` (each an SQE plus the byte count it must transfer) in
    /// chunks of the ring's depth and waits for all of them.
    fn run(&self, ops: &[(io_uring::squeue::Entry, usize)]) -> io::Result<()> {
        let mut ring = self.ring.lock().unwrap();
        for chunk in ops.chunks(self.depth as usize) {
            for (i, (sqe, _)) in chunk.iter().enumerate() {
                let sqe = sqe.clone().user_data(i as u64);
                // SAFETY: the buffers named by the SQEs belong to the caller's
                // slices, which outlive this call, and we wait below for every
                // submitted entry to complete before returning.
                unsafe {
                    ring.submission()
                        .push(&sqe)
                        .map_err(|_| io::Error::other("submission queue full"))?;
                }
            }
            ring.submit_and_wait(chunk.len())?;
            let mut done = 0;
            let mut err = None;
            while done < chunk.len() {
                for cqe in ring.completion() {
                    done += 1;
                    let i = cqe.user_data() as usize;
                    let want = chunk[i].1;
                    let r = cqe.result();
                    if r < 0 {
                        err.get_or_insert(io::Error::from_raw_os_error(-r));
                    } else if (r as usize) != want {
                        err.get_or_insert(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("short transfer: {r} of {want} bytes"),
                        ));
                    }
                }
                if done < chunk.len() {
                    ring.submit_and_wait(chunk.len() - done)?;
                }
            }
            if let Some(e) = err {
                return Err(e);
            }
        }
        Ok(())
    }
}

impl BlockDev for UringDev {
    fn len(&self) -> u64 {
        self.len
    }
    fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.read_batch(&mut [(off, buf)])
    }
    fn write(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.write_batch(&[(off, buf)])
    }
    fn flush(&self) -> io::Result<()> {
        use io_uring::{opcode, types};
        let fd = types::Fd(self.file.as_raw_fd());
        let sqe = opcode::Fsync::new(fd)
            .flags(types::FsyncFlags::DATASYNC)
            .build();
        self.run(&[(sqe, 0)])
    }
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.file.as_raw_fd())
    }
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        check_aligned(off, len as usize, self.len)?;
        discard_fd(&self.file, off, len)
    }
    fn write_batch(&self, writes: &[(u64, &[u8])]) -> io::Result<()> {
        use io_uring::{opcode, types};
        let fd = types::Fd(self.file.as_raw_fd());
        let mut ops = Vec::with_capacity(writes.len());
        for (off, buf) in writes {
            check_aligned(*off, buf.len(), self.len)?;
            let sqe = opcode::Write::new(fd, buf.as_ptr(), buf.len() as u32)
                .offset(*off)
                .build();
            ops.push((sqe, buf.len()));
        }
        self.run(&ops)
    }
    fn read_batch(&self, reads: &mut [(u64, &mut [u8])]) -> io::Result<()> {
        use io_uring::{opcode, types};
        let fd = types::Fd(self.file.as_raw_fd());
        let mut ops = Vec::with_capacity(reads.len());
        for (off, buf) in reads.iter_mut() {
            check_aligned(*off, buf.len(), self.len)?;
            let sqe = opcode::Read::new(fd, buf.as_mut_ptr(), buf.len() as u32)
                .offset(*off)
                .build();
            ops.push((sqe, buf.len()));
        }
        self.run(&ops)
    }
}

/// A device in memory with a volatile write cache, for crash tests.
///
/// Writes land in the cache; [`BlockDev::flush`] makes them durable.
/// [`FaultDev::crash_image`] returns what a power loss would leave: the
/// durable image plus an arbitrary subset of cached blocks (each block of
/// each cached write kept or lost independently, so writes are reordered
/// and multi-block writes torn). After [`FaultDev::fail_after`] operations
/// the device fails every call, as if power went out in the middle.
/// [`FaultDev::misstep`] makes one block's write go wrong on its way to the
/// media, as a device that lies does (#241).
pub struct FaultDev {
    inner: Mutex<FaultState>,
}

/// How one block's write goes wrong on its way to the media, though the
/// device acknowledged it: reads see it until the flush, then not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    /// Never stored.
    Lost,
    /// Stored at this block instead.
    Misdirected(u64),
}

struct FaultState {
    durable: Vec<u8>,
    /// Cached writes since the last flush, in order: (offset, data, fault).
    cache: Vec<(u64, Vec<u8>, Option<WriteFault>)>,
    ops_left: Option<u64>,
    /// Blocks written so far, and the faults armed by the index of the
    /// block written.
    written: u64,
    faults: std::collections::BTreeMap<u64, WriteFault>,
    /// The faults that struck: the block each write was meant for.
    struck: Vec<(u64, WriteFault)>,
}

impl FaultDev {
    pub fn new(len: u64) -> Self {
        Self::from_image(vec![0; len as usize])
    }

    pub fn from_image(img: Vec<u8>) -> Self {
        assert_eq!(img.len() % BLOCK, 0);
        Self {
            inner: Mutex::new(FaultState {
                durable: img,
                cache: Vec::new(),
                ops_left: None,
                written: 0,
                faults: Default::default(),
                struck: Vec::new(),
            }),
        }
    }

    /// After `n` more operations, every operation fails.
    pub fn fail_after(&self, n: u64) {
        self.inner.lock().unwrap().ops_left = Some(n);
    }

    /// Makes the write of the `at`-th block from now (0: the next one) go
    /// wrong as `fault` says.
    pub fn misstep(&self, at: u64, fault: WriteFault) {
        let mut s = self.inner.lock().unwrap();
        let i = s.written + at;
        s.faults.insert(i, fault);
    }

    /// The faults that struck so far: the block each write was meant for,
    /// and what happened to it.
    pub fn struck(&self) -> Vec<(u64, WriteFault)> {
        self.inner.lock().unwrap().struck.clone()
    }

    /// The image the media holds now: what was flushed (an unflushed cache
    /// is lost).
    pub fn image(&self) -> Vec<u8> {
        self.inner.lock().unwrap().durable.clone()
    }

    /// The image a power loss would leave. `keep(i)` decides, for the i-th
    /// cached block in write order, whether it reached the media.
    pub fn crash_image(&self, mut keep: impl FnMut(usize) -> bool) -> Vec<u8> {
        let s = self.inner.lock().unwrap();
        let mut img = s.durable.clone();
        let mut i = 0;
        for (off, data, fault) in &s.cache {
            for (b, block) in data.chunks(BLOCK).enumerate() {
                if keep(i) {
                    store(&mut img, *off as usize + b * BLOCK, block, *fault);
                }
                i += 1;
            }
        }
        img
    }

    /// Number of blocks written since the last flush.
    pub fn cached_blocks(&self) -> usize {
        let s = self.inner.lock().unwrap();
        s.cache.iter().map(|(_, d, _)| d.len() / BLOCK).sum()
    }

    fn tick(s: &mut FaultState) -> io::Result<()> {
        if let Some(n) = s.ops_left.as_mut() {
            if *n == 0 {
                return Err(io::Error::other("injected power loss"));
            }
            *n -= 1;
        }
        Ok(())
    }

    /// The current contents as a reader sees them (durable plus cache).
    fn view(s: &FaultState, off: u64, buf: &mut [u8]) {
        buf.copy_from_slice(&s.durable[off as usize..off as usize + buf.len()]);
        let end = off + buf.len() as u64;
        for (woff, data, _) in &s.cache {
            let wend = woff + data.len() as u64;
            let lo = off.max(*woff);
            let hi = end.min(wend);
            if lo < hi {
                buf[(lo - off) as usize..(hi - off) as usize]
                    .copy_from_slice(&data[(lo - woff) as usize..(hi - woff) as usize]);
            }
        }
    }
}

impl BlockDev for FaultDev {
    fn len(&self) -> u64 {
        self.inner.lock().unwrap().durable.len() as u64
    }
    fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        check_aligned(off, buf.len(), s.durable.len() as u64)?;
        Self::tick(&mut s)?;
        Self::view(&s, off, buf);
        Ok(())
    }
    fn write(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        check_aligned(off, buf.len(), s.durable.len() as u64)?;
        Self::tick(&mut s)?;
        let first = s.written;
        let n = (buf.len() / BLOCK) as u64;
        s.written += n;
        let armed: Vec<(u64, WriteFault)> = s
            .faults
            .range(first..first + n)
            .map(|(i, f)| (*i, *f))
            .collect();
        if armed.is_empty() {
            s.cache.push((off, buf.to_vec(), None));
        } else {
            // Block by block, so the fault is on its block alone.
            for (b, block) in buf.chunks(BLOCK).enumerate() {
                let i = first + b as u64;
                let fault = s.faults.remove(&i);
                if let Some(f) = fault {
                    s.struck.push((off / BLOCK as u64 + b as u64, f));
                }
                s.cache
                    .push((off + (b * BLOCK) as u64, block.to_vec(), fault));
            }
        }
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::tick(&mut s)?;
        let cache = std::mem::take(&mut s.cache);
        for (off, data, fault) in cache {
            for (b, block) in data.chunks(BLOCK).enumerate() {
                store(&mut s.durable, off as usize + b * BLOCK, block, fault);
            }
        }
        Ok(())
    }
    /// Zeros at once, on the media and in cached writes alike, so that
    /// neither a flush nor a crash brings back what was there: a test
    /// reading a block discarded while in use sees zeros.
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        check_aligned(off, len as usize, s.durable.len() as u64)?;
        Self::tick(&mut s)?;
        let (a, b) = (off as usize, (off + len) as usize);
        s.durable[a..b].fill(0);
        for (at, data, _) in &mut s.cache {
            let (ca, cb) = (*at as usize, *at as usize + data.len());
            if ca < b && a < cb {
                data[a.max(ca) - ca..b.min(cb) - ca].fill(0);
            }
        }
        Ok(())
    }
}

/// Stores one block's write at `at` in `img`, or where `fault` sends it.
fn store(img: &mut [u8], at: usize, block: &[u8], fault: Option<WriteFault>) {
    let at = match fault {
        None => at,
        Some(WriteFault::Lost) => return,
        Some(WriteFault::Misdirected(b)) => b as usize * BLOCK,
    };
    img[at..at + BLOCK].copy_from_slice(block);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::AlignedBuf;

    fn exercise(dev: &dyn BlockDev) {
        let mut a = AlignedBuf::zeroed(2 * BLOCK);
        a.fill(0xab);
        let mut b = AlignedBuf::zeroed(BLOCK);
        b.fill(0xcd);
        dev.write_batch(&[(0, &a), (4 * BLOCK as u64, &b)]).unwrap();
        dev.flush().unwrap();
        let mut r1 = AlignedBuf::zeroed(2 * BLOCK);
        let mut r2 = AlignedBuf::zeroed(BLOCK);
        dev.read_batch(&mut [(0, &mut r1), (4 * BLOCK as u64, &mut r2)])
            .unwrap();
        assert!(r1.iter().all(|&x| x == 0xab));
        assert!(r2.iter().all(|&x| x == 0xcd));
        assert!(dev.write(1, &b).is_err());
        assert!(dev.write(dev.len(), &b).is_err());
    }

    #[test]
    fn mem_dev() {
        exercise(&MemDev::new(16 * BLOCK as u64));
    }

    #[test]
    fn file_and_uring_dev_direct() {
        let dir = std::env::temp_dir().join(format!("hs-dev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        // O_DIRECT isn't supported on every filesystem (tmpfs refuses it).
        let direct = FileDev::open(&p, true, Some(16 * BLOCK as u64)).is_ok();
        exercise(&FileDev::open(&p, direct, Some(16 * BLOCK as u64)).unwrap());
        exercise(&UringDev::open(&p, direct, None, 8).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A discard reads back as zeros on a file (a punched hole, D275),
    /// in memory and on the fault device, where neither a flush nor a
    /// crash brings back what a cached write put there.
    #[test]
    fn discarded_ranges_read_as_zeros() {
        let check = |d: &dyn BlockDev| {
            let ones = AlignedBuf::from_slice(&vec![1u8; 4 * BLOCK]);
            d.write(0, &ones).unwrap();
            d.flush().unwrap();
            d.discard(BLOCK as u64, 2 * BLOCK as u64).unwrap();
            let mut r = AlignedBuf::zeroed(4 * BLOCK);
            d.read(0, &mut r).unwrap();
            assert!(r[..BLOCK].iter().all(|&x| x == 1));
            assert!(r[BLOCK..3 * BLOCK].iter().all(|&x| x == 0));
            assert!(r[3 * BLOCK..].iter().all(|&x| x == 1));
            assert!(d.discard(1, BLOCK as u64).is_err(), "aligned only");
            assert!(
                d.discard(d.len(), BLOCK as u64).is_err(),
                "within the device"
            );
        };
        check(&MemDev::new(8 * BLOCK as u64));
        let f = FaultDev::new(8 * BLOCK as u64);
        check(&f);
        f.write(4 * BLOCK as u64, &vec![7u8; 2 * BLOCK]).unwrap();
        f.discard(5 * BLOCK as u64, BLOCK as u64).unwrap();
        let img = f.crash_image(|_| true);
        assert!(img[4 * BLOCK..5 * BLOCK].iter().all(|&x| x == 7));
        assert!(img[5 * BLOCK..6 * BLOCK].iter().all(|&x| x == 0));
        let dir = std::env::temp_dir().join(format!("hs-discard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        check(&FileDev::open(&p, false, Some(8 * BLOCK as u64)).unwrap());
        check(&UringDev::open(&p, false, None, 8).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_dev_crash_keeps_durable_and_some_cache() {
        let d = FaultDev::new(8 * BLOCK as u64);
        let one = vec![1u8; BLOCK];
        let two = vec![2u8; 2 * BLOCK];
        d.write(0, &one).unwrap();
        d.flush().unwrap();
        d.write(BLOCK as u64, &two).unwrap();
        let mut r = vec![0u8; BLOCK];
        d.read(2 * BLOCK as u64, &mut r).unwrap();
        assert_eq!(r[0], 2, "reads see the cache");
        assert_eq!(d.cached_blocks(), 2);
        let img = d.crash_image(|i| i == 1);
        assert_eq!(img[0], 1);
        assert_eq!(img[BLOCK], 0, "first cached block lost");
        assert_eq!(img[2 * BLOCK], 2, "second cached block kept: a torn write");
        d.fail_after(0);
        assert!(d.write(0, &one).is_err());
    }

    #[test]
    fn fault_dev_loses_and_misdirects_single_blocks() {
        let d = FaultDev::new(8 * BLOCK as u64);
        let data: Vec<u8> = (1..=3u8).flat_map(|v| vec![v; BLOCK]).collect();
        d.misstep(1, WriteFault::Lost);
        d.misstep(2, WriteFault::Misdirected(6));
        d.write(0, &data).unwrap();
        let mut r = vec![0u8; 3 * BLOCK];
        d.read(0, &mut r).unwrap();
        assert_eq!(
            (r[0], r[BLOCK], r[2 * BLOCK]),
            (1, 2, 3),
            "acknowledged as written"
        );
        d.flush().unwrap();
        let img = d.image();
        assert_eq!(img[0], 1);
        assert_eq!(img[BLOCK], 0, "lost");
        assert_eq!(img[2 * BLOCK], 0, "not where it was meant to go");
        assert_eq!(img[6 * BLOCK], 3, "but at block 6");
    }
}
