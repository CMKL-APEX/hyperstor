//! ublk control commands (add, set parameters, start, stop, delete), on
//! `/dev/ublk-control` through a ring of 128-byte entries of its own: their
//! command takes 32 bytes, more than a standard entry carries (D93). Each
//! call blocks until the driver answers; `start` blocks until every queue
//! has fetched its requests.

use crate::sys::*;
use io_uring::{IoUring, cqueue, opcode, squeue, types};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct Control {
    fd: OwnedFd,
    ring: IoUring<squeue::Entry128, cqueue::Entry>,
}

impl Control {
    pub fn open() -> io::Result<Self> {
        // SAFETY: a NUL-terminated path; open returns a new descriptor or -1.
        let fd = unsafe {
            libc::open(
                c"/dev/ublk-control".as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: fd was just opened and is owned by nothing else.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            ring: IoUring::<squeue::Entry128, cqueue::Entry>::builder().build(4)?,
        })
    }

    /// Sends one command and waits for its result. `c.addr`, if set, must
    /// point at `c.len` bytes that stay valid until this returns, which it
    /// does only after the completion.
    fn cmd(&mut self, op: u32, c: CtrlCmd) -> io::Result<i32> {
        let sqe = opcode::UringCmd80::new(types::Fd(self.fd.as_raw_fd()), op)
            .cmd(c.bytes())
            .build()
            .user_data(1);
        // SAFETY: the entry names only memory the caller keeps valid until
        // the completion, which is waited for below.
        unsafe {
            self.ring
                .submission()
                .push(&sqe)
                .map_err(|_| io::Error::other("control ring full"))?;
        }
        self.ring.submit_and_wait(1)?;
        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| io::Error::other("no completion"))?;
        match cqe.result() {
            r if r < 0 => Err(io::Error::from_raw_os_error(-r)),
            r => Ok(r),
        }
    }

    /// Adds a device of `queues` queues of `depth` requests of at most
    /// `buf_bytes` each; the driver picks its number.
    pub fn add(&mut self, queues: u16, depth: u16, buf_bytes: u32) -> io::Result<DevInfo> {
        let mut info = DevInfo {
            nr_hw_queues: queues,
            queue_depth: depth,
            max_io_buf_bytes: buf_bytes,
            dev_id: u32::MAX,
            flags: F_CMD_IOCTL_ENCODE,
            ..Default::default()
        };
        self.cmd(
            CMD_ADD_DEV,
            CtrlCmd {
                dev_id: u32::MAX,
                queue_id: u16::MAX,
                len: size_of::<DevInfo>() as u16,
                addr: &mut info as *mut DevInfo as u64,
                ..Default::default()
            },
        )?;
        Ok(info)
    }

    pub fn set_params(&mut self, dev: u32, p: &Params) -> io::Result<()> {
        self.cmd(
            CMD_SET_PARAMS,
            CtrlCmd {
                dev_id: dev,
                queue_id: u16::MAX,
                len: size_of::<Params>() as u16,
                addr: p as *const Params as u64,
                ..Default::default()
            },
        )
        .map(|_| ())
    }

    /// Starts serving: the block device appears. Returns once every queue
    /// has fetched its requests.
    pub fn start(&mut self, dev: u32) -> io::Result<()> {
        self.cmd(
            CMD_START_DEV,
            CtrlCmd {
                dev_id: dev,
                queue_id: u16::MAX,
                data: std::process::id() as u64,
                ..Default::default()
            },
        )
        .map(|_| ())
    }

    /// Stops the device: the block device goes, and the queues' pending
    /// fetches complete.
    pub fn stop(&mut self, dev: u32) -> io::Result<()> {
        self.simple(CMD_STOP_DEV, dev)
    }

    /// Deletes the device. Waits until its character device is closed.
    pub fn delete(&mut self, dev: u32) -> io::Result<()> {
        self.simple(CMD_DEL_DEV, dev)
    }

    pub fn info(&mut self, dev: u32) -> io::Result<DevInfo> {
        let mut info = DevInfo::default();
        self.cmd(
            CMD_GET_DEV_INFO,
            CtrlCmd {
                dev_id: dev,
                queue_id: u16::MAX,
                len: size_of::<DevInfo>() as u16,
                addr: &mut info as *mut DevInfo as u64,
                ..Default::default()
            },
        )?;
        Ok(info)
    }

    fn simple(&mut self, op: u32, dev: u32) -> io::Result<()> {
        self.cmd(
            op,
            CtrlCmd {
                dev_id: dev,
                queue_id: u16::MAX,
                ..Default::default()
            },
        )
        .map(|_| ())
    }
}

/// The parameters of a volume of `bytes` served in requests of at most
/// `buf_bytes`: 4 KiB blocks, and no volatile cache, since a write is
/// answered once every copy has it (so the kernel sends no flushes).
pub fn params(bytes: u64, buf_bytes: u32) -> Params {
    Params {
        len: size_of::<Params>() as u32,
        types: PARAM_TYPE_BASIC,
        basic: ParamBasic {
            attrs: 0,
            logical_bs_shift: 12,
            physical_bs_shift: 12,
            io_opt_shift: 12,
            io_min_shift: 12,
            max_sectors: buf_bytes >> 9,
            chunk_sectors: 0,
            dev_sectors: bytes >> 9,
            virt_boundary_mask: 0,
        },
        ..Default::default()
    }
}
