//! The data path's TLS handshake (04-decisions.md D25, D373, D376;
//! 30-data-path-tls.md §33.3), on a reactor's socket.
//!
//! rustls does no I/O of its own: [`caller`] and [`answerer`] feed it
//! what the socket reads and write out what it produces. Both ends check
//! the other's chain against the cluster's root and return the identity
//! in its certificate.
//!
//! The connection then turns back to plaintext. The caller sends
//! `close_notify`; the answerer sends its own and nothing more until the
//! caller's next frame; the caller sends frames once it has read the
//! answerer's `close_notify`. So neither side ever reads a byte of
//! plaintext into TLS: when either function returns, the next byte on
//! the socket is the first byte of the next frame.

use hyperstor_pki::Identity;
use hyperstor_reactor::net::TcpStream;
use hyperstor_reactor::time::timeout;
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// How long a handshake may take from its first byte to the turn back to
/// plaintext. Why a bound: a peer that stops mid-handshake would hold the
/// connection, and whoever waits to use it, forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a handshake failed.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("handshake: {0}")]
    Io(#[from] io::Error),
    #[error("handshake: {0}")]
    Tls(#[from] rustls::Error),
    #[error("the peer closed the connection during the handshake")]
    Closed,
    #[error("the handshake took longer than {0:?}")]
    TimedOut(Duration),
    #[error("the peer's certificate carries no hyperstor identity")]
    NoIdentity,
    #[error("not an address or name TLS can dial")]
    BadName,
}

/// Who a handshake's other end proved to be, and which of its
/// certificates it showed: what the revocation list names (D384).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub who: Identity,
    /// The certificate's serial, as `hyperstor_pki::serial_of` gives it.
    pub serial: String,
    /// When it was issued (Unix seconds): an identity's revocation covers
    /// certificates issued until then.
    pub issued_unix: u64,
    /// The session stayed TLS, in kernel TLS (D402): set once it is.
    pub encrypted: bool,
}

impl Peer {
    fn of(leaf: &[u8]) -> Option<Peer> {
        Some(Peer {
            who: hyperstor_pki::identity_of(leaf)?,
            serial: hyperstor_pki::serial_of(leaf)?,
            issued_unix: hyperstor_pki::issued_unix(leaf)?,
            encrypted: false,
        })
    }
}

/// One connection's handshake state and whether the peer has sent its
/// `close_notify`.
struct Session<'a> {
    stream: &'a TcpStream,
    conn: rustls::Connection,
    peer_closed: bool,
}

impl Session<'_> {
    /// Writes out everything rustls has to send.
    async fn flush(&mut self) -> Result<(), TlsError> {
        while self.conn.wants_write() {
            let mut out = Vec::with_capacity(4096);
            self.conn.write_tls(&mut out)?;
            let (r, _) = self.stream.write_all(out).await;
            r?;
        }
        Ok(())
    }

    /// Reads once from the socket and gives rustls all of it.
    async fn read(&mut self) -> Result<(), TlsError> {
        let (r, buf) = self.stream.read(vec![0u8; 16 * 1024]).await;
        let n = r?;
        if n == 0 {
            return Err(TlsError::Closed);
        }
        let mut rest = &buf[..n];
        while !rest.is_empty() {
            if self.conn.read_tls(&mut rest)? == 0 {
                return Err(TlsError::Closed);
            }
            let state = self.conn.process_new_packets()?;
            self.peer_closed |= state.peer_has_closed();
        }
        Ok(())
    }

    /// The handshake, then the turn back to plaintext: `first` sends its
    /// `close_notify` first (the caller), the other answers it.
    async fn run(&mut self, first: bool) -> Result<Peer, TlsError> {
        loop {
            self.flush().await?;
            if !self.conn.is_handshaking() {
                break;
            }
            self.read().await?;
        }
        let who = self
            .conn
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|leaf| Peer::of(leaf))
            .ok_or(TlsError::NoIdentity)?;
        if first {
            self.conn.send_close_notify();
            self.flush().await?;
        }
        while !self.peer_closed {
            self.read().await?;
        }
        if !first {
            self.conn.send_close_notify();
            self.flush().await?;
        }
        Ok(who)
    }
}

async fn bounded(mut s: Session<'_>, first: bool) -> Result<Peer, TlsError> {
    match timeout(HANDSHAKE_TIMEOUT, s.run(first)).await {
        Ok(r) => r,
        Err(_) => Err(TlsError::TimedOut(HANDSHAKE_TIMEOUT)),
    }
}

/// The byte the answerer sends, as TLS, once its handshake is done on a
/// session that stays.
const READY: u8 = 0x5e;

impl Session<'_> {
    /// The handshake of a session that stays TLS, ending where neither
    /// side holds a record it hasn't processed: the answerer sends one
    /// byte of application data, [`READY`], once it has the caller's
    /// `Finished`, and the caller sends nothing after its `Finished` until
    /// it has read that byte. So rustls has consumed exactly the
    /// handshake on both sides when its secrets are taken (kernel TLS can
    /// take no record rustls has already read).
    async fn run_kept(&mut self, first: bool) -> Result<Peer, TlsError> {
        loop {
            self.flush().await?;
            if !self.conn.is_handshaking() {
                break;
            }
            self.read().await?;
        }
        let who = self
            .conn
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|leaf| Peer::of(leaf))
            .ok_or(TlsError::NoIdentity)?;
        if first {
            loop {
                let mut b = [0u8; 1];
                match io::Read::read(&mut self.conn.reader(), &mut b) {
                    Ok(1) if b[0] == READY => break,
                    Ok(_) => return Err(TlsError::Closed),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.read().await?,
                    Err(e) => return Err(e.into()),
                }
            }
        } else {
            io::Write::write_all(&mut self.conn.writer(), &[READY])?;
            self.flush().await?;
        }
        Ok(who)
    }
}

/// A session that stays TLS after its handshake: who the peer is, and
/// the session's keys and record numbers, to encrypt and decrypt what
/// follows (kernel TLS, `crate::ktls`).
pub struct Kept {
    pub peer: Peer,
    pub secrets: rustls::ExtractedSecrets,
}

async fn bounded_kept(mut s: Session<'_>, first: bool) -> Result<Kept, TlsError> {
    let peer = match timeout(HANDSHAKE_TIMEOUT, s.run_kept(first)).await {
        Ok(r) => r?,
        Err(_) => return Err(TlsError::TimedOut(HANDSHAKE_TIMEOUT)),
    };
    let secrets = s.conn.dangerous_extract_secrets()?;
    Ok(Kept { peer, secrets })
}

/// [`caller`] on a connection that stays TLS (D25; 30-data-path-tls.md
/// §33.9): the handshake, and the session's secrets.
pub async fn caller_kept(
    stream: &TcpStream,
    config: Arc<rustls::ClientConfig>,
) -> Result<Kept, TlsError> {
    let name = rustls::pki_types::ServerName::from(
        stream.peer_addr().map_err(|_| TlsError::BadName)?.ip(),
    );
    let conn = rustls::ClientConnection::new(config, name)?;
    bounded_kept(
        Session {
            stream,
            conn: conn.into(),
            peer_closed: false,
        },
        true,
    )
    .await
}

/// [`answerer`] on a connection that stays TLS.
pub async fn answerer_kept(
    stream: &TcpStream,
    config: Arc<rustls::ServerConfig>,
) -> Result<Kept, TlsError> {
    let conn = rustls::ServerConnection::new(config)?;
    bounded_kept(
        Session {
            stream,
            conn: conn.into(),
            peer_closed: false,
        },
        false,
    )
    .await
}

/// The caller's side, on a connection whose peer has just agreed to start
/// TLS: the handshake with `config` (`hyperstor_pki::data_client_config`),
/// then the turn back to plaintext. Returns who the peer is. On an error
/// the connection is no longer usable and must be closed.
pub async fn caller(
    stream: &TcpStream,
    config: Arc<rustls::ClientConfig>,
) -> Result<Peer, TlsError> {
    // The name is never checked (the peer's identity is, by the caller of
    // this function), but TLS wants one: the address dialled.
    let name = rustls::pki_types::ServerName::from(
        stream.peer_addr().map_err(|_| TlsError::BadName)?.ip(),
    );
    let conn = rustls::ClientConnection::new(config, name)?;
    bounded(
        Session {
            stream,
            conn: conn.into(),
            peer_closed: false,
        },
        true,
    )
    .await
}

/// The answering side, after it agreed to start TLS: the handshake with
/// `config` (`hyperstor_pki::data_server_config`), then the turn back to
/// plaintext. Returns who the caller is.
pub async fn answerer(
    stream: &TcpStream,
    config: Arc<rustls::ServerConfig>,
) -> Result<Peer, TlsError> {
    let conn = rustls::ServerConnection::new(config)?;
    bounded(
        Session {
            stream,
            conn: conn.into(),
            peer_closed: false,
        },
        false,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_pki::{Pem, Role};
    use hyperstor_reactor::net::TcpListener;
    use hyperstor_reactor::{block_on, spawn};

    struct Ca {
        root: Pem,
        ca: Pem,
    }

    fn ca(cluster: &str) -> Ca {
        let root = hyperstor_pki::new_root(cluster).unwrap();
        let ca = hyperstor_pki::new_intermediate(&root, cluster).unwrap();
        Ca { root, ca }
    }

    fn leaf(ca: &Ca, cluster: &str, role: Role, id: &str) -> Pem {
        let who = Identity {
            cluster: cluster.into(),
            role,
            id: id.into(),
        };
        hyperstor_pki::issue(&ca.ca, &who, &[], &[], 30).unwrap()
    }

    fn configs(
        ca: &Ca,
        client: &Pem,
        server: &Pem,
    ) -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
        (
            Arc::new(
                hyperstor_pki::data_client_config(&client.cert, &client.key, &ca.root.cert)
                    .unwrap(),
            ),
            Arc::new(
                hyperstor_pki::data_server_config(&server.cert, &server.key, &ca.root.cert)
                    .unwrap(),
            ),
        )
    }

    /// One connection as the protocol uses it: plaintext before, the
    /// handshake, plaintext after. `n` marks the bytes, so a byte that
    /// went astray shows.
    async fn exchange(
        addr: std::net::SocketAddr,
        config: Arc<rustls::ClientConfig>,
        n: u8,
    ) -> Result<Identity, TlsError> {
        let s = TcpStream::connect(addr).await?;
        let (r, _) = s.write_all(vec![n; 5]).await;
        r?;
        let (r, hello) = s.read_exact_n(5).await;
        r?;
        assert_eq!(hello, vec![n ^ 0xff; 5]);
        let who = caller(&s, config).await?.who;
        // Frames go on at once, several before any answer is read.
        for i in 0..3u8 {
            let (r, _) = s.write_all(vec![n.wrapping_add(i); 1000]).await;
            r?;
        }
        for i in 0..3u8 {
            let (r, back) = s.read_exact_n(1000).await;
            r?;
            assert_eq!(back, vec![n.wrapping_add(i) ^ 0xff; 1000], "frame {i}");
        }
        Ok(who)
    }

    async fn serve_one(
        s: TcpStream,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Identity, TlsError> {
        let (r, hello) = s.read_exact_n(5).await;
        r?;
        let (r, _) = s.write_all(hello.iter().map(|b| b ^ 0xff).collect()).await;
        r?;
        let who = answerer(&s, config).await?.who;
        for _ in 0..3 {
            let (r, frame) = s.read_exact_n(1000).await;
            r?;
            let (r, _) = s.write_all(frame.iter().map(|b| b ^ 0xff).collect()).await;
            r?;
        }
        Ok(who)
    }

    /// G67: after each side's `close_notify`, the next bytes are the
    /// frames that follow, whole and in order, on one connection and on
    /// 64 at once; and each end learns who the other is.
    #[test]
    fn plaintext_follows_the_handshake_with_no_byte_lost() {
        let c = ca("7");
        let (client, server) = configs(
            &c,
            &leaf(&c, "7", Role::Host, "hv-1"),
            &leaf(&c, "7", Role::Node, "3"),
        );
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let accept = spawn(async move {
                let mut served = Vec::new();
                for _ in 0..65 {
                    let (s, _) = l.accept().await.unwrap();
                    served.push(spawn(serve_one(s, server.clone())));
                }
                let mut who = Vec::new();
                for h in served {
                    who.push(h.await.unwrap());
                }
                who
            });
            let one = exchange(addr, client.clone(), 1).await.unwrap();
            assert_eq!(
                (one.role, one.id.as_str(), one.cluster.as_str()),
                (Role::Node, "3", "7")
            );
            let many: Vec<_> = (0..64u8)
                .map(|n| spawn(exchange(addr, client.clone(), n.wrapping_mul(3))))
                .collect();
            for h in many {
                assert_eq!(h.await.unwrap().id, "3");
            }
            let callers = accept.await;
            assert_eq!(callers.len(), 65);
            assert!(
                callers
                    .iter()
                    .all(|w| (w.role, w.id.as_str()) == (Role::Host, "hv-1"))
            );
        });
    }

    /// A peer of another cluster is refused by both ends, and so is a
    /// caller that stops mid-handshake, within the time allowed.
    #[test]
    fn another_clusters_certificate_is_refused() {
        let ours = ca("7");
        let theirs = ca("8");
        let (good_client, server) = configs(
            &ours,
            &leaf(&ours, "7", Role::Host, "hv-1"),
            &leaf(&ours, "7", Role::Node, "3"),
        );
        let stranger = leaf(&theirs, "8", Role::Host, "hv-9");
        // A stranger calling us: it trusts its own root, so it refuses
        // ours; with our root, we refuse its certificate.
        let lost = Arc::new(
            hyperstor_pki::data_client_config(&stranger.cert, &stranger.key, &theirs.root.cert)
                .unwrap(),
        );
        let forged = Arc::new(
            hyperstor_pki::data_client_config(&stranger.cert, &stranger.key, &ours.root.cert)
                .unwrap(),
        );
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let accept = spawn(async move {
                let mut out = Vec::new();
                for _ in 0..3 {
                    let (s, _) = l.accept().await.unwrap();
                    out.push(serve_one(s, server.clone()).await.is_ok());
                }
                out
            });
            assert!(exchange(addr, lost, 1).await.is_err());
            assert!(exchange(addr, forged, 2).await.is_err());
            assert!(exchange(addr, good_client, 3).await.is_ok());
            assert_eq!(accept.await, vec![false, false, true]);
        });
    }

    /// A minimal served connection, as nodes serve them: `Hello`,
    /// `StartTls` answered through the writer's queue, then pings. Returns
    /// who called, if it authenticated.
    async fn serve_frames(s: TcpStream, tls: crate::AnswererTls) -> Option<Identity> {
        use hyperstor_proto::{Code, Msg};
        let s = std::rc::Rc::new(s);
        let (tx, rx) = hyperstor_reactor::chan::channel::<crate::Frame>();
        spawn(crate::writer(s.clone(), rx));
        let mut who = None;
        loop {
            let Ok((h, msg, _)) = crate::read_frame(&s).await else {
                return who;
            };
            match msg {
                Msg::Hello { .. } => {
                    let ok = Msg::HelloOk {
                        version: hyperstor_proto::VERSION,
                        node: 3,
                        map_epoch: 1,
                    };
                    let _ = tx.send(crate::Frame::new(h.req, &ok, &[]));
                }
                Msg::StartTls | Msg::StartTlsOffer { .. } => {
                    let no = Msg::Error {
                        code: Code::Invalid,
                        map_epoch: 1,
                        detail: "no TLS here".into(),
                    };
                    let offer = match msg {
                        Msg::StartTlsOffer { encrypt } => Some(encrypt),
                        _ => None,
                    };
                    match crate::answer_start_tls(&s, &tx, h.req, &tls, no, offer).await {
                        Ok(w) => who = w.map(|p| p.who),
                        Err(_) => return who,
                    }
                }
                _ => {
                    let _ = tx.send(crate::Frame::new(h.req, &Msg::Pong, &[]));
                }
            }
        }
    }

    /// D373: a caller with an identity authenticates a peer that offers
    /// TLS and then uses the connection as before, many requests at
    /// once; a peer that refuses is used unauthenticated, unless the
    /// cluster requires TLS; a caller without an identity never asks.
    #[test]
    fn a_connection_authenticates_after_hello_when_both_can() {
        use hyperstor_proto::Msg;
        let c = ca("7");
        let (client, server) = configs(
            &c,
            &leaf(&c, "7", Role::Host, "hv-1"),
            &leaf(&c, "7", Role::Node, "3"),
        );
        let hello = Msg::Hello {
            min_version: hyperstor_proto::OLDEST_VERSION,
            max_version: hyperstor_proto::VERSION,
            id: 9,
            instance: 10,
        };
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            // Four connections: the first two served with TLS, the last
            // two by a peer that has none.
            let accept = spawn(async move {
                let mut served = Vec::new();
                for i in 0..4 {
                    let (s, _) = l.accept().await.unwrap();
                    let tls = (i < 2).then(|| crate::Answering {
                        config: server.clone(),
                        encrypt: false,
                        required: false,
                    });
                    served.push(spawn(serve_frames(s, tls)));
                }
                let mut who = Vec::new();
                for h in served {
                    who.push(h.await.map(|w| w.id));
                }
                who
            });
            let limit = Duration::from_secs(5);
            let allow = crate::CallerTls {
                config: client.clone(),
                required: false,
                encrypt: false,
            };
            let require = crate::CallerTls {
                config: client,
                required: true,
                encrypt: false,
            };
            let pings = async |conn: &crate::Conn| {
                let all: Vec<_> = (0..32).map(|_| conn.start(&Msg::Ping, &[])).collect();
                for p in all {
                    assert!(matches!(p.await.unwrap().0, Msg::Pong));
                }
            };

            // Authenticated, in `allow` and in `require`.
            for tls in [&allow, &require] {
                let (conn, answer) = crate::Conn::open(addr, limit, &hello, Some(tls))
                    .await
                    .unwrap();
                assert!(matches!(answer, Msg::HelloOk { node: 3, .. }));
                assert_eq!(conn.version(), hyperstor_proto::VERSION);
                let peer = conn.peer().unwrap();
                assert_eq!((peer.role, peer.id.as_str()), (Role::Node, "3"));
                pings(&conn).await;
                conn.kill();
            }
            // Refused by the peer: used as it is in `allow`.
            let (conn, _) = crate::Conn::open(addr, limit, &hello, Some(&allow))
                .await
                .unwrap();
            assert!(conn.peer().is_none());
            pings(&conn).await;
            conn.kill();
            // And an error in `require`.
            let e = crate::Conn::open(addr, limit, &hello, Some(&require))
                .await
                .err()
                .unwrap();
            assert!(e.to_string().contains("refused TLS"), "{e}");

            assert_eq!(
                accept.await,
                vec![
                    Some("hv-1".to_string()),
                    Some("hv-1".to_string()),
                    None,
                    None
                ]
            );
        });
    }

    /// D403: a session stays encrypted, in kernel TLS, when the caller
    /// offers to or the answerer's map says so; either way requests and
    /// answers of every size go through it. A session neither side wants
    /// encrypted turns back to plaintext.
    #[test]
    fn a_session_stays_encrypted_when_either_side_asks() {
        use hyperstor_proto::Msg;
        let c = ca("7");
        let (client, server) = configs(
            &c,
            &leaf(&c, "7", Role::Host, "hv-1"),
            &leaf(&c, "7", Role::Node, "3"),
        );
        let hello = Msg::Hello {
            min_version: hyperstor_proto::OLDEST_VERSION,
            max_version: hyperstor_proto::VERSION,
            id: 9,
            instance: 10,
        };
        // (caller asks, answerer's map says, encrypted)
        let cases = [
            (false, false, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ];
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let accept = spawn(async move {
                for (_, theirs, _) in cases {
                    let (s, _) = l.accept().await.unwrap();
                    let tls = Some(crate::Answering {
                        config: server.clone(),
                        encrypt: theirs,
                        required: false,
                    });
                    spawn(serve_frames(s, tls));
                }
            });
            for (ours, _, want) in cases {
                let tls = crate::CallerTls {
                    config: client.clone(),
                    required: true,
                    encrypt: ours,
                };
                let (conn, _) = crate::Conn::open(addr, Duration::from_secs(5), &hello, Some(&tls))
                    .await
                    .unwrap();
                assert_eq!(conn.encrypted(), want, "asked {ours}");
                assert_eq!(conn.peer().unwrap().id, "3");
                let all: Vec<_> = (0..32).map(|_| conn.start(&Msg::Ping, &[])).collect();
                for p in all {
                    assert!(matches!(p.await.unwrap().0, Msg::Pong));
                }
                // A large payload each way: the node answers a ping with
                // `Pong` whatever it carries.
                let big = vec![0xa5u8; 1 << 20];
                assert!(matches!(
                    conn.start(&Msg::Ping, &big).await.unwrap().0,
                    Msg::Pong
                ));
                conn.kill();
            }
            accept.await;
        });
    }

    /// D403: in `require`, an answerer whose map says a session must be
    /// encrypted refuses a caller that can only ask `StartTls`; in
    /// `allow` it serves it in plaintext, authenticated.
    #[test]
    fn a_caller_that_cant_encrypt_is_refused_only_in_require() {
        use hyperstor_proto::Msg;
        let c = ca("7");
        let (client, server) = configs(
            &c,
            &leaf(&c, "7", Role::Host, "hv-1"),
            &leaf(&c, "7", Role::Node, "3"),
        );
        // A caller of version 11: `StartTls` only.
        let hello = Msg::Hello {
            min_version: hyperstor_proto::OLDEST_VERSION,
            max_version: hyperstor_proto::ENCRYPT_VERSION - 1,
            id: 9,
            instance: 10,
        };
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            let accept = spawn(async move {
                let mut who = Vec::new();
                for required in [false, true] {
                    let (s, _) = l.accept().await.unwrap();
                    let tls = Some(crate::Answering {
                        config: server.clone(),
                        encrypt: true,
                        required,
                    });
                    who.push(spawn(serve_frames(s, tls)));
                }
                let mut out = Vec::new();
                for h in who {
                    out.push(h.await.map(|w| w.id));
                }
                out
            });
            let tls = crate::CallerTls {
                config: client,
                required: false,
                encrypt: false,
            };
            for _ in 0..2 {
                let (conn, _) =
                    crate::Conn::open_capped(addr, Duration::from_secs(5), &hello, Some(&tls), 11)
                        .await
                        .unwrap();
                assert!(!conn.encrypted());
                conn.kill();
            }
            assert_eq!(accept.await, vec![Some("hv-1".to_string()), None]);
        });
    }
}
