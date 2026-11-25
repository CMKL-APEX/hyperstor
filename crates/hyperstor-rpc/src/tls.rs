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
    async fn run(&mut self, first: bool) -> Result<Identity, TlsError> {
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
            .and_then(|leaf| hyperstor_pki::identity_of(leaf))
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

async fn bounded(mut s: Session<'_>, first: bool) -> Result<Identity, TlsError> {
    match timeout(HANDSHAKE_TIMEOUT, s.run(first)).await {
        Ok(r) => r,
        Err(_) => Err(TlsError::TimedOut(HANDSHAKE_TIMEOUT)),
    }
}

/// The caller's side, on a connection whose peer has just agreed to start
/// TLS: the handshake with `config` (`hyperstor_pki::data_client_config`),
/// then the turn back to plaintext. Returns who the peer is. On an error
/// the connection is no longer usable and must be closed.
pub async fn caller(
    stream: &TcpStream,
    config: Arc<rustls::ClientConfig>,
) -> Result<Identity, TlsError> {
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
) -> Result<Identity, TlsError> {
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
        let who = caller(&s, config).await?;
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
        let who = answerer(&s, config).await?;
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
                Msg::StartTls => {
                    let no = Msg::Error {
                        code: Code::Invalid,
                        map_epoch: 1,
                        detail: "no TLS here".into(),
                    };
                    match crate::answer_start_tls(&s, &tx, h.req, &tls, no).await {
                        Ok(w) => who = w,
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
                    let tls = (i < 2).then(|| server.clone());
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
            };
            let require = crate::CallerTls {
                config: client,
                required: true,
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
}
