//! Long operations (19-administration.md §23.5, 04-decisions.md D120, D121):
//! drains, flattens and purges, as rows of the replicated store, so every
//! member answers for them, they survive restarts, and a new leader
//! resumes those still running. Without a store (a map service of one,
//! without `--data`) they are kept in memory.
//!
//! A row is the operation the API shows, plus what is needed to run it
//! again: each kind restarts safely (a drain is followed from the map, a
//! flatten copies what is left, a purge deletes what is left). Finished
//! operations are kept [`KEEP_SECS`].

use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Finished operations are kept a week.
pub const KEEP_SECS: u64 = 7 * 86400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// What an operation does, with what it needs to run again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Work {
    /// A node's devices out, followed until no PG lives on it.
    Drain {
        node: u32,
        devices: Vec<u32>,
        /// The PGs that move: (pool, PG).
        pgs: Vec<(u32, u32)>,
    },
    /// A clone made independent of its parents.
    Flatten {
        #[serde(with = "hex_id")]
        volume: u128,
    },
    /// A pool deleted with what it holds (D176): its volumes to the trash
    /// and purged, then the pool.
    DeletePool { pool: u32 },
    /// A volume, or one of its snapshots, deleted from the trash.
    Purge {
        #[serde(with = "hex_id")]
        volume: u128,
        epoch: Option<u64>,
    },
}

/// Volume ids as 32 hex digits: serde can't buffer a `u128` in a tagged
/// enum's content (and JSON numbers that large aren't portable anyway).
mod hex_id {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{id:032x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        u128::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
    }
}

impl Work {
    pub fn kind(&self) -> &'static str {
        match self {
            Work::Drain { .. } => "drain",
            Work::Flatten { .. } => "flatten",
            Work::Purge { .. } => "purge",
            Work::DeletePool { .. } => "delete-pool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub id: String,
    pub work: Work,
    pub target: String,
    pub state: State,
    pub done: u32,
    pub total: u32,
    pub message: String,
    pub started_unix: u64,
    /// The start in milliseconds, which orders operations started in the
    /// same second; 0 in records written before it was kept.
    #[serde(default)]
    pub started_ms: u64,
    pub finished_unix: u64,
    pub created_by: String,
}

impl Op {
    pub fn running(&self) -> bool {
        self.state == State::Running
    }

    fn started(&self) -> u64 {
        if self.started_ms > 0 {
            self.started_ms
        } else {
            self.started_unix * 1000
        }
    }
}

enum Rows {
    Db(Arc<Db>),
    Memory(Mutex<BTreeMap<String, Op>>),
}

pub struct Ops(Rows);

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Ops {
    pub fn replicated(db: Arc<Db>) -> Ops {
        Ops(Rows::Db(db))
    }

    pub fn in_memory() -> Ops {
        Ops(Rows::Memory(Mutex::new(BTreeMap::new())))
    }

    pub fn get(&self, id: &str) -> Result<Option<Op>, Error> {
        match &self.0 {
            Rows::Memory(m) => Ok(m.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()),
            Rows::Db(db) => {
                let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
                let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Operations))
                else {
                    return Ok(None);
                };
                t.get(id)
                    .map_err(|e| Error::Store(e.to_string()))?
                    .map(|v| {
                        serde_json::from_slice(v.value()).map_err(|e| Error::Store(e.to_string()))
                    })
                    .transpose()
            }
        }
    }

    /// Every operation kept, newest first.
    pub fn list(&self) -> Result<Vec<Op>, Error> {
        let mut out: Vec<Op> = match &self.0 {
            Rows::Memory(m) => m
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect(),
            Rows::Db(db) => {
                let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
                let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Operations))
                else {
                    return Ok(Vec::new());
                };
                let mut v = Vec::new();
                for row in t.iter().map_err(|e| Error::Store(e.to_string()))? {
                    let (_, val) = row.map_err(|e| Error::Store(e.to_string()))?;
                    if let Ok(o) = serde_json::from_slice(val.value()) {
                        v.push(o);
                    }
                }
                v
            }
        };
        out.sort_by(|a, b| b.started().cmp(&a.started()).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    /// Writes `op`. Only the leader, with a store.
    pub fn put(&self, op: &Op) -> Result<(), Error> {
        match &self.0 {
            Rows::Memory(m) => {
                m.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(op.id.clone(), op.clone());
                Ok(())
            }
            Rows::Db(db) => {
                let v = serde_json::to_vec(op).map_err(|e| Error::Store(e.to_string()))?;
                db.write(Origin::Leader, |tx| -> Result<(), Error> {
                    let mut t = tx
                        .open_table(definition::<store::MapSchema>(Table::Operations))
                        .map_err(StoreError::from)?;
                    t.insert(op.id.as_str(), v.as_slice())
                        .map_err(StoreError::from)?;
                    Ok(())
                })
            }
        }
    }

    /// Changes operation `id` with `f` (read, then written whole): its new
    /// self, or `None` if there is none.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Op)) -> Result<Option<Op>, Error> {
        let Some(mut o) = self.get(id)? else {
            return Ok(None);
        };
        f(&mut o);
        self.put(&o)?;
        Ok(Some(o))
    }

    /// Removes operations finished more than [`KEEP_SECS`] ago. Only the
    /// leader.
    pub fn prune(&self) -> Result<usize, Error> {
        let now = now_unix();
        let old: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|o| !o.running() && o.finished_unix + KEEP_SECS < now)
            .map(|o| o.id)
            .collect();
        match &self.0 {
            Rows::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                for id in &old {
                    m.remove(id);
                }
            }
            Rows::Db(db) => {
                if !old.is_empty() {
                    db.write(Origin::Leader, |tx| -> Result<(), Error> {
                        let mut t = tx
                            .open_table(definition::<store::MapSchema>(Table::Operations))
                            .map_err(StoreError::from)?;
                        for id in &old {
                            t.remove(id.as_str()).map_err(StoreError::from)?;
                        }
                        Ok(())
                    })?;
                }
            }
        }
        Ok(old.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: &str, started: u64, state: State, finished: u64) -> Op {
        Op {
            id: id.into(),
            work: Work::Purge {
                volume: u128::MAX - 7,
                epoch: Some(3),
            },
            target: "volume ff…f8 snapshot 3".into(),
            state,
            done: 0,
            total: 1,
            message: String::new(),
            started_unix: started,
            started_ms: 0,
            finished_unix: finished,
            created_by: "t".into(),
        }
    }

    /// Operations started in the same second list by their milliseconds,
    /// not by their random ids; older records by their seconds.
    #[test]
    fn the_same_second_lists_newest_first() {
        let ops = Ops::in_memory();
        let at = |id: &str, ms: u64| Op {
            started_unix: ms / 1000,
            started_ms: ms,
            ..op(id, 0, State::Done, 0)
        };
        ops.put(&at("f0", 1_000_100)).unwrap();
        ops.put(&at("0f", 1_000_900)).unwrap();
        ops.put(&op("old", 999, State::Done, 0)).unwrap();
        let ids: Vec<String> = ops.list().unwrap().into_iter().map(|o| o.id).collect();
        assert_eq!(ids, ["0f", "f0", "old"]);
    }

    fn rows_behave(ops: Ops) {
        let now = now_unix();
        ops.put(&op("a", now - 10, State::Running, 0)).unwrap();
        ops.put(&op("b", now - 5, State::Done, now - 4)).unwrap();
        ops.put(&op("old", now - 9 * 86400, State::Done, now - 8 * 86400))
            .unwrap();
        let ids: Vec<String> = ops.list().unwrap().into_iter().map(|o| o.id).collect();
        assert_eq!(ids, ["b", "a", "old"], "newest first");
        let u = ops.update("a", |o| {
            o.done = 1;
            o.state = State::Done;
            o.finished_unix = now;
        });
        assert_eq!(u.unwrap().unwrap().done, 1);
        assert_eq!(ops.get("a").unwrap().unwrap().state, State::Done);
        assert!(ops.update("none", |_| {}).unwrap().is_none());
        assert_eq!(ops.prune().unwrap(), 1, "finished more than a week ago");
        assert!(ops.get("old").unwrap().is_none());
        // What a row holds survives encoding.
        let d = Op {
            work: Work::Drain {
                node: 2,
                devices: vec![2000],
                pgs: vec![(1, 4)],
            },
            ..op("d", now, State::Running, 0)
        };
        ops.put(&d).unwrap();
        assert_eq!(ops.get("d").unwrap().unwrap(), d);
        let f = Op {
            work: Work::Flatten { volume: 1 << 100 },
            ..op("f", now, State::Running, 0)
        };
        ops.put(&f).unwrap();
        assert_eq!(ops.get("f").unwrap().unwrap(), f);
        let p = Op {
            work: Work::DeletePool { pool: 3 },
            ..op("p", now, State::Running, 0)
        };
        ops.put(&p).unwrap();
        assert_eq!(ops.get("p").unwrap().unwrap(), p);
        assert_eq!(
            ops.get("b").unwrap().unwrap().work,
            op("b", 0, State::Done, 0).work
        );
    }

    #[test]
    fn operations_are_kept_listed_and_pruned() {
        rows_behave(Ops::in_memory());
        let dir = tempfile::tempdir().unwrap();
        rows_behave(Ops::replicated(Arc::new(
            Db::create(dir.path().join("map.redb")).unwrap(),
        )));
    }
}
