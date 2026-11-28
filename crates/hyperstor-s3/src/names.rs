//! Datasets' names in their projects (29-hsfs.md §32.3, D360, D424):
//! a table of `hyperstor-kv`'s store 2, one row per project and name,
//! claimed by a one-row batch before the dataset is made, so two makers
//! never both take a name. Here because both the map service (volumes,
//! bucket checks) and the gateway (deleting buckets) change it.
//!
//! The datasets stay the truth: a claim whose dataset doesn't exist (its
//! maker stopped in between, or a build from before the index deleted
//! it) is replaced by the next claim of the name, once [`GRACE`] has
//! passed since it was made.

use hyperstor_kv::{Batch, DATASETS_STORE, KvError, Store, Table};
use hyperstor_map::META_POOL;
use std::rc::Rc;

/// The table's name in store 2.
const TABLE: &[u8] = b"names";
/// The one column of a name's row: who holds it ([`Holder`]).
const HELD: &[u8] = b"d";
/// The column of [`COMPLETE`]'s row once every dataset made before the
/// index has its row.
const DONE: &[u8] = b"complete";
/// Project 0 is never indexed (volumes in no project are exempt, D387),
/// so its empty name's row marks the index complete.
const COMPLETE: &[u8] = &[0, 0, 0, 0];

/// How long a claim whose dataset isn't seen yet still holds its name:
/// its maker may be making it (ms).
pub const GRACE: u64 = 10 * 60 * 1000;

/// What holds a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Volume(u128),
    /// A bucket; its table id isn't known until the gateway makes it, and
    /// a project's bucket names are unique, so the kind is enough.
    Bucket,
}

/// A name's row: its holder and when it was claimed (Unix ms; 0: indexed
/// from a dataset that existed, so no grace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    pub owner: Owner,
    pub claimed_ms: u64,
}

impl Holder {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(25);
        match self.owner {
            Owner::Volume(id) => {
                b.push(b'v');
                b.extend_from_slice(&id.to_be_bytes());
            }
            Owner::Bucket => {
                b.push(b'b');
                b.extend_from_slice(&[0; 16]);
            }
        }
        b.extend_from_slice(&self.claimed_ms.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Holder, KvError> {
        let bad = || KvError::Corrupt(format!("a name's holder of {} bytes", b.len()));
        if b.len() != 25 {
            return Err(bad());
        }
        let id = u128::from_be_bytes(b[1..17].try_into().unwrap());
        let owner = match b[0] {
            b'v' => Owner::Volume(id),
            b'b' => Owner::Bucket,
            _ => return Err(bad()),
        };
        Ok(Holder {
            owner,
            claimed_ms: u64::from_be_bytes(b[17..].try_into().unwrap()),
        })
    }

    /// Whether a claim whose dataset isn't seen still holds its name at
    /// `now_ms`.
    pub fn fresh(&self, now_ms: u64) -> bool {
        self.claimed_ms != 0 && now_ms.saturating_sub(self.claimed_ms) < GRACE
    }
}

/// A name's row: the project, then the name's bytes.
pub fn row(project: u32, name: &str) -> Vec<u8> {
    let mut r = project.to_be_bytes().to_vec();
    r.extend_from_slice(name.as_bytes());
    r
}

/// What a claim found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Claimed,
    /// Someone holds the name; nothing was changed.
    Held(Holder),
}

/// The index, on one client's reactor.
pub struct Names {
    kv: Store,
}

impl Names {
    pub fn open(client: Rc<hyperstor_client::Client>) -> Names {
        Names {
            kv: Store::open(client, DATASETS_STORE),
        }
    }

    /// The table, made with the first claim (in the `.meta` pool, with
    /// the volumes' headers).
    async fn table(&self) -> Result<Table, KvError> {
        if let Some(t) = self.kv.table(TABLE).await? {
            return Ok(t);
        }
        match self.kv.create_table(TABLE, META_POOL, META_POOL).await {
            Err(KvError::TableExists(_)) => self
                .kv
                .table(TABLE)
                .await?
                .ok_or_else(|| KvError::NoTable(String::from_utf8_lossy(TABLE).into())),
            r => r,
        }
    }

    /// Who holds `name` in `project`, if anyone.
    pub async fn holder(&self, project: u32, name: &str) -> Result<Option<Holder>, KvError> {
        let t = self.table().await?;
        match self.kv.get(&t, &row(project, name), HELD).await? {
            Some(v) => Ok(Some(Holder::decode(&v)?)),
            None => Ok(None),
        }
    }

    /// Claims `name` in `project` for `new` if no one holds it, or if
    /// `replace` still does (a stale claim the caller checked).
    pub async fn claim(
        &self,
        project: u32,
        name: &str,
        new: Holder,
        replace: Option<Holder>,
    ) -> Result<Claim, KvError> {
        let t = self.table().await?;
        let r = row(project, name);
        let old = replace.map(|h| h.encode());
        let mut b = Batch::new();
        b.assert(HELD, old.as_deref()).put(HELD, &new.encode());
        match self.kv.apply(&t, &r, &b).await {
            Ok(()) => Ok(Claim::Claimed),
            Err(KvError::Conflict) => match self.kv.get(&t, &r, HELD).await? {
                // Our own claim, from an attempt whose answer was lost.
                Some(v) if v == new.encode() => Ok(Claim::Claimed),
                Some(v) => Ok(Claim::Held(Holder::decode(&v)?)),
                // Released meanwhile: the caller asks again.
                None => Err(KvError::Conflict),
            },
            Err(e) => Err(e),
        }
    }

    /// Frees `name` in `project` if `owner` holds it; someone else's
    /// claim is left alone. True if it was freed.
    pub async fn release(&self, project: u32, name: &str, owner: Owner) -> Result<bool, KvError> {
        let t = self.table().await?;
        let r = row(project, name);
        loop {
            let Some(v) = self.kv.get(&t, &r, HELD).await? else {
                return Ok(false);
            };
            if Holder::decode(&v)?.owner != owner {
                return Ok(false);
            }
            let mut b = Batch::new();
            b.assert(HELD, Some(&v)).delete(HELD);
            match self.kv.apply(&t, &r, &b).await {
                Err(KvError::Conflict) => continue,
                r => return r.map(|()| true),
            }
        }
    }

    /// Whether every dataset made before the index has its row.
    pub async fn complete(&self) -> Result<bool, KvError> {
        let t = self.table().await?;
        Ok(self.kv.get(&t, COMPLETE, DONE).await?.is_some())
    }

    pub async fn mark_complete(&self) -> Result<(), KvError> {
        let t = self.table().await?;
        let mut b = Batch::new();
        b.put(DONE, &[1]);
        self.kv.apply(&t, COMPLETE, &b).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holders_decode_as_encoded_and_age() {
        for owner in [Owner::Volume(0xabcd << 64 | 7), Owner::Bucket] {
            let h = Holder {
                owner,
                claimed_ms: 1_700_000_000_000,
            };
            assert_eq!(Holder::decode(&h.encode()).unwrap(), h);
        }
        assert!(Holder::decode(b"x").is_err());
        let mut bad = Holder {
            owner: Owner::Bucket,
            claimed_ms: 0,
        }
        .encode();
        bad[0] = b'?';
        assert!(Holder::decode(&bad).is_err());
        let h = Holder {
            owner: Owner::Bucket,
            claimed_ms: 1000,
        };
        assert!(h.fresh(1000 + GRACE - 1));
        assert!(!h.fresh(1000 + GRACE));
        let indexed = Holder { claimed_ms: 0, ..h };
        assert!(!indexed.fresh(1), "indexed from a dataset: no grace");
        // A project's rows sort together, and project 0's empty name is
        // the completion mark, never a dataset's.
        assert!(row(1, "zz") < row(2, ""));
        assert_eq!(row(0, ""), COMPLETE);
    }
}
