//! Snapshots, clones, rollback and flatten (volumes.md §10.3, §10.4;
//! decisions.md D10, D21, D101, D103, D104).
//!
//! A chunk's data in a branch is a set of versions, one per head epoch it
//! was written in (engine.md §7.2). A snapshot is a compare-and-set on the
//! header: it records the head epoch, and the head moves on; nothing is
//! copied. A clone or a rollback is a new branch of the lineage, cut from
//! a snapshot; its reads go through its chain of parents.
//!
//! The lineage object `l/{lineage}` gives out branch numbers and records
//! each branch (as key–value entries): its volume, the chain it was cut
//! with, and the epoch it was flattened at, if it was. A snapshot is in
//! use while a branch that still reads its chain has it in its chain; a
//! flattened branch still does while its volume has snapshots from before
//! the flatten (implementation-notes.md I115). Clone and delete race
//! safely: a delete marks the snapshot deleting before it looks for
//! dependents, and a clone records itself before it checks the snapshot
//! is still live.

use crate::volume::{Snapshot, Volume, VolumeState, attr, cas, conflict, header_key};
use crate::{Client, ClientError, NS_META, Route, random_u64};
use hyperstor_codec::{Dec, Enc};
use hyperstor_map::META_POOL;
use hyperstor_proto::{Msg, WireOp, encode_ops};
use std::rc::Rc;

/// At most this many snapshots per volume (volumes.md §10.3).
pub const MAX_SNAPSHOTS: usize = 255;
/// Clone chains longer than this are refused: flatten first.
pub const MAX_CHAIN: usize = 16;

const NEXT: &[u8] = b"next";

fn lineage_key(lineage: u128) -> Vec<u8> {
    format!("l/{lineage:032x}").into_bytes()
}

/// A branch of a lineage, as its lineage object records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub branch: u32,
    /// The volume whose branch it is.
    pub volume: u128,
    /// The chain it was cut with: its parent first. Kept as it was.
    pub chain: Vec<(u32, u64)>,
    /// The head epoch it was flattened at (0: not flattened). Reads at that
    /// epoch or later don't go through the chain.
    pub flattened: u64,
}

fn encode_branch(b: &Branch) -> Vec<u8> {
    let mut e = Enc::new();
    e.u128(b.volume)
        .u64(b.flattened)
        .seq(&b.chain, |e, (x, ep)| {
            e.u32(*x).u64(*ep);
        });
    e.into_vec()
}

fn decode_branch(k: &[u8], v: &[u8]) -> Option<Branch> {
    let branch = u32::from_be_bytes(k.try_into().ok()?);
    let mut d = Dec::new(v);
    Some(Branch {
        branch,
        volume: d.u128().ok()?,
        flattened: d.u64().ok()?,
        chain: d.seq(12, |d| Ok((d.u32()?, d.u64()?))).ok()?,
    })
}

fn unexpected(m: Msg) -> ClientError {
    ClientError::Rpc(crate::RpcError::Protocol(format!("unexpected {m:?}")))
}

impl Client {
    /// Whether branch record `r` still reads through its chain: it isn't
    /// flattened, or its volume has snapshots of it from before.
    async fn needs_chain(&self, r: &Branch) -> Result<bool, ClientError> {
        if r.flattened == 0 {
            return Ok(true);
        }
        Ok(match self.volume(r.volume).await? {
            Some(v) => v
                .snapshots
                .iter()
                .any(|s| s.branch == r.branch && s.epoch < r.flattened),
            None => false,
        })
    }

    /// [`Self::needs_chain`], for volume deletion.
    pub(crate) async fn branch_reads(&self, r: &Branch) -> Result<bool, ClientError> {
        self.needs_chain(r).await
    }

    /// The branch records that still read through their chains.
    async fn reading(&self, branches: &[Branch]) -> Result<Vec<Branch>, ClientError> {
        let mut out = Vec::new();
        for b in branches {
            if self.needs_chain(b).await? {
                out.push(b.clone());
            }
        }
        Ok(out)
    }

    /// The epochs of `branch` something reads at, other than `but`: the
    /// owner's snapshots of it, its head if on it, and every chain that
    /// goes through it.
    fn cuts(v: &Volume, reading: &[Branch], branch: u32, but: u64) -> Vec<u64> {
        let mut cuts: Vec<u64> = v
            .snapshots
            .iter()
            .filter(|x| x.branch == branch && x.epoch != but && !x.deleting)
            .map(|x| x.epoch)
            .chain(
                reading
                    .iter()
                    .flat_map(|r| r.chain.iter())
                    .filter(|(b, e)| *b == branch && *e != but)
                    .map(|(_, e)| *e),
            )
            .chain(
                v.snapshots
                    .iter()
                    .filter(|x| !x.deleting && x.epoch != but)
                    .flat_map(|x| x.chain.iter())
                    .filter(|(b, _)| *b == branch)
                    .map(|(_, e)| *e),
            )
            .collect();
        if v.branch == branch {
            cuts.push(v.head_epoch);
        }
        cuts.extend(
            v.chain
                .iter()
                .filter(|(b, _)| *b == branch)
                .map(|(_, e)| *e),
        );
        cuts
    }

    /// The branches of a lineage other than branch 0, as recorded.
    pub async fn branches(&self, lineage: u128) -> Result<Vec<Branch>, ClientError> {
        let l = self
            .lookup(
                META_POOL,
                NS_META,
                &lineage_key(lineage),
                b"",
                None,
                1 << 20,
            )
            .await?;
        Ok(l.map(|l| {
            l.kv.iter()
                .filter_map(|(k, v)| decode_branch(k, v))
                .collect()
        })
        .unwrap_or_default())
    }

    /// A new branch number in `lineage`.
    async fn new_branch(&self, lineage: u128) -> Result<u32, ClientError> {
        let key = lineage_key(lineage);
        for _ in 0..20 {
            let cur = self
                .lookup(META_POOL, NS_META, &key, b"", None, 0)
                .await?
                .and_then(|l| attr(&l, NEXT).map(<[u8]>::to_vec));
            let next: u32 = cur
                .as_ref()
                .and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())
                .unwrap_or(1);
            let ops = [
                WireOp::AssertAttr {
                    name: NEXT.to_vec(),
                    value: cur,
                },
                WireOp::Touch,
                WireOp::SetAttr(NEXT.to_vec(), (next + 1).to_string().into_bytes()),
            ];
            match self.exec(META_POOL, NS_META, &key, &ops).await {
                Ok(()) => return Ok(next),
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ClientError::Refused(format!(
            "lineage {lineage:032x}: branch numbers kept changing"
        )))
    }

    async fn record_branch(&self, lineage: u128, b: &Branch) -> Result<(), ClientError> {
        self.exec(
            META_POOL,
            NS_META,
            &lineage_key(lineage),
            &[
                WireOp::Touch,
                WireOp::KvPut(b.branch.to_be_bytes().to_vec(), encode_branch(b)),
            ],
        )
        .await
    }

    pub(crate) async fn forget_branch(
        &self,
        lineage: u128,
        branch: u32,
    ) -> Result<(), ClientError> {
        self.exec(
            META_POOL,
            NS_META,
            &lineage_key(lineage),
            &[WireOp::Touch, WireOp::KvDel(branch.to_be_bytes().to_vec())],
        )
        .await
    }

    /// Changes a header by compare-and-set, retrying on conflicts: `f`
    /// gets the volume as it is and returns the changed one, or an error.
    async fn change(
        &self,
        id: u128,
        mut f: impl FnMut(&Volume) -> Result<Volume, ClientError>,
        extra: &[WireOp],
    ) -> Result<Volume, ClientError> {
        for _ in 0..10 {
            let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
            let mut new = f(&v)?;
            new.version = v.version + 1;
            let mut ops = extra.to_vec();
            ops.extend(cas(v.version, &new));
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

    /// Takes a snapshot of volume `id` (volumes.md §10.3): its head epoch is
    /// recorded and moves on. If the volume is attached, its holder is
    /// quiesced first by the header's primary (D103); if it doesn't within
    /// 2 s the snapshot is abandoned (`Busy`).
    pub async fn snapshot(&self, id: u128, name: &str) -> Result<Snapshot, ClientError> {
        let key = header_key(id);
        for _ in 0..10 {
            let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
            if v.state != VolumeState::Active {
                return Err(ClientError::NoVolume(id));
            }
            if v.snapshots.len() >= MAX_SNAPSHOTS {
                return Err(ClientError::Refused(format!(
                    "volume {id:032x} has {MAX_SNAPSHOTS} snapshots"
                )));
            }
            let snap = Snapshot {
                branch: v.branch,
                epoch: v.head_epoch,
                name: name.to_string(),
                group: 0,
                deleting: false,
                chain: v.chain.clone(),
            };
            let mut new = v.clone();
            new.snapshots.push(snap.clone());
            new.head_epoch += 1;
            new.version += 1;
            let payload = encode_ops(&cas(v.version, &new));
            let route = Route::Object {
                ns: NS_META,
                name: &key,
            };
            match self
                .to_primary(META_POOL, route, &payload, &|to, key| Msg::Snapshot {
                    to,
                    key,
                })
                .await
            {
                Ok((Msg::ExecOk, _)) => return Ok(snap),
                Ok((m, _)) => return Err(unexpected(m)),
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ClientError::BadVolume(format!(
            "volume {id:032x} kept changing"
        )))
    }

    /// Runs `f` for every chunk a volume of `v`'s size can have, 32 at a
    /// time.
    async fn each_chunk<F, Fut>(self: &Rc<Self>, v: &Volume, f: F) -> Result<(), ClientError>
    where
        F: Fn(Rc<Client>, u64) -> Fut,
        Fut: std::future::Future<Output = Result<(), ClientError>> + 'static,
    {
        let chunk = u64::from(
            self.map()
                .pools
                .get(&v.pool)
                .ok_or(ClientError::NoPool(v.pool))?
                .chunk_size,
        );
        let chunks = v.size.div_ceil(chunk);
        let mut next = 0;
        while next < chunks {
            let batch: Vec<_> = (next..(next + 32).min(chunks))
                .map(|c| hyperstor_reactor::spawn(f(self.clone(), c)))
                .collect();
            for b in batch {
                b.await?;
            }
            next += 32;
        }
        Ok(())
    }

    async fn versions(&self, v: &Volume, chunk: u64, branch: u32) -> Result<Vec<u64>, ClientError> {
        match self
            .to_chunk(v.pool, v.lineage, chunk, branch, 0, &|to, key| {
                Msg::Versions { to, key }
            })
            .await?
        {
            (Msg::VersionsOk { versions }, _) => Ok(versions),
            (m, _) => Err(unexpected(m)),
        }
    }

    /// Deletes a chunk's versions in `branch` from epoch `from` on.
    pub(crate) async fn drop_versions(
        &self,
        pool: u32,
        lineage: u128,
        chunk: u64,
        branch: u32,
        from: u64,
    ) -> Result<(), ClientError> {
        match self
            .to_chunk(pool, lineage, chunk, branch, 0, &|to, key| {
                Msg::DropVersions { to, key, from }
            })
            .await?
        {
            (Msg::ExecOk, _) => Ok(()),
            (m, _) => Err(unexpected(m)),
        }
    }

    /// Deletes the snapshot at `epoch` (volumes.md §10.3). Refused while a
    /// clone or rollback depends on it. Its versions are merged into the
    /// next newer version of each chunk without copying (D104), unless a
    /// later cut point (a snapshot, a clone's or rollback's parent, the
    /// head) still reads them through; versions nothing reads are deleted.
    /// A delete that stops half-way is resumed by deleting it again.
    pub async fn delete_snapshot(self: &Rc<Self>, id: u128, epoch: u64) -> Result<(), ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        let s = v
            .snapshot(epoch)
            .cloned()
            .ok_or(ClientError::NoSnapshot(epoch))?;
        if v.snapshots.iter().any(|x| x.deleting && x.epoch != epoch) {
            return Err(ClientError::Refused(
                "another snapshot of this volume is being deleted".into(),
            ));
        }
        // Marked first, then dependents looked for: a clone records itself
        // before it checks the snapshot, so one of the two always sees the
        // other.
        let v = self
            .change(
                id,
                |v| {
                    let mut n = v.clone();
                    let s = n
                        .snapshots
                        .iter_mut()
                        .find(|x| x.epoch == epoch)
                        .ok_or(ClientError::NoSnapshot(epoch))?;
                    s.deleting = true;
                    Ok(n)
                },
                &[],
            )
            .await?;
        let branches = self.branches(v.lineage).await?;
        let reading = self.reading(&branches).await?;
        let used = reading.iter().any(|r| r.chain.contains(&(s.branch, epoch)))
            || v.chain.contains(&(s.branch, epoch))
            || v.snapshots
                .iter()
                .any(|x| x.epoch != epoch && x.chain.contains(&(s.branch, epoch)));
        if used {
            self.change(
                id,
                |v| {
                    let mut n = v.clone();
                    if let Some(s) = n.snapshots.iter_mut().find(|x| x.epoch == epoch) {
                        s.deleting = false;
                    }
                    Ok(n)
                },
                &[],
            )
            .await?;
            return Err(ClientError::SnapshotInUse(epoch));
        }
        // What else reads this branch at or below some epoch.
        let cuts = Self::cuts(&v, &reading, s.branch, epoch);
        let below = cuts.iter().copied().filter(|c| *c < epoch).max();
        let above = cuts.iter().copied().filter(|c| *c > epoch).min();
        let v2 = v.clone();
        self.each_chunk(&v, move |me, chunk| {
            let v = v2.clone();
            async move {
                let versions = me.versions(&v, chunk, s.branch).await?;
                let mine: Vec<u64> = versions
                    .iter()
                    .copied()
                    .filter(|e| *e <= epoch && below.is_none_or(|b| *e > b))
                    .collect();
                if mine.is_empty() {
                    return Ok(());
                }
                let newer = versions.iter().copied().find(|e| *e > epoch);
                match (newer, above) {
                    // The next version absorbs them, newest first, so the
                    // newest of them wins where they overlap.
                    (Some(n), a) if a.is_none_or(|a| a >= n) => {
                        let ops: Vec<WireOp> =
                            mine.iter().rev().map(|e| WireOp::Absorb(*e)).collect();
                        let route = Route::Chunk {
                            lineage: v.lineage,
                            chunk,
                            branch: s.branch,
                            epoch: n,
                        };
                        me.exec_at(v.pool, route, &ops).await
                    }
                    // Nothing newer, and nothing reads them: gone.
                    (None, None) => {
                        let from = below.map_or(0, |b| b + 1);
                        me.drop_versions(v.pool, v.lineage, chunk, s.branch, from)
                            .await
                    }
                    // A later cut point still reads them.
                    _ => Ok(()),
                }
            }
        })
        .await?;
        self.change(
            id,
            |v| {
                let mut n = v.clone();
                n.snapshots.retain(|x| x.epoch != epoch);
                Ok(n)
            },
            &[],
        )
        .await?;
        Ok(())
    }

    /// Creates volume `new_id` (or a random id) as a clone of the snapshot
    /// at `epoch` of volume `id` (volumes.md §10.4): a new branch of the
    /// same lineage (D21), allocating nothing.
    pub async fn clone_snapshot(
        &self,
        id: u128,
        epoch: u64,
        name: &str,
        new_id: Option<u128>,
    ) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        let s = v
            .snapshot(epoch)
            .filter(|s| !s.deleting)
            .cloned()
            .ok_or(ClientError::NoSnapshot(epoch))?;
        let chain: Vec<(u32, u64)> = std::iter::once((s.branch, epoch))
            .chain(s.chain.iter().copied())
            .collect();
        if chain.len() > MAX_CHAIN {
            return Err(ClientError::Refused(format!(
                "clone chains are limited to {MAX_CHAIN} levels: flatten first"
            )));
        }
        let nid =
            new_id.unwrap_or_else(|| (u128::from(random_u64()) << 64) | u128::from(random_u64()));
        let b = self.new_branch(v.lineage).await?;
        let rec = Branch {
            branch: b,
            volume: nid,
            chain: chain.clone(),
            flattened: 0,
        };
        self.record_branch(v.lineage, &rec).await?;
        // Recorded first, then the snapshot checked again: a delete that
        // marked it meanwhile wins, and this clone gives up.
        let still = self
            .volume(id)
            .await?
            .is_some_and(|v| v.snapshot(epoch).is_some_and(|s| !s.deleting));
        if !still {
            self.forget_branch(v.lineage, b).await?;
            return Err(ClientError::NoSnapshot(epoch));
        }
        let c = Volume {
            id: nid,
            pool: v.pool,
            size: v.size,
            lineage: v.lineage,
            state: VolumeState::Active,
            name: name.to_string(),
            version: 1,
            holder: None,
            head_epoch: 1,
            branch: b,
            chain,
            snapshots: Vec::new(),
        };
        if let Err(e) = self.put_new(&c).await {
            self.forget_branch(v.lineage, b).await?;
            return Err(e);
        }
        Ok(c)
    }

    /// Rolls volume `id` back to its snapshot at `epoch` (volumes.md
    /// §10.3): its head becomes a new branch cut from the snapshot, at
    /// once; then the old head's versions newer than anything still reading
    /// them are deleted. The volume must be detached.
    pub async fn rollback(self: &Rc<Self>, id: u128, epoch: u64) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.holder.is_some() {
            return Err(ClientError::VolumeAttached(id));
        }
        let s = v
            .snapshot(epoch)
            .filter(|s| !s.deleting)
            .cloned()
            .ok_or(ClientError::NoSnapshot(epoch))?;
        let chain: Vec<(u32, u64)> = std::iter::once((s.branch, epoch))
            .chain(s.chain.iter().copied())
            .collect();
        if chain.len() > MAX_CHAIN {
            return Err(ClientError::Refused(format!(
                "rollbacks and clones nest at most {MAX_CHAIN} levels: flatten first"
            )));
        }
        let b = self.new_branch(v.lineage).await?;
        self.record_branch(
            v.lineage,
            &Branch {
                branch: b,
                volume: id,
                chain: chain.clone(),
                flattened: 0,
            },
        )
        .await?;
        let lease_free = [WireOp::AssertAttr {
            name: crate::volume::LEASE_ATTR.to_vec(),
            value: None,
        }];
        let r = self
            .change(
                id,
                |cur| {
                    if cur.holder.is_some() {
                        return Err(ClientError::VolumeAttached(id));
                    }
                    if cur.snapshot(epoch).is_none_or(|s| s.deleting) {
                        return Err(ClientError::NoSnapshot(epoch));
                    }
                    let mut n = cur.clone();
                    n.branch = b;
                    n.chain = chain.clone();
                    n.head_epoch = cur.head_epoch + 1;
                    Ok(n)
                },
                &lease_free,
            )
            .await;
        let new = match r {
            Ok(n) => n,
            Err(e) => {
                self.forget_branch(v.lineage, b).await?;
                return Err(e);
            }
        };
        self.trim_branch(&new, v.branch).await?;
        Ok(new)
    }

    /// Deletes the versions of `old` (one of the volume's former head
    /// branches) above its newest remaining cut point: they are read by
    /// nothing.
    async fn trim_branch(self: &Rc<Self>, v: &Volume, old: u32) -> Result<(), ClientError> {
        let branches = self.branches(v.lineage).await?;
        let reading = self.reading(&branches).await?;
        let keep = Self::cuts(v, &reading, old, u64::MAX).into_iter().max();
        let from = keep.map_or(0, |k| k + 1);
        let (pool, lineage) = (v.pool, v.lineage);
        self.each_chunk(v, move |me, chunk| async move {
            me.drop_versions(pool, lineage, chunk, old, from).await
        })
        .await
    }

    /// Copies into clone `id`'s own branch every block it still reads from
    /// its parents, then clears its chain (volumes.md §10.4): it then
    /// depends on nothing, and its parents' snapshots can be deleted.
    /// Refused while it has snapshots taken before a rollback, which read
    /// through the chain.
    pub async fn flatten(self: &Rc<Self>, id: u128) -> Result<Volume, ClientError> {
        let v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        if v.chain.is_empty() {
            return Ok(v);
        }
        let v2 = v.clone();
        self.each_chunk(&v, move |me, chunk| {
            let v = v2.clone();
            async move {
                let layers = v.chain.clone();
                match me
                    .to_chunk(
                        v.pool,
                        v.lineage,
                        chunk,
                        v.branch,
                        v.head_epoch,
                        &|to, key| Msg::Flatten {
                            to,
                            key,
                            layers: layers.clone(),
                        },
                    )
                    .await?
                {
                    (Msg::ExecOk, _) => Ok(()),
                    (m, _) => Err(unexpected(m)),
                }
            }
        })
        .await?;
        let new = self
            .change(
                id,
                |cur| {
                    let mut n = cur.clone();
                    n.chain.clear();
                    Ok(n)
                },
                &[],
            )
            .await?;
        // Its own record: flattened at the head epoch the copies went to.
        let mut branches = self.branches(v.lineage).await?;
        if let Some(r) = branches.iter_mut().find(|r| r.branch == v.branch) {
            r.flattened = v.head_epoch;
            let r = r.clone();
            self.record_branch(v.lineage, &r).await?;
        }
        // Former branches of its own (left by rollbacks) that nothing reads
        // any more are dropped, until none is left to drop.
        loop {
            let reading = self.reading(&branches).await?;
            let new = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
            let gone = v.chain.iter().map(|(b, _)| *b).find(|b| {
                *b != new.branch
                    && branches.iter().any(|r| r.branch == *b && r.volume == id)
                    && !new
                        .snapshots
                        .iter()
                        .any(|s| s.branch == *b || s.chain.iter().any(|(x, _)| x == b))
                    && !reading
                        .iter()
                        .any(|r| r.branch != *b && r.chain.iter().any(|(x, _)| x == b))
            });
            let Some(b) = gone else { break };
            let (pool, lineage) = (v.pool, v.lineage);
            self.each_chunk(&v, move |me, chunk| async move {
                me.drop_versions(pool, lineage, chunk, b, 0).await
            })
            .await?;
            self.forget_branch(v.lineage, b).await?;
            branches.retain(|r| r.branch != b);
        }
        Ok(new)
    }

    /// The branches volume `v` owns: its head's, branch 0 if it began the
    /// lineage, and those its rollbacks left.
    pub(crate) async fn own_branches(
        &self,
        v: &Volume,
    ) -> Result<(Vec<u32>, Vec<Branch>), ClientError> {
        let branches = self.branches(v.lineage).await?;
        let mut own: Vec<u32> = branches
            .iter()
            .filter(|b| b.volume == v.id)
            .map(|b| b.branch)
            .collect();
        own.push(v.branch);
        if v.lineage == v.id {
            own.push(0);
        }
        own.sort_unstable();
        own.dedup();
        Ok((own, branches))
    }
}
