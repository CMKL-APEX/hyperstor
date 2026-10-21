//! Joining hosts, renewing certificates and long operations
//! (spec/administration.md §23.4.2, §23.5, §23.6, §23.8): `TokenService`,
//! `CaService`, `NodeService` and `OperationService`.
//!
//! - **Join tokens** (D57) are `hsj1-<root fingerprint>-<secret>`. The
//!   member keeps only the secret's SHA-256, the role, the expiry and
//!   whether it was used: a token is single-use and burnt by its first
//!   join, successful or not, unless the member asked doesn't lead and
//!   so did nothing. Tokens are held in memory by the member that
//!   made them, so a restart revokes them all (implementation-notes I80).
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
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{
    CaService, CaServiceExt, NodeService, NodeServiceExt, OperationService, OperationServiceExt,
    TokenService, TokenServiceExt,
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
}

/// Tokens live an hour by default and a week at most.
const TOKEN_TTL: u32 = 3600;
const TOKEN_TTL_MAX: u32 = 7 * 86400;
const OPS_KEPT: usize = 100;

struct TokenEntry {
    hash: String,
    info: pb::Token,
}

pub(super) struct State {
    signing: Option<Signing>,
    root: Option<String>,
    api_address: String,
    tokens: Mutex<Vec<TokenEntry>>,
    ops: Mutex<VecDeque<Arc<Mutex<pb::Operation>>>>,
}

impl State {
    pub(super) fn new(signing: Option<Signing>, root: Option<String>, api_address: String) -> Self {
        State {
            signing,
            root,
            api_address,
            tokens: Mutex::new(Vec::new()),
            ops: Mutex::new(VecDeque::new()),
        }
    }
}

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    let router = TokenServiceExt::register(api.clone(), router);
    let router = CaServiceExt::register(api.clone(), router);
    let router = NodeServiceExt::register(api.clone(), router);
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

    /// Checks and burns a join token for `role`; returns its id.
    fn redeem(&self, token: &str, root: &str) -> Result<pb::Token, ConnectError> {
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
        let mut tokens = lock(&self.admin.tokens);
        let t = tokens.iter_mut().find(|t| t.hash == hash).ok_or_else(|| {
            with_reason(
                ConnectError::permission_denied(
                    "unknown token (tokens don't survive a restart of the member that made them)",
                ),
                "token_invalid",
            )
        })?;
        let refuse = |why: &str, reason: &str| {
            with_reason(
                ConnectError::permission_denied(format!("token {}: {why}", t.info.id)),
                reason,
            )
        };
        if t.info.revoked {
            return Err(refuse("revoked", "token_revoked"));
        }
        if t.info.used {
            return Err(refuse("already used", "token_used"));
        }
        if t.info.expires_unix <= now_unix() {
            return Err(refuse("expired", "token_expired"));
        }
        t.info.used = true;
        Ok(t.info.clone())
    }

    /// Makes a token usable again after a join that did nothing.
    fn unredeem(&self, id: &str) {
        if let Some(t) = lock(&self.admin.tokens)
            .iter_mut()
            .find(|t| t.info.id == id)
        {
            t.info.used = false;
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
        if req.role == "node" {
            // Only the leader adds hosts to the map, and tokens are this
            // member's: a node token made elsewhere couldn't be used.
            self.ask(Admin::Leads).await?.map_err(|e| {
                meta_error(
                    format!("{e}: make storage tokens on the leader's host"),
                    ConnectError::failed_precondition,
                    "not_leader",
                )
            })?;
        }
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
        {
            let mut tokens = lock(&self.admin.tokens);
            // Forget tokens a day past their expiry.
            let horizon = now_unix().saturating_sub(86400);
            tokens.retain(|t| t.info.expires_unix > horizon);
            tokens.push(TokenEntry {
                hash,
                info: info.clone(),
            });
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
        Ok(pb::ListTokensResponse {
            tokens: lock(&self.admin.tokens)
                .iter()
                .map(|t| t.info.clone())
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
        let found = lock(&self.admin.tokens)
            .iter_mut()
            .find(|t| t.info.id == req.id)
            .map(|t| t.info.revoked = true)
            .is_some();
        audit(
            &who,
            "RevokeToken",
            &target,
            if found { "ok" } else { "failed: no such token" },
        );
        if !found {
            return Err(with_reason(
                ConnectError::not_found(format!("no token {}", req.id)),
                "not_found",
            ));
        }
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
            let token = self.redeem(req.token, root)?;
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
                            let e =
                                meta_error(e, ConnectError::failed_precondition, "node_conflict");
                            if hyperstor_api::reason_of(&e).as_deref() == Some("not_leader") {
                                // Nothing was done: the token may be used again.
                                self.unredeem(&token.id);
                            }
                            e
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
        Ok(pb::RenewCertificateResponse {
            certificate: r.map_err(|e| invalid(e, "bad_request"))?,
            root: root.to_string(),
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
            let (map, _) = self.published().await?;
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
        let Some((map, _)) = rx.recv().await else {
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
            let (map, _) = self.published().await?;
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
        for bad in [
            format!("hsj2-{pin}-{secret}"),
            format!("hsj1-{pin}{secret}"),
            format!("hsj1-{}-{secret}", &pin[1..]),
            format!("hsj1-{pin}-{}g", &secret[1..]),
            String::new(),
        ] {
            assert_eq!(super::parse_token(&bad), None, "{bad}");
        }
    }
}
