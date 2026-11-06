//! Realms (20-tenants.md §24.8, 04-decisions.md D248–D253): groups of
//! tenants with their own pools and endpoints, rows of the replicated
//! store's settings table as S3's keys are (`crate::s3keys`):
//!
//! - `realm/<name>`: its id, description, and when and by whom it was
//!   made.
//! - `trealm/<tenant>`: the realm a tenant is in, when not "default".
//! - `prealm/<pool id>`: the realms that may use a pool, when not every
//!   realm.
//!
//! "default" is realm 0 and has no row. What one row needs of the others
//! (a realm deleted only when nothing names it, a pool taken from a realm
//! only when none of its placements use it, a placement only in pools its
//! realm may use) is checked in the write's own transaction.

use crate::s3keys::{self, Placement};
use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable;
use std::collections::BTreeMap;

const REALM: &str = "realm/";
const TENANT: &str = "trealm/";
const POOL: &str = "prealm/";

/// The realm every tenant is in until put in another.
pub const DEFAULT_REALM: &str = "default";

/// A realm as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realm {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub created_unix: u64,
    pub created_by: String,
}

impl Realm {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(self.id)
            .str(&self.description)
            .u64(self.created_unix)
            .str(&self.created_by);
        e.into_vec()
    }

    fn decode(name: &str, b: &[u8]) -> Result<Realm, DecodeError> {
        let mut d = Dec::new(b);
        let r = Realm {
            id: d.u32()?,
            name: name.to_string(),
            description: d.str()?.to_string(),
            created_unix: d.u64()?,
            created_by: d.str()?.to_string(),
        };
        d.finish()?;
        Ok(r)
    }
}

/// Whether `name` may name a new realm: as a placement (1-63 of a-z, 0-9
/// and -), and not "default".
pub fn valid_name(name: &str) -> bool {
    s3keys::valid_placement(name) && name != DEFAULT_REALM
}

/// Every realm, and who is in which, from one read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Realms {
    /// By name; "default" isn't among them.
    pub realms: Vec<Realm>,
    /// Tenants in a realm other than "default".
    pub tenants: BTreeMap<u32, u32>,
    /// Pools with a list, and the realms on it.
    pub pools: BTreeMap<u32, Vec<u32>>,
}

impl Realms {
    /// The id of realm `name`; "default" is 0.
    pub fn id(&self, name: &str) -> Option<u32> {
        if name == DEFAULT_REALM || name.is_empty() {
            return Some(0);
        }
        self.realms.iter().find(|r| r.name == name).map(|r| r.id)
    }

    /// The name of realm `id`.
    pub fn name(&self, id: u32) -> String {
        self.realms
            .iter()
            .find(|r| r.id == id)
            .map_or_else(|| DEFAULT_REALM.to_string(), |r| r.name.clone())
    }

    /// The realm `tenant` is in.
    pub fn realm_of(&self, tenant: u32) -> u32 {
        self.tenants.get(&tenant).copied().unwrap_or(0)
    }

    /// Whether `realm` may use `pool` (D249): it's on the pool's list, or
    /// the pool has none.
    pub fn may_use(&self, realm: u32, pool: u32) -> bool {
        self.pools.get(&pool).is_none_or(|l| l.contains(&realm))
    }
}

/// The rows of `t` that begin with `prefix`, the prefix taken off.
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

fn ids(b: &[u8]) -> Result<Vec<u32>, DecodeError> {
    let mut d = Dec::new(b);
    let v = d.seq(4, |d| d.u32())?;
    d.finish()?;
    Ok(v)
}

fn read(t: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<Realms, Error> {
    let bad = |row: &str, e: String| Error::Store(format!("{row}: {e}"));
    let mut out = Realms::default();
    for (name, v) in scan(t, REALM)? {
        out.realms
            .push(Realm::decode(&name, &v).map_err(|e| bad(&format!("{REALM}{name}"), e.to_string()))?);
    }
    for (tenant, v) in scan(t, TENANT)? {
        let row = format!("{TENANT}{tenant}");
        let tenant = tenant.parse().map_err(|_| bad(&row, "not a tenant".into()))?;
        let mut d = Dec::new(&v);
        let realm = d.u32().map_err(|e| bad(&row, e.to_string()))?;
        out.tenants.insert(tenant, realm);
    }
    for (pool, v) in scan(t, POOL)? {
        let row = format!("{POOL}{pool}");
        let pool = pool.parse().map_err(|_| bad(&row, "not a pool".into()))?;
        out.pools
            .insert(pool, ids(&v).map_err(|e| bad(&row, e.to_string()))?);
    }
    Ok(out)
}

/// Every realm, from this member's copy.
pub fn load(db: &Db) -> Result<Realms, Error> {
    let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(Realms::default());
    };
    read(&t)
}

fn refused(reason: &'static str, why: String) -> Error {
    Error::Refused { reason, why }
}

/// Runs `f` on the settings table and what it holds of realms, in one
/// write. Only the leader.
fn write<T>(
    db: &Db,
    f: impl FnOnce(&mut redb::Table<&'static str, &'static [u8]>, &Realms) -> Result<T, Error>,
) -> Result<T, Error> {
    db.write(Origin::Leader, |tx| -> Result<T, Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        let now = read(&t)?;
        f(&mut t, &now)
    })
}

/// Makes realm `r` with the next free id, which it returns.
pub fn create(db: &Db, r: &Realm) -> Result<u32, Error> {
    write(db, |t, now| {
        if now.id(&r.name).is_some() {
            return Err(refused("realm_exists", format!("realm {} exists", r.name)));
        }
        let id = now.realms.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        let made = Realm { id, ..r.clone() };
        t.insert(format!("{REALM}{}", r.name).as_str(), made.encode().as_slice())
            .map_err(StoreError::from)?;
        Ok(id)
    })
}

/// Deletes realm `name`, which no tenant, pool or placement may name.
pub fn delete(db: &Db, name: &str) -> Result<(), Error> {
    let placements = s3keys::placements(db)?;
    write(db, |t, now| {
        let Some(r) = now.realms.iter().find(|r| r.name == name) else {
            return Err(Error::NotFound);
        };
        let tenants = now.tenants.values().filter(|&&x| x == r.id).count();
        let pools = now.pools.values().filter(|l| l.contains(&r.id)).count();
        let places = placements.iter().filter(|p| p.realm == r.id).count();
        if tenants + pools + places > 0 {
            return Err(refused(
                "realm_in_use",
                format!(
                    "realm {name} has {tenants} tenant(s), is on {pools} pool(s)' lists and has {places} S3 placement(s)"
                ),
            ));
        }
        t.remove(format!("{REALM}{name}").as_str())
            .map_err(StoreError::from)?;
        Ok(())
    })
}

/// Puts `tenant` in realm `realm` (0: "default", which removes its row).
pub fn set_tenant(db: &Db, tenant: u32, realm: u32) -> Result<(), Error> {
    write(db, |t, now| {
        if realm != 0 && !now.realms.iter().any(|r| r.id == realm) {
            return Err(Error::NotFound);
        }
        let row = format!("{TENANT}{tenant}");
        if realm == 0 {
            t.remove(row.as_str()).map_err(StoreError::from)?;
        } else {
            let mut e = Enc::new();
            e.u32(realm);
            t.insert(row.as_str(), e.into_vec().as_slice())
                .map_err(StoreError::from)?;
        }
        Ok(())
    })
}

/// Sets the realms that may use `pool` (none: every realm). A realm that
/// loses the pool may not have a placement in it.
pub fn set_pool(db: &Db, pool: u32, realms: &[u32]) -> Result<(), Error> {
    let placements = s3keys::placements(db)?;
    write(db, |t, now| {
        for &r in realms {
            if r != 0 && !now.realms.iter().any(|x| x.id == r) {
                return Err(Error::NotFound);
            }
        }
        let after = Realms {
            pools: {
                let mut p = now.pools.clone();
                if realms.is_empty() {
                    p.remove(&pool);
                } else {
                    p.insert(pool, realms.to_vec());
                }
                p
            },
            ..now.clone()
        };
        if let Some(p) = placements.iter().find(|p| {
            [p.data_pool, p.index_pool].contains(&pool) && !after.may_use(p.realm, pool)
        }) {
            return Err(refused(
                "pool_in_use",
                format!(
                    "realm {}'s S3 placement {} uses the pool",
                    now.name(p.realm),
                    p.name
                ),
            ));
        }
        let row = format!("{POOL}{pool}");
        if realms.is_empty() {
            t.remove(row.as_str()).map_err(StoreError::from)?;
        } else {
            let mut e = Enc::new();
            e.seq(realms, |e, r| {
                e.u32(*r);
            });
            t.insert(row.as_str(), e.into_vec().as_slice())
                .map_err(StoreError::from)?;
        }
        Ok(())
    })
}

/// Adds or replaces `p`, whose pools its realm must be able to use and
/// whose realm must exist (D249).
pub fn set_placement(db: &Db, p: &Placement) -> Result<(), Error> {
    write(db, |t, now| {
        if p.realm != 0 && !now.realms.iter().any(|r| r.id == p.realm) {
            return Err(Error::NotFound);
        }
        for pool in [p.data_pool, p.index_pool] {
            if !now.may_use(p.realm, pool) {
                return Err(refused(
                    "pool_not_in_realm",
                    format!(
                        "realm {} may not use pool {pool} (realm pool sets its list)",
                        now.name(p.realm)
                    ),
                ));
            }
        }
        s3keys::put_placement(t, p)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realm(name: &str) -> Realm {
        Realm {
            id: 0,
            name: name.into(),
            description: "a customer".into(),
            created_unix: 1_700_000_000,
            created_by: "hyperstor://9/admin/local".into(),
        }
    }

    fn placement(realm: u32, pool: u32) -> Placement {
        Placement {
            name: "default".into(),
            data_pool: pool,
            index_pool: pool,
            realm,
        }
    }

    #[test]
    fn realms_hold_tenants_and_pools() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let none = load(&db).unwrap();
        assert_eq!((none.id("default"), none.id(""), none.id("a")), (Some(0), Some(0), None));
        assert_eq!(none.realm_of(5), 0);
        assert!(none.may_use(0, 1) && none.may_use(9, 1));
        let a = create(&db, &realm("acme")).unwrap();
        let b = create(&db, &realm("blue")).unwrap();
        assert_eq!((a, b), (1, 2));
        assert!(matches!(
            create(&db, &realm("acme")),
            Err(Error::Refused { reason: "realm_exists", .. })
        ));
        // Tenants: "default" until put elsewhere.
        set_tenant(&db, 5, a).unwrap();
        set_tenant(&db, 6, b).unwrap();
        assert_eq!(set_tenant(&db, 7, 9), Err(Error::NotFound));
        let r = load(&db).unwrap();
        assert_eq!((r.realm_of(5), r.realm_of(6), r.realm_of(7)), (a, b, 0));
        assert_eq!((r.name(a), r.name(0)), ("acme".to_string(), "default".to_string()));
        // Pools: open to all, until listed.
        set_pool(&db, 3, &[a]).unwrap();
        let r = load(&db).unwrap();
        assert!(r.may_use(a, 3) && !r.may_use(b, 3) && !r.may_use(0, 3));
        assert!(r.may_use(b, 4));
        // A placement only in pools its realm may use.
        assert!(matches!(
            set_placement(&db, &placement(b, 3)),
            Err(Error::Refused { reason: "pool_not_in_realm", .. })
        ));
        assert_eq!(set_placement(&db, &placement(9, 4)), Err(Error::NotFound));
        set_placement(&db, &placement(a, 3)).unwrap();
        set_placement(&db, &placement(b, 4)).unwrap();
        // ... and kept so: a pool isn't taken from a realm that uses it.
        assert!(matches!(
            set_pool(&db, 3, &[b]),
            Err(Error::Refused { reason: "pool_in_use", .. })
        ));
        assert!(matches!(
            set_pool(&db, 4, &[a]),
            Err(Error::Refused { reason: "pool_in_use", .. })
        ));
        set_pool(&db, 3, &[a, b]).unwrap();
        set_pool(&db, 3, &[]).unwrap();
        assert!(load(&db).unwrap().may_use(0, 3));
        // Deleted only when nothing names it.
        assert!(matches!(
            delete(&db, "acme"),
            Err(Error::Refused { reason: "realm_in_use", .. })
        ));
        s3keys::delete_placement(&db, a, "default").unwrap();
        set_tenant(&db, 5, 0).unwrap();
        delete(&db, "acme").unwrap();
        assert_eq!(delete(&db, "acme"), Err(Error::NotFound));
        let r = load(&db).unwrap();
        assert_eq!(r.realms.len(), 1);
        assert_eq!(r.realm_of(5), 0);
        // Ids aren't reused while a higher one lives.
        assert_eq!(create(&db, &realm("cyan")).unwrap(), b + 1);
    }

    #[test]
    fn realm_names_are_placement_names_but_default() {
        assert!(valid_name("acme") && valid_name("t-1"));
        for bad in ["", "default", "Acme", "a/b", &"a".repeat(64)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
