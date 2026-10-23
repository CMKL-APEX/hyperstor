//! The trash (administration.md §23.9.3, decisions.md D54): a deleted
//! volume or snapshot keeps its data until it is purged, after a pool's
//! grace period or at once, or restored.
//!
//! - **A trashed volume** is a header in state [`VolumeState::Trashed`]
//!   with the time it went: still in its pool's directory (so the purger
//!   finds it), not listed, not attachable (storage nodes grant no lease on
//!   a header that isn't active), no snapshots taken of it. Trashing is
//!   refused as deleting is: while it is attached (unless forced), or while
//!   another volume reads one of its branches; so a purge can always go on.
//! - **A trashed snapshot** has its time set: not listed, not cloned from
//!   or rolled back to. It is marked first and then checked for
//!   dependents, as a delete is, so a clone that recorded itself meanwhile
//!   is seen.
//! - **Purging** is the delete of before: the volume delete that resumes
//!   after a crash, [`Client::delete_snapshot`] for a snapshot; a group's object
//!   goes with its last snapshot.
//! - **Restoring** clears the mark; a volume whose name another volume of
//!   the pool took meanwhile comes back as `NAME-restored`.

use crate::volume::{Snapshot, Volume, VolumeState, cas, conflict, header_key};
use crate::{Client, ClientError, NS_META};
use hyperstor_map::META_POOL;
use hyperstor_proto::{LEASE_RW, WireOp};
use std::rc::Rc;

/// Now, in Unix seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Something in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trashed {
    Volume(Volume),
    /// A snapshot, and the volume it belongs to.
    Snapshot(Volume, Snapshot),
}

impl Trashed {
    pub fn trashed_unix(&self) -> u64 {
        match self {
            Trashed::Volume(v) => v.trashed_unix,
            Trashed::Snapshot(_, s) => s.trashed_unix,
        }
    }

    pub fn pool(&self) -> u32 {
        match self {
            Trashed::Volume(v) | Trashed::Snapshot(v, _) => v.pool,
        }
    }
}

/// What's in the trash among `volumes`: trashed volumes (with their
/// snapshots), and trashed snapshots of the others.
pub fn trash_of(volumes: &[Volume]) -> Vec<Trashed> {
    let mut out = Vec::new();
    for v in volumes {
        match v.state {
            VolumeState::Trashed => out.push(Trashed::Volume(v.clone())),
            VolumeState::Active => out.extend(
                v.snapshots
                    .iter()
                    .filter(|s| s.trashed_unix != 0)
                    .map(|s| Trashed::Snapshot(v.clone(), s.clone())),
            ),
            VolumeState::Deleting => {}
        }
    }
    out
}

impl Client {
    /// Moves volume `id` to the trash (D54). Refused while attached, unless
    /// `force` (D80: the holder is preempted, so blocklisted, first).
    pub async fn trash_volume(
        self: &Rc<Self>,
        id: u128,
        force: bool,
    ) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.state != VolumeState::Active {
            return Err(ClientError::NoVolume(id));
        }
        let held = if force && v.holder.is_some() {
            Some(self.preempt(id).await?)
        } else {
            None
        };
        let me = held.as_ref().map(|_| self.instance);
        let r = self.trash_held(id, me).await;
        if let Some(h) = held {
            // Renewals stop: the header is no longer active.
            h.lost.set(true);
            self.attached.borrow_mut().remove(&id);
        }
        r
    }

    async fn trash_held(self: &Rc<Self>, id: u128, me: Option<u64>) -> Result<Volume, ClientError> {
        for _ in 0..10 {
            let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
            if v.state != VolumeState::Active {
                return Err(ClientError::NoVolume(id));
            }
            if v.holder.is_some() && v.holder != me {
                return Err(ClientError::VolumeAttached(id));
            }
            // As a delete: not while another volume's branch reads one of
            // its branches, so the purge can always go on.
            let (own, branches) = self.own_branches(&v).await?;
            for b in branches.iter().filter(|b| b.volume != id) {
                if let Some((_, e)) = b.chain.iter().find(|(x, _)| own.contains(x))
                    && self.branch_reads(b).await?
                {
                    return Err(ClientError::SnapshotInUse(*e));
                }
            }
            let new = Volume {
                state: VolumeState::Trashed,
                trashed_unix: now_unix(),
                version: v.version + 1,
                ..v.clone()
            };
            // Decided by the header's primary, as leases are: not while
            // someone else holds it.
            let mut ops = cas(v.version, &new);
            ops.insert(
                0,
                WireOp::AssertAttr {
                    name: crate::volume::LEASE_ATTR.to_vec(),
                    value: me.map(|i| crate::volume::encode_holder(i, LEASE_RW)),
                },
            );
            match self.exec(META_POOL, NS_META, &header_key(id), &ops).await {
                Ok(()) => return Ok(new),
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ClientError::BadVolume(format!(
            "volume {id:032x} kept changing"
        )))
    }

    /// Brings volume `id` back from the trash, as `name` if given, else
    /// under its own name, or `NAME-restored` if another volume of its
    /// pool took the name meanwhile.
    pub async fn restore_volume(
        self: &Rc<Self>,
        id: u128,
        name: Option<&str>,
    ) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.state != VolumeState::Trashed {
            return Err(ClientError::Refused(format!(
                "volume {id:032x} isn't in the trash"
            )));
        }
        let name = match name {
            Some(n) => n.to_string(),
            None => {
                let taken = self
                    .list_volumes(v.pool)
                    .await?
                    .iter()
                    .any(|o| o.id != id && o.state == VolumeState::Active && o.name == v.name);
                if taken && !v.name.is_empty() {
                    format!("{}-restored", v.name)
                } else {
                    v.name.clone()
                }
            }
        };
        self.change_volume(id, |v| {
            if v.state != VolumeState::Trashed {
                return Err(ClientError::Refused(format!(
                    "volume {id:032x} isn't in the trash"
                )));
            }
            Ok(Volume {
                state: VolumeState::Active,
                trashed_unix: 0,
                name: name.clone(),
                ..v.clone()
            })
        })
        .await
    }

    /// Deletes trashed volume `id` and its data for good (or finishes a
    /// delete that stopped half-way).
    pub async fn purge_volume(self: &Rc<Self>, id: u128) -> Result<(), ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.state == VolumeState::Active {
            return Err(ClientError::Refused(format!(
                "volume {id:032x} isn't in the trash"
            )));
        }
        self.delete_held(v, None).await
    }

    /// Moves the snapshot at `epoch` of volume `id` to the trash. Refused
    /// while a clone or rollback depends on it, as a delete is.
    pub async fn trash_snapshot(
        self: &Rc<Self>,
        id: u128,
        epoch: u64,
    ) -> Result<Snapshot, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        let s = v
            .snapshot(epoch)
            .filter(|s| !s.deleting && s.trashed_unix == 0)
            .cloned()
            .ok_or(ClientError::NoSnapshot(epoch))?;
        // Marked first, then dependents looked for: a clone records itself
        // before it checks the snapshot, so one of the two sees the other.
        let t = now_unix();
        let v = self
            .change_volume(id, |v| {
                let mut n = v.clone();
                let x = n
                    .snapshots
                    .iter_mut()
                    .find(|x| x.epoch == epoch && !x.deleting && x.trashed_unix == 0)
                    .ok_or(ClientError::NoSnapshot(epoch))?;
                x.trashed_unix = t;
                Ok(n)
            })
            .await?;
        if self.snapshot_used(&v, &s).await? {
            self.restore_snapshot(id, epoch).await?;
            return Err(ClientError::SnapshotInUse(epoch));
        }
        Ok(Snapshot {
            trashed_unix: t,
            ..s
        })
    }

    /// Brings the snapshot at `epoch` of volume `id` back from the trash.
    pub async fn restore_snapshot(
        self: &Rc<Self>,
        id: u128,
        epoch: u64,
    ) -> Result<Snapshot, ClientError> {
        let v = self
            .change_volume(id, |v| {
                let mut n = v.clone();
                let x = n
                    .snapshots
                    .iter_mut()
                    .find(|x| x.epoch == epoch && x.trashed_unix != 0)
                    .ok_or_else(|| {
                        ClientError::Refused(format!("no snapshot at epoch {epoch} in the trash"))
                    })?;
                x.trashed_unix = 0;
                Ok(n)
            })
            .await?;
        v.snapshot(epoch)
            .cloned()
            .ok_or(ClientError::NoSnapshot(epoch))
    }

    /// Deletes the trashed snapshot at `epoch` of volume `id` for good; a
    /// consistency group's object goes with its last snapshot.
    pub async fn purge_snapshot(self: &Rc<Self>, id: u128, epoch: u64) -> Result<(), ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        let s = v
            .snapshot(epoch)
            .cloned()
            .ok_or(ClientError::NoSnapshot(epoch))?;
        if s.trashed_unix == 0 && !s.deleting {
            return Err(ClientError::Refused(format!(
                "the snapshot at epoch {epoch} isn't in the trash"
            )));
        }
        self.delete_snapshot(id, epoch).await?;
        if s.group != 0 {
            self.forget_group_if_empty(s.group).await?;
        }
        Ok(())
    }

    /// Moves every snapshot of consistency group `gid` to the trash.
    pub async fn trash_group(
        self: &Rc<Self>,
        gid: u128,
    ) -> Result<Vec<(u128, Snapshot)>, ClientError> {
        let mut out = Vec::new();
        for id in self.group_members(gid).await? {
            let Some(v) = self.volume(id).await? else {
                continue;
            };
            for s in v
                .snapshots
                .iter()
                .filter(|s| s.group == gid && s.trashed_unix == 0)
            {
                out.push((id, self.trash_snapshot(id, s.epoch).await?));
            }
        }
        Ok(out)
    }

    /// A compare-and-set of volume `id`'s header with `f`'s change.
    async fn change_volume(
        &self,
        id: u128,
        mut f: impl FnMut(&Volume) -> Result<Volume, ClientError>,
    ) -> Result<Volume, ClientError> {
        for _ in 0..10 {
            let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
            let mut new = f(&v)?;
            new.version = v.version + 1;
            match self
                .exec(META_POOL, NS_META, &header_key(id), &cas(v.version, &new))
                .await
            {
                Ok(()) => return Ok(new),
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ClientError::BadVolume(format!(
            "volume {id:032x} kept changing"
        )))
    }
}
