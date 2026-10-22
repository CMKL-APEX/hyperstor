//! io_uring passthrough commands (`IORING_OP_URING_CMD`): drivers that take
//! their commands through io_uring, as ublk does (`hyperstor-ublk`, D93)
//! and NVMe passthrough will (D3). Only the 16-byte command of a standard
//! entry; a driver that needs 80 bytes keeps a ring of its own.

use crate::{Resource, current};
use io_uring::{opcode, types};
use std::os::fd::RawFd;

/// Sends command `cmd_op` with the 16 bytes `cmd` to the driver behind
/// `fd`, from this reactor's thread; resolves to the completion's result.
///
/// # Safety
///
/// `fd` must stay open until the command completes, and any memory the
/// command names (a buffer address in `cmd`) must stay valid until then,
/// even if this future is dropped first: the kernel may still use it.
pub async unsafe fn uring_cmd16(fd: RawFd, cmd_op: u32, cmd: [u8; 16]) -> i32 {
    let sqe = opcode::UringCmd16::new(types::Fd(fd), cmd_op)
        .cmd(cmd)
        .build();
    let (r, _) = current().submit(sqe, Resource::None).await;
    r
}
