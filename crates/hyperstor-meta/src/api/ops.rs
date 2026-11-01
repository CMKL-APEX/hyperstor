//! Running long operations (04-decisions.md D120, D121): each kind in a task
//! on the API's thread, its row in the replicated store updated as it goes
//! (`crate::operations`). The leader resumes running operations it isn't
//! following (after a restart or a change of leader), prunes finished ones
//! past a week, and runs the trash's purger (D54), whose purges are
//! operations too.
//!
//! A leader runs nothing before it has fenced the last one (D125): that
//! one's client instance, recorded in the settings, is blocklisted, so a
//! flatten or a purge a former leader still runs can't write once this
//! one carries it on.

use super::audit;
use crate::Admin;
use crate::operations::{Op, Ops, State, Work};
use crate::tokens::TokenError;
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_reactor::chan::{Sender, oneshot};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// An operation as the API shows it.
pub(super) fn op_pb(o: &Op) -> pb::Operation {
    use pb::OperationState as S;
    pb::Operation {
        id: o.id.clone(),
        kind: o.work.kind().into(),
        target: o.target.clone(),
        state: match o.state {
            State::Running => S::OPERATION_STATE_RUNNING,
            State::Done => S::OPERATION_STATE_DONE,
            State::Failed => S::OPERATION_STATE_FAILED,
            State::Cancelled => S::OPERATION_STATE_CANCELLED,
        }
        .into(),
        done: o.done,
        total: o.total,
        message: o.message.clone(),
        started_unix: o.started_unix,
        finished_unix: o.finished_unix,
        created_by: o.created_by.clone(),
        ..Default::default()
    }
}

#[derive(Clone)]
pub(super) struct Runner {
    tx: Sender<Admin>,
    pub(super) ops: Arc<Ops>,
    /// Operations this member follows now.
    running: Arc<Mutex<HashSet<String>>>,
    /// Where the leaders' instances are recorded (none: no store, a map
    /// service of one).
    settings: Option<Arc<crate::settings::Settings>>,
    /// The lead this member has fenced for, by when it began.
    fenced: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
}

impl Runner {
    pub(super) fn new(
        tx: Sender<Admin>,
        ops: Arc<Ops>,
        settings: Option<Arc<crate::settings::Settings>>,
    ) -> Runner {
        Runner {
            tx,
            ops,
            running: Arc::default(),
            settings,
            fenced: Arc::default(),
        }
    }

    /// Fences the last leader once per lead of this member (D125): its
    /// client instance is blocklisted (answered once every storage node
    /// holds that, or its lease has run out), then this one's recorded.
    /// Refused unless this member leads.
    pub(super) async fn fence(&self) -> Result<(), TokenError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Lead(reply))
            .map_err(|_| TokenError::Store("map service stopped".into()))?;
        let (since, instance) = match rx.recv().await {
            Some(crate::Lead::Standalone) => return Ok(()),
            Some(crate::Lead::Leader { since, instance }) => (since, instance),
            Some(crate::Lead::Follower) | None => return Err(TokenError::NotLeader),
        };
        let mut fenced = self.fenced.lock().await;
        if *fenced == Some(since) {
            return Ok(());
        }
        if let Some(settings) = self.settings.clone() {
            let s = settings.clone();
            let last = tokio::task::spawn_blocking(move || s.ops_instance())
                .await
                .map_err(|e| TokenError::Store(e.to_string()))??;
            if let Some(last) = last.filter(|l| *l != instance) {
                let (reply, rx) = oneshot();
                self.tx
                    .send(Admin::Blocklist {
                        ids: vec![last],
                        reply,
                    })
                    .map_err(|_| TokenError::Store("map service stopped".into()))?;
                match rx.recv().await {
                    Some(Ok(epoch)) => eprintln!(
                        "hyperstor-meta: operations: the last leader's client \
                         {last:016x} is blocklisted (map epoch {epoch})"
                    ),
                    Some(Err(e)) => return Err(TokenError::Store(e)),
                    None => return Err(TokenError::Store("map service stopped".into())),
                }
            }
            tokio::task::spawn_blocking(move || settings.set_ops_instance(instance))
                .await
                .map_err(|e| TokenError::Store(e.to_string()))??;
        }
        *fenced = Some(since);
        Ok(())
    }

    /// `f` on the store, off this thread: a write waits for Raft.
    pub(super) async fn store<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Ops) -> Result<T, TokenError> + Send + 'static,
    ) -> Result<T, TokenError> {
        let ops = self.ops.clone();
        tokio::task::spawn_blocking(move || f(&ops))
            .await
            .map_err(|e| TokenError::Store(e.to_string()))?
    }

    async fn leads(&self) -> bool {
        let (reply, rx) = oneshot();
        self.tx.send(Admin::Leads(reply)).is_ok() && matches!(rx.recv().await, Some(Ok(())))
    }

    /// A new operation of `work`, recorded and started.
    pub(super) async fn create(
        &self,
        work: Work,
        target: String,
        total: u32,
        message: String,
        created_by: String,
    ) -> Result<Op, TokenError> {
        self.fence().await?;
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let op = Op {
            id: super::admin::random_hex(8),
            work,
            target,
            state: State::Running,
            done: 0,
            total,
            message,
            started_unix: started_ms / 1000,
            started_ms,
            finished_unix: 0,
            created_by,
        };
        let o = op.clone();
        self.store(move |s| s.put(&o)).await?;
        self.start(op.clone(), false);
        Ok(op)
    }

    /// Follows `op` in a task of its own, unless this member already does;
    /// `resumed` when it was started before (by this member or another).
    pub(super) fn start(&self, op: Op, resumed: bool) {
        if !self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(op.id.clone())
        {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let id = op.id.clone();
            match op.work.clone() {
                Work::Drain { node, devices, pgs } => {
                    // A drain changes the working map, which is committed
                    // when it is published: one taken just before the
                    // leader stopped may be lost. Taking the devices out
                    // again is a no-op if it wasn't.
                    if resumed {
                        let (reply, rx) = oneshot();
                        let sent = me.tx.send(Admin::Drain {
                            node,
                            dry_run: false,
                            reply,
                        });
                        if sent.is_err() || !matches!(rx.recv().await, Some(Ok(_))) {
                            me.forget(&id);
                            return;
                        }
                    }
                    let done = format!("node {node} drained: no PG lives on it");
                    me.follow_out(&op, &devices, &pgs, &format!("node {node}"), Some(done))
                        .await;
                }
                Work::RemoveDevice { device, pgs } => {
                    // As for a drain: taking it out again is a no-op if it
                    // was, and it was forced or checked when it started; a
                    // device the map no longer has was removed, which the
                    // finish below finds.
                    if resumed {
                        let (reply, rx) = oneshot();
                        let sent = me.tx.send(Admin::RemoveDevice {
                            device,
                            force: true,
                            dry_run: false,
                            reply,
                        });
                        if sent.is_err() || rx.recv().await.is_none() {
                            me.forget(&id);
                            return;
                        }
                    }
                    let what = format!("device {device}");
                    if me.follow_out(&op, &[device], &pgs, &what, None).await {
                        let (reply, rx) = oneshot();
                        let r = match me.tx.send(Admin::FinishRemoval { device, reply }) {
                            Ok(()) => rx.recv().await,
                            Err(_) => None,
                        };
                        // Done once the published map, which clients and
                        // the installer read, lacks it (it is fenced first).
                        if let Some(Ok(Some((_, epoch)))) = &r {
                            let t = std::time::Instant::now();
                            while t.elapsed() < Duration::from_secs(10) {
                                let (reply, rx) = oneshot();
                                if me.tx.send(Admin::Published(reply)).is_err() {
                                    break;
                                }
                                match rx.recv().await {
                                    Some((map, ..)) if map.epoch < *epoch => {}
                                    _ => break,
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        }
                        let done = match &r {
                            Some(Ok(Some((true, _)))) => format!(
                                "device {device} removed from the map, and node {} with it",
                                device / 1000
                            ),
                            _ => format!("device {device} removed from the map"),
                        };
                        let r =
                            r.map(|r| r.map(|_| ()).map_err(|e| (crate::VolumeFault::InUse, e)));
                        me.finish(&op, r, done).await;
                    }
                }
                Work::Flatten { volume } => {
                    let (reply, rx) = oneshot();
                    let sent = me.tx.send(Admin::Snapshots {
                        op: crate::SnapOp::Flatten { id: volume },
                        reply,
                    });
                    let r = match sent {
                        Ok(()) => rx.recv().await,
                        Err(_) => None,
                    };
                    me.finish(
                        &op,
                        r.map(|r| r.map(|_| ())),
                        format!("volume {volume:032x} flattened"),
                    )
                    .await;
                }
                Work::DeletePool { pool } => {
                    let r = me.delete_pool(&op, pool).await;
                    me.finish(&op, r, format!("pool {pool} deleted")).await;
                }
                Work::Purge { volume, epoch } => {
                    let (reply, rx) = oneshot();
                    let sent = me.tx.send(Admin::Trash {
                        op: crate::TrashOp::Purge { id: volume, epoch },
                        reply,
                    });
                    let r = match sent {
                        Ok(()) => rx.recv().await,
                        Err(_) => None,
                    };
                    let what = match epoch {
                        Some(e) => format!("snapshot {e} of volume {volume:032x} purged"),
                        None => format!("volume {volume:032x} purged"),
                    };
                    me.finish(&op, r.map(|r| r.map(|_| ())), what).await;
                }
            }
            me.forget(&id);
        });
    }

    fn forget(&self, id: &str) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// Records how a flatten or purge ended (none: the map service stopped,
    /// and a new leader resumes it).
    async fn finish(
        &self,
        op: &Op,
        r: Option<Result<(), (crate::VolumeFault, String)>>,
        done: String,
    ) {
        let Some(r) = r else { return };
        let (state, message) = match &r {
            Ok(()) => (State::Done, done),
            Err((_, e)) => (State::Failed, e.clone()),
        };
        let id = op.id.clone();
        let m = message.clone();
        let _ = self
            .store(move |s| {
                s.update(&id, |o| {
                    if o.running() {
                        o.state = state;
                        o.done = o.total;
                        o.message = m;
                        o.finished_unix = now_unix();
                    }
                })
            })
            .await;
        // Recorded under the call that started it, as the operation's.
        let rpc = match op.work {
            Work::Flatten { .. } => "FlattenVolume",
            Work::DeletePool { .. } => "DeletePool",
            Work::RemoveDevice { .. } => "RemoveDevice",
            _ => "PurgeFromTrash",
        };
        audit(
            &op.created_by,
            rpc,
            &op.target,
            &match state {
                State::Done => format!("ok, operation {} done", op.id),
                _ => format!("failed: operation {}: {message}", op.id),
            },
        );
    }

    /// Deletes pool `pool` with what it holds (D176): each volume to the
    /// trash, its holder preempted, then purged with its snapshots, each
    /// audited; then the pool. Purges are tried again while any succeeds,
    /// so clones go before what they were cloned from. Runs again from
    /// what is left when resumed; stops when cancelled. None: the map
    /// service stopped.
    async fn delete_pool(
        &self,
        op: &Op,
        pool: u32,
    ) -> Option<Result<(), (crate::VolumeFault, String)>> {
        let mut last_err = None;
        loop {
            let id = op.id.clone();
            match self.store(move |s| s.get(&id)).await {
                Ok(Some(now)) if now.running() => {}
                _ => return None,
            }
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::ListVolumes {
                    pool,
                    trash: true,
                    reply,
                })
                .ok()?;
            let left = match rx.recv().await? {
                Ok(v) => v,
                // The pool may already be gone: resumed after its delete.
                Err((crate::VolumeFault::NotFound, _)) => Vec::new(),
                Err(e) => return Some(Err(e)),
            };
            if left.is_empty() {
                break;
            }
            let id = op.id.clone();
            let n = left.len() as u32;
            let _ = self
                .store(move |s| {
                    s.update(&id, |o| {
                        o.total = o.total.max(n + 1);
                        o.done = o.total - n - 1;
                        o.message = format!("{n} volume(s) left");
                    })
                })
                .await;
            let mut progress = false;
            for v in left {
                let target = format!("volume {:032x} (pool {pool} deleted)", v.id);
                if v.state != crate::VolumeState::Trashed {
                    let (reply, rx) = oneshot();
                    self.tx
                        .send(Admin::DeleteVolume {
                            id: v.id,
                            force: true,
                            reply,
                        })
                        .ok()?;
                    let r = rx.recv().await?;
                    audit(
                        &op.created_by,
                        "DeleteVolume",
                        &format!("{target} force"),
                        &match &r {
                            Ok(_) => format!("ok, operation {}", op.id),
                            Err((_, e)) => format!("failed: {e}"),
                        },
                    );
                    if let Err(e) = r {
                        last_err = Some(e);
                        continue;
                    }
                }
                let (reply, rx) = oneshot();
                self.tx
                    .send(Admin::Trash {
                        op: crate::TrashOp::Purge {
                            id: v.id,
                            epoch: None,
                        },
                        reply,
                    })
                    .ok()?;
                let r = rx.recv().await?;
                audit(
                    &op.created_by,
                    "PurgeFromTrash",
                    &target,
                    &match &r {
                        Ok(_) => format!("ok, operation {}", op.id),
                        Err((_, e)) => format!("failed: {e}"),
                    },
                );
                match r {
                    Ok(_) => progress = true,
                    Err(e) => last_err = Some(e),
                }
            }
            if !progress {
                return Some(Err(last_err.unwrap_or((
                    crate::VolumeFault::Invalid,
                    "no volume could be purged".into(),
                ))));
            }
        }
        let (reply, rx) = oneshot();
        self.tx.send(Admin::DeletePool { pool, reply }).ok()?;
        match rx.recv().await? {
            Ok(_) => Some(Ok(())),
            // Deleted before a restart.
            Err(e) if e.starts_with("no pool") => Some(Ok(())),
            Err(e) => Some(Err((crate::VolumeFault::Invalid, e))),
        }
    }

    /// Follows a drain or a removal every half second until every moved PG
    /// is clean of `devices` and settled (`acting` = `up`); stops when the
    /// operation is cancelled, and fails it if the devices come back in.
    /// Then marks it done with `done`, or, without, returns true and leaves
    /// it running for the caller to finish.
    async fn follow_out(
        &self,
        op: &Op,
        devices: &[u32],
        pgs: &[(u32, u32)],
        what: &str,
        done: Option<String>,
    ) -> bool {
        // The drain is fenced and then published: until the published map
        // has the devices out, it's in progress, not undone.
        let mut seen_out = false;
        loop {
            // A removal: any later change to the map (a weight, a node
            // coming back) can leave a PG listing the device with nothing
            // to backfill; it is let go each time (D186).
            if done.is_none() {
                for &device in devices {
                    let (reply, rx) = oneshot();
                    if self.tx.send(Admin::Release { device, reply }).is_ok() {
                        let _ = rx.recv().await;
                    }
                }
            }
            let id = op.id.clone();
            let Ok(Some(now)) = self.store(move |s| s.get(&id)).await else {
                return false;
            };
            if !now.running() {
                return false;
            }
            let (reply, rx) = oneshot();
            if self.tx.send(Admin::Published(reply)).is_err() {
                return false;
            }
            let Some((map, ..)) = rx.recv().await else {
                return false;
            };
            let any_in = devices
                .iter()
                .any(|d| map.devices.get(d).is_some_and(|d| d.up_in));
            let back_in = seen_out && any_in;
            seen_out |= !any_in;
            // A deleted pool's PGs live nowhere.
            let left = pgs
                .iter()
                .filter(|k| map.pools.contains_key(&k.0))
                .filter(|k| {
                    map.pgs.get(k).is_none_or(|pg| {
                        pg.acting != pg.up || pg.acting.iter().any(|d| devices.contains(d))
                    })
                })
                .count() as u32;
            let id = op.id.clone();
            let (what, done) = (what.to_string(), done.clone());
            let r = self
                .store(move |s| {
                    s.update(&id, |o| {
                        if !o.running() {
                            return;
                        }
                        o.done = o.total - left.min(o.total);
                        if back_in {
                            o.state = State::Failed;
                            o.message = format!("{what}: brought back in");
                            o.finished_unix = now_unix();
                        } else if left == 0 && seen_out {
                            if let Some(done) = &done {
                                o.state = State::Done;
                                o.message = done.clone();
                                o.finished_unix = now_unix();
                            }
                        } else {
                            o.message = format!("{left} PG(s) still moving off {what}");
                        }
                    })
                })
                .await;
            match r {
                Ok(Some(o)) if o.running() => {
                    if left == 0 && seen_out {
                        return true;
                    }
                }
                // Finished, gone, or no longer the leader's to write.
                _ => return false,
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// While this member leads, once it has fenced the last leader:
    /// resumes running operations it doesn't
    /// follow (every 2 s), prunes finished ones past a week (hourly),
    /// purges the trash past each pool's grace (every `scan`), and the
    /// oldest trash of full pools set to purge it (D115; every 2 s).
    pub(super) async fn upkeep(self, scan: Duration) {
        let settings = self.settings.clone();
        let mut last_prune = std::time::Instant::now() - Duration::from_secs(3600);
        let mut last_scan = std::time::Instant::now();
        let mut purged_for_full = std::collections::BTreeMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(2).min(scan)).await;
            if !self.leads().await || self.fence().await.is_err() {
                continue;
            }
            if let Ok(all) = self.store(|s| s.list()).await {
                for o in all.into_iter().filter(Op::running) {
                    self.start(o, true);
                }
            }
            if last_prune.elapsed() >= Duration::from_secs(3600) {
                last_prune = std::time::Instant::now();
                let _ = self.store(|s| s.prune()).await;
            }
            if last_scan.elapsed() >= scan {
                last_scan = std::time::Instant::now();
                self.purge_expired(settings.as_ref()).await;
            }
            self.purge_for_full(settings.as_ref(), &mut purged_for_full)
                .await;
        }
    }

    /// The trash's purger (D54): a purge operation for each volume or
    /// snapshot past its pool's grace that has none running, and none that
    /// failed in the last hour (so a purge that can't finish doesn't add an
    /// operation every scan).
    async fn purge_expired(&self, settings: Option<&Arc<crate::settings::Settings>>) {
        let Some(items) = self.trash().await else {
            return;
        };
        let now = now_unix();
        let (running, failed) = self.purges(now).await;
        let busy: HashSet<String> = running.union(&failed).cloned().collect();
        for item in items {
            let grace = settings
                .and_then(|s| s.trash(item.pool()).ok())
                .unwrap_or_default()
                .grace_secs;
            if item.trashed_unix().saturating_add(grace) > now {
                continue;
            }
            let (volume, epoch) = purged(&item);
            let target = purge_target(volume, epoch);
            if busy.contains(&target) {
                continue;
            }
            let _ = self
                .create(
                    Work::Purge { volume, epoch },
                    target,
                    1,
                    format!("past its pool's grace ({grace} s in the trash)"),
                    "map-service/purger".into(),
                )
                .await;
        }
    }

    /// A full pool's trash (D115): in each pool past full whose trash is
    /// set to `purge`, a purge of its oldest item, one at a time per pool,
    /// skipping an item whose purge failed in the last hour. A pool set
    /// to `keep` (the default) keeps its trash and stays full. `busy` has
    /// when each pool last had a purge running: the next waits 10 s, two
    /// device reports, so the space a purge freed is counted first.
    async fn purge_for_full(
        &self,
        settings: Option<&Arc<crate::settings::Settings>>,
        busy: &mut std::collections::BTreeMap<u32, std::time::Instant>,
    ) {
        let Some(settings) = settings else {
            return;
        };
        let full: Vec<(u32, String)> = super::full_pools(&self.tx)
            .await
            .into_iter()
            .filter(|(p, _)| {
                settings
                    .trash(*p)
                    .is_ok_and(|t| t.on_full == crate::settings::OnFull::Purge)
            })
            .collect();
        if full.is_empty() {
            return;
        }
        let Some(items) = self.trash().await else {
            return;
        };
        let (running, failed) = self.purges(now_unix()).await;
        for (pool, why) in full {
            let mut held: Vec<_> = items
                .iter()
                .filter(|i| i.pool() == pool)
                .map(|i| (i.trashed_unix(), purged(i)))
                .collect();
            if held
                .iter()
                .any(|(_, (v, e))| running.contains(&purge_target(*v, *e)))
            {
                busy.insert(pool, std::time::Instant::now());
                continue;
            }
            if busy
                .get(&pool)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
            {
                continue;
            }
            held.sort();
            let Some((_, (volume, epoch))) = held
                .into_iter()
                .find(|(_, (v, e))| !failed.contains(&purge_target(*v, *e)))
            else {
                continue;
            };
            eprintln!(
                "hyperstor-meta: {why}; purging {} from its trash",
                purge_target(volume, epoch)
            );
            let _ = self
                .create(
                    Work::Purge { volume, epoch },
                    purge_target(volume, epoch),
                    1,
                    format!("{why}; its oldest in the trash, as trash_on_full = purge"),
                    "map-service/purger".into(),
                )
                .await;
            busy.insert(pool, std::time::Instant::now());
        }
    }

    /// Everything in the trash, of every pool.
    async fn trash(&self) -> Option<Vec<hyperstor_client::trash::Trashed>> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Trash {
                op: crate::TrashOp::List { pool: None },
                reply,
            })
            .ok()?;
        match rx.recv().await {
            Some(Ok(crate::TrashDone::Items(items))) => Some(items),
            _ => None,
        }
    }

    /// The targets of purges running, and of those failed in the hour
    /// before `now`.
    async fn purges(&self, now: u64) -> (HashSet<String>, HashSet<String>) {
        let (mut running, mut failed) = (HashSet::new(), HashSet::new());
        for o in self.store(|s| s.list()).await.unwrap_or_default() {
            if o.work.kind() != "purge" {
                continue;
            }
            if o.running() {
                running.insert(o.target);
            } else if o.state == State::Failed && o.finished_unix + 3600 > now {
                failed.insert(o.target);
            }
        }
        (running, failed)
    }
}

/// The volume and snapshot epoch a trashed item's purge names.
fn purged(item: &hyperstor_client::trash::Trashed) -> (u128, Option<u64>) {
    use hyperstor_client::trash::Trashed;
    match item {
        Trashed::Volume(v) => (v.id, None),
        Trashed::Snapshot(v, s) => (v.id, Some(s.epoch)),
    }
}

/// What a purge names: a volume, or one of its snapshots.
pub(super) fn purge_target(volume: u128, epoch: Option<u64>) -> String {
    match epoch {
        Some(e) => format!("volume {volume:032x} snapshot {e}"),
        None => format!("volume {volume:032x}"),
    }
}
