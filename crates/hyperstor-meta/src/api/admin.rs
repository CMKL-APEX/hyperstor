//! Joining hosts, renewing certificates and long operations
//! (spec/administration.md §23.4.2, §23.5, §23.6, §23.8): `TokenService`,
//! `CaService`, `NodeService` and `OperationService`.
//!
//! - **Join tokens** (D57) are `hsj1-<root fingerprint>-<secret>`. Only
//!   the secret's SHA-256, the role, the expiry and whether it was used
//!   are kept, in the replicated store (D81, `crate::tokens`): the leader
//!   makes, burns and revokes them (D82), any member lists them. A token
//!   is single-use and burnt by its first join, successful or not;
//!   burning is a Raft write, so a join through a follower is refused
//!   without burning it.
//! - **A join** needs no client certificate. The host pinned the root by
//!   the token's fingerprint before it sent anything, and sends a
//!   certificate request; only the request's key is used, and the token
//!   decides the identity. A storage host is added to the map and gets
//!   `node/<N>`; a map service member gets `meta/<id>` and the
//!   intermediate CA, which every member holds (§23.8).
//! - **Renewal** signs a new key for the caller's own identity and
//!   addresses, read from the certificate it connected with (D56).
//! - **Operations** are held in memory too, the last 100: a drain takes a
//!   node's devices out and follows its PGs until none still lives on the
//!   node and none is moving.

use super::{Api, Need, audit, meta_error, with_reason};
use crate::Admin;
use crate::tokens::{Record, TokenError};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{
    CaService, CaServiceExt, MapService, MapServiceExt, NodeService, NodeServiceExt,
    OperationService, OperationServiceExt, TokenService, TokenServiceExt,
};
use hyperstor_pki::{Identity, Pem, Role};
use hyperstor_reactor::chan::oneshot;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a member needs to sign joins and renewals.
pub struct Signing {
    /// The intermediate CA (§23.8).
    pub ca: Pem,
    /// The cluster file the group was seeded from, for joining members.
    pub cluster_file: String,
    /// Where storage nodes reach the map service: every member's map
    /// address, separated by commas.
    pub map_addresses: String,
    /// Where join tokens are kept.
    pub tokens: Arc<crate::tokens::Tokens>,
    /// Certificates issued and the revocation list (D93).
    pub certs: Arc<crate::certs::Certs>,
    /// The replicated store, for backups (D86); none without `--data`.
    pub store: Option<Arc<crate::store::Db>>,
}

/// Tokens live an hour by default and a week at most.
const TOKEN_TTL: u32 = 3600;
const TOKEN_TTL_MAX: u32 = 7 * 86400;
const OPS_KEPT: usize = 100;

pub(super) struct State {
    signing: Option<Signing>,
    root: Option<String>,
    api_address: String,
    ops: Mutex<VecDeque<Arc<Mutex<pb::Operation>>>>,
}

impl State {
    /// Whether the certificate with `serial` is revoked; false when this
    /// member keeps no list (no `--ca`).
    pub(super) fn is_revoked(&self, serial: &str) -> bool {
        self.signing
            .as_ref()
            .is_some_and(|s| s.certs.is_revoked(serial, now_unix()))
    }

    pub(super) fn new(signing: Option<Signing>, root: Option<String>, api_address: String) -> Self {
        State {
            signing,
            root,
            api_address,
            ops: Mutex::new(VecDeque::new()),
        }
    }
}

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    let router = TokenServiceExt::register(api.clone(), router);
    let router = CaServiceExt::register(api.clone(), router);
    let router = NodeServiceExt::register(api.clone(), router);
    let router = MapServiceExt::register(api.clone(), router);
    OperationServiceExt::register(api, router)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    // SAFETY: getrandom writes at most `b.len()` bytes into `b`.
    let n = unsafe { libc::getrandom(b.as_mut_ptr() as *mut libc::c_void, b.len(), 0) };
    assert_eq!(n, b.len() as isize, "getrandom");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A join token's parts: the root's fingerprint and the secret.
pub fn parse_token(t: &str) -> Option<(&str, &str)> {
    let rest = t.trim().strip_prefix("hsj1-")?;
    let (pin, secret) = rest.split_once('-')?;
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|c| c.is_ascii_hexdigit());
    (hex(pin, 64) && hex(secret, 64)).then_some((pin, secret))
}

/// The first certificate of a PEM chain, in DER.
fn rustls_pemfile_first(pem: &str) -> Option<Vec<u8>> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
        .next()?
        .ok()
        .map(|c| c.to_vec())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn invalid(msg: impl Into<String>, reason: &str) -> ConnectError {
    with_reason(ConnectError::invalid_argument(msg.into()), reason)
}

impl Api {
    fn signing(&self) -> Result<(&Signing, &str), ConnectError> {
        match (&self.admin.signing, &self.admin.root) {
            (Some(s), Some(r)) => Ok((s, r)),
            _ => Err(with_reason(
                ConnectError::failed_precondition(
                    "this map service member can't sign certificates (no --ca, or no TLS)",
                ),
                "no_ca",
            )),
        }
    }

    async fn ask<T>(
        &self,
        make: impl FnOnce(hyperstor_reactor::chan::OneSender<T>) -> Admin,
    ) -> Result<T, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(make(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        rx.recv()
            .await
            .ok_or_else(|| ConnectError::unavailable("map service stopped"))
    }

    /// Runs `f` on the token store off the API's thread: a write waits
    /// for Raft to commit it.
    async fn tokens<T: Send + 'static>(
        &self,
        f: impl FnOnce(&crate::tokens::Tokens) -> Result<T, TokenError> + Send + 'static,
    ) -> Result<T, ConnectError> {
        let (signing, _) = self.signing()?;
        let t = signing.tokens.clone();
        match tokio::task::spawn_blocking(move || f(&t)).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(self.token_error(e).await),
            Err(e) => Err(ConnectError::internal(e.to_string())),
        }
    }

    async fn token_error(&self, e: TokenError) -> ConnectError {
        match e {
            TokenError::NotLeader => {
                let why = match self.ask(Admin::Leads).await {
                    Ok(Err(e)) => e,
                    _ => "this map service member isn't the leader".into(),
                };
                meta_error(
                    format!("{why}: tokens and revocations are written by the leader"),
                    ConnectError::unavailable,
                    "not_leader",
                )
            }
            TokenError::NotFormed => with_reason(
                ConnectError::unavailable(
                    "the map service has no leader yet: until it has, only the first member makes tokens, and only meta ones",
                ),
                "not_formed",
            ),
            TokenError::Refused {
                reason: "invalid_argument",
                why,
            } => invalid(why, "invalid_argument"),
            TokenError::Refused { reason, why } => {
                with_reason(ConnectError::permission_denied(why), reason)
            }
            TokenError::NotFound => {
                with_reason(ConnectError::not_found("no such token"), "not_found")
            }
            TokenError::Store(e) => ConnectError::internal(e),
        }
    }

    /// Records a certificate the API issued (D93), off the API's thread.
    /// Best effort: only the leader records, and a renewal through another
    /// member is signed all the same.
    async fn record_issued(&self, chain: &str, by: &str) {
        let Ok((signing, _)) = self.signing() else {
            return;
        };
        let der = rustls_pemfile_first(chain);
        let (Some(serial), Some(identity), Some(not_after)) = (
            der.as_deref().and_then(hyperstor_pki::serial_of),
            der.as_deref().and_then(hyperstor_pki::identity_of),
            der.as_deref().and_then(hyperstor_pki::not_after_unix),
        ) else {
            return;
        };
        let rec = crate::certs::Issued {
            serial,
            identity: identity.uri(),
            not_after_unix: not_after,
            issued_unix: now_unix(),
            by: by.to_string(),
        };
        let certs = signing.certs.clone();
        let now = now_unix();
        let _ = tokio::task::spawn_blocking(move || certs.record(rec, now)).await;
    }

    /// Checks and burns a join token; returns its record.
    async fn redeem(&self, token: &str, root: &str) -> Result<Record, ConnectError> {
        let (pin, secret) =
            parse_token(token).ok_or_else(|| invalid("not a join token", "token_invalid"))?;
        let ours = hyperstor_pki::fingerprint(root).map_err(ConnectError::internal)?;
        if pin != ours {
            return Err(invalid(
                "the token is for another cluster's root",
                "token_invalid",
            ));
        }
        let hash = hyperstor_pki::sha256_hex(secret.as_bytes());
        let now = now_unix();
        match self.tokens(move |t| t.redeem(&hash, now)).await {
            Err(e) if hyperstor_api::reason_of(&e).as_deref() == Some("not_found") => {
                Err(with_reason(
                    ConnectError::permission_denied("unknown token"),
                    "token_invalid",
                ))
            }
            r => r,
        }
    }

    fn op(&self, id: &str) -> Result<Arc<Mutex<pb::Operation>>, ConnectError> {
        lock(&self.admin.ops)
            .iter()
            .find(|o| lock(o).id == id)
            .cloned()
            .ok_or_else(|| {
                with_reason(
                    ConnectError::not_found(format!("no operation {id}")),
                    "not_found",
                )
            })
    }
}

#[allow(refining_impl_trait)]
impl TokenService for Api {
    async fn create_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateTokenRequest>,
    ) -> ServiceResult<pb::CreateTokenResponse> {
        let target = format!("role {}", req.role);
        let who = self.authorize(&ctx, "CreateToken", &target, true)?;
        let (_, root) = self.signing()?;
        if !matches!(req.role, "node" | "meta") {
            return Err(invalid(
                format!("{}: a token is for role node or meta", req.role),
                "invalid_argument",
            ));
        }
        let ttl = match req.ttl_seconds {
            0 => TOKEN_TTL,
            t if t <= TOKEN_TTL_MAX => t,
            _ => {
                return Err(invalid(
                    format!("a token lives at most {TOKEN_TTL_MAX} s"),
                    "invalid_argument",
                ));
            }
        };
        let secret = random_hex(32);
        let pin = hyperstor_pki::fingerprint(root).map_err(ConnectError::internal)?;
        let hash = hyperstor_pki::sha256_hex(secret.as_bytes());
        let info = pb::Token {
            id: hash[..12].to_string(),
            role: req.role.to_string(),
            expires_unix: now_unix() + u64::from(ttl),
            created_by: who.clone(),
            ..Default::default()
        };
        let rec = Record {
            hash,
            id: info.id.clone(),
            role: info.role.clone(),
            expires_unix: info.expires_unix,
            used: false,
            revoked: false,
            created_by: who.clone(),
        };
        let now = now_unix();
        if let Err(e) = self.tokens(move |t| t.create(rec, now)).await {
            audit(
                &who,
                "CreateToken",
                &target,
                &format!("failed: {}", e.message.as_deref().unwrap_or("")),
            );
            return Err(e);
        }
        audit(
            &who,
            "CreateToken",
            &target,
            &format!("ok, token {} for {ttl} s", info.id),
        );
        Ok(pb::CreateTokenResponse {
            token: format!("hsj1-{pin}-{secret}"),
            info: buffa::MessageField::some(info),
            api_address: self.admin.api_address.clone(),
            ..Default::default()
        }
        .into())
    }

    async fn list_tokens(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListTokensRequest>,
    ) -> ServiceResult<pb::ListTokensResponse> {
        self.authorize(&ctx, "ListTokens", "tokens", false)?;
        let tokens = self.tokens(|t| t.list()).await?;
        Ok(pb::ListTokensResponse {
            tokens: tokens
                .into_iter()
                .map(|r| pb::Token {
                    id: r.id,
                    role: r.role,
                    expires_unix: r.expires_unix,
                    used: r.used,
                    revoked: r.revoked,
                    created_by: r.created_by,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
        .into())
    }

    async fn revoke_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RevokeTokenRequest>,
    ) -> ServiceResult<pb::RevokeTokenResponse> {
        let target = format!("token {}", req.id);
        let who = self.authorize(&ctx, "RevokeToken", &target, true)?;
        let (id, now) = (req.id.to_string(), now_unix());
        let r = self.tokens(move |t| t.revoke(&id, now)).await;
        audit(
            &who,
            "RevokeToken",
            &target,
            &match &r {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("failed: {}", e.message.as_deref().unwrap_or("")),
            },
        );
        r?;
        Ok(pb::RevokeTokenResponse::default().into())
    }

    async fn join(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::JoinRequest>,
    ) -> ServiceResult<pb::JoinResponse> {
        let peer = ctx.peer_addr().map_or("?".into(), |a| a.to_string());
        let caller = self.authorize_as(&ctx, "Join", "cluster", Need::Anyone)?;
        let (signing, root) = self.signing()?;
        let cluster = self.cluster.clone().unwrap_or_default();
        let r = async {
            // Burnt through the leader (a follower refuses and burns
            // nothing), which then adds the storage host itself.
            let token = self.redeem(req.token, root).await?;
            let who_token = format!("token {} from {peer}", token.id);
            match token.role.as_str() {
                "node" => {
                    let addrs: Vec<SocketAddr> = req
                        .addresses
                        .iter()
                        .map(|a| {
                            a.parse::<SocketAddr>()
                                .ok()
                                .filter(|a| !a.ip().is_unspecified() && a.port() != 0)
                                .ok_or_else(|| {
                                    invalid(
                                        format!("{a}: not an address a node can listen on"),
                                        "invalid_argument",
                                    )
                                })
                        })
                        .collect::<Result<_, _>>()?;
                    let rack = req.rack;
                    let node = self
                        .ask(|reply| Admin::AddNode {
                            rack,
                            addrs: addrs.clone(),
                            reply,
                        })
                        .await?
                        .map_err(|e| {
                            meta_error(e, ConnectError::failed_precondition, "node_conflict")
                        })?;
                    let mut ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
                    ips.sort();
                    ips.dedup();
                    let who = Identity {
                        cluster: cluster.clone(),
                        role: Role::Node,
                        id: node.to_string(),
                    };
                    let certificate = hyperstor_pki::sign_request(
                        &signing.ca,
                        req.csr,
                        &who,
                        &ips,
                        &[],
                        hyperstor_pki::LEAF_DAYS,
                    )
                    .map_err(|e| invalid(e, "bad_request"))?;
                    Ok((
                        format!("{who_token}: node {node}"),
                        pb::JoinResponse {
                            cluster_id: cluster.clone(),
                            certificate,
                            root: root.to_string(),
                            node,
                            map_addresses: signing.map_addresses.clone(),
                            ..Default::default()
                        },
                    ))
                }
                _ => {
                    if req.member_id == 0 {
                        return Err(invalid(
                            "a member joins with its member_id",
                            "invalid_argument",
                        ));
                    }
                    let mut ips: Vec<IpAddr> = req
                        .member_ips
                        .iter()
                        .map(|a| {
                            a.parse::<IpAddr>().map_err(|_| {
                                invalid(format!("{a}: not an IP address"), "invalid_argument")
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    ips.push(IpAddr::from([127, 0, 0, 1]));
                    ips.sort();
                    ips.dedup();
                    let who = Identity {
                        cluster: cluster.clone(),
                        role: Role::Meta,
                        id: req.member_id.to_string(),
                    };
                    let certificate = hyperstor_pki::sign_request(
                        &signing.ca,
                        req.csr,
                        &who,
                        &ips,
                        &["localhost".to_string()],
                        hyperstor_pki::LEAF_DAYS,
                    )
                    .map_err(|e| invalid(e, "bad_request"))?;
                    Ok((
                        format!("{who_token}: member {}", req.member_id),
                        pb::JoinResponse {
                            cluster_id: cluster.clone(),
                            certificate,
                            root: root.to_string(),
                            map_addresses: signing.map_addresses.clone(),
                            intermediate: signing.ca.cert.clone(),
                            intermediate_key: signing.ca.key.clone(),
                            cluster_file: signing.cluster_file.clone(),
                            ..Default::default()
                        },
                    ))
                }
            }
        }
        .await;
        match r {
            Ok((what, resp)) => {
                audit(&caller, "Join", "cluster", &format!("ok, {what}"));
                self.record_issued(&resp.certificate, &caller).await;
                Ok(resp.into())
            }
            Err(e) => {
                audit(
                    &caller,
                    "Join",
                    "cluster",
                    &format!("refused: {}", e.message.as_deref().unwrap_or("")),
                );
                Err(e)
            }
        }
    }
}

#[allow(refining_impl_trait)]
impl CaService for Api {
    async fn renew_certificate(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RenewCertificateRequest>,
    ) -> ServiceResult<pb::RenewCertificateResponse> {
        let caller = self.authorize_as(&ctx, "RenewCertificate", "own", Need::Identity)?;
        let (signing, root) = self.signing()?;
        let leaf = ctx.peer_certs().and_then(|c| c.first()).ok_or_else(|| {
            with_reason(
                ConnectError::unauthenticated("renewal needs the certificate being renewed"),
                "unauthenticated",
            )
        })?;
        let who = hyperstor_pki::identity_of(leaf)
            .ok_or_else(|| ConnectError::unauthenticated("no identity in the certificate"))?;
        let (ips, names) = hyperstor_pki::names_of_der(leaf).map_err(ConnectError::internal)?;
        let r = hyperstor_pki::sign_request(
            &signing.ca,
            req.csr,
            &who,
            &ips,
            &names,
            hyperstor_pki::LEAF_DAYS,
        );
        audit(
            &caller,
            "RenewCertificate",
            "own",
            &match &r {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("failed: {e}"),
            },
        );
        let certificate = r.map_err(|e| invalid(e, "bad_request"))?;
        self.record_issued(&certificate, &caller).await;
        Ok(pb::RenewCertificateResponse {
            certificate,
            root: root.to_string(),
            ..Default::default()
        }
        .into())
    }

    async fn list_certificates(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListCertificatesRequest>,
    ) -> ServiceResult<pb::ListCertificatesResponse> {
        self.authorize(&ctx, "ListCertificates", "certificates", false)?;
        let (signing, _) = self.signing()?;
        let certs = signing.certs.clone();
        let (issued, revoked) = tokio::task::spawn_blocking(move || certs.list())
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))?
            .map_err(|e| ConnectError::internal(format!("{e:?}")))?;
        let now = now_unix();
        let live: Vec<&crate::certs::Revoked> =
            revoked.iter().filter(|r| r.until_unix > now).collect();
        Ok(pb::ListCertificatesResponse {
            certificates: issued
                .iter()
                .filter(|i| i.not_after_unix > now)
                .map(|i| pb::Certificate {
                    serial: i.serial.clone(),
                    identity: i.identity.clone(),
                    not_after_unix: i.not_after_unix,
                    issued_unix: i.issued_unix,
                    issued_by: i.by.clone(),
                    revoked: live.iter().any(|r| r.serial == i.serial),
                    ..Default::default()
                })
                .collect(),
            revoked: live.iter().map(|r| revoked_pb(r)).collect(),
            ..Default::default()
        }
        .into())
    }

    async fn revoke_certificate(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RevokeCertificateRequest>,
    ) -> ServiceResult<pb::RevokeCertificateResponse> {
        let target = format!("certificate {}", req.serial);
        let who = self.authorize(&ctx, "RevokeCertificate", &target, true)?;
        let (signing, _) = self.signing()?;
        let (certs, serial, by, now) = (
            signing.certs.clone(),
            req.serial.to_string(),
            who.clone(),
            now_unix(),
        );
        let r = tokio::task::spawn_blocking(move || certs.revoke(&serial, &by, now))
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))?;
        let r = match r {
            Ok(r) => Ok(r),
            Err(e) => Err(self.token_error(e).await),
        };
        audit(
            &who,
            "RevokeCertificate",
            &target,
            &match &r {
                Ok(x) => format!("ok, refused until {}", x.until_unix),
                Err(e) => format!("failed: {}", e.message.as_deref().unwrap_or("")),
            },
        );
        Ok(pb::RevokeCertificateResponse {
            revoked: buffa::MessageField::some(revoked_pb(&r?)),
            ..Default::default()
        }
        .into())
    }
}

fn revoked_pb(r: &crate::certs::Revoked) -> pb::RevokedSerial {
    pb::RevokedSerial {
        serial: r.serial.clone(),
        until_unix: r.until_unix,
        revoked_unix: r.revoked_unix,
        revoked_by: r.by.clone(),
        ..Default::default()
    }
}

#[allow(refining_impl_trait)]
impl MapService for Api {
    async fn get_map_backup(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetMapBackupRequest>,
    ) -> ServiceResult<pb::GetMapBackupResponse> {
        let who = self.authorize(&ctx, "GetMapBackup", "map", true)?;
        let (signing, _) = self.signing()?;
        let Some(db) = signing.store.clone() else {
            return Err(with_reason(
                ConnectError::failed_precondition("this map service keeps no store (no --data)"),
                "no_store",
            ));
        };
        let b = tokio::task::spawn_blocking(move || crate::backup::Backup::take(&db))
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))?
            .map_err(|e| with_reason(ConnectError::unavailable(e), "not_formed"))?;
        audit(
            &who,
            "GetMapBackup",
            "map",
            &format!("ok, map epoch {}", b.epoch),
        );
        Ok(pb::GetMapBackupResponse {
            backup: b.encode(),
            cluster_id: b.cluster_id.to_string(),
            epoch: b.epoch,
            ..Default::default()
        }
        .into())
    }
}

#[allow(refining_impl_trait)]
impl NodeService for Api {
    async fn drain_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DrainNodeRequest>,
    ) -> ServiceResult<pb::DrainNodeResponse> {
        let (node, dry_run) = (req.node, req.dry_run);
        let target = format!("node {node}");
        // A dry run only reads.
        let who = self.authorize(&ctx, "DrainNode", &target, !dry_run)?;
        let devices: Vec<u32> = {
            let (map, ..) = self.published().await?;
            map.devices
                .values()
                .filter(|d| d.node == node)
                .map(|d| d.id)
                .collect()
        };
        let moves = self
            .ask(|reply| Admin::Drain {
                node,
                dry_run,
                reply,
            })
            .await?
            .map_err(|e| meta_error(e, ConnectError::not_found, "not_found"));
        if !dry_run {
            audit(
                &who,
                "DrainNode",
                &target,
                &match &moves {
                    Ok(m) => format!("ok, {} PG(s) move", m.len()),
                    Err(e) => format!("failed: {}", e.message.as_deref().unwrap_or("")),
                },
            );
        }
        let moves = moves?;
        let pb_moves = moves
            .iter()
            .map(|m| pb::PgMove {
                pool: m.pool,
                pg: m.pg,
                before: m.before.clone(),
                after: m.after.clone(),
                ..Default::default()
            })
            .collect();
        if dry_run {
            return Ok(pb::DrainNodeResponse {
                moves: pb_moves,
                ..Default::default()
            }
            .into());
        }
        let op = Arc::new(Mutex::new(pb::Operation {
            id: random_hex(8),
            kind: "drain".into(),
            target: target.clone(),
            state: pb::OperationState::OPERATION_STATE_RUNNING.into(),
            total: moves.len() as u32,
            message: format!("{} PG(s) to move off node {node}", moves.len()),
            started_unix: now_unix(),
            created_by: who,
            ..Default::default()
        }));
        {
            let mut ops = lock(&self.admin.ops);
            ops.push_front(op.clone());
            ops.truncate(OPS_KEPT);
        }
        let tx = self.tx.clone();
        let follow = op.clone();
        tokio::spawn(async move { follow_drain(tx, follow, node, devices, moves).await });
        let snapshot = lock(&op).clone();
        Ok(pb::DrainNodeResponse {
            operation: buffa::MessageField::some(snapshot),
            moves: pb_moves,
            ..Default::default()
        }
        .into())
    }
}

/// Follows a drain every half second until every moved PG is clean of
/// the node's devices and settled (`acting` = `up`); stops when the
/// operation is cancelled, and fails if the devices come back in.
async fn follow_drain(
    tx: hyperstor_reactor::chan::Sender<Admin>,
    op: Arc<Mutex<pb::Operation>>,
    node: u32,
    devices: Vec<u32>,
    moves: Vec<crate::PgMove>,
) {
    let running = pb::OperationState::OPERATION_STATE_RUNNING;
    // The drain is fenced and then published: until the published map has
    // the devices out, it's in progress, not undone.
    let mut seen_out = false;
    loop {
        if lock(&op).state.as_known() != Some(running) {
            return;
        }
        let (reply, rx) = oneshot();
        if tx.send(Admin::Published(reply)).is_err() {
            return;
        }
        let Some((map, ..)) = rx.recv().await else {
            return;
        };
        let any_in = map.devices.values().any(|d| d.node == node && d.up_in);
        let back_in = seen_out && any_in;
        seen_out |= !any_in;
        let left = moves
            .iter()
            .filter(|m| {
                map.pgs.get(&(m.pool, m.pg)).is_none_or(|pg| {
                    pg.acting != pg.up || pg.acting.iter().any(|d| devices.contains(d))
                })
            })
            .count() as u32;
        {
            let mut o = lock(&op);
            if o.state.as_known() != Some(running) {
                return;
            }
            o.done = o.total - left.min(o.total);
            if back_in {
                o.state = pb::OperationState::OPERATION_STATE_FAILED.into();
                o.message = format!("node {node}'s devices were brought back in");
                o.finished_unix = now_unix();
                return;
            }
            if left == 0 && seen_out {
                o.state = pb::OperationState::OPERATION_STATE_DONE.into();
                o.message = format!("node {node} drained: no PG lives on it");
                o.finished_unix = now_unix();
                return;
            }
            o.message = format!("{left} PG(s) still moving off node {node}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[allow(refining_impl_trait)]
impl OperationService for Api {
    async fn get_operation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetOperationRequest>,
    ) -> ServiceResult<pb::Operation> {
        self.authorize(
            &ctx,
            "GetOperation",
            &format!("operation {}", req.id),
            false,
        )?;
        let op = self.op(req.id)?;
        let snapshot = lock(&op).clone();
        Ok(snapshot.into())
    }

    async fn list_operations(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListOperationsRequest>,
    ) -> ServiceResult<pb::ListOperationsResponse> {
        self.authorize(&ctx, "ListOperations", "operations", false)?;
        Ok(pb::ListOperationsResponse {
            operations: lock(&self.admin.ops)
                .iter()
                .map(|o| lock(o).clone())
                .collect(),
            ..Default::default()
        }
        .into())
    }

    async fn cancel_operation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CancelOperationRequest>,
    ) -> ServiceResult<pb::Operation> {
        let target = format!("operation {}", req.id);
        let who = self.authorize(&ctx, "CancelOperation", &target, true)?;
        let op = self.op(req.id)?;
        let node = {
            let mut o = lock(&op);
            if o.state.as_known() != Some(pb::OperationState::OPERATION_STATE_RUNNING) {
                return Err(with_reason(
                    ConnectError::failed_precondition(format!("operation {} has finished", o.id)),
                    "finished",
                ));
            }
            o.state = pb::OperationState::OPERATION_STATE_CANCELLED.into();
            o.finished_unix = now_unix();
            o.target
                .strip_prefix("node ")
                .and_then(|n| n.parse::<u32>().ok())
        };
        // A cancelled drain brings the node's devices back in.
        let mut message = "cancelled".to_string();
        if let Some(node) = node {
            let (map, ..) = self.published().await?;
            let devices: Vec<u32> = map
                .devices
                .values()
                .filter(|d| d.node == node)
                .map(|d| d.id)
                .collect();
            for device in devices {
                let r = self
                    .ask(|reply| Admin::SetDeviceIn {
                        device,
                        up_in: true,
                        reply,
                    })
                    .await?;
                if let Err(e) = r {
                    message = format!("cancelled; device {device} not brought back in: {e}");
                    break;
                }
            }
            if !message.contains(':') {
                message = format!("cancelled; node {node}'s devices are back in");
            }
        }
        audit(&who, "CancelOperation", &target, &format!("ok, {message}"));
        let snapshot = {
            let mut o = lock(&op);
            o.message = message;
            o.clone()
        };
        Ok(snapshot.into())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tokens_parse_only_in_their_form() {
        let pin = "a".repeat(64);
        let secret = "0123456789abcdef".repeat(4);
        let t = format!("hsj1-{pin}-{secret}");
        assert_eq!(
            super::parse_token(&t),
            Some((pin.as_str(), secret.as_str()))
        );
        assert_eq!(
            super::parse_token(&format!(" {t}\n")),
            Some((pin.as_str(), secret.as_str())),
            "read from a file"
        );
        for (i, bad) in [
            format!("hsj2-{pin}-{secret}"),
            format!("hsj1-{pin}{secret}"),
            format!("hsj1-{}-{secret}", &pin[1..]),
            format!("hsj1-{pin}-{}g", &secret[1..]),
            String::new(),
        ]
        .iter()
        .enumerate()
        {
            // Named by position: tokens are never printed, even in tests.
            assert_eq!(super::parse_token(bad), None, "case {i}");
        }
    }
}
