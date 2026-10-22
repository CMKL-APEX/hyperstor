//! One ublk queue on the hyperstor reactor (D93): its descriptors mapped
//! from `/dev/ublkcN`, a buffer per tag, and a task per tag that fetches a
//! request, serves it through the attached volume, and commits the result
//! with the next fetch, all as `uring_cmd`s from this reactor's thread
//! (the driver takes a queue's commands from one task only).

use crate::sys::*;
use hyperstor_client::volume::Attached;
use hyperstor_reactor::passthrough::uring_cmd16;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

const PAGE: usize = 4096;

/// A page-aligned buffer the driver copies a request's data into or out
/// of, by its address.
struct IoBuf {
    ptr: *mut u8,
    len: usize,
}

impl IoBuf {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, PAGE).expect("buffer layout");
        // SAFETY: a non-zero size with a valid alignment.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory");
        Self { ptr, len }
    }
}

impl Drop for IoBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe {
            std::alloc::dealloc(
                self.ptr,
                std::alloc::Layout::from_size_align(self.len, PAGE).unwrap(),
            )
        };
    }
}

pub struct Queue {
    id: u16,
    cdev: OwnedFd,
    descs: *const IoDesc,
    map_len: usize,
    bufs: Vec<IoBuf>,
}

impl Queue {
    /// Opens queue `id` of device `dev`, waiting up to 5 s for its
    /// character device to appear.
    pub async fn open(dev: u32, id: u16, depth: u16, buf_bytes: usize) -> io::Result<Self> {
        let path = std::ffi::CString::new(format!("/dev/ublkc{dev}")).unwrap();
        let t = Instant::now();
        let fd = loop {
            // SAFETY: a NUL-terminated path; open returns a descriptor or -1.
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd >= 0 {
                break fd;
            }
            let e = io::Error::last_os_error();
            if t.elapsed() > Duration::from_secs(5) {
                return Err(e);
            }
            hyperstor_reactor::time::sleep(Duration::from_millis(20)).await;
        };
        // SAFETY: fd was just opened and is owned by nothing else.
        let cdev = unsafe { OwnedFd::from_raw_fd(fd) };
        let map_len = (depth as usize * size_of::<IoDesc>()).div_ceil(PAGE) * PAGE;
        let area = (MAX_QUEUE_DEPTH * size_of::<IoDesc>()).div_ceil(PAGE) * PAGE;
        let off = CMD_BUF_OFFSET as usize + id as usize * area;
        // SAFETY: maps the driver's read-only descriptor area of this queue;
        // checked for failure below, unmapped in Drop.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                cdev.as_raw_fd(),
                off as libc::off_t,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            id,
            cdev,
            descs: p as *const IoDesc,
            map_len,
            bufs: (0..depth).map(|_| IoBuf::new(buf_bytes)).collect(),
        })
    }

    pub fn depth(&self) -> u16 {
        self.bufs.len() as u16
    }

    fn desc(&self, tag: u16) -> IoDesc {
        assert!((tag as usize) < self.bufs.len());
        // SAFETY: tag is within the mapped area; the driver wrote this
        // descriptor before completing the fetch we are answering.
        unsafe { std::ptr::read_volatile(self.descs.add(tag as usize)) }
    }

    /// Serves tag `tag` until the device stops: fetch, serve, commit and
    /// fetch again.
    pub async fn serve_tag(self: Rc<Self>, tag: u16, vol: Rc<Attached>) {
        let mut op = IO_FETCH_REQ;
        let mut result = -1;
        loop {
            let cmd = IoCmd {
                q_id: self.id,
                tag,
                result,
                addr: self.bufs[tag as usize].ptr as u64,
            };
            // SAFETY: the character device and this tag's buffer live as
            // long as `self`, which this task holds until the command
            // completes; the driver writes into the buffer only for this
            // tag's requests, which this task alone serves.
            let r = unsafe { uring_cmd16(self.cdev.as_raw_fd(), op, cmd.bytes()) }.await;
            if r != IO_RES_OK {
                // Aborted (the device stopped), or refused.
                return;
            }
            result = self.serve(tag, &vol).await;
            op = IO_COMMIT_AND_FETCH_REQ;
        }
    }

    /// One request: the result is the bytes done, or a negative errno.
    async fn serve(&self, tag: u16, vol: &Attached) -> i32 {
        let d = self.desc(tag);
        let (off, len) = (d.start_sector << 9, (d.nr_sectors as usize) << 9);
        let buf = &self.bufs[tag as usize];
        if len > buf.len {
            return -libc::EINVAL;
        }
        match (d.op_flags & 0xff) as u8 {
            IO_OP_READ => match vol.read(off, len as u64).await {
                Ok(data) => {
                    // SAFETY: len <= buf.len, and no command for this tag
                    // is in flight while it is served.
                    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf.ptr, len) };
                    len as i32
                }
                Err(_) => -libc::EIO,
            },
            IO_OP_WRITE => {
                // SAFETY: as above; the driver copied the data in before
                // completing the fetch.
                let data = unsafe { std::slice::from_raw_parts(buf.ptr, len) }.to_vec();
                match vol.write(off, data).await {
                    Ok(()) => len as i32,
                    Err(_) => -libc::EIO,
                }
            }
            // Writes are durable once answered: nothing to flush.
            IO_OP_FLUSH => 0,
            _ => -libc::EOPNOTSUPP,
        }
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `open`, no longer read.
        unsafe { libc::munmap(self.descs as *mut libc::c_void, self.map_len) };
    }
}
