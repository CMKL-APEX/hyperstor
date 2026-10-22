//! Volumes (metadata.md §4.2, decisions.md D77): a header object per volume
//! and directory objects per pool, in the `.meta` pool, changed by
//! compare-and-set on the header's version (implementation-notes.md I68).
//!
//! - **Header** `h/{id}` (32 hex digits): attributes `v` (the version, a
//!   decimal number), `hdr` (the encoded [`Volume`]), `by` (who created
//!   it: a token that recognises a retried create) and, while the volume
//!   is attached, `lease` (its holder, kept by the header's primary:
//!   metadata.md §9).
//! - **Directory** `d/{pool}/{shard}`: the pool's volumes in 64 shards by
//!   id, one key–value entry per volume (its id, big-endian).
//!
//! A volume is identified by a 128-bit id, chosen by the caller (glidex:
//! its disk's UUID) or drawn at random; its name is shown, not unique.

use crate::{Client, ClientError, NS_META, Route, object_pg, random_u64};
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

/// The `lease` attribute: the holder's instance and mode.
pub fn encode_holder(instance: u64, mode: u8) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(instance).u8(mode);
    e.into_vec()
}

pub fn decode_holder(b: &[u8]) -> Option<(u64, u8)> {
    let mut d = Dec::new(b);
    let h = (d.u64().ok()?, d.u8().ok()?);
    d.finish().ok()?;
    Some(h)
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub id: u128,
    /// The data pool.
    pub pool: u32,
    pub size: u64,
    /// Where its chunks are placed (decisions.md D21): its own id, or its
    /// parent's lineage for a clone.
    pub lineage: u128,
    pub state: VolumeState,
    pub name: String,
    /// The header's version: every change bumps it.
    pub version: u64,
    /// The client instance that has it attached (holds its lease).
    pub holder: Option<u64>,
    /// The epoch writes go to now (D10): each snapshot takes the epoch it
    /// was, and moves this on.
    pub head_epoch: u64,
    /// Its branch in the lineage (D21): 0 for a volume created as such, a
    /// new one for a clone or after a rollback.
    pub branch: u32,
    /// The branches it reads through, parent first: each (branch, epoch)
    /// a clone or rollback was cut from (engine.md §7.2).
    pub chain: Vec<(u32, u64)>,
    pub snapshots: Vec<Snapshot>,
}

/// A snapshot of a volume (volumes.md §10.3): the volume as it was at
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
}

impl Volume {
    /// The layers a read at the head goes through: its own branch up to
    /// the head epoch, then its chain.
    pub fn layers(&self) -> Vec<(u32, u64)> {
        std::iter::once((self.branch, self.head_epoch))
            .chain(self.chain.iter().copied())
            .collect()
    }

    /// The layers a read of snapshot `s` goes through.
    pub fn snapshot_layers(&self, s: &Snapshot) -> Vec<(u32, u64)> {
        std::iter::once((s.branch, s.epoch))
            .chain(s.chain.iter().copied())
            .collect()
    }

    pub fn snapshot(&self, epoch: u64) -> Option<&Snapshot> {
        self.snapshots.iter().find(|s| s.epoch == epoch)
    }
}

fn dir_key(pool: u32, id: u128) -> Vec<u8> {
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
            state: if d.u8()? == 0 {
                VolumeState::Active
            } else {
                VolumeState::Deleting
            },
            name: d.str()?.to_string(),
            version,
            holder: attr(l, LEASE_ATTR).and_then(|h| decode_holder(h).map(|h| h.0)),
            head_epoch: 0,
            branch: 0,
            chain: Vec::new(),
            snapshots: Vec::new(),
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
                })
            })?;
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
            head_epoch: 1,
            branch: 0,
            chain: Vec::new(),
            snapshots: Vec::new(),
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
    /// (overview.md §1); the same size changes nothing.
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

    /// Deletes a volume even if attached (decisions.md D80): preempts the
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

    /// Deletes `v`, which has no holder, or is held through `held`.
    async fn delete_held(
        self: &Rc<Self>,
        mut v: Volume,
        held: Option<&Attached>,
    ) -> Result<(), ClientError> {
        let id = v.id;
        let me = held.map(|_| self.instance);
        let mut tries = 0;
        while v.state == VolumeState::Active {
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
                    value: me.map(|i| encode_holder(i, LEASE_RW)),
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
                            me.drop_versions(v.pool, v.lineage, c, b, 0).await
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
/// lease (metadata.md §9), which the client renews every second (D75),
/// and does I/O within the volume. Once the lease is lost (taken over, or
/// this client blocklisted), I/O fails with [`ClientError::LeaseLost`].
/// Dropped without [`Attached::detach`], its renewals stop: the lease
/// runs out, and another instance must [`Client::take_over`].
pub struct Attached {
    client: Rc<Client>,
    volume: RefCell<Volume>,
    lost: Cell<bool>,
    /// Quiesced for a snapshot: writes wait (D98).
    paused: Cell<bool>,
    /// Writes in flight, which a quiesce waits for.
    inflight: Cell<u32>,
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

    /// A blocklisted client has lost every lease.
    fn check<T>(&self, r: Result<T, ClientError>) -> Result<T, ClientError> {
        if let Err(ClientError::Server {
            code: Code::Blocklisted,
            ..
        }) = &r
        {
            self.client.lose_all();
        }
        r
    }

    /// Writes at the head epoch of the volume's branch. While the holder
    /// is quiesced for a snapshot, waits.
    pub async fn write(&self, off: u64, data: Vec<u8>) -> Result<(), ClientError> {
        while self.paused.get() {
            sleep(Duration::from_millis(1)).await;
        }
        let v = self.usable(off, data.len() as u64)?;
        self.inflight.set(self.inflight.get() + 1);
        let r = self
            .client
            .write_at(v.pool, v.lineage, v.branch, v.head_epoch, off, data)
            .await;
        self.inflight.set(self.inflight.get() - 1);
        self.check(r)
    }

    /// Reads at the head, through the volume's versions and its chain.
    pub async fn read(&self, off: u64, len: u64) -> Result<Vec<u8>, ClientError> {
        let v = self.usable(off, len)?;
        let r = self
            .client
            .read_at(v.pool, v.lineage, &v.layers(), off, len)
            .await;
        self.check(r)
    }

    /// Reads the header again (after news from the watch).
    async fn refresh(&self) {
        let id = self.volume.borrow().id;
        if let Ok(Some(v)) = self.client.volume(id).await {
            *self.volume.borrow_mut() = v;
        }
    }

    /// Gives the lease up; I/O through this handle fails from now on.
    pub async fn detach(&self) -> Result<(), ClientError> {
        let id = self.volume.borrow().id;
        self.lost.set(true);
        self.client.attached.borrow_mut().remove(&id);
        let key = header_key(id);
        let (msg, _) = self
            .client
            .to_primary(
                META_POOL,
                Route::Object {
                    ns: NS_META,
                    name: &key,
                },
                &[],
                &|to, key| Msg::LeaseRelease { to, key },
            )
            .await?;
        match msg {
            Msg::LeaseOk { .. } => Ok(()),
            m => Err(unexpected(m)),
        }
    }
}

fn unexpected(m: Msg) -> ClientError {
    ClientError::Rpc(crate::RpcError::Protocol(format!("unexpected {m:?}")))
}

impl Client {
    /// Attaches volume `id` read-write: takes its lease, refused with
    /// [`ClientError::LeaseHeld`] while another instance holds it.
    pub async fn attach(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.acquire(id, false).await
    }

    /// Attaches volume `id` whose holder has died or is cut off
    /// (metadata.md §9, a forced takeover): waits until the holder's lease
    /// has expired (up to 4 s after its last renewal, D75), blocklists it,
    /// then takes the lease. The old holder's requests are refused from
    /// then on. A holder that keeps renewing is left alone:
    /// [`ClientError::LeaseHeld`].
    pub async fn take_over(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.take(id, false).await
    }

    /// Attaches volume `id` from a live holder (an operator's preemption):
    /// blocklists the holder first, so its renewals and I/O are refused,
    /// then waits for its lease to expire and takes it.
    pub async fn preempt(self: &Rc<Self>, id: u128) -> Result<Rc<Attached>, ClientError> {
        self.take(id, true).await
    }

    async fn take(self: &Rc<Self>, id: u128, preempt: bool) -> Result<Rc<Attached>, ClientError> {
        let t = Instant::now();
        let mut blocked = None;
        loop {
            match self.acquire(id, true).await {
                Err(ClientError::LeaseHeld {
                    holder,
                    remaining_ms,
                }) => {
                    if (remaining_ms == 0 || preempt) && blocked != Some(holder) {
                        self.blocklist(holder).await?;
                        blocked = Some(holder);
                    } else if t.elapsed() > 3 * LEASE_EXPIRES {
                        return Err(ClientError::LeaseHeld {
                            holder,
                            remaining_ms,
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

    /// Blocklists a client instance (metadata.md §9): returns once every
    /// storage node refuses it, and this client's map has the entry.
    pub async fn blocklist(&self, instance: u64) -> Result<(), ClientError> {
        match self.call_meta(&Msg::Blocklist { instance }).await? {
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
    ) -> Result<Rc<Attached>, ClientError> {
        let key = header_key(id);
        let reply = self
            .to_primary(
                META_POOL,
                Route::Object {
                    ns: NS_META,
                    name: &key,
                },
                &[],
                &|to, key| Msg::Lease {
                    to,
                    key,
                    mode: LEASE_RW,
                    takeover,
                },
            )
            .await;
        match reply {
            Ok((Msg::LeaseOk { .. }, _)) => {}
            Ok((
                Msg::LeaseHeld {
                    holder,
                    remaining_ms,
                },
                _,
            )) => {
                return Err(ClientError::LeaseHeld {
                    holder,
                    remaining_ms,
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
            lost: Cell::new(false),
            paused: Cell::new(false),
            inflight: Cell::new(0),
        });
        self.attached.borrow_mut().insert(id, Rc::downgrade(&a));
        hyperstor_reactor::spawn(self.clone().watch(Rc::downgrade(&a)));
        if !self.renewing.replace(true) {
            hyperstor_reactor::spawn(self.clone().renew_leases());
        }
        Ok(a)
    }

    /// Keeps a watch on an attached volume's header (D98) until it is
    /// detached or lost: reads the header again on news, and quiesces for
    /// a snapshot (stops writing, drains, tells the primary, and resumes at
    /// the new epoch once the snapshot is taken or abandoned).
    async fn watch(self: Rc<Self>, a: std::rc::Weak<Attached>) {
        loop {
            let Some(at) = a.upgrade().filter(|at| !at.lost.get()) else {
                return;
            };
            let (id, version) = {
                let v = at.volume.borrow();
                (v.id, v.version)
            };
            drop(at);
            let key = header_key(id);
            let route = Route::Object {
                ns: NS_META,
                name: &key,
            };
            let r = self
                .to_primary(META_POOL, route, &[], &|to, key| Msg::Watch {
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
                    let _ = self
                        .to_primary(META_POOL, route, &[], &|to, key| Msg::Quiesced {
                            to,
                            key,
                            token,
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
            let Some(pg_count) = map.pools.get(&META_POOL).map(|p| p.pg_count) else {
                continue;
            };
            let mut by_primary: HashMap<std::net::SocketAddr, Vec<(Rc<Attached>, u32, u64)>> =
                HashMap::new();
            for a in live {
                let pg = object_pg(&header_key(a.volume.borrow().id), pg_count);
                if let Some(info) = map.pg(META_POOL, pg)
                    && let Some(addr) = info.primary().and_then(|d| map.device_addr(d))
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
                    .map(|(a, pg, e)| (*pg, *e, header_key(a.volume.borrow().id)))
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
                                LEASE_LOST => a.lost.set(true),
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
            }],
        };
        let l = Looked {
            size: 0,
            attrs: vec![
                (VER.to_vec(), b"3".to_vec()),
                (HDR.to_vec(), encode(&v)),
                (LEASE_ATTR.to_vec(), encode_holder(9, LEASE_RW)),
            ],
            kv: vec![],
        };
        assert_eq!(decode(7, &l).unwrap(), v);
        let attrs: std::collections::BTreeMap<_, _> = l.attrs.iter().cloned().collect();
        assert_eq!(header_version(&attrs), 3);
        assert!(header_deleting(&attrs));
        let active = Volume {
            state: VolumeState::Active,
            ..v.clone()
        };
        let attrs = [(HDR.to_vec(), encode(&active))].into_iter().collect();
        assert!(!header_deleting(&attrs));
        assert_eq!(
            decode_holder(&encode_holder(9, LEASE_RW)),
            Some((9, LEASE_RW))
        );
        assert_eq!(decode_holder(b"short"), None);
        assert!(decode(7, &Looked::default()).is_err());
        assert_eq!(dir_key(1, 65), b"d/1/01".to_vec());
        assert_eq!(
            header_key(255),
            b"h/000000000000000000000000000000ff".to_vec()
        );
    }
}
