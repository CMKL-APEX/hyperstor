//! Long operations (19-administration.md §23.5, 04-decisions.md D120, D121):
//! drains, flattens, copies, moves and purges, as rows of the replicated store, so every
//! member answers for them, they survive restarts, and a new leader
//! resumes those still running. Without a store (a map service of one,
//! without `--data`) they are kept in memory.
//!
//! A row is the operation the API shows, plus what is needed to run it
//! again: each kind restarts safely (a drain is followed from the map, a
//! flatten copies what is left, a copy starts over, a move starts over or
//! finishes, a purge deletes what is left). Finished
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
    /// A device out, followed until no PG lives on it, then removed from
    /// the map (24-pools-and-devices.md §27.2.5, D186).
    RemoveDevice {
        device: u32,
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
    /// A volume, or its snapshot at `epoch`, copied into new volume
    /// `new_id` of `pool` (10-volumes.md §10.6, D214).
    Copy {
        #[serde(with = "hex_id")]
        volume: u128,
        epoch: Option<u64>,
        pool: u32,
        #[serde(with = "hex_id")]
        new_id: u128,
        name: String,
    },
    /// A volume moved to `pool`, keeping its id (D214).
    Move {
        #[serde(with = "hex_id")]
        volume: u128,
        pool: u32,
    },
    /// A pool's PGs scrubbed now (28-scrub.md §31.6, D229), deep or
    /// light: done once each reported a scrub started at or after `since`
    /// (Unix seconds).
    Scrub {
        pool: u32,
        pgs: Vec<u32>,
        deep: bool,
        since: u64,
    },
    /// A volume, or one of its snapshots, deleted from the trash.
    Purge {
        #[serde(with = "hex_id")]
        volume: u128,
        epoch: Option<u64>,
    },
    /// A project moved from tenant `from` to tenant `to` (20-tenants.md
    /// §24.9.1, D303): its buckets renamed, then its record. Not
    /// cancelled: it ends moved, or undone if it can't.
    MoveProject { project: u32, from: u32, to: u32 },
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
    /// Whether `other` asks for what this does, so that a retried request
    /// is answered with the operation already running (I472): the same
    /// node drained, the same device removed (the PGs that move are found
    /// when asked, so they differ); the same PGs scrubbed as deep, whenever
    /// asked; any other kind with the same fields.
    pub fn same(&self, other: &Work) -> bool {
        match (self, other) {
            (Work::Drain { node: a, .. }, Work::Drain { node: b, .. }) => a == b,
            (Work::RemoveDevice { device: a, .. }, Work::RemoveDevice { device: b, .. }) => a == b,
            (
                Work::Scrub {
                    pool: a,
                    pgs: p,
                    deep: x,
                    ..
                },
                Work::Scrub {
                    pool: b,
                    pgs: q,
                    deep: y,
                    ..
                },
            ) => a == b && p == q && x == y,
            (a, b) => a == b,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Work::Drain { .. } => "drain",
            Work::Flatten { .. } => "flatten",
            Work::Copy { .. } => "copy",
            Work::Move { .. } => "move",
            Work::Purge { .. } => "purge",
            Work::Scrub { .. } => "scrub",
            Work::DeletePool { .. } => "delete-pool",
            Work::RemoveDevice { .. } => "remove-device",
            Work::MoveProject { .. } => "move-project",
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
    /// The tenant of the volume it acts on (D283), whose realm's people
    /// see it; `None` for the cluster's own (drains, scrubs, pools,
    /// devices) and in records written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<u32>,
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

    /// Records `op`, unless an operation that does the [same](Work::same)
    /// is running: that one, and `false`. Checked and written in one
    /// transaction, so two retries of one request start one operation
    /// (I472). Only the leader, with a store.
    pub fn begin(&self, op: &Op) -> Result<(Op, bool), Error> {
        let running = |o: &Op| o.running() && o.work.same(&op.work);
        match &self.0 {
            Rows::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(o) = m.values().find(|o| running(o)) {
                    return Ok((o.clone(), false));
                }
                m.insert(op.id.clone(), op.clone());
                Ok((op.clone(), true))
            }
            Rows::Db(db) => {
                let v = serde_json::to_vec(op).map_err(|e| Error::Store(e.to_string()))?;
                db.write(Origin::Leader, |tx| -> Result<(Op, bool), Error> {
                    let mut t = tx
                        .open_table(definition::<store::MapSchema>(Table::Operations))
                        .map_err(StoreError::from)?;
                    for row in t.iter().map_err(StoreError::from)? {
                        let (_, val) = row.map_err(StoreError::from)?;
                        if let Ok(o) = serde_json::from_slice::<Op>(val.value())
                            && running(&o)
                        {
                            return Ok((o, false));
                        }
                    }
                    t.insert(op.id.as_str(), v.as_slice())
                        .map_err(StoreError::from)?;
                    Ok((op.clone(), true))
                })
            }
        }
    }

    /// Changes operation `id` with `f`: its new self, or `None` if there
    /// is none. Read and written in one transaction, so `f` sees what any
    /// other change wrote (a cancel isn't undone by a progress report
    /// read before it). Only the leader, with a store.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Op)) -> Result<Option<Op>, Error> {
        match &self.0 {
            Rows::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                Ok(m.get_mut(id).map(|o| {
                    f(o);
                    o.clone()
                }))
            }
            Rows::Db(db) => db.write(Origin::Leader, |tx| -> Result<Option<Op>, Error> {
                let mut t = tx
                    .open_table(definition::<store::MapSchema>(Table::Operations))
                    .map_err(StoreError::from)?;
                let old = t.get(id).map_err(StoreError::from)?;
                let Some(mut o) = old
                    .map(|v| serde_json::from_slice::<Op>(v.value()))
                    .transpose()
                    .map_err(|e| Error::Store(e.to_string()))?
                else {
                    return Ok(None);
                };
                f(&mut o);
                let v = serde_json::to_vec(&o).map_err(|e| Error::Store(e.to_string()))?;
                t.insert(id, v.as_slice()).map_err(StoreError::from)?;
                Ok(Some(o))
            }),
        }
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
            tenant: None,
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
        for (i, work) in [
            Work::Copy {
                volume: 1 << 100,
                epoch: Some(3),
                pool: 2,
                new_id: 7 << 90,
                name: "c".into(),
            },
            Work::Move {
                volume: 1 << 100,
                pool: 2,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let c = Op {
                work,
                ..op(&format!("c{i}"), now, State::Running, 0)
            };
            ops.put(&c).unwrap();
            assert_eq!(ops.get(&c.id).unwrap().unwrap(), c);
        }
        let p = Op {
            work: Work::DeletePool { pool: 3 },
            ..op("p", now, State::Running, 0)
        };
        ops.put(&p).unwrap();
        assert_eq!(ops.get("p").unwrap().unwrap(), p);
        let r = Op {
            work: Work::RemoveDevice {
                device: 3001,
                pgs: vec![(1, 4), (0, 2)],
            },
            ..op("r", now, State::Running, 0)
        };
        ops.put(&r).unwrap();
        assert_eq!(ops.get("r").unwrap().unwrap(), r);
        assert_eq!(
            ops.get("b").unwrap().unwrap().work,
            op("b", 0, State::Done, 0).work
        );
    }

    /// A cancel racing a task's progress reports and its finish: whichever
    /// lands first ends the operation, and nothing after it starts it
    /// again or changes how it ended (I472). Before, an update read the
    /// row, then wrote it whole, so a progress report read before the
    /// cancel put `Running` back.
    fn a_cancel_stays(ops: Arc<Ops>) {
        for round in 0..20 {
            let id = format!("r{round}");
            ops.put(&op(&id, now_unix(), State::Running, 0)).unwrap();
            let progress = {
                let (ops, id) = (ops.clone(), id.clone());
                std::thread::spawn(move || {
                    for n in 0..50 {
                        ops.update(&id, |o| {
                            if o.running() {
                                o.done = n;
                            }
                        })
                        .unwrap();
                    }
                    ops.update(&id, |o| {
                        if o.running() {
                            o.state = State::Done;
                        }
                    })
                    .unwrap()
                    .unwrap()
                })
            };
            let cancel = {
                let (ops, id) = (ops.clone(), id.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_micros(200 * round));
                    ops.update(&id, |o| {
                        if o.running() {
                            o.state = State::Cancelled;
                        }
                    })
                    .unwrap()
                    .unwrap()
                })
            };
            let (finished, cancelled) = (progress.join().unwrap(), cancel.join().unwrap());
            let last = ops.get(&id).unwrap().unwrap();
            assert_ne!(last.state, State::Running, "round {round}");
            // The second to land saw how the first ended it.
            assert_eq!(finished.state, last.state, "round {round}");
            assert_eq!(cancelled.state, last.state, "round {round}");
        }
    }

    #[test]
    fn a_cancel_is_never_undone() {
        a_cancel_stays(Arc::new(Ops::in_memory()));
        let dir = tempfile::tempdir().unwrap();
        a_cancel_stays(Arc::new(Ops::replicated(Arc::new(
            Db::create(dir.path().join("map.redb")).unwrap(),
        ))));
    }

    /// Starts at random, several at once, of a few kinds on a few targets,
    /// with operations ending between them, against a model: at most one
    /// operation runs for each kind of work, a start meeting it answers
    /// it, and any other starts a new one (I472).
    fn starts_match_a_model(ops: Arc<Ops>) {
        use std::collections::HashMap;
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        // What runs, by its kind of work, in the model.
        let mut model: HashMap<String, String> = HashMap::new();
        let key = |w: &Work| match w {
            Work::Drain { node, .. } => format!("drain {node}"),
            Work::Scrub {
                pool, pgs, deep, ..
            } => format!("scrub {pool} {pgs:?} {deep}"),
            w => serde_json::to_string(w).unwrap(),
        };
        let mut next = 0;
        for _ in 0..300 {
            if rand(4) == 0 && !model.is_empty() {
                // One ends.
                let k = model
                    .keys()
                    .nth(rand(model.len() as u64) as usize)
                    .unwrap()
                    .clone();
                let id = model.remove(&k).unwrap();
                let s = [State::Done, State::Failed, State::Cancelled][rand(3) as usize];
                ops.update(&id, |o| o.state = s).unwrap().unwrap();
                continue;
            }
            let work = match rand(4) {
                0 => Work::Flatten {
                    volume: rand(3) as u128,
                },
                1 => Work::Drain {
                    node: rand(2) as u32,
                    devices: vec![],
                    pgs: vec![(0, rand(9) as u32)],
                },
                2 => Work::Scrub {
                    pool: rand(2) as u32,
                    pgs: [vec![], vec![3]][rand(2) as usize].clone(),
                    deep: rand(2) == 0,
                    since: rand(1000),
                },
                _ => Work::Purge {
                    volume: rand(2) as u128,
                    epoch: [None, Some(1)][rand(2) as usize],
                },
            };
            // The same start sent by up to four callers at once.
            let n = 1 + rand(4) as usize;
            let starts: Vec<_> = (0..n)
                .map(|_| {
                    next += 1;
                    let o = Op {
                        work: work.clone(),
                        ..op(&format!("s{next}"), now_unix(), State::Running, 0)
                    };
                    let ops = ops.clone();
                    std::thread::spawn(move || ops.begin(&o).unwrap())
                })
                .collect();
            let got: Vec<(Op, bool)> = starts.into_iter().map(|t| t.join().unwrap()).collect();
            let k = key(&work);
            let new: Vec<&Op> = got.iter().filter(|(_, n)| *n).map(|(o, _)| o).collect();
            match model.get(&k) {
                Some(id) => assert!(new.is_empty(), "{k} runs as {id}: none new"),
                None => {
                    assert_eq!(new.len(), 1, "{k}: one new");
                    model.insert(k.clone(), new[0].id.clone());
                }
            }
            for (o, _) in &got {
                assert_eq!(&o.id, &model[&k], "{k}: every caller has the running one");
            }
            let running: Vec<Op> = ops
                .list()
                .unwrap()
                .into_iter()
                .filter(Op::running)
                .collect();
            assert_eq!(running.len(), model.len());
        }
    }

    #[test]
    fn a_retried_start_answers_the_running_operation() {
        starts_match_a_model(Arc::new(Ops::in_memory()));
        let dir = tempfile::tempdir().unwrap();
        starts_match_a_model(Arc::new(Ops::replicated(Arc::new(
            Db::create(dir.path().join("map.redb")).unwrap(),
        ))));
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
