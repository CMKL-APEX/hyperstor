//! Tenants and projects as records (20-tenants.md §24.9, 04-decisions.md
//! D299, D300): rows of the replicated store's settings table, as realms
//! are (`crate::realms`):
//!
//! - `tenant/<id>`: its name, its optional glidex link (directory issuer
//!   and glidex tenant id), its default project, when and by whom made.
//! - `project/<id>`: its tenant, name, state, optional glidex project id,
//!   when and by whom made.
//! - `tenancy/next-tenant`, `tenancy/next-project`: the next ids to give,
//!   so an id is never given twice, even after a delete.
//!
//! Ids are unique in the cluster (point 264). A tenant used before records
//! existed (a bare number on volumes, buckets and keys) has no row; making
//! a record for it adopts its id. What a tenant's realm is stays in
//! `crate::realms`' `trealm/` rows. What one row needs of the others is
//! checked in the write's own transaction.

use crate::realms::valid_name;
use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{RecTable, StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable;

const TENANT: &str = "tenant/";
const PROJECT: &str = "project/";
const NEXT_TENANT: &str = "tenancy/next-tenant";
const NEXT_PROJECT: &str = "tenancy/next-project";

/// The name of the project every tenant is made with.
pub const DEFAULT_PROJECT: &str = "default";

/// A tenant as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    pub id: u32,
    pub name: String,
    /// The glidex directory it comes from (its issuer); empty: none.
    pub directory: String,
    /// Its id in that directory; empty: none.
    pub glidex_id: String,
    /// The project that takes containers naming none.
    pub default_project: u32,
    pub created_unix: u64,
    pub created_by: String,
}

impl Tenant {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.str(&self.name)
            .str(&self.directory)
            .str(&self.glidex_id)
            .u32(self.default_project)
            .u64(self.created_unix)
            .str(&self.created_by);
        e.into_vec()
    }

    fn decode(id: u32, b: &[u8]) -> Result<Tenant, DecodeError> {
        let mut d = Dec::new(b);
        let t = Tenant {
            id,
            name: d.str()?.to_string(),
            directory: d.str()?.to_string(),
            glidex_id: d.str()?.to_string(),
            default_project: d.u32()?,
            created_unix: d.u64()?,
            created_by: d.str()?.to_string(),
        };
        // Later fields are appended; a reader skips those it doesn't know.
        Ok(t)
    }
}

/// Whether a project takes new containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectState {
    Active,
    /// Moving to another tenant (D303): no new containers.
    Moving,
}

/// A project as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: u32,
    pub tenant: u32,
    pub name: String,
    pub state: ProjectState,
    /// Its id in glidex; empty: none.
    pub glidex_id: String,
    pub created_unix: u64,
    pub created_by: String,
}

impl Project {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(self.tenant)
            .str(&self.name)
            .u8(match self.state {
                ProjectState::Active => 0,
                ProjectState::Moving => 1,
            })
            .str(&self.glidex_id)
            .u64(self.created_unix)
            .str(&self.created_by);
        e.into_vec()
    }

    fn decode(id: u32, b: &[u8]) -> Result<Project, DecodeError> {
        let mut d = Dec::new(b);
        Ok(Project {
            id,
            tenant: d.u32()?,
            name: d.str()?.to_string(),
            state: match d.u8()? {
                0 => ProjectState::Active,
                _ => ProjectState::Moving,
            },
            glidex_id: d.str()?.to_string(),
            created_unix: d.u64()?,
            created_by: d.str()?.to_string(),
        })
    }
}

/// Every tenant and project, from one read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tenancy {
    /// By id.
    pub tenants: Vec<Tenant>,
    /// By id.
    pub projects: Vec<Project>,
    next_tenant: u32,
    next_project: u32,
}

impl Tenancy {
    pub fn tenant(&self, id: u32) -> Option<&Tenant> {
        self.tenants.iter().find(|t| t.id == id)
    }

    pub fn tenant_named(&self, name: &str) -> Option<&Tenant> {
        self.tenants.iter().find(|t| t.name == name)
    }

    pub fn project(&self, id: u32) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Project `name` of tenant `tenant`.
    pub fn project_named(&self, tenant: u32, name: &str) -> Option<&Project> {
        self.projects
            .iter()
            .find(|p| p.tenant == tenant && p.name == name)
    }

    /// The project a container of `tenant` that names `project` counts to
    /// (D300): the one named, else its tenant's default project; 0 when
    /// neither is known (the cluster's own, or a bare tenant).
    pub fn project_of(&self, tenant: u32, project: u32) -> u32 {
        if project != 0 {
            return project;
        }
        self.tenant(tenant).map_or(0, |t| t.default_project)
    }

    /// The tenant of a container naming `project`, else `tenant` (its
    /// own record): a project's tenant is the authority (D300).
    pub fn tenant_of(&self, tenant: u32, project: u32) -> u32 {
        self.project(project).map_or(tenant, |p| p.tenant)
    }

    /// A project's name as `tenant/project`, by names where known.
    pub fn project_label(&self, id: u32) -> String {
        match self.project(id) {
            None if id == 0 => String::new(),
            None => format!("#{id}"),
            Some(p) => format!("{}/{}", self.tenant_label(p.tenant), p.name),
        }
    }

    /// A tenant's name, or its number when it has no record.
    pub fn tenant_label(&self, id: u32) -> String {
        self.tenant(id)
            .map_or_else(|| id.to_string(), |t| t.name.clone())
    }
}

fn scan(
    t: &impl ReadableTable<&'static str, &'static [u8]>,
    prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut out = Vec::new();
    for row in t.range(prefix..).map_err(StoreError::from)? {
        let (k, v) = row.map_err(StoreError::from)?;
        let Some(rest) = k.value().strip_prefix(prefix) else {
            break;
        };
        out.push((rest.to_string(), v.value().to_vec()));
    }
    Ok(out)
}

fn read(t: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<Tenancy, Error> {
    let bad = |row: &str, e: String| Error::Store(format!("{row}: {e}"));
    let mut out = Tenancy::default();
    for (id, v) in scan(t, TENANT)? {
        let row = format!("{TENANT}{id}");
        let id = id.parse().map_err(|_| bad(&row, "not a tenant".into()))?;
        out.tenants
            .push(Tenant::decode(id, &v).map_err(|e| bad(&row, e.to_string()))?);
    }
    for (id, v) in scan(t, PROJECT)? {
        let row = format!("{PROJECT}{id}");
        let id = id.parse().map_err(|_| bad(&row, "not a project".into()))?;
        out.projects
            .push(Project::decode(id, &v).map_err(|e| bad(&row, e.to_string()))?);
    }
    out.tenants.sort_by_key(|t| t.id);
    out.projects.sort_by_key(|p| p.id);
    let next = |row: &str| -> Result<u32, Error> {
        match t.get(row).map_err(StoreError::from)? {
            None => Ok(0),
            Some(v) => Dec::new(v.value())
                .u32()
                .map_err(|e| bad(row, e.to_string())),
        }
    };
    out.next_tenant = next(NEXT_TENANT)?;
    out.next_project = next(NEXT_PROJECT)?;
    Ok(out)
}

/// Every tenant and project, from this member's copy.
pub fn load(db: &Db) -> Result<Tenancy, Error> {
    let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(Tenancy::default());
    };
    read(&t)
}

fn refused(reason: &'static str, why: String) -> Error {
    Error::Refused { reason, why }
}

fn write<T>(
    db: &Db,
    f: impl FnOnce(&mut RecTable<'_, store::MapSchema>, &Tenancy) -> Result<T, Error>,
) -> Result<T, Error> {
    db.write(Origin::Leader, |tx| -> Result<T, Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        let now = read(&*t)?;
        f(&mut t, &now)
    })
}

fn put(t: &mut RecTable<'_, store::MapSchema>, key: &str, v: &[u8]) -> Result<(), Error> {
    t.insert(key, v).map_err(StoreError::from)?;
    Ok(())
}

fn put_next(t: &mut RecTable<'_, store::MapSchema>, key: &str, next: u32) -> Result<(), Error> {
    let mut e = Enc::new();
    e.u32(next);
    put(t, key, &e.into_vec())
}

/// Whether `name` may name a tenant or a project: as a realm (1-63 of
/// a-z, 0-9 and -), "default" allowed for a project.
pub fn valid_tenant_name(name: &str) -> bool {
    valid_name(name)
}

pub fn valid_project_name(name: &str) -> bool {
    valid_name(name) || name == DEFAULT_PROJECT
}

/// What a new tenant is: its name, an id to adopt (a bare tenant's) or
/// none for the next free one, its glidex link, and who made it.
#[derive(Debug, Clone, Default)]
pub struct NewTenant {
    pub name: String,
    pub id: Option<u32>,
    pub directory: String,
    pub glidex_id: String,
    pub created_unix: u64,
    pub created_by: String,
}

/// Makes tenant `n` and its default project; their ids. A new id is past
/// every id given and every tenant in `in_use` (bare tenants on volumes,
/// buckets and keys) and in a realm.
pub fn create_tenant(db: &Db, n: &NewTenant, in_use: &[u32]) -> Result<(u32, u32), Error> {
    let realms = crate::realms::load(db)?;
    write(db, |t, now| {
        if now.tenant_named(&n.name).is_some() {
            return Err(refused(
                "tenant_exists",
                format!("a tenant is named {}", n.name),
            ));
        }
        if !n.glidex_id.is_empty()
            && now
                .tenants
                .iter()
                .any(|x| x.directory == n.directory && x.glidex_id == n.glidex_id)
        {
            return Err(refused(
                "tenant_exists",
                format!("glidex tenant {} has a tenant already", n.glidex_id),
            ));
        }
        let past = now
            .tenants
            .iter()
            .map(|x| x.id)
            .chain(in_use.iter().copied())
            .chain(realms.tenants.keys().copied())
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(now.next_tenant)
            .max(1);
        let id = match n.id {
            Some(0) => return Err(refused("invalid_argument", "a tenant is 1 or more".into())),
            Some(id) if now.tenant(id).is_some() => {
                return Err(refused(
                    "tenant_exists",
                    format!("tenant {id} has a record"),
                ));
            }
            Some(id) => id,
            None => past,
        };
        if id == u32::MAX {
            return Err(refused("exhausted", "no tenant ids are left".into()));
        }
        let project = now
            .projects
            .iter()
            .map(|p| p.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(now.next_project)
            .max(1);
        let tenant = Tenant {
            id,
            name: n.name.clone(),
            directory: n.directory.clone(),
            glidex_id: n.glidex_id.clone(),
            default_project: project,
            created_unix: n.created_unix,
            created_by: n.created_by.clone(),
        };
        let p = Project {
            id: project,
            tenant: id,
            name: DEFAULT_PROJECT.into(),
            state: ProjectState::Active,
            glidex_id: String::new(),
            created_unix: n.created_unix,
            created_by: n.created_by.clone(),
        };
        put(t, &format!("{TENANT}{id}"), &tenant.encode())?;
        put(t, &format!("{PROJECT}{project}"), &p.encode())?;
        put_next(t, NEXT_TENANT, (id.saturating_add(1)).max(past))?;
        put_next(t, NEXT_PROJECT, project + 1)?;
        Ok((id, project))
    })
}

/// Renames tenant `id`.
pub fn rename_tenant(db: &Db, id: u32, name: &str) -> Result<(), Error> {
    write(db, |t, now| {
        let Some(old) = now.tenant(id) else {
            return Err(Error::NotFound);
        };
        if now.tenant_named(name).is_some_and(|x| x.id != id) {
            return Err(refused(
                "tenant_exists",
                format!("a tenant is named {name}"),
            ));
        }
        let new = Tenant {
            name: name.to_string(),
            ..old.clone()
        };
        put(t, &format!("{TENANT}{id}"), &new.encode())
    })
}

/// Deletes tenant `id` and its default project. Refused while it has
/// another project, or while `holds` (its containers and keys, as the
/// caller found them) names anything, or while it is in a realm other
/// than "default".
pub fn delete_tenant(db: &Db, id: u32, holds: &[String]) -> Result<(), Error> {
    let realms = crate::realms::load(db)?;
    write(db, |t, now| {
        let Some(tenant) = now.tenant(id) else {
            return Err(Error::NotFound);
        };
        let others: Vec<&str> = now
            .projects
            .iter()
            .filter(|p| p.tenant == id && p.id != tenant.default_project)
            .map(|p| p.name.as_str())
            .collect();
        if !others.is_empty() {
            return Err(refused(
                "tenant_in_use",
                format!(
                    "tenant {} has projects {}: delete them first",
                    tenant.name,
                    others.join(", ")
                ),
            ));
        }
        if let Some(h) = holds.first() {
            return Err(refused(
                "tenant_in_use",
                format!("tenant {} still has {h}", tenant.name),
            ));
        }
        if realms.realm_of(id) != 0 {
            return Err(refused(
                "tenant_in_use",
                format!(
                    "tenant {} is in realm {}: put it back in default first",
                    tenant.name,
                    realms.realm_name(id)
                ),
            ));
        }
        t.remove(format!("{TENANT}{id}").as_str())
            .map_err(StoreError::from)?;
        t.remove(format!("{PROJECT}{}", tenant.default_project).as_str())
            .map_err(StoreError::from)?;
        Ok(())
    })
}

/// Makes project `name` of tenant `tenant`; its id.
pub fn create_project(
    db: &Db,
    tenant: u32,
    name: &str,
    glidex_id: &str,
    created_unix: u64,
    created_by: &str,
) -> Result<u32, Error> {
    write(db, |t, now| {
        if now.tenant(tenant).is_none() {
            return Err(Error::NotFound);
        }
        if now.project_named(tenant, name).is_some() {
            return Err(refused(
                "project_exists",
                format!(
                    "tenant {} has a project named {name}",
                    now.tenant_label(tenant)
                ),
            ));
        }
        let id = now
            .projects
            .iter()
            .map(|p| p.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(now.next_project)
            .max(1);
        if id == u32::MAX {
            return Err(refused("exhausted", "no project ids are left".into()));
        }
        let p = Project {
            id,
            tenant,
            name: name.to_string(),
            state: ProjectState::Active,
            glidex_id: glidex_id.to_string(),
            created_unix,
            created_by: created_by.to_string(),
        };
        put(t, &format!("{PROJECT}{id}"), &p.encode())?;
        put_next(t, NEXT_PROJECT, id + 1)?;
        Ok(id)
    })
}

/// Renames project `id`; a tenant's default project keeps its name.
pub fn rename_project(db: &Db, id: u32, name: &str) -> Result<(), Error> {
    write(db, |t, now| {
        let Some(old) = now.project(id) else {
            return Err(Error::NotFound);
        };
        if now
            .tenant(old.tenant)
            .is_some_and(|x| x.default_project == id)
        {
            return Err(refused(
                "default_project",
                "a tenant's default project keeps its name".into(),
            ));
        }
        if name == DEFAULT_PROJECT
            || now
                .project_named(old.tenant, name)
                .is_some_and(|x| x.id != id)
        {
            return Err(refused(
                "project_exists",
                format!(
                    "tenant {} has a project named {name}",
                    now.tenant_label(old.tenant)
                ),
            ));
        }
        let new = Project {
            name: name.to_string(),
            ..old.clone()
        };
        put(t, &format!("{PROJECT}{id}"), &new.encode())
    })
}

/// Deletes project `id`. Refused for a tenant's default project (deleted
/// with its tenant) and while `holds` (its containers, as the caller
/// found them) names anything.
pub fn delete_project(db: &Db, id: u32, holds: &[String]) -> Result<(), Error> {
    write(db, |t, now| {
        let Some(p) = now.project(id) else {
            return Err(Error::NotFound);
        };
        if now
            .tenant(p.tenant)
            .is_some_and(|x| x.default_project == id)
        {
            return Err(refused(
                "default_project",
                "a tenant's default project goes with its tenant".into(),
            ));
        }
        if let Some(h) = holds.first() {
            return Err(refused(
                "project_in_use",
                format!("project {} still has {h}", now.project_label(id)),
            ));
        }
        t.remove(format!("{PROJECT}{id}").as_str())
            .map_err(StoreError::from)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(name: &str) -> NewTenant {
        NewTenant {
            name: name.into(),
            created_unix: 1_700_000_000,
            created_by: "hyperstor://9/admin/local".into(),
            ..Default::default()
        }
    }

    fn refusal(r: Result<impl std::fmt::Debug, Error>) -> &'static str {
        match r {
            Err(Error::Refused { reason, .. }) => reason,
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn tenants_and_projects_get_ids_never_given_twice() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        assert_eq!(load(&db).unwrap(), Tenancy::default());
        // Past the bare tenants in use and those in realms.
        let r = crate::realms::create(
            &db,
            &crate::realms::Realm {
                id: 0,
                name: "acme".into(),
                description: String::new(),
                created_unix: 0,
                created_by: String::new(),
            },
        )
        .unwrap();
        crate::realms::set_tenant(&db, 9, r, &[]).unwrap();
        let (a, ap) = create_tenant(&db, &new("acme"), &[3, 5]).unwrap();
        assert_eq!(a, 10);
        let (b, bp) = create_tenant(&db, &new("blue"), &[]).unwrap();
        assert_eq!((b, bp), (11, ap + 1));
        // A bare tenant's id adopted, below the next.
        let (c, _) = create_tenant(
            &db,
            &NewTenant {
                id: Some(5),
                ..new("cyan")
            },
            &[],
        )
        .unwrap();
        assert_eq!(c, 5);
        assert_eq!(
            refusal(create_tenant(
                &db,
                &NewTenant {
                    id: Some(5),
                    ..new("dark")
                },
                &[]
            )),
            "tenant_exists"
        );
        assert_eq!(
            refusal(create_tenant(&db, &new("acme"), &[])),
            "tenant_exists"
        );
        let now = load(&db).unwrap();
        assert_eq!(now.tenant(a).unwrap().default_project, ap);
        assert_eq!(now.project(ap).unwrap().name, DEFAULT_PROJECT);
        assert_eq!((now.project_of(a, 0), now.project_of(a, 77)), (ap, 77));
        assert_eq!(now.project_of(3, 0), 0, "a bare tenant has none");
        // Projects: per tenant names, ids across the cluster.
        let web = create_project(&db, a, "web", "", 1, "x").unwrap();
        let web_b = create_project(&db, b, "web", "", 1, "x").unwrap();
        assert_ne!(web, web_b);
        assert_eq!(
            refusal(create_project(&db, a, "web", "", 1, "x")),
            "project_exists"
        );
        assert_eq!(
            create_project(&db, 77, "web", "", 1, "x"),
            Err(Error::NotFound)
        );
        let now = load(&db).unwrap();
        assert_eq!(now.project_named(b, "web").unwrap().id, web_b);
        assert_eq!(now.tenant_of(0, web_b), b);
        assert_eq!(now.project_label(web), "acme/web");
        // Deleted ids aren't given again.
        delete_project(&db, web_b, &[]).unwrap();
        let next = create_project(&db, b, "api", "", 1, "x").unwrap();
        assert!(next > web_b);
        // The default project keeps its name and goes with its tenant.
        assert_eq!(refusal(rename_project(&db, ap, "x")), "default_project");
        assert_eq!(refusal(delete_project(&db, ap, &[])), "default_project");
        assert_eq!(
            refusal(rename_project(&db, web, "default")),
            "project_exists"
        );
        rename_project(&db, web, "www").unwrap();
        assert_eq!(
            refusal(delete_project(&db, web, &["volume 1".into()])),
            "project_in_use"
        );
        // A tenant goes once empty and back in "default".
        assert_eq!(refusal(delete_tenant(&db, a, &[])), "tenant_in_use");
        delete_project(&db, web, &[]).unwrap();
        assert_eq!(
            refusal(delete_tenant(&db, a, &["key AK".into()])),
            "tenant_in_use"
        );
        rename_tenant(&db, a, "amber").unwrap();
        assert_eq!(refusal(rename_tenant(&db, b, "amber")), "tenant_exists");
        delete_tenant(&db, a, &[]).unwrap();
        let now = load(&db).unwrap();
        assert!(now.tenant(a).is_none() && now.project(ap).is_none());
        let (d, _) = create_tenant(&db, &new("dark"), &[]).unwrap();
        assert!(d > b, "{d}");
        // In a realm: refused until back in "default".
        crate::realms::set_tenant(&db, c, r, &[]).unwrap();
        assert_eq!(refusal(delete_tenant(&db, c, &[])), "tenant_in_use");
    }

    #[test]
    fn a_glidex_tenant_has_one_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let linked = |name: &str| NewTenant {
            directory: "https://dir.example/d/uni".into(),
            glidex_id: "t-1".into(),
            ..new(name)
        };
        let (a, _) = create_tenant(&db, &linked("lab"), &[]).unwrap();
        assert_eq!(
            refusal(create_tenant(&db, &linked("lab2"), &[])),
            "tenant_exists"
        );
        let other = NewTenant {
            directory: "https://other.example".into(),
            ..linked("lab3")
        };
        create_tenant(&db, &other, &[]).unwrap();
        assert_eq!(load(&db).unwrap().tenant(a).unwrap().glidex_id, "t-1");
    }

    #[test]
    fn names() {
        assert!(valid_tenant_name("acme") && !valid_tenant_name("default"));
        assert!(valid_project_name("default") && valid_project_name("web-1"));
        assert!(!valid_project_name("Web") && !valid_project_name(""));
    }
}
