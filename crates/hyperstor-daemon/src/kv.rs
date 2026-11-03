//! Key–value range jobs on each range's primary (25-kv-ranges.md §28.7):
//! a range's size is looked at after it is written, at most once a second,
//! and a sweep of every range held as primary looks at sizes and takes up
//! jobs that stopped. The jobs themselves ([`hyperstor_kv::jobs`]) go
//! through this reactor's client, as any client's writes would.

use super::*;
use hyperstor_kv::jobs::{self, Done, Limits};
use hyperstor_kv::{NS_KV, Range};

/// How often the sweep runs.
pub(crate) const SWEEP: Duration = Duration::from_secs(10);
/// Jobs one reactor runs at a time, so that copies don't crowd out its
/// clients' I/O.
const JOBS: usize = 2;

enum Job {
    Check(Limits),
    Resume(Range),
}

impl ShardState {
    /// Notes a write to a range object, for its size to be looked at.
    pub(crate) fn kv_wrote(&self, to: &Target, key: &WireKey) {
        if key.ns == NS_KV
            && key.name.len() == 16
            && let Ok(k) = engine_key(to, key)
        {
            self.kv_written.borrow_mut().insert(k);
        }
    }

    /// A range object as this device's copy has it, and its table's bytes.
    async fn kv_range(&self, k: &ObjKey) -> Option<(Range, u64)> {
        let id = u128::from_be_bytes(k.name.as_slice().try_into().ok()?);
        // Once no commit is in flight: commits leave the reactor (I199).
        let sh = self.shard.get().await;
        let bytes = sh.stat(k)?.kv_bytes;
        let attrs: Vec<_> = sh
            .attrs(k)?
            .iter()
            .map(|(a, v)| (a.clone(), v.clone()))
            .collect();
        Some((Range::from_attrs(id, &attrs).ok()?, bytes))
    }

    fn primary_of(&self, k: &ObjKey) -> bool {
        self.map().pg(k.pool, k.pg).and_then(|i| i.primary()) == Some(self.me)
    }

    pub(crate) async fn kv_ranges(self: Rc<Self>, limits: Limits, sweep: Duration) {
        // The epoch of each range found unfinished by the last sweep.
        let mut seen: HashMap<u128, u64> = HashMap::new();
        let mut swept = std::time::Instant::now();
        loop {
            sleep(sweep.min(Duration::from_secs(1))).await;
            let written: Vec<ObjKey> = self.kv_written.borrow_mut().drain().collect();
            for k in written {
                self.kv_size(&k, &limits).await;
            }
            if swept.elapsed() < sweep {
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
            for k in keys {
                let Some((r, _)) = self.kv_range(&k).await else {
                    continue;
                };
                if !jobs::unfinished(&r) {
                    self.kv_size(&k, &limits).await;
                    continue;
                }
                // Left so since the last sweep: no job is under way, or it
                // is stuck (a primary that died). Taking it up is safe
                // either way, and a live job's next step then fails.
                if seen.get(&r.id) == Some(&r.epoch) {
                    self.kv_job(k.pool, r.id, Job::Resume(r.clone()));
                }
                now.insert(r.id, r.epoch);
            }
            seen = now;
        }
    }

    /// Starts a check of range `k` if its size calls for a split or a
    /// merge (D202).
    async fn kv_size(self: &Rc<Self>, k: &ObjKey, limits: &Limits) {
        if !self.primary_of(k) {
            return;
        }
        let Some((r, bytes)) = self.kv_range(k).await else {
            return;
        };
        let small = bytes < limits.merge_bytes && r.end.is_some();
        if !jobs::unfinished(&r) && (bytes > limits.split_bytes || small) {
            self.kv_job(k.pool, r.id, Job::Check(*limits));
        }
    }

    fn kv_job(self: &Rc<Self>, pool: u32, id: u128, job: Job) {
        {
            let mut running = self.kv_jobs.borrow_mut();
            if running.len() >= JOBS || !running.insert(id) {
                return;
            }
        }
        let me = self.clone();
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
