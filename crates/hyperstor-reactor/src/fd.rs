//! Waiting on and reading descriptors the reactor doesn't wrap: eventfds
//! (vhost-user's kick and call, 11-frontends.md §11.1) and Unix sockets whose
//! messages another library reads once they are ready.

use crate::{Resource, check, current};
use io_uring::{opcode, types};
use std::io;
use std::os::fd::RawFd;

/// Resolves once `fd` has something to read (or is closed or broken).
/// `fd` must stay open until then.
pub async fn readable(fd: RawFd) -> io::Result<()> {
    let sqe = opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as u32).build();
    let (r, _) = current().submit(sqe, Resource::None).await;
    check(r).map(|_| ())
}

/// Reads into `buf[..buf.len()]` from the current position (an eventfd's
/// 8-byte counter, say); returns how many bytes arrived.
pub async fn read(fd: RawFd, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
    let sqe = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
        .offset(u64::MAX)
        .build();
    let (r, res) = current().submit(sqe, Resource::Buf(buf)).await;
    let Resource::Buf(buf) = res else {
        unreachable!()
    };
    (check(r).map(|n| n as usize), buf)
}

/// Writes `buf` at the current position; returns how many bytes went.
pub async fn write(fd: RawFd, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
    let sqe = opcode::Write::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32)
        .offset(u64::MAX)
        .build();
    let (r, res) = current().submit(sqe, Resource::Buf(buf)).await;
    let Resource::Buf(buf) = res else {
        unreachable!()
    };
    (check(r).map(|n| n as usize), buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[test]
    fn an_eventfd_wakes_a_waiting_task() {
        // SAFETY: eventfd returns a new descriptor (checked), owned below.
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(efd >= 0);
        // SAFETY: `efd` is a fresh descriptor nothing else owns.
        let efd = unsafe { OwnedFd::from_raw_fd(efd) };
        let fd = efd.as_raw_fd();
        crate::block_on(async move {
            let waiter = crate::spawn(async move {
                readable(fd).await.unwrap();
                let (r, b) = read(fd, vec![0; 8]).await;
                assert_eq!(r.unwrap(), 8);
                u64::from_ne_bytes(b.try_into().unwrap())
            });
            crate::time::sleep(std::time::Duration::from_millis(5)).await;
            assert!(!waiter.is_finished());
            let (r, _) = write(fd, 3u64.to_ne_bytes().to_vec()).await;
            assert_eq!(r.unwrap(), 8);
            assert_eq!(waiter.await, 3);
        });
        drop(efd);
    }
}
