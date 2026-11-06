//! `RealmService` (20-tenants.md §24.8, 04-decisions.md D248–D253): realms,
//! the tenants in them and the pools they may use, kept by
//! `crate::realms`. Pools are named here and kept by id, as placements are.

use super::{Api, audit, with_reason};
use crate::realms::{self, DEFAULT_REALM, Realm, Realms};
use crate::tokens::TokenError;
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{RealmService, RealmServiceExt};
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

    /// The realms, from this member's copy.
    pub(super) async fn realms(&self) -> Result<Realms, ConnectError> {
        self.realm_rows("realm", realms::load).await
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
        self.authorize(&ctx, "ListRealms", "realms", false)?;
        let all = self.realms().await?;
        let (map, ..) = self.published().await?;
        let pools_of = |id: u32| -> Vec<String> {
            map.pools
                .values()
                .filter(|p| all.pools.get(&p.id).is_some_and(|l| l.contains(&id)))
                .map(|p| p.name.clone())
                .collect()
        };
        let tenants_of = |id: u32| -> Vec<u32> {
            all.tenants
                .iter()
                .filter(|&(_, r)| *r == id)
                .map(|(t, _)| *t)
                .collect()
        };
        let mut out = vec![pb::Realm {
            id: 0,
            name: DEFAULT_REALM.into(),
            description: "every tenant not put in another realm".into(),
            pools: pools_of(0),
            ..Default::default()
        }];
        for r in &all.realms {
            out.push(pb::Realm {
                id: r.id,
                name: r.name.clone(),
                description: r.description.clone(),
                created_unix: r.created_unix,
                created_by: r.created_by.clone(),
                tenants: tenants_of(r.id),
                pools: pools_of(r.id),
                ..Default::default()
            });
        }
        Ok(pb::ListRealmsResponse {
            realms: out,
            open_pools: map
                .pools
                .values()
                .filter(|p| p.id != hyperstor_map::META_POOL && !all.pools.contains_key(&p.id))
                .map(|p| p.name.clone())
                .collect(),
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
            self.realm_rows("such realm", move |db| {
                realms::set_tenant(db, tenant, realm)
            })
            .await
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
            self.realm_rows("such realm", move |db| realms::set_pool(db, pool, &ids))
                .await
        }
        .await;
        Self::audited(&who, "SetPoolRealms", &target, r)?;
        Ok(pb::SetPoolRealmsResponse::default().into())
    }
}
