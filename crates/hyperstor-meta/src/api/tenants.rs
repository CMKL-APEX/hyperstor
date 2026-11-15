//! `TenantService` (20-tenants.md §24.9, 04-decisions.md D299–D301):
//! tenants and their projects, kept by `crate::tenancy`.
//!
//! Tenants are made, renamed and deleted for the cluster as a whole.
//! Projects are made, renamed and deleted by whoever reaches their tenant
//! (a cluster-wide role, its realm, or `role@tenant:NAME`); someone who
//! reaches only a project sees it, and may not make its siblings.

use super::realms::Scope;
use super::{Api, with_reason};
use crate::tenancy::{self, NewTenant, Project, ProjectState, Tenancy, Tenant};
use crate::{BucketDone, BucketOp};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{TenantService, TenantServiceExt};
use std::collections::BTreeSet;
use std::sync::Arc;

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    TenantServiceExt::register(api, router)
}

/// Why a call may not change what a trusted glidex directory owns
/// (D342): the name, realm and membership of a tenant linked to it, and
/// of a project with a glidex id in such a tenant. `None` when the call
/// may go on: a local record, a local project of a linked tenant, or one
/// of a directory no longer trusted. The feed changes them by another
/// path (`trust::plan`'s steps), which never passes here.
pub(super) fn directory_owned(
    t: &Tenancy,
    trusted: &BTreeSet<String>,
    tenant: u32,
    project: Option<u32>,
) -> Option<String> {
    let linked = |x: &Tenant| !x.glidex_id.is_empty() && trusted.contains(&x.directory);
    match project {
        Some(id) => {
            let p = t.project(id)?;
            let owner = t.tenant(p.tenant)?;
            (!p.glidex_id.is_empty() && linked(owner)).then(|| {
                format!(
                    "project {}/{} comes from glidex directory {}: change it there; it is \
                     local here, to change, once the trust ends (admission end) or the \
                     directory unlinks it",
                    owner.name, p.name, owner.directory
                )
            })
        }
        None => {
            let x = t.tenant(tenant)?;
            linked(x).then(|| {
                format!(
                    "tenant {} comes from glidex directory {}: change it there; it is local \
                     here, to change, once the trust ends (admission end)",
                    x.name, x.directory
                )
            })
        }
    }
}

/// The issuers of the directories trusted now.
pub(super) fn trusted_issuers(
    db: &crate::store::Db,
) -> Result<BTreeSet<String>, crate::tokens::TokenError> {
    Ok(crate::trust::directories(db)?
        .into_iter()
        .map(|d| d.dir.issuer)
        .collect())
}

/// The refusal for [`directory_owned`]'s reason.
pub(super) fn from_directory(msg: String) -> ConnectError {
    with_reason(ConnectError::failed_precondition(msg), "from_directory")
}

fn invalid(msg: impl Into<String>) -> ConnectError {
    with_reason(
        ConnectError::invalid_argument(msg.into()),
        "invalid_argument",
    )
}

/// A glidex id or directory: up to 200 characters, none of them control
/// characters or spaces.
fn valid_link(s: &str) -> bool {
    s.chars().count() <= 200 && !s.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn project_pb(t: &Tenancy, p: &Project) -> pb::Project {
    pb::Project {
        id: p.id,
        tenant: p.tenant,
        name: p.name.clone(),
        state: match p.state {
            ProjectState::Active => "active",
            ProjectState::Moving => "moving",
        }
        .into(),
        glidex_id: p.glidex_id.clone(),
        created_unix: p.created_unix,
        created_by: p.created_by.clone(),
        is_default: t
            .tenant(p.tenant)
            .is_some_and(|x| x.default_project == p.id),
        ..Default::default()
    }
}

fn tenant_pb(t: &Tenant, realm: String, projects: Vec<pb::Project>) -> pb::Tenant {
    pb::Tenant {
        id: t.id,
        name: t.name.clone(),
        directory: t.directory.clone(),
        glidex_id: t.glidex_id.clone(),
        default_project: t.default_project,
        created_unix: t.created_unix,
        created_by: t.created_by.clone(),
        realm,
        projects,
        ..Default::default()
    }
}

impl Api {
    /// What holds `tenant` (its volumes, those in the trash too, its
    /// buckets and its S3 keys), or project `project` of it, described for
    /// a refusal; read from the cluster when asked.
    pub(super) async fn holds(
        &self,
        tenancy: &Tenancy,
        tenant: u32,
        project: Option<u32>,
    ) -> Result<Vec<String>, ConnectError> {
        let mut out: Vec<String> = self
            .held(None)
            .await?
            .into_iter()
            .filter(|h| {
                let t = tenancy.tenant_of(h.tenant, h.project);
                match project {
                    None => t == tenant,
                    Some(p) => t == tenant && tenancy.project_of(h.tenant, h.project) == p,
                }
            })
            .map(|h| h.what)
            .collect();
        out.dedup();
        if project.is_none() {
            let keys = self
                .s3_rows("key", crate::s3keys::keys)
                .await?
                .into_iter()
                .filter(|k| k.tenant == tenant)
                .map(|k| format!("S3 key {}", k.id));
            out.extend(keys);
        }
        Ok(out)
    }

    /// Tenant `id`'s, if `scope` reaches it; one outside answers as one
    /// that doesn't exist.
    fn tenant_reached<'a>(
        scope: &Scope,
        tenancy: &'a Tenancy,
        id: u32,
        who: &str,
        op: &str,
        target: &str,
    ) -> Result<&'a Tenant, ConnectError> {
        match tenancy.tenant(id) {
            Some(t) if scope.tenant(id) => Ok(t),
            _ => {
                let e = with_reason(
                    ConnectError::not_found(format!("no tenant {id}")),
                    "not_found",
                );
                super::refused(who, op, target, &e);
                Err(e)
            }
        }
    }
}

/// The new tenant's caps hold the project's space (D303): a hook, as caps
/// per tenant and pool aren't kept yet (HS15's quotas); `force` (a
/// cluster administrator's) will pass them.
fn caps_hold(
    _tenancy: &Tenancy,
    _to: u32,
    _project: u32,
    _force: bool,
) -> Result<(), ConnectError> {
    Ok(())
}

impl Api {
    /// The project's volumes that a client holds now, with their holder's
    /// label, for the host check of a move across realms (D251, D303).
    async fn attached_in(
        &self,
        tenancy: &Tenancy,
        project: u32,
    ) -> Result<Vec<String>, ConnectError> {
        let (map, ..) = self.published().await?;
        let mut out = Vec::new();
        for pool in map.pools.keys().filter(|&&p| p != hyperstor_map::META_POOL) {
            let (reply, rx) = hyperstor_reactor::chan::oneshot();
            self.tx
                .send(crate::Admin::ListVolumes {
                    pool: *pool,
                    trash: false,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            for v in super::volume_result(rx.recv().await)? {
                let t = tenancy.tenant_of(v.tenant, v.project);
                if tenancy.project_of(t, v.project) == project && v.holder.is_some() {
                    out.push(format!(
                        "volume {:032x} ({})",
                        v.id,
                        v.holder_label.clone().unwrap_or_else(|| "held".into())
                    ));
                }
            }
        }
        Ok(out)
    }
}

impl Api {
    /// Checks a move of project `project` (of `t`) to tenant `to` and
    /// starts it (D303), for whoever was allowed to ask: the pools of the
    /// new realm, hosts bound to realms, bucket names and caps, then the
    /// project `moving` and the operation. Refusals are audited as `who`'s.
    pub(super) async fn start_project_move(
        &self,
        who: &str,
        target: &str,
        t: &Tenancy,
        project: u32,
        to: u32,
        force: bool,
    ) -> Result<pb::Operation, ConnectError> {
        let refuse = |reason: &str, e: ConnectError| {
            let e = with_reason(e, reason);
            super::refused(who, "MoveProject", target, &e);
            e
        };
        let Some(p) = t.project(project) else {
            return Err(refuse(
                "not_found",
                ConnectError::not_found(format!("no project {project}")),
            ));
        };
        let from = p.tenant;
        let realms = self.realms().await?;
        let realm_to = realms.realm_of(to);
        let realm_from = realms.realm_of(from);
        // The new realm may use every pool its volumes and buckets are in
        // (D249).
        let held = self.held(None).await?;
        if let Some(h) = held
            .iter()
            .find(|h| h.project == project && !realms.may_use(realm_to, h.pool))
        {
            return Err(refuse(
                "pool_not_in_realm",
                ConnectError::failed_precondition(format!(
                    "{} is in a pool realm {} may not use",
                    h.what,
                    realms.name(realm_to)
                )),
            ));
        }
        // Across realms, no host bound to realms may lose a volume it
        // holds at its next attach (D251): while any host is bound, the
        // project's volumes are detached first.
        if realm_from != realm_to {
            let (map, ..) = self.published().await?;
            if !map.host_realms.is_empty()
                && let Some(v) = self.attached_in(t, project).await?.first()
            {
                return Err(refuse(
                    "in_use",
                    ConnectError::failed_precondition(format!(
                        "{v} is attached, and hosts are bound to realms: detach it first, as its host may not attach it in realm {}",
                        realms.name(realm_to)
                    )),
                ));
            }
        }
        // No name of its buckets taken in the new tenant (D243).
        let BucketDone::List(theirs) = self.bucket_op(BucketOp::List { tenant: Some(to) }).await?
        else {
            return Err(ConnectError::internal("not a list"));
        };
        let BucketDone::List(ours) = self
            .bucket_op(BucketOp::List { tenant: Some(from) })
            .await?
        else {
            return Err(ConnectError::internal("not a list"));
        };
        let ours: Vec<_> = ours.into_iter().filter(|b| b.project == project).collect();
        if let Some(b) = ours
            .iter()
            .find(|b| theirs.iter().any(|x| x.name == b.name))
        {
            return Err(refuse(
                "bucket_exists",
                ConnectError::failed_precondition(format!(
                    "tenant {} has a bucket named {} already: rename one first",
                    t.tenant_label(to),
                    b.name
                )),
            ));
        }
        caps_hold(t, to, project, force)?;
        // Moving: nothing new in it, its tenant's keys put new buckets in
        // the tenant's default project; then the operation.
        let started = self
            .realm_rows("such project", move |db| {
                tenancy::start_move(db, project, to)
            })
            .await;
        Self::audited(who, "MoveProject", target, started.map(|_| ()))?;
        let op = self
            .start_op(
                who,
                "MoveProject",
                crate::operations::Work::MoveProject { project, from, to },
                target.to_string(),
                format!(
                    "moving project {} and {} bucket(s) to tenant {}",
                    t.project_label(project),
                    ours.len(),
                    t.tenant_label(to)
                ),
                Some(from),
            )
            .await;
        let op = match op {
            Ok(op) => op,
            Err(e) => {
                // Not started: active again where it was.
                let _ = self
                    .realm_rows("such project", move |db| tenancy::undo_move(db, project))
                    .await;
                return Err(e);
            }
        };
        Ok(op)
    }
}

impl Api {
    /// Refuses (`from_directory`) a change to what a trusted directory
    /// owns: tenant `tenant`, or project `project` (D342).
    pub(super) async fn not_directory_owned(
        &self,
        t: &Tenancy,
        tenant: u32,
        project: Option<u32>,
    ) -> Result<(), ConnectError> {
        let trusted = self.realm_rows("directory", trusted_issuers).await?;
        match directory_owned(t, &trusted, tenant, project) {
            Some(msg) => Err(from_directory(msg)),
            None => Ok(()),
        }
    }
}

#[allow(refining_impl_trait)]
impl TenantService for Api {
    async fn move_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::MoveProjectRequest>,
    ) -> ServiceResult<pb::MoveProjectResponse> {
        let target = format!("project {} to tenant {}", req.project, req.to);
        let (who, reach) = self.authorize_in(&ctx, "MoveProject", &target, true)?;
        let scope = self.scope(reach).await?;
        let t = self.tenancy().await?;
        let refuse = |reason: &str, e: ConnectError| {
            let e = with_reason(e, reason);
            super::refused(&who, "MoveProject", &target, &e);
            e
        };
        let Some(p) = t.project(req.project) else {
            return Err(refuse(
                "not_found",
                ConnectError::not_found(format!("no project {}", req.project)),
            ));
        };
        let from = p.tenant;
        // Reached: both tenants, in one realm, for someone with roles in
        // them; across realms, the cluster's administrators alone.
        Self::tenant_reached(&scope, &t, from, &who, "MoveProject", &target)?;
        if t.tenant(req.to).is_none() {
            return Err(refuse(
                "not_found",
                ConnectError::not_found(format!("no tenant {}", req.to)),
            ));
        }
        let realms = self.realms().await?;
        let (realm_from, realm_to) = (realms.realm_of(from), realms.realm_of(req.to));
        if !scope.whole() && (!scope.tenant(req.to) || realm_from != realm_to) {
            return Err(scope.outside(
                &who,
                "MoveProject",
                &target,
                &if realm_from != realm_to {
                    format!(
                        "a move from realm {} to realm {}",
                        realms.name(realm_from),
                        realms.name(realm_to)
                    )
                } else {
                    format!("tenant {}", req.to)
                },
            ));
        }
        if req.force && !scope.whole() {
            return Err(refuse(
                "permission_denied",
                ConnectError::permission_denied("--force is a cluster administrator's"),
            ));
        }
        // The feed moves a directory's projects (D342), through
        // start_project_move itself.
        if let Err(e) = self.not_directory_owned(&t, from, Some(req.project)).await {
            super::refused(&who, "MoveProject", &target, &e);
            return Err(e);
        }
        let op = self
            .start_project_move(&who, &target, &t, req.project, req.to, req.force)
            .await?;
        Ok(pb::MoveProjectResponse {
            operation: buffa::MessageField::some(op),
            ..Default::default()
        }
        .into())
    }

    async fn create_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateTenantRequest>,
    ) -> ServiceResult<pb::CreateTenantResponse> {
        let target = format!("tenant {}", req.name);
        let who = self.authorize(&ctx, "CreateTenant", &target, true)?;
        let r = async {
            if !tenancy::valid_tenant_name(req.name) {
                return Err(invalid(format!(
                    "{:?}: a tenant is named by 1-63 of a-z, 0-9 and -, and isn't \"default\"",
                    req.name
                )));
            }
            if !valid_link(req.directory) || !valid_link(req.glidex_id) {
                return Err(invalid(
                    "a directory and a glidex id have at most 200 characters, no spaces",
                ));
            }
            if req.glidex_id.is_empty() != req.directory.is_empty() {
                return Err(invalid("a glidex link names both its directory and its id"));
            }
            // A trusted directory's tenants come from its feed (D342).
            if !req.directory.is_empty()
                && self
                    .realm_rows("directory", trusted_issuers)
                    .await?
                    .contains(req.directory)
            {
                return Err(from_directory(format!(
                    "{} is trusted: its tenants come from its feed, through an offer",
                    req.directory
                )));
            }
            // Past every bare tenant in use: on volumes, buckets and keys.
            let mut in_use: Vec<u32> = self.held(None).await?.iter().map(|h| h.tenant).collect();
            in_use.extend(
                self.s3_rows("key", crate::s3keys::keys)
                    .await?
                    .iter()
                    .map(|k| k.tenant),
            );
            let n = NewTenant {
                name: req.name.to_string(),
                id: (req.id != 0).then_some(req.id),
                directory: req.directory.to_string(),
                glidex_id: req.glidex_id.to_string(),
                created_unix: super::admin::now_unix(),
                created_by: who.clone(),
            };
            self.realm_rows("tenant", move |db| tenancy::create_tenant(db, &n, &in_use))
                .await?;
            self.tenancy().await
        }
        .await;
        let t = Self::audited(&who, "CreateTenant", &target, r)?;
        let made = t
            .tenant_named(req.name)
            .ok_or_else(|| ConnectError::internal("a tenant made isn't there"))?;
        let projects = t
            .projects
            .iter()
            .filter(|p| p.tenant == made.id)
            .map(|p| project_pb(&t, p))
            .collect();
        let realm = self.realms().await?.realm_name(made.id);
        Ok(pb::CreateTenantResponse {
            tenant: buffa::MessageField::some(tenant_pb(made, realm, projects)),
            ..Default::default()
        }
        .into())
    }

    async fn list_tenants(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListTenantsRequest>,
    ) -> ServiceResult<pb::ListTenantsResponse> {
        let (_, reach) = self.authorize_in(&ctx, "ListTenants", "tenants", false)?;
        let scope = self.scope(reach).await?;
        let t = self.tenancy().await?;
        let realms = self.realms().await?;
        let tenants = t
            .tenants
            .iter()
            .filter_map(|x| {
                // A tenant reached shows every project; one reached only
                // through projects shows those.
                let whole = scope.tenant(x.id);
                let projects: Vec<pb::Project> = t
                    .projects
                    .iter()
                    .filter(|p| p.tenant == x.id && (whole || scope.container(x.id, p.id)))
                    .map(|p| project_pb(&t, p))
                    .collect();
                (whole || !projects.is_empty())
                    .then(|| tenant_pb(x, realms.realm_name(x.id), projects))
            })
            .collect();
        Ok(pb::ListTenantsResponse {
            tenants,
            ..Default::default()
        }
        .into())
    }

    async fn rename_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RenameTenantRequest>,
    ) -> ServiceResult<pb::RenameTenantResponse> {
        let target = format!("tenant {}: name {}", req.id, req.name);
        let who = self.authorize(&ctx, "RenameTenant", &target, true)?;
        let r = async {
            self.not_directory_owned(&self.tenancy().await?, req.id, None)
                .await?;
            if !tenancy::valid_tenant_name(req.name) {
                return Err(invalid(format!("{:?} isn't a tenant name", req.name)));
            }
            let (id, name) = (req.id, req.name.to_string());
            self.realm_rows("such tenant", move |db| {
                tenancy::rename_tenant(db, id, &name)
            })
            .await
        }
        .await;
        Self::audited(&who, "RenameTenant", &target, r)?;
        Ok(pb::RenameTenantResponse::default().into())
    }

    async fn delete_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteTenantRequest>,
    ) -> ServiceResult<pb::DeleteTenantResponse> {
        let target = format!("tenant {}", req.id);
        let who = self.authorize(&ctx, "DeleteTenant", &target, true)?;
        let r = async {
            let t = self.tenancy().await?;
            if t.tenant(req.id).is_none() {
                return Err(with_reason(
                    ConnectError::not_found(format!("no tenant {}", req.id)),
                    "not_found",
                ));
            }
            self.not_directory_owned(&t, req.id, None).await?;
            // Checked again in the write, for its projects and realm.
            let holds = self.holds(&t, req.id, None).await?;
            let id = req.id;
            self.realm_rows("such tenant", move |db| {
                tenancy::delete_tenant(db, id, &holds)
            })
            .await
        }
        .await;
        Self::audited(&who, "DeleteTenant", &target, r)?;
        Ok(pb::DeleteTenantResponse::default().into())
    }

    async fn create_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateProjectRequest>,
    ) -> ServiceResult<pb::CreateProjectResponse> {
        let target = format!("tenant {}: project {}", req.tenant, req.name);
        let (who, reach) = self.authorize_in(&ctx, "CreateProject", &target, true)?;
        let scope = self.scope(reach).await?;
        let t = self.tenancy().await?;
        Self::tenant_reached(&scope, &t, req.tenant, &who, "CreateProject", &target)?;
        let r = async {
            if !tenancy::valid_tenant_name(req.name) {
                return Err(invalid(format!(
                    "{:?}: a project is named by 1-63 of a-z, 0-9 and -, and isn't \"default\"",
                    req.name
                )));
            }
            if !valid_link(req.glidex_id) {
                return Err(invalid("a glidex id has at most 200 characters, no spaces"));
            }
            // A project made here in a directory's tenant is local; the
            // directory's own come from its feed (D342).
            if !req.glidex_id.is_empty()
                && directory_owned(
                    &t,
                    &self.realm_rows("directory", trusted_issuers).await?,
                    req.tenant,
                    None,
                )
                .is_some()
            {
                return Err(from_directory(format!(
                    "tenant {}'s projects with a glidex id come from its directory's feed; \
                     make it there, or here without --directory-project",
                    req.tenant
                )));
            }
            let (tenant, name, gid, who) = (
                req.tenant,
                req.name.to_string(),
                req.glidex_id.to_string(),
                who.clone(),
            );
            self.realm_rows("such tenant", move |db| {
                tenancy::create_project(db, tenant, &name, &gid, super::admin::now_unix(), &who)
            })
            .await
        }
        .await;
        let id = Self::audited(&who, "CreateProject", &target, r)?;
        let t = self.tenancy().await?;
        let p = t
            .project(id)
            .ok_or_else(|| ConnectError::internal("a project made isn't there"))?;
        Ok(pb::CreateProjectResponse {
            project: buffa::MessageField::some(project_pb(&t, p)),
            ..Default::default()
        }
        .into())
    }

    async fn rename_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RenameProjectRequest>,
    ) -> ServiceResult<pb::RenameProjectResponse> {
        let target = format!("project {}: name {}", req.id, req.name);
        let (who, reach) = self.authorize_in(&ctx, "RenameProject", &target, true)?;
        let scope = self.scope(reach).await?;
        let t = self.tenancy().await?;
        let tenant = t.project(req.id).map_or(0, |p| p.tenant);
        Self::tenant_reached(&scope, &t, tenant, &who, "RenameProject", &target)?;
        let r = async {
            self.not_directory_owned(&t, tenant, Some(req.id)).await?;
            if !tenancy::valid_tenant_name(req.name) {
                return Err(invalid(format!("{:?} isn't a project name", req.name)));
            }
            let (id, name) = (req.id, req.name.to_string());
            self.realm_rows("such project", move |db| {
                tenancy::rename_project(db, id, &name)
            })
            .await
        }
        .await;
        Self::audited(&who, "RenameProject", &target, r)?;
        Ok(pb::RenameProjectResponse::default().into())
    }

    async fn delete_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteProjectRequest>,
    ) -> ServiceResult<pb::DeleteProjectResponse> {
        let target = format!("project {}", req.id);
        let (who, reach) = self.authorize_in(&ctx, "DeleteProject", &target, true)?;
        let scope = self.scope(reach).await?;
        let t = self.tenancy().await?;
        let tenant = t.project(req.id).map_or(0, |p| p.tenant);
        Self::tenant_reached(&scope, &t, tenant, &who, "DeleteProject", &target)?;
        let r = async {
            self.not_directory_owned(&t, tenant, Some(req.id)).await?;
            let holds = self.holds(&t, tenant, Some(req.id)).await?;
            let id = req.id;
            self.realm_rows("such project", move |db| {
                tenancy::delete_project(db, id, &holds)
            })
            .await
        }
        .await;
        Self::audited(&who, "DeleteProject", &target, r)?;
        Ok(pb::DeleteProjectResponse::default().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(id: u32, name: &str, directory: &str, gid: &str) -> Tenant {
        Tenant {
            id,
            name: name.into(),
            directory: directory.into(),
            glidex_id: gid.into(),
            default_project: 0,
            created_unix: 0,
            created_by: String::new(),
        }
    }

    fn project(id: u32, tenant: u32, name: &str, gid: &str) -> Project {
        Project {
            id,
            tenant,
            name: name.into(),
            state: ProjectState::Active,
            glidex_id: gid.into(),
            created_unix: 0,
            created_by: String::new(),
            made_in: tenant,
        }
    }

    /// What a trusted directory owns is refused to every call that
    /// changes it (D342): tenants (rename, realm, delete) by
    /// `directory_owned(.., None)`, projects (rename, move, delete) by
    /// `Some(project)`. Local records, local projects of a directory's
    /// tenant, and records of a directory no longer trusted are not.
    #[test]
    fn a_trusted_directory_s_records_are_its_own() {
        let dir = "https://dir.example/d/uni";
        let mut t = Tenancy::default();
        t.tenants = vec![
            tenant(1, "physics", dir, "t1"),
            tenant(2, "local", "", ""),
            tenant(3, "gone", "https://old.example/d/x", "t9"),
        ];
        t.projects = vec![
            project(10, 1, "optics", "p1"),
            project(11, 1, "made-here", ""),
            project(12, 2, "web", ""),
            project(13, 3, "kept", "p9"),
        ];
        let trusted: BTreeSet<String> = [dir.to_string()].into();
        // RenameTenant, SetTenantRealm and DeleteTenant on physics.
        let why = directory_owned(&t, &trusted, 1, None).unwrap();
        assert!(why.contains("physics") && why.contains(dir), "{why}");
        // RenameProject, MoveProject and DeleteProject on optics.
        let why = directory_owned(&t, &trusted, 1, Some(10)).unwrap();
        assert!(why.contains("physics/optics"), "{why}");
        // A local project of the directory's tenant is local.
        assert_eq!(directory_owned(&t, &trusted, 1, Some(11)), None);
        // Local records.
        assert_eq!(directory_owned(&t, &trusted, 2, None), None);
        assert_eq!(directory_owned(&t, &trusted, 2, Some(12)), None);
        // A directory no longer trusted: its links no longer hold.
        assert_eq!(directory_owned(&t, &trusted, 3, None), None);
        assert_eq!(directory_owned(&t, &trusted, 3, Some(13)), None);
        // Nothing trusted: nothing is a directory's.
        assert_eq!(directory_owned(&t, &BTreeSet::new(), 1, None), None);
        // No such record: the call's own not_found follows.
        assert_eq!(directory_owned(&t, &trusted, 99, None), None);
        assert_eq!(directory_owned(&t, &trusted, 1, Some(99)), None);
    }
}
