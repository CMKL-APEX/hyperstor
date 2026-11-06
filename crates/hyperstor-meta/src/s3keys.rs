//! S3's access keys and placements (14-s3.md §14.2, 04-decisions.md D231,
//! D232), rows of the replicated store's settings table, so a member from
//! before them still applies every entry (as scrub's records, I265):
//!
//! - `s3key/<id>`: the key's tenant, description, when and by whom it was
//!   made, and its secret sealed with the cluster key (D162), bound to the
//!   id. Revoking deletes the row.
//! - `s3place/<name>`: a placement's data and index pools, by id, so a
//!   renamed pool keeps its placements; `s3rplace/<realm>/<name>` for a
//!   realm other than "default" (D249), so members from before realms
//!   never read one.
//!
//! Writes go through the leader; reads come from the member's applied
//! copy, so any member answers a gateway.

use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable as _;

const KEY: &str = "s3key/";
const PLACE: &str = "s3place/";
const REALM_PLACE: &str = "s3rplace/";

/// The placement CreateBucket uses when its LocationConstraint names none.
pub const DEFAULT_PLACEMENT: &str = "default";

/// An access key as stored: its secret sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Key {
    pub id: String,
    pub tenant: u32,
    pub description: String,
    pub created_unix: u64,
    pub created_by: String,
    /// The secret, sealed by [`hyperstor_pki::ClusterKey::seal`] for
    /// [`context`].
    pub sealed: Vec<u8>,
}

impl S3Key {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(self.tenant)
            .str(&self.description)
            .u64(self.created_unix)
            .str(&self.created_by)
            .bytes(&self.sealed);
        e.into_vec()
    }

    fn decode(id: &str, b: &[u8]) -> Result<S3Key, DecodeError> {
        let mut d = Dec::new(b);
        let k = S3Key {
            id: id.to_string(),
            tenant: d.u32()?,
            description: d.str()?.to_string(),
            created_unix: d.u64()?,
            created_by: d.str()?.to_string(),
            sealed: d.bytes()?.to_vec(),
        };
        d.finish()?;
        Ok(k)
    }
}

/// What a key's secret is sealed for: its id, so a sealed secret copied to
/// another key's row doesn't open.
pub fn context(id: &str) -> String {
    format!("hyperstor s3 key {id}")
}

/// Pools for new buckets, by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub name: String,
    pub data_pool: u32,
    pub index_pool: u32,
    /// The realm whose buckets it places; 0 is "default" (D249).
    pub realm: u32,
}

impl Placement {
    fn row(realm: u32, name: &str) -> String {
        match realm {
            0 => format!("{PLACE}{name}"),
            r => format!("{REALM_PLACE}{r}/{name}"),
        }
    }
}

/// RFC 4648's base32 alphabet: ids that are all capitals and digits, as
/// AWS's look, and that no shell or URL needs to quote.
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A new access key id, `HS` and 18 base32 characters (90 random bits),
/// and its secret, 40 characters of URL-safe base64 (240 random bits).
pub fn new_key(random: impl Fn(usize) -> Vec<u8>) -> (String, String) {
    let id: String = random(18)
        .iter()
        .map(|b| BASE32[usize::from(b & 31)] as char)
        .collect();
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let secret: String = random(40)
        .iter()
        .map(|b| B64[usize::from(b & 63)] as char)
        .collect();
    (format!("HS{id}"), secret)
}

/// Whether `id` is shaped as [`new_key`] makes them.
pub fn valid_id(id: &str) -> bool {
    id.len() == 20 && id.starts_with("HS") && id[2..].bytes().all(|b| BASE32.contains(&b))
}

/// Whether `name` may name a placement: `default`, or what S3 allows in a
/// LocationConstraint we accept (1-63 of a-z, 0-9 and -).
pub fn valid_placement(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn rows(db: &Db, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in t.range(prefix..).map_err(|e| Error::Store(e.to_string()))? {
        let (k, v) = row.map_err(|e| Error::Store(e.to_string()))?;
        let Some(name) = k.value().strip_prefix(prefix) else {
            break;
        };
        out.push((name.to_string(), v.value().to_vec()));
    }
    Ok(out)
}

/// Every key, from this member's copy.
pub fn keys(db: &Db) -> Result<Vec<S3Key>, Error> {
    rows(db, KEY)?
        .into_iter()
        .map(|(id, v)| S3Key::decode(&id, &v).map_err(|e| Error::Store(format!("{KEY}{id}: {e}"))))
        .collect()
}

/// Adds `k`. Only the leader.
pub fn create(db: &Db, k: &S3Key) -> Result<(), Error> {
    let row = format!("{KEY}{}", k.id);
    db.write(Origin::Leader, |tx| -> Result<(), Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        // Ids are random; a clash would replace another's secret.
        if t.get(row.as_str()).map_err(StoreError::from)?.is_some() {
            return Err(Error::Store(format!("key {} exists", k.id)));
        }
        t.insert(row.as_str(), k.encode().as_slice())
            .map_err(StoreError::from)?;
        Ok(())
    })
}

/// Deletes key `id`. Only the leader.
pub fn revoke(db: &Db, id: &str) -> Result<(), Error> {
    let row = format!("{KEY}{id}");
    db.write(Origin::Leader, |tx| -> Result<(), Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        if t.remove(row.as_str()).map_err(StoreError::from)?.is_none() {
            return Err(Error::NotFound);
        }
        Ok(())
    })
}

/// `realm`'s placement `name`, from its row's value.
fn placement_of(realm: u32, name: &str, v: &[u8]) -> Result<Placement, DecodeError> {
    let mut d = Dec::new(v);
    let p = Placement {
        name: name.to_string(),
        data_pool: d.u32()?,
        index_pool: d.u32()?,
        realm,
    };
    d.finish()?;
    Ok(p)
}

/// Every realm's placements, from this member's copy: the data and index
/// pools' ids in each row. "default"'s first, then by realm id.
pub fn placements(db: &Db) -> Result<Vec<Placement>, Error> {
    let mut out = Vec::new();
    for (name, v) in rows(db, PLACE)? {
        out.push(
            placement_of(0, &name, &v)
                .map_err(|e| Error::Store(format!("{PLACE}{name}: {e}")))?,
        );
    }
    let mut others = Vec::new();
    for (rest, v) in rows(db, REALM_PLACE)? {
        let bad = |e: String| Error::Store(format!("{REALM_PLACE}{rest}: {e}"));
        let (realm, name) = rest
            .split_once('/')
            .and_then(|(r, n)| Some((r.parse::<u32>().ok()?, n)))
            .ok_or_else(|| bad("not <realm>/<name>".into()))?;
        others.push(placement_of(realm, name, &v).map_err(|e| bad(e.to_string()))?);
    }
    others.sort_by(|a, b| (a.realm, &a.name).cmp(&(b.realm, &b.name)));
    out.extend(others);
    Ok(out)
}

/// Adds or replaces `p`, unchecked. Only the leader; the API sets them
/// with `crate::realms::set_placement`, which checks its realm's pools.
pub fn set_placement(db: &Db, p: &Placement) -> Result<(), Error> {
    db.write(Origin::Leader, |tx| -> Result<(), Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        put_placement(&mut t, p)
    })
}

/// Writes `p`'s row in a write's settings table `t`.
pub(crate) fn put_placement(
    t: &mut redb::Table<&'static str, &'static [u8]>,
    p: &Placement,
) -> Result<(), Error> {
    let mut e = Enc::new();
    e.u32(p.data_pool).u32(p.index_pool);
    t.insert(Placement::row(p.realm, &p.name).as_str(), e.into_vec().as_slice())
        .map_err(StoreError::from)?;
    Ok(())
}

/// Removes `realm`'s placement `name`. Only the leader.
pub fn delete_placement(db: &Db, realm: u32, name: &str) -> Result<(), Error> {
    let row = Placement::row(realm, name);
    db.write(Origin::Leader, |tx| -> Result<(), Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        if t.remove(row.as_str()).map_err(StoreError::from)?.is_none() {
            return Err(Error::NotFound);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, tenant: u32) -> S3Key {
        S3Key {
            id: id.into(),
            tenant,
            description: "backups".into(),
            created_unix: 1_700_000_000,
            created_by: "hyperstor://9/admin/local".into(),
            sealed: vec![1, 2, 3],
        }
    }

    #[test]
    fn keys_and_placements_are_rows_of_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        assert!(keys(&db).unwrap().is_empty());
        let (a, b) = (
            key("HSAAAAAAAAAAAAAAAAAA", 1),
            key("HSBBBBBBBBBBBBBBBBBB", 2),
        );
        create(&db, &a).unwrap();
        create(&db, &b).unwrap();
        assert!(create(&db, &a).is_err(), "an id is never replaced");
        // Placements, after the keys' prefix, aren't keys.
        set_placement(
            &db,
            &Placement {
                name: DEFAULT_PLACEMENT.into(),
                data_pool: 1,
                index_pool: 3,
                realm: 0,
            },
        )
        .unwrap();
        // A realm's placement of the same name is another (D249).
        let theirs = Placement {
            name: DEFAULT_PLACEMENT.into(),
            data_pool: 4,
            index_pool: 4,
            realm: 7,
        };
        set_placement(&db, &theirs).unwrap();
        assert_eq!(keys(&db).unwrap(), vec![a.clone(), b.clone()]);
        revoke(&db, &a.id).unwrap();
        assert_eq!(revoke(&db, &a.id), Err(Error::NotFound));
        assert_eq!(keys(&db).unwrap(), vec![b]);
        let p = placements(&db).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].data_pool, p[0].index_pool, p[0].realm), (1, 3, 0));
        assert_eq!(p[1], theirs);
        delete_placement(&db, 0, DEFAULT_PLACEMENT).unwrap();
        assert_eq!(
            delete_placement(&db, 0, DEFAULT_PLACEMENT),
            Err(Error::NotFound)
        );
        assert_eq!(placements(&db).unwrap(), vec![theirs]);
        delete_placement(&db, 7, DEFAULT_PLACEMENT).unwrap();
        assert!(placements(&db).unwrap().is_empty());
    }

    #[test]
    fn new_keys_are_shaped_as_aws_keys() {
        let random = |len: usize| {
            (0..len)
                .map(|i| (i as u8).wrapping_mul(37))
                .collect::<Vec<u8>>()
        };
        let (id, secret) = new_key(random);
        assert!(valid_id(&id), "{id}");
        assert_eq!(secret.len(), 40);
        assert!(
            secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        for bad in [
            "",
            "HS",
            "AKIAAAAAAAAAAAAAAAAA",
            "HSaaaaaaaaaaaaaaaaaa",
            "HSAAAAAAAAAAAAAAAAA1",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(valid_placement("default") && valid_placement("fast-1"));
        for bad in ["", "Fast", "a b", "a/b", &"a".repeat(64)] {
            assert!(!valid_placement(bad), "{bad}");
        }
        assert_ne!(context("HSA"), context("HSB"));
    }
}
