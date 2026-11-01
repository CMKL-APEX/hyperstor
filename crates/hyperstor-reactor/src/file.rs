//! Writes to and reads from a file or block device at offsets, on the
//! reactor's ring: the engine's commits (I199) and reads (D190), which used
//! to wait in the device's own calls and so held the reactor, pings
//! included, for as long as the device took.

use crate::{Resource, check, current};
use io_uring::{opcode, types};
use std::cell::RefCell;
use std::io;
use std::ops::DerefMut;
use std::os::fd::RawFd;
use std::rc::Rc;

/// Writes every buffer at its byte offset of `fd`, all submitted at once,
/// then, once each has completed in full, makes them durable
/// (`fdatasync`). The first error wins; a short write is one.
///
/// The buffers are the reactor's until the kernel is done with them, so
/// dropping this future (a timeout, say) frees nothing the kernel still
/// uses.
///
/// # Safety
///
/// `fd` must stay open until every write and the flush have completed,
/// even if this future is dropped first.
pub async unsafe fn write_then_sync<B: AsRef<[u8]> + 'static>(
    fd: RawFd,
    writes: Vec<(u64, B)>,
) -> io::Result<()> {
    let r = current();
    let bufs = Rc::new(writes);
    let keep: Rc<dyn std::any::Any> = bufs.clone();
    let ops: Vec<_> = bufs
        .iter()
        .map(|(off, b)| {
            let b = b.as_ref();
            let sqe = opcode::Write::new(types::Fd(fd), b.as_ptr(), b.len() as u32)
                .offset(*off)
                .build();
            (r.submit(sqe, Resource::Keep(keep.clone())), b.len())
        })
        .collect();
    drop((bufs, keep));
    let mut err = None;
    for (op, want) in ops {
        let (n, _) = op.await;
        match check(n) {
            Ok(n) if n as usize == want => {}
            Ok(n) => {
                err.get_or_insert(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("short write: {n} of {want} bytes"),
                ));
            }
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    let sqe = opcode::Fsync::new(types::Fd(fd))
        .flags(types::FsyncFlags::DATASYNC)
        .build();
    let (n, _) = r.submit(sqe, Resource::None).await;
    check(n).map(|_| ())
}

/// Reads into `buf` at each `(byte offset of fd, offset in buf, length)`,
/// all submitted at once, and gives `buf` back once each read has
/// completed in full. The first error wins; a short read is one.
///
/// As for [`write_then_sync`], the buffer is the reactor's until the kernel
/// is done with it, so dropping this future frees nothing the kernel still
/// writes to.
///
/// # Safety
///
/// `fd` must stay open until every read has completed, even if this future
/// is dropped first.
pub async unsafe fn read_into<B: DerefMut<Target = [u8]> + 'static>(
    fd: RawFd,
    buf: B,
    ios: &[(u64, usize, usize)],
) -> io::Result<B> {
    let r = current();
    let cell = Rc::new(RefCell::new(buf));
    let base = {
        let mut b = cell.borrow_mut();
        for &(_, at, len) in ios {
            assert!(at + len <= b.len(), "a read past the buffer");
        }
        b.as_mut_ptr()
    };
    let keep: Rc<dyn std::any::Any> = cell.clone();
    let ops: Vec<_> = ios
        .iter()
        .map(|&(off, at, len)| {
            // SAFETY: `at + len` is within the buffer (asserted above), whose
            // memory stays where it is inside `cell`, which the ring keeps
            // until this read completes; nothing else touches the buffer
            // until every read has completed.
            let ptr = unsafe { base.add(at) };
            let sqe = opcode::Read::new(types::Fd(fd), ptr, len as u32)
                .offset(off)
                .build();
            (r.submit(sqe, Resource::Keep(keep.clone())), len)
        })
        .collect();
    drop(keep);
    let mut err = None;
    for (op, want) in ops {
        let (n, res) = op.await;
        drop(res);
        match check(n) {
            Ok(n) if n as usize == want => {}
            Ok(n) => {
                err.get_or_insert(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("short read: {n} of {want} bytes"),
                ));
            }
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    let Ok(cell) = Rc::try_unwrap(cell) else {
        unreachable!("every read has completed and given its hold back");
    };
    Ok(cell.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;

    #[test]
    fn writes_land_at_their_offsets() {
        let dir = std::env::temp_dir().join(format!("hs-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(1 << 16).unwrap();
        let fd = f.as_raw_fd();
        crate::block_on(async move {
            let w = vec![(4096u64, vec![1u8; 4096]), (12288, vec![2u8; 8192])];
            // SAFETY: `f` stays open until after block_on returns.
            unsafe { write_then_sync(fd, w) }.await.unwrap();
        });
        let mut b = vec![0u8; 1 << 16];
        f.read_exact_at(&mut b, 0).unwrap();
        assert!(b[..4096].iter().all(|&x| x == 0));
        assert!(b[4096..8192].iter().all(|&x| x == 1));
        assert!(b[8192..12288].iter().all(|&x| x == 0));
        assert!(b[12288..20480].iter().all(|&x| x == 2));
        drop(f);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_land_in_the_buffer() {
        let dir = std::env::temp_dir().join(format!("hs-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev");
        let f = std::fs::File::create_new(&path).unwrap();
        let mut b = vec![0u8; 1 << 16];
        b[4096..8192].fill(1);
        b[20480..28672].fill(2);
        f.write_all_at(&b, 0).unwrap();
        let fd = f.as_raw_fd();
        let (got, short) = crate::block_on(async move {
            // SAFETY: `f` stays open until after block_on returns.
            let got = unsafe {
                read_into(
                    fd,
                    vec![9u8; 16384],
                    &[(4096, 0, 4096), (20480, 8192, 8192)],
                )
            }
            .await
            .unwrap();
            // SAFETY: as above.
            let short = unsafe { read_into(fd, vec![0u8; 8192], &[(61440, 0, 8192)]) }.await;
            (got, short)
        });
        assert!(got[..4096].iter().all(|&x| x == 1));
        assert!(got[4096..8192].iter().all(|&x| x == 9), "left alone");
        assert!(got[8192..].iter().all(|&x| x == 2));
        assert_eq!(short.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        drop(f);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn errors_are_returned() {
        crate::block_on(async {
            // SAFETY: -1 is no descriptor; the kernel refuses it at once.
            let r = unsafe { write_then_sync(-1, vec![(0u64, vec![0u8; 4096])]) }.await;
            assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EBADF));
            // SAFETY: as above.
            let r = unsafe { read_into(-1, vec![0u8; 4096], &[(0, 0, 4096)]) }.await;
            assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EBADF));
        });
    }
}
