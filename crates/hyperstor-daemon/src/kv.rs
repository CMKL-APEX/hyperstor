//! Key–value range jobs on each range's primary (25-kv-ranges.md §28.5–
//! §28.7): a range is looked at after it is written, at most once a
//! second, for its deletion queue and its size; a sweep of every range
//! held as primary looks at sizes, takes up jobs that stopped, and now and
//! then finds ranges nothing names. The jobs themselves
//! ([`hyperstor_kv::jobs`]) go through this reactor's client, as any
//! client's writes would.

use super::*;
use hyperstor_kv::jobs::{self, Done};
use hyperstor_kv::keys::{QUEUE, now_secs, queue_due_end};
use hyperstor_kv::{NS_KV, Range};

/// How often the sweep runs.
pub(crate) const SWEEP: Duration = Duration::from_secs(10);
/// How long a range nothing names is kept (§28.5): long past any crash
/// and take-up, which take seconds.
pub(crate) const LEFTOVER: Duration = Duration::from_secs(3600);
/// Jobs one reactor runs at a time, so that copies don't crowd out its
/// clients' I/O.
const JOBS: usize = 2;

enum Job {
    Check(jobs::Limits),
    Resume(Range),
    Drain(Range),
}

/// A range object as a device's copy has it.
struct Local {
    range: Range,
    /// Its table's bytes.
    bytes: u64,
    /// Whether its deletion queue has entries due.
    queued: bool,
}

impl ShardState {
    /// Notes a write to a range object, for it to be looked at.
    pub(crate) fn kv_wrote(&self, to: &Target, key: &WireKey) {
        if key.ns == NS_KV
            && key.name.len() == 16
            && let Ok(k) = engine_key(to, key)
        {
            self.kv_written.borrow_mut().insert(k);
        }
    }

    async fn kv_local(&self, k: &ObjKey) -> Option<Local> {
        let id = u128::from_be_bytes(k.name.as_slice().try_into().ok()?);
        // Once no commit is in flight: commits leave the reactor (I199).
        let sh = self.shard.get().await;
        let bytes = sh.stat(k)?.kv_bytes;
        let attrs: Vec<_> = sh
            .attrs(k)?
            .iter()
            .map(|(a, v)| (a.clone(), v.clone()))
            .collect();
        let due = queue_due_end(now_secs());
        let queued = !sh.kv_scan(k, &[QUEUE], Some(&due), 1).is_empty();
        Some(Local {
            range: Range::from_attrs(id, &attrs).ok()?,
            bytes,
            queued,
        })
    }

    fn primary_of(&self, k: &ObjKey) -> bool {
        self.map().pg(k.pool, k.pg).and_then(|i| i.primary()) == Some(self.me)
    }

    pub(crate) async fn kv_ranges(self: Rc<Self>, cfg: KvJobs) {
        // The epoch of each range found unfinished by the last sweep.
        let mut seen: HashMap<u128, u64> = HashMap::new();
        // When each range was first found named by nothing.
        let mut unnamed: HashMap<u128, std::time::Instant> = HashMap::new();
        let mut swept = std::time::Instant::now();
        let mut named_at = std::time::Instant::now();
        loop {
            sleep(cfg.sweep.min(Duration::from_secs(1))).await;
            let written: Vec<ObjKey> = self.kv_written.borrow_mut().drain().collect();
            for k in written {
                self.kv_look(&k, &cfg.limits).await;
            }
            if swept.elapsed() < cfg.sweep {
                continue;
            }
            swept = std::time::Instant::now();
            let keys: Vec<ObjKey> = {
                let map = self.map();
                let sh = self.shard.get().await;
                map.pgs
                    .iter()
                    .filter(|(_, i)| i.primary() == Some(self.me))
                    .flat_map(|((pool, pg), _)| {
                        let from = ObjKey::new(*pool, *pg, Namespace::Kv, Vec::new(), 0);
                        let to = ObjKey::new(*pool, *pg, Namespace::S3, Vec::new(), 0);
                        sh.list(&from, &to)
                    })
                    .filter(|k| k.name.len() == 16)
                    .collect()
            };
            let mut now = HashMap::new();
            for k in &keys {
                let Some(l) = self.kv_local(k).await else {
                    continue;
                };
                let r = l.range;
                if !jobs::unfinished(&r) {
                    self.kv_look(k, &cfg.limits).await;
                    continue;
                }
                // Left so since the last sweep: no job is under way, or it
                // is stuck (a primary that died). Taking it up is safe
                // either way, and a live job's next step then fails.
                if seen.get(&r.id) == Some(&r.epoch) {
                    self.kv_job(k, Job::Resume(r.clone()));
                }
                now.insert(r.id, r.epoch);
            }
            seen = now;
            // A quarter of the leftover time between looks for ranges
            // nothing names: each look costs a few reads of the root.
            if named_at.elapsed() >= cfg.leftover / 4 {
                named_at = std::time::Instant::now();
                self.kv_leftovers(&keys, cfg.leftover, &mut unnamed).await;
            }
        }
    }

    /// Deletes ranges found named by nothing on every look for `leftover`
    /// (§28.5). A primary that restarts starts counting again.
    async fn kv_leftovers(
        &self,
        keys: &[ObjKey],
        leftover: Duration,
        unnamed: &mut HashMap<u128, std::time::Instant>,
    ) {
        let mut still = HashMap::new();
        for k in keys {
            let Some(l) = self.kv_local(k).await else {
                continue;
            };
            let r = l.range;
            match jobs::named(&self.client, k.pool, &r).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("hyperstord: device {}: range {:032x}: {e}", self.me, r.id);
                    continue;
                }
            }
            let since = unnamed
                .get(&r.id)
                .copied()
                .unwrap_or_else(std::time::Instant::now);
            if since.elapsed() < leftover {
                still.insert(r.id, since);
                continue;
            }
            match jobs::delete_leftover(&self.client, k.pool, &r).await {
                Ok(true) => eprintln!(
                    "hyperstord: device {}: range {:032x} deleted: nothing named it for {} s",
                    self.me,
                    r.id,
                    leftover.as_secs()
                ),
                Ok(false) => {}
                Err(e) => eprintln!("hyperstord: device {}: range {:032x}: {e}", self.me, r.id),
            }
        }
        *unnamed = still;
    }

    /// Starts a drain of range `k`'s deletion queue if it has entries due, or
    /// else a check if its size calls for a split or a merge (D202).
    async fn kv_look(self: &Rc<Self>, k: &ObjKey, limits: &jobs::Limits) {
        if !self.primary_of(k) {
            return;
        }
        let Some(l) = self.kv_local(k).await else {
            return;
        };
        if l.queued {
            return self.kv_job(k, Job::Drain(l.range));
        }
        let small = l.bytes < limits.merge_bytes && l.range.end.is_some();
        if !jobs::unfinished(&l.range) && (l.bytes > limits.split_bytes || small) {
            self.kv_job(k, Job::Check(*limits));
        }
    }

    fn kv_job(self: &Rc<Self>, k: &ObjKey, job: Job) {
        let Ok(id) = k.name.as_slice().try_into().map(u128::from_be_bytes) else {
            return;
        };
        {
            let mut running = self.kv_jobs.borrow_mut();
            if running.len() >= JOBS || !running.insert(id) {
                // Looked at again on the next sweep, or the next write.
                return;
            }
        }
        let (me, k, pool) = (self.clone(), k.clone(), k.pool);
        spawn(async move {
            let cl = me.client.clone();
            let what = match job {
                Job::Check(limits) => jobs::check(&cl, pool, id, &limits).await.map(|d| match d {
                    Done::Nothing => None,
                    Done::Split(n) => Some(format!("split; its upper rows are in {n:032x}")),
                    Done::Merged(n) => Some(format!("took in the rows of {n:032x}")),
                }),
                Job::Resume(r) => jobs::resume(&cl, pool, &r)
                    .await
                    .map(|()| Some(format!("looked at, found {}", r.state.name()))),
                Job::Drain(r) => jobs::drain(&cl, pool, &r, Some(now_secs())).await.map(|n| {
                    // More, or a size to look at: once more.
                    me.kv_written.borrow_mut().insert(k.clone());
                    (n > 0).then(|| format!("deleted the objects of {n} queue entries"))
                }),
            };
            match what {
                Ok(Some(what)) => eprintln!("hyperstord: device {}: range {id:032x} {what}", me.me),
                Ok(None) => {}
                Err(e) => eprintln!("hyperstord: device {}: range {id:032x}: {e}", me.me),
            }
            me.kv_jobs.borrow_mut().remove(&id);
        });
    }
}
