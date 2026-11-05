//! `S3Service` (14-s3.md §14.2, 04-decisions.md D231, D232): S3's access
//! keys and placements, kept by `crate::s3keys`.
//!
//! - **Keys** are made here, the secret from the kernel's random source;
//!   the reply carries it once, and the store keeps it sealed with the
//!   cluster key (D162). A member without the cluster key can neither make
//!   keys nor answer gateways.
//! - **Gateways** (`s3` certificates, D233) fetch every key with its
//!   secret, and the placements, from any member's applied copy.
//! - **Placements** name pools of the map; setting one checks they exist,
//!   and that neither is a cache pool (S3's data must not be evicted).

use super::admin::random_bytes;
use super::{Api, Need, audit, with_reason};
use crate::PoolKind;
use crate::s3keys::{self, Placement, S3Key};
use crate::tokens::TokenError;
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{S3Service, S3ServiceExt};
use std::sync::Arc;

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    S3ServiceExt::register(api, router)
}

fn invalid(msg: impl Into<String>) -> ConnectError {
    with_reason(
        ConnectError::invalid_argument(msg.into()),
        "invalid_argument",
    )
}

fn placement_pb(p: Placement) -> pb::S3Placement {
    pb::S3Placement {
        name: p.name,
        data_pool: p.data_pool,
        index_pool: p.index_pool,
        ..Default::default()
    }
}

impl Api {
    /// The replicated store, where S3's rows live.
    fn s3_store(&self) -> Result<Arc<crate::store::Db>, ConnectError> {
        self.admin
            .signing
            .as_ref()
            .and_then(|s| s.store.clone())
            .ok_or_else(|| {
                with_reason(
                    ConnectError::failed_precondition(
                        "this map service keeps no store (no --data)",
                    ),
                    "no_store",
                )
            })
    }

    /// The cluster key, which seals and opens S3's secrets.
    fn cluster_key(&self) -> Result<Arc<hyperstor_pki::ClusterKey>, ConnectError> {
        self.admin
            .signing
            .as_ref()
            .and_then(|s| s.cluster_key.clone())
            .ok_or_else(|| {
                with_reason(
                    ConnectError::failed_precondition(
                        "this map service member has no cluster key (cluster.key beside the intermediate CA's key)",
                    ),
                    "no_cluster_key",
                )
            })
    }

    /// Runs `f` on the store off the API's thread; "not found" is `what`'s.
    async fn s3_rows<T: Send + 'static>(
        &self,
        what: &str,
        f: impl FnOnce(&crate::store::Db) -> Result<T, TokenError> + Send + 'static,
    ) -> Result<T, ConnectError> {
        let db = self.s3_store()?;
        match tokio::task::spawn_blocking(move || f(&db)).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(TokenError::NotFound)) => Err(with_reason(
                ConnectError::not_found(format!("no {what}")),
                "not_found",
            )),
            Ok(Err(e)) => Err(self.store_error(e).await),
            Err(e) => Err(ConnectError::internal(e.to_string())),
        }
    }

    /// Audits `r` as the outcome of `op` on `target`, then returns it.
    fn audited<T>(
        who: &str,
        op: &str,
        target: &str,
        r: Result<T, ConnectError>,
    ) -> Result<T, ConnectError> {
        let outcome = match &r {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("failed: {}", e.message.as_deref().unwrap_or("")),
        };
        audit(who, op, target, &outcome);
        r
    }
}

#[allow(refining_impl_trait)]
impl S3Service for Api {
    async fn create_s3_key(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateS3KeyRequest>,
    ) -> ServiceResult<pb::CreateS3KeyResponse> {
        let target = format!("tenant {}", req.tenant);
        let who = self.authorize(&ctx, "CreateS3Key", &target, true)?;
        if req.tenant == 0 {
            return Err(invalid("a key's tenant is 1 or more"));
        }
        if req.description.chars().count() > 200 || req.description.chars().any(char::is_control) {
            return Err(invalid(
                "a description has at most 200 characters, none of them control characters",
            ));
        }
        let key = self.cluster_key()?;
        let (id, secret) = s3keys::new_key(random_bytes);
        let k = S3Key {
            id: id.clone(),
            tenant: req.tenant,
            description: req.description.to_string(),
            created_unix: super::admin::now_unix(),
            created_by: who.clone(),
            sealed: key
                .seal(&s3keys::context(&id), secret.as_bytes())
                .map_err(ConnectError::internal)?,
        };
        let row = k.clone();
        let r = self
            .s3_rows("key", move |db| s3keys::create(db, &row))
            .await;
        // The audit names the key, never its secret.
        let r = Self::audited(&who, "CreateS3Key", &format!("{target}, key {id}"), r);
        r?;
        Ok(pb::CreateS3KeyResponse {
            key: buffa::MessageField::some(pb::S3Key {
                id: k.id,
                tenant: k.tenant,
                description: k.description,
                created_unix: k.created_unix,
                created_by: k.created_by,
                ..Default::default()
            }),
            secret,
            ..Default::default()
        }
        .into())
    }

    async fn list_s3_keys(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListS3KeysRequest>,
    ) -> ServiceResult<pb::ListS3KeysResponse> {
        self.authorize(&ctx, "ListS3Keys", "s3 keys", false)?;
        let keys = self.s3_rows("key", s3keys::keys).await?;
        Ok(pb::ListS3KeysResponse {
            keys: keys
                .into_iter()
                .map(|k| pb::S3Key {
                    id: k.id,
                    tenant: k.tenant,
                    description: k.description,
                    created_unix: k.created_unix,
                    created_by: k.created_by,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
        .into())
    }

    async fn revoke_s3_key(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RevokeS3KeyRequest>,
    ) -> ServiceResult<pb::RevokeS3KeyResponse> {
        let target = format!("key {}", req.id);
        let who = self.authorize(&ctx, "RevokeS3Key", &target, true)?;
        let r = if s3keys::valid_id(req.id) {
            let id = req.id.to_string();
            self.s3_rows("such key", move |db| s3keys::revoke(db, &id))
                .await
        } else {
            Err(invalid(format!("{:?}: not an access key id", req.id)))
        };
        Self::audited(&who, "RevokeS3Key", &target, r)?;
        Ok(pb::RevokeS3KeyResponse::default().into())
    }

    async fn set_s3_placement(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetS3PlacementRequest>,
    ) -> ServiceResult<pb::SetS3PlacementResponse> {
        let p = req.placement.as_option().cloned().unwrap_or_default();
        let target = format!("placement {}", p.name);
        let who = self.authorize(&ctx, "SetS3Placement", &target, true)?;
        let r = async {
            if !s3keys::valid_placement(p.name) {
                return Err(invalid(format!(
                    "{:?}: a placement is named by 1-63 of a-z, 0-9 and -",
                    p.name
                )));
            }
            let (map, ..) = self.published().await?;
            for name in [&p.data_pool, &p.index_pool] {
                match map.pools.values().find(|q| &q.name == name) {
                    None => return Err(invalid(format!("no pool {name:?}"))),
                    Some(q) if q.kind == PoolKind::Cache => {
                        return Err(invalid(format!(
                            "{name} is a cache pool: S3's objects must not be evicted"
                        )));
                    }
                    Some(_) => {}
                }
            }
            let place = Placement {
                name: p.name.to_string(),
                data_pool: p.data_pool.to_string(),
                index_pool: p.index_pool.to_string(),
            };
            self.s3_rows("placement", move |db| s3keys::set_placement(db, &place))
                .await
        }
        .await;
        let target = format!("{target}: data {}, index {}", p.data_pool, p.index_pool);
        Self::audited(&who, "SetS3Placement", &target, r)?;
        Ok(pb::SetS3PlacementResponse::default().into())
    }

    async fn delete_s3_placement(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteS3PlacementRequest>,
    ) -> ServiceResult<pb::DeleteS3PlacementResponse> {
        let target = format!("placement {}", req.name);
        let who = self.authorize(&ctx, "DeleteS3Placement", &target, true)?;
        let name = req.name.to_string();
        let r = self
            .s3_rows("such placement", move |db| {
                s3keys::delete_placement(db, &name)
            })
            .await;
        Self::audited(&who, "DeleteS3Placement", &target, r)?;
        Ok(pb::DeleteS3PlacementResponse::default().into())
    }

    async fn list_s3_placements(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListS3PlacementsRequest>,
    ) -> ServiceResult<pb::ListS3PlacementsResponse> {
        self.authorize(&ctx, "ListS3Placements", "s3 placements", false)?;
        let p = self.s3_rows("placement", s3keys::placements).await?;
        Ok(pb::ListS3PlacementsResponse {
            placements: p.into_iter().map(placement_pb).collect(),
            ..Default::default()
        }
        .into())
    }

    async fn get_s3_gateway(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetS3GatewayRequest>,
    ) -> ServiceResult<pb::GetS3GatewayResponse> {
        // Every few seconds from every gateway: refusals are audited,
        // answers aren't.
        self.authorize_as(&ctx, "GetS3Gateway", "s3 keys", Need::S3)?;
        let key = self.cluster_key()?;
        let (keys, placements) = self
            .s3_rows("key", |db| Ok((s3keys::keys(db)?, s3keys::placements(db)?)))
            .await?;
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let Some(secret) = key.open(&s3keys::context(&k.id), &k.sealed) else {
                // Sealed with another key (a member that made its own):
                // that key can't be served; the others can.
                eprintln!(
                    "hyperstor-meta: S3 key {} doesn't open with this member's cluster key",
                    k.id
                );
                continue;
            };
            out.push(pb::S3GatewayKey {
                id: k.id,
                secret: String::from_utf8_lossy(&secret).into_owned(),
                tenant: k.tenant,
                ..Default::default()
            });
        }
        Ok(pb::GetS3GatewayResponse {
            keys: out,
            placements: placements.into_iter().map(placement_pb).collect(),
            ..Default::default()
        }
        .into())
    }
}
