//! `DatasetService` (29-hsfs.md §32.2, §32.3; 04-decisions.md D360, D361):
//! datasets by name and their properties, kept by `crate::datasets`.
//!
//! A tenant's properties are changed by whoever reaches the tenant; a
//! project's and a dataset's by whoever reaches the project, as for its
//! volumes and buckets (D301). One outside answers as one that doesn't
//! exist (D244).

use super::realms::Scope;
use super::{Api, audit, volume_result, with_reason};
use crate::datasets::{self, Entry, Id, Kind, Level, Path, Props, Source, Unresolved};
use crate::tenancy::Tenancy;
use crate::{Admin, BucketDone, BucketOp};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{DatasetService, DatasetServiceExt};
use hyperstor_reactor::chan::oneshot;
use std::sync::Arc;

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    DatasetServiceExt::register(api, router)
}

fn invalid(msg: impl Into<String>) -> ConnectError {
    with_reason(
        ConnectError::invalid_argument(msg.into()),
        "invalid_argument",
    )
}

fn not_found(what: &str) -> ConnectError {
    with_reason(ConnectError::not_found(format!("no {what}")), "not_found")
}

/// What a path resolved to, with what its properties come through.
struct Resolved {
    path: Path,
    name: String,
    kind: &'static str,
    chain: Vec<Level>,
    /// The tenant whose realm may use the pools named.
    tenant: u32,
    reached: bool,
}

impl Api {
    /// Every live dataset: buckets and volumes not in the trash, with their
    /// tenant and project as the records say now (D300, D303).
    pub(super) async fn datasets(&self, tenancy: &Tenancy) -> Result<Vec<Entry>, ConnectError> {
        let mut out = Vec::new();
        let BucketDone::List(buckets) = self.bucket_op(BucketOp::List { tenant: None }).await?
        else {
            return Err(ConnectError::internal("a bucket list answered otherwise"));
        };
        for b in buckets {
            out.push(Entry {
                id: Id::Bucket(b.table),
                tenant: b.tenant,
                project: tenancy.bucket_project(b.tenant, b.project),
                name: b.name,
                pool: b.data_pool,
            });
        }
        let (map, ..) = self.published().await?;
        for &pool in map.pools.keys().filter(|&&p| p != hyperstor_map::META_POOL) {
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::ListVolumes {
                    pool,
                    trash: false,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            for v in volume_result(rx.recv().await)? {
                let tenant = tenancy.tenant_of(v.tenant, v.project);
                out.push(Entry {
                    id: Id::Volume(v.id),
                    tenant,
                    project: tenancy.project_of(tenant, v.project),
                    name: v.name,
                    pool: v.pool,
                });
            }
        }
        Ok(out)
    }

    /// Refuses a new dataset of `tenant`'s `project` named `name` when
    /// another live one there has it (D360, I459), audited as `op`.
    pub(super) async fn name_free(
        &self,
        tenant: u32,
        project: u32,
        name: &str,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<(), ConnectError> {
        let tenancy = self.tenancy().await?;
        let tenant = tenancy.tenant_of(tenant, project);
        let project = tenancy.project_of(tenant, project);
        if project == 0 || name.is_empty() {
            return Ok(());
        }
        let entries = self.datasets(&tenancy).await?;
        match datasets::clash(&tenancy, &entries, tenant, project, name) {
            None => Ok(()),
            Some(why) => {
                audit(who, op, target, &format!("failed: {why}"));
                Err(with_reason(
                    ConnectError::already_exists(format!(
                        "{why}: a dataset's name is unique in its project"
                    )),
                    "dataset_exists",
                ))
            }
        }
    }

    /// The name a dataset of `tenant`'s `project` takes in place of `name`
    /// when its caller named none: `name-SUFFIX`, `-2`… if `name` is taken
    /// in the project, else `name`.
    pub(super) async fn free_name(
        &self,
        tenant: u32,
        project: u32,
        name: &str,
        suffix: &str,
    ) -> Result<String, ConnectError> {
        let tenancy = self.tenancy().await?;
        let tenant = tenancy.tenant_of(tenant, project);
        let project = tenancy.project_of(tenant, project);
        let entries = self.datasets(&tenancy).await?;
        if datasets::clash(&tenancy, &entries, tenant, project, name).is_none() {
            return Ok(name.into());
        }
        Ok(datasets::free_name(&entries, tenant, project, name, suffix))
    }

    /// The pool a new volume of `tenant`'s `project` goes to when its
    /// creation names none: its project's or tenant's `pool` property.
    pub(super) async fn default_pool(
        &self,
        tenant: u32,
        project: u32,
    ) -> Result<Option<u32>, ConnectError> {
        let tenancy = self.tenancy().await?;
        let tenant = tenancy.tenant_of(tenant, project);
        let project = tenancy.project_of(tenant, project);
        let props = self.realm_rows("properties", datasets::load).await?;
        Ok(props
            .get(&datasets::chain(None, tenant, project), "pool")
            .and_then(|p| p.parse().ok()))
    }

    /// The volumes whose `readonly` is on, as their chunks are named
    /// (D420): all but those a running copy or move writes to, which take
    /// it once it is done.
    #[allow(clippy::type_complexity)]
    async fn readonly_volumes(
        &self,
    ) -> Result<
        (
            std::collections::BTreeSet<(u128, u32)>,
            std::collections::BTreeSet<(u32, String)>,
        ),
        ConnectError,
    > {
        use crate::operations::Work;
        let tenancy = self.tenancy().await?;
        let props = self.realm_rows("properties", datasets::load).await?;
        let busy: std::collections::BTreeSet<u128> = self
            .admin
            .runner
            .store(|s| s.list())
            .await
            .map_err(|e| ConnectError::unavailable(format!("operations: {e:?}")))?
            .into_iter()
            .filter(|o| o.running())
            .filter_map(|o| match o.work {
                Work::Copy { new_id, .. } => Some(new_id),
                Work::Move { volume, .. } => Some(volume),
                _ => None,
            })
            .collect();
        let mut out = std::collections::BTreeSet::new();
        let (map, ..) = self.published().await?;
        for &pool in map.pools.keys().filter(|&&p| p != hyperstor_map::META_POOL) {
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::ListVolumes {
                    pool,
                    trash: false,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            for v in volume_result(rx.recv().await)? {
                let tenant = tenancy.tenant_of(v.tenant, v.project);
                let project = tenancy.project_of(tenant, v.project);
                let at = datasets::chain(Some(Level::Volume(v.id)), tenant, project);
                if props.get(&at, "readonly") == Some("on") && !busy.contains(&v.id) {
                    out.insert((v.lineage, v.branch));
                }
            }
        }
        // Buckets by the name their gateways know them by.
        let BucketDone::List(list) = self.bucket_op(BucketOp::List { tenant: None }).await? else {
            return Err(ConnectError::internal("a bucket list answered otherwise"));
        };
        let buckets = list
            .into_iter()
            .filter(|b| {
                let project = tenancy.bucket_project(b.tenant, b.project);
                let at = datasets::chain(Some(Level::Bucket(b.table)), b.tenant, project);
                props.get(&at, "readonly") == Some("on")
            })
            .map(|b| (b.tenant, b.name))
            .collect();
        Ok((out, buckets))
    }

    /// Puts the read-only volumes in the map (D420), answered once storage
    /// nodes hold them.
    pub(super) async fn sync_readonly(&self) -> Result<(), ConnectError> {
        let (readonly, buckets) = self.readonly_volumes().await?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SyncReadonly {
                readonly,
                buckets,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        match rx.recv().await {
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(ConnectError::unavailable(e)),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    /// Keeps the map's read-only volumes current on the leader (D420):
    /// volumes made, cloned, copied or moved in a read-only project, and
    /// copies and moves that finished, are in it within 30 s.
    pub(super) async fn readonly_upkeep(self: Arc<Self>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if self.admin.runner.leads().await
                && let Err(e) = self.sync_readonly().await
            {
                eprintln!(
                    "hyperstor-meta: read-only volumes not updated: {}",
                    e.message.as_deref().unwrap_or("")
                );
            }
        }
    }

    /// Refuses `op` on volume `v` while its `readonly` is on (D420), as its
    /// chunks' primaries refuse its writes: expanding, rolling back or
    /// flattening it would change what it reads.
    pub(super) async fn not_readonly(
        &self,
        v: &crate::Volume,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<(), ConnectError> {
        let tenancy = self.tenancy().await?;
        let tenant = tenancy.tenant_of(v.tenant, v.project);
        let project = tenancy.project_of(tenant, v.project);
        let props = self.realm_rows("properties", datasets::load).await?;
        let at = datasets::chain(Some(Level::Volume(v.id)), tenant, project);
        if props.get(&at, "readonly") != Some("on") {
            return Ok(());
        }
        let e = with_reason(
            ConnectError::failed_precondition(format!(
                "volume {:032x} is read-only (readonly=on): set readonly=off first",
                v.id
            )),
            "readonly",
        );
        super::refused(who, op, target, &e);
        Err(e)
    }

    async fn resolve(
        &self,
        scope: &Scope,
        tenancy: &Tenancy,
        path: &str,
    ) -> Result<Resolved, ConnectError> {
        let entries = self.datasets(tenancy).await?;
        let path = match datasets::resolve(tenancy, &entries, path) {
            Ok(p) => p,
            Err(Unresolved::NotFound(m)) => {
                return Err(with_reason(ConnectError::not_found(m), "not_found"));
            }
            Err(Unresolved::Ambiguous(names)) => {
                return Err(with_reason(
                    ConnectError::failed_precondition(format!(
                        "{path} names more than one dataset: {}",
                        names.join(", ")
                    )),
                    "ambiguous",
                ));
            }
        };
        Ok(match path {
            Path::Tenant(t) => Resolved {
                path,
                name: tenancy.tenant_label(t),
                kind: "tenant",
                chain: datasets::chain(None, t, 0),
                tenant: t,
                reached: scope.tenant(t),
            },
            Path::Project(p) => {
                let t = tenancy.tenant_of(0, p);
                Resolved {
                    path,
                    name: tenancy.project_label(p),
                    kind: "project",
                    chain: datasets::chain(Some(Level::Project(p)), t, p),
                    tenant: t,
                    reached: scope.container(t, p),
                }
            }
            Path::Dataset(id) => {
                let qualified = datasets::qualified(&entries);
                let (e, q) = entries
                    .iter()
                    .zip(qualified)
                    .find(|(e, _)| e.id == id)
                    .ok_or_else(|| not_found("dataset"))?;
                Resolved {
                    path,
                    name: datasets::full_name(tenancy, e, q),
                    kind: id.kind().name(),
                    chain: datasets::chain(Some(id.level()), e.tenant, e.project),
                    tenant: e.tenant,
                    reached: match id {
                        Id::Bucket(_) => scope.bucket_in(e.tenant, e.project),
                        Id::Volume(_) => scope.container(e.tenant, e.project),
                    },
                }
            }
        })
    }

    /// `dataset`, if `scope` reaches it; else as one that doesn't exist.
    async fn reached(
        &self,
        scope: &Scope,
        dataset: &str,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<(Tenancy, Resolved), ConnectError> {
        let tenancy = self.tenancy().await?;
        let r = self.resolve(scope, &tenancy, dataset).await;
        match r {
            Ok(r) if r.reached => Ok((tenancy, r)),
            Ok(_) => {
                let e = not_found(&format!("dataset {dataset}"));
                super::refused(who, op, target, &e);
                Err(e)
            }
            Err(e) => {
                super::refused(who, op, target, &e);
                Err(e)
            }
        }
    }

    fn property_pb(
        &self,
        tenancy: &Tenancy,
        map: &crate::ClusterMap,
        v: &datasets::Value,
        kind: Option<Kind>,
    ) -> pb::DatasetProperty {
        let value = if v.property.is_pool() && v.set.is_some() {
            pool_name(map, &v.value)
        } else {
            v.value.clone()
        };
        pb::DatasetProperty {
            name: v.property.name.into(),
            value,
            source: match v.source {
                Source::Local => "local".into(),
                Source::Default => "default".into(),
                Source::Inherited(Level::Tenant(t)) => tenancy.tenant_label(t),
                Source::Inherited(Level::Project(p)) => tenancy.project_label(p),
                Source::Inherited(l) => format!("{l:?}"),
            },
            enforced: v.property.enforced_for(kind),
            set_by: v.set.as_ref().map(|s| s.set_by.clone()).unwrap_or_default(),
            set_unix: v.set.as_ref().map_or(0, |s| s.set_unix),
            ..Default::default()
        }
    }
}

/// Pool `id` (as text) by its name, or as it is if there is no such pool.
fn pool_name(map: &crate::ClusterMap, id: &str) -> String {
    id.parse::<u32>()
        .ok()
        .and_then(|i| map.pools.get(&i))
        .map_or_else(|| id.to_string(), |p| p.name.clone())
}

#[allow(refining_impl_trait)]
impl DatasetService for Api {
    async fn list_datasets(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListDatasetsRequest>,
    ) -> ServiceResult<pb::ListDatasetsResponse> {
        let target = if req.under.is_empty() {
            "datasets".to_string()
        } else {
            format!("datasets under {}", req.under)
        };
        let (who, reach) = self.authorize_in(&ctx, "ListDatasets", &target, false)?;
        let scope = self.scope(reach).await?;
        let kind = match req.kind {
            "" => None,
            "volume" => Some(Kind::Volume),
            "filesystem" => Some(Kind::Filesystem),
            "bucket" => Some(Kind::Bucket),
            t => {
                return Err(invalid(format!(
                    "{t:?}: a type is volume, filesystem or bucket"
                )));
            }
        };
        let tenancy = self.tenancy().await?;
        let entries = self.datasets(&tenancy).await?;
        let under = match req.under {
            "" => None,
            u => match datasets::resolve(&tenancy, &entries, u) {
                Ok(Path::Tenant(t)) if scope.tenant(t) => Some((t, None)),
                Ok(Path::Project(p)) if scope.container(tenancy.tenant_of(0, p), p) => {
                    Some((tenancy.tenant_of(0, p), Some(p)))
                }
                _ => {
                    let e = not_found(&format!("tenant or project {u}"));
                    super::refused(&who, "ListDatasets", &target, &e);
                    return Err(e);
                }
            },
        };
        let (map, ..) = self.published().await?;
        let names = self.realm_names(&scope).await;
        let qualified = datasets::qualified(&entries);
        let mut out: Vec<pb::Dataset> = entries
            .iter()
            .zip(qualified)
            .filter(|(e, _)| match e.id {
                Id::Bucket(_) => scope.bucket_in(e.tenant, e.project),
                Id::Volume(_) => scope.container(e.tenant, e.project),
            })
            .filter(|(e, _)| kind.is_none_or(|k| e.id.kind() == k))
            .filter(|(e, _)| {
                under.is_none_or(|(t, p)| e.tenant == t && p.is_none_or(|p| e.project == p))
            })
            .map(|(e, q)| pb::Dataset {
                name: datasets::full_name(&tenancy, e, q),
                kind: e.id.kind().name().into(),
                id: match e.id {
                    Id::Volume(v) => format!("{v:032x}"),
                    Id::Bucket(b) => b.to_string(),
                },
                tenant: e.tenant,
                project: e.project,
                realm: names.realm_name(e.tenant),
                pool: e.pool,
                pool_name: pool_name(&map, &e.pool.to_string()),
                ..Default::default()
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(pb::ListDatasetsResponse {
            datasets: out,
            ..Default::default()
        }
        .into())
    }

    async fn get_dataset_properties(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetDatasetPropertiesRequest>,
    ) -> ServiceResult<pb::GetDatasetPropertiesResponse> {
        let target = format!("dataset {}", req.dataset);
        let (who, reach) = self.authorize_in(&ctx, "GetDatasetProperties", &target, false)?;
        let scope = self.scope(reach).await?;
        for n in &req.names {
            if datasets::property(n).is_none() {
                return Err(invalid(format!(
                    "no property {n:?}: one of {}",
                    datasets::names().join(", ")
                )));
            }
        }
        let (tenancy, r) = self
            .reached(&scope, req.dataset, &who, "GetDatasetProperties", &target)
            .await?;
        let props: Props = self.realm_rows("properties", datasets::load).await?;
        let (map, ..) = self.published().await?;
        let kind = match r.path {
            Path::Dataset(id) => Some(id.kind()),
            _ => None,
        };
        if let Some(k) = kind
            && let Some(n) = req
                .names
                .iter()
                .find(|n| datasets::property(n).is_some_and(|p| !p.applies_to(k)))
        {
            return Err(invalid(format!("{n} doesn't apply to a {}", k.name())));
        }
        Ok(pb::GetDatasetPropertiesResponse {
            dataset: r.name.clone(),
            kind: r.kind.into(),
            properties: props
                .values(&r.chain, kind)
                .iter()
                .filter(|v| req.names.is_empty() || req.names.contains(&v.property.name))
                .map(|v| self.property_pb(&tenancy, &map, v, kind))
                .collect(),
            ..Default::default()
        }
        .into())
    }

    async fn set_dataset_property(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDatasetPropertyRequest>,
    ) -> ServiceResult<pb::SetDatasetPropertyResponse> {
        let target = format!("dataset {}: {}={}", req.dataset, req.name, req.value);
        let (who, reach) = self.authorize_in(&ctx, "SetDatasetProperty", &target, true)?;
        let scope = self.scope(reach).await?;
        let (tenancy, r) = self
            .reached(&scope, req.dataset, &who, "SetDatasetProperty", &target)
            .await?;
        let set_kind = match r.path {
            Path::Dataset(id) => Some(id.kind()),
            _ => None,
        };
        let (map, ..) = self.published().await?;
        let r = async {
            let p = datasets::property(req.name).ok_or_else(|| {
                invalid(format!(
                    "no property {:?}: one of {}",
                    req.name,
                    datasets::names().join(", ")
                ))
            })?;
            let mut value = req.value.to_string();
            // A pool by its name or id, kept by its id; only one the
            // tenant's realm may use (D249).
            if p.is_pool() {
                let id = map
                    .pools
                    .iter()
                    .find(|(id, x)| x.name == value || id.to_string() == value)
                    .map(|(id, _)| *id)
                    .filter(|&id| id != hyperstor_map::META_POOL)
                    .ok_or_else(|| invalid(format!("no pool {value:?} takes datasets")))?;
                let realms = self.realms().await?;
                let realm = realms.realm_of(r.tenant);
                if !realms.may_use(realm, id) {
                    return Err(with_reason(
                        ConnectError::failed_precondition(format!(
                            "realm {} may not use pool {value}",
                            realms.name(realm)
                        )),
                        "pool_not_in_realm",
                    ));
                }
                value = id.to_string();
            }
            let level = r.chain[0];
            datasets::check(level, req.name, &value).map_err(invalid)?;
            let (name, by) = (req.name.to_string(), who.clone());
            let stored = self
                .realm_rows("dataset", move |db| {
                    datasets::set(db, level, &name, &value, super::admin::now_unix(), &by)
                })
                .await?;
            Ok(datasets::Value {
                property: p,
                value: stored.clone(),
                source: Source::Local,
                set: Some(datasets::Set {
                    value: stored,
                    set_unix: super::admin::now_unix(),
                    set_by: who.clone(),
                }),
            })
        }
        .await;
        let v = Self::audited(&who, "SetDatasetProperty", &target, r)?;
        // Storage nodes refuse a read-only volume's writes once they hold
        // the map that says so (D420): answered then.
        if req.name == "readonly" {
            self.sync_readonly().await?;
        }
        Ok(pb::SetDatasetPropertyResponse {
            property: buffa::MessageField::some(self.property_pb(&tenancy, &map, &v, set_kind)),
            ..Default::default()
        }
        .into())
    }

    async fn inherit_dataset_property(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::InheritDatasetPropertyRequest>,
    ) -> ServiceResult<pb::InheritDatasetPropertyResponse> {
        let target = format!("dataset {}: {}", req.dataset, req.name);
        let (who, reach) = self.authorize_in(&ctx, "InheritDatasetProperty", &target, true)?;
        let scope = self.scope(reach).await?;
        let (_, r) = self
            .reached(&scope, req.dataset, &who, "InheritDatasetProperty", &target)
            .await?;
        if datasets::property(req.name).is_none() {
            return Err(invalid(format!(
                "no property {:?}: one of {}",
                req.name,
                datasets::names().join(", ")
            )));
        }
        let (level, name) = (r.chain[0], req.name.to_string());
        let was = self
            .realm_rows("dataset", move |db| datasets::inherit(db, level, &name))
            .await;
        let was_set = Self::audited(&who, "InheritDatasetProperty", &target, was)?;
        if was_set && req.name == "readonly" {
            self.sync_readonly().await?;
        }
        Ok(pb::InheritDatasetPropertyResponse {
            was_set,
            ..Default::default()
        }
        .into())
    }
}
