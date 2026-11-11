//! Limits on the gateway's connections, as mgmt's (`hyperstor-mgmt`'s
//! `conn.rs`, 01-implementation-notes.md I355): how long a request's
//! head may take, how long a kept connection may sit idle, and how many
//! connections are open at once.
//!
//! - **Head:** a new connection has [`Limits::head`] (10 s) to finish its
//!   TLS handshake and its first request's headers; a kept connection, as
//!   long from the first byte of each next request.
//! - **Idle:** a kept connection with no request for [`Limits::idle`]
//!   (2 min) is closed, as is one whose answer the client stops reading
//!   for as long. SDKs keep idle connections for less (botocore and the
//!   AWS CLI keep theirs in a pool and retry on one the server closed).
//! - **Cap:** at most [`Limits::max`] connections (4096) are open; past
//!   that, the gateway stops accepting until one closes, a backstop for
//!   file handles.
//!
//! A request's body has its own limit, in the gateway: [`BODY_IDLE`]
//! between its pieces. hyper's own `header_read_timeout` isn't used: it
//! also runs while a kept connection waits for its next request, so it
//! would close kept connections after the head limit.
//!
//! The same code as mgmt's, but for the limits; sharing it is for when a
//! third server needs it.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// How long a connection may take over each part.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The TLS handshake and first request's head, and each later
    /// request's head from its first byte.
    pub head: Duration,
    /// A kept connection with no request in flight.
    pub idle: Duration,
    /// Connections open at once.
    pub max: usize,
}

/// The most connections open at once, when file handles allow.
pub const MAX_CONNECTIONS: usize = 4096;

/// The longest a request's body may go without a byte arriving.
pub const BODY_IDLE: Duration = Duration::from_secs(60);

impl Limits {
    /// The limits the gateway serves with. Raises the soft limit on open
    /// files to the hard one first (systemd starts services with 1024),
    /// and keeps the cap to half of it, the rest for the cluster's
    /// connections.
    pub fn for_this_process() -> Limits {
        let files = raise_open_files();
        Limits {
            head: Duration::from_secs(10),
            idle: Duration::from_secs(120),
            max: MAX_CONNECTIONS.min((files / 2) as usize).max(16),
        }
    }
}

/// Raises the soft `RLIMIT_NOFILE` to the hard limit; the soft limit
/// now in force.
fn raise_open_files() -> u64 {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit into `lim`, which outlives the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return 1024;
    }
    if lim.rlim_cur < lim.rlim_max {
        let want = libc::rlimit {
            rlim_cur: lim.rlim_max,
            rlim_max: lim.rlim_max,
        };
        // SAFETY: setrlimit reads one rlimit from `want`, which outlives the
        // call; raising the soft limit up to the hard one needs no privilege.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &want) } == 0 {
            return want.rlim_cur;
        }
    }
    lim.rlim_cur
}

/// What one connection is doing, for its watchdog: times are
/// milliseconds since it was accepted, plus one (0: none).
pub struct Watch {
    start: Instant,
    /// The last byte read or written, or request finished.
    last: AtomicU64,
    /// When the head being read began: at accept for the first, at the
    /// first byte read with no request in flight for the others.
    head: AtomicU64,
    /// Requests being served.
    busy: AtomicUsize,
}

impl Watch {
    /// A connection just accepted: its first head's time starts now.
    pub fn new() -> Arc<Watch> {
        Arc::new(Watch {
            start: Instant::now(),
            last: AtomicU64::new(1),
            head: AtomicU64::new(1),
            busy: AtomicUsize::new(0),
        })
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }

    /// A request is being served until the guard drops: its head is in,
    /// and the idle limit doesn't run.
    pub fn busy(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.head.store(0, Ordering::SeqCst);
        Busy(self.clone())
    }

    fn read(&self) {
        let now = self.now();
        self.last.store(now, Ordering::SeqCst);
        if self.busy.load(Ordering::SeqCst) == 0 {
            let _ = self
                .head
                .compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    fn wrote(&self) {
        self.last.store(self.now(), Ordering::SeqCst);
    }

    /// Which limit the connection is past now, if any.
    fn over(&self, l: &Limits) -> Option<&'static str> {
        if self.busy.load(Ordering::SeqCst) > 0 {
            return None;
        }
        let now = self.now();
        let head = self.head.load(Ordering::SeqCst);
        if head != 0 && now.saturating_sub(head) > l.head.as_millis() as u64 {
            return Some("head");
        }
        let last = self.last.load(Ordering::SeqCst);
        (now.saturating_sub(last) > l.idle.as_millis() as u64).then_some("idle")
    }

    /// Returns once the connection is past a limit: its head too slow, or
    /// idle too long, with no request in flight.
    pub async fn expired(&self, l: &Limits) -> &'static str {
        let tick =
            (l.head.min(l.idle) / 10).clamp(Duration::from_millis(10), Duration::from_secs(1));
        loop {
            tokio::time::sleep(tick).await;
            if let Some(why) = self.over(l) {
                return why;
            }
        }
    }
}

/// A request in flight; see [`Watch::busy`].
pub struct Busy(Arc<Watch>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.wrote();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A connection's stream, telling its [`Watch`] of each read and write.
pub struct Watched<T> {
    inner: T,
    watch: Arc<Watch>,
}

impl<T> Watched<T> {
    pub fn new(inner: T, watch: Arc<Watch>) -> Self {
        Watched { inner, watch }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Watched<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.watch.read();
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Watched<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.watch.wrote();
        }
        r
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.watch.wrote();
        }
        r
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const L: Limits = Limits {
        head: Duration::from_millis(200),
        idle: Duration::from_millis(600),
        max: 4,
    };

    /// Serves one connection over an in-memory pipe as the gateway does: each
    /// request sleeps `work`, then answers. Returns the client's end and
    /// the time and reason the server closed the connection.
    fn serve(
        work: Duration,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<(Duration, &'static str)>,
    ) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let w = Watch::new();
        let task = tokio::spawn(async move {
            let t = Instant::now();
            let ww = w.clone();
            let svc = hyper::service::service_fn(move |_req| {
                let busy = ww.busy();
                async move {
                    tokio::time::sleep(work).await;
                    drop(busy);
                    Ok::<_, std::convert::Infallible>(http::Response::new(
                        http_body_util::Full::new(hyper::body::Bytes::from_static(b"ok")),
                    ))
                }
            });
            let conn = hyper::server::conn::http1::Builder::new().serve_connection(
                hyper_util::rt::TokioIo::new(Watched::new(server, w.clone())),
                svc,
            );
            let why = tokio::select! {
                _ = conn => "closed",
                why = w.expired(&L) => why,
            };
            (t.elapsed(), why)
        });
        (client, task)
    }

    const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";

    /// Reads one answer of `ok` and returns it.
    async fn answer(c: &mut tokio::io::DuplexStream) -> String {
        let mut out = Vec::new();
        let mut b = [0u8; 256];
        while !out.ends_with(b"ok") {
            let n = c.read(&mut b).await.unwrap();
            assert!(n > 0, "closed: {}", String::from_utf8_lossy(&out));
            out.extend_from_slice(&b[..n]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn a_connection_that_sends_nothing_is_closed_at_the_head_limit() {
        let (_c, task) = serve(Duration::ZERO);
        let (took, why) = task.await.unwrap();
        assert_eq!(why, "head");
        assert!(took >= L.head && took < L.idle, "{took:?}");
    }

    #[tokio::test]
    async fn headers_trickled_in_are_cut_off_at_the_head_limit() {
        let (mut c, task) = serve(Duration::ZERO);
        // A request answered, so the connection is kept…
        c.write_all(GET).await.unwrap();
        answer(&mut c).await;
        // …then the next one's head a byte at a time, never finished.
        let t = Instant::now();
        for b in b"GET / HTTP/1.1\r\nHost: x\r\n".iter() {
            if c.write_all(&[*b]).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_, why) = task.await.unwrap();
        assert_eq!(why, "head");
        assert!(t.elapsed() < L.idle * 2, "{:?}", t.elapsed());
    }

    #[tokio::test]
    async fn a_kept_connection_lasts_until_idle_and_serves_meanwhile() {
        let (mut c, task) = serve(Duration::ZERO);
        // Requests a head limit apart keep it open past the head limit.
        for _ in 0..3 {
            c.write_all(GET).await.unwrap();
            assert!(answer(&mut c).await.starts_with("HTTP/1.1 200"));
            tokio::time::sleep(L.head + Duration::from_millis(100)).await;
        }
        let t = Instant::now();
        let (_, why) = task.await.unwrap();
        assert_eq!(why, "idle");
        let idle = t.elapsed() + L.head + Duration::from_millis(100);
        assert!(idle >= L.idle, "{idle:?}");
    }

    #[tokio::test]
    async fn a_request_being_served_is_never_cut_off() {
        // The answer takes longer than either limit.
        let (mut c, task) = serve(L.idle * 2);
        c.write_all(GET).await.unwrap();
        assert!(answer(&mut c).await.starts_with("HTTP/1.1 200"));
        let (took, why) = task.await.unwrap();
        assert_eq!(why, "idle");
        assert!(took >= L.idle * 3, "{took:?}");
    }

    #[test]
    fn the_cap_leaves_room_for_other_files() {
        let l = Limits::for_this_process();
        assert!(l.max >= 16 && l.max <= MAX_CONNECTIONS, "{l:?}");
    }
}
