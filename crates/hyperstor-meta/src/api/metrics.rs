//! The map service's metrics (28-scrub.md §31.8, 04-decisions.md D234,
//! D235): `GET /metrics` on the cluster API's port, beside the RPCs and
//! behind the same TLS, for a certificate of role `metrics` (or any that
//! may read). Every member answers from its own replicated copy of the
//! scrub records, and says whether it leads.
//!
//! connectrpc's server takes only its own service, so the API's TCP port
//! has an accept loop of its own here, as the local socket has: TLS with
//! the handshake bounded, then HTTP/1 or HTTP/2, each request carrying
//! the peer's address and certificates as connectrpc's server gives them.

use super::{Api, Need, pool_scrub};
use crate::Admin;
use connectrpc::{ConnectRpcBody, ConnectRpcService, RequestContext};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_reactor::chan::oneshot;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

/// How long a client may take over its TLS handshake, and to send a
/// request's headers (connectrpc's own bounds).
const HANDSHAKE: Duration = Duration::from_secs(10);
const HEADERS: Duration = Duration::from_secs(30);

/// A label value as Prometheus' text format quotes it.
fn label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// The text Prometheus reads: whether this member leads, and each
/// scrubbed pool's oldest light and deep scrub and its PGs overdue.
pub(super) fn render(pools: &[pb::PoolScrub], leader: bool) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "# HELP hyperstor_meta_leader Whether this map service member leads.\n\
         # TYPE hyperstor_meta_leader gauge\n\
         hyperstor_meta_leader {}",
        u8::from(leader)
    );
    let _ = writeln!(
        o,
        "# HELP hyperstor_pool_scrub_oldest_seconds Seconds since the pool's PG scrubbed longest ago was last scrubbed, light (a deep scrub counts) or deep.\n\
         # TYPE hyperstor_pool_scrub_oldest_seconds gauge"
    );
    for p in pools {
        let (id, name) = (p.pool, label(&p.name));
        let _ = writeln!(
            o,
            "hyperstor_pool_scrub_oldest_seconds{{pool=\"{id}\",name=\"{name}\",kind=\"light\"}} {}\n\
             hyperstor_pool_scrub_oldest_seconds{{pool=\"{id}\",name=\"{name}\",kind=\"deep\"}} {}",
            p.oldest_light_secs, p.oldest_deep_secs
        );
    }
    let _ = writeln!(
        o,
        "# HELP hyperstor_pool_scrub_overdue_pgs PGs with no light scrub in twice the interval, or no deep scrub in twice the deep interval.\n\
         # TYPE hyperstor_pool_scrub_overdue_pgs gauge"
    );
    for p in pools {
        let _ = writeln!(
            o,
            "hyperstor_pool_scrub_overdue_pgs{{pool=\"{}\",name=\"{}\"}} {}",
            p.pool,
            label(&p.name),
            p.overdue
        );
    }
    o
}

fn text(status: http::StatusCode, body: String) -> http::Response<ConnectRpcBody> {
    let mut r = http::Response::new(ConnectRpcBody::Full(http_body_util::Full::new(
        bytes::Bytes::from(body),
    )));
    *r.status_mut() = status;
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    r
}

impl Api {
    async fn leads(&self) -> bool {
        let (reply, rx) = oneshot();
        self.tx.send(Admin::Leads(reply)).is_ok() && matches!(rx.recv().await, Some(Ok(())))
    }

    /// `GET /metrics`, for the caller the request's extensions name.
    async fn metrics(
        &self,
        headers: http::HeaderMap,
        extensions: http::Extensions,
    ) -> http::Response<ConnectRpcBody> {
        let ctx = RequestContext::new(headers).with_extensions(extensions);
        if let Err(e) = self.authorize_as(&ctx, "GetMetrics", "metrics", Need::Metrics) {
            let status = match e.code {
                connectrpc::ErrorCode::Unauthenticated => http::StatusCode::UNAUTHORIZED,
                connectrpc::ErrorCode::ResourceExhausted => http::StatusCode::TOO_MANY_REQUESTS,
                _ => http::StatusCode::FORBIDDEN,
            };
            return text(status, format!("{}\n", e.message.unwrap_or_default()));
        }
        let (map, records) = match (self.published().await, self.scrub_records().await) {
            (Ok((map, ..)), Ok(r)) => (map, r),
            (Err(e), _) | (_, Err(e)) => {
                return text(
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    format!("{}\n", e.message.unwrap_or_default()),
                );
            }
        };
        let now = crate::unix_now();
        let pools: Vec<pb::PoolScrub> = map
            .pools
            .values()
            .filter(|p| p.kind != crate::PoolKind::Cache)
            .map(|p| pool_scrub(&map, p, &records, now))
            .collect();
        text(http::StatusCode::OK, render(&pools, self.leads().await))
    }
}

/// The API on TCP: `GET /metrics`, and every other request to the RPCs.
pub(super) async fn serve(
    listener: tokio::net::TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    service: ConnectRpcService,
    api: Arc<Api>,
) {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                // Out of descriptors, say: wait rather than spin.
                eprintln!("hyperstor-meta: cluster API: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let (acceptor, service, api) = (acceptor.clone(), service.clone(), api.clone());
        tokio::spawn(async move {
            match acceptor {
                None => connection(stream, addr, None, service, api).await,
                Some(a) => {
                    let Ok(Ok(s)) = tokio::time::timeout(HANDSHAKE, a.accept(stream)).await else {
                        return;
                    };
                    let certs = s.get_ref().1.peer_certificates().map(|c| -> Arc<[_]> {
                        c.iter().map(|x| x.clone().into_owned()).collect()
                    });
                    connection(s, addr, certs, service, api).await
                }
            }
        });
    }
}

async fn connection<S>(
    io: S,
    addr: std::net::SocketAddr,
    certs: Option<Arc<[rustls::pki_types::CertificateDer<'static>]>>,
    service: ConnectRpcService,
    api: Arc<Api>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tower::Service;
    let svc = hyper::service::service_fn(move |mut req: http::Request<hyper::body::Incoming>| {
        req.extensions_mut().insert(connectrpc::PeerAddr(addr));
        if let Some(c) = &certs {
            req.extensions_mut()
                .insert(connectrpc::PeerCerts(c.clone()));
        }
        let mut service = service.clone();
        let api = api.clone();
        async move {
            if req.method() == http::Method::GET && req.uri().path() == "/metrics" {
                let (parts, _) = req.into_parts();
                return Ok(api.metrics(parts.headers, parts.extensions).await);
            }
            service.call(req).await
        }
    });
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADERS);
    let _ = builder
        .serve_connection(hyper_util::rt::TokioIo::new(io), svc)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_name_each_pool_and_the_leader() {
        let pools = [pb::PoolScrub {
            pool: 1,
            name: "vol\"a".into(),
            oldest_light_secs: 120,
            oldest_deep_secs: 86_400,
            overdue: 3,
            ..Default::default()
        }];
        let t = render(&pools, true);
        assert!(t.contains("hyperstor_meta_leader 1\n"), "{t}");
        assert!(
            t.contains(
                "hyperstor_pool_scrub_oldest_seconds{pool=\"1\",name=\"vol\\\"a\",kind=\"light\"} 120\n"
            ) && t.contains(
                "hyperstor_pool_scrub_oldest_seconds{pool=\"1\",name=\"vol\\\"a\",kind=\"deep\"} 86400\n"
            ) && t.contains("hyperstor_pool_scrub_overdue_pgs{pool=\"1\",name=\"vol\\\"a\"} 3\n"),
            "{t}"
        );
        // Every line a comment or a sample, as Prometheus parses them.
        assert!(
            t.lines()
                .all(|l| l.starts_with('#') || l.starts_with("hyperstor_"))
        );
        assert!(render(&[], false).contains("hyperstor_meta_leader 0\n"));
    }
}
