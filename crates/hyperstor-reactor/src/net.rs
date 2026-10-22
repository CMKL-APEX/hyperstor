//! TCP on the reactor.

use crate::{Resource, check, current};
use io_uring::{opcode, types};
use std::io;
use std::net::{SocketAddr, TcpListener as StdListener, TcpStream as StdStream};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

/// How long [`bind_retrying`] waits for an address in use to be let go.
pub const BIND_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Binds `addr`, waiting up to [`BIND_RETRY`] while it is in use. A
/// reactor's listener stays open a moment after its process has exited:
/// the kernel tears an io_uring instance down in the background, and a
/// pending accept holds the socket until then. A service restarted at
/// once (a test, `systemctl restart`) would otherwise find its own old
/// address taken.
pub fn bind_retrying(addr: SocketAddr) -> io::Result<StdListener> {
    let until = std::time::Instant::now() + BIND_RETRY;
    loop {
        match StdListener::bind(addr) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

/// A listening socket.
pub struct TcpListener {
    inner: StdListener,
}

impl TcpListener {
    /// Binds `addr` ([`bind_retrying`]: waits a moment if it is in use).
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let inner = bind_retrying(addr)?;
        Ok(Self { inner })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Wraps a listener bound elsewhere (for example before the reactor
    /// thread started, so its address could go into the map first).
    pub fn from_std(inner: StdListener) -> Self {
        Self { inner }
    }

    /// Accepts a connection.
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let sqe = opcode::Accept::new(
            types::Fd(self.inner.as_raw_fd()),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_CLOEXEC)
        .build();
        let (r, _) = current().submit(sqe, Resource::None).await;
        let fd = check(r)?;
        // SAFETY: accept returned a new descriptor that we now own.
        let s = unsafe { StdStream::from_raw_fd(fd) };
        let peer = s.peer_addr()?;
        s.set_nodelay(true)?;
        Ok((TcpStream { inner: s }, peer))
    }
}

/// A connected TCP socket. Reads and writes take ownership of their buffer
/// and give it back with the result.
pub struct TcpStream {
    inner: StdStream,
}

fn sockaddr(addr: SocketAddr) -> Box<(libc::sockaddr_storage, libc::socklen_t)> {
    // SAFETY: sockaddr_storage is plain data; all-zero is a valid value.
    let mut st: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: sockaddr_in fits in sockaddr_storage.
            unsafe { std::ptr::write(&mut st as *mut _ as *mut libc::sockaddr_in, sin) };
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: a.scope_id(),
            };
            // SAFETY: sockaddr_in6 fits in sockaddr_storage.
            unsafe { std::ptr::write(&mut st as *mut _ as *mut libc::sockaddr_in6, sin6) };
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    Box::new((st, len as libc::socklen_t))
}

impl TcpStream {
    /// Connects to `addr`.
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        // SAFETY: socket returns a new descriptor or -1.
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is new and owned here; the stream closes it on error.
        let s = unsafe { StdStream::from_raw_fd(fd) };
        let sa = sockaddr(addr);
        let sqe = opcode::Connect::new(
            types::Fd(s.as_raw_fd()),
            &sa.0 as *const _ as *const libc::sockaddr,
            sa.1,
        )
        .build();
        let (r, _) = current().submit(sqe, Resource::Addr(sa)).await;
        check(r)?;
        s.set_nodelay(true)?;
        Ok(Self { inner: s })
    }

    /// Reads into `buf[..buf.len()]`; returns how many bytes arrived (0 at
    /// end of stream).
    pub async fn read(&self, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        let sqe = opcode::Recv::new(
            types::Fd(self.inner.as_raw_fd()),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
        .build();
        let (r, res) = current().submit(sqe, Resource::Buf(buf)).await;
        let Resource::Buf(buf) = res else {
            unreachable!()
        };
        (check(r).map(|n| n as usize), buf)
    }

    /// Fills `buf` completely, or fails (with `UnexpectedEof` if the peer
    /// closes first).
    pub async fn read_exact(&self, mut buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        let want = buf.len();
        let mut got = 0;
        while got < want {
            let rest = buf.split_off(got);
            let (r, rest) = self.read(rest).await;
            buf.extend_from_slice(&rest[..r.as_ref().map_or(0, |n| *n)]);
            match r {
                Ok(0) => return (Err(io::ErrorKind::UnexpectedEof.into()), buf),
                Ok(n) => got += n,
                Err(e) => return (Err(e), buf),
            }
            buf.resize(want.max(buf.len()), 0);
            buf.truncate(want);
        }
        (Ok(()), buf)
    }

    /// Writes all of `buf`.
    pub async fn write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        let mut off = 0;
        let mut buf = buf;
        while off < buf.len() {
            let sqe = opcode::Send::new(
                types::Fd(self.inner.as_raw_fd()),
                buf[off..].as_ptr(),
                (buf.len() - off) as u32,
            )
            .flags(libc::MSG_NOSIGNAL)
            .build();
            let (r, res) = current().submit(sqe, Resource::Buf(buf)).await;
            let Resource::Buf(b) = res else {
                unreachable!()
            };
            buf = b;
            match check(r) {
                Ok(0) => return (Err(io::ErrorKind::WriteZero.into()), buf),
                Ok(n) => off += n as usize,
                Err(e) => return (Err(e), buf),
            }
        }
        (Ok(()), buf)
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    /// Shuts down both directions; pending reads on other tasks end.
    pub fn shutdown(&self) -> io::Result<()> {
        self.inner.shutdown(std::net::Shutdown::Both)
    }

    /// The descriptor, for handing a connection to another reactor.
    pub fn into_std(self) -> StdStream {
        self.inner
    }

    pub fn from_std(s: StdStream) -> Self {
        Self { inner: s }
    }

    /// Duplicates the socket so one task can read while another writes.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
        })
    }
}

impl IntoRawFd for TcpStream {
    fn into_raw_fd(self) -> std::os::fd::RawFd {
        self.inner.into_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block_on, spawn};

    #[test]
    fn echo_many_clients() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            spawn(async move {
                loop {
                    let (s, _) = l.accept().await.unwrap();
                    spawn(async move {
                        loop {
                            let (r, buf) = s.read(vec![0; 4096]).await;
                            let n = r.unwrap();
                            if n == 0 {
                                break;
                            }
                            let (w, _) = s.write_all(buf[..n].to_vec()).await;
                            w.unwrap();
                        }
                    });
                }
            });
            let mut hs = Vec::new();
            for c in 0..20u8 {
                hs.push(spawn(async move {
                    let s = TcpStream::connect(addr).await.unwrap();
                    for i in 0..50u8 {
                        let msg = vec![c ^ i; 1000];
                        let (w, _) = s.write_all(msg.clone()).await;
                        w.unwrap();
                        let (r, back) = s.read_exact(vec![0; 1000]).await;
                        r.unwrap();
                        assert_eq!(back, msg);
                    }
                }));
            }
            for h in hs {
                h.await;
            }
        });
    }

    #[test]
    fn connect_refused_and_dropped_reads() {
        block_on(async {
            let l = StdListener::bind("127.0.0.1:0").unwrap();
            let addr = l.local_addr().unwrap();
            drop(l);
            assert!(TcpStream::connect(addr).await.is_err());

            // A read dropped while pending must not break the reactor.
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let c = TcpStream::connect(addr).await.unwrap();
            let (s, _) = l.accept().await.unwrap();
            let r = crate::time::timeout(std::time::Duration::from_millis(10), s.read(vec![0; 64]))
                .await;
            assert!(r.is_err(), "nothing was sent");
            let (w, _) = c.write_all(b"after".to_vec()).await;
            w.unwrap();
            let (r, buf) = s.read(vec![0; 64]).await;
            assert_eq!(&buf[..r.unwrap()], b"after");
        });
    }
}

#[cfg(test)]
mod bind_tests {
    /// An address let go a moment later is bound once it is free.
    #[test]
    fn a_bind_waits_for_an_address_let_go() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });
        let start = std::time::Instant::now();
        super::bind_retrying(addr).unwrap();
        assert!(start.elapsed() >= std::time::Duration::from_millis(150));
        t.join().unwrap();
    }
}
