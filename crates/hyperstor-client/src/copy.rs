//! Copies and moves between pools (10-volumes.md §10.6, 04-decisions.md
//! D214).
//!
//! A copy reads a volume's head (the volume held meanwhile, so nothing
//! writes it) or one of its snapshots, and writes what it reads into a new
//! volume in another pool, held too, skipping holes and blocks of zeros. The
//! copy is flat: no snapshots, no parents. Pools may differ in chunk size:
//! data is read and written by offset.
//!
//! A move keeps the volume's id. Its head is copied into lineage `id` of
//! the new pool (free there: a lineage is named per pool), then its header
//! points there in one compare-and-set, and the data left in the old pool
//! is deleted. Before the switch, what is left behind is recorded in
//! `m/{id}` in `.meta`, so a move that stops after it is finished by
//! running it again. A move needs the volume detached, without snapshots,
//! and with nothing reading through it; it stays held until the old data
//! is gone.

use crate::volume::{
    Attached, Take, Volume, VolumeState, attr, cas, conflict, dir_key, header_key,
};
use crate::{BLOCK, Client, ClientError, NS_META, random_u64};
use hyperstor_codec::{Dec, Enc};
use hyperstor_map::META_POOL;
use hyperstor_proto::WireOp;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Bytes read and written at a time, and how many such steps are in flight.
const STEP: u64 = 4 << 20;
const IN_FLIGHT: u64 = 8;

const OLD: &[u8] = b"old";

/// How far a copy or move has got, and whether to stop: shared with
/// whoever follows it (the map service's operation, on another thread).
#[derive(Debug, Default)]
pub struct Progress {
    /// Bytes of the volume gone through.
    pub done: AtomicU64,
    /// The volume's size.
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    /// For tests: a move stops right after its switch, as if the map
    /// service had stopped there.
    #[doc(hidden)]
    pub stop_after_switch: AtomicBool,
}

/// Where copied blocks go.
#[derive(Clone)]
enum Dest {
    /// A new volume, held by the copy.
    Volume(Rc<Attached>),
    /// Lineage `lineage` of `pool`, at `epoch`, under lease `lease`.
    Raw {
        lease: u64,
        pool: u32,
        lineage: u128,
        epoch: u64,
    },
}

/// The data a move leaves in the old pool.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Left {
    to: u32,
    pool: u32,
    lineage: u128,
    size: u64,
    branches: Vec<u32>,
}

fn moving_key(id: u128) -> Vec<u8> {
    format!("m/{id:032x}").into_bytes()
}

fn encode_left(l: &Left) -> Vec<u8> {
    let mut e = Enc::new();
    e.u32(l.to).u32(l.pool).u128(l.lineage).u64(l.size);
    e.seq(&l.branches, |e, b| {
        e.u32(*b);
    });
    e.into_vec()
}

fn decode_left(b: &[u8]) -> Option<Left> {
    let mut d = Dec::new(b);
    Some(Left {
        to: d.u32().ok()?,
        pool: d.u32().ok()?,
        lineage: d.u128().ok()?,
        size: d.u64().ok()?,
        branches: d.seq(4, |d| d.u32()).ok()?,
    })
}

fn cancelled(p: &Progress) -> Result<(), ClientError> {
    if p.cancel.load(Ordering::Relaxed) {
        return Err(ClientError::Refused("cancelled".into()));
    }
    Ok(())
}

impl Client {
    /// Takes volume `id`'s lease for a copy or a move: from a holder
    /// that has stopped renewing too, such as a former leader that was
    /// running the same operation (it is blocklisted, D125); a holder
    /// that renews keeps it.
    async fn hold(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.attach_as(id, Take::TakeOver, &self.lease_label())
            .await
    }

    /// Copies volume `id` into a new volume `new_id` named `name` in pool
    /// `pool`: its snapshot at `epoch`, or its head, which needs it
    /// detached (it is held meanwhile). The new volume is held while it
    /// fills, and deleted if the copy fails. Running it again after it
    /// stopped starts over, into the same new volume.
    pub async fn copy_volume(
        self: &Rc<Self>,
        id: u128,
        epoch: Option<u64>,
        pool: u32,
        new_id: u128,
        name: &str,
        p: &Progress,
    ) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.state != VolumeState::Active {
            return Err(ClientError::NoVolume(id));
        }
        let held = match epoch {
            Some(_) => None,
            None => Some(self.hold(id).await.map_err(|e| attached(id, e))?),
        };
        let r = self.copy_held(id, epoch, pool, new_id, name, p).await;
        if let Some(h) = held {
            let _ = h.detach().await;
        }
        r
    }

    async fn copy_held(
        self: &Rc<Self>,
        id: u128,
        epoch: Option<u64>,
        pool: u32,
        new_id: u128,
        name: &str,
        p: &Progress,
    ) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        let layers = match epoch {
            Some(e) => {
                let s = v
                    .snapshot(e)
                    .filter(|s| !s.deleting && s.trashed_unix == 0)
                    .ok_or(ClientError::NoSnapshot(e))?;
                v.snapshot_layers(s)
            }
            None => v.layers(),
        };
        // A copy run again finds its volume: emptied, then filled again.
        match self.volume(new_id).await? {
            Some(n) if n.pool == pool && n.size == v.size && n.lineage == new_id => {
                self.drop_data(pool, new_id, n.size, &[0]).await?;
            }
            Some(_) => return Err(ClientError::VolumeExists(new_id)),
            None => {
                self.create_volume(pool, v.size, Some(new_id), name).await?;
            }
        }
        let r = async {
            let to = self.hold(new_id).await?;
            let r = self
                .copy_blocks(&v, &layers, &Dest::Volume(to.clone()), p)
                .await;
            let _ = to.detach().await;
            r?;
            // A snapshot deleted meanwhile may have been merged under the
            // copy: refused, as if it had gone before.
            if let Some(e) = epoch {
                let now = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
                now.snapshot(e)
                    .filter(|s| !s.deleting && s.trashed_unix == 0)
                    .ok_or(ClientError::NoSnapshot(e))?;
            }
            self.volume(new_id)
                .await?
                .ok_or(ClientError::NoVolume(new_id))
        }
        .await;
        if r.is_err() {
            let _ = self.delete_volume(new_id).await;
        }
        r
    }

    /// Moves volume `id` to pool `pool`, keeping its id. Refused while it
    /// is attached, has snapshots (in the trash too), or another volume
    /// reads through it; a clone is flattened by the move. Running it
    /// again after it stopped finishes it, or starts over if the switch
    /// wasn't made.
    pub async fn move_volume(
        self: &Rc<Self>,
        id: u128,
        pool: u32,
        p: &Progress,
    ) -> Result<Volume, ClientError> {
        let mut v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.state != VolumeState::Active {
            return Err(ClientError::NoVolume(id));
        }
        if !self.map().pools.contains_key(&pool) || pool == META_POOL {
            return Err(ClientError::NoPool(pool));
        }
        let left = self.left(id).await?;
        if v.pool == pool && left.is_none() {
            return Ok(v);
        }
        let held = self.hold(id).await.map_err(|e| attached(id, e))?;
        let r = async {
            match left {
                // Switched before it stopped: what is left goes.
                Some(l) if v.pool == l.to => {}
                // Not switched: as if it hadn't started.
                Some(_) => {
                    self.exec(META_POOL, NS_META, &moving_key(id), &[WireOp::Delete])
                        .await?;
                    v = self.switch_to(&held, pool, p).await?;
                }
                None => v = self.switch_to(&held, pool, p).await?,
            }
            if p.stop_after_switch.load(Ordering::Relaxed) {
                return Err(ClientError::Refused("stopped after the switch".into()));
            }
            self.finish_move(id).await?;
            self.volume(id).await?.ok_or(ClientError::NoVolume(id))
        }
        .await;
        let _ = held.detach().await;
        r
    }

    /// Copies held volume `held` into lineage `id` of `pool`, records what
    /// is left behind and switches the header over.
    async fn switch_to(
        self: &Rc<Self>,
        held: &Rc<Attached>,
        pool: u32,
        p: &Progress,
    ) -> Result<Volume, ClientError> {
        let v = self.volume(held.volume().id).await?;
        let v = v.ok_or(ClientError::NoVolume(held.volume().id))?;
        let id = v.id;
        if !v.snapshots.is_empty() {
            return Err(ClientError::Refused(format!(
                "volume {id:032x} has snapshots: delete and purge them first"
            )));
        }
        let (own, branches) = self.own_branches(&v).await?;
        for b in branches.iter().filter(|b| b.volume != id) {
            if let Some((_, e)) = b.chain.iter().find(|(x, _)| own.contains(x))
                && self.branch_reads(b).await?
            {
                return Err(ClientError::SnapshotInUse(*e));
            }
        }
        // What an earlier attempt may have left there goes first: blocks
        // of zeros aren't written.
        self.drop_data(pool, id, v.size, &[0]).await?;
        let dest = Dest::Raw {
            lease: held.lease(),
            pool,
            lineage: id,
            epoch: v.head_epoch,
        };
        let r = self.copy_blocks(&v, &v.layers(), &dest, p).await;
        if let Err(e) = r {
            let _ = self.drop_data(pool, id, v.size, &[0]).await;
            return Err(e);
        }
        let left = Left {
            to: pool,
            pool: v.pool,
            lineage: v.lineage,
            size: v.size,
            branches: own,
        };
        self.exec(
            META_POOL,
            NS_META,
            &moving_key(id),
            &[
                WireOp::Touch,
                WireOp::SetAttr(OLD.to_vec(), encode_left(&left)),
            ],
        )
        .await?;
        let new = Volume {
            pool,
            lineage: id,
            branch: 0,
            chain: Vec::new(),
            skip: Vec::new(),
            incarnation: random_u64() | 1,
            version: v.version + 1,
            ..v.clone()
        };
        // Held by this lease, unchanged since it was read: the copy is
        // what it holds.
        let mut ops = cas(v.version, &new);
        ops.insert(
            0,
            WireOp::AssertAttr {
                name: crate::volume::LEASE_ATTR.to_vec(),
                value: Some(held.holder_attr()),
            },
        );
        match self.exec(META_POOL, NS_META, &header_key(id), &ops).await {
            Ok(()) => Ok(new),
            Err(e) => {
                let _ = self
                    .exec(META_POOL, NS_META, &moving_key(id), &[WireOp::Delete])
                    .await;
                let _ = self.drop_data(pool, id, v.size, &[0]).await;
                if conflict(&e) {
                    return Err(ClientError::Refused(format!(
                        "volume {id:032x} changed during the move"
                    )));
                }
                Err(e)
            }
        }
    }

    /// After the switch: lists the volume in its new pool's directory,
    /// takes it from the old one's, deletes the data left there and the
    /// branch records it owned, then the record of the move.
    async fn finish_move(self: &Rc<Self>, id: u128) -> Result<(), ClientError> {
        let Some(l) = self.left(id).await? else {
            return Ok(());
        };
        self.exec(
            META_POOL,
            NS_META,
            &dir_key(l.to, id),
            &[
                WireOp::Touch,
                WireOp::KvPut(id.to_be_bytes().to_vec(), Vec::new()),
            ],
        )
        .await?;
        self.exec(
            META_POOL,
            NS_META,
            &dir_key(l.pool, id),
            &[WireOp::Touch, WireOp::KvDel(id.to_be_bytes().to_vec())],
        )
        .await?;
        self.drop_data(l.pool, l.lineage, l.size, &l.branches)
            .await?;
        for b in l.branches.iter().filter(|b| **b != 0) {
            self.forget_branch(l.lineage, *b).await?;
        }
        self.exec(META_POOL, NS_META, &moving_key(id), &[WireOp::Delete])
            .await
    }

    /// What a move of `id` recorded it leaves behind, if one is under way.
    async fn left(&self, id: u128) -> Result<Option<Left>, ClientError> {
        let l = self
            .lookup(META_POOL, NS_META, &moving_key(id), b"", None, 0)
            .await?;
        Ok(l.and_then(|l| attr(&l, OLD).and_then(decode_left)))
    }

    /// Deletes every version of `branches` of lineage `lineage` in `pool`,
    /// over `size` bytes, a few chunks at a time.
    async fn drop_data(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        size: u64,
        branches: &[u32],
    ) -> Result<(), ClientError> {
        let chunk = u64::from(
            self.map()
                .pools
                .get(&pool)
                .ok_or(ClientError::NoPool(pool))?
                .chunk_size,
        );
        let chunks = size.div_ceil(chunk);
        for b in branches {
            let mut next = 0;
            while next < chunks {
                let batch: Vec<_> = (next..(next + 32).min(chunks))
                    .map(|c| {
                        let (me, b) = (self.clone(), *b);
                        hyperstor_reactor::spawn(async move {
                            me.drop_versions(pool, lineage, c, b, 0, u64::MAX).await
                        })
                    })
                    .collect();
                for x in batch {
                    x.await?;
                }
                next += 32;
            }
        }
        Ok(())
    }

    /// Reads `v` through `layers` and writes what isn't a hole or zeros
    /// to `dest`, [`STEP`] bytes at a time, [`IN_FLIGHT`] steps at once.
    async fn copy_blocks(
        self: &Rc<Self>,
        v: &Volume,
        layers: &[(u32, u64, u64)],
        dest: &Dest,
        p: &Progress,
    ) -> Result<(), ClientError> {
        p.total.store(v.size, Ordering::Relaxed);
        p.done.store(0, Ordering::Relaxed);
        let layers: Rc<Vec<(u32, u64, u64)>> = Rc::new(layers.to_vec());
        let dest = Rc::new(dest.clone());
        let mut off = 0;
        while off < v.size {
            cancelled(p)?;
            let batch: Vec<_> = (0..IN_FLIGHT)
                .map(|i| off + i * STEP)
                .take_while(|o| *o < v.size)
                .map(|o| {
                    let len = STEP.min(v.size - o);
                    let (me, layers, dest) = (self.clone(), layers.clone(), dest.clone());
                    let (pool, lineage) = (v.pool, v.lineage);
                    hyperstor_reactor::spawn(async move {
                        let (data, sources) =
                            me.read_sourced(pool, lineage, &layers, o, len).await?;
                        me.write_runs(&dest, o, &data, &sources).await
                    })
                })
                .collect();
            let n = batch.len() as u64;
            for b in batch {
                b.await?;
            }
            off = (off + n * STEP).min(v.size);
            p.done.store(off, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Writes each run of blocks of `data` (read at `off`) that holds
    /// something: not a hole, not zeros.
    async fn write_runs(
        self: &Rc<Self>,
        dest: &Dest,
        off: u64,
        data: &[u8],
        sources: &[(u32, u64)],
    ) -> Result<(), ClientError> {
        let blocks = data.len() / BLOCK as usize;
        let full = |i: usize| {
            sources.get(i).is_none_or(|s| s.0 != u32::MAX)
                && data[i * BLOCK as usize..(i + 1) * BLOCK as usize]
                    .iter()
                    .any(|b| *b != 0)
        };
        let mut i = 0;
        while i < blocks {
            if !full(i) {
                i += 1;
                continue;
            }
            let start = i;
            while i < blocks && full(i) {
                i += 1;
            }
            let at = off + (start as u64) * BLOCK;
            let run = data[start * BLOCK as usize..i * BLOCK as usize].to_vec();
            match dest {
                Dest::Volume(a) => a.write(at, run).await?,
                Dest::Raw {
                    lease,
                    pool,
                    lineage,
                    epoch,
                } => {
                    self.write_under(*lease, *pool, *lineage, 0, *epoch, at, run)
                        .await?
                }
            }
        }
        Ok(())
    }
}

/// A volume someone holds, as the API names it.
fn attached(id: u128, e: ClientError) -> ClientError {
    match e {
        ClientError::LeaseHeld { .. } => ClientError::VolumeAttached(id),
        e => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_move_leaves_round_trips() {
        let l = Left {
            to: 3,
            pool: 2,
            lineage: 1 << 100,
            size: 1 << 30,
            branches: vec![0, 4, 7],
        };
        assert_eq!(decode_left(&encode_left(&l)), Some(l));
        assert_eq!(decode_left(b"short"), None);
    }
}
