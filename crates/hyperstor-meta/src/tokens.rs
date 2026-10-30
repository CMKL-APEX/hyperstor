//! Join tokens (spec/19-administration.md §23.4.2, 04-decisions.md D57, D81):
//! the records the cluster API checks a joining host's token against.
//!
//! - **Replicated** (a map service with `--data`): one row per token in the
//!   store's `tokens` table, keyed by the secret's SHA-256, written by the
//!   leader alone. A join marks its token used in the same Raft write that
//!   checks it, so a token is used once across every member. Any member
//!   lists them from its applied copy.
//! - **Bootstrap**: before the group has committed its first map there is
//!   no leader to write, yet members after the first join with `meta`
//!   tokens. The first member (the lowest number, which forms the group)
//!   then keeps its `meta` tokens in a file of its own, `bootstrap-tokens`
//!   in its data directory, readable by its service alone. Once the group
//!   has a map, no token is added there; those it holds stay usable until
//!   used or expired, and the file goes with the last of them.
//! - **In memory**, for a map service without a store (tests, benches).
//!
//! The secret is never kept, only its hash.

use crate::store::{self, Db, Origin, Table};
use glidex_raftstore::store::{StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// One token, without its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The secret's SHA-256, hex.
    pub hash: String,
    /// The first 12 hex digits of `hash`: what lists and revocations name.
    pub id: String,
    /// `node`, `meta` or `mgmt`.
    pub role: String,
    pub expires_unix: u64,
    pub used: bool,
    pub revoked: bool,
    pub created_by: String,
    /// `mgmt`: the identity it enrolls (D122); empty otherwise, and in
    /// records written before.
    pub name: String,
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.str(&self.id)
            .str(&self.role)
            .u64(self.expires_unix)
            .bool(self.used)
            .bool(self.revoked)
            .str(&self.created_by)
            .str(&self.name);
        e.into_vec()
    }

    fn decode(hash: &str, b: &[u8]) -> Result<Record, DecodeError> {
        let mut d = Dec::new(b);
        let r = Record {
            hash: hash.to_string(),
            id: d.str()?.to_string(),
            role: d.str()?.to_string(),
            expires_unix: d.u64()?,
            used: d.bool()?,
            revoked: d.bool()?,
            created_by: d.str()?.to_string(),
            name: if d.remaining() > 0 {
                d.str()?.to_string()
            } else {
                String::new()
            },
        };
        d.finish()?;
        Ok(r)
    }
}

/// Why a token operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// Only the leader writes tokens (D82).
    NotLeader,
    /// The group has no map yet, and this member isn't the first.
    NotFormed,
    /// The token can't be used: `reason` is the API's (`token_used`, …).
    Refused {
        reason: &'static str,
        why: String,
    },
    NotFound,
    Store(String),
}

impl From<StoreError> for TokenError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotLeader { .. } => TokenError::NotLeader,
            e => TokenError::Store(e.to_string()),
        }
    }
}

/// Whether `r` may be used now, for a join.
fn usable(r: &Record, now: u64) -> Result<(), TokenError> {
    let refuse = |reason, why: &str| {
        Err(TokenError::Refused {
            reason,
            why: format!("token {}: {why}", r.id),
        })
    };
    if r.revoked {
        return refuse("token_revoked", "revoked");
    }
    if r.used {
        return refuse("token_used", "already used");
    }
    if r.expires_unix <= now {
        return refuse("token_expired", "expired");
    }
    Ok(())
}

/// Tokens a day past their expiry are forgotten.
const FORGET_AFTER: u64 = 86400;

/// The first member's bootstrap tokens: a file of tab-separated lines,
/// rewritten whole (write beside, then rename) on every change.
struct Ledger {
    path: PathBuf,
}

impl Ledger {
    fn read(&self) -> Vec<Record> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                // An eighth field, the name, came with mgmt tokens.
                let (base, name) = match f.len() {
                    7 => (&f[..], ""),
                    8 => (&f[..7], f[7]),
                    _ => return None,
                };
                let [hash, id, role, exp, used, revoked, by] = base[..] else {
                    return None;
                };
                Some(Record {
                    hash: hash.into(),
                    id: id.into(),
                    role: role.into(),
                    expires_unix: exp.parse().ok()?,
                    used: used == "1",
                    revoked: revoked == "1",
                    created_by: by.into(),
                    name: name.into(),
                })
            })
            .collect()
    }

    /// Writes `records`, or removes the file once none is usable.
    fn write(&self, records: &[Record], now: u64) -> Result<(), TokenError> {
        if !records.iter().any(|r| usable(r, now).is_ok()) {
            let _ = std::fs::remove_file(&self.path);
            return Ok(());
        }
        let text: String = records
            .iter()
            .map(|r| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}{}\n",
                    r.hash,
                    r.id,
                    r.role,
                    r.expires_unix,
                    u8::from(r.used),
                    u8::from(r.revoked),
                    // The audit's names have no tabs; keep it so.
                    r.created_by.replace(['\t', '\n'], " "),
                    if r.name.is_empty() {
                        String::new()
                    } else {
                        format!("\t{}", r.name)
                    }
                )
            })
            .collect();
        let tmp = self.path.with_extension("new");
        let io = |e: std::io::Error| TokenError::Store(format!("{}: {e}", self.path.display()));
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(io)?;
            f.write_all(text.as_bytes()).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        std::fs::rename(&tmp, &self.path).map_err(io)
    }
}

enum Kind {
    Memory(Mutex<Vec<Record>>),
    Replicated {
        db: Arc<Db>,
        /// The first member's, while it has bootstrap tokens.
        ledger: Option<Mutex<Ledger>>,
    },
}

/// Where a map service member keeps join tokens.
pub struct Tokens(Kind);

impl Tokens {
    pub fn in_memory() -> Tokens {
        Tokens(Kind::Memory(Mutex::new(Vec::new())))
    }

    /// The replicated store `db`; with `bootstrap`, the first member's file
    /// for tokens made before the group has a map.
    pub fn replicated(db: Arc<Db>, bootstrap: Option<PathBuf>) -> Tokens {
        Tokens(Kind::Replicated {
            db,
            ledger: bootstrap.map(|path| Mutex::new(Ledger { path })),
        })
    }

    /// Adds `r`. In a replicated store only the leader can; before the
    /// group has a map, only the first member, and only `meta` tokens.
    pub fn create(&self, r: Record, now: u64) -> Result<(), TokenError> {
        match &self.0 {
            Kind::Memory(m) => {
                let mut v = lock(m);
                v.retain(|t| t.expires_unix + FORGET_AFTER > now);
                v.push(r);
                Ok(())
            }
            Kind::Replicated { db, ledger } => {
                if !store::has_map(db).map_err(TokenError::Store)? {
                    let (Some(l), "meta") = (ledger, r.role.as_str()) else {
                        return Err(TokenError::NotFormed);
                    };
                    let l = lock(l);
                    let mut v = l.read();
                    v.retain(|t| t.expires_unix + FORGET_AFTER > now);
                    v.push(r);
                    return l.write(&v, now);
                }
                let key = r.hash.clone();
                db.write(Origin::Leader, |tx| -> Result<(), StoreError> {
                    let mut t = tx.open_table(definition::<store::MapSchema>(Table::Tokens))?;
                    let mut old = Vec::new();
                    for row in t.iter()? {
                        let (k, v) = row?;
                        if let Ok(x) = Record::decode(k.value(), v.value())
                            && x.expires_unix + FORGET_AFTER <= now
                        {
                            old.push(k.value().to_string());
                        }
                    }
                    for k in old {
                        t.remove(k.as_str())?;
                    }
                    t.insert(key.as_str(), r.encode().as_slice())?;
                    Ok(())
                })?;
                Ok(())
            }
        }
    }

    /// Checks the token whose secret hashes to `hash` and marks it used,
    /// in one step: a bootstrap token on the first member, else a
    /// replicated one through the leader.
    pub fn redeem(&self, hash: &str, now: u64) -> Result<Record, TokenError> {
        let burn = |v: &mut Vec<Record>| -> Option<Result<Record, TokenError>> {
            let r = v.iter_mut().find(|t| t.hash == hash)?;
            Some(usable(r, now).map(|()| {
                r.used = true;
                r.clone()
            }))
        };
        match &self.0 {
            Kind::Memory(m) => burn(&mut lock(m)).unwrap_or(Err(TokenError::NotFound)),
            Kind::Replicated { db, ledger } => {
                if let Some(l) = ledger {
                    let l = lock(l);
                    let mut v = l.read();
                    if let Some(r) = burn(&mut v) {
                        // Burnt on disk before anything is signed.
                        let r = r?;
                        l.write(&v, now)?;
                        return Ok(r);
                    }
                }
                let out = db.write(Origin::Leader, |tx| -> Result<_, TokenError> {
                    let mut t = tx
                        .open_table(definition::<store::MapSchema>(Table::Tokens))
                        .map_err(StoreError::from)?;
                    let Some(v) = t.get(hash).map_err(StoreError::from)? else {
                        return Err(TokenError::NotFound);
                    };
                    let mut r = Record::decode(hash, v.value())
                        .map_err(|e| TokenError::Store(format!("token row: {e}")))?;
                    drop(v);
                    usable(&r, now)?;
                    r.used = true;
                    t.insert(hash, r.encode().as_slice())
                        .map_err(StoreError::from)?;
                    Ok(r)
                });
                // A refusal writes nothing: the write set is dropped.
                out
            }
        }
    }

    /// Marks the token with id `id` revoked.
    pub fn revoke(&self, id: &str, now: u64) -> Result<(), TokenError> {
        match &self.0 {
            Kind::Memory(m) => lock(m)
                .iter_mut()
                .find(|t| t.id == id)
                .map(|t| t.revoked = true)
                .ok_or(TokenError::NotFound),
            Kind::Replicated { db, ledger } => {
                if let Some(l) = ledger {
                    let l = lock(l);
                    let mut v = l.read();
                    if let Some(t) = v.iter_mut().find(|t| t.id == id) {
                        t.revoked = true;
                        return l.write(&v, now);
                    }
                }
                db.write(Origin::Leader, |tx| -> Result<(), TokenError> {
                    let mut t = tx
                        .open_table(definition::<store::MapSchema>(Table::Tokens))
                        .map_err(StoreError::from)?;
                    let mut found = None;
                    for row in t.iter().map_err(StoreError::from)? {
                        let (k, v) = row.map_err(StoreError::from)?;
                        if let Ok(r) = Record::decode(k.value(), v.value())
                            && r.id == id
                        {
                            found = Some(r);
                        }
                    }
                    let mut r = found.ok_or(TokenError::NotFound)?;
                    r.revoked = true;
                    t.insert(r.hash.as_str(), r.encode().as_slice())
                        .map_err(StoreError::from)?;
                    Ok(())
                })
            }
        }
    }

    /// Every token this member knows: bootstrap ones, then the store's
    /// (from its applied copy; any member answers).
    pub fn list(&self) -> Result<Vec<Record>, TokenError> {
        match &self.0 {
            Kind::Memory(m) => Ok(lock(m).clone()),
            Kind::Replicated { db, ledger } => {
                let mut out = ledger.as_ref().map(|l| lock(l).read()).unwrap_or_default();
                let txn = db
                    .begin_read()
                    .map_err(|e| TokenError::Store(e.to_string()))?;
                let t = txn
                    .open_table(definition::<store::MapSchema>(Table::Tokens))
                    .map_err(|e| TokenError::Store(e.to_string()));
                // A store from before this table: no tokens yet.
                let Ok(t) = t else { return Ok(out) };
                for row in t.iter().map_err(|e| TokenError::Store(e.to_string()))? {
                    let (k, v) = row.map_err(|e| TokenError::Store(e.to_string()))?;
                    if let Ok(r) = Record::decode(k.value(), v.value()) {
                        out.push(r);
                    }
                }
                Ok(out)
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: u8, role: &str, expires: u64) -> Record {
        let hash = format!("{n:02x}").repeat(32);
        Record {
            id: hash[..12].to_string(),
            hash,
            role: role.into(),
            expires_unix: expires,
            used: false,
            revoked: false,
            created_by: "test".into(),
            name: String::new(),
        }
    }

    /// A store that applies its own writes, as a group of one does.
    fn db(dir: &std::path::Path) -> Arc<Db> {
        Arc::new(Db::create(dir.join("map.redb")).unwrap())
    }

    #[test]
    fn replicated_tokens_are_used_once_and_revocable() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let m =
            crate::parse_cluster("cluster 1\npool 1 v replicated size 1 min 1 pgs 8\n").unwrap();
        store::save(&db, &m).unwrap();
        let t = Tokens::replicated(db.clone(), None);
        t.create(rec(1, "node", 100), 10).unwrap();
        t.create(rec(2, "node", 100), 10).unwrap();
        assert_eq!(t.redeem(&rec(1, "node", 0).hash, 20).unwrap().role, "node");
        let again = t.redeem(&rec(1, "node", 0).hash, 21);
        assert!(matches!(
            again,
            Err(TokenError::Refused {
                reason: "token_used",
                ..
            })
        ));
        t.revoke(&rec(2, "node", 0).id, 22).unwrap();
        let r = t.redeem(&rec(2, "node", 0).hash, 23);
        assert!(matches!(
            r,
            Err(TokenError::Refused {
                reason: "token_revoked",
                ..
            })
        ));
        assert_eq!(
            t.redeem(&rec(9, "node", 0).hash, 23),
            Err(TokenError::NotFound)
        );
        // Kept across a reopen: they are in the store, not in memory.
        drop(t);
        let t = Tokens::replicated(db, None);
        let l = t.list().unwrap();
        assert_eq!(l.len(), 2);
        assert!(l.iter().any(|r| r.used) && l.iter().any(|r| r.revoked));
        // Expired: refused, and forgotten a day later.
        t.create(rec(3, "node", 30), 25).unwrap();
        let r = t.redeem(&rec(3, "node", 0).hash, 31);
        assert!(matches!(
            r,
            Err(TokenError::Refused {
                reason: "token_expired",
                ..
            })
        ));
        t.create(rec(4, "node", 200_000), 30 + FORGET_AFTER)
            .unwrap();
        assert!(
            !t.list()
                .unwrap()
                .iter()
                .any(|r| r.id == rec(3, "node", 0).id)
        );
    }

    /// Before the group has a map, only the first member makes tokens, only
    /// `meta` ones, in its file; once there is a map, new tokens go to the
    /// store and the file's own stay usable until used, then it goes.
    #[test]
    fn bootstrap_tokens_live_on_the_first_member_until_used() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let file = dir.path().join("bootstrap-tokens");
        let first = Tokens::replicated(db.clone(), Some(file.clone()));
        let other = Tokens::replicated(db.clone(), None);
        assert_eq!(
            other.create(rec(1, "meta", 100), 10),
            Err(TokenError::NotFormed)
        );
        assert_eq!(
            first.create(rec(1, "node", 100), 10),
            Err(TokenError::NotFormed)
        );
        first.create(rec(1, "meta", 100), 10).unwrap();
        first.create(rec(2, "meta", 100), 10).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains("secret"));
        first.redeem(&rec(1, "meta", 0).hash, 20).unwrap();
        assert!(matches!(
            first.redeem(&rec(1, "meta", 0).hash, 21),
            Err(TokenError::Refused {
                reason: "token_used",
                ..
            })
        ));
        // The group forms: new tokens are replicated.
        let m =
            crate::parse_cluster("cluster 1\npool 1 v replicated size 1 min 1 pgs 8\n").unwrap();
        store::save(&db, &m).unwrap();
        first.create(rec(3, "meta", 100), 30).unwrap();
        assert!(
            !std::fs::read_to_string(&file)
                .unwrap()
                .contains(&rec(3, "meta", 0).hash)
        );
        assert_eq!(first.list().unwrap().len(), 3);
        // The bootstrap token left is still good, and the file goes with it.
        first.redeem(&rec(2, "meta", 0).hash, 40).unwrap();
        assert!(!file.exists());
        // Another member sees the replicated one.
        assert_eq!(
            other.redeem(&rec(3, "meta", 0).hash, 41).unwrap().id,
            rec(3, "meta", 0).id
        );
    }

    #[test]
    fn records_round_trip() {
        let mut r = rec(5, "meta", 77);
        r.used = true;
        assert_eq!(Record::decode(&r.hash, &r.encode()).unwrap(), r);
        assert!(Record::decode(&r.hash, &r.encode()[..3]).is_err());
    }
}
