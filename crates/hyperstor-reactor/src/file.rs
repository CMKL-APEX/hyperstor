//! Writes to a file or block device at offsets, on the reactor's ring: the
//! engine's commits (I199), which used to wait in the device's own calls
//! and so held the reactor, pings included, for as long as the device took.

use crate::{Resource, check, current};
use io_uring::{opcode, types};
use std::io;
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
    fn errors_are_returned() {
        crate::block_on(async {
            // SAFETY: -1 is no descriptor; the kernel refuses it at once.
            let r = unsafe { write_then_sync(-1, vec![(0u64, vec![0u8; 4096])]) }.await;
            assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EBADF));
        });
    }
}
