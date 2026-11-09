//! `RealmService` (20-tenants.md §24.8, 04-decisions.md D248–D253): realms,
//! the tenants in them and the pools they may use, kept by
//! `crate::realms`. Pools are named here and kept by id, as placements are.

use super::{Api, with_reason};
use crate::realms::{self, DEFAULT_REALM, InPool, Realm, Realms};
use crate::tokens::TokenError;
use crate::{Admin, BucketDone, BucketOp};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{RealmService, RealmServiceExt};
use hyperstor_reactor::chan::oneshot;
use std::sync::Arc;

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    RealmServiceExt::register(api, router)
}

fn invalid(msg: impl Into<String>) -> ConnectError {
    with_reason(
        ConnectError::invalid_argument(msg.into()),
        "invalid_argument",
    )
}

/// The realms a call reaches (D262): every one, or those a person holds
/// the call's role in. Realms that don't exist reach nothing.
#[derive(Debug, Clone, Default)]
pub(super) struct Scope {
    ids: Option<Vec<u32>>,
    pub(super) realms: Realms,
}

impl Scope {
    /// Whether the call reaches the cluster as a whole.
    pub(super) fn whole(&self) -> bool {
        self.ids.is_none()
    }

    pub(super) fn realm(&self, id: u32) -> bool {
        self.ids.as_ref().is_none_or(|l| l.contains(&id))
    }

    /// Whether `tenant`'s realm is reached; tenant 0 is "default"'s.
    pub(super) fn tenant(&self, tenant: u32) -> bool {
        self.whole() || self.realm(self.realms.realm_of(tenant))
    }

    /// Whether operation `op` is seen (D280): the cluster's sees them all;
    /// in realms, those on their tenants' volumes, and a volume's recorded
    /// before tenants were (none) if `who` started it. The cluster's own
    /// (drains, scrubs, pools, devices) never.
    pub(super) fn op(&self, op: &crate::operations::Op, who: &str) -> bool {
        use crate::operations::Work;
        if self.whole() {
            return true;
        }
        match op.tenant {
            Some(t) => self.tenant(t),
            None => {
                op.created_by == who
                    && matches!(
                        op.work,
                        Work::Flatten { .. }
                            | Work::Copy { .. }
                            | Work::Move { .. }
                            | Work::Purge { .. }
                    )
            }
        }
    }

    /// Bucket `tenant/...` if reached; one outside answers as one that
    /// doesn't exist (D244).
    pub(super) fn bucket(
        &self,
        who: &str,
        op: &str,
        target: &str,
        tenant: u32,
    ) -> Result<(), ConnectError> {
        if self.tenant(tenant) {
            return Ok(());
        }
        let e = with_reason(ConnectError::not_found(format!("no {target}")), "not_found");
        super::refused(who, op, target, &e);
        Err(e)
    }

    /// Realm `name` ("" is "default") if reached.
    pub(super) fn realm_named(
        &self,
        who: &str,
        op: &str,
        target: &str,
        name: &str,
    ) -> Result<(), ConnectError> {
        match self.realms.id(name) {
            Some(id) if self.realm(id) => Ok(()),
            _ if self.whole() => Ok(()),
            _ => Err(self.outside(who, op, target, &format!("realm {name:?}"))),
        }
    }

    /// For a creation that names no tenant or realm, which would land in
    /// "default" (D281): in realms, refused with `reason` unless "default"
    /// is the only one reached, so it never lands in one by chance.
    pub(super) fn named(
        &self,
        who: &str,
        op: &str,
        target: &str,
        what: &str,
        reason: &str,
    ) -> Result<(), ConnectError> {
        match &self.ids {
            None => Ok(()),
            Some(ids) if ids[..] == [0] => Ok(()),
            Some(ids) => {
                let names: Vec<String> = ids.iter().map(|&i| self.realms.name(i)).collect();
                let e = with_reason(
                    ConnectError::invalid_argument(format!(
                        "name {what}: this call acts in realm {}",
                        names.join(", ")
                    )),
                    reason,
                );
                super::refused(who, op, target, &e);
                Err(e)
            }
        }
    }

    /// A refusal of something outside the realms reached, audited.
    pub(super) fn outside(&self, who: &str, op: &str, target: &str, what: &str) -> ConnectError {
        let e = with_reason(
            ConnectError::permission_denied(format!("{what} is outside the realms {who} acts in")),
            "outside_realm",
        );
        super::refused(who, op, target, &e);
        e
    }
}

impl Api {
    /// Runs `f` on the store off the API's thread; "not found" is `what`'s,
    /// and a refusal is a failed precondition with its reason.
    pub(super) async fn realm_rows<T: Send + 'static>(
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
            Ok(Err(TokenError::Refused { reason, why })) => {
                Err(with_reason(ConnectError::failed_precondition(why), reason))
            }
            Ok(Err(e)) => Err(self
                .store_error(e, "realms are written by the leader")
                .await),
            Err(e) => Err(ConnectError::internal(e.to_string())),
        }
    }

    /// Every container of `tenant` (every tenant's, with none) and the
    /// pools it is in, read from the cluster when asked (D254): buckets'
    /// index and data pools, and volumes', those in the trash too, which
    /// still hold their pool's space. A tenant's volumes of tenant 0 are
    /// "default"'s.
    pub(super) async fn held(&self, tenant: Option<u32>) -> Result<Vec<InPool>, ConnectError> {
        let mut out = Vec::new();
        let (map, ..) = self.published().await?;
        let name = |id: u32| {
            map.pools
                .get(&id)
                .map_or(format!("#{id}"), |p| p.name.clone())
        };
        let BucketDone::List(buckets) = self.bucket_op(BucketOp::List { tenant }).await? else {
            return Err(ConnectError::internal("a bucket list answered otherwise"));
        };
        for b in buckets {
            for pool in [b.index_pool, b.data_pool] {
                out.push(InPool {
                    tenant: b.tenant,
                    pool,
                    what: format!("bucket {}/{} (pool {})", b.tenant, b.name, name(pool)),
                });
            }
        }
        for pool in map.pools.keys().filter(|&&p| p != hyperstor_map::META_POOL) {
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::ListVolumes {
                    pool: *pool,
                    trash: true,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            for v in super::volume_result(rx.recv().await)? {
                if tenant.is_none_or(|t| t == v.tenant) {
                    out.push(InPool {
                        tenant: v.tenant,
                        pool: v.pool,
                        what: format!("volume {:032x} (pool {})", v.id, name(v.pool)),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Mirrors the realm rows' host bindings and tenants' realms into the
    /// map (D259), answered once storage nodes can read them.
    pub(super) async fn sync_realms(&self) -> Result<(), ConnectError> {
        let realms = self.realms().await?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SyncRealms { realms, reply })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        match rx.recv().await {
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(ConnectError::unavailable(e)),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    /// The realms, from this member's copy.
    pub(super) async fn realms(&self) -> Result<Realms, ConnectError> {
        self.realm_rows("realm", realms::load).await
    }

    /// Every realm, to name the realm of each row a list answers (D281):
    /// a limited call's, read once; read for the cluster's too, and
    /// "default" for all when they can't be read, rather than no list.
    pub(super) async fn realm_names(&self, scope: &Scope) -> Realms {
        if scope.whole() {
            self.realms().await.unwrap_or_default()
        } else {
            scope.realms.clone()
        }
    }

    /// What a call that [`Api::authorize_in`] limited to `reach` may see
    /// and change (D262). Realms are read only for a limited call.
    pub(super) async fn scope(&self, reach: Option<Vec<String>>) -> Result<Scope, ConnectError> {
        Ok(match reach {
            None => Scope::default(),
            Some(names) => {
                let realms = self.realms().await?;
                Scope {
                    ids: Some(names.iter().filter_map(|n| realms.id(n)).collect()),
                    realms,
                }
            }
        })
    }

    /// Volume `id` if `scope` reaches it: its tenant's realm is one of
    /// the scope's. One outside answers as one that doesn't exist, as
    /// another tenant's bucket does (D244). Read whatever its state, the
    /// trash's too; nothing is read for the cluster as a whole.
    pub(super) async fn volume_in(
        &self,
        scope: &Scope,
        id: u128,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<(), ConnectError> {
        if scope.whole() {
            return Ok(());
        }
        match self.volume_any(id).await? {
            v if scope.tenant(v.tenant) => Ok(()),
            _ => {
                let e = with_reason(
                    ConnectError::not_found(format!("no volume {id:032x}")),
                    "not_found",
                );
                super::refused(who, op, target, &e);
                Err(e)
            }
        }
    }

    /// Group snapshot `group` if `scope` reaches every volume with a
    /// snapshot in it; read only for a limited call. A group outside
    /// answers as one that doesn't exist.
    pub(super) async fn group_in(
        &self,
        scope: &Scope,
        group: u128,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<(), ConnectError> {
        if scope.whole() {
            return Ok(());
        }
        let (map, ..) = self.published().await?;
        for pool in map.pools.keys().filter(|&&p| p != hyperstor_map::META_POOL) {
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::ListVolumes {
                    pool: *pool,
                    trash: true,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            for v in super::volume_result(rx.recv().await)? {
                if v.snapshots.iter().any(|s| s.group == group) && !scope.tenant(v.tenant) {
                    let e = with_reason(
                        ConnectError::not_found(format!("no group snapshot {group:032x}")),
                        "not_found",
                    );
                    super::refused(who, op, target, &e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Refuses `pool` for a copy or move of `v` unless its tenant's realm
    /// may use it (D249), as creating a volume there would be.
    pub(super) async fn pool_for(
        &self,
        v: &crate::Volume,
        pool: u32,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<(), ConnectError> {
        let realms = self.realms().await?;
        let realm = realms.realm_of(v.tenant);
        if realms.may_use(realm, pool) {
            return Ok(());
        }
        let why = format!(
            "realm {} of tenant {} may not use pool {pool}",
            realms.name(realm),
            v.tenant
        );
        super::audit(who, op, target, &format!("failed: {why}"));
        Err(with_reason(
            ConnectError::failed_precondition(why),
            "pool_not_in_realm",
        ))
    }

    /// The id of realm `name`, which must exist.
    pub(super) async fn realm_id(&self, name: &str) -> Result<u32, ConnectError> {
        self.realms().await?.id(name).ok_or_else(|| {
            with_reason(
                ConnectError::not_found(format!("no realm {name:?}")),
                "not_found",
            )
        })
    }
}

#[allow(refining_impl_trait)]
impl RealmService for Api {
    async fn create_realm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateRealmRequest>,
    ) -> ServiceResult<pb::CreateRealmResponse> {
        let target = format!("realm {}", req.name);
        let who = self.authorize(&ctx, "CreateRealm", &target, true)?;
        let r = async {
            if !realms::valid_name(req.name) {
                return Err(invalid(format!(
                    "{:?}: a realm is named by 1-63 of a-z, 0-9 and -, and isn't {DEFAULT_REALM:?}",
                    req.name
                )));
            }
            if req.description.chars().count() > 200
                || req.description.chars().any(char::is_control)
            {
                return Err(invalid(
                    "a description has at most 200 characters, none of them control characters",
                ));
            }
            let realm = Realm {
                id: 0,
                name: req.name.to_string(),
                description: req.description.to_string(),
                created_unix: super::admin::now_unix(),
                created_by: who.clone(),
            };
            let row = realm.clone();
            let id = self
                .realm_rows("realm", move |db| realms::create(db, &row))
                .await?;
            Ok(Realm { id, ..realm })
        }
        .await;
        let r = Self::audited(&who, "CreateRealm", &target, r)?;
        Ok(pb::CreateRealmResponse {
            realm: buffa::MessageField::some(pb::Realm {
                id: r.id,
                name: r.name,
                description: r.description,
                created_unix: r.created_unix,
                created_by: r.created_by,
                ..Default::default()
            }),
            ..Default::default()
        }
        .into())
    }

    async fn list_realms(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListRealmsRequest>,
    ) -> ServiceResult<pb::ListRealmsResponse> {
        let (_, reach) = self.authorize_in(&ctx, "ListRealms", "realms", false)?;
        let scope = self.scope(reach).await?;
        let all = self.realms().await?;
        let (map, ..) = self.published().await?;
        let pools_of = |id: u32| -> (Vec<String>, Vec<u32>) {
            map.pools
                .values()
                .filter(|p| all.pools.get(&p.id).is_some_and(|l| l.contains(&id)))
                .map(|p| (p.name.clone(), p.id))
                .unzip()
        };
        let hosts_of = |id: u32| -> Vec<String> {
            all.hosts
                .iter()
                .filter(|(_, l)| l.contains(&id))
                .map(|(h, _)| h.clone())
                .collect()
        };
        let tenants_of = |id: u32| -> Vec<u32> {
            all.tenants
                .iter()
                .filter(|&(_, r)| *r == id)
                .map(|(t, _)| *t)
                .collect()
        };
        let (pools, pool_ids) = pools_of(0);
        let mut out = vec![pb::Realm {
            id: 0,
            name: DEFAULT_REALM.into(),
            description: "every tenant not put in another realm".into(),
            pools,
            pool_ids,
            hosts: hosts_of(0),
            ..Default::default()
        }];
        for r in &all.realms {
            let (pools, pool_ids) = pools_of(r.id);
            out.push(pb::Realm {
                id: r.id,
                name: r.name.clone(),
                description: r.description.clone(),
                created_unix: r.created_unix,
                created_by: r.created_by.clone(),
                tenants: tenants_of(r.id),
                pools,
                pool_ids,
                hosts: hosts_of(r.id),
                ..Default::default()
            });
        }
        out.retain(|r| scope.realm(r.id));
        let (open_pools, open_pool_ids) = map
            .pools
            .values()
            .filter(|p| p.id != hyperstor_map::META_POOL && !all.pools.contains_key(&p.id))
            .map(|p| (p.name.clone(), p.id))
            .unzip();
        Ok(pb::ListRealmsResponse {
            realms: out,
            open_pools,
            open_pool_ids,
            ..Default::default()
        }
        .into())
    }

    async fn delete_realm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteRealmRequest>,
    ) -> ServiceResult<pb::DeleteRealmResponse> {
        let target = format!("realm {}", req.name);
        let who = self.authorize(&ctx, "DeleteRealm", &target, true)?;
        let r = if req.name == DEFAULT_REALM {
            Err(invalid("the realm \"default\" can't be deleted"))
        } else {
            let name = req.name.to_string();
            self.realm_rows("such realm", move |db| realms::delete(db, &name))
                .await
        };
        Self::audited(&who, "DeleteRealm", &target, r)?;
        Ok(pb::DeleteRealmResponse::default().into())
    }

    async fn set_tenant_realm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetTenantRealmRequest>,
    ) -> ServiceResult<pb::SetTenantRealmResponse> {
        let target = format!("tenant {}: realm {}", req.tenant, req.realm);
        let who = self.authorize(&ctx, "SetTenantRealm", &target, true)?;
        let r = async {
            if req.tenant == 0 {
                return Err(invalid("a tenant is 1 or more"));
            }
            let realm = self.realm_id(req.realm).await?;
            let tenant = req.tenant;
            // Checked again in the write, against the realms as they are
            // then (D248).
            let held = self.held(Some(tenant)).await?;
            self.realm_rows("such realm", move |db| {
                realms::set_tenant(db, tenant, realm, &held)
            })
            .await?;
            // Bound hosts check volumes by their tenant's realm (D259).
            self.sync_realms().await
        }
        .await;
        Self::audited(&who, "SetTenantRealm", &target, r)?;
        Ok(pb::SetTenantRealmResponse::default().into())
    }

    async fn set_pool_realms(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetPoolRealmsRequest>,
    ) -> ServiceResult<pb::SetPoolRealmsResponse> {
        let names: Vec<&str> = req.realms.iter().map(|r| r.as_ref()).collect();
        let target = format!("pool {}: realms {}", req.pool, names.join(","));
        let who = self.authorize(&ctx, "SetPoolRealms", &target, true)?;
        let r = async {
            let (map, ..) = self.published().await?;
            let pool = match map.pools.get(&req.pool) {
                None => return Err(invalid(format!("no pool {}", req.pool))),
                Some(q) if q.id == hyperstor_map::META_POOL => {
                    return Err(invalid(format!("{} is the map service's own pool", q.name)));
                }
                Some(q) => q.id,
            };
            let all = self.realms().await?;
            let mut ids = Vec::new();
            for n in &names {
                let id = all
                    .id(n)
                    .ok_or_else(|| invalid(format!("no realm {n:?}")))?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            let held = self.held(None).await?;
            self.realm_rows("such realm", move |db| {
                realms::set_pool(db, pool, &ids, &held)
            })
            .await
        }
        .await;
        Self::audited(&who, "SetPoolRealms", &target, r)?;
        Ok(pb::SetPoolRealmsResponse::default().into())
    }

    async fn set_host_realms(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetHostRealmsRequest>,
    ) -> ServiceResult<pb::SetHostRealmsResponse> {
        let names: Vec<&str> = req.realms.iter().map(|r| r.as_ref()).collect();
        let target = format!("host {}: realms {}", req.host, names.join(","));
        let who = self.authorize(&ctx, "SetHostRealms", &target, true)?;
        let r = async {
            let host = req.host.to_string();
            if !realms::valid_host(&host) {
                return Err(invalid(
                    "a host is 1-64 of letters, digits, '.', '-' and '_'",
                ));
            }
            let all = self.realms().await?;
            let mut ids = Vec::new();
            for n in &names {
                let id = all
                    .id(n)
                    .ok_or_else(|| invalid(format!("no realm {n:?}")))?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            self.realm_rows("such realm", move |db| realms::set_host(db, &host, &ids))
                .await?;
            self.sync_realms().await
        }
        .await;
        Self::audited(&who, "SetHostRealms", &target, r)?;
        Ok(pb::SetHostRealmsResponse::default().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::{Op, State, Work};

    /// D280, D281: in realms, the operations on their tenants' volumes and
    /// one's own are seen, the cluster's are not; a creation naming no
    /// tenant or realm passes only for the cluster or "default" alone.
    #[test]
    fn realms_see_their_operations_and_name_their_creations() {
        let realms = Realms {
            realms: vec![crate::realms::Realm {
                id: 1,
                name: "acme".into(),
                description: String::new(),
                created_unix: 0,
                created_by: String::new(),
            }],
            tenants: [(7, 1)].into(),
            ..Default::default()
        };
        let scope = |ids: Option<Vec<u32>>| Scope {
            ids,
            realms: realms.clone(),
        };
        let op = |tenant: Option<u32>, by: &str| Op {
            id: "o".into(),
            work: Work::Purge {
                volume: 1,
                epoch: None,
            },
            target: String::new(),
            state: State::Running,
            done: 0,
            total: 1,
            message: String::new(),
            started_unix: 0,
            started_ms: 0,
            finished_unix: 0,
            created_by: by.into(),
            tenant,
        };
        let acme = scope(Some(vec![1]));
        assert!(acme.op(&op(Some(7), "ann"), "bob"));
        assert!(!acme.op(&op(Some(8), "ann"), "bob"));
        assert!(
            !acme.op(&op(Some(8), "bob"), "bob"),
            "another realm's, one's own"
        );
        assert!(
            acme.op(&op(None, "bob"), "bob"),
            "one's own, recorded before"
        );
        assert!(!acme.op(&op(None, "ann"), "bob"));
        let mut drain = op(None, "bob");
        drain.work = Work::Drain {
            node: 1,
            devices: vec![],
            pgs: vec![],
        };
        assert!(!acme.op(&drain, "bob"), "the cluster's own");
        assert!(scope(None).op(&op(None, "ann"), "bob"));
        assert_eq!(realms.realm_name(7), "acme");
        assert_eq!(realms.realm_name(8), "default");

        let named = |s: &Scope| s.named("bob", "CreateVolume", "t", "a tenant", "tenant_required");
        assert!(named(&scope(None)).is_ok());
        assert!(named(&scope(Some(vec![0]))).is_ok());
        let e = named(&acme).unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::InvalidArgument);
        assert!(
            e.message.as_deref().unwrap_or("").contains("realm acme"),
            "{e:?}"
        );
        assert!(named(&scope(Some(vec![0, 1]))).is_err());
    }
}
