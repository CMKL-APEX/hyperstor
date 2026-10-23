//! Running long operations (decisions.md D120, D121): each kind in a task
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
                        instance: last,
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
        let op = Op {
            id: super::admin::random_hex(8),
            work,
            target,
            state: State::Running,
            done: 0,
            total,
            message,
            started_unix: now_unix(),
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
                    me.drain(&op, node, &devices, &pgs).await
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

    /// Follows a drain every half second until every moved PG is clean of
    /// the node's devices and settled (`acting` = `up`); stops when the
    /// operation is cancelled, and fails if the devices come back in.
    async fn drain(&self, op: &Op, node: u32, devices: &[u32], pgs: &[(u32, u32)]) {
        // The drain is fenced and then published: until the published map
        // has the devices out, it's in progress, not undone.
        let mut seen_out = false;
        loop {
            let id = op.id.clone();
            let Ok(Some(now)) = self.store(move |s| s.get(&id)).await else {
                return;
            };
            if !now.running() {
                return;
            }
            let (reply, rx) = oneshot();
            if self.tx.send(Admin::Published(reply)).is_err() {
                return;
            }
            let Some((map, ..)) = rx.recv().await else {
                return;
            };
            let any_in = map.devices.values().any(|d| d.node == node && d.up_in);
            let back_in = seen_out && any_in;
            seen_out |= !any_in;
            let left = pgs
                .iter()
                .filter(|k| {
                    map.pgs.get(k).is_none_or(|pg| {
                        pg.acting != pg.up || pg.acting.iter().any(|d| devices.contains(d))
                    })
                })
                .count() as u32;
            let id = op.id.clone();
            let r = self
                .store(move |s| {
                    s.update(&id, |o| {
                        if !o.running() {
                            return;
                        }
                        o.done = o.total - left.min(o.total);
                        if back_in {
                            o.state = State::Failed;
                            o.message = format!("node {node}'s devices were brought back in");
                            o.finished_unix = now_unix();
                        } else if left == 0 && seen_out {
                            o.state = State::Done;
                            o.message = format!("node {node} drained: no PG lives on it");
                            o.finished_unix = now_unix();
                        } else {
                            o.message = format!("{left} PG(s) still moving off node {node}");
                        }
                    })
                })
                .await;
            match r {
                Ok(Some(o)) if o.running() => {}
                // Finished, gone, or no longer the leader's to write.
                _ => return,
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// While this member leads, once it has fenced the last leader:
    /// resumes running operations it doesn't
    /// follow (every 2 s), prunes finished ones past a week (hourly), and
    /// purges the trash past each pool's grace (every `scan`).
    pub(super) async fn upkeep(self, scan: Duration) {
        let settings = self.settings.clone();
        let mut last_prune = std::time::Instant::now() - Duration::from_secs(3600);
        let mut last_scan = std::time::Instant::now();
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
        }
    }

    /// The trash's purger (D54): a purge operation for each volume or
    /// snapshot past its pool's grace that has none running, and none that
    /// failed in the last hour (so a purge that can't finish doesn't add an
    /// operation every scan).
    async fn purge_expired(&self, settings: Option<&Arc<crate::settings::Settings>>) {
        use hyperstor_client::trash::Trashed;
        let (reply, rx) = oneshot();
        if self
            .tx
            .send(Admin::Trash {
                op: crate::TrashOp::List { pool: None },
                reply,
            })
            .is_err()
        {
            return;
        }
        let Some(Ok(crate::TrashDone::Items(items))) = rx.recv().await else {
            return;
        };
        let now = now_unix();
        let busy: HashSet<String> = self
            .store(|s| s.list())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|o| {
                o.work.kind() == "purge"
                    && (o.running() || (o.state == State::Failed && o.finished_unix + 3600 > now))
            })
            .map(|o| o.target)
            .collect();
        for item in items {
            let grace = settings
                .and_then(|s| s.trash(item.pool()).ok())
                .unwrap_or_default()
                .grace_secs;
            if item.trashed_unix().saturating_add(grace) > now {
                continue;
            }
            let (volume, epoch) = match &item {
                Trashed::Volume(v) => (v.id, None),
                Trashed::Snapshot(v, s) => (v.id, Some(s.epoch)),
            };
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
}

/// What a purge names: a volume, or one of its snapshots.
pub(super) fn purge_target(volume: u128, epoch: Option<u64>) -> String {
    match epoch {
        Some(e) => format!("volume {volume:032x} snapshot {e}"),
        None => format!("volume {volume:032x}"),
    }
}
