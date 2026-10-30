//! `hyperstor-mgmt`'s rows in the replicated store (04-decisions.md D98,
//! 19-administration.md §23.7): users, identities, sessions, tokens and role
//! links, as opaque values under keys mgmt chooses. Only the `mgmt`
//! identity reaches them, through `MgmtStoreService`; writes go through
//! the leader, reads come from the member's applied copy.

use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use redb::ReadableTable as _;
use std::sync::Arc;

/// Keys up to this long, values up to this large.
pub const MAX_KEY: usize = 256;
pub const MAX_VALUE: usize = 64 << 10;
/// A list returns this many rows at most.
pub const MAX_LIST: usize = 1000;

pub struct MgmtStore(Arc<Db>);

fn check_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.len() > MAX_KEY || key.chars().any(char::is_control) {
        return Err(Error::Refused {
            reason: "invalid_argument",
            why: format!("a key is 1 to {MAX_KEY} bytes, without control characters"),
        });
    }
    Ok(())
}

impl MgmtStore {
    pub fn new(db: Arc<Db>) -> MgmtStore {
        MgmtStore(db)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        check_key(key)?;
        let txn = self
            .0
            .begin_read()
            .map_err(|e| Error::Store(e.to_string()))?;
        let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Mgmt)) else {
            return Ok(None);
        };
        Ok(t.get(key)
            .map_err(|e| Error::Store(e.to_string()))?
            .map(|v| v.value().to_vec()))
    }

    /// Writes `value` under `key`; with `if_absent`, only if there is none
    /// (`Refused { reason: "already_exists" }` otherwise). Only the leader.
    pub fn put(&self, key: &str, value: &[u8], if_absent: bool) -> Result<(), Error> {
        check_key(key)?;
        if value.len() > MAX_VALUE {
            return Err(Error::Refused {
                reason: "invalid_argument",
                why: format!("a value is at most {MAX_VALUE} bytes"),
            });
        }
        self.0.write(Origin::Leader, |tx| -> Result<(), Error> {
            let mut t = tx
                .open_table(definition::<store::MapSchema>(Table::Mgmt))
                .map_err(StoreError::from)?;
            if if_absent && t.get(key).map_err(StoreError::from)?.is_some() {
                return Err(Error::Refused {
                    reason: "already_exists",
                    why: format!("{key} exists"),
                });
            }
            t.insert(key, value).map_err(StoreError::from)?;
            Ok(())
        })
    }

    /// Removes `key`; whether it was there. Only the leader.
    pub fn delete(&self, key: &str) -> Result<bool, Error> {
        check_key(key)?;
        self.0.write(Origin::Leader, |tx| -> Result<bool, Error> {
            let mut t = tx
                .open_table(definition::<store::MapSchema>(Table::Mgmt))
                .map_err(StoreError::from)?;
            Ok(t.remove(key).map_err(StoreError::from)?.is_some())
        })
    }

    /// Rows whose key starts with `prefix`, in key order, after `after`
    /// (exclusive), at most `limit` (and [`MAX_LIST`]).
    pub fn list(
        &self,
        prefix: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let txn = self
            .0
            .begin_read()
            .map_err(|e| Error::Store(e.to_string()))?;
        let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Mgmt)) else {
            return Ok(Vec::new());
        };
        let start = if after.is_empty() { prefix } else { after };
        let mut out = Vec::new();
        for row in t.range(start..).map_err(|e| Error::Store(e.to_string()))? {
            let (k, v) = row.map_err(|e| Error::Store(e.to_string()))?;
            let k = k.value();
            if !k.starts_with(prefix) {
                break;
            }
            if k == after {
                continue;
            }
            out.push((k.to_string(), v.value().to_vec()));
            if out.len() >= limit.clamp(1, MAX_LIST) {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_put_get_list_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = MgmtStore::new(Arc::new(Db::create(dir.path().join("map.redb")).unwrap()));
        assert_eq!(s.get("user/1").unwrap(), None);
        s.put("user/1", b"a", true).unwrap();
        assert!(matches!(
            s.put("user/1", b"b", true),
            Err(Error::Refused {
                reason: "already_exists",
                ..
            })
        ));
        s.put("user/1", b"b", false).unwrap();
        s.put("user/2", b"c", false).unwrap();
        s.put("users", b"x", false).unwrap();
        assert_eq!(s.get("user/1").unwrap().as_deref(), Some(&b"b"[..]));
        let all = s.list("user/", "", 10).unwrap();
        assert_eq!(all.len(), 2, "{all:?}");
        let next = s.list("user/", "user/1", 10).unwrap();
        assert_eq!(next, vec![("user/2".to_string(), b"c".to_vec())]);
        assert_eq!(s.list("user/", "", 1).unwrap().len(), 1);
        assert!(s.delete("user/1").unwrap());
        assert!(!s.delete("user/1").unwrap());
        assert!(s.put("", b"", false).is_err());
        assert!(s.put("k", &vec![0; MAX_VALUE + 1], false).is_err());
    }
}
