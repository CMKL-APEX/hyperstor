//! `S3Service` (14-s3.md §14.2, 04-decisions.md D231, D232): S3's access
//! keys and placements, kept by `crate::s3keys`.
//!
//! - **Keys** are made here, the secret from the kernel's random source;
//!   the reply carries it once, and the store keeps it sealed with the
//!   cluster key (D162). A member without the cluster key can neither make
//!   keys nor answer gateways.
//! - **Gateways** (`s3` certificates, D233) fetch every key with its
//!   secret, and the placements, from any member's applied copy.
//! - **Placements** name pools of the map, kept by id so a renamed pool
//!   keeps its placements; setting one checks they exist, that neither
//!   is a cache pool (S3's data must not be evicted) or `.meta`, and that
//!   the placement's realm may use them (D249).
//! - **Realms** (D248, D250): a gateway serves one, and is answered only
//!   its tenants' keys and its placements.

use super::admin::random_bytes;
use super::{Api, Need, audit, volume_result, with_reason};
use crate::PoolKind;
use crate::realms::{self, Realms};
use crate::s3keys::{self, Placement, S3Key};
use crate::tokens::TokenError;
use crate::{Admin, BucketDone, BucketOp, BucketRow};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{S3Service, S3ServiceExt};
use hyperstor_reactor::chan::oneshot;
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

/// `p` as the API answers it: the pools' ids, and their names in `map`
/// now (empty for a pool deleted since).
fn placement_pb(p: Placement, map: &crate::ClusterMap, realms: &Realms) -> pb::S3Placement {
    let name = |id: u32| {
        map.pools
            .get(&id)
            .map(|q| q.name.clone())
            .unwrap_or_default()
    };
    pb::S3Placement {
        data_pool: name(p.data_pool),
        index_pool: name(p.index_pool),
        data_pool_id: p.data_pool,
        index_pool_id: p.index_pool,
        realm: realms.name(p.realm),
        name: p.name,
        ..Default::default()
    }
}

impl Api {
    /// The replicated store, where S3's rows live.
    pub(super) fn s3_store(&self) -> Result<Arc<crate::store::Db>, ConnectError> {
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
            Ok(Err(e)) => Err(self
                .store_error(e, "S3 keys and placements are written by the leader")
                .await),
            Err(e) => Err(ConnectError::internal(e.to_string())),
        }
    }

    /// Audits `r` as the outcome of `op` on `target`, then returns it.
    pub(super) fn audited<T>(
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

impl Api {
    /// Runs a bucket operation as a client of the cluster (D254).
    pub(super) async fn bucket_op(&self, op: BucketOp) -> Result<BucketDone, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Buckets { op, reply })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        volume_result(rx.recv().await)
    }

    /// Buckets as administrators see them: pools and realm named now.
    async fn buckets_pb(&self, rows: Vec<BucketRow>) -> Result<Vec<pb::S3Bucket>, ConnectError> {
        let (map, ..) = self.published().await?;
        let realms = self.realms().await?;
        let name = |id: u32| {
            map.pools
                .get(&id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        Ok(rows
            .into_iter()
            .map(|b| pb::S3Bucket {
                tenant: b.tenant,
                created_unix: b.created_ms / 1000,
                data_pool: name(b.data_pool),
                index_pool: name(b.index_pool),
                data_pool_id: b.data_pool,
                index_pool_id: b.index_pool,
                realm: realms.name(realms.realm_of(b.tenant)),
                counted: b.counts.is_some(),
                objects: b.counts.map_or(0, |c| c.0),
                bytes: b.counts.map_or(0, |c| c.1),
                name: b.name,
                ..Default::default()
            })
            .collect())
    }
}

/// A tenant and bucket name as a request gives them.
fn bucket_of(tenant: u32, name: &str) -> Result<(u32, String), ConnectError> {
    if tenant == 0 {
        return Err(invalid("a bucket's tenant is 1 or more"));
    }
    if !hyperstor_s3::valid_bucket_name(name) {
        return Err(invalid(format!("{name:?} isn't a bucket name")));
    }
    Ok((tenant, name.to_string()))
}

#[allow(refining_impl_trait)]
impl S3Service for Api {
    async fn list_s3_buckets(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListS3BucketsRequest>,
    ) -> ServiceResult<pb::ListS3BucketsResponse> {
        let (_, reach) = self.authorize_in(&ctx, "ListS3Buckets", "s3 buckets", false)?;
        let scope = self.scope(reach).await?;
        let tenant = (req.tenant != 0).then_some(req.tenant);
        let BucketDone::List(mut rows) = self.bucket_op(BucketOp::List { tenant }).await? else {
            return Err(ConnectError::internal("not a list"));
        };
        rows.retain(|b| scope.tenant(b.tenant));
        Ok(pb::ListS3BucketsResponse {
            buckets: self.buckets_pb(rows).await?,
            ..Default::default()
        }
        .into())
    }

    async fn get_s3_bucket(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetS3BucketRequest>,
    ) -> ServiceResult<pb::GetS3BucketResponse> {
        let target = format!("bucket {}/{}", req.tenant, req.name);
        let (who, reach) = self.authorize_in(&ctx, "GetS3Bucket", &target, false)?;
        let (tenant, name) = bucket_of(req.tenant, req.name)?;
        self.scope(reach)
            .await?
            .bucket(&who, "GetS3Bucket", &target, tenant)?;
        let BucketDone::List(rows) = self.bucket_op(BucketOp::Get { tenant, name }).await? else {
            return Err(ConnectError::internal("not a list"));
        };
        Ok(pb::GetS3BucketResponse {
            bucket: self.buckets_pb(rows).await?.into_iter().next().into(),
            ..Default::default()
        }
        .into())
    }

    async fn delete_s3_bucket(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteS3BucketRequest>,
    ) -> ServiceResult<pb::DeleteS3BucketResponse> {
        let target = format!("bucket {}/{}", req.tenant, req.name);
        let (who, reach) = self.authorize_in(&ctx, "DeleteS3Bucket", &target, true)?;
        self.scope(reach)
            .await?
            .bucket(&who, "DeleteS3Bucket", &target, req.tenant)?;
        let r = match bucket_of(req.tenant, req.name) {
            Ok((tenant, name)) => self
                .bucket_op(BucketOp::Delete { tenant, name })
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        };
        Self::audited(&who, "DeleteS3Bucket", &target, r)?;
        Ok(pb::DeleteS3BucketResponse::default().into())
    }

    async fn create_s3_key(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateS3KeyRequest>,
    ) -> ServiceResult<pb::CreateS3KeyResponse> {
        let target = format!("tenant {}", req.tenant);
        let (who, reach) = self.authorize_in(&ctx, "CreateS3Key", &target, true)?;
        if req.tenant == 0 {
            return Err(invalid("a key's tenant is 1 or more"));
        }
        let scope = self.scope(reach).await?;
        if !scope.tenant(req.tenant) {
            return Err(scope.outside(&who, "CreateS3Key", &target, &target));
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
        let (_, reach) = self.authorize_in(&ctx, "ListS3Keys", "s3 keys", false)?;
        let scope = self.scope(reach).await?;
        let keys = self.s3_rows("key", s3keys::keys).await?;
        let realms = self.realms().await?;
        Ok(pb::ListS3KeysResponse {
            keys: keys
                .into_iter()
                .filter(|k| scope.tenant(k.tenant))
                .map(|k| pb::S3Key {
                    realm: realms.name(realms.realm_of(k.tenant)),
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
        let (who, reach) = self.authorize_in(&ctx, "RevokeS3Key", &target, true)?;
        let scope = self.scope(reach).await?;
        if !scope.whole() {
            // Another realm's key answers as one that doesn't exist.
            let keys = self.s3_rows("key", s3keys::keys).await?;
            if !keys
                .iter()
                .any(|k| k.id == req.id && scope.tenant(k.tenant))
            {
                let e = with_reason(
                    ConnectError::not_found(format!("no key {:?}", req.id)),
                    "not_found",
                );
                super::refused(&who, "RevokeS3Key", &target, &e);
                return Err(e);
            }
        }
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
        let target = match p.realm {
            "" | realms::DEFAULT_REALM => format!("placement {}", p.name),
            r => format!("realm {r}'s placement {}", p.name),
        };
        let (who, reach) = self.authorize_in(&ctx, "SetS3Placement", &target, true)?;
        let scope = self.scope(reach).await?;
        if p.realm.is_empty() {
            scope.named(
                &who,
                "SetS3Placement",
                &target,
                "the placement's realm",
                "realm_required",
            )?;
        }
        scope.realm_named(&who, "SetS3Placement", &target, p.realm)?;
        let r = async {
            if !s3keys::valid_placement(p.name) {
                return Err(invalid(format!(
                    "{:?}: a placement is named by 1-63 of a-z, 0-9 and -",
                    p.name
                )));
            }
            let (map, ..) = self.published().await?;
            let mut ids = Vec::new();
            for name in [&p.data_pool, &p.index_pool] {
                match map.pools.values().find(|q| &q.name == name) {
                    None => return Err(invalid(format!("no pool {name:?}"))),
                    Some(q) if q.kind == PoolKind::Cache => {
                        return Err(invalid(format!(
                            "{name} is a cache pool: S3's objects must not be evicted"
                        )));
                    }
                    Some(q) if q.id == hyperstor_map::META_POOL => {
                        return Err(invalid(format!("{name} is the map service's own pool")));
                    }
                    Some(q) => ids.push(q.id),
                }
            }
            let place = Placement {
                name: p.name.to_string(),
                data_pool: ids[0],
                index_pool: ids[1],
                realm: self.realm_id(p.realm).await?,
            };
            self.realm_rows("such realm", move |db| realms::set_placement(db, &place))
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
        let target = match req.realm {
            "" | realms::DEFAULT_REALM => format!("placement {}", req.name),
            r => format!("realm {r}'s placement {}", req.name),
        };
        let (who, reach) = self.authorize_in(&ctx, "DeleteS3Placement", &target, true)?;
        self.scope(reach)
            .await?
            .realm_named(&who, "DeleteS3Placement", &target, req.realm)?;
        let name = req.name.to_string();
        let r = async {
            let realm = self.realm_id(req.realm).await?;
            self.s3_rows("such placement", move |db| {
                s3keys::delete_placement(db, realm, &name)
            })
            .await
        }
        .await;
        Self::audited(&who, "DeleteS3Placement", &target, r)?;
        Ok(pb::DeleteS3PlacementResponse::default().into())
    }

    async fn list_s3_placements(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListS3PlacementsRequest>,
    ) -> ServiceResult<pb::ListS3PlacementsResponse> {
        let (_, reach) = self.authorize_in(&ctx, "ListS3Placements", "s3 placements", false)?;
        let scope = self.scope(reach).await?;
        let mut p = self.s3_rows("placement", s3keys::placements).await?;
        p.retain(|p| scope.realm(p.realm));
        let realms = self.realms().await?;
        let (map, ..) = self.published().await?;
        Ok(pb::ListS3PlacementsResponse {
            placements: p
                .into_iter()
                .map(|p| placement_pb(p, &map, &realms))
                .collect(),
            ..Default::default()
        }
        .into())
    }

    async fn get_s3_gateway(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetS3GatewayRequest>,
    ) -> ServiceResult<pb::GetS3GatewayResponse> {
        // Every few seconds from every gateway: refusals are audited,
        // answers aren't.
        self.authorize_as(&ctx, "GetS3Gateway", "s3 keys", Need::S3)?;
        let key = self.cluster_key()?;
        let (keys, placements, realms) = self
            .s3_rows("key", |db| {
                Ok((
                    s3keys::keys(db)?,
                    s3keys::placements(db)?,
                    realms::load(db)?,
                ))
            })
            .await?;
        // Only the realm's tenants' keys and its placements (D250): another
        // realm's key is unknown here.
        let realm = realms.id(req.realm).ok_or_else(|| {
            with_reason(
                ConnectError::not_found(format!("no realm {:?}", req.realm)),
                "not_found",
            )
        })?;
        let (map, ..) = self.published().await?;
        let mut out = Vec::with_capacity(keys.len());
        for k in keys
            .into_iter()
            .filter(|k| realms.realm_of(k.tenant) == realm)
        {
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
            placements: placements
                .into_iter()
                .filter(|p| p.realm == realm)
                .map(|p| placement_pb(p, &map, &realms))
                .collect(),
            ..Default::default()
        }
        .into())
    }
}
