//! The map service's replicated store (04-decisions.md D20, D71): the
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
    /// Join tokens, by their secret's SHA-256 (`tokens.rs`, D81).
    Tokens = 3,
    /// Certificates the API issued, by serial (`certs.rs`, D93).
    Issued = 4,
    /// Revoked serials (`certs.rs`, D56, D93).
    Revoked = 5,
    /// `hyperstor-mgmt`'s rows (`mgmt_store.rs`, D98).
    Mgmt = 6,
    /// Settings the map service keeps for the cluster (`settings.rs`): the
    /// trash's per pool (D54, D115).
    Settings = 7,
    /// Long operations (`operations.rs`, D121).
    Operations = 8,
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
        [
            Table::Map,
            Table::Local,
            Table::Tokens,
            Table::Issued,
            Table::Revoked,
            Table::Mgmt,
            Table::Settings,
            Table::Operations,
        ]
        .into_iter()
        .find(|t| *t as u16 == id)
    }
}

impl rs::Schema for MapSchema {
    type Table = Table;
    type Origin = Origin;
    const TABLES: &'static [Table] = &[
        Table::Map,
        Table::Local,
        Table::Tokens,
        Table::Issued,
        Table::Revoked,
        Table::Mgmt,
        Table::Settings,
        Table::Operations,
    ];
    const LOCAL: Table = Table::Local;
    const SYSTEM: Origin = Origin::System;
    const SNAPSHOT_MAGIC: &'static [u8; 8] = b"HSMAP01\n";
    const CHECK_ENV: &'static str = "HS_CHECK_WRITE_SETS";
    /// The first map of a large cluster is written whole: 65,536 PGs
    /// (06-metadata.md §4.3) are about 10 MB as a write set.
    const MAX_WRITE_SET_BYTES: usize = 64 << 20;

    fn table_name(t: Table) -> &'static str {
        match t {
            Table::Map => "map",
            Table::Local => "local",
            Table::Tokens => "tokens",
            Table::Issued => "issued",
            Table::Revoked => "revoked",
            Table::Mgmt => "mgmt",
            Table::Settings => "settings",
            Table::Operations => "operations",
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
/// Nodes' host names (D348), `host/{node}`: rows beside the head, so the
/// encoded map clients read stays as it was; older members ignore them.
const HOST_FIRST: &str = "host/";
const HOST_END: &str = "host0";

fn pg_key(pool: u32, pg: u32) -> String {
    format!("pg/{pool:010}/{pg:010}")
}

fn parse_pg_key(k: &str) -> Option<(u32, u32)> {
    let (pool, pg) = k.strip_prefix(PG_FIRST)?.split_once('/')?;
    Some((pool.parse().ok()?, pg.parse().ok()?))
}

/// Whether `k` is spelled as [`pg_key`] spells it: `pg/`, ten digits, `/`,
/// ten digits. Such keys sort as their numbers do.
fn is_pg_key(k: &str) -> bool {
    let b = k.as_bytes();
    b.len() == 24
        && b.starts_with(PG_FIRST.as_bytes())
        && b[13] == b'/'
        && b[3..13].iter().chain(&b[14..]).all(u8::is_ascii_digit)
}

fn encode_pg(i: &PgInfo) -> Vec<u8> {
    encode_pg_into(i, Vec::new())
}

/// `i`'s row, encoded into `buf` (cleared first) and returned.
fn encode_pg_into(i: &PgInfo, mut buf: Vec<u8>) -> Vec<u8> {
    buf.clear();
    let mut e = Enc::from_vec(buf);
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
        blocklist: map.blocklist.clone(),
        auto_out: map.auto_out.clone(),
        failed: map.failed.clone(),
        fixed_weights: map.fixed_weights.clone(),
        kinds: map.kinds.clone(),
        fixed_classes: map.fixed_classes.clone(),
        deleted_pools: map.deleted_pools.clone(),
        removed_devices: map.removed_devices.clone(),
        removed_nodes: map.removed_nodes.clone(),
        cache_gens: map.cache_gens.clone(),
        keep_acting: Default::default(),
        cache_held: map.cache_held.clone(),
        cache_released: map.cache_released.clone(),
        full_ratios: map.full_ratios,
        backfillfull: map.backfillfull.clone(),
        networks: map.networks.clone(),
        endpoints: map.endpoints.clone(),
        endpoints_down: map.endpoints_down.clone(),
        ports: map.ports.clone(),
        claims: map.claims.clone(),
        lending: map.lending,
        port_lending: map.port_lending.clone(),
        host_realms: map.host_realms.clone(),
        tenant_realms: map.tenant_realms.clone(),
        project_tenants: map.project_tenants.clone(),
        data_tls: map.data_tls,
        data_tls_encrypt: map.data_tls_encrypt,
        revoked: map.revoked.clone(),
        upgrades: map.upgrades.clone(),
        host_names: Default::default(),
    }
    .encode()
}

/// Whether the store holds a map: the group has committed its first.
pub fn has_map(db: &Db) -> Result<bool, String> {
    let txn = db.begin_read().map_err(|e| e.to_string())?;
    let Ok(t) = txn.open_table(definition::<MapSchema>(Table::Map)) else {
        return Ok(false);
    };
    Ok(t.get(HEAD).map_err(|e| e.to_string())?.is_some())
}

/// The published map the store holds, if any.
pub fn load(db: &Db) -> Result<Option<ClusterMap>, String> {
    let txn = db.begin_read().map_err(|e| e.to_string())?;
    let t = txn
        .open_table(definition::<MapSchema>(Table::Map))
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for row in t.iter().map_err(|e| e.to_string())? {
        let (k, v) = row.map_err(|e| e.to_string())?;
        rows.push((k.value().to_string(), v.value().to_vec()));
    }
    map_from_rows(&rows)
}

/// The map in the `map` table's rows (a backup's, or the store's).
pub fn map_from_rows(rows: &[(String, Vec<u8>)]) -> Result<Option<ClusterMap>, String> {
    let Some((_, h)) = rows.iter().find(|(k, _)| k == HEAD) else {
        return Ok(None);
    };
    let mut map = ClusterMap::decode(h).map_err(|e| format!("stored map: {e}"))?;
    for (k, v) in rows.iter().filter(|(k, _)| k.starts_with(PG_FIRST)) {
        let key = parse_pg_key(k).ok_or_else(|| format!("stored map: row {k}"))?;
        let info = decode_pg(v).map_err(|e| format!("stored map, {k}: {e}"))?;
        map.pgs.insert(key, info);
    }
    for (k, v) in rows.iter().filter(|(k, _)| k.starts_with(HOST_FIRST)) {
        let node = k[HOST_FIRST.len()..]
            .parse()
            .map_err(|_| format!("stored map: row {k}"))?;
        let name = String::from_utf8(v.clone()).map_err(|_| format!("stored map: row {k}"))?;
        map.host_names.insert(node, name);
    }
    Ok(Some(map))
}

/// Make the stored map `map` (on the leader, a Raft proposal that returns
/// once it is committed and applied here): the head, the PG rows whose
/// contents differ from what the store holds, and deletes for PGs `map`
/// no longer has. Compared inside the write, against what it applies to.
///
/// The stored PG rows and `map`'s PGs are both in `(pool, pg)` order (the
/// keys' numbers are zero-padded), so they are walked side by side: each
/// PG is encoded into one reused buffer to compare, and only the rows that
/// differ are built (#317).
pub fn save(db: &Db, map: &ClusterMap) -> Result<(), StoreError> {
    let head = head(map);
    db.write(Origin::Leader, |tx| -> Result<(), StoreError> {
        let mut t = tx.open_table(definition::<MapSchema>(Table::Map))?;
        if t.get(HEAD)?.is_none_or(|v| v.value() != head.as_slice()) {
            t.insert(HEAD, &head)?;
        }
        let mut put: Vec<(&(u32, u32), &PgInfo)> = Vec::new();
        let mut gone = Vec::new();
        let mut want = map.pgs.iter().peekable();
        let mut buf = Vec::new();
        for row in t.range(PG_FIRST..PG_END)? {
            let (k, v) = row?;
            let k = k.value();
            // A row whose key isn't one this writes is removed.
            let Some(key) = parse_pg_key(k).filter(|_| is_pg_key(k)) else {
                gone.push(k.to_string());
                continue;
            };
            while let Some(w) = want.next_if(|(wk, _)| **wk < key) {
                put.push(w);
            }
            match want.next_if(|(wk, _)| **wk == key) {
                Some((wk, i)) => {
                    buf = encode_pg_into(i, buf);
                    if buf.as_slice() != v.value() {
                        put.push((wk, i));
                    }
                }
                None => gone.push(k.to_string()),
            }
        }
        put.extend(want);
        for k in gone {
            t.remove(&k)?;
        }
        for ((pool, pg), i) in put {
            t.insert(&pg_key(*pool, *pg), &encode_pg(i))?;
        }
        let mut hosts: BTreeMap<String, &[u8]> = map
            .host_names
            .iter()
            .map(|(n, h)| (format!("{HOST_FIRST}{n}"), h.as_bytes()))
            .collect();
        let mut gone = Vec::new();
        for row in t.range(HOST_FIRST..HOST_END)? {
            let (k, v) = row?;
            match hosts.get(k.value()) {
                Some(w) if *w == v.value() => {
                    hosts.remove(k.value());
                }
                Some(_) => {}
                None => gone.push(k.value().to_string()),
            }
        }
        for k in gone {
            t.remove(&k)?;
        }
        for (k, v) in &hosts {
            t.insert(k.as_str(), v)?;
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

    /// D348: nodes' host names are rows beside the head, kept across a
    /// reopen, changed and removed with the map, and not in the head.
    #[test]
    fn host_names_are_rows_beside_the_map() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.db")).unwrap();
        let mut m = crate::parse_cluster(
            "cluster 7\nnode 0 rack 0 127.0.0.1:7000\nnode 1 rack 0 127.0.0.1:7001\npool 1 vol replicated size 2 min 1 pgs 8\n",
        )
        .unwrap();
        m.recompute();
        let plain = head(&m);
        m.host_names.insert(0, "storage-a".into());
        m.host_names.insert(1, "storage-b".into());
        assert_eq!(head(&m), plain, "names stay out of the head");
        save(&db, &m).unwrap();
        assert_eq!(load(&db).unwrap().unwrap().host_names, m.host_names);
        m.host_names.insert(1, "storage-c".into());
        m.host_names.remove(&0);
        save(&db, &m).unwrap();
        let back = load(&db).unwrap().unwrap();
        assert_eq!(back.host_names, [(1, "storage-c".to_string())].into());
        assert_eq!(back.pgs, m.pgs);
    }

    /// #317: against a model (a full compare of every encoded row), random
    /// changes write exactly the rows that differ, delete the rows the map
    /// no longer has (and rows spelled otherwise), and load back as the map.
    #[test]
    fn saves_write_exactly_the_rows_that_differ() {
        use rand::rngs::SmallRng;
        use rand::{RngExt, SeedableRng};
        for seed in 0..20u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let dir = tempfile::tempdir().unwrap();
            let db = Db::create(dir.path().join("map.db")).unwrap();
            let mut m = crate::parse_cluster(
                "cluster 7\nnode 0 rack 0 127.0.0.1:7000\nnode 1 rack 0 127.0.0.1:7001\nnode 2 rack 0 127.0.0.1:7002\npool 1 vol replicated size 2 min 1 pgs 32\n",
            )
            .unwrap();
            m.recompute();
            save(&db, &m).unwrap();
            let mut rx = db.subscribe();
            for _ in 0..40 {
                m.epoch += 1;
                for _ in 0..rng.random_range(0..6) {
                    match rng.random_range(0..5) {
                        // A PG's epoch or members change.
                        0 | 1 => {
                            if let Some(pg) = m
                                .pgs
                                .keys()
                                .nth(rng.random_range(0..m.pgs.len().max(1)))
                                .copied()
                            {
                                let i = m.pgs.get_mut(&pg).unwrap();
                                i.epoch = m.epoch;
                                i.acting.rotate_left(1);
                                if rng.random_bool(0.5) {
                                    i.up.push(rng.random_range(0..9));
                                }
                            }
                        }
                        // PGs of a pool (or a pool of its own) come.
                        2 => {
                            let pool = rng.random_range(1..5u32);
                            for _ in 0..rng.random_range(1..8) {
                                m.pgs.insert(
                                    (pool, rng.random_range(0..64u32)),
                                    PgInfo {
                                        epoch: m.epoch,
                                        acting: vec![0, 1],
                                        up: vec![0, 1],
                                    },
                                );
                            }
                        }
                        // PGs go.
                        3 => {
                            let n = rng.random_range(0..4);
                            let keys: Vec<_> = m
                                .pgs
                                .keys()
                                .copied()
                                .filter(|_| rng.random_bool(0.2))
                                .take(n)
                                .collect();
                            for k in keys {
                                m.pgs.remove(&k);
                            }
                        }
                        // A row this never writes: removed at the next save.
                        _ => {
                            db.write(Origin::Leader, |tx| -> Result<(), StoreError> {
                                let mut t = tx.open_table(definition::<MapSchema>(Table::Map))?;
                                t.insert("pg/1/3", b"stray")?;
                                Ok(())
                            })
                            .unwrap();
                            while rx.try_recv().is_ok() {}
                        }
                    }
                }
                let stored: BTreeMap<String, Vec<u8>> = {
                    let txn = db.begin_read().unwrap();
                    let t = txn.open_table(definition::<MapSchema>(Table::Map)).unwrap();
                    t.range(PG_FIRST..PG_END)
                        .unwrap()
                        .map(|r| {
                            let (k, v) = r.unwrap();
                            (k.value().to_string(), v.value().to_vec())
                        })
                        .collect()
                };
                // The model: every row encoded and compared.
                let want: BTreeMap<String, Vec<u8>> = m
                    .pgs
                    .iter()
                    .map(|((p, g), i)| (pg_key(*p, *g), encode_pg(i)))
                    .collect();
                let mut model: Vec<(String, bool)> = want
                    .iter()
                    .filter(|(k, v)| stored.get(*k) != Some(*v))
                    .map(|(k, _)| (k.clone(), true))
                    .chain(
                        stored
                            .keys()
                            .filter(|k| !want.contains_key(*k))
                            .map(|k| (k.clone(), false)),
                    )
                    .collect();
                model.sort();
                save(&db, &m).unwrap();
                let mut wrote: Vec<(String, bool)> = match rx.try_recv() {
                    Ok(a) => {
                        a.ws.ops
                            .iter()
                            .filter(|op| op.key().starts_with(PG_FIRST.as_bytes()))
                            .map(|op| {
                                let k = String::from_utf8(op.key().to_vec()).unwrap();
                                (k, matches!(op, rs::Op::Put { .. }))
                            })
                            .collect()
                    }
                    Err(_) => Vec::new(),
                };
                wrote.sort();
                assert_eq!(wrote, model, "seed {seed}");
                assert_eq!(load(&db).unwrap().unwrap().pgs, m.pgs, "seed {seed}");
            }
        }
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

    /// The largest map of the scale envelope (06-metadata.md §4.3: 65,536 PGs)
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
        assert!(changed > 0 && changed < m.pgs.len());
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
