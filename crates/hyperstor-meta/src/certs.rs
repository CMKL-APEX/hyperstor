//! Certificates the cluster API issued, and the revocation list
//! (spec/administration.md §23.8, decisions.md D56, D93).
//!
//! - **Issued**: every certificate signed through the API (joins and
//!   renewals), by serial: its identity, expiry and who asked. Recorded
//!   by the leader; a renewal through another member is still signed, and
//!   only its record is skipped (the list is for finding serials, not
//!   proof of every certificate).
//! - **Revoked**: serials refused from now on by every map service member,
//!   on the cluster API and between members, read from each member's
//!   applied copy of the store. An entry is kept until the certificate
//!   would have expired anyway (D56's "entries drop once the certificate
//!   would have expired"); for a serial the store didn't issue (the map
//!   service host's own certificates), a leaf's longest life.
//!
//! Without a store (tests, benches), both live in memory.

use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable as _;
use std::sync::{Arc, Mutex};

/// A certificate the API issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub serial: String,
    /// `hyperstor://<cluster>/<role>/<id>`.
    pub identity: String,
    pub not_after_unix: u64,
    pub issued_unix: u64,
    /// Who it was issued to or for, as the audit names callers.
    pub by: String,
}

/// A revoked serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    pub serial: String,
    /// Kept until then: the certificate is useless after.
    pub until_unix: u64,
    pub revoked_unix: u64,
    pub by: String,
}

/// A leaf lives 30 days (D56): how long a serial the store didn't issue
/// stays on the list, with a day's margin.
pub const UNKNOWN_KEEP: u64 = 31 * 86400;

fn enc_issued(i: &Issued) -> Vec<u8> {
    let mut e = Enc::new();
    e.str(&i.identity)
        .u64(i.not_after_unix)
        .u64(i.issued_unix)
        .str(&i.by);
    e.into_vec()
}

fn dec_issued(serial: &str, b: &[u8]) -> Result<Issued, DecodeError> {
    let mut d = Dec::new(b);
    let i = Issued {
        serial: serial.to_string(),
        identity: d.str()?.to_string(),
        not_after_unix: d.u64()?,
        issued_unix: d.u64()?,
        by: d.str()?.to_string(),
    };
    d.finish()?;
    Ok(i)
}

fn enc_revoked(r: &Revoked) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(r.until_unix).u64(r.revoked_unix).str(&r.by);
    e.into_vec()
}

fn dec_revoked(serial: &str, b: &[u8]) -> Result<Revoked, DecodeError> {
    let mut d = Dec::new(b);
    let r = Revoked {
        serial: serial.to_string(),
        until_unix: d.u64()?,
        revoked_unix: d.u64()?,
        by: d.str()?.to_string(),
    };
    d.finish()?;
    Ok(r)
}

enum Kind {
    Memory(Mutex<(Vec<Issued>, Vec<Revoked>)>),
    Replicated(Arc<Db>),
}

/// Where a map service member keeps issued certificates and revocations.
pub struct Certs(Kind);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Certs {
    pub fn in_memory() -> Certs {
        Certs(Kind::Memory(Mutex::new((Vec::new(), Vec::new()))))
    }

    pub fn replicated(db: Arc<Db>) -> Certs {
        Certs(Kind::Replicated(db))
    }

    /// Records a certificate issued; drops records of expired ones. Only
    /// the leader can (`Error::NotLeader` otherwise).
    pub fn record(&self, i: Issued, now: u64) -> Result<(), Error> {
        match &self.0 {
            Kind::Memory(m) => {
                let mut g = lock(m);
                g.0.retain(|x| x.not_after_unix > now);
                g.0.push(i);
                Ok(())
            }
            Kind::Replicated(db) => db.write(Origin::Leader, |tx| -> Result<(), Error> {
                let mut t = tx
                    .open_table(definition::<store::MapSchema>(Table::Issued))
                    .map_err(StoreError::from)?;
                let mut old = Vec::new();
                for row in t.iter().map_err(StoreError::from)? {
                    let (k, v) = row.map_err(StoreError::from)?;
                    if dec_issued(k.value(), v.value()).is_ok_and(|x| x.not_after_unix <= now) {
                        old.push(k.value().to_string());
                    }
                }
                for k in old {
                    t.remove(k.as_str()).map_err(StoreError::from)?;
                }
                t.insert(i.serial.as_str(), enc_issued(&i).as_slice())
                    .map_err(StoreError::from)?;
                Ok(())
            }),
        }
    }

    /// Revokes `serial`, kept until its certificate's expiry if the store
    /// issued it, else for [`UNKNOWN_KEEP`]; drops entries past theirs.
    /// Returns the entry. Only the leader can.
    pub fn revoke(&self, serial: &str, by: &str, now: u64) -> Result<Revoked, Error> {
        let serial = serial.trim_start_matches('0').to_ascii_lowercase();
        if serial.is_empty() || !serial.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Refused {
                reason: "invalid_argument",
                why: "a serial is hex digits".into(),
            });
        }
        let entry = |until| Revoked {
            serial: serial.clone(),
            until_unix: until,
            revoked_unix: now,
            by: by.to_string(),
        };
        match &self.0 {
            Kind::Memory(m) => {
                let mut g = lock(m);
                let until =
                    g.0.iter()
                        .find(|x| x.serial == serial)
                        .map_or(now + UNKNOWN_KEEP, |x| x.not_after_unix);
                g.1.retain(|x| x.until_unix > now && x.serial != serial);
                let r = entry(until);
                g.1.push(r.clone());
                Ok(r)
            }
            Kind::Replicated(db) => db.write(Origin::Leader, |tx| -> Result<Revoked, Error> {
                let until = {
                    let issued = tx
                        .open_table(definition::<store::MapSchema>(Table::Issued))
                        .map_err(StoreError::from)?;
                    let found = issued
                        .get(serial.as_str())
                        .map_err(StoreError::from)?
                        .and_then(|v| dec_issued(&serial, v.value()).ok());
                    found.map_or(now + UNKNOWN_KEEP, |x| x.not_after_unix)
                };
                let mut t = tx
                    .open_table(definition::<store::MapSchema>(Table::Revoked))
                    .map_err(StoreError::from)?;
                let mut old = Vec::new();
                for row in t.iter().map_err(StoreError::from)? {
                    let (k, v) = row.map_err(StoreError::from)?;
                    if dec_revoked(k.value(), v.value()).is_ok_and(|x| x.until_unix <= now) {
                        old.push(k.value().to_string());
                    }
                }
                for k in old {
                    t.remove(k.as_str()).map_err(StoreError::from)?;
                }
                let r = entry(until.max(now + 1));
                t.insert(serial.as_str(), enc_revoked(&r).as_slice())
                    .map_err(StoreError::from)?;
                Ok(r)
            }),
        }
    }

    /// Whether `serial` is revoked, from this member's copy. A store that
    /// can't be read counts as not revoked: the check never locks the
    /// cluster out of its own API.
    pub fn is_revoked(&self, serial: &str, now: u64) -> bool {
        match &self.0 {
            Kind::Memory(m) => lock(m)
                .1
                .iter()
                .any(|r| r.serial == serial && r.until_unix > now),
            Kind::Replicated(db) => (|| {
                let txn = db.begin_read().ok()?;
                let t = txn
                    .open_table(definition::<store::MapSchema>(Table::Revoked))
                    .ok()?;
                let v = t.get(serial).ok()??;
                dec_revoked(serial, v.value()).ok()
            })()
            .is_some_and(|r| r.until_unix > now),
        }
    }

    /// The certificates recorded and the revocations, from this member's
    /// copy (any member answers).
    pub fn list(&self) -> Result<(Vec<Issued>, Vec<Revoked>), Error> {
        match &self.0 {
            Kind::Memory(m) => {
                let g = lock(m);
                Ok((g.0.clone(), g.1.clone()))
            }
            Kind::Replicated(db) => {
                let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
                let mut issued = Vec::new();
                if let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Issued)) {
                    for row in t.iter().map_err(|e| Error::Store(e.to_string()))? {
                        let (k, v) = row.map_err(|e| Error::Store(e.to_string()))?;
                        if let Ok(i) = dec_issued(k.value(), v.value()) {
                            issued.push(i);
                        }
                    }
                }
                let mut revoked = Vec::new();
                if let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Revoked)) {
                    for row in t.iter().map_err(|e| Error::Store(e.to_string()))? {
                        let (k, v) = row.map_err(|e| Error::Store(e.to_string()))?;
                        if let Ok(r) = dec_revoked(k.value(), v.value()) {
                            revoked.push(r);
                        }
                    }
                }
                Ok((issued, revoked))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued(serial: &str, not_after: u64) -> Issued {
        Issued {
            serial: serial.into(),
            identity: "hyperstor://1/node/0".into(),
            not_after_unix: not_after,
            issued_unix: 1,
            by: "test".into(),
        }
    }

    fn both() -> Vec<(Certs, Option<tempfile::TempDir>)> {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("map.redb")).unwrap());
        vec![
            (Certs::in_memory(), None),
            (Certs::replicated(db), Some(dir)),
        ]
    }

    /// A revoked serial is refused until its certificate would have
    /// expired, then dropped; one the store didn't issue is kept a leaf's
    /// life.
    #[test]
    fn revocations_last_as_long_as_the_certificate() {
        for (c, _dir) in both() {
            c.record(issued("abc1", 1000), 10).unwrap();
            let r = c.revoke("00ABC1", "admin", 20).unwrap();
            assert_eq!(
                (r.serial.as_str(), r.until_unix),
                ("abc1", 1000),
                "normalized"
            );
            assert!(c.is_revoked("abc1", 999));
            assert!(!c.is_revoked("abc1", 1000), "past its certificate's end");
            let u = c.revoke("ff", "admin", 20).unwrap();
            assert_eq!(u.until_unix, 20 + UNKNOWN_KEEP);
            assert!(c.is_revoked("ff", 30) && !c.is_revoked("fe", 30));
            assert!(c.revoke("xyz", "admin", 20).is_err());
            // A later revocation drops the ones past their end.
            c.revoke("aa", "admin", 2000).unwrap();
            let (iss, rev) = c.list().unwrap();
            assert!(!rev.iter().any(|r| r.serial == "abc1"), "{rev:?}");
            assert_eq!(iss.len(), 1);
        }
    }
}
