//! Calls to the cluster API on a person's behalf (spec/administration.md
//! §23.7): once [`crate::policy`] allows the action, the request goes to a
//! map service member as it came (Connect, gRPC-Web or gRPC, JSON or
//! protobuf), over mutual TLS as mgmt's own identity, and the answer comes
//! back as it is. A member that is down or isn't the leader (`503`) is
//! passed over for the next.

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// An answer, at most this large (a map backup is the largest).
const MAX_ANSWER: usize = 256 << 20;

pub struct Answer {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

/// Headers a call carries through: what the protocols define.
fn passes(name: &http::HeaderName) -> bool {
    let n = name.as_str();
    n == "content-type"
        || n == "accept"
        || n.starts_with("connect-")
        || n.starts_with("grpc-")
        || n == "te"
        || n == "x-grpc-web"
        || n == "x-user-agent"
}

async fn once(
    tls: &Arc<connectrpc::rustls::ClientConfig>,
    addr: &str,
    path: &str,
    headers: &http::HeaderMap,
    body: Bytes,
) -> Result<Answer, String> {
    let sock: SocketAddr = addr
        .parse()
        .map_err(|_| format!("{addr}: not an address"))?;
    let tcp = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(sock))
        .await
        .map_err(|_| format!("{addr}: no answer"))?
        .map_err(|e| format!("{addr}: {e}"))?;
    let name = rustls::pki_types::ServerName::IpAddress(sock.ip().into());
    let stream = tokio_rustls::TlsConnector::from(tls.clone())
        .connect(name, tcp)
        .await
        .map_err(|e| format!("{addr}: TLS: {e}"))?;
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| format!("{addr}: {e}"))?;
    tokio::spawn(conn);
    let mut req = http::Request::post(format!("https://{addr}{path}"))
        .header(http::header::HOST, addr)
        .body(Full::new(body))
        .map_err(|e| e.to_string())?;
    for (k, v) in headers.iter() {
        req.headers_mut().append(k.clone(), v.clone());
    }
    let resp = tokio::time::timeout(Duration::from_secs(120), send.send_request(req))
        .await
        .map_err(|_| format!("{addr}: no answer in 120 s"))?
        .map_err(|e| format!("{addr}: {e}"))?;
    let (parts, body) = resp.into_parts();
    let body = Limited::new(body, MAX_ANSWER)
        .collect()
        .await
        .map_err(|e| format!("{addr}: {e}"))?
        .to_bytes();
    Ok(Answer {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body,
    })
}

/// Sends the call `path` (`/hyperstor.v1.<Service>/<Method>`) to each of
/// `addrs` in turn until one answers other than `503`.
/// The map service is told the call is for `person` (`user:NAME`,
/// [`hyperstor_api::ON_BEHALF_HEADER`]); a caller's own such header never
/// passes ([`passes`]).
pub async fn forward(
    tls: &Arc<connectrpc::rustls::ClientConfig>,
    addrs: &[String],
    path: &str,
    headers: &http::HeaderMap,
    person: &str,
    body: Bytes,
) -> Result<Answer, String> {
    let mut headers: http::HeaderMap = headers
        .iter()
        .filter(|(k, _)| passes(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    headers.insert(
        http::HeaderName::from_static(hyperstor_api::ON_BEHALF_HEADER),
        http::HeaderValue::from_str(person).map_err(|e| e.to_string())?,
    );
    let mut last = Err("no map service member configured".to_string());
    for a in addrs {
        last = once(tls, a, path, &headers, body.clone()).await;
        match &last {
            Ok(r) if r.status != 503 => return last,
            _ => {}
        }
    }
    last
}
