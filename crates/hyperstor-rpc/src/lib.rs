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

pub mod ktls;
pub mod tls;

/// A data-path peer's identity, from its certificate.
pub use hyperstor_pki::Identity;
/// What a certificate lets its holder do.
pub use hyperstor_pki::Role;

use hyperstor_codec::{AlignedBuf, IoBuf};
use hyperstor_proto::{FrameError, HEADER_LEN, Header, Msg, encode_frame, encode_head};
use hyperstor_reactor::chan::{OneSender, Receiver, Sender, channel, oneshot};
use hyperstor_reactor::net::TcpStream;
use hyperstor_reactor::spawn;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::ops::Range;
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

/// A frame queued for [`writer`]: its bytes, and perhaps a payload sent
/// after them without being copied in: all or part of a buffer, owned or
/// shared with the frames to other replicas.
pub struct Frame {
    head: Vec<u8>,
    tail: Option<IoBuf>,
    /// A barrier ([`Frame::barrier`]): told once everything queued before
    /// it is on the socket.
    flushed: Option<OneSender<()>>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("len", &self.len())
            .field("barrier", &self.flushed.is_some())
            .finish()
    }
}

impl From<Vec<u8>> for Frame {
    fn from(head: Vec<u8>) -> Self {
        Frame {
            head,
            tail: None,
            flushed: None,
        }
    }
}

impl Frame {
    /// A frame with `payload` copied in.
    pub fn new(req: u64, msg: &Msg, payload: &[u8]) -> Frame {
        encode_frame(req, msg, payload).into()
    }

    /// A frame that sends `payload` as it is, unless it is small enough
    /// to copy and join other small frames.
    pub fn owned(req: u64, msg: &Msg, payload: Vec<u8>) -> Frame {
        if payload.len() < OWN_READ {
            return Frame::new(req, msg, &payload);
        }
        Frame::shared(req, msg, &payload.into())
    }

    /// A frame that sends `payload` as it is, shared with others.
    pub fn shared(req: u64, msg: &Msg, payload: &IoBuf) -> Frame {
        Frame::part(req, msg, payload, 0..payload.len())
    }

    /// A frame that sends `buf[range]` as it is, unless it is small
    /// enough to copy: one piece of a client's write that spans chunks.
    pub fn part(req: u64, msg: &Msg, buf: &IoBuf, range: Range<usize>) -> Frame {
        if range.len() < OWN_READ {
            return Frame::new(req, msg, &buf[range]);
        }
        Frame {
            head: encode_head(req, msg, range.len(), 0),
            tail: Some(buf.slice(range)),
            flushed: None,
        }
    }

    /// No bytes: [`writer`] tells `flushed` once every frame queued
    /// before this one is on the socket. For whoever must then write the
    /// socket itself (the TLS handshake, D376).
    pub fn barrier(flushed: OneSender<()>) -> Frame {
        Frame {
            head: Vec::new(),
            tail: None,
            flushed: Some(flushed),
        }
    }

    pub fn len(&self) -> usize {
        self.head.len() + self.tail.as_ref().map_or(0, |t| t.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A queued request's number and the receiver of its answer, or `None`
/// on a dead connection.
type Queued = Option<(
    u64,
    hyperstor_reactor::chan::OneReceiver<Result<Reply, RpcError>>,
)>;

struct Inner {
    sock: Rc<TcpStream>,
    out: Sender<Frame>,
    pending: RefCell<HashMap<u64, OneSender<Result<Reply, RpcError>>>>,
    next: Cell<u64>,
    dead: Cell<bool>,
    /// The protocol version the peer's `HelloOk` answered; 0 before one.
    version: Cell<u16>,
    /// Who the peer proved to be in the TLS handshake ([`Conn::open`]);
    /// None on a connection that didn't authenticate.
    peer: RefCell<Option<tls::Peer>>,
    /// The session stayed TLS, in kernel TLS (D402, D403).
    encrypted: Cell<bool>,
}

/// What a caller with an identity does about TLS on a new connection
/// (D373; 30-data-path-tls.md §33.3, §33.4).
#[derive(Clone)]
pub struct CallerTls {
    /// `hyperstor_pki::data_client_config`.
    pub config: std::sync::Arc<rustls::ClientConfig>,
    /// The cluster's mode is `require`: a peer that speaks no TLS, or
    /// refuses it, is dropped; so is one that can't keep the session
    /// encrypted when `encrypt` says it must be.
    pub required: bool,
    /// The address dialled is one whose sessions stay encrypted
    /// (`ClusterMap::encrypts`; D402).
    pub encrypt: bool,
}

/// A process's identity on the data path (D25): what it calls with and
/// what it answers with, from one certificate.
#[derive(Clone)]
pub struct TlsIdentity {
    pub caller: std::sync::Arc<rustls::ClientConfig>,
    pub answerer: std::sync::Arc<rustls::ServerConfig>,
    /// Who the certificate says this process is.
    pub who: hyperstor_pki::Identity,
}

impl TlsIdentity {
    /// From a certificate chain, its key and the cluster's root, as PEM.
    pub fn from_pem(chain: &str, key: &str, root: &str) -> Result<TlsIdentity, String> {
        Ok(TlsIdentity {
            caller: std::sync::Arc::new(hyperstor_pki::data_client_config(chain, key, root)?),
            answerer: std::sync::Arc::new(hyperstor_pki::data_server_config(chain, key, root)?),
            who: hyperstor_pki::identity_in(chain)
                .ok_or("the certificate carries no hyperstor identity")?,
        })
    }

    /// From `dir`: the first of `names` there (`hyperstor_pki::identity_files`).
    pub fn load(dir: &std::path::Path, names: &[String]) -> Result<TlsIdentity, String> {
        let (chain, key, root) = hyperstor_pki::identity_files(dir, names)?;
        Self::from_pem(&chain, &key, &root)
    }

    /// What a caller does with it: `required` in a cluster whose mode is
    /// `require`.
    pub fn calling(&self, required: bool) -> CallerTls {
        CallerTls {
            config: self.caller.clone(),
            required,
            encrypt: false,
        }
    }

    /// What an answerer does with it: `encrypt` where the address it was
    /// called at is one whose sessions stay encrypted, `required` in mode
    /// `require`.
    pub fn answering(&self, encrypt: bool, required: bool) -> Answering {
        Answering {
            config: self.answerer.clone(),
            encrypt,
            required,
        }
    }
}

impl CallerTls {
    /// With the session kept encrypted when `encrypt` (D402).
    pub fn encrypting(mut self, encrypt: bool) -> CallerTls {
        self.encrypt = encrypt;
        self
    }
}

/// How a served connection answers a request to start TLS.
#[derive(Clone)]
pub struct Answering {
    /// `hyperstor_pki::data_server_config`.
    pub config: std::sync::Arc<rustls::ServerConfig>,
    /// This end's map says the session stays encrypted (D402): it does,
    /// whatever the caller says, if the caller speaks
    /// [`hyperstor_proto::ENCRYPT_VERSION`].
    pub encrypt: bool,
    /// Mode `require`: a caller that can't keep it encrypted when
    /// `encrypt` is refused TLS, and with it everything else.
    pub required: bool,
}

/// What a served connection answers `StartTls` with, or None where this
/// end has no identity or the cluster's mode is `off`.
pub type AnswererTls = Option<Answering>;

/// Answers a `StartTls` or a `StartTlsOffer` (request `req`; `offer` is
/// the offer's `encrypt`, None for `StartTls`) on a served connection
/// whose replies go through `tx` to [`writer`]. With `tls`: `StartTlsOk`
/// or `StartTlsEncrypted`, then, once it is on the socket, the handshake
/// on `s`; who the caller is, or an error after which the connection must
/// be closed. Without: `refusal` and None, and frames go on
/// unauthenticated.
///
/// The session stays encrypted, in kernel TLS, if the caller offered to
/// or `tls.encrypt` says so and the caller can (D403). A caller of a
/// version before that, where `tls.encrypt` and `tls.required`, is sent
/// `refusal`: in `require` it is then served nothing.
///
/// The caller sends nothing after its request until it has the answer,
/// so the next bytes read from `s` are the handshake's; whoever calls
/// this reads no frame meanwhile.
pub async fn answer_start_tls(
    s: &TcpStream,
    tx: &Sender<Frame>,
    req: u64,
    tls: &AnswererTls,
    refusal: Msg,
    offer: Option<bool>,
) -> Result<Option<tls::Peer>, RpcError> {
    let Some(a) = tls else {
        let _ = tx.send(Frame::new(req, &refusal, &[]));
        return Ok(None);
    };
    let encrypt = match offer {
        Some(asked) => asked || a.encrypt,
        None if a.encrypt && a.required => {
            let _ = tx.send(Frame::new(req, &refusal, &[]));
            return Ok(None);
        }
        None => false,
    };
    let answer = if encrypt {
        Msg::StartTlsEncrypted
    } else {
        Msg::StartTlsOk
    };
    let (flushed, wait) = oneshot();
    if tx.send(Frame::new(req, &answer, &[])).is_err()
        || tx.send(Frame::barrier(flushed)).is_err()
        || wait.recv().await.is_none()
    {
        return Err(RpcError::Closed);
    }
    let failed = |e: tls::TlsError| RpcError::Protocol(e.to_string());
    if !encrypt {
        return tls::answerer(s, a.config.clone())
            .await
            .map(Some)
            .map_err(failed);
    }
    let kept = tls::answerer_kept(s, a.config.clone())
        .await
        .map_err(failed)?;
    encrypt_kept(s, &kept)?;
    Ok(Some(tls::Peer {
        encrypted: true,
        ..kept.peer
    }))
}

/// Hands a kept session to kernel TLS (D404).
fn encrypt_kept(s: &TcpStream, kept: &tls::Kept) -> Result<(), RpcError> {
    use std::os::fd::AsRawFd;
    ktls::enable(s.as_raw_fd(), &kept.secrets)
        .map_err(|e| RpcError::Protocol(format!("kernel TLS: {e}")))
}

/// A client connection. Cheap to clone; all clones share the socket.
#[derive(Clone)]
pub struct Conn {
    inner: Rc<Inner>,
    pub addr: SocketAddr,
}

/// Bytes hyperstor's connections sent and received in this process, by
/// local address (26-networks.md §29.8): a node reports them with its
/// ports' counters, so the fabric reconciler can tell its own traffic
/// from other traffic on a port.
pub mod traffic {
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    pub struct Counter {
        pub tx: AtomicU64,
        pub rx: AtomicU64,
    }

    impl Counter {
        pub fn sent(&self, n: usize) {
            self.tx.fetch_add(n as u64, Ordering::Relaxed);
        }
        pub fn received(&self, n: usize) {
            self.rx.fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    static TABLE: Mutex<BTreeMap<IpAddr, Arc<Counter>>> = Mutex::new(BTreeMap::new());

    /// The counter of connections from `ip`. Taken once per connection,
    /// so the lock is off the data path.
    pub fn of(ip: IpAddr) -> Arc<Counter> {
        let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        t.entry(ip).or_default().clone()
    }

    /// Every local address's bytes so far, as (address, sent, received).
    pub fn totals() -> Vec<(IpAddr, u64, u64)> {
        let t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        t.iter()
            .map(|(ip, c)| {
                (
                    *ip,
                    c.tx.load(Ordering::Relaxed),
                    c.rx.load(Ordering::Relaxed),
                )
            })
            .collect()
    }
}

/// The traffic counter of `s`'s local address, if it has one.
pub fn counter_of(s: &TcpStream) -> Option<std::sync::Arc<traffic::Counter>> {
    s.local_addr().ok().map(|a| traffic::of(a.ip()))
}

/// Reads one frame: header, body (decoded) and payload.
pub async fn read_frame(s: &TcpStream) -> Result<(Header, Msg, Vec<u8>), RpcError> {
    read_frame_counted(s, None).await
}

/// [`read_frame`], adding the frame's bytes to `counter`.
pub async fn read_frame_counted(
    s: &TcpStream,
    counter: Option<&traffic::Counter>,
) -> Result<(Header, Msg, Vec<u8>), RpcError> {
    match read_any(s, counter, false).await? {
        (h, msg, Got::Vec(p)) => Ok((h, msg, p)),
        (_, _, Got::Aligned(_)) => unreachable!("not asked for"),
    }
}

/// [`read_frame_counted`] for a request to a storage node: a payload of
/// 16 KiB or more is received into a buffer aligned to a
/// block, which the engine then writes as it is (I343).
pub async fn read_request(
    s: &TcpStream,
    counter: Option<&traffic::Counter>,
) -> Result<(Header, Msg, IoBuf), RpcError> {
    let (h, msg, p) = read_any(s, counter, true).await?;
    let p = match p {
        Got::Vec(p) => p.into(),
        Got::Aligned(p) => p,
    };
    Ok((h, msg, p))
}

/// A payload as read: other frames keep plain vectors, which cost no
/// allocation of an [`IoBuf`] when they are replies or empty.
enum Got {
    Vec(Vec<u8>),
    Aligned(IoBuf),
}

async fn read_any(
    s: &TcpStream,
    counter: Option<&traffic::Counter>,
    aligned: bool,
) -> Result<(Header, Msg, Got), RpcError> {
    let (r, hb) = s.read_exact(vec![0; HEADER_LEN]).await;
    r.map_err(|e| RpcError::Io(e.to_string()))?;
    let h = Header::decode(hb[..].try_into().unwrap())?;
    let n = h.payload_len as usize;
    let (body, payload) = if n >= OWN_READ {
        let body = read_n(s, h.body_len as usize).await?;
        let payload = if aligned {
            Got::Aligned(read_aligned(s, n).await?)
        } else {
            Got::Vec(read_n(s, n).await?)
        };
        (body, payload)
    } else {
        let mut rest = read_n(s, h.body_len as usize + n).await?;
        let payload = rest.split_off(h.body_len as usize);
        (rest, Got::Vec(payload))
    };
    if let Some(c) = counter {
        c.received(HEADER_LEN + body.len() + n);
    }
    let msg = Msg::decode(h.kind, &body)?;
    Ok((h, msg, payload))
}

/// Payloads from this size up are read into a buffer of their own rather
/// than split off the body, which would copy them.
const OWN_READ: usize = 16 << 10;

async fn read_n(s: &TcpStream, n: usize) -> Result<Vec<u8>, RpcError> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let (r, b) = s.read_exact_n(n).await;
    r.map_err(|e| RpcError::Io(e.to_string()))?;
    Ok(b)
}

/// An aligned buffer being received into: its first bytes are written
/// by the kernel before anything reads them.
struct Receiving(AlignedBuf);

// SAFETY: the bytes are the buffer's heap allocation, owned by it and
// never moved; `recv_cap` is its length.
unsafe impl hyperstor_reactor::net::RecvBuf for Receiving {
    fn recv_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
    fn recv_cap(&self) -> usize {
        self.0.len()
    }
}

async fn read_aligned(s: &TcpStream, n: usize) -> Result<IoBuf, RpcError> {
    // SAFETY: the receive below writes all of the first `n` bytes before
    // the buffer is read; after an error it is dropped unread.
    let buf = Receiving(unsafe { AlignedBuf::for_receive(n) });
    let (r, Receiving(b)) = s.read_exact_into(buf, n).await;
    r.map_err(|e| RpcError::Io(e.to_string()))?;
    Ok(IoBuf::aligned(b, n))
}

/// How many bytes of queued frames [`writer`] joins into one send.
pub const BATCH_BYTES: usize = 256 << 10;

/// The next send: `first`, joined by the frames queued behind it while
/// the whole fits in [`BATCH_BYTES`]. A frame that doesn't fit is left in
/// `next` and sent on its own, uncopied: joining every queued push into
/// one buffer copied hundreds of MB and held the reactor 100–125 ms
/// (I283). A frame with a payload of its own is never joined.
fn next_send(first: Frame, rx: &mut Receiver<Frame>, next: &mut Option<Frame>) -> Frame {
    if first.tail.is_some() || first.flushed.is_some() {
        return first;
    }
    let mut buf = first.head;
    while buf.len() < BATCH_BYTES {
        let Some(more) = rx.try_recv() else { break };
        if more.tail.is_some() || more.flushed.is_some() || buf.len() + more.len() > BATCH_BYTES {
            *next = Some(more);
            break;
        }
        buf.extend_from_slice(&more.head);
    }
    buf.into()
}

/// Sends frames from `rx` on `s`, batching small frames that have queued
/// up ([`next_send`]), and counts them as its local address's traffic.
pub async fn writer(s: Rc<TcpStream>, mut rx: Receiver<Frame>) {
    let counter = counter_of(&s);
    let mut next = None;
    loop {
        let first = match next.take() {
            Some(f) => f,
            None => match rx.recv().await {
                Some(f) => f,
                None => return,
            },
        };
        let f = next_send(first, &mut rx, &mut next);
        if let Some(flushed) = f.flushed {
            // Every write before it was awaited: it is all on the socket.
            let _ = flushed.send(());
            continue;
        }
        if let Some(c) = &counter {
            c.sent(f.len());
        }
        let r = match f.tail {
            Some(tail) => s.write_all2(f.head, tail).await,
            None => s.write_all(f.head).await.0,
        };
        if r.is_err() {
            let _ = s.shutdown();
            return;
        }
    }
}

impl Conn {
    /// Connects to `addr` and starts the connection's tasks.
    pub async fn connect(addr: SocketAddr) -> Result<Conn, RpcError> {
        Self::connect_within(addr, CONNECT_TIMEOUT).await
    }

    /// [`Conn::connect`], giving up after `limit` rather than
    /// [`CONNECT_TIMEOUT`].
    pub async fn connect_within(addr: SocketAddr, limit: Duration) -> Result<Conn, RpcError> {
        let s = match hyperstor_reactor::time::timeout(limit, TcpStream::connect(addr)).await {
            Ok(r) => r.map_err(|e| RpcError::Io(e.to_string()))?,
            Err(_) => return Err(RpcError::Timeout),
        };
        Ok(Self::run_on(s, addr))
    }

    /// Connects to `addr`, says `hello`, and, with `tls`, starts TLS when
    /// the peer's `HelloOk` answers a version that has it (D373): the
    /// handshake runs before the connection carries anything else.
    /// Returns the connection, its version and peer set, and the answer
    /// to `hello` (`HelloOk`, or an `Error` such as a blocklisted
    /// client's).
    ///
    /// A peer that speaks no TLS, has no identity, or runs in mode `off`
    /// is used unauthenticated, unless `tls.required`: then it is an
    /// error.
    pub async fn open(
        addr: SocketAddr,
        limit: Duration,
        hello: &Msg,
        tls: Option<&CallerTls>,
    ) -> Result<(Conn, Msg), RpcError> {
        Self::open_capped(addr, limit, hello, tls, u16::MAX).await
    }

    /// [`Conn::open`] as a caller of protocol `cap` would: its tests
    /// play callers of older builds.
    async fn open_capped(
        addr: SocketAddr,
        limit: Duration,
        hello: &Msg,
        tls: Option<&CallerTls>,
        cap: u16,
    ) -> Result<(Conn, Msg), RpcError> {
        let s = match hyperstor_reactor::time::timeout(limit, TcpStream::connect(addr)).await {
            Ok(r) => r.map_err(|e| RpcError::Io(e.to_string()))?,
            Err(_) => return Err(RpcError::Timeout),
        };
        let counter = counter_of(&s);
        let preamble = async {
            let ask = |m: &Msg| {
                let f = encode_frame(0, m, &[]);
                if let Some(c) = &counter {
                    c.sent(f.len());
                }
                s.write_all(f)
            };
            ask(hello)
                .await
                .0
                .map_err(|e| RpcError::Io(e.to_string()))?;
            let (_, answer, _) = read_frame_counted(&s, counter.as_deref()).await?;
            let version = match &answer {
                Msg::HelloOk { version, .. } => (*version).min(cap),
                _ => 0,
            };
            let mut peer = None;
            let mut encrypted = false;
            if let Some(tls) = tls {
                let refused = |why: &str| {
                    RpcError::Protocol(format!(
                        "{addr} {why}, and the cluster requires authenticated connections"
                    ))
                };
                if version >= hyperstor_proto::TLS_VERSION {
                    // D403: a peer that can keep the session is offered
                    // to; an older one can't, and in `require` a session
                    // that must be encrypted isn't made with it.
                    let request = if version >= hyperstor_proto::ENCRYPT_VERSION {
                        Msg::StartTlsOffer {
                            encrypt: tls.encrypt,
                        }
                    } else if tls.encrypt && tls.required {
                        return Err(refused("speaks a protocol that can't encrypt"));
                    } else {
                        Msg::StartTls
                    };
                    ask(&request)
                        .await
                        .0
                        .map_err(|e| RpcError::Io(e.to_string()))?;
                    let failed = |e: tls::TlsError| RpcError::Protocol(format!("{addr}: {e}"));
                    match read_frame_counted(&s, counter.as_deref()).await?.1 {
                        Msg::StartTlsOk
                            if tls.encrypt && matches!(request, Msg::StartTlsOffer { .. }) =>
                        {
                            return Err(RpcError::Protocol(format!(
                                "{addr} would turn a session that must be encrypted back to plaintext"
                            )));
                        }
                        Msg::StartTlsOk => {
                            peer = Some(tls::caller(&s, tls.config.clone()).await.map_err(failed)?);
                        }
                        Msg::StartTlsEncrypted if matches!(request, Msg::StartTlsOffer { .. }) => {
                            let kept = tls::caller_kept(&s, tls.config.clone())
                                .await
                                .map_err(failed)?;
                            encrypt_kept(&s, &kept)?;
                            encrypted = true;
                            peer = Some(tls::Peer {
                                encrypted: true,
                                ..kept.peer
                            });
                        }
                        _ if tls.required => return Err(refused("refused TLS")),
                        _ => {}
                    }
                } else if tls.required && matches!(answer, Msg::HelloOk { .. }) {
                    return Err(refused("speaks a protocol from before TLS"));
                }
            }
            Ok((version, peer, encrypted, answer))
        };
        let (version, peer, encrypted, answer) =
            match hyperstor_reactor::time::timeout(CALL_TIMEOUT, preamble).await {
                Ok(r) => r?,
                Err(_) => return Err(RpcError::Timeout),
            };
        let c = Self::run_on(s, addr);
        c.set_version(version);
        *c.inner.peer.borrow_mut() = peer;
        c.inner.encrypted.set(encrypted);
        Ok((c, answer))
    }

    /// Who the peer proved to be, on a connection [`Conn::open`]
    /// authenticated.
    pub fn peer(&self) -> Option<hyperstor_pki::Identity> {
        self.inner.peer.borrow().as_ref().map(|p| p.who.clone())
    }

    /// Whether the session stayed TLS (D402): every byte since the
    /// handshake encrypted, by kernel TLS.
    pub fn encrypted(&self) -> bool {
        self.inner.encrypted.get()
    }

    /// Whether the peer authenticated with a certificate `revoked` says
    /// is revoked (D384): given its serial, its identity's URI and when
    /// it was issued. False on an unauthenticated connection.
    pub fn peer_revoked(&self, revoked: impl FnOnce(&str, &str, u64) -> bool) -> bool {
        self.inner
            .peer
            .borrow()
            .as_ref()
            .is_some_and(|p| revoked(&p.serial, &p.who.uri(), p.issued_unix))
    }

    /// Starts the connection's tasks on a connected socket.
    fn run_on(s: TcpStream, addr: SocketAddr) -> Conn {
        let s = Rc::new(s);
        let (tx, rx) = channel();
        let inner = Rc::new(Inner {
            sock: s.clone(),
            out: tx,
            pending: RefCell::new(HashMap::new()),
            next: Cell::new(1),
            dead: Cell::new(false),
            version: Cell::new(0),
            peer: RefCell::new(None),
            encrypted: Cell::new(false),
        });
        spawn(writer(s.clone(), rx));
        let rd = inner.clone();
        let counter = counter_of(&s);
        spawn(async move {
            let err = loop {
                match read_frame_counted(&s, counter.as_deref()).await {
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
        Conn { inner, addr }
    }

    pub fn is_dead(&self) -> bool {
        self.inner.dead.get()
    }

    /// The protocol version the peer answered in `HelloOk`, which says what
    /// it may be sent (`hyperstor_proto::VERSION`); 0 if it wasn't asked.
    pub fn version(&self) -> u16 {
        self.inner.version.get()
    }

    /// Records what the peer's `HelloOk` answered.
    pub fn set_version(&self, v: u16) {
        self.inner.version.set(v);
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
    fn queue(&self, frame: impl FnOnce(u64) -> Frame) -> Queued {
        if self.inner.dead.get() {
            return None;
        }
        let (tx, rx) = oneshot();
        let req = self.inner.next.get();
        self.inner.next.set(req + 1);
        self.inner.pending.borrow_mut().insert(req, tx);
        if self.inner.out.send(frame(req)).is_err() {
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
        Self::answer(self.queue(|req| Frame::new(req, msg, payload)))
    }

    /// [`Conn::start`] with a payload shared with other calls (one write
    /// to every replica), sent without a copy when it is large.
    pub fn start_shared(
        &self,
        msg: &Msg,
        payload: &IoBuf,
    ) -> impl Future<Output = Result<Reply, RpcError>> + 'static {
        Self::answer(self.queue(|req| Frame::shared(req, msg, payload)))
    }

    /// [`Conn::start`] with `buf[range]` as the payload, sent without a
    /// copy when it is large ([`Frame::part`]).
    pub fn start_part(
        &self,
        msg: &Msg,
        buf: &IoBuf,
        range: Range<usize>,
    ) -> impl Future<Output = Result<Reply, RpcError>> + 'static {
        Self::answer(self.queue(|req| Frame::part(req, msg, buf, range)))
    }

    async fn answer(queued: Queued) -> Result<Reply, RpcError> {
        let Some((_, rx)) = queued else {
            return Err(RpcError::Closed);
        };
        rx.recv().await.unwrap_or(Err(RpcError::Closed))
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
        let queued = self.queue(|req| Frame::new(req, msg, payload));
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

    /// Closes the connection kept for `addr` if `stale` says so: the next
    /// [`Pool::get_with`] makes a new one. For a connection made before
    /// the cluster required authentication, which is refused everything
    /// now (D377).
    pub fn close_if(&self, addr: SocketAddr, stale: impl FnOnce(&Conn) -> bool) {
        if let Some(c) = self.conns.borrow().get(&addr)
            && !c.is_dead()
            && stale(c)
        {
            c.kill();
        }
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

    /// Requests waiting for their answer on the connection to `addr`; 0
    /// without one (26-networks.md §29.6: a path's load).
    pub fn pending(&self, addr: SocketAddr) -> usize {
        self.conns
            .borrow()
            .get(&addr)
            .filter(|c| !c.is_dead())
            .map_or(0, Conn::pending)
    }

    /// Connects made so far (tests).
    pub fn connects(&self) -> u64 {
        self.connects.get()
    }

    /// Every live connection held.
    pub fn conns(&self) -> Vec<Conn> {
        self.conns
            .borrow()
            .values()
            .filter(|c| !c.is_dead())
            .cloned()
            .collect()
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

    /// I283: small frames queued together go out in one send of at most
    /// [`BATCH_BYTES`]; a frame that doesn't fit goes out on its own,
    /// uncopied, and the order holds.
    #[test]
    fn the_writer_joins_small_frames_and_sends_large_ones_alone() {
        let (tx, mut rx) = channel::<Frame>();
        let push = vec![7u8; 4 << 20];
        let push_at = push.as_ptr();
        let big = vec![8u8; BATCH_BYTES];
        let big_at = big.as_ptr();
        let small = |n, b| Frame::from(vec![b; n]);
        for f in [small(100, 1), small(200, 2), push.into(), small(300, 3)] {
            tx.send(f).unwrap();
        }
        for _ in 0..2 * BATCH_BYTES / 1000 {
            tx.send(small(1000, 4)).unwrap();
        }
        // A payload of its own, after a small frame: not joined to it.
        tx.send(Frame::owned(9, &Msg::Ping, big)).unwrap();
        tx.send(small(10, 5)).unwrap();
        let mut next = None;
        let mut sends = Vec::new();
        while let Some(first) = next.take().or_else(|| rx.try_recv()) {
            sends.push(next_send(first, &mut rx, &mut next));
        }
        assert_eq!(sends[0].head, [vec![1u8; 100], vec![2; 200]].concat());
        assert_eq!(
            sends[1].head.as_ptr(),
            push_at,
            "the push is sent as queued"
        );
        assert_eq!(sends[1].len(), 4 << 20);
        assert!(sends[2].head.starts_with(&[3; 300]));
        assert!(sends[2..5].iter().all(|b| b.len() <= BATCH_BYTES));
        assert_eq!(sends.len(), 7, "small frames fill sends of the batch size");
        let tail: Vec<u8> = sends[2..5].iter().flat_map(|f| f.head.clone()).collect();
        assert_eq!(tail.len(), 300 + 2 * BATCH_BYTES / 1000 * 1000);
        let own = sends[5].tail.as_ref().expect("a payload of its own");
        assert_eq!(own.as_ptr(), big_at, "the payload is sent as given");
        assert_eq!(sends[6].head, vec![5u8; 10]);
    }

    /// HS5b: one payload shared by two frames, sent from where it is, and
    /// read back into a buffer of its own; small ones the usual way.
    #[test]
    fn a_shared_payload_reaches_the_peer_from_both_frames() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let reader = spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                let mut got = Vec::new();
                for _ in 0..4 {
                    let (h, m, p) = read_frame(&s).await.unwrap();
                    got.push((h.req, m, p));
                }
                got
            });
            let s = Rc::new(TcpStream::connect(addr).await.unwrap());
            let (tx, rx) = channel();
            let big: Vec<u8> = (0..3 << 20).map(|i| (i % 251) as u8).collect();
            let shared = IoBuf::from(big.clone());
            tx.send(Frame::shared(1, &Msg::Ping, &shared)).unwrap();
            tx.send(Frame::new(2, &Msg::Ping, b"small")).unwrap();
            tx.send(Frame::shared(3, &Msg::Ping, &shared)).unwrap();
            tx.send(Frame::owned(4, &Msg::Ping, vec![9; OWN_READ - 1]))
                .unwrap();
            drop((tx, shared));
            writer(s, rx).await;
            let got = reader.await;
            assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [1, 2, 3, 4]);
            assert!(got.iter().all(|g| g.1 == Msg::Ping));
            assert!(got[0].2 == big && got[2].2 == big);
            assert_eq!(got[1].2, b"small");
            assert_eq!(got[3].2, vec![9; OWN_READ - 1]);
        });
    }

    /// HS5b (I324): the parts of one buffer that a client's write sends to
    /// different chunks reach the peer as they were cut: large ones from
    /// the buffer itself, at any offset, and a small one copied.
    #[test]
    fn parts_of_one_buffer_reach_the_peer_as_cut() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let reader = spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                let mut got = Vec::new();
                for _ in 0..3 {
                    got.push(read_frame(&s).await.unwrap().2);
                }
                got
            });
            let s = Rc::new(TcpStream::connect(addr).await.unwrap());
            let (tx, rx) = channel();
            let buf = IoBuf::from((0..3 << 20).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
            let cuts = [
                0..(1 << 20) + 7,
                (1 << 20) + 7..(3 << 20) - 100,
                (3 << 20) - 100..3 << 20,
            ];
            for (i, r) in cuts.iter().enumerate() {
                let f = Frame::part(i as u64, &Msg::Ping, &buf, r.clone());
                assert_eq!(f.tail.is_some(), r.len() >= OWN_READ);
                tx.send(f).unwrap();
            }
            drop(tx);
            writer(s, rx).await;
            let got = reader.await;
            for (g, r) in got.iter().zip(&cuts) {
                assert!(g[..] == buf[r.clone()]);
            }
        });
    }

    /// I343: a storage node receives a large payload into a buffer that
    /// starts on a block, whole, whatever came before it on the stream.
    #[test]
    fn large_request_payloads_arrive_aligned() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let sizes = [OWN_READ - 1, OWN_READ, (1 << 20) + 4096, 4096, 3 << 20];
            let reader = spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                let mut got = Vec::new();
                for _ in 0..sizes.len() {
                    got.push(read_request(&s, None).await.unwrap().2);
                }
                got
            });
            let s = Rc::new(TcpStream::connect(addr).await.unwrap());
            let (tx, rx) = channel();
            let payloads: Vec<Vec<u8>> = (0..sizes.len())
                .map(|i| (0..sizes[i]).map(|j| ((i * 31 + j) % 251) as u8).collect())
                .collect();
            for (i, p) in payloads.iter().enumerate() {
                tx.send(Frame::owned(i as u64, &Msg::Ping, p.clone()))
                    .unwrap();
            }
            drop(tx);
            writer(s, rx).await;
            let got = reader.await;
            for (g, p) in got.iter().zip(&payloads) {
                assert!(g[..] == p[..]);
                if p.len() >= OWN_READ {
                    assert!(g.is_aligned(), "{} bytes", p.len());
                }
            }
        });
    }

    /// I283: frames of every size reach the peer whole and in order.
    #[test]
    fn the_writer_delivers_mixed_frames_in_order() {
        block_on(async {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let frames: Vec<Vec<u8>> = (0..40u32)
                .map(|i| {
                    vec![
                        i as u8;
                        if i % 7 == 3 {
                            3 << 20
                        } else {
                            100 + i as usize * 50
                        }
                    ]
                })
                .collect();
            let want = frames.concat();
            let n = want.len();
            let reader = spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                let (r, got) = s.read_exact(vec![0; n]).await;
                r.unwrap();
                got
            });
            let s = Rc::new(TcpStream::connect(addr).await.unwrap());
            let (tx, rx) = channel();
            for f in frames {
                // The large ones as a header and the rest of a shared
                // buffer, sent by `write_all2`.
                let f = if f.len() > BATCH_BYTES {
                    Frame {
                        head: f[..100].to_vec(),
                        tail: Some(IoBuf::from(f.clone()).slice(100..f.len())),
                        flushed: None,
                    }
                } else {
                    f.into()
                };
                tx.send(f).unwrap();
            }
            drop(tx);
            writer(s, rx).await;
            assert!(reader.await == want);
        });
    }
}
