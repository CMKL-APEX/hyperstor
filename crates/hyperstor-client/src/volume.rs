//! Volumes (metadata.md §4.2, decisions.md D77): a header object per volume
//! and directory objects per pool, in the `.meta` pool, changed by
//! compare-and-set on the header's version (implementation-notes.md I68).
//!
//! - **Header** `h/{id}` (32 hex digits): attributes `v` (the version, a
//!   decimal number), `hdr` (the encoded [`Volume`]) and `by` (who created
//!   it: a token that recognises a retried create).
//! - **Directory** `d/{pool}/{shard}`: the pool's volumes in 64 shards by
//!   id, one key–value entry per volume (its id, big-endian).
//!
//! A volume is identified by a 128-bit id, chosen by the caller (glidex:
//! its disk's UUID) or drawn at random; its name is shown, not unique.

use crate::{Client, ClientError, NS_META, random_u64};
use hyperstor_codec::{Dec, Enc};
use hyperstor_map::META_POOL;
use hyperstor_proto::{Code, Looked, WireOp};
use std::rc::Rc;

/// Directory shards per pool.
pub const DIR_SHARDS: u128 = 64;

const VER: &[u8] = b"v";
const HDR: &[u8] = b"hdr";
const BY: &[u8] = b"by";

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
}

fn header_key(id: u128) -> Vec<u8> {
    format!("h/{id:032x}").into_bytes()
}

fn dir_key(pool: u32, id: u128) -> Vec<u8> {
    format!("d/{pool}/{:02}", id % DIR_SHARDS).into_bytes()
}

fn encode(v: &Volume) -> Vec<u8> {
    let mut e = Enc::new();
    e.u8(1)
        .u32(v.pool)
        .u64(v.size)
        .u128(v.lineage)
        .u8(match v.state {
            VolumeState::Active => 0,
            VolumeState::Deleting => 1,
        })
        .str(&v.name);
    e.into_vec()
}

fn attr<'a>(l: &'a Looked, name: &[u8]) -> Option<&'a [u8]> {
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
    if fmt != 1 {
        return Err(bad(&format!("format {fmt}")));
    }
    let v = (|| {
        Ok::<_, hyperstor_codec::DecodeError>(Volume {
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
        })
    })()
    .map_err(|e| bad(&e.to_string()))?;
    Ok(v)
}

/// Replace a header at version `from` with `v` (whose version is next).
fn cas(from: u64, v: &Volume) -> Vec<WireOp> {
    vec![
        WireOp::AssertAttr {
            name: VER.to_vec(),
            value: Some(from.to_string().into_bytes()),
        },
        WireOp::SetAttr(VER.to_vec(), v.version.to_string().into_bytes()),
        WireOp::SetAttr(HDR.to_vec(), encode(v)),
    ]
}

fn conflict(e: &ClientError) -> bool {
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
        };
        let token = format!("{:016x}{:016x}", self.instance, random_u64()).into_bytes();
        let ops = [
            WireOp::AssertAbsent,
            WireOp::SetAttr(VER.to_vec(), b"1".to_vec()),
            WireOp::SetAttr(HDR.to_vec(), encode(&v)),
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
        Ok(v)
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
    /// by deleting it again.
    pub async fn delete_volume(self: &Rc<Self>, id: u128) -> Result<(), ClientError> {
        let mut v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
        while v.state == VolumeState::Active {
            let new = Volume {
                state: VolumeState::Deleting,
                version: v.version + 1,
                ..v.clone()
            };
            match self
                .exec(META_POOL, NS_META, &header_key(id), &cas(v.version, &new))
                .await
            {
                Ok(()) => v = new,
                Err(e) if conflict(&e) => {
                    v = self.volume(id).await?.ok_or(ClientError::NoVolume(id))?;
                }
                Err(e) => return Err(e),
            }
        }
        // The data: every chunk the volume can have, a few at a time. A
        // chunk never written is a delete of nothing.
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
                .map(|c| {
                    let me = self.clone();
                    hyperstor_reactor::spawn(async move {
                        me.exec_chunk(v.pool, v.lineage, c, &[WireOp::Delete]).await
                    })
                })
                .collect();
            for b in batch {
                b.await?;
            }
            next += 32;
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
        };
        let l = Looked {
            size: 0,
            attrs: vec![(VER.to_vec(), b"3".to_vec()), (HDR.to_vec(), encode(&v))],
            kv: vec![],
        };
        assert_eq!(decode(7, &l).unwrap(), v);
        assert!(decode(7, &Looked::default()).is_err());
        assert_eq!(dir_key(1, 65), b"d/1/01".to_vec());
        assert_eq!(
            header_key(255),
            b"h/000000000000000000000000000000ff".to_vec()
        );
    }
}
