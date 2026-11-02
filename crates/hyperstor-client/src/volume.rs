//! Volumes (06-metadata.md §4.2, 04-decisions.md D77): a header object per volume
//! and directory objects per pool, in the `.meta` pool, changed by
//! compare-and-set on the header's version (01-implementation-notes.md I68).
//!
//! - **Header** `h/{id}` (32 hex digits): attributes `v` (the version, a
//!   decimal number), `hdr` (the encoded [`Volume`]), `by` (who created
//!   it: a token that recognises a retried create) and, while the volume
//!   is attached, `lease` (its holder, kept by the header's primary:
//!   06-metadata.md §9).
//! - **Directory** `d/{pool}/{shard}`: the pool's volumes in 64 shards by
//!   id, one key–value entry per volume (its id, big-endian).
//!
//! A volume is identified by a 128-bit id, chosen by the caller (glidex:
//! its disk's UUID) or drawn at random; its name is shown, not unique.

use crate::{BLOCK, Client, ClientError, NS_META, Route, cache, random_u64};
use hyperstor_codec::{Dec, Enc};
use hyperstor_map::META_POOL;
use hyperstor_proto::{
    Code, LEASE_EXPIRES, LEASE_LOST, LEASE_RENEW_EVERY, LEASE_RENEWED, LEASE_RW, Looked, Msg,
    WATCH_QUIESCE, WireOp,
};
use hyperstor_reactor::time::sleep;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Directory shards per pool.
pub const DIR_SHARDS: u128 = 64;

const VER: &[u8] = b"v";
const HDR: &[u8] = b"hdr";
const BY: &[u8] = b"by";
/// The lease holder's attribute: [`encode_holder`].
pub const LEASE_ATTR: &[u8] = b"lease";
/// The lease's delegates (D33, D151): [`encode_delegates`]. Kept apart
/// from [`LEASE_ATTR`], which stays the same for the lease's life, so a
/// holder can assert it holds the lease whatever its delegates.
pub const DELEGATES_ATTR: &[u8] = b"lease.delegates";

/// A volume's lease holder, as its header records it (06-metadata.md §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The lease's identity (D150), which every request under the lease
    /// carries.
    pub lease: u64,
    /// The client instance it was taken by.
    pub instance: u64,
    pub mode: u8,
    /// Who holds it, as the holder said (D153).
    pub label: String,
}

/// The `lease` attribute: the lease's identity, the holder's instance,
/// the mode and the label.
pub fn encode_holder(h: &Holder) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(h.lease).u64(h.instance).u8(h.mode).str(&h.label);
    e.into_vec()
}

/// A `lease` attribute; one written before identities (D150), an
/// instance and a mode, reads as a lease whose identity is the instance.
pub fn decode_holder(b: &[u8]) -> Option<Holder> {
    let mut d = Dec::new(b);
    let h = if b.len() == 9 {
        let i = d.u64().ok()?;
        Holder {
            lease: i,
            instance: i,
            mode: d.u8().ok()?,
            label: String::new(),
        }
    } else {
        Holder {
            lease: d.u64().ok()?,
            instance: d.u64().ok()?,
            mode: d.u8().ok()?,
            label: d.str().ok()?.to_string(),
        }
    };
    d.finish().ok()?;
    Some(h)
}

pub fn encode_delegates(delegates: &[u64]) -> Vec<u8> {
    let mut e = Enc::new();
    e.seq(delegates, |e, d| {
        e.u64(*d);
    });
    e.into_vec()
}

pub fn decode_delegates(b: &[u8]) -> Vec<u64> {
    let mut d = Dec::new(b);
    d.seq(8, |d| d.u64())
        .ok()
        .filter(|_| d.finish().is_ok())
        .unwrap_or_default()
}

/// A header's version, from its attributes (0: none).
pub fn header_version(attrs: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>) -> u64 {
    attrs
        .get(VER)
        .and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())
        .unwrap_or(0)
}

/// Whether a header says its volume is being deleted.
pub fn header_deleting(attrs: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>) -> bool {
    let Some(h) = attrs.get(HDR) else {
        return false;
    };
    // Format, pool, size, lineage, then the state.
    h.len() > 1 + 4 + 8 + 16 && (h[0] == 1 || h[0] == 2) && h[1 + 4 + 8 + 16] != 0
}

/// The name of volume `id`'s header object.
pub fn header_key(id: u128) -> Vec<u8> {
    format!("h/{id:032x}").into_bytes()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeState {
    Active,
    /// Being deleted: its data is going, and it can't be attached.
    Deleting,
    /// In the trash (04-decisions.md D54): its data stays until it is purged
    /// or restored; it isn't listed and can't be attached (storage nodes
    /// grant no lease on a header that isn't active).
    Trashed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub id: u128,
    /// The data pool.
    pub pool: u32,
    pub size: u64,
    /// Where its chunks are placed (04-decisions.md D21): its own id, or its
    /// parent's lineage for a clone.
    pub lineage: u128,
    pub state: VolumeState,
    pub name: String,
    /// The header's version: every change bumps it.
    pub version: u64,
    /// The identity of the lease it is attached under (D150): the
    /// holder's.
    pub holder: Option<u64>,
    /// The identities that act under the holder's lease (D33, D151).
    pub delegates: Vec<u64>,
    /// The holder's label (D153).
    pub holder_label: Option<String>,
    /// The epoch writes go to now (D10): each snapshot takes the epoch it
    /// was, and moves this on.
    pub head_epoch: u64,
    /// Its branch in the lineage (D21): 0 for a volume created as such, a
    /// new one for a clone or after a rollback.
    pub branch: u32,
    /// The branches it reads through, parent first: each (branch, epoch)
    /// a clone or rollback was cut from (08-engine.md §7.2).
    pub chain: Vec<(u32, u64)>,
    pub snapshots: Vec<Snapshot>,
    /// Epochs of its own branch a reset in place dropped (D106), both ends
    /// included: reads skip them until their versions are deleted.
    pub skip: Vec<(u64, u64)>,
    /// A random value of its lineage, shared by its clones, for the read
    /// cache (D102): a volume recreated with the same id has a new one.
    /// 0, in a header written before it, turns the cache off.
    pub incarnation: u64,
    /// When it went to the trash (Unix seconds), while [`VolumeState::Trashed`].
    pub trashed_unix: u64,
}

/// A snapshot of a volume (10-volumes.md §10.3): the volume as it was at
/// `epoch` of `branch` (its branch when taken).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub branch: u32,
    pub epoch: u64,
    pub name: String,
    /// The consistency group it was taken in; 0 if none.
    pub group: u128,
    /// Being deleted: its versions are being merged.
    pub deleting: bool,
    /// The chain below its branch when it was taken: what it reads through
    /// after its own branch.
    pub chain: Vec<(u32, u64)>,
    /// When it went to the trash (Unix seconds); 0: it isn't there.
    pub trashed_unix: u64,
}

impl Volume {
    /// The layers a read at the head goes through, as (branch, oldest,
    /// newest epoch): its own branch up to the head epoch, less what a
    /// reset skips, then its chain.
    pub fn layers(&self) -> Vec<(u32, u64, u64)> {
        let mut out = Vec::new();
        let mut skips: Vec<(u64, u64)> = self
            .skip
            .iter()
            .copied()
            .filter(|(lo, hi)| lo <= hi)
            .collect();
        skips.sort_unstable_by(|a, b| b.cmp(a));
        let mut hi = Some(self.head_epoch);
        for (lo, top) in skips {
            let Some(h) = hi else { break };
            if top < h {
                out.push((self.branch, top + 1, h));
            }
            hi = lo.checked_sub(1);
        }
        if let Some(h) = hi {
            out.push((self.branch, 0, h));
        }
        out.extend(self.chain.iter().map(|(b, e)| (*b, 0, *e)));
        out
    }

    /// The layers a read of snapshot `s` goes through.
    pub fn snapshot_layers(&self, s: &Snapshot) -> Vec<(u32, u64, u64)> {
        std::iter::once((s.branch, 0, s.epoch))
            .chain(s.chain.iter().map(|(b, e)| (*b, 0, *e)))
            .collect()
    }

    pub fn snapshot(&self, epoch: u64) -> Option<&Snapshot> {
        self.snapshots.iter().find(|s| s.epoch == epoch)
    }
}

pub(crate) fn dir_key(pool: u32, id: u128) -> Vec<u8> {
    format!("d/{pool}/{:02}", id % DIR_SHARDS).into_bytes()
}

/// A header: format 2 adds epochs, branches and snapshots (HS4); a format
/// 1 header reads as branch 0 at epoch 0, with none.
fn encode(v: &Volume) -> Vec<u8> {
    let mut e = Enc::new();
    e.u8(2)
        .u32(v.pool)
        .u64(v.size)
        .u128(v.lineage)
        .u8(match v.state {
            VolumeState::Active => 0,
            VolumeState::Deleting => 1,
            VolumeState::Trashed => 2,
        })
        .str(&v.name);
    e.u64(v.head_epoch).u32(v.branch);
    e.seq(&v.chain, |e, (b, ep)| {
        e.u32(*b).u64(*ep);
    });
    e.seq(&v.snapshots, |e, s| {
        e.u32(s.branch)
            .u64(s.epoch)
            .str(&s.name)
            .u128(s.group)
            .bool(s.deleting);
        e.seq(&s.chain, |e, (b, ep)| {
            e.u32(*b).u64(*ep);
        });
    });
    e.seq(&v.skip, |e, (lo, hi)| {
        e.u64(*lo).u64(*hi);
    });
    e.u64(v.incarnation);
    // The trash (D54): when the volume went, and each trashed snapshot's
    // epoch and time. Older readers stop before these.
    e.u64(v.trashed_unix);
    let trashed: Vec<(u64, u64)> = v
        .snapshots
        .iter()
        .filter(|s| s.trashed_unix != 0)
        .map(|s| (s.epoch, s.trashed_unix))
        .collect();
    e.seq(&trashed, |e, (ep, t)| {
        e.u64(*ep).u64(*t);
    });
    e.into_vec()
}

pub(crate) fn attr<'a>(l: &'a Looked, name: &[u8]) -> Option<&'a [u8]> {
    l.attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_slice())
}

fn decode(id: u128, l: &Looked) -> Result<Volume, ClientError> {
    let bad = |what: &str| ClientError::BadVolume(format!("header of {id:032x}: {what}"));
    let version = attr(l, VER)
        .and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())
        .ok_or_else(|| bad("no version"))?;
    let mut d = Dec::new(attr(l, HDR).ok_or_else(|| bad("no header"))?);
    let fmt = d.u8().map_err(|e| bad(&e.to_string()))?;
    if fmt != 1 && fmt != 2 {
        return Err(bad(&format!("format {fmt}")));
    }
    let v = (|| {
        let mut v = Volume {
            id,
            pool: d.u32()?,
            size: d.u64()?,
            lineage: d.u128()?,
            state: match d.u8()? {
                0 => VolumeState::Active,
                2 => VolumeState::Trashed,
                _ => VolumeState::Deleting,
            },
            name: d.str()?.to_string(),
            version,
            holder: attr(l, LEASE_ATTR).and_then(|h| decode_holder(h).map(|h| h.lease)),
            delegates: attr(l, DELEGATES_ATTR).map_or_else(Vec::new, decode_delegates),
            holder_label: attr(l, LEASE_ATTR).and_then(|h| decode_holder(h).map(|h| h.label)),
            head_epoch: 0,
            branch: 0,
            chain: Vec::new(),
            snapshots: Vec::new(),
            skip: Vec::new(),
            incarnation: 0,
            trashed_unix: 0,
        };
        if fmt == 2 {
            v.head_epoch = d.u64()?;
            v.branch = d.u32()?;
            v.chain = d.seq(12, |d| Ok((d.u32()?, d.u64()?)))?;
            v.snapshots = d.seq(4 + 8 + 4 + 16 + 1 + 4, |d| {
                Ok(Snapshot {
                    branch: d.u32()?,
                    epoch: d.u64()?,
                    name: d.str()?.to_string(),
                    group: d.u128()?,
                    deleting: d.bool()?,
                    chain: d.seq(12, |d| Ok((d.u32()?, d.u64()?)))?,
                    trashed_unix: 0,
                })
            })?;
            v.skip = d.seq(16, |d| Ok((d.u64()?, d.u64()?)))?;
            if d.remaining() > 0 {
                v.incarnation = d.u64()?;
            }
            if d.remaining() > 0 {
                v.trashed_unix = d.u64()?;
                for (ep, t) in d.seq(16, |d| Ok((d.u64()?, d.u64()?)))? {
                    if let Some(s) = v.snapshots.iter_mut().find(|s| s.epoch == ep) {
                        s.trashed_unix = t;
                    }
                }
            }
        }
        Ok::<_, hyperstor_codec::DecodeError>(v)
    })()
    .map_err(|e| bad(&e.to_string()))?;
    Ok(v)
}

/// Replace a header at version `from` with `v` (whose version is next).
pub(crate) fn cas(from: u64, v: &Volume) -> Vec<WireOp> {
    vec![
        WireOp::AssertAttr {
            name: VER.to_vec(),
            value: Some(from.to_string().into_bytes()),
        },
        WireOp::SetAttr(VER.to_vec(), v.version.to_string().into_bytes()),
        WireOp::SetAttr(HDR.to_vec(), encode(v)),
    ]
}

pub(crate) fn conflict(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Server {
            code: Code::Conflict,
            ..
        }
    )
}

impl Client {
    /// Creates a volume of `size` bytes (whole blocks) in data pool `pool`,
    /// with `id`, or a random one. A retry after an unknown outcome (the
    /// first attempt was applied, but its answer lost) is recognised by
    /// the creation token.
    pub async fn create_volume(
        &self,
        pool: u32,
        size: u64,
        id: Option<u128>,
        name: &str,
    ) -> Result<Volume, ClientError> {
        let map = self.map();
        if pool == META_POOL || !map.pools.contains_key(&pool) {
            return Err(ClientError::NoPool(pool));
        }
        if size == 0 || !size.is_multiple_of(4096) {
            return Err(ClientError::BadVolume(
                "the size must be a positive multiple of 4096".into(),
            ));
        }
        let id = id.unwrap_or_else(|| (u128::from(random_u64()) << 64) | u128::from(random_u64()));
        let v = Volume {
            id,
            pool,
            size,
            lineage: id,
            state: VolumeState::Active,
            name: name.to_string(),
            version: 1,
            holder: None,
            delegates: Vec::new(),
            holder_label: None,
            head_epoch: 1,
            branch: 0,
            chain: Vec::new(),
            snapshots: Vec::new(),
            skip: Vec::new(),
            incarnation: random_u64() | 1,
            trashed_unix: 0,
        };
        self.put_new(&v).await?;
        Ok(v)
    }

    /// Writes a new volume's header, then its directory entry. A retry
    /// after an unknown outcome is recognised by the creation token.
    pub(crate) async fn put_new(&self, v: &Volume) -> Result<(), ClientError> {
        let (id, pool) = (v.id, v.pool);
        let token = format!("{:016x}{:016x}", self.instance, random_u64()).into_bytes();
        let ops = [
            WireOp::AssertAbsent,
            WireOp::SetAttr(VER.to_vec(), b"1".to_vec()),
            WireOp::SetAttr(HDR.to_vec(), encode(v)),
            WireOp::SetAttr(BY.to_vec(), token.clone()),
        ];
        match self.exec(META_POOL, NS_META, &header_key(id), &ops).await {
            Ok(()) => {}
            Err(e) if conflict(&e) => {
                let mine = self
                    .lookup(META_POOL, NS_META, &header_key(id), b"", None, 0)
                    .await?
                    .is_some_and(|l| attr(&l, BY) == Some(token.as_slice()));
                if !mine {
                    return Err(ClientError::VolumeExists(id));
                }
            }
            Err(e) => return Err(e),
        }
        // Listed once it has a header: a create that stops here leaves a
        // volume that works but isn't listed until created again.
        self.exec(
            META_POOL,
            NS_META,
            &dir_key(pool, id),
            &[
                WireOp::Touch,
                WireOp::KvPut(id.to_be_bytes().to_vec(), Vec::new()),
            ],
        )
        .await?;
        Ok(())
    }

    /// The volume `id`, if it exists.
    pub async fn volume(&self, id: u128) -> Result<Option<Volume>, ClientError> {
        match self
            .lookup(META_POOL, NS_META, &header_key(id), b"", None, 0)
            .await?
        {
            Some(l) => decode(id, &l).map(Some),
            None => Ok(None),
        }
    }

    /// Every volume of data pool `pool`, in id order within each directory
    /// shard.
    pub async fn list_volumes(&self, pool: u32) -> Result<Vec<Volume>, ClientError> {
        const PAGE: u32 = 1000;
        let mut out = Vec::new();
        for shard in 0..DIR_SHARDS {
            let key = dir_key(pool, shard);
            let mut from = Vec::new();
            loop {
                let Some(l) = self
                    .lookup(META_POOL, NS_META, &key, &from, None, PAGE)
                    .await?
                else {
                    break;
                };
                for (k, _) in &l.kv {
                    let Ok(b) = <[u8; 16]>::try_from(k.as_slice()) else {
                        continue;
                    };
                    let id = u128::from_be_bytes(b);
                    // A directory entry whose header is gone: a delete that
                    // stopped half-way. Not listed.
                    if let Some(v) = self.volume(id).await? {
                        out.push(v);
                    }
                }
                match l.kv.last() {
                    Some((k, _)) if l.kv.len() == PAGE as usize => {
                        from = k.clone();
                        from.push(0);
                    }
                    _ => break,
                }
            }
        }
        Ok(out)
    }

    /// Grows a volume to `size` bytes (whole blocks). Shrinking is refused
    /// (03-overview.md §1); the same size changes nothing.
    pub async fn expand_volume(&self, id: u128, size: u64) -> Result<Volume, ClientError> {
        if !size.is_multiple_of(4096) {
            return Err(ClientError::BadVolume(
                "the size must be a multiple of 4096".into(),
            ));
        }
        for _ in 0..10 {
            let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
            if v.state != VolumeState::Active {
                return Err(ClientError::NoVolume(id));
            }
            if size < v.size {
                return Err(ClientError::Shrink {
                    have: v.size,
                    want: size,
                });
            }
            if size == v.size {
                return Ok(v);
            }
            let new = Volume {
                size,
                version: v.version + 1,
                ..v.clone()
            };
            match self
                .exec(META_POOL, NS_META, &header_key(id), &cas(v.version, &new))
                .await
            {
                Ok(()) => return Ok(new),
                // Changed meanwhile (or our own retry applied): look again.
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ClientError::BadVolume(format!(
            "volume {id:032x} kept changing"
        )))
    }

    /// Deletes a volume: marks it deleting, deletes its data, then its
    /// directory entry and header. A delete that stops half-way is resumed
    /// by deleting it again. An attached volume is refused: detach it, or
    /// take it over, first.
    pub async fn delete_volume(self: &Rc<Self>, id: u128) -> Result<(), ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        self.delete_held(v, None).await
    }

    /// Deletes a volume even if attached (04-decisions.md D80): preempts the
    /// holder first, which blocklists it, and deletes the volume holding
    /// its lease, so no write of the old holder's lands meanwhile.
    pub async fn force_delete_volume(self: &Rc<Self>, id: u128) -> Result<(), ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.state != VolumeState::Active {
            // Being deleted: no lease is granted; finish the delete.
            return self.delete_held(v, None).await;
        }
        let held = self.preempt(id).await?;
        let v = held.volume();
        let r = self.delete_held(v, Some(&held)).await;
        // Renewals stop; the header goes with the volume.
        held.lost.set(true);
        self.attached.borrow_mut().remove(&id);
        r
    }

    /// Deletes `v`, which has no holder, or is held through `held`: from
    /// active, from the trash, or a delete that stopped half-way.
    pub(crate) async fn delete_held(
        self: &Rc<Self>,
        mut v: Volume,
        held: Option<&Attached>,
    ) -> Result<(), ClientError> {
        let id = v.id;
        let me = held.map(|h| h.lease);
        let mut tries = 0;
        while v.state != VolumeState::Deleting {
            tries += 1;
            if tries > 10 {
                return Err(ClientError::BadVolume(format!(
                    "volume {id:032x} kept changing"
                )));
            }
            if v.holder.is_some() && v.holder != me {
                return Err(ClientError::VolumeAttached(id));
            }
            // Not while another volume's branch reads one of its branches.
            let (own, branches) = self.own_branches(&v).await?;
            for b in branches.iter().filter(|b| b.volume != id) {
                if let Some((_, e)) = b.chain.iter().find(|(x, _)| own.contains(x))
                    && self.branch_reads(b).await?
                {
                    return Err(ClientError::SnapshotInUse(*e));
                }
            }
            let new = Volume {
                state: VolumeState::Deleting,
                version: v.version + 1,
                ..v.clone()
            };
            // Decided by the header's primary, as leases are: no lease is
            // granted once it is deleting, and it isn't marked while held.
            let mut ops = cas(v.version, &new);
            ops.insert(
                0,
                WireOp::AssertAttr {
                    name: LEASE_ATTR.to_vec(),
                    value: held.map(|h| h.holder_attr()),
                },
            );
            match self.exec(META_POOL, NS_META, &header_key(id), &ops).await {
                Ok(()) => v = new,
                Err(e) if conflict(&e) => {
                    v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
                }
                Err(e) => return Err(e),
            }
        }
        // The data: every version of every chunk in each branch it owns,
        // a few chunks at a time. A chunk never written has none.
        let (own, _) = self.own_branches(&v).await?;
        let chunk = u64::from(
            self.map()
                .pools
                .get(&v.pool)
                .ok_or(ClientError::NoPool(v.pool))?
                .chunk_size,
        );
        let chunks = v.size.div_ceil(chunk);
        for b in &own {
            let mut next = 0;
            while next < chunks {
                let batch: Vec<_> = (next..(next + 32).min(chunks))
                    .map(|c| {
                        let (me, b) = (self.clone(), *b);
                        hyperstor_reactor::spawn(async move {
                            me.drop_versions(v.pool, v.lineage, c, b, 0, u64::MAX).await
                        })
                    })
                    .collect();
                for x in batch {
                    x.await?;
                }
                next += 32;
            }
        }
        for b in own.iter().filter(|b| **b != 0) {
            self.forget_branch(v.lineage, *b).await?;
        }
        self.exec(
            META_POOL,
            NS_META,
            &dir_key(v.pool, id),
            &[WireOp::Touch, WireOp::KvDel(id.to_be_bytes().to_vec())],
        )
        .await?;
        let gone = [
            WireOp::AssertAttr {
                name: VER.to_vec(),
                value: Some(v.version.to_string().into_bytes()),
            },
            WireOp::Delete,
        ];
        match self.exec(META_POOL, NS_META, &header_key(id), &gone).await {
            Ok(()) => Ok(()),
            // Our own retry already deleted it.
            Err(e) if conflict(&e) && self.volume(id).await?.is_none() => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// A volume attached through a [`Client`]: it holds the volume's `rw`
/// lease (06-metadata.md §9), which the client renews every second (D75),
/// and does I/O within the volume. Once the lease is lost (taken over,
/// its identity fenced, or this client blocklisted), I/O fails with
/// [`ClientError::LeaseLost`]. Dropped without [`Attached::detach`], its
/// renewals stop: the lease runs out, and another instance must
/// [`Client::take_over`].
///
/// A delegate ([`Client::delegate`]) is the same handle acting under
/// another client's lease (D33): it renews nothing, and is lost when its
/// identity is fenced or the holder no longer lists it.
pub struct Attached {
    client: Rc<Client>,
    volume: RefCell<Volume>,
    /// The identity every request carries (D150): the lease's, or this
    /// delegate's.
    lease: u64,
    /// A delegate's handle, not the holder's.
    delegate: bool,
    /// The holder's delegates, as last recorded (D151).
    delegates: RefCell<Vec<u64>>,
    /// The label the lease was taken with (D153).
    label: String,
    pub(crate) lost: Cell<bool>,
    /// Quiesced for a snapshot: writes wait (D103).
    paused: Cell<bool>,
    /// Writes in flight, which a quiesce waits for.
    inflight: Cell<u32>,
    /// For the read cache (D102): the version each block was last read
    /// from, when that version can't change (not the head).
    known: RefCell<HashMap<(u64, u64), (u32, u64)>>,
    /// Bumped as each write starts and ends: a read that saw a write
    /// doesn't record where its blocks came from.
    writes: Cell<u64>,
}

impl Attached {
    /// The volume, as of the last renewal that saw its header change.
    pub fn volume(&self) -> Volume {
        self.volume.borrow().clone()
    }

    /// Whether the lease is lost (or given up).
    pub fn lost(&self) -> bool {
        self.lost.get()
    }

    fn usable(&self, off: u64, len: u64) -> Result<Volume, ClientError> {
        let v = self.volume();
        if self.lost.get() {
            return Err(ClientError::LeaseLost(v.id));
        }
        if off.checked_add(len).is_none_or(|end| end > v.size) {
            return Err(ClientError::BadVolume(format!(
                "{len} bytes at {off} are past the end of volume {:032x}",
                v.id
            )));
        }
        Ok(v)
    }

    /// The identity this handle's requests carry (D150): for the holder,
    /// the lease's; for a delegate, its own.
    pub fn lease(&self) -> u64 {
        self.lease
    }

    /// The label the lease was taken with (D153).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this is a delegate's handle ([`Client::delegate`]).
    pub fn is_delegate(&self) -> bool {
        self.delegate
    }

    /// The `lease` attribute this handle's header has while it holds the
    /// lease: what an operation that needs the lease asserts.
    pub(crate) fn holder_attr(&self) -> Vec<u8> {
        encode_holder(&Holder {
            lease: self.lease,
            instance: self.client.instance,
            mode: LEASE_RW,
            label: self.label.clone(),
        })
    }

    /// A blocklisted client has lost every lease; a fenced identity, this
    /// one (D150).
    fn check<T>(&self, r: Result<T, ClientError>) -> Result<T, ClientError> {
        match &r {
            Err(ClientError::Server {
                code: Code::Blocklisted,
                ..
            }) => self.client.lose_all(),
            Err(ClientError::Server {
                code: Code::Fenced, ..
            }) => self.lose(),
            _ => {}
        }
        r
    }

    /// Marks this lease lost: renewals and I/O stop.
    fn lose(&self) {
        self.lost.set(true);
        if !self.delegate {
            let id = self.volume.borrow().id;
            let mut att = self.client.attached.borrow_mut();
            if att
                .get(&id)
                .and_then(|a| a.upgrade())
                .is_some_and(|a| std::ptr::eq(&*a, self))
            {
                att.remove(&id);
            }
        }
    }

    /// Writes at the head epoch of the volume's branch. While the holder
    /// is quiesced for a snapshot, waits.
    pub async fn write(&self, off: u64, data: Vec<u8>) -> Result<(), ClientError> {
        while self.paused.get() {
            sleep(Duration::from_millis(1)).await;
        }
        let v = self.usable(off, data.len() as u64)?;
        self.inflight.set(self.inflight.get() + 1);
        self.writes.set(self.writes.get() + 1);
        if let Some(blocks) = self.blocks(&v, off, data.len() as u64) {
            let mut known = self.known.borrow_mut();
            for b in blocks {
                known.remove(&b);
            }
        }
        let r = self
            .client
            .write_under(
                self.lease,
                v.pool,
                v.lineage,
                v.branch,
                v.head_epoch,
                off,
                data,
            )
            .await;
        self.writes.set(self.writes.get() + 1);
        self.inflight.set(self.inflight.get() - 1);
        self.check(r)
    }

    /// The (chunk, block in chunk) of each block of `[off, off + len)`;
    /// none if unaligned.
    fn blocks(&self, v: &Volume, off: u64, len: u64) -> Option<Vec<(u64, u64)>> {
        let cs = self.client.map().pools.get(&v.pool)?.chunk_size as u64;
        if !off.is_multiple_of(BLOCK) || !len.is_multiple_of(BLOCK) || !cs.is_multiple_of(BLOCK) {
            return None;
        }
        Some(
            (off / BLOCK..(off + len) / BLOCK)
                .map(|b| (b * BLOCK / cs, b * BLOCK % cs / BLOCK))
                .collect(),
        )
    }

    /// Reads at the head, through the volume's versions and its chain.
    /// Blocks whose version can't change come from the client's read
    /// cache when it has them (D102).
    pub async fn read(&self, off: u64, len: u64) -> Result<Vec<u8>, ClientError> {
        let v = self.usable(off, len)?;
        let blocks = self.blocks(&v, off, len).filter(|_| v.incarnation != 0);
        let Some(blocks) = blocks else {
            let r = self
                .client
                .read_at(v.pool, v.lineage, &v.layers(), off, len)
                .await;
            return self.check(r);
        };
        let key = |(chunk, block): (u64, u64), (branch, epoch): (u32, u64)| cache::Key {
            lineage: v.lineage,
            incarnation: v.incarnation,
            branch,
            epoch,
            chunk,
            block,
        };
        if let Some(out) = self.cached(&blocks, &key) {
            return Ok(out);
        }
        let (writes, quiet) = (self.writes.get(), self.inflight.get() == 0);
        let unchanged = |me: &Self| quiet && me.writes.get() == writes && *me.volume.borrow() == v;
        let cacheable = |src: &(u32, u64)| src.0 != u32::MAX && *src != (v.branch, v.head_epoch);
        // Blocks this handle hasn't read may be cached by another volume
        // of the lineage, such as a clone of the same image: ask where they
        // come from first.
        if self
            .client
            .cache
            .borrow()
            .has_lineage(v.lineage, v.incarnation)
        {
            let r = self
                .client
                .read_layers(self.lease, v.pool, v.lineage, &v.layers(), off, len, false)
                .await
                .map(|r| r.1);
            let sources = self.check(r)?;
            if unchanged(self) && sources.iter().all(cacheable) {
                let mut known = self.known.borrow_mut();
                for (b, src) in blocks.iter().zip(&sources) {
                    known.insert(*b, *src);
                }
                drop(known);
                if let Some(out) = self.cached(&blocks, &key) {
                    return Ok(out);
                }
            }
        }
        let r = self
            .client
            .read_layers(self.lease, v.pool, v.lineage, &v.layers(), off, len, true)
            .await;
        let (data, sources) = self.check(r)?;
        // Only what no write overlapped, from the header we read through.
        if unchanged(self) {
            let mut known = self.known.borrow_mut();
            let mut cache = self.client.cache.borrow_mut();
            for ((b, src), block) in blocks.iter().zip(&sources).zip(data.chunks(BLOCK as usize)) {
                if !cacheable(src) {
                    known.remove(b);
                    continue;
                }
                known.insert(*b, *src);
                cache.insert(key(*b, *src), block.into());
            }
        }
        Ok(data)
    }

    /// The blocks, if every one is known and in the cache.
    fn cached(
        &self,
        blocks: &[(u64, u64)],
        key: &dyn Fn((u64, u64), (u32, u64)) -> cache::Key,
    ) -> Option<Vec<u8>> {
        let known = self.known.borrow();
        let srcs: Vec<_> = blocks
            .iter()
            .map(|b| known.get(b).map(|s| key(*b, *s)))
            .collect::<Option<_>>()?;
        let mut cache = self.client.cache.borrow_mut();
        let mut out = Vec::with_capacity(blocks.len() * BLOCK as usize);
        for k in &srcs {
            out.extend_from_slice(&cache.get(k)?);
        }
        Some(out)
    }

    /// Reads the header again (after news from the watch). A changed
    /// header may read through other versions: forget where blocks were.
    async fn refresh(&self) {
        let id = self.volume.borrow().id;
        if let Ok(Some(v)) = self.client.volume(id).await {
            // A delegate acts only while the holder lists it.
            if self.delegate && !v.delegates.contains(&self.lease) {
                self.lose();
            }
            let old = self.volume.replace(v);
            let v = self.volume.borrow();
            if (
                old.branch,
                old.head_epoch,
                &old.chain,
                &old.skip,
                old.incarnation,
            ) != (v.branch, v.head_epoch, &v.chain, &v.skip, v.incarnation)
            {
                self.known.borrow_mut().clear();
            }
        }
    }

    /// Gives the lease up; I/O through this handle fails from now on.
    /// The holder's delegates are fenced first (D151): none of them
    /// writes after the lease is gone. A delegate's handle just stops.
    pub async fn detach(&self) -> Result<(), ClientError> {
        let was_lost = self.lost.get();
        self.lose();
        if self.delegate {
            return Ok(());
        }
        let delegates = self.delegates.borrow().clone();
        if !delegates.is_empty() && !was_lost {
            self.client.blocklist(&delegates).await?;
        }
        let key = header_key(self.volume.borrow().id);
        let r = self
            .client
            .to_primary(
                META_POOL,
                Route::Object {
                    ns: NS_META,
                    name: &key,
                },
                self.lease,
                &[],
                &|to, key| Msg::LeaseRelease { to, key },
            )
            .await;
        match r {
            Ok((Msg::LeaseOk { .. }, _)) => Ok(()),
            // Fenced: the lease is gone already.
            Err(ClientError::Server {
                code: Code::Fenced, ..
            }) => Ok(()),
            Ok((m, _)) => Err(unexpected(m)),
            Err(e) => Err(e),
        }
    }

    /// Adds a delegate (D33, D151): an identity that acts under this
    /// lease. Returns it: whoever is given it opens the volume with
    /// [`Client::delegate`]. Delegates renew nothing, and are fenced with
    /// the holder.
    pub async fn add_delegate(&self) -> Result<u64, ClientError> {
        let d = random_u64().max(1);
        let mut list = self.delegates.borrow().clone();
        list.push(d);
        self.set_delegates(list).await?;
        Ok(d)
    }

    /// Removes delegate `d`: fences it first (its requests are refused
    /// from then on), then takes it off the list.
    pub async fn remove_delegate(&self, d: u64) -> Result<(), ClientError> {
        if !self.delegates.borrow().contains(&d) {
            return Ok(());
        }
        self.client.blocklist(&[d]).await?;
        let list = self
            .delegates
            .borrow()
            .iter()
            .copied()
            .filter(|x| *x != d)
            .collect();
        self.set_delegates(list).await
    }

    /// The holder's delegates, as last recorded.
    pub fn delegates(&self) -> Vec<u64> {
        self.delegates.borrow().clone()
    }

    async fn set_delegates(&self, list: Vec<u64>) -> Result<(), ClientError> {
        if self.delegate {
            return Err(ClientError::BadVolume(
                "a delegate can't add delegates".into(),
            ));
        }
        if self.lost.get() {
            return Err(ClientError::LeaseLost(self.volume.borrow().id));
        }
        let key = header_key(self.volume.borrow().id);
        let r = self
            .client
            .to_primary(
                META_POOL,
                Route::Object {
                    ns: NS_META,
                    name: &key,
                },
                self.lease,
                &[],
                &|to, key| Msg::LeaseDelegates {
                    to,
                    key,
                    delegates: list.clone(),
                },
            )
            .await;
        match self.check(r)? {
            (Msg::LeaseOk { .. }, _) => {
                *self.delegates.borrow_mut() = list;
                Ok(())
            }
            (Msg::LeaseHeld { .. }, _) => {
                self.lose();
                Err(ClientError::LeaseLost(self.volume.borrow().id))
            }
            (m, _) => Err(unexpected(m)),
        }
    }
}

fn unexpected(m: Msg) -> ClientError {
    ClientError::Rpc(crate::RpcError::Protocol(format!("unexpected {m:?}")))
}

/// How [`Client::attach_as`] takes a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// [`Client::attach`].
    Attach,
    /// [`Client::take_over`].
    TakeOver,
    /// [`Client::preempt`].
    Preempt,
}

impl Client {
    /// Attaches volume `id` read-write: takes its lease, refused with
    /// [`ClientError::LeaseHeld`] while another instance holds it.
    pub async fn attach(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.attach_as(id, Take::Attach, &self.lease_label()).await
    }

    /// [`Self::attach`], [`Self::take_over`] or [`Self::preempt`], with
    /// `label` recorded with the lease instead of this client's (D153).
    pub async fn attach_as(
        self: &Rc<Self>,
        id: u128,
        how: Take,
        label: &str,
    ) -> Result<Rc<Attached>, ClientError> {
        let label = crate::cut_label(label);
        match how {
            Take::Attach => self.acquire(id, false, label).await,
            Take::TakeOver => self.take(id, false, label).await,
            Take::Preempt => self.take(id, true, label).await,
        }
    }

    /// Attaches volume `id` whose holder has died or is cut off
    /// (06-metadata.md §9, a forced takeover): waits until the holder's lease
    /// has expired (up to 4 s after its last renewal, D75), blocklists it,
    /// then takes the lease. The old holder's requests are refused from
    /// then on. A holder that keeps renewing is left alone:
    /// [`ClientError::LeaseHeld`].
    pub async fn take_over(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.attach_as(id, Take::TakeOver, &self.lease_label())
            .await
    }

    /// Attaches volume `id` from a live holder (an operator's preemption):
    /// blocklists the holder first, so its renewals and I/O are refused,
    /// then waits for its lease to expire and takes it.
    pub async fn preempt(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.attach_as(id, Take::Preempt, &self.lease_label()).await
    }

    /// Opens volume `id` as delegate `lease` of its holder (D33, D151):
    /// I/O under the holder's lease, which this client doesn't renew. The
    /// holder gave `lease` out ([`Attached::add_delegate`]); refused unless
    /// the header lists it. Lost once it is fenced or no longer listed.
    pub async fn delegate(
        self: &Rc<Self>,
        id: u128,
        lease: u64,
    ) -> Result<Rc<Attached>, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if lease == 0 || v.holder.is_none() || !v.delegates.contains(&lease) {
            return Err(ClientError::BadVolume(format!(
                "{lease:016x} is not a delegate of volume {id:032x}'s holder"
            )));
        }
        let a = Rc::new(Attached {
            client: self.clone(),
            volume: RefCell::new(v),
            lease,
            delegate: true,
            delegates: RefCell::new(Vec::new()),
            label: String::new(),
            lost: Cell::new(false),
            paused: Cell::new(false),
            inflight: Cell::new(0),
            known: RefCell::new(HashMap::new()),
            writes: Cell::new(0),
        });
        // Quiesces for snapshots as the holder does.
        hyperstor_reactor::spawn(self.clone().watch(Rc::downgrade(&a)));
        Ok(a)
    }

    async fn take(
        self: &Rc<Self>,
        id: u128,
        preempt: bool,
        label: String,
    ) -> Result<Rc<Attached>, ClientError> {
        let t = Instant::now();
        let mut blocked: Option<Vec<u64>> = None;
        loop {
            match self.acquire(id, true, label.clone()).await {
                Err(ClientError::LeaseHeld {
                    holder,
                    delegates,
                    remaining_ms,
                    label: theirs,
                }) => {
                    // The lease's identity and its delegates', not this
                    // holder's client: its other volumes go on (D150).
                    let ids: Vec<u64> = std::iter::once(holder).chain(delegates.clone()).collect();
                    if (remaining_ms == 0 || preempt) && blocked.as_ref() != Some(&ids) {
                        self.blocklist(&ids).await?;
                        blocked = Some(ids);
                    } else if t.elapsed() > 3 * LEASE_EXPIRES {
                        return Err(ClientError::LeaseHeld {
                            holder,
                            delegates,
                            remaining_ms,
                            label: theirs,
                        });
                    } else {
                        let ms = u64::from(remaining_ms).clamp(10, 1000);
                        sleep(Duration::from_millis(ms)).await;
                    }
                }
                r => return r,
            }
        }
    }

    /// Blocklists client instances or lease identities (06-metadata.md §9,
    /// D150): returns once every storage node refuses them, and this
    /// client's map has the entries.
    pub async fn blocklist(&self, ids: &[u64]) -> Result<(), ClientError> {
        let msg = Msg::Blocklist { ids: ids.to_vec() };
        match self.call_meta(&msg).await? {
            (Msg::BlocklistOk { map_epoch }, _) => {
                while self.map().epoch < map_epoch {
                    self.refresh_map().await?;
                    if self.map().epoch < map_epoch {
                        sleep(Duration::from_millis(10)).await;
                    }
                }
                Ok(())
            }
            (Msg::Error { code, detail, .. }, _) => Err(ClientError::Server { code, detail }),
            (m, _) => Err(unexpected(m)),
        }
    }

    async fn acquire(
        self: &Rc<Self>,
        id: u128,
        takeover: bool,
        label: String,
    ) -> Result<Rc<Attached>, ClientError> {
        let key = header_key(id);
        // Each attach its own identity (D150).
        let lease = random_u64().max(1);
        let reply = self
            .to_primary(
                META_POOL,
                Route::Object {
                    ns: NS_META,
                    name: &key,
                },
                lease,
                &[],
                &|to, key| Msg::Lease {
                    to,
                    key,
                    mode: LEASE_RW,
                    takeover,
                    label: label.clone(),
                },
            )
            .await;
        match reply {
            Ok((Msg::LeaseOk { .. }, _)) => {}
            Ok((
                Msg::LeaseHeld {
                    holder,
                    delegates,
                    remaining_ms,
                    label,
                },
                _,
            )) => {
                return Err(ClientError::LeaseHeld {
                    holder,
                    delegates,
                    remaining_ms,
                    label,
                });
            }
            Ok((m, _)) => return Err(unexpected(m)),
            Err(ClientError::Server {
                code: Code::NotFound,
                ..
            }) => return Err(ClientError::NoVolume(id)),
            Err(e) => return Err(e),
        }
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        let a = Rc::new(Attached {
            client: self.clone(),
            volume: RefCell::new(v),
            lease,
            delegate: false,
            delegates: RefCell::new(Vec::new()),
            label: label.clone(),
            lost: Cell::new(false),
            paused: Cell::new(false),
            inflight: Cell::new(0),
            known: RefCell::new(HashMap::new()),
            writes: Cell::new(0),
        });
        self.attached.borrow_mut().insert(id, Rc::downgrade(&a));
        hyperstor_reactor::spawn(self.clone().watch(Rc::downgrade(&a)));
        if !self.renewing.replace(true) {
            hyperstor_reactor::spawn(self.clone().renew_leases());
        }
        Ok(a)
    }

    /// Keeps a watch on an attached volume's header (D103) until it is
    /// detached or lost: reads the header again on news, and quiesces for
    /// a snapshot (stops writing, drains, tells the primary, and resumes at
    /// the new epoch once the snapshot is taken or abandoned).
    async fn watch(self: Rc<Self>, a: std::rc::Weak<Attached>) {
        loop {
            let Some(at) = a.upgrade().filter(|at| !at.lost.get()) else {
                return;
            };
            let (id, version, lease) = {
                let v = at.volume.borrow();
                (v.id, v.version, at.lease)
            };
            drop(at);
            let key = header_key(id);
            let route = Route::Object {
                ns: NS_META,
                name: &key,
            };
            let r = self
                .to_primary(META_POOL, route, lease, &[], &|to, key| Msg::Watch {
                    to,
                    key,
                    version,
                })
                .await;
            let Some(at) = a.upgrade().filter(|at| !at.lost.get()) else {
                return;
            };
            match r {
                Ok((
                    Msg::WatchOk {
                        event: WATCH_QUIESCE,
                        token,
                    },
                    _,
                )) => {
                    at.paused.set(true);
                    while at.inflight.get() > 0 {
                        sleep(Duration::from_millis(1)).await;
                    }
                    let _ =
                        self.to_primary(META_POOL, route, at.lease, &[], &|to, key| {
                            Msg::Quiesced { to, key, token }
                        })
                        .await;
                    at.refresh().await;
                    at.paused.set(false);
                }
                Ok((Msg::WatchOk { .. }, _)) => at.refresh().await,
                _ => sleep(Duration::from_millis(200)).await,
            }
        }
    }

    /// Marks every attached volume's lease lost.
    fn lose_all(&self) {
        for a in self
            .attached
            .borrow_mut()
            .drain()
            .filter_map(|(_, a)| a.upgrade())
        {
            a.lost.set(true);
        }
    }

    /// Renews every attached volume's lease each second (D75): one message
    /// to each `.meta` primary, for all the headers it serves. A lease the
    /// primary says is lost is marked so; a header whose version changed
    /// (grown) is read again. Stops when nothing is attached.
    async fn renew_leases(self: Rc<Self>) {
        loop {
            sleep(LEASE_RENEW_EVERY).await;
            let live: Vec<Rc<Attached>> = {
                let mut att = self.attached.borrow_mut();
                att.retain(|_, a| a.upgrade().is_some_and(|a| !a.lost.get()));
                att.values().filter_map(|a| a.upgrade()).collect()
            };
            if live.is_empty() {
                self.renewing.set(false);
                return;
            }
            let map = self.map();
            let Some(meta) = map.pools.get(&META_POOL) else {
                continue;
            };
            let mut by_primary: HashMap<std::net::SocketAddr, Vec<(Rc<Attached>, u32, u64)>> =
                HashMap::new();
            for a in live {
                let pg = meta.pg_of_key(NS_META, &header_key(a.volume.borrow().id));
                if let Some(info) = map.pg(META_POOL, pg)
                    && let Some(addr) = info.primary().and_then(|d| self.addr_of(&map, d))
                {
                    by_primary
                        .entry(addr)
                        .or_default()
                        .push((a, pg, info.epoch));
                }
            }
            let mut retry = false;
            for (addr, group) in by_primary {
                let leases = group
                    .iter()
                    .map(|(a, pg, e)| (*pg, *e, header_key(a.volume.borrow().id), a.lease))
                    .collect();
                let msg = Msg::LeaseRenew {
                    pool: META_POOL,
                    map_epoch: map.epoch,
                    leases,
                };
                let reply = match self.conn(addr).await {
                    Ok(c) => c.call(&msg, &[]).await.ok(),
                    Err(_) => None,
                };
                match reply {
                    Some((Msg::LeaseRenewOk { results }, _)) if results.len() == group.len() => {
                        for ((a, _, _), (r, version)) in group.into_iter().zip(results) {
                            match r {
                                LEASE_RENEWED if version > a.volume.borrow().version => {
                                    let me = self.clone();
                                    hyperstor_reactor::spawn(async move {
                                        let id = a.volume.borrow().id;
                                        if let Ok(Some(v)) = me.volume(id).await {
                                            *a.volume.borrow_mut() = v;
                                        }
                                    });
                                }
                                LEASE_RENEWED => {}
                                LEASE_LOST => a.lose(),
                                _ => retry = true,
                            }
                        }
                    }
                    Some((
                        Msg::Error {
                            code: Code::Blocklisted,
                            ..
                        },
                        _,
                    )) => self.lose_all(),
                    _ => retry = true,
                }
            }
            if retry {
                let _ = self.refresh_map().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads at the head skip what resets dropped (D106) until it is
    /// trimmed, then go through the chain.
    #[test]
    fn layers_skip_what_a_reset_dropped() {
        let mut v = Volume {
            id: 1,
            pool: 1,
            size: 4096,
            lineage: 1,
            state: VolumeState::Active,
            name: String::new(),
            version: 1,
            holder: None,
            delegates: Vec::new(),
            holder_label: None,
            head_epoch: 9,
            branch: 2,
            chain: vec![(0, 3)],
            snapshots: Vec::new(),
            skip: Vec::new(),
            incarnation: 0,
            trashed_unix: 0,
        };
        assert_eq!(v.layers(), vec![(2, 0, 9), (0, 0, 3)]);
        // Reset to 3 at head 5, then to 6 at head 8.
        v.skip = vec![(4, 5), (7, 8)];
        assert_eq!(v.layers(), vec![(2, 9, 9), (2, 6, 6), (2, 0, 3), (0, 0, 3)]);
        // A reset right after a snapshot: nothing between.
        v.skip = vec![(9, 8)];
        assert_eq!(v.layers()[0], (2, 0, 9));
    }

    #[test]
    fn headers_round_trip() {
        let v = Volume {
            id: 7,
            pool: 1,
            size: 1 << 30,
            lineage: 7,
            state: VolumeState::Deleting,
            name: "disk-a".into(),
            version: 3,
            holder: Some(9),
            delegates: vec![4, 6],
            holder_label: Some("vhost:n1".into()),
            head_epoch: 5,
            branch: 2,
            chain: vec![(0, 3)],
            snapshots: vec![Snapshot {
                branch: 2,
                epoch: 4,
                name: "s".into(),
                group: 0,
                deleting: false,
                chain: vec![(0, 3)],
                trashed_unix: 0,
            }],
            skip: vec![(2, 3)],
            incarnation: 11,
            trashed_unix: 0,
        };
        let l = Looked {
            size: 0,
            attrs: vec![
                (VER.to_vec(), b"3".to_vec()),
                (HDR.to_vec(), encode(&v)),
                (
                    LEASE_ATTR.to_vec(),
                    encode_holder(&Holder {
                        lease: 9,
                        instance: 8,
                        mode: LEASE_RW,
                        label: "vhost:n1".into(),
                    }),
                ),
                (DELEGATES_ATTR.to_vec(), encode_delegates(&[4, 6])),
            ],
            kv: vec![],
        };
        assert_eq!(decode(7, &l).unwrap(), v);
        // The trash's fields come last: 8 bytes of the volume's time, then
        // the trashed snapshots (none here: a 4-byte count).
        let h = encode(&v);
        let trash_len = 8 + 4;
        // A header from before the trash reads as not in it...
        let mut old = l.clone();
        old.attrs[1].1 = h[..h.len() - trash_len].to_vec();
        assert_eq!(decode(7, &old).unwrap(), v);
        // ...and from before incarnations, with none.
        old.attrs[1].1 = h[..h.len() - trash_len - 8].to_vec();
        assert_eq!(
            decode(7, &old).unwrap(),
            Volume {
                incarnation: 0,
                ..v.clone()
            }
        );
        // In the trash: the volume's time, and its trashed snapshots'.
        let mut t = v.clone();
        t.state = VolumeState::Trashed;
        t.trashed_unix = 1_800_000_000;
        t.snapshots[0].trashed_unix = 1_800_000_100;
        let mut lt = l.clone();
        lt.attrs[1].1 = encode(&t);
        assert_eq!(decode(7, &lt).unwrap(), t);
        let attrs: std::collections::BTreeMap<_, _> = lt.attrs.iter().cloned().collect();
        assert!(
            header_deleting(&attrs),
            "storage nodes grant a trashed volume no lease"
        );
        let attrs: std::collections::BTreeMap<_, _> = l.attrs.iter().cloned().collect();
        assert_eq!(header_version(&attrs), 3);
        assert!(header_deleting(&attrs));
        let active = Volume {
            state: VolumeState::Active,
            ..v.clone()
        };
        let attrs = [(HDR.to_vec(), encode(&active))].into_iter().collect();
        assert!(!header_deleting(&attrs));
        let h = Holder {
            lease: 9,
            instance: 8,
            mode: LEASE_RW,
            label: "tool".into(),
        };
        assert_eq!(decode_holder(&encode_holder(&h)), Some(h));
        // Written before identities (D150): the instance is the identity.
        let mut old = 5u64.to_le_bytes().to_vec();
        old.push(LEASE_RW);
        assert_eq!(
            decode_holder(&old),
            Some(Holder {
                lease: 5,
                instance: 5,
                mode: LEASE_RW,
                label: String::new(),
            })
        );
        assert_eq!(decode_holder(b"short"), None);
        assert_eq!(decode_delegates(&encode_delegates(&[1, 2])), vec![1, 2]);
        assert_eq!(decode_delegates(b"bad"), Vec::<u64>::new());
        assert!(decode(7, &Looked::default()).is_err());
        assert_eq!(dir_key(1, 65), b"d/1/01".to_vec());
        assert_eq!(
            header_key(255),
            b"h/000000000000000000000000000000ff".to_vec()
        );
    }
}
