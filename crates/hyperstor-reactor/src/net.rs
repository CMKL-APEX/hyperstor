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

/// TCP keepalive on every connection (01-implementation-notes.md I184):
/// probes start after this long idle, every [`KEEPALIVE_INTERVAL`], and
/// [`KEEPALIVE_COUNT`] unanswered probes close it. A peer that vanished
/// without a FIN or RST (a host gone, a network cut) is noticed in about
/// 25 s instead of never.
pub const KEEPALIVE_IDLE: std::time::Duration = std::time::Duration::from_secs(10);
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
pub const KEEPALIVE_COUNT: u32 = 3;
/// Data sent and not acknowledged for this long closes the connection
/// (`TCP_USER_TIMEOUT`): keepalive covers idle connections, this one
/// those with writes stuck in flight.
pub const USER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn setsockopt(fd: i32, level: i32, name: i32, v: i32) -> io::Result<()> {
    // SAFETY: setsockopt reads `size_of::<i32>()` bytes from a local i32.
    let r = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&v as *const i32).cast(),
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn getsockopt(fd: i32, level: i32, name: i32) -> io::Result<i32> {
    let mut v: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into a local i32.
    let r = unsafe { libc::getsockopt(fd, level, name, (&mut v as *mut i32).cast(), &mut len) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(v)
}

/// Settings every connection gets, connected or accepted: no Nagle delay,
/// keepalive, and a bound on unacknowledged data.
fn tune(s: &StdStream) -> io::Result<()> {
    s.set_nodelay(true)?;
    let fd = s.as_raw_fd();
    setsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)?;
    setsockopt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPIDLE,
        KEEPALIVE_IDLE.as_secs() as i32,
    )?;
    setsockopt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        KEEPALIVE_INTERVAL.as_secs() as i32,
    )?;
    setsockopt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPCNT,
        KEEPALIVE_COUNT as i32,
    )?;
    setsockopt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_USER_TIMEOUT,
        USER_TIMEOUT.as_millis() as i32,
    )
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
        tune(&s)?;
        Ok((TcpStream { inner: s }, peer))
    }
}

/// A buffer the kernel receives into ([`TcpStream::read_exact_into`]).
///
/// # Safety
///
/// `recv_ptr` points to `recv_cap` bytes that may be written, owned by
/// the value and at the same address however the value is moved, until
/// it is dropped (heap memory, not the value's own fields).
pub unsafe trait RecvBuf: 'static {
    fn recv_ptr(&mut self) -> *mut u8;
    fn recv_cap(&self) -> usize;
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
    /// The local address the connection is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// The keepalive this socket has: on, idle seconds before probing, and
    /// the user timeout in milliseconds ([`KEEPALIVE_IDLE`], [`USER_TIMEOUT`]).
    pub fn keepalive(&self) -> io::Result<(bool, u32, u32)> {
        let fd = self.inner.as_raw_fd();
        Ok((
            getsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE)? != 0,
            getsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE)? as u32,
            getsockopt(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT)? as u32,
        ))
    }

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
        tune(&s)?;
        Ok(Self { inner: s })
    }

    /// Reads into `buf[..buf.len()]`; returns how many bytes arrived (0 at
    /// end of stream).
    pub async fn read(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.read_at(buf, 0).await
    }

    /// Reads into `buf[off..]`, in place.
    async fn read_at(&self, mut buf: Vec<u8>, off: usize) -> (io::Result<usize>, Vec<u8>) {
        let sqe = opcode::Recv::new(
            types::Fd(self.inner.as_raw_fd()),
            buf[off..].as_mut_ptr(),
            (buf.len() - off) as u32,
        )
        .build();
        let (r, res) = current().submit(sqe, Resource::Buf(buf)).await;
        let Resource::Buf(buf) = res else {
            unreachable!()
        };
        (check(r).map(|n| n as usize), buf)
    }

    /// Fills `buf` completely, or fails (with `UnexpectedEof` if the peer
    /// closes first). A short read continues in place: splitting off the
    /// rest and joining it back copied the unread part of the buffer at
    /// every short read.
    pub async fn read_exact(&self, mut buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        let mut got = 0;
        while got < buf.len() {
            let (r, b) = self.read_at(buf, got).await;
            buf = b;
            match r {
                Ok(0) => return (Err(io::ErrorKind::UnexpectedEof.into()), buf),
                Ok(n) => got += n,
                Err(e) => return (Err(e), buf),
            }
        }
        (Ok(()), buf)
    }

    /// Reads exactly `n` bytes into a new buffer, which isn't zeroed
    /// first: each piece is received into its spare capacity, and the
    /// buffer's length grows only by what the kernel wrote (HS5b, I327).
    /// On an error or an early end it holds what arrived.
    pub async fn read_exact_n(&self, n: usize) -> (io::Result<()>, Vec<u8>) {
        let mut buf: Vec<u8> = Vec::with_capacity(n);
        while buf.len() < n {
            let got = buf.len();
            let sqe = opcode::Recv::new(
                types::Fd(self.inner.as_raw_fd()),
                buf.spare_capacity_mut().as_mut_ptr().cast(),
                (n - got) as u32,
            )
            .build();
            // The heap memory stays where it is while the reactor holds
            // the vector, as for every receive.
            let (r, res) = current().submit(sqe, Resource::Buf(buf)).await;
            let Resource::Buf(b) = res else {
                unreachable!()
            };
            buf = b;
            match check(r) {
                Ok(0) => return (Err(io::ErrorKind::UnexpectedEof.into()), buf),
                // SAFETY: the kernel wrote `k <= n - got` bytes from
                // `got`, within the capacity of `n`.
                Ok(k) => unsafe { buf.set_len(got + k as usize) },
                Err(e) => return (Err(e), buf),
            }
        }
        (Ok(()), buf)
    }

    /// Reads exactly `n` bytes into the start of `buf`, which isn't
    /// zeroed first: an aligned buffer, so that a payload goes to the
    /// device as it arrived (I343). On an error or an early end, `buf`
    /// comes back with fewer than `n` bytes written, which only its type
    /// can tell apart from the rest.
    pub async fn read_exact_into<B: RecvBuf>(&self, mut buf: B, n: usize) -> (io::Result<()>, B) {
        assert!(n <= buf.recv_cap(), "{n} bytes into {}", buf.recv_cap());
        let base = buf.recv_ptr();
        let mut keep: std::rc::Rc<dyn std::any::Any> = std::rc::Rc::new(buf);
        let mut got = 0;
        let mut r = Ok(());
        while got < n {
            // SAFETY: `base` is valid for `recv_cap() >= n` bytes, and
            // stays where it is while `keep` holds the buffer (`RecvBuf`).
            let at = unsafe { base.add(got) };
            let sqe =
                opcode::Recv::new(types::Fd(self.inner.as_raw_fd()), at, (n - got) as u32).build();
            let (res, k) = current().submit(sqe, Resource::Keep(keep)).await;
            let Resource::Keep(k) = k else { unreachable!() };
            keep = k;
            match check(res) {
                Ok(0) => {
                    r = Err(io::ErrorKind::UnexpectedEof.into());
                    break;
                }
                Ok(k) => got += k as usize,
                Err(e) => {
                    r = Err(e);
                    break;
                }
            }
        }
        let buf = std::rc::Rc::downcast::<B>(keep)
            .ok()
            .and_then(|b| std::rc::Rc::try_unwrap(b).ok())
            .expect("a completed receive holds the buffer no more");
        (r, buf)
    }

    /// Writes all of `head`, then all of `tail`, in one `sendmsg` each time
    /// (two pieces of one buffer, so the tail is never copied after `head`).
    /// `tail` may be shared, with other connections sending it or other
    /// parts of it too.
    pub async fn write_all2<T: AsRef<[u8]> + 'static>(
        &self,
        head: Vec<u8>,
        tail: T,
    ) -> io::Result<()> {
        let bufs = std::rc::Rc::new((head, tail));
        let total = bufs.0.len() + bufs.1.as_ref().len();
        let mut off = 0;
        while off < total {
            let (head, tail) = (&bufs.0[..], bufs.1.as_ref());
            let piece = |b: &[u8]| libc::iovec {
                iov_base: b.as_ptr() as *mut libc::c_void,
                iov_len: b.len(),
            };
            let iov = if off < head.len() {
                [piece(&head[off..]), piece(tail)]
            } else {
                [piece(&tail[off - head.len()..]), piece(&[])]
            };
            let mut msg = Box::new((
                // SAFETY: an all-zero msghdr is valid: no address, no
                // control data, no iovecs (set below).
                unsafe { std::mem::zeroed::<libc::msghdr>() },
                iov,
            ));
            msg.0.msg_iov = msg.1.as_mut_ptr();
            msg.0.msg_iovlen = if iov[1].iov_len > 0 { 2 } else { 1 };
            let sqe = opcode::SendMsg::new(types::Fd(self.inner.as_raw_fd()), &msg.0)
                .flags(libc::MSG_NOSIGNAL as u32)
                .build();
            // The kernel reads the header, the iovecs and both buffers:
            // all kept alive by the operation until it completes.
            let keep = std::rc::Rc::new((bufs.clone(), msg));
            let (r, _) = current().submit(sqe, Resource::Keep(keep)).await;
            match check(r) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => off += n as usize,
                Err(e) => return Err(e),
            }
        }
        Ok(())
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

/// A UDP socket, for peers' pings (D185). Datagrams are small and few: a
/// receive waits on the ring for the socket to be readable and then takes
/// what arrived without blocking; a send that would block is refused at
/// once, as a lost datagram would be.
pub struct UdpSocket {
    inner: std::net::UdpSocket,
}

impl UdpSocket {
    /// Binds `addr`, waiting up to [`BIND_RETRY`] while it is in use: a
    /// pending receive holds the socket a moment after its process has
    /// exited, as a pending accept does ([`bind_retrying`]).
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let until = std::time::Instant::now() + BIND_RETRY;
        let inner = loop {
            match std::net::UdpSocket::bind(addr) {
                Err(e)
                    if e.kind() == io::ErrorKind::AddrInUse
                        && std::time::Instant::now() < until =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                r => break r?,
            }
        };
        inner.set_nonblocking(true)?;
        Ok(Self { inner })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Receives one datagram into `buf` (truncated to its length); returns
    /// its length and sender.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            match self.inner.recv_from(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    crate::fd::readable(self.inner.as_raw_fd()).await?;
                }
                r => return r,
            }
        }
    }

    /// Sends one datagram to `addr`.
    pub fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.inner.send_to(buf, addr)
    }
}

#[cfg(test)]
mod udp_tests {
    use super::*;
    use crate::{block_on, spawn};

    /// Datagrams go both ways between two sockets on one reactor, and a
    /// receive waits for one to arrive without holding the reactor.
    #[test]
    fn datagrams_go_both_ways() {
        block_on(async {
            let a = std::rc::Rc::new(UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
            let b = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
            let echo = {
                let a = a.clone();
                spawn(async move {
                    let mut buf = [0u8; 16];
                    let (n, from) = a.recv_from(&mut buf).await.unwrap();
                    a.send_to(&buf[..n], from).unwrap();
                    from
                })
            };
            crate::time::sleep(std::time::Duration::from_millis(5)).await;
            assert!(!echo.is_finished(), "nothing sent yet");
            b.send_to(b"ping", aa).unwrap();
            let mut buf = [0u8; 16];
            let (n, from) = b.recv_from(&mut buf).await.unwrap();
            assert_eq!((&buf[..n], from), (&b"ping"[..], aa));
            assert_eq!(echo.await, ba);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block_on, spawn};

    /// Both ends get keepalive and a user timeout (I184): a peer that
    /// vanishes without closing is noticed instead of held for ever.
    #[test]
    fn connections_keep_alive_both_ways() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let accepted = spawn(async move { l.accept().await.unwrap().0 });
            let c = TcpStream::connect(addr).await.unwrap();
            let a = accepted.await;
            let want = (
                true,
                KEEPALIVE_IDLE.as_secs() as u32,
                USER_TIMEOUT.as_millis() as u32,
            );
            assert_eq!(c.keepalive().unwrap(), want, "connected");
            assert_eq!(a.keepalive().unwrap(), want, "accepted");
        });
    }

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

    /// I327: a read of `n` bytes into a buffer that isn't zeroed fills it
    /// from pieces of any size, and an early end reports what arrived.
    #[test]
    fn read_exact_n_fills_from_pieces_and_reports_an_early_end() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let data: Vec<u8> = (0..3 << 20).map(|i| (i % 251) as u8).collect();
            let sent = data.clone();
            spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                let mut at = 0;
                for k in (1..).map(|i| (i * 7919) % 300_000 + 1) {
                    let end = (at + k).min(sent.len());
                    s.write_all(sent[at..end].to_vec()).await.0.unwrap();
                    at = end;
                    if at == sent.len() {
                        break;
                    }
                }
                s.write_all(vec![9; 10]).await.0.unwrap();
            });
            let s = TcpStream::connect(addr).await.unwrap();
            let (r, got) = s.read_exact_n(data.len()).await;
            r.unwrap();
            assert!(got == data);
            let (r, got) = s.read_exact_n(1000).await;
            assert_eq!(r.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
            assert_eq!(got, vec![9; 10]);
        });
    }

    /// A boxed array: its bytes stay put when the box moves.
    struct Boxed(Box<[u8; 1 << 16]>);

    // SAFETY: the array is on the heap, owned by the box.
    unsafe impl RecvBuf for Boxed {
        fn recv_ptr(&mut self) -> *mut u8 {
            self.0.as_mut_ptr()
        }
        fn recv_cap(&self) -> usize {
            self.0.len()
        }
    }

    /// I343: a receive into a buffer of the caller's type fills its start
    /// from pieces of any size, leaves the rest alone, and gives it back
    /// after an early end too.
    #[test]
    fn read_exact_into_fills_the_callers_buffer_from_pieces() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let data: Vec<u8> = (0..60_000).map(|i| (i % 251) as u8).collect();
            let sent = data.clone();
            spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                for piece in sent.chunks(7919) {
                    s.write_all(piece.to_vec()).await.0.unwrap();
                }
                s.write_all(vec![9; 10]).await.0.unwrap();
            });
            let s = TcpStream::connect(addr).await.unwrap();
            let (r, got) = s
                .read_exact_into(Boxed(Box::new([1; 1 << 16])), data.len())
                .await;
            r.unwrap();
            assert!(got.0[..data.len()] == data[..]);
            assert!(got.0[data.len()..].iter().all(|&b| b == 1));
            let (r, got) = s.read_exact_into(got, 1000).await;
            assert_eq!(r.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
            assert_eq!(got.0[..10], [9; 10]);
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
