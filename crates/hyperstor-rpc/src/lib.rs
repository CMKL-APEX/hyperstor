//! A multiplexed request/response connection on the reactor.
//!
//! Requests are numbered; any number may be outstanding, and responses
//! may arrive in any order. A writer task sends queued frames, several per
//! system call when they pile up; a reader task matches responses to
//! requests. If the connection breaks, every outstanding request fails.
//!
//! Connecting gives up after [`CONNECT_TIMEOUT`]. A call made with a
//! deadline ([`Conn::call_within`]) that passes it takes the connection
//! as broken: it is shut down, its other calls fail, and the next caller
//! connects afresh (01-implementation-notes.md I184).

use hyperstor_proto::{FrameError, HEADER_LEN, Header, Msg, encode_frame};
use hyperstor_reactor::chan::{OneSender, Receiver, Sender, channel, oneshot};
use hyperstor_reactor::net::TcpStream;
use hyperstor_reactor::spawn;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;
use thiserror::Error;

/// How long a connect may take before it fails.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// The deadline of calls between nodes and to the map service that have
/// none of their own: long enough for any answer that isn't a long
/// operation, short enough that a peer that stopped answering is let go.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RpcError {
    #[error("connection failed: {0}")]
    Io(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("connection closed")]
    Closed,
    #[error("no answer in time")]
    Timeout,
}

impl From<FrameError> for RpcError {
    fn from(e: FrameError) -> Self {
        RpcError::Protocol(e.to_string())
    }
}

/// A response: the message and its payload.
pub type Reply = (Msg, Vec<u8>);

struct Inner {
    sock: Rc<TcpStream>,
    out: Sender<Vec<u8>>,
    pending: RefCell<HashMap<u64, OneSender<Result<Reply, RpcError>>>>,
    next: Cell<u64>,
    dead: Cell<bool>,
}

/// A client connection. Cheap to clone; all clones share the socket.
#[derive(Clone)]
pub struct Conn {
    inner: Rc<Inner>,
    pub addr: SocketAddr,
}

/// Reads one frame: header, body (decoded) and payload.
pub async fn read_frame(s: &TcpStream) -> Result<(Header, Msg, Vec<u8>), RpcError> {
    let (r, hb) = s.read_exact(vec![0; HEADER_LEN]).await;
    r.map_err(|e| RpcError::Io(e.to_string()))?;
    let h = Header::decode(hb[..].try_into().unwrap())?;
    let mut rest = vec![0; (h.body_len + h.payload_len) as usize];
    if !rest.is_empty() {
        let (r, b) = s.read_exact(rest).await;
        r.map_err(|e| RpcError::Io(e.to_string()))?;
        rest = b;
    }
    let payload = rest.split_off(h.body_len as usize);
    let msg = Msg::decode(h.kind, &rest)?;
    Ok((h, msg, payload))
}

/// Sends frames from `rx` on `s`, batching what has queued up.
pub async fn writer(s: Rc<TcpStream>, mut rx: Receiver<Vec<u8>>) {
    while let Some(first) = rx.recv().await {
        let mut buf = first;
        for more in rx.drain() {
            buf.extend_from_slice(&more);
        }
        let (r, _) = s.write_all(buf).await;
        if r.is_err() {
            let _ = s.shutdown();
            return;
        }
    }
}

impl Conn {
    /// Connects to `addr` and starts the connection's tasks.
    pub async fn connect(addr: SocketAddr) -> Result<Conn, RpcError> {
        let s = match hyperstor_reactor::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
        {
            Ok(r) => r.map_err(|e| RpcError::Io(e.to_string()))?,
            Err(_) => return Err(RpcError::Timeout),
        };
        let s = Rc::new(s);
        let (tx, rx) = channel();
        let inner = Rc::new(Inner {
            sock: s.clone(),
            out: tx,
            pending: RefCell::new(HashMap::new()),
            next: Cell::new(1),
            dead: Cell::new(false),
        });
        spawn(writer(s.clone(), rx));
        let rd = inner.clone();
        spawn(async move {
            let err = loop {
                match read_frame(&s).await {
                    Ok((h, msg, payload)) => {
                        if let Some(tx) = rd.pending.borrow_mut().remove(&h.req) {
                            let _ = tx.send(Ok((msg, payload)));
                        }
                    }
                    Err(e) => break e,
                }
            };
            rd.dead.set(true);
            let _ = s.shutdown();
            for (_, tx) in rd.pending.borrow_mut().drain() {
                let _ = tx.send(Err(err.clone()));
            }
        });
        Ok(Conn { inner, addr })
    }

    pub fn is_dead(&self) -> bool {
        self.inner.dead.get()
    }

    /// Requests waiting for their answer (tests).
    pub fn pending(&self) -> usize {
        self.inner.pending.borrow().len()
    }

    /// Takes the connection as broken: shuts it down, so that its reader
    /// fails every outstanding call and the next caller connects afresh.
    pub fn kill(&self) {
        self.inner.dead.set(true);
        let _ = self.inner.sock.shutdown();
    }

    /// Queues a request; its number and the receiver of its answer, or
    /// `None` on a dead connection.
    fn queue(
        &self,
        msg: &Msg,
        payload: &[u8],
    ) -> Option<(
        u64,
        hyperstor_reactor::chan::OneReceiver<Result<Reply, RpcError>>,
    )> {
        if self.inner.dead.get() {
            return None;
        }
        let (tx, rx) = oneshot();
        let req = self.inner.next.get();
        self.inner.next.set(req + 1);
        self.inner.pending.borrow_mut().insert(req, tx);
        if self
            .inner
            .out
            .send(encode_frame(req, msg, payload))
            .is_err()
        {
            self.inner.pending.borrow_mut().remove(&req);
            return None;
        }
        Some((req, rx))
    }

    /// Queues a request now and returns the future of its response. The
    /// request is on the wire in the order `start` was called, which
    /// replication relies on (spec/09-replication.md §8.1).
    pub fn start(
        &self,
        msg: &Msg,
        payload: &[u8],
    ) -> impl Future<Output = Result<Reply, RpcError>> + 'static {
        let queued = self.queue(msg, payload);
        async move {
            let Some((_, rx)) = queued else {
                return Err(RpcError::Closed);
            };
            rx.recv().await.unwrap_or(Err(RpcError::Closed))
        }
    }

    /// [`Conn::start`] with a deadline: past it the call fails with
    /// [`RpcError::Timeout`], its pending entry goes, and the connection is
    /// taken as broken ([`Conn::kill`]).
    pub fn start_within(
        &self,
        msg: &Msg,
        payload: &[u8],
        within: Duration,
    ) -> impl Future<Output = Result<Reply, RpcError>> + 'static {
        self.deadline(msg, payload, within, true)
    }

    fn deadline(
        &self,
        msg: &Msg,
        payload: &[u8],
        within: Duration,
        kill: bool,
    ) -> impl Future<Output = Result<Reply, RpcError>> + 'static {
        let queued = self.queue(msg, payload);
        let conn = self.clone();
        async move {
            let Some((req, rx)) = queued else {
                return Err(RpcError::Closed);
            };
            match hyperstor_reactor::time::timeout(within, rx.recv()).await {
                Ok(r) => r.unwrap_or(Err(RpcError::Closed)),
                Err(_) => {
                    conn.inner.pending.borrow_mut().remove(&req);
                    if kill {
                        conn.kill();
                    }
                    Err(RpcError::Timeout)
                }
            }
        }
    }

    /// A call that gives up waiting after `within` but leaves the
    /// connection as it is (a ping on a connection that also carries
    /// replication): its pending entry goes, and a late answer is
    /// dropped.
    pub async fn try_within(
        &self,
        msg: &Msg,
        payload: &[u8],
        within: Duration,
    ) -> Result<Reply, RpcError> {
        self.deadline(msg, payload, within, false).await
    }

    pub async fn call(&self, msg: &Msg, payload: &[u8]) -> Result<Reply, RpcError> {
        self.start(msg, payload).await
    }

    /// [`Conn::call`] with a deadline ([`Conn::start_within`]).
    pub async fn call_within(
        &self,
        msg: &Msg,
        payload: &[u8],
        within: Duration,
    ) -> Result<Reply, RpcError> {
        self.start_within(msg, payload, within).await
    }
}

/// A caller waiting on another's connect.
type Waiter = OneSender<Result<Conn, RpcError>>;

/// Connections by address, at most one live each (I184). Concurrent
/// callers for an address with no live connection share one connect; a
/// live connection is never replaced, and a dead one only by a new one.
/// Without this, callers that raced past an empty slot each connected and
/// the last one won, leaving the others open and unused for as long as
/// the peer kept them (hundreds, on a busy restarted node).
#[derive(Default)]
pub struct Pool {
    conns: RefCell<HashMap<SocketAddr, Conn>>,
    /// Callers waiting on a connect in progress, by address.
    connecting: RefCell<HashMap<SocketAddr, Vec<Waiter>>>,
    /// Connects made (tests).
    connects: Cell<u64>,
}

impl Pool {
    /// The live connection to `addr`, connecting with [`Conn::connect`] if
    /// there is none.
    pub async fn get(&self, addr: SocketAddr) -> Result<Conn, RpcError> {
        self.get_with(addr, Conn::connect).await
    }

    /// [`Pool::get`] with `connect` making a new connection (and saying
    /// whatever it must say first). Only one runs per address at a time;
    /// other callers wait for its result.
    pub async fn get_with<F, Fut>(&self, addr: SocketAddr, connect: F) -> Result<Conn, RpcError>
    where
        F: FnOnce(SocketAddr) -> Fut,
        Fut: Future<Output = Result<Conn, RpcError>>,
    {
        if let Some(c) = self.conns.borrow().get(&addr)
            && !c.is_dead()
        {
            return Ok(c.clone());
        }
        let waiting = {
            let mut connecting = self.connecting.borrow_mut();
            match connecting.get_mut(&addr) {
                Some(w) => {
                    let (tx, rx) = oneshot();
                    w.push(tx);
                    Some(rx)
                }
                None => {
                    connecting.insert(addr, Vec::new());
                    None
                }
            }
        };
        if let Some(rx) = waiting {
            return rx.recv().await.unwrap_or(Err(RpcError::Closed));
        }
        self.connects.set(self.connects.get() + 1);
        // Whatever happens, the waiters are told and the slot freed (a
        // cancelled connect drops this guard).
        struct Done<'a>(&'a Pool, SocketAddr, Option<Result<Conn, RpcError>>);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                let r = self.2.take().unwrap_or(Err(RpcError::Closed));
                if let Ok(c) = &r {
                    self.0.conns.borrow_mut().insert(self.1, c.clone());
                }
                let waiters = self.0.connecting.borrow_mut().remove(&self.1);
                for w in waiters.into_iter().flatten() {
                    let _ = w.send(r.clone());
                }
            }
        }
        let mut done = Done(self, addr, None);
        let r = connect(addr).await;
        done.2 = Some(r.clone());
        drop(done);
        r
    }

    /// Connects made so far (tests).
    pub fn connects(&self) -> u64 {
        self.connects.get()
    }

    /// Live connections held.
    pub fn live(&self) -> usize {
        self.conns
            .borrow()
            .values()
            .filter(|c| !c.is_dead())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_reactor::block_on;
    use hyperstor_reactor::net::TcpListener;

    /// A call to a peer that accepts and never answers fails at its
    /// deadline, leaves no pending entry, and takes the connection down so
    /// the next caller connects afresh; a call without a deadline on the
    /// same connection fails with it.
    #[test]
    fn a_call_past_its_deadline_fails_and_takes_the_connection_down() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            // Accepts, reads nothing, answers nothing; kept open.
            let held = spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                hyperstor_reactor::time::sleep(Duration::from_secs(5)).await;
                drop(s);
            });
            let c = Conn::connect(addr).await.unwrap();
            let other = c.start(&Msg::Ping, &[]);
            // A ping that gives up leaves the connection and its other
            // calls as they are, and takes only its own entry away.
            assert_eq!(
                c.try_within(&Msg::Ping, &[], Duration::from_millis(100))
                    .await,
                Err(RpcError::Timeout)
            );
            assert!(!c.is_dead());
            assert_eq!(c.pending(), 1);
            let t = std::time::Instant::now();
            let r = c
                .call_within(&Msg::Ping, &[], Duration::from_millis(200))
                .await;
            assert_eq!(r, Err(RpcError::Timeout));
            assert!(t.elapsed() < Duration::from_secs(2));
            assert!(c.is_dead());
            assert!(other.await.is_err(), "its other calls fail too");
            assert_eq!(c.pending(), 0);
            assert_eq!(
                c.call(&Msg::Ping, &[]).await,
                Err(RpcError::Closed),
                "a dead connection takes no calls"
            );
            drop(held);
        });
    }

    /// Connecting to an address nothing answers gives up in time.
    #[test]
    fn a_connect_that_hangs_gives_up() {
        block_on(async {
            // A documentation address (RFC 5737): nothing answers its SYN.
            let addr: SocketAddr = "192.0.2.1:9".parse().unwrap();
            let t = std::time::Instant::now();
            let r = Conn::connect(addr).await;
            assert!(r.is_err(), "{:?}", r.as_ref().map(|c| c.addr));
            assert!(t.elapsed() <= CONNECT_TIMEOUT + Duration::from_secs(1));
        });
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use hyperstor_reactor::net::TcpListener;
    use hyperstor_reactor::{block_on, spawn};

    /// A hundred callers at once get one connection, made once.
    #[test]
    fn concurrent_callers_share_one_connect() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let accepts = Rc::new(Cell::new(0u32));
            let a = accepts.clone();
            spawn(async move {
                let mut held = Vec::new();
                while let Ok((s, _)) = l.accept().await {
                    a.set(a.get() + 1);
                    held.push(s);
                }
            });
            let pool = Rc::new(Pool::default());
            let callers: Vec<_> = (0..100)
                .map(|_| {
                    let p = pool.clone();
                    spawn(async move { p.get(addr).await.map(|c| c.addr) })
                })
                .collect();
            for c in callers {
                assert_eq!(c.await, Ok(addr));
            }
            hyperstor_reactor::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(pool.connects(), 1);
            assert_eq!(accepts.get(), 1, "one socket reached the peer");
            assert_eq!(pool.live(), 1);
            // A dead connection is replaced, once.
            pool.get(addr).await.unwrap().kill();
            let again: Vec<_> = (0..10)
                .map(|_| {
                    let p = pool.clone();
                    spawn(async move { p.get(addr).await.is_ok() })
                })
                .collect();
            for c in again {
                assert!(c.await);
            }
            assert_eq!(pool.connects(), 2);
        });
    }
}
