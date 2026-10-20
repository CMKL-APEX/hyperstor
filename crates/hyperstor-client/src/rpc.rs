//! A multiplexed request/response connection on the reactor.
//!
//! Requests are numbered; any number may be outstanding, and responses
//! may arrive in any order. A writer task sends queued frames, several per
//! system call when they pile up; a reader task matches responses to
//! requests. If the connection breaks, every outstanding request fails.

use hyperstor_proto::{FrameError, HEADER_LEN, Header, Msg, encode_frame};
use hyperstor_reactor::chan::{OneSender, Receiver, Sender, channel, oneshot};
use hyperstor_reactor::net::TcpStream;
use hyperstor_reactor::spawn;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RpcError {
    #[error("connection failed: {0}")]
    Io(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("connection closed")]
    Closed,
}

impl From<FrameError> for RpcError {
    fn from(e: FrameError) -> Self {
        RpcError::Protocol(e.to_string())
    }
}

/// A response: the message and its payload.
pub type Reply = (Msg, Vec<u8>);

struct Inner {
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
        let s = TcpStream::connect(addr)
            .await
            .map_err(|e| RpcError::Io(e.to_string()))?;
        let s = Rc::new(s);
        let (tx, rx) = channel();
        let inner = Rc::new(Inner {
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

    /// Queues a request now and returns the future of its response. The
    /// request is on the wire in the order `start` was called, which
    /// replication relies on (spec/design.md §8.1).
    pub fn start(
        &self,
        msg: &Msg,
        payload: &[u8],
    ) -> impl Future<Output = Result<Reply, RpcError>> + 'static {
        let (tx, rx) = oneshot();
        let ok = if self.inner.dead.get() {
            false
        } else {
            let req = self.inner.next.get();
            self.inner.next.set(req + 1);
            self.inner.pending.borrow_mut().insert(req, tx);
            self.inner.out.send(encode_frame(req, msg, payload)).is_ok()
        };
        async move {
            if !ok {
                return Err(RpcError::Closed);
            }
            rx.recv().await.unwrap_or(Err(RpcError::Closed))
        }
    }

    pub async fn call(&self, msg: &Msg, payload: &[u8]) -> Result<Reply, RpcError> {
        self.start(msg, payload).await
    }
}
