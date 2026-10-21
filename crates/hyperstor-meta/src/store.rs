//! The map service's replicated store (decisions.md D20, D71): the
//! published map in `glidex-raftstore`'s write-set store.
//!
//! The map is rows of the `map` table: `head` (epoch, nodes, devices,
//! pools: the map without its PGs) and one row per PG,
//! `pg/{pool}/{pg}`. Publishing a change writes the head and the PG rows
//! whose contents changed, in one write set, so a change is atomic and
//! its entry is about the size of what changed.

use crate::{ClusterMap, PgInfo};
use glidex_raftstore::store::{self as rs, StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The map service's [`rs::Schema`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MapSchema;

/// The store's tables. **Append-only**: ids are on disk in Raft logs and
/// snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum Table {
    /// The published map: `head` and one row per PG.
    Map = 1,
    /// Each member's own Raft bookkeeping (not replicated).
    Local = 2,
}

impl Serialize for Table {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(*self as u16)
    }
}

impl<'de> Deserialize<'de> for Table {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = u16::deserialize(d)?;
        MapSchema::table_from_id_(id)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown table id {id}")))
    }
}

/// Who wrote: the leader publishing a map, or Raft itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Leader,
    System,
}

impl MapSchema {
    fn table_from_id_(id: u16) -> Option<Table> {
        [Table::Map, Table::Local]
            .into_iter()
            .find(|t| *t as u16 == id)
    }
}

impl rs::Schema for MapSchema {
    type Table = Table;
    type Origin = Origin;
    const TABLES: &'static [Table] = &[Table::Map, Table::Local];
    const LOCAL: Table = Table::Local;
    const SYSTEM: Origin = Origin::System;
    const SNAPSHOT_MAGIC: &'static [u8; 8] = b"HSMAP01\n";
    const CHECK_ENV: &'static str = "HS_CHECK_WRITE_SETS";
    /// The first map of a large cluster is written whole: 65,536 PGs
    /// (metadata.md §4.3) are about 10 MB as a write set.
    const MAX_WRITE_SET_BYTES: usize = 64 << 20;

    fn table_name(t: Table) -> &'static str {
        match t {
            Table::Map => "map",
            Table::Local => "local",
        }
    }
    fn table_id(t: Table) -> u16 {
        t as u16
    }
    fn table_from_id(id: u16) -> Option<Table> {
        Self::table_from_id_(id)
    }
    fn table_from_name(name: &str) -> Option<Table> {
        Self::TABLES
            .iter()
            .copied()
            .find(|t| Self::table_name(*t) == name)
    }
}

pub type Db = rs::Db<MapSchema>;

const HEAD: &str = "head";
/// PG rows sort between these two keys.
const PG_FIRST: &str = "pg/";
const PG_END: &str = "pg0";

fn pg_key(pool: u32, pg: u32) -> String {
    format!("pg/{pool:010}/{pg:010}")
}

fn parse_pg_key(k: &str) -> Option<(u32, u32)> {
    let (pool, pg) = k.strip_prefix(PG_FIRST)?.split_once('/')?;
    Some((pool.parse().ok()?, pg.parse().ok()?))
}

fn encode_pg(i: &PgInfo) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(i.epoch)
        .seq(&i.acting, |e, d| {
            e.u32(*d);
        })
        .seq(&i.up, |e, d| {
            e.u32(*d);
        });
    e.into_vec()
}

fn decode_pg(b: &[u8]) -> Result<PgInfo, DecodeError> {
    let mut d = Dec::new(b);
    let i = PgInfo {
        epoch: d.u64()?,
        acting: d.seq(4, |d| d.u32())?,
        up: d.seq(4, |d| d.u32())?,
    };
    d.finish()?;
    Ok(i)
}

/// The map without its PGs.
fn head(map: &ClusterMap) -> Vec<u8> {
    ClusterMap {
        epoch: map.epoch,
        cluster_id: map.cluster_id,
        nodes: map.nodes.clone(),
        devices: map.devices.clone(),
        pools: map.pools.clone(),
        pgs: BTreeMap::new(),
    }
    .encode()
}

/// The published map the store holds, if any.
pub fn load(db: &Db) -> Result<Option<ClusterMap>, String> {
    let txn = db.begin_read().map_err(|e| e.to_string())?;
    let t = txn
        .open_table(definition::<MapSchema>(Table::Map))
        .map_err(|e| e.to_string())?;
    let Some(h) = t.get(HEAD).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let mut map = ClusterMap::decode(h.value()).map_err(|e| format!("stored map: {e}"))?;
    for row in t.range(PG_FIRST..PG_END).map_err(|e| e.to_string())? {
        let (k, v) = row.map_err(|e| e.to_string())?;
        let key =
            parse_pg_key(k.value()).ok_or_else(|| format!("stored map: row {}", k.value()))?;
        let info = decode_pg(v.value()).map_err(|e| format!("stored map, {}: {e}", k.value()))?;
        map.pgs.insert(key, info);
    }
    Ok(Some(map))
}

/// Make the stored map `map` (on the leader, a Raft proposal that returns
/// once it is committed and applied here): the head, the PG rows whose
/// contents differ from what the store holds, and deletes for PGs `map`
/// no longer has. Compared inside the write, against what it applies to.
pub fn save(db: &Db, map: &ClusterMap) -> Result<(), StoreError> {
    let mut want: BTreeMap<String, Vec<u8>> = map
        .pgs
        .iter()
        .map(|((pool, pg), i)| (pg_key(*pool, *pg), encode_pg(i)))
        .collect();
    let head = head(map);
    db.write(Origin::Leader, |tx| -> Result<(), StoreError> {
        let mut t = tx.open_table(definition::<MapSchema>(Table::Map))?;
        if t.get(HEAD)?.is_none_or(|v| v.value() != head.as_slice()) {
            t.insert(HEAD, &head)?;
        }
        let mut gone = Vec::new();
        for row in t.range(PG_FIRST..PG_END)? {
            let (k, v) = row?;
            match want.get(k.value()) {
                Some(w) if w.as_slice() == v.value() => {
                    want.remove(k.value());
                }
                Some(_) => {}
                None => gone.push(k.value().to_string()),
            }
        }
        for k in gone {
            t.remove(&k)?;
        }
        for (k, v) in &want {
            t.insert(k, v)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_map_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("map.db");
        let mut m = crate::parse_cluster(
            "cluster 7\nnode 0 rack 0 127.0.0.1:7000\nnode 1 rack 0 127.0.0.1:7001\npool 1 vol replicated size 2 min 1 pgs 8\n",
        )
        .unwrap();
        m.recompute();
        {
            let db = Db::create(&path).unwrap();
            assert!(load(&db).unwrap().is_none());
            save(&db, &m).unwrap();
        }
        let db = Db::create(&path).unwrap();
        let back = load(&db).unwrap().unwrap();
        assert_eq!(back.encode(), m.encode());
    }

    /// Applies what it is given, as a group of one would: proposals go
    /// through the size check a replicated store makes.
    struct Applier(std::sync::atomic::AtomicU64);

    impl rs::Replicator<MapSchema> for Applier {
        fn propose(&self, db: &Db, ws: rs::WriteSet<MapSchema>) -> Result<u64, StoreError> {
            let i = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            db.apply(&ws, i, |_| Ok(()))?;
            Ok(i)
        }
    }

    /// The largest map of the scale envelope (metadata.md §4.3: 65,536 PGs)
    /// is written whole as one write set when it is first stored.
    #[test]
    fn a_large_map_fits_one_write_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.db")).unwrap();
        db.set_replicator(std::sync::Arc::new(Applier(Default::default())));
        let mut m = ClusterMap::new(7);
        for n in 0..100u32 {
            let a: Vec<std::net::SocketAddr> = (0..4)
                .map(|i| format!("10.0.0.{}:{}", n + 1, 9100 + i).parse().unwrap())
                .collect();
            m.add_node(n, &a, n % 10);
        }
        m.add_pool(crate::replicated_pool(1, "vol", 3, 65536));
        m.recompute();
        save(&db, &m).unwrap();
        assert_eq!(load(&db).unwrap().unwrap().pgs.len(), 65536);
    }

    /// A change writes the head and the PGs it changed, nothing else.
    #[test]
    fn a_change_writes_what_changed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.db")).unwrap();
        let mut m = crate::parse_cluster(
            "cluster 7\nnode 0 rack 0 127.0.0.1:7000\nnode 1 rack 0 127.0.0.1:7001\nnode 2 rack 0 127.0.0.1:7002\npool 1 vol replicated size 2 min 1 pgs 64\n",
        )
        .unwrap();
        m.recompute();
        save(&db, &m).unwrap();
        let mut rx = db.subscribe();
        m.nodes.get_mut(&2).unwrap().up = false;
        m.recompute();
        let changed = m.pgs.values().filter(|p| p.epoch == m.epoch).count();
        assert!(changed > 0 && changed < 64);
        save(&db, &m).unwrap();
        let applied = rx.try_recv().unwrap();
        assert_eq!(
            applied.ws.ops.len(),
            changed + 1,
            "the head and the changed PGs"
        );
        assert_eq!(load(&db).unwrap().unwrap(), m);
        // Nothing changed: nothing written.
        save(&db, &m).unwrap();
        assert!(rx.try_recv().is_err());
        // A pool dropped from the map: its rows go.
        m.pools.clear();
        m.pgs.clear();
        save(&db, &m).unwrap();
        assert_eq!(load(&db).unwrap().unwrap().pgs.len(), 0);
    }
}
