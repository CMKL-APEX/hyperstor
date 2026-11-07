//! The gateway (14-s3.md §14.2, stage 2 of §14.7): S3's HTTP API,
//! path-style, over TLS on hyper and tokio (D195). Each request is
//! authenticated (SigV4, in a header or a presigned URL) and handed to
//! one of the reactor threads, each with its own client of the cluster
//! and its own [`S3`]. Bodies stream both ways through bounded channels,
//! so neither side holds an object whole.
//!
//! - **Tokio's side:** TLS, HTTP, signatures, `aws-chunked` bodies and
//!   the hashes a body is checked against ([`crate::chunked`]).
//! - **A reactor's side:** the object layer; MD5 and CRC32C as the
//!   bytes go by, and the chunks.
//! - A PUT is made visible only once tokio's side has seen the whole
//!   body and found it good: until then the reactor holds the upload
//!   open, and gives it up if the body is bad.
//! - **Keys and placements** (D231, D232) come from the map service:
//!   fetched over mutual TLS with the gateway's `s3` certificate every few
//!   seconds, so a revoked key is refused within seconds, and kept while
//!   no member answers ([`Source::Api`]). Without the cluster API (tests),
//!   they are fixed at start ([`Source::Fixed`]).
//! - Object names and bodies are user data: never logged.

use crate::chunked::{Algorithm, BodyError, Checks, Chunked};
use crate::keys::Keys;
use crate::sigv4::{self, AuthError, ChunkSigner, Payload};
use crate::xml::Xml;
use crate::{Bucket, Condition, Config, Entry, List, Meta, S3, S3Error, dates, past_prefix};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Frame, Incoming};
use hyperstor_client::Client;
use hyperstor_kv::KvError;
use md5::Digest as _;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// S3's limit on a single PUT.
pub const MAX_PUT: u64 = 5 << 30;
/// S3's limit on user metadata, names and values together.
const MAX_META: usize = 2 << 10;
/// Bytes a PUT's body is gathered into before it goes to the reactor.
const PIECE: usize = 1 << 20;

pub struct Options {
    /// The map service's members.
    pub meta: Vec<SocketAddr>,
    pub networks: Vec<String>,
    pub listen: Vec<SocketAddr>,
    pub tls: Arc<rustls::ServerConfig>,
    /// Where keys and placements come from.
    pub source: Source,
    /// Reactor threads.
    pub reactors: usize,
    /// Tokio's threads.
    pub threads: usize,
    pub config: Config,
    /// How often expired uploads are cleaned up (14-s3.md §14.6).
    pub clean_every: Duration,
}

/// Where the access keys and placements come from (D231, D232).
pub enum Source {
    /// Fixed at start: keys from a file, and new buckets in two pools,
    /// the `default` placement. For running without the cluster API.
    Fixed {
        keys: Keys,
        data_pool: String,
        index_pool: String,
    },
    /// From the cluster API (`S3Service.GetS3Gateway`) at these addresses,
    /// tried in turn, every `every`, over mutual TLS with `tls` (the
    /// gateway's `s3` identity): the keys of `realm`'s tenants and its
    /// placements, another realm's keys unknown here (D250).
    Api {
        addrs: Vec<String>,
        tls: Arc<rustls::ClientConfig>,
        every: Duration,
        realm: String,
    },
}

/// A placement's pool: by id from the cluster API, by name when fixed at
/// start.
#[derive(Debug, Clone)]
enum Pool {
    Id(u32),
    Name(String),
}

/// What a gateway serves with: keys, and placements by name (data pool,
/// index pool).
struct Served {
    keys: Keys,
    placements: std::collections::HashMap<String, (Pool, Pool)>,
}

/// The placement CreateBucket uses unless its LocationConstraint names
/// another (D232).
const DEFAULT_PLACEMENT: &str = "default";

/// One answer of the cluster API, or why none came.
async fn fetch(
    addrs: &[String],
    tls: &Arc<rustls::ClientConfig>,
    realm: &str,
) -> Result<Served, String> {
    use hyperstor_api::proto::hyperstor::v1 as pb;
    let mut why = Vec::new();
    for a in addrs {
        let Ok(uri) = format!("https://{a}").parse::<http::Uri>() else {
            why.push(format!("{a}: not an address"));
            continue;
        };
        let client = connectrpc::client::HttpClient::with_tls(tls.clone());
        let config =
            connectrpc::client::ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5));
        match hyperstor_api::S3ServiceClient::new(client, config)
            .get_s3_gateway(pb::GetS3GatewayRequest {
                realm: realm.to_string(),
                ..Default::default()
            })
            .await
        {
            Ok(r) => {
                let r = r.into_owned();
                let keys = Keys::from_keys(
                    r.keys
                        .into_iter()
                        .map(|k| (k.id, zeroize::Zeroizing::new(k.secret), k.tenant)),
                );
                let placements = r
                    .placements
                    .into_iter()
                    .map(|p| {
                        (
                            p.name,
                            (Pool::Id(p.data_pool_id), Pool::Id(p.index_pool_id)),
                        )
                    })
                    .collect();
                return Ok(Served { keys, placements });
            }
            Err(e) => why.push(format!(
                "{a}: {}",
                e.message.as_deref().unwrap_or("no answer")
            )),
        }
    }
    Err(why.join("; "))
}

/// Fetches every `every` and swaps in each answer; logs when the counts
/// change or fetching starts or stops failing (never a key's secret).
async fn refresh(
    state: Arc<State>,
    addrs: Vec<String>,
    tls: Arc<rustls::ClientConfig>,
    every: Duration,
    realm: String,
) {
    let mut failing = false;
    loop {
        tokio::time::sleep(every).await;
        match fetch(&addrs, &tls, &realm).await {
            Ok(s) => {
                let (k, p) = (s.keys.len(), s.placements.len());
                let old = state.swap(s);
                if failing || (old.keys.len(), old.placements.len()) != (k, p) {
                    eprintln!("hyperstor-s3: {k} access keys, {p} placements");
                }
                failing = false;
            }
            Err(e) if !failing => {
                eprintln!(
                    "hyperstor-s3: keys from the cluster API: {e}; serving with the last ones"
                );
                failing = true;
            }
            Err(_) => {}
        }
    }
}

type Body = http_body_util::combinators::BoxBody<Bytes, io::Error>;
type Task = Box<dyn FnOnce(Rc<S3>) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// The reactor threads, taking tasks in turn.
struct Reactors {
    tx: Vec<hyperstor_reactor::chan::Sender<Task>>,
    next: AtomicUsize,
}

impl Reactors {
    /// Starts `n` reactor threads, each connected to the cluster.
    fn start(o: &Options) -> Result<Reactors, String> {
        let mut tx = Vec::new();
        for _ in 0..o.reactors.max(1) {
            let (t, mut rx) = hyperstor_reactor::chan::channel::<Task>();
            let (ready, up) = std::sync::mpsc::channel();
            let (meta, networks, cfg) = (o.meta.clone(), o.networks.clone(), o.config);
            std::thread::spawn(move || {
                hyperstor_reactor::block_on(async move {
                    let id = hyperstor_client::random_u64() | 1;
                    let client = match Client::connect_members(meta, id).await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = ready.send(Err(format!("map service: {e}")));
                            return;
                        }
                    };
                    client.set_networks(networks);
                    let _ = ready.send(Ok(()));
                    let s3 = Rc::new(S3::open(client, cfg));
                    while let Some(task) = rx.recv().await {
                        hyperstor_reactor::spawn(task(s3.clone()));
                    }
                })
            });
            up.recv()
                .map_err(|_| "a reactor thread stopped".to_string())??;
            tx.push(t);
        }
        Ok(Reactors {
            tx,
            next: AtomicUsize::new(0),
        })
    }

    fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Rc<S3>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.tx.len();
        let _ = self.tx[i].send(Box::new(move |s3| Box::pin(f(s3))));
    }

    /// Runs `f` on a reactor and waits for its result.
    async fn call<T, F, Fut>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(Rc<S3>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.spawn(move |s3| async move {
            let _ = tx.send(f(s3).await);
        });
        rx.await
            .map_err(|_| ApiError::new(503, "ServiceUnavailable", "the gateway is stopping"))
    }
}

/// An error as S3 answers it.
#[derive(Debug)]
pub struct ApiError {
    status: u16,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    fn not_implemented(what: impl std::fmt::Display) -> ApiError {
        ApiError::new(501, "NotImplemented", format!("{what} isn't implemented"))
    }

    fn response(&self, resource: &str, id: &str, head: bool) -> Response<Body> {
        let mut x = Xml::bare("Error");
        x.elem("Code", self.code)
            .elem("Message", &self.message)
            .elem("Resource", resource)
            .elem("RequestId", id);
        let body = if head {
            empty()
        } else {
            full(x.finish("Error"))
        };
        let mut r = Response::new(body);
        *r.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        r.headers_mut().insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml"),
        );
        r
    }
}

impl From<S3Error> for ApiError {
    fn from(e: S3Error) -> ApiError {
        let m = e.to_string();
        match e {
            S3Error::NoSuchBucket(_) => ApiError::new(404, "NoSuchBucket", m),
            // Only ever the tenant's own (D244).
            S3Error::BucketExists(_) => ApiError::new(409, "BucketAlreadyOwnedByYou", m),
            S3Error::BucketNotEmpty(_) => ApiError::new(409, "BucketNotEmpty", m),
            S3Error::InvalidBucketName(_) => ApiError::new(400, "InvalidBucketName", m),
            S3Error::InvalidKey => ApiError::new(400, "KeyTooLongError", m),
            S3Error::BadSize { .. } => ApiError::new(400, "IncompleteBody", m),
            S3Error::InvalidRange => ApiError::new(416, "InvalidRange", m),
            S3Error::UploadGone | S3Error::Changed => ApiError::new(500, "InternalError", m),
            S3Error::PreconditionFailed => precondition_failed(),
            S3Error::Kv(KvError::Conflict | KvError::Exhausted(..)) => {
                ApiError::new(503, "SlowDown", "the object kept changing; try again")
            }
            // Its text may name the cluster's hosts: logged, not sent.
            e => {
                eprintln!("hyperstor-s3: {e}");
                ApiError::new(
                    503,
                    "ServiceUnavailable",
                    "the cluster didn't answer; try again",
                )
            }
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> ApiError {
        match e {
            AuthError::Missing => ApiError::new(403, "AccessDenied", "the request isn't signed"),
            AuthError::Malformed(w) => ApiError::new(
                400,
                "AuthorizationHeaderMalformed",
                format!("bad signature: {w}"),
            ),
            AuthError::UnknownKey => {
                ApiError::new(403, "InvalidAccessKeyId", "the access key doesn't exist")
            }
            AuthError::Mismatch => ApiError::new(
                403,
                "SignatureDoesNotMatch",
                "the signature doesn't match the request and key",
            ),
            AuthError::Skewed => ApiError::new(
                403,
                "RequestTimeTooSkewed",
                "the request's time is too far from the gateway's",
            ),
            AuthError::Expired => ApiError::new(403, "AccessDenied", "the request has expired"),
            AuthError::Unsupported(w) => ApiError::not_implemented(w),
        }
    }
}

impl From<BodyError> for ApiError {
    fn from(e: BodyError) -> ApiError {
        match e {
            BodyError::Malformed(w) => {
                ApiError::new(400, "InvalidRequest", format!("bad body: {w}"))
            }
            BodyError::Incomplete => ApiError::new(400, "IncompleteBody", "the body ended early"),
            BodyError::Signature => ApiError::new(
                403,
                "SignatureDoesNotMatch",
                "a chunk's signature doesn't match",
            ),
            BodyError::Sha256 => ApiError::new(
                400,
                "XAmzContentSHA256Mismatch",
                "the body's SHA-256 doesn't match x-amz-content-sha256",
            ),
            BodyError::Checksum(a) => ApiError::new(
                400,
                "BadDigest",
                format!("the {a} you specified did not match the calculated checksum"),
            ),
            BodyError::Unsupported(w) => ApiError::not_implemented(w),
        }
    }
}

fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|e| match e {}).boxed()
}

fn empty() -> Body {
    full(Bytes::new())
}

/// A GET's body, from the reactor that reads it.
struct Streamed(mpsc::Receiver<io::Result<Bytes>>);

impl hyper::body::Body for Streamed {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.0.poll_recv(cx).map(|o| o.map(|r| r.map(Frame::data)))
    }
}

struct State {
    reactors: Reactors,
    served: std::sync::RwLock<Arc<Served>>,
}

impl State {
    fn served(&self) -> Arc<Served> {
        self.served
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn swap(&self, s: Served) -> Arc<Served> {
        std::mem::replace(
            &mut *self.served.write().unwrap_or_else(|e| e.into_inner()),
            Arc::new(s),
        )
    }
}

/// Serves until `stop` receives (or is dropped). Calls `ready` with the
/// addresses it listens on.
pub fn serve(
    o: Options,
    ready: impl FnOnce(&[SocketAddr]),
    stop: std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(o.threads.max(1))
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    let reactors = Reactors::start(&o)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(o.tls.clone());
    rt.block_on(async move {
        let (served, api) = match o.source {
            Source::Fixed {
                keys,
                data_pool,
                index_pool,
            } => {
                let placements = std::collections::HashMap::from([(
                    DEFAULT_PLACEMENT.to_string(),
                    (Pool::Name(data_pool), Pool::Name(index_pool)),
                )]);
                (Served { keys, placements }, None)
            }
            Source::Api {
                addrs,
                tls,
                every,
                realm,
            } => {
                // Nothing to serve with until the first answer.
                let mut said = false;
                let served = loop {
                    match fetch(&addrs, &tls, &realm).await {
                        Ok(s) => break s,
                        Err(e) => {
                            if !said {
                                eprintln!(
                                    "hyperstor-s3: keys from the cluster API: {e}; trying again"
                                );
                                said = true;
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                };
                eprintln!(
                    "hyperstor-s3: {} access keys, {} placements",
                    served.keys.len(),
                    served.placements.len()
                );
                (served, Some((addrs, tls, every, realm)))
            }
        };
        let state = Arc::new(State {
            reactors,
            served: std::sync::RwLock::new(Arc::new(served)),
        });
        if let Some((addrs, tls, every, realm)) = api {
            tokio::spawn(refresh(state.clone(), addrs, tls, every, realm));
        }
        let mut bound = Vec::new();
        let mut listeners = Vec::new();
        for a in &o.listen {
            let l = tokio::net::TcpListener::bind(a)
                .await
                .map_err(|e| format!("{a}: {e}"))?;
            bound.push(l.local_addr().map_err(|e| e.to_string())?);
            listeners.push(l);
        }
        ready(&bound);
        for l in listeners {
            tokio::spawn(accept(l, state.clone(), acceptor.clone()));
        }
        tokio::spawn(clean(state.clone(), o.clean_every));
        let _ = tokio::task::spawn_blocking(move || stop.recv()).await;
        Ok(())
    })
}

async fn accept(
    l: tokio::net::TcpListener,
    state: Arc<State>,
    acceptor: tokio_rustls::TlsAcceptor,
) {
    loop {
        let Ok((stream, _)) = l.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let (state, acceptor) = (state.clone(), acceptor.clone());
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(stream).await else {
                return;
            };
            let svc = hyper::service::service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(&state, req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                .await;
        });
    }
}

/// Gives up expired uploads in every bucket, every `every`, starting at a
/// random point in the first period so gateways spread out.
async fn clean(state: Arc<State>, every: Duration) {
    let first =
        every.mul_f64(f64::from(hyperstor_client::random_u64() as u32) / f64::from(u32::MAX));
    tokio::time::sleep(first).await;
    loop {
        let r = state
            .reactors
            .call(|s3| async move {
                let now = crate::now_secs();
                let mut n = 0;
                for b in s3.all_buckets().await? {
                    n += s3.clean(&b, now).await?;
                }
                Ok::<_, S3Error>(n)
            })
            .await;
        match r {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => eprintln!("hyperstor-s3: gave up {n} expired uploads"),
            Ok(Err(e)) => eprintln!("hyperstor-s3: cleaning up uploads: {e}"),
            Err(_) => return,
        }
        tokio::time::sleep(every).await;
    }
}

fn request_id() -> String {
    format!("{:016X}", hyperstor_client::random_u64())
}

async fn handle(state: &State, req: http::Request<Incoming>) -> Response<Body> {
    let id = request_id();
    let head = req.method() == Method::HEAD;
    let resource = req.uri().path().to_string();
    let mut r = match route(state, req).await {
        Ok(r) => r,
        Err(e) => e.response(&resource, &id, head),
    };
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&id) {
        h.insert("x-amz-request-id", v);
    }
    h.insert(http::header::SERVER, HeaderValue::from_static("hyperstor"));
    r
}

/// What a path names.
enum Target {
    Service,
    Bucket(String),
    Object(String, Vec<u8>),
}

fn target(path: &str) -> Result<Target, ApiError> {
    let bad = || ApiError::new(400, "InvalidURI", "the path couldn't be decoded");
    let p = path.strip_prefix('/').ok_or_else(bad)?;
    let (b, k) = match p.split_once('/') {
        Some((b, k)) => (b, Some(k)),
        None => (p, None),
    };
    if b.is_empty() {
        return Ok(Target::Service);
    }
    let b = String::from_utf8(sigv4::percent_decode(b.as_bytes()).ok_or_else(bad)?)
        .map_err(|_| bad())?;
    match k {
        None | Some("") => Ok(Target::Bucket(b)),
        Some(k) => {
            let k = sigv4::percent_decode(k.as_bytes()).ok_or_else(bad)?;
            if std::str::from_utf8(&k).is_err() {
                return Err(ApiError::new(400, "InvalidURI", "object names are UTF-8"));
            }
            Ok(Target::Object(b, k))
        }
    }
}

use sigv4::Pairs;

fn param<'a>(q: &'a Pairs, name: &str) -> Option<&'a [u8]> {
    q.iter()
        .find(|(k, _)| k == name.as_bytes())
        .map(|(_, v)| v.as_slice())
}

fn text_param(q: &Pairs, name: &str) -> Result<Option<String>, ApiError> {
    param(q, name)
        .map(|v| {
            String::from_utf8(v.to_vec())
                .map_err(|_| ApiError::new(400, "InvalidArgument", format!("{name} isn't UTF-8")))
        })
        .transpose()
}

/// Refuses parameters other than `allowed`: each names something not
/// built (a subresource such as `?tagging`, or `?uploadId`).
fn only(q: &Pairs, allowed: &[&str]) -> Result<(), ApiError> {
    for (k, _) in q {
        let k = String::from_utf8_lossy(k);
        if !allowed.contains(&k.as_ref()) && !k.starts_with("X-Amz-") && k != "x-id" {
            return Err(ApiError::not_implemented(format!("?{k}")));
        }
    }
    Ok(())
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

async fn route(state: &State, req: http::Request<Incoming>) -> Result<Response<Body>, ApiError> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let q = sigv4::query_pairs(&query)
        .ok_or_else(|| ApiError::new(400, "InvalidURI", "the query couldn't be decoded"))?;
    let signed = sigv4::parse(&query, req.headers())?;
    let served = state.served();
    let key = served.keys.get(&signed.key).ok_or(AuthError::UnknownKey)?;
    let signing = sigv4::verify(
        &signed,
        &key.secret,
        req.method().as_str(),
        &path,
        &query,
        req.headers(),
        crate::now_secs(),
    )?;
    let tenant = key.tenant;
    drop(served);
    let m = req.method().clone();
    match (target(&path)?, m) {
        (Target::Service, Method::GET) => {
            only(&q, &[])?;
            list_buckets(state, tenant).await
        }
        (Target::Service, _) => Err(method_not_allowed()),
        (Target::Bucket(b), Method::GET) if param(&q, "location").is_some() => {
            only(&q, &["location"])?;
            state.bucket(&b, tenant).await?;
            Ok(xml_ok(
                Xml::new("LocationConstraint").finish("LocationConstraint"),
            ))
        }
        (Target::Bucket(b), Method::GET) => list_objects(state, &b, tenant, &q).await,
        (Target::Bucket(b), Method::PUT) => {
            only(&q, &[])?;
            let body = small_body(signed.clone(), signing, req.into_body()).await?;
            create_bucket(state, &b, tenant, &placement_of(&body, &signed.region)?).await
        }
        (Target::Bucket(b), Method::HEAD) => {
            only(&q, &[])?;
            state.bucket(&b, tenant).await?;
            Ok(Response::new(empty()))
        }
        (Target::Bucket(b), Method::DELETE) => {
            only(&q, &[])?;
            state
                .reactors
                .call(move |s3| async move { s3.delete_bucket(tenant, &b).await })
                .await??;
            Ok(status(204))
        }
        (Target::Bucket(b), Method::POST) if param(&q, "delete").is_some() => {
            only(&q, &["delete"])?;
            let h = req.headers().clone();
            let body = checked_body(signed, signing, &h, req.into_body(), DELETE_BODY).await?;
            delete_objects(state, &b, tenant, &body).await
        }
        (Target::Bucket(_), Method::POST) => {
            only(&q, &[])?;
            Err(method_not_allowed())
        }
        (Target::Bucket(_), _) => Err(method_not_allowed()),
        (Target::Object(b, k), Method::GET | Method::HEAD) => {
            only(&q, RESPONSE_OVERRIDES)?;
            get_object(
                state,
                &b,
                k,
                tenant,
                &q,
                req.method() == Method::HEAD,
                req.headers(),
            )
            .await
        }
        (Target::Object(b, k), Method::PUT) => {
            only(&q, &[])?;
            put_object(state, &b, k, tenant, signed, signing, req).await
        }
        (Target::Object(b, k), Method::DELETE) => {
            only(&q, &[])?;
            let cond = match header(req.headers(), "if-match") {
                None => Condition::Any,
                Some(v) => if_match(v),
            };
            let bucket = state.bucket(&b, tenant).await?;
            state
                .reactors
                .call(move |s3| async move { s3.delete_if(&bucket, &k, &cond).await })
                .await??;
            Ok(status(204))
        }
        (Target::Object(..), Method::POST) => {
            only(&q, &[])?;
            Err(method_not_allowed())
        }
        (Target::Object(..), _) => Err(method_not_allowed()),
    }
}

fn precondition_failed() -> ApiError {
    ApiError::new(
        412,
        "PreconditionFailed",
        "at least one of the preconditions you specified did not hold",
    )
}

fn method_not_allowed() -> ApiError {
    ApiError::new(
        405,
        "MethodNotAllowed",
        "the method isn't allowed on this resource",
    )
}

fn status(s: u16) -> Response<Body> {
    let mut r = Response::new(empty());
    *r.status_mut() = StatusCode::from_u16(s).unwrap_or(StatusCode::OK);
    r
}

fn xml_ok(text: String) -> Response<Body> {
    let mut r = Response::new(full(text));
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    r
}

impl State {
    /// `tenant`'s bucket `name`; another tenant's don't exist to it (D244).
    async fn bucket(&self, name: &str, tenant: u32) -> Result<Bucket, ApiError> {
        let n = name.to_string();
        Ok(self
            .reactors
            .call(move |s3| async move { s3.bucket(tenant, &n).await })
            .await??)
    }
}

async fn list_buckets(state: &State, tenant: u32) -> Result<Response<Body>, ApiError> {
    let buckets = state
        .reactors
        .call(move |s3| async move { s3.buckets(tenant).await })
        .await??;
    let mut x = Xml::new("ListAllMyBucketsResult");
    x.open("Owner")
        .elem("ID", &tenant.to_string())
        .elem("DisplayName", &tenant.to_string())
        .close("Owner")
        .open("Buckets");
    for b in &buckets {
        x.open("Bucket")
            .elem("Name", &b.name)
            .elem("CreationDate", &dates::iso(b.info.created))
            .close("Bucket");
    }
    x.close("Buckets");
    Ok(xml_ok(x.finish("ListAllMyBucketsResult")))
}

/// The most a request's body may hold when it isn't an object.
const SMALL_BODY: usize = 64 << 10;
/// The most a DeleteObjects body may hold: 1000 keys of up to 1024
/// bytes, escaped.
const DELETE_BODY: usize = 8 << 20;

/// A small body (CreateBucket's), whole, checked as a PUT's is.
async fn small_body(
    signed: sigv4::Signed,
    signing: ring::hmac::Key,
    body: Incoming,
) -> Result<Vec<u8>, ApiError> {
    read_body(signed, signing, None, None, body, SMALL_BODY).await
}

/// A body of at most `max` bytes that must come with a `Content-MD5` or
/// a checksum, as S3 requires of DeleteObjects; checked against it.
async fn checked_body(
    signed: sigv4::Signed,
    signing: ring::hmac::Key,
    h: &HeaderMap,
    body: Incoming,
    max: usize,
) -> Result<Vec<u8>, ApiError> {
    let md5 = content_md5(h)?;
    let checksum = checksum_of(h, &signed)?;
    if md5.is_none() && checksum.is_none() {
        return Err(ApiError::new(
            400,
            "InvalidRequest",
            "Content-MD5 or an x-amz-checksum header is required",
        ));
    }
    let out = read_body(signed, signing, checksum, md5, body, max).await?;
    Ok(out)
}

/// The `Content-MD5` header, decoded.
fn content_md5(h: &HeaderMap) -> Result<Option<[u8; 16]>, ApiError> {
    match header(h, "content-md5") {
        None => Ok(None),
        Some(v) => Ok(Some(
            B64.decode(v)
                .ok()
                .and_then(|d| d.try_into().ok())
                .ok_or_else(|| ApiError::new(400, "InvalidDigest", "bad Content-MD5"))?,
        )),
    }
}

/// The checksum a body is to be checked against: its algorithm, and its
/// value if a header gives it (else a trailer will).
fn checksum_of(
    h: &HeaderMap,
    signed: &sigv4::Signed,
) -> Result<Option<(Algorithm, Option<String>)>, ApiError> {
    let mut checksum = None;
    for (k, v) in h {
        if let Some(a) = Algorithm::of_header(k.as_str())? {
            let v = v
                .to_str()
                .map_err(|_| ApiError::new(400, "InvalidArgument", "bad checksum"))?;
            checksum = Some((a, Some(v.to_string())));
        }
    }
    if checksum.is_none()
        && let Some(t) = header(h, "x-amz-trailer")
    {
        if signed.payload != Payload::ChunkedTrailer {
            return Err(ApiError::new(
                400,
                "InvalidRequest",
                "a trailer needs an aws-chunked body",
            ));
        }
        let a = Algorithm::of_header(t.trim())?
            .ok_or_else(|| ApiError::new(400, "InvalidRequest", "the trailer isn't a checksum"))?;
        checksum = Some((a, None));
    }
    Ok(checksum)
}

/// A body, whole, of at most `max` bytes, checked as a PUT's is.
async fn read_body(
    signed: sigv4::Signed,
    signing: ring::hmac::Key,
    checksum: Option<(Algorithm, Option<String>)>,
    md5: Option<[u8; 16]>,
    mut body: Incoming,
    max: usize,
) -> Result<Vec<u8>, ApiError> {
    let sha = match signed.payload {
        Payload::Sha256(s) => Some(s),
        _ => None,
    };
    let mut checks = Checks::new(sha, checksum.as_ref().map(|(a, v)| (*a, v.as_deref())))?;
    let mut decoder = match signed.payload {
        Payload::Chunked => Some(Chunked::new(Some(ChunkSigner::new(signing, &signed)))),
        Payload::ChunkedTrailer => Some(Chunked::new(None)),
        _ => None,
    };
    let mut out = Vec::new();
    while let Some(f) = body.frame().await {
        let f = f.map_err(|_| ApiError::new(400, "IncompleteBody", "the body ended early"))?;
        let Ok(data) = f.into_data() else {
            continue;
        };
        let start = out.len();
        match &mut decoder {
            Some(d) => d.feed(&data, &mut out)?,
            None => out.extend_from_slice(&data),
        }
        checks.update(&out[start..]);
        if out.len() > max {
            return Err(ApiError::new(
                400,
                "MaxMessageLengthExceeded",
                "the body is too long",
            ));
        }
    }
    let trailers = match decoder {
        Some(d) => d.finish()?,
        None => Vec::new(),
    };
    checks.finish(&trailers)?;
    if md5.is_some_and(|m| m != <[u8; 16]>::from(md5::Md5::digest(&out))) {
        return Err(bad_md5());
    }
    Ok(out)
}

fn bad_md5() -> ApiError {
    ApiError::new(
        400,
        "BadDigest",
        "the Content-MD5 you specified did not match what we received",
    )
}

/// The most keys a DeleteObjects request names.
const MAX_DELETE: usize = 1000;

/// DeleteObjects: the keys a `<Delete>` body names, each deleted (if its
/// `<ETag>`, when given, matches), and what happened to each.
async fn delete_objects(
    state: &State,
    b: &str,
    tenant: u32,
    body: &[u8],
) -> Result<Response<Body>, ApiError> {
    let malformed = |why: &str| ApiError::new(400, "MalformedXML", why.to_string());
    let text = std::str::from_utf8(body).map_err(|_| malformed("the body isn't UTF-8"))?;
    let doc = crate::xml::parse(text).map_err(malformed)?;
    if doc.name != "Delete" {
        return Err(malformed("not a Delete"));
    }
    let quiet = doc.text_of("Quiet").is_some_and(|q| q.trim() == "true");
    let mut asked = Vec::new();
    for o in doc.all("Object") {
        let key = o
            .text_of("Key")
            .ok_or_else(|| malformed("an Object without a Key"))?;
        let version = o.text_of("VersionId").map(str::trim).unwrap_or("");
        let cond = match o.text_of("ETag") {
            Some(t) => if_match(t),
            None => Condition::Any,
        };
        asked.push((key.as_bytes().to_vec(), version.to_string(), cond));
    }
    if asked.is_empty() || asked.len() > MAX_DELETE {
        return Err(malformed("a Delete names 1 to 1000 objects"));
    }
    let bucket = state.bucket(b, tenant).await?;
    let results = state
        .reactors
        .call(move |s3| async move {
            // A few at once, in the order asked.
            let mut out = Vec::with_capacity(asked.len());
            let mut asked = asked.into_iter().peekable();
            while asked.peek().is_some() {
                let batch: Vec<_> = asked
                    .by_ref()
                    .take(16)
                    .map(|(key, version, cond)| {
                        let (s3, bucket) = (s3.clone(), bucket.clone());
                        hyperstor_reactor::spawn(async move {
                            let r = if key.is_empty() || key.len() > 1024 {
                                Err(S3Error::InvalidKey.into())
                            } else if !version.is_empty() && version != "null" {
                                // No versioning: only the null version exists.
                                Err(ApiError::new(
                                    404,
                                    "NoSuchVersion",
                                    "the version doesn't exist",
                                ))
                            } else {
                                s3.delete_if(&bucket, &key, &cond)
                                    .await
                                    .map_err(ApiError::from)
                            };
                            (key, r)
                        })
                    })
                    .collect();
                for j in batch {
                    out.push(j.await);
                }
            }
            out
        })
        .await?;
    let mut x = Xml::new("DeleteResult");
    for (key, r) in &results {
        let key = String::from_utf8_lossy(key);
        match r {
            Ok(()) if quiet => {}
            Ok(()) => {
                x.open("Deleted").elem("Key", &key).close("Deleted");
            }
            Err(e) => {
                x.open("Error")
                    .elem("Key", &key)
                    .elem("Code", e.code)
                    .elem("Message", &e.message)
                    .close("Error");
            }
        }
    }
    Ok(xml_ok(x.finish("DeleteResult")))
}

/// The ETags an `If-Match` or `If-None-Match` lists, unquoted.
fn etags(v: &str) -> Vec<String> {
    v.split(',')
        .map(|t| {
            let t = t.trim();
            t.strip_prefix("W/")
                .unwrap_or(t)
                .trim_matches('"')
                .to_string()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// An `If-Match` as a condition on a write or delete.
fn if_match(v: &str) -> Condition {
    if v.trim() == "*" {
        Condition::Exists
    } else {
        Condition::Matches(etags(v))
    }
}

/// A PUT's condition (D194): `If-None-Match: *` (no object of that
/// name) or `If-Match` (the object it replaces).
fn write_condition(h: &HeaderMap) -> Result<Condition, ApiError> {
    match (header(h, "if-match"), header(h, "if-none-match")) {
        (None, None) => Ok(Condition::Any),
        (Some(_), Some(_)) => Err(ApiError::not_implemented(
            "If-Match and If-None-Match together",
        )),
        (Some(m), None) => Ok(if_match(m)),
        (None, Some(n)) if n.trim() == "*" => Ok(Condition::Absent),
        (None, Some(_)) => Err(ApiError::not_implemented("If-None-Match other than *")),
    }
}

/// What a GET or HEAD's conditions say of `e` (RFC 9110, section 13.2.2): `None`
/// to answer it, else 304 or 412.
fn read_condition(h: &HeaderMap, e: &Entry) -> Option<u16> {
    let tag = e.etag();
    let tag = tag.trim_matches('"');
    let modified = e.written / 1000;
    let any = |v: &str| v.trim() == "*" || etags(v).iter().any(|t| t == tag);
    let date = |name| header(h, name).and_then(dates::parse_http);
    match header(h, "if-match") {
        Some(v) if !any(v) => return Some(412),
        Some(_) => {}
        None => {
            if date("if-unmodified-since").is_some_and(|d| modified > d) {
                return Some(412);
            }
        }
    }
    match header(h, "if-none-match") {
        Some(v) if any(v) => Some(304),
        Some(_) => None,
        None => date("if-modified-since")
            .is_some_and(|d| modified <= d)
            .then_some(304),
    }
}

/// The object's checksum headers: the one it was written with, else the
/// CRC32C kept for every object.
fn checksum_headers(h: &mut HeaderMap, e: &Entry) {
    let (name, value) = match &e.checksum {
        Some((a, v)) => (a.header(), B64.encode(v)),
        None => (
            Algorithm::Crc32c.header(),
            B64.encode(e.crc32c.to_be_bytes()),
        ),
    };
    if let (Ok(k), Ok(v)) = (
        http::HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(&value),
    ) {
        h.insert(k, v);
        h.insert(
            "x-amz-checksum-type",
            HeaderValue::from_static("FULL_OBJECT"),
        );
    }
}

/// The placement a CreateBucket body asks for (D232): the one its
/// LocationConstraint names, or `default` when it names none or the
/// region the request was signed for (clients send their region there).
fn placement_of(body: &[u8], region: &str) -> Result<String, ApiError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| ApiError::new(400, "MalformedXML", "the body isn't UTF-8"))?;
    let lc = match (
        text.find("<LocationConstraint>"),
        text.find("</LocationConstraint>"),
    ) {
        (Some(a), Some(b)) if a < b => text[a + "<LocationConstraint>".len()..b].trim(),
        (None, None) if text.trim().is_empty() || text.contains("CreateBucketConfiguration") => "",
        _ => {
            return Err(ApiError::new(
                400,
                "MalformedXML",
                "not a CreateBucketConfiguration",
            ));
        }
    };
    Ok(if lc.is_empty() || lc == region {
        DEFAULT_PLACEMENT.to_string()
    } else {
        lc.to_string()
    })
}

async fn create_bucket(
    state: &State,
    name: &str,
    tenant: u32,
    placement: &str,
) -> Result<Response<Body>, ApiError> {
    let Some((data, index)) = state.served().placements.get(placement).cloned() else {
        return Err(if placement == DEFAULT_PLACEMENT {
            eprintln!(
                "hyperstor-s3: no default placement: buckets can't be created until one is set"
            );
            ApiError::new(
                503,
                "ServiceUnavailable",
                "no default placement: an administrator sets one (hyperstorctl s3 placement set --name default)",
            )
        } else {
            ApiError::new(
                400,
                "InvalidLocationConstraint",
                format!("no placement {placement}"),
            )
        });
    };
    let n = name.to_string();
    let r = state
        .reactors
        .call(move |s3| async move {
            let map = s3.client().map();
            let id = |p: &Pool| match p {
                Pool::Id(id) => map.pools.contains_key(id).then_some(*id),
                Pool::Name(n) => map.pools.values().find(|q| &q.name == n).map(|q| q.id),
            };
            let (Some(d), Some(i)) = (id(&data), id(&index)) else {
                eprintln!(
                    "hyperstor-s3: the pools for new buckets ({data:?}, {index:?}) aren't in the map"
                );
                return Err(ApiError::new(
                    503,
                    "ServiceUnavailable",
                    "the bucket pools don't exist",
                ));
            };
            Ok(s3.create_bucket(&n, tenant, i, d).await)
        })
        .await??;
    r?;
    let mut r = Response::new(empty());
    if let Ok(v) = HeaderValue::from_str(&format!("/{}", sigv4::aws_encode(name.as_bytes(), true)))
    {
        r.headers_mut().insert(http::header::LOCATION, v);
    }
    Ok(r)
}

/// Encoded for `encoding-type=url`, or as it is.
fn shown(b: &[u8], url: bool) -> String {
    if url {
        sigv4::aws_encode(b, false)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

async fn list_objects(
    state: &State,
    b: &str,
    tenant: u32,
    q: &Pairs,
) -> Result<Response<Body>, ApiError> {
    let v2 = match text_param(q, "list-type")?.as_deref() {
        None => false,
        Some("2") => true,
        Some(_) => {
            return Err(ApiError::new(
                400,
                "InvalidArgument",
                "list-type is 2 or nothing",
            ));
        }
    };
    if v2 {
        only(
            q,
            &[
                "list-type",
                "prefix",
                "delimiter",
                "max-keys",
                "continuation-token",
                "start-after",
                "encoding-type",
                "fetch-owner",
            ],
        )?;
    } else {
        only(
            q,
            &["prefix", "delimiter", "max-keys", "marker", "encoding-type"],
        )?;
    }
    let url = match text_param(q, "encoding-type")?.as_deref() {
        None => false,
        Some("url") => true,
        Some(_) => {
            return Err(ApiError::new(
                400,
                "InvalidArgument",
                "encoding-type is url or nothing",
            ));
        }
    };
    let prefix = param(q, "prefix").unwrap_or_default().to_vec();
    let delimiter = param(q, "delimiter").unwrap_or_default().to_vec();
    let max: usize = match text_param(q, "max-keys")? {
        None => 1000,
        Some(m) => m
            .parse::<usize>()
            .map_err(|_| ApiError::new(400, "InvalidArgument", "max-keys is a number"))?
            .min(1000),
    };
    let token = text_param(q, "continuation-token")?;
    let start_after = param(q, "start-after").map(<[u8]>::to_vec);
    let marker = param(q, "marker").map(<[u8]>::to_vec);
    let start = if let Some(t) = &token {
        sigv4::unhex(t)
            .ok_or_else(|| ApiError::new(400, "InvalidArgument", "bad continuation-token"))?
    } else if let Some(s) = start_after.as_ref().or(marker.as_ref()) {
        after(s, &prefix, &delimiter)
    } else {
        Vec::new()
    };
    let bucket = state.bucket(b, tenant).await?;
    let listing = if max == 0 {
        Default::default()
    } else {
        let (p, d, s) = (prefix.clone(), delimiter.clone(), start);
        state
            .reactors
            .call(move |s3| async move {
                let l = List {
                    prefix: &p,
                    delimiter: &d,
                    start: &s,
                    max,
                };
                s3.list(&bucket, &l).await
            })
            .await??
    };
    let mut x = Xml::new("ListBucketResult");
    x.elem("Name", b)
        .elem("Prefix", &shown(&prefix, url))
        .elem("MaxKeys", &max.to_string());
    if !delimiter.is_empty() {
        x.elem("Delimiter", &shown(&delimiter, url));
    }
    if url {
        x.elem("EncodingType", "url");
    }
    x.elem(
        "IsTruncated",
        if listing.next.is_some() {
            "true"
        } else {
            "false"
        },
    );
    if v2 {
        x.elem(
            "KeyCount",
            &(listing.objects.len() + listing.prefixes.len()).to_string(),
        );
        if let Some(t) = &token {
            x.elem("ContinuationToken", t);
        }
        if let Some(n) = &listing.next {
            x.elem("NextContinuationToken", &sigv4::hex(n));
        }
        if let Some(s) = &start_after {
            x.elem("StartAfter", &shown(s, url));
        }
    } else {
        x.elem("Marker", &shown(marker.as_deref().unwrap_or_default(), url));
        if listing.next.is_some() {
            // The last name or common prefix given: the next page starts
            // after it ([`after`]).
            let last = listing
                .objects
                .last()
                .map(|(k, _)| k)
                .into_iter()
                .chain(listing.prefixes.last())
                .max();
            if let Some(l) = last {
                x.elem("NextMarker", &shown(l, url));
            }
        }
    }
    for (k, e) in &listing.objects {
        x.open("Contents")
            .elem("Key", &shown(k, url))
            .elem("LastModified", &dates::iso(e.written))
            .elem("ETag", &e.etag())
            .elem("Size", &e.size.to_string())
            .elem("StorageClass", "STANDARD")
            .close("Contents");
    }
    for p in &listing.prefixes {
        x.open("CommonPrefixes")
            .elem("Prefix", &shown(p, url))
            .close("CommonPrefixes");
    }
    Ok(xml_ok(x.finish("ListBucketResult")))
}

/// Where a listing starts after name `s` (StartAfter, or v1's marker):
/// past every name it rolls up into, if it is a common prefix.
fn after(s: &[u8], prefix: &[u8], delimiter: &[u8]) -> Vec<u8> {
    if !delimiter.is_empty()
        && let Some(rest) = s.strip_prefix(prefix)
        && let Some(i) = rest.windows(delimiter.len()).position(|w| w == delimiter)
        && prefix.len() + i + delimiter.len() == s.len()
        && let Some(p) = past_prefix(s)
    {
        return p;
    }
    [s, &[0]].concat()
}

/// GET's query parameters that set the response's headers.
const RESPONSE_OVERRIDES: &[&str] = &[
    "response-content-type",
    "response-content-language",
    "response-expires",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
];

/// A `Range` header against an object of `size` bytes: `None` for the
/// whole object, else the offset and length; `Err` if it can't be met.
fn range(h: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = h.and_then(|h| h.strip_prefix("bytes=")) else {
        return Ok(None);
    };
    // Several ranges: S3 sends the whole object.
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let num = |s: &str| s.trim().parse::<u64>().ok();
    let (from, to) = match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, false) => {
            let n = num(b).ok_or(())?;
            if n == 0 || size == 0 {
                return Err(());
            }
            (size.saturating_sub(n), size - 1)
        }
        (false, true) => (num(a).ok_or(())?, size.saturating_sub(1)),
        (false, false) => {
            let (f, t) = (num(a).ok_or(())?, num(b).ok_or(())?);
            if t < f {
                return Ok(None);
            }
            (f, t.min(size.saturating_sub(1)))
        }
        (true, true) => return Ok(None),
    };
    if from >= size {
        return Err(());
    }
    Ok(Some((from, to - from + 1)))
}

fn object_headers(r: &mut http::response::Builder, e: &Entry) {
    let h = r.headers_mut().expect("a builder without errors");
    let mut set = |k: &str, v: &str| {
        if let (Ok(k), Ok(v)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            h.insert(k, v);
        }
    };
    set("etag", &e.etag());
    set("last-modified", &dates::http(e.written / 1000));
    set("content-type", &e.content_type);
    set("accept-ranges", "bytes");
    for (k, v) in &e.meta {
        set(&format!("x-amz-meta-{k}"), v);
    }
}

async fn get_object(
    state: &State,
    b: &str,
    key: Vec<u8>,
    tenant: u32,
    q: &Pairs,
    head: bool,
    h: &HeaderMap,
) -> Result<Response<Body>, ApiError> {
    let bucket = state.bucket(b, tenant).await?;
    let (bk, kk) = (bucket.clone(), key);
    let e = state
        .reactors
        .call(move |s3| async move { s3.head(&bk, &kk).await })
        .await??
        .ok_or_else(|| ApiError::new(404, "NoSuchKey", "the object doesn't exist"))?;
    let mut r = Response::builder();
    object_headers(&mut r, &e);
    match read_condition(h, &e) {
        None => {}
        Some(304) => {
            return r
                .status(304)
                .body(empty())
                .map_err(|_| ApiError::new(500, "InternalError", "a header couldn't be written"));
        }
        Some(_) => return Err(precondition_failed()),
    }
    for o in RESPONSE_OVERRIDES {
        if let Some(v) = text_param(q, o)? {
            let name = o.trim_start_matches("response-");
            if let Ok(v) = HeaderValue::from_str(&v) {
                r = r.header(name, v);
            }
        }
    }
    let (off, len) =
        match range(header(h, "range"), e.size) {
            Ok(None) => (0, e.size),
            Ok(Some((off, len))) => {
                r = r.status(206).header(
                    "content-range",
                    format!("bytes {off}-{}/{}", off + len - 1, e.size),
                );
                (off, len)
            }
            Err(()) => {
                let mut err = ApiError::new(416, "InvalidRange", "the range can't be met")
                    .response("", &request_id(), head);
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", e.size)) {
                    err.headers_mut().insert("content-range", v);
                }
                return Ok(err);
            }
        };
    if len == e.size
        && header(h, "x-amz-checksum-mode").is_some_and(|m| m.eq_ignore_ascii_case("enabled"))
    {
        checksum_headers(r.headers_mut().expect("a builder without errors"), &e);
    }
    r = r.header("content-length", len);
    let body = if head || len == 0 {
        empty()
    } else {
        let (tx, rx) = mpsc::channel(2);
        state
            .reactors
            .spawn(move |s3| send_range(s3, bucket, e, off, len, tx));
        Streamed(rx).boxed()
    };
    r.body(body)
        .map_err(|_| ApiError::new(500, "InternalError", "a header couldn't be written"))
}

/// Reads `len` bytes of `e` from `off`, a chunk at a time with the next
/// one read ahead, into `tx` until it is done or the client goes.
async fn send_range(
    s3: Rc<S3>,
    b: Bucket,
    e: Entry,
    off: u64,
    len: u64,
    tx: mpsc::Sender<io::Result<Bytes>>,
) {
    let step = s3.config().chunk.max(4096);
    let (mut at, end) = (off, off + len);
    let mut reading = VecDeque::new();
    loop {
        while reading.len() < 2 && at < end {
            let n = (step - at % step).min(end - at);
            let (s3, b, e) = (s3.clone(), b.clone(), e.clone());
            reading.push_back(hyperstor_reactor::spawn(async move {
                s3.read(&b, &e, at, n).await
            }));
            at += n;
        }
        let Some(r) = reading.pop_front() else {
            return;
        };
        let item = r.await.map(Bytes::from).map_err(|err| {
            eprintln!("hyperstor-s3: a GET failed part-way: {err}");
            io::Error::other("the read failed")
        });
        let failed = item.is_err();
        if tx.send(item).await.is_err() || failed {
            return;
        }
    }
}

/// The user metadata and content type of a PUT.
fn meta_of(h: &HeaderMap) -> Result<Meta, ApiError> {
    let mut meta = Vec::new();
    let mut bytes = 0;
    for (k, v) in h {
        if let Some(name) = k.as_str().strip_prefix("x-amz-meta-") {
            let v = v
                .to_str()
                .map_err(|_| ApiError::new(400, "InvalidArgument", "metadata is ASCII"))?;
            bytes += name.len() + v.len();
            meta.push((name.to_string(), v.to_string()));
        }
    }
    if bytes > MAX_META {
        return Err(ApiError::new(
            400,
            "MetadataTooLarge",
            "user metadata is 2 KiB at most",
        ));
    }
    Ok(Meta {
        content_type: header(h, "content-type")
            .unwrap_or("binary/octet-stream")
            .to_string(),
        meta,
        checksum: None,
    })
}

async fn put_object(
    state: &State,
    b: &str,
    key: Vec<u8>,
    tenant: u32,
    signed: sigv4::Signed,
    signing: ring::hmac::Key,
    req: http::Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let h = req.headers();
    if let Some(src) = header(h, "x-amz-copy-source") {
        return copy_object(state, b, key, tenant, src, h).await;
    }
    let cond = write_condition(h)?;
    if key.len() > 1024 {
        return Err(S3Error::InvalidKey.into());
    }
    let chunked = matches!(signed.payload, Payload::Chunked | Payload::ChunkedTrailer);
    let length = if chunked {
        "x-amz-decoded-content-length"
    } else {
        "content-length"
    };
    let size: u64 = header(h, length)
        .ok_or_else(|| ApiError::new(411, "MissingContentLength", format!("{length} is needed")))?
        .parse()
        .map_err(|_| ApiError::new(400, "InvalidArgument", format!("bad {length}")))?;
    if size > MAX_PUT {
        return Err(ApiError::new(
            400,
            "EntityTooLarge",
            "a PUT is 5 GiB at most",
        ));
    }
    let md5 = content_md5(h)?;
    let checksum = checksum_of(h, &signed)?;
    let meta = meta_of(h)?;
    let sha = match signed.payload {
        Payload::Sha256(s) => Some(s),
        _ => None,
    };
    let mut checks = Checks::new(sha, checksum.as_ref().map(|(a, v)| (*a, v.as_deref())))?;
    let mut decoder = match signed.payload {
        Payload::Chunked => Some(Chunked::new(Some(ChunkSigner::new(signing, &signed)))),
        Payload::ChunkedTrailer => Some(Chunked::new(None)),
        _ => None,
    };
    let bucket = state.bucket(b, tenant).await?;
    let (tx, rx) = mpsc::channel::<Bytes>(4);
    // Sent once the body is found good, with the checksum it matched.
    let (good_tx, good_rx) = oneshot::channel::<Option<(Algorithm, Vec<u8>)>>();
    let job = state
        .reactors
        .call(move |s3| write_object(s3, bucket, key, size, meta, md5, cond, rx, good_rx));
    let mut body = req.into_body();
    let pump = async move {
        let mut piece = Vec::with_capacity(PIECE);
        let mut got = 0u64;
        while let Some(f) = body.frame().await {
            let f = f.map_err(|_| ApiError::new(400, "IncompleteBody", "the body ended early"))?;
            let Ok(data) = f.into_data() else {
                continue;
            };
            let start = piece.len();
            match &mut decoder {
                Some(d) => d.feed(&data, &mut piece)?,
                None => piece.extend_from_slice(&data),
            }
            checks.update(&piece[start..]);
            got += (piece.len() - start) as u64;
            if got > size {
                return Err(ApiError::new(
                    400,
                    "InvalidRequest",
                    "the body is longer than its length",
                ));
            }
            if piece.len() >= PIECE {
                let full = std::mem::replace(&mut piece, Vec::with_capacity(PIECE));
                if tx.send(full.into()).await.is_err() {
                    // The reactor gave up; its error says why.
                    return Ok(());
                }
            }
        }
        if got < size {
            return Err(ApiError::new(
                400,
                "IncompleteBody",
                "the body is shorter than its length",
            ));
        }
        let trailers = match decoder {
            Some(d) => d.finish()?,
            None => Vec::new(),
        };
        let sum = checks.finish(&trailers)?;
        if !piece.is_empty() {
            let _ = tx.send(piece.into()).await;
        }
        drop(tx);
        let _ = good_tx.send(sum);
        Ok(())
    };
    let (pumped, written) = tokio::join!(pump, job);
    pumped?;
    let e = written??;
    let mut r = Response::new(empty());
    if let Ok(v) = HeaderValue::from_str(&e.etag()) {
        r.headers_mut().insert(http::header::ETAG, v);
    }
    checksum_headers(r.headers_mut(), &e);
    Ok(r)
}

/// The bucket and object an `x-amz-copy-source` names: `bucket/key`,
/// URL-encoded, perhaps with a leading `/` and a `?versionId=`.
fn copy_source(v: &str) -> Result<(String, Vec<u8>), ApiError> {
    let bad = || ApiError::new(400, "InvalidArgument", "bad x-amz-copy-source");
    let (path, query) = v.split_once('?').unwrap_or((v, ""));
    if let Some(version) = query.strip_prefix("versionId=")
        && version != "null"
    {
        // No versioning: only the null version exists.
        return Err(ApiError::new(
            404,
            "NoSuchVersion",
            "the version doesn't exist",
        ));
    }
    let path = path.strip_prefix('/').unwrap_or(path);
    let (b, k) = path.split_once('/').ok_or_else(bad)?;
    let b = String::from_utf8(sigv4::percent_decode(b.as_bytes()).ok_or_else(bad)?)
        .map_err(|_| bad())?;
    let k = sigv4::percent_decode(k.as_bytes()).ok_or_else(bad)?;
    if b.is_empty() || k.is_empty() || std::str::from_utf8(&k).is_err() {
        return Err(bad());
    }
    Ok((b, k))
}

/// What `x-amz-copy-source-if-*` say of the source: as a GET's
/// conditions, except that S3 answers every failure with 412.
fn copy_source_condition(h: &HeaderMap, e: &Entry) -> Result<(), ApiError> {
    let mut as_get = HeaderMap::new();
    for name in [
        "if-match",
        "if-none-match",
        "if-modified-since",
        "if-unmodified-since",
    ] {
        if let Some(v) = h.get(format!("x-amz-copy-source-{name}")) {
            as_get.insert(name, v.clone());
        }
    }
    match read_condition(&as_get, e) {
        None => Ok(()),
        Some(_) => Err(precondition_failed()),
    }
}

/// CopyObject (D193): the source, of the same tenant, read and written
/// again as `key`, with its metadata or the request's
/// (`x-amz-metadata-directive: REPLACE`).
async fn copy_object(
    state: &State,
    b: &str,
    key: Vec<u8>,
    tenant: u32,
    src: &str,
    h: &HeaderMap,
) -> Result<Response<Body>, ApiError> {
    if key.len() > 1024 {
        return Err(S3Error::InvalidKey.into());
    }
    let (sb, sk) = copy_source(src)?;
    let replace = match header(h, "x-amz-metadata-directive") {
        None | Some("COPY") => false,
        Some("REPLACE") => true,
        Some(_) => {
            return Err(ApiError::new(
                400,
                "InvalidArgument",
                "x-amz-metadata-directive is COPY or REPLACE",
            ));
        }
    };
    if sb == b && sk == key && !replace {
        return Err(ApiError::new(
            400,
            "InvalidRequest",
            "this copy request is illegal because it is trying to copy an object to itself without changing the object's metadata",
        ));
    }
    let hash = match header(h, "x-amz-checksum-algorithm") {
        None => None,
        Some(a) => Some(
            Algorithm::of_header(&format!("x-amz-checksum-{}", a.to_ascii_lowercase()))?
                .ok_or_else(|| ApiError::new(400, "InvalidRequest", "bad checksum algorithm"))?,
        ),
    };
    let cond = write_condition(h)?;
    let meta = if replace { Some(meta_of(h)?) } else { None };
    let from = state.bucket(&sb, tenant).await?;
    let to = if sb == b {
        from.clone()
    } else {
        state.bucket(b, tenant).await?
    };
    let (fk, ff) = (sk, from.clone());
    let e = state
        .reactors
        .call(move |s3| async move { s3.head(&ff, &fk).await })
        .await??
        .ok_or_else(|| ApiError::new(404, "NoSuchKey", "the source object doesn't exist"))?;
    copy_source_condition(h, &e)?;
    if e.size > MAX_PUT {
        return Err(ApiError::new(
            400,
            "InvalidRequest",
            "a copy source is 5 GiB at most; copy larger objects in parts",
        ));
    }
    let meta = meta.unwrap_or_else(|| Meta {
        content_type: e.content_type.clone(),
        meta: e.meta.clone(),
        checksum: None,
    });
    let made = state
        .reactors
        .call(move |s3| async move {
            s3.copy(&from, &e, &to, &key, meta, cond, hash)
                .await
                .map_err(|err| match err {
                    // The source was replaced and its chunks are gone.
                    S3Error::Changed => ApiError::new(
                        503,
                        "SlowDown",
                        "the source object changed during the copy; try again",
                    ),
                    err => err.into(),
                })
        })
        .await??;
    let mut x = Xml::new("CopyObjectResult");
    x.elem("LastModified", &dates::iso(made.written))
        .elem("ETag", &made.etag());
    if let Some((a, v)) = &made.checksum {
        x.elem(&format!("Checksum{}", a.name()), &B64.encode(v));
    }
    Ok(xml_ok(x.finish("CopyObjectResult")))
}

/// A PUT's reactor side: writes what `rx` brings, then, once tokio's
/// side has found the body good, checks the MD5 and makes the object
/// visible.
#[allow(clippy::too_many_arguments)]
async fn write_object(
    s3: Rc<S3>,
    b: Bucket,
    key: Vec<u8>,
    size: u64,
    mut meta: Meta,
    md5: Option<[u8; 16]>,
    cond: Condition,
    mut rx: mpsc::Receiver<Bytes>,
    good: oneshot::Receiver<Option<(crate::chunked::Algorithm, Vec<u8>)>>,
) -> Result<Entry, ApiError> {
    // Checked before the body is taken, and again as it is made visible.
    if cond != Condition::Any && !cond.holds(s3.head(&b, &key).await?.as_ref()) {
        return Err(precondition_failed());
    }
    let mut put = s3.put(&b, &key, size).await?;
    put.require(cond);
    while let Some(d) = rx.recv().await {
        if let Err(e) = put.write(&d).await {
            let _ = put.abort().await;
            return Err(e.into());
        }
    }
    let Ok(sum) = good.await else {
        let _ = put.abort().await;
        return Err(ApiError::new(400, "IncompleteBody", "the body was refused"));
    };
    if md5.is_some_and(|m| m != put.md5()) {
        put.abort().await?;
        return Err(bad_md5());
    }
    meta.checksum = sum;
    Ok(put.finish(meta).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_constraints_choose_a_placement() {
        let p = |b: &str| placement_of(b.as_bytes(), "us-west-2").map_err(|e| e.code);
        assert_eq!(p("").as_deref(), Ok("default"));
        let cfg = |lc: &str| {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{lc}</LocationConstraint></CreateBucketConfiguration>"
            )
        };
        assert_eq!(p(&cfg("fast")).as_deref(), Ok("fast"));
        // The region the client signed for is what SDKs send by default.
        assert_eq!(p(&cfg("us-west-2")).as_deref(), Ok("default"));
        assert_eq!(p(&cfg(" ")).as_deref(), Ok("default"));
        assert_eq!(p("<CreateBucketConfiguration/>").as_deref(), Ok("default"));
        for bad in [
            "<Bucket/>",
            "</LocationConstraint><LocationConstraint>",
            "nonsense",
        ] {
            assert_eq!(p(bad), Err("MalformedXML"), "{bad}");
        }
        assert_eq!(
            placement_of(&[0xff], "x").map_err(|e| e.code),
            Err("MalformedXML")
        );
    }

    #[test]
    fn ranges_are_read_as_s3_reads_them() {
        let r = |h: &str, size| range(Some(h), size);
        assert_eq!(r("bytes=0-9", 100), Ok(Some((0, 10))));
        assert_eq!(r("bytes=90-200", 100), Ok(Some((90, 10))));
        assert_eq!(r("bytes=95-", 100), Ok(Some((95, 5))));
        assert_eq!(r("bytes=-5", 100), Ok(Some((95, 5))));
        assert_eq!(r("bytes=-500", 100), Ok(Some((0, 100))));
        assert_eq!(r("bytes=100-", 100), Err(()));
        assert_eq!(r("bytes=-0", 100), Err(()));
        assert_eq!(r("bytes=0-0", 0), Err(()));
        assert_eq!(r("bytes=5-2", 100), Ok(None));
        assert_eq!(r("bytes=0-1,5-6", 100), Ok(None));
        assert_eq!(r("items=0-1", 100), Ok(None));
        assert_eq!(range(None, 100), Ok(None));
    }

    #[test]
    fn listings_resume_after_a_name_or_common_prefix() {
        assert_eq!(after(b"a/b", b"", b"/"), b"a/b\0");
        assert_eq!(after(b"a/", b"", b"/"), b"a0");
        assert_eq!(after(b"p/a/", b"p/", b"/"), b"p/a0");
        assert_eq!(after(b"a/", b"", b""), b"a/\0");
    }

    fn entry(written_secs: u64) -> Entry {
        Entry {
            written: written_secs * 1000 + 500,
            size: 1,
            md5: [0xab; 16],
            parts: 0,
            crc32c: 7,
            content_type: String::new(),
            meta: Vec::new(),
            data: crate::layout::Data::Inline(b"x".to_vec()),
            checksum: None,
        }
    }

    #[test]
    fn conditions_are_read_as_s3_reads_them() {
        let e = entry(1_000_000);
        let tag = "ab".repeat(16);
        let h = |pairs: &[(&'static str, String)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        let at = |secs| dates::http(secs);
        let q = |pairs: &[(&'static str, String)]| read_condition(&h(pairs), &e);
        assert_eq!(q(&[]), None);
        assert_eq!(q(&[("if-match", format!("\"{tag}\""))]), None);
        assert_eq!(q(&[("if-match", format!("\"x\", W/\"{tag}\""))]), None);
        assert_eq!(q(&[("if-match", "*".into())]), None);
        assert_eq!(q(&[("if-match", "\"x\"".into())]), Some(412));
        assert_eq!(q(&[("if-none-match", format!("\"{tag}\""))]), Some(304));
        assert_eq!(q(&[("if-none-match", "\"x\"".into())]), None);
        assert_eq!(q(&[("if-modified-since", at(1_000_000))]), Some(304));
        assert_eq!(q(&[("if-modified-since", at(999_999))]), None);
        assert_eq!(q(&[("if-unmodified-since", at(999_999))]), Some(412));
        assert_eq!(q(&[("if-unmodified-since", at(1_000_000))]), None);
        // If-Match holding overrides If-Unmodified-Since; If-None-Match
        // not matching overrides If-Modified-Since (as S3 documents).
        assert_eq!(
            q(&[
                ("if-match", format!("\"{tag}\"")),
                ("if-unmodified-since", at(999_999))
            ]),
            None
        );
        assert_eq!(
            q(&[
                ("if-none-match", "\"x\"".into()),
                ("if-modified-since", at(1_000_000))
            ]),
            None
        );
        // Writes: If-None-Match is * or nothing.
        let w = |pairs: &[(&'static str, String)]| write_condition(&h(pairs)).map_err(|e| e.code);
        assert_eq!(w(&[]), Ok(Condition::Any));
        assert_eq!(w(&[("if-none-match", "*".into())]), Ok(Condition::Absent));
        assert_eq!(w(&[("if-match", "*".into())]), Ok(Condition::Exists));
        assert_eq!(
            w(&[("if-match", "\"a\",\"b\"".into())]),
            Ok(Condition::Matches(vec!["a".into(), "b".into()]))
        );
        assert_eq!(
            w(&[("if-none-match", "\"a\"".into())]),
            Err("NotImplemented")
        );
        assert!(Condition::Matches(vec![tag.clone()]).holds(Some(&e)));
        assert!(!Condition::Matches(vec![tag]).holds(None));
        assert!(Condition::Absent.holds(None) && !Condition::Absent.holds(Some(&e)));
    }

    #[test]
    fn copy_sources_name_a_bucket_and_object() {
        let c = |v: &str| copy_source(v).map_err(|e| e.code);
        assert_eq!(c("b/k"), Ok(("b".into(), b"k".to_vec())));
        assert_eq!(c("/b/a%20b/c%3Fd"), Ok(("b".into(), b"a b/c?d".to_vec())));
        assert_eq!(c("b/k?versionId=null"), Ok(("b".into(), b"k".to_vec())));
        assert_eq!(c("b/k?versionId=x1"), Err("NoSuchVersion"));
        for bad in ["b", "b/", "/k", "b/%zz", "b/%ff"] {
            assert_eq!(c(bad), Err("InvalidArgument"), "{bad}");
        }
    }

    #[test]
    fn paths_name_buckets_and_objects() {
        assert!(matches!(target("/"), Ok(Target::Service)));
        assert!(matches!(target("/b"), Ok(Target::Bucket(b)) if b == "b"));
        assert!(matches!(target("/b/"), Ok(Target::Bucket(b)) if b == "b"));
        assert!(
            matches!(target("/b/a%20b//c/"), Ok(Target::Object(b, k)) if b == "b" && k == b"a b//c/")
        );
        assert!(target("/b/%ff").is_err());
        assert!(target("/b/%zz").is_err());
    }
}
