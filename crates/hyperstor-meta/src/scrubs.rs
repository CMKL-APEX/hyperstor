//! Each PG's last light and deep scrub (28-scrub.md §31.4, 04-decisions.md
//! D227), and the health checks they feed (§31.5, D228).
//!
//! The leader keeps the records in memory as primaries report and writes
//! the changed ones to the replicated store at most once a minute, so they
//! survive a leader change and a full restart. They are rows `scrub/…` of
//! `Table::Settings`: a member from before them applies the writes as rows
//! it never reads, where a table of their own would fail its apply in a
//! rolling upgrade. Every member reads its applied copy, so the API answers
//! on any of them, up to a minute behind the leader.

use crate::health::Health;
use crate::store::{self, Db, Origin, Table};
use crate::{ClusterMap, PoolInfo, PoolKind};
use glidex_raftstore::store::{StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use hyperstor_proto::ScrubDetail;
use std::collections::BTreeMap;

/// What one scrub of a PG did and found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// Unix seconds when it started.
    pub at: u64,
    pub took_ms: u32,
    pub compared: u32,
    pub inconsistent: u32,
    pub repaired: u32,
    pub unresolved: u32,
    pub damaged: u32,
    pub holes: u32,
    pub holes_repaired: u32,
    /// Why the hole check was skipped; empty when it ran.
    pub holes_skipped: String,
}

impl Summary {
    pub fn from_report(unresolved: u32, r: &ScrubDetail) -> Summary {
        Summary {
            at: r.started,
            took_ms: r.took_ms,
            compared: r.compared,
            inconsistent: r.inconsistent,
            repaired: r.repaired,
            unresolved,
            damaged: r.damaged,
            holes: r.holes,
            holes_repaired: r.holes_repaired,
            holes_skipped: r.holes_skipped.clone(),
        }
    }
}

/// A PG's record: since when it is counted (made, or first seen by a
/// leader), its last light and deep scrub, and how many scrubs running
/// skipped the hole check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PgScrubs {
    pub since: u64,
    pub light: Option<Summary>,
    pub deep: Option<Summary>,
    pub skipped: u8,
}

/// Every PG's record, by pool and PG.
pub type Records = BTreeMap<(u32, u32), PgScrubs>;

/// Records to write, and those to remove (`None`), by pool and PG.
pub type Changes = Vec<((u32, u32), Option<PgScrubs>)>;

/// Why a PG is overdue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overdue {
    Light,
    Deep,
}

impl PgScrubs {
    pub fn new(since: u64) -> PgScrubs {
        PgScrubs {
            since,
            ..Default::default()
        }
    }

    /// When its last scrub started, light or deep (a deep scrub compares
    /// the copies too); 0 for never.
    pub fn light_at(&self) -> u64 {
        self.light.as_ref().map_or(0, |s| s.at).max(self.deep_at())
    }

    pub fn deep_at(&self) -> u64 {
        self.deep.as_ref().map_or(0, |s| s.at)
    }

    /// The newest of its two reports.
    pub fn last(&self) -> Option<&Summary> {
        match (&self.light, &self.deep) {
            (Some(l), Some(d)) => Some(if d.at >= l.at { d } else { l }),
            (l, d) => l.as_ref().or(d.as_ref()),
        }
    }

    /// Records a scrub of the PG.
    pub fn note(&mut self, s: Summary, deep: bool) {
        self.skipped = if s.holes_skipped.is_empty() {
            0
        } else {
            self.skipped.saturating_add(1)
        };
        if deep {
            self.deep = Some(s);
        } else {
            self.light = Some(s);
        }
    }

    /// Whether the PG has gone without a light scrub for twice the pool's
    /// interval, or without a deep one for twice the deep interval (D228).
    /// A pool with scrub off is warned of as reduced instead.
    pub fn overdue(&self, pool: &PoolInfo, now: u64) -> Option<Overdue> {
        let hours = pool.scrub.hours as u64;
        if hours == 0 || pool.kind == PoolKind::Cache {
            return None;
        }
        let every = hours * 3600;
        let age = |at: u64| now.saturating_sub(at.max(self.since));
        if age(self.light_at()) > 2 * every {
            Some(Overdue::Light)
        } else if age(self.deep_at()) > 2 * every * pool.scrub.deep_every.max(1) as u64 {
            Some(Overdue::Deep)
        } else {
            None
        }
    }

    /// Whether its hole check was skipped in its last two scrubs (D228).
    pub fn unchecked(&self) -> bool {
        self.skipped >= 2
    }

    /// Whether its last scrub left damaged objects with no good copy to
    /// repair them from (D228).
    pub fn damaged(&self) -> bool {
        self.last().is_some_and(|s| s.damaged > 0)
    }
}

/// The records' rows sort between these two keys.
const FIRST: &str = "scrub/";
const END: &str = "scrub0";

fn key(pool: u32, pg: u32) -> String {
    format!("{FIRST}{pool:010}/{pg:010}")
}

fn parse_key(k: &str) -> Option<(u32, u32)> {
    let (pool, pg) = k.strip_prefix(FIRST)?.split_once('/')?;
    Some((pool.parse().ok()?, pg.parse().ok()?))
}

fn enc_summary(e: &mut Enc, s: &Option<Summary>) {
    let Some(s) = s else {
        e.bool(false);
        return;
    };
    e.bool(true)
        .u64(s.at)
        .u32(s.took_ms)
        .u32(s.compared)
        .u32(s.inconsistent)
        .u32(s.repaired)
        .u32(s.unresolved)
        .u32(s.damaged)
        .u32(s.holes)
        .u32(s.holes_repaired)
        .str(&s.holes_skipped);
}

fn dec_summary(d: &mut Dec<'_>) -> Result<Option<Summary>, DecodeError> {
    if !d.bool()? {
        return Ok(None);
    }
    Ok(Some(Summary {
        at: d.u64()?,
        took_ms: d.u32()?,
        compared: d.u32()?,
        inconsistent: d.u32()?,
        repaired: d.u32()?,
        unresolved: d.u32()?,
        damaged: d.u32()?,
        holes: d.u32()?,
        holes_repaired: d.u32()?,
        holes_skipped: d.str()?.to_string(),
    }))
}

/// The record's bytes in the store: a format byte (1), then its fields.
pub fn encode(r: &PgScrubs) -> Vec<u8> {
    let mut e = Enc::new();
    e.u8(1).u64(r.since).u8(r.skipped);
    enc_summary(&mut e, &r.light);
    enc_summary(&mut e, &r.deep);
    e.into_vec()
}

pub fn decode(b: &[u8]) -> Result<PgScrubs, DecodeError> {
    let mut d = Dec::new(b);
    let format = d.u8()?;
    if format != 1 {
        return Err(DecodeError::Invalid {
            field: "scrub record format",
            value: format as u64,
        });
    }
    Ok(PgScrubs {
        since: d.u64()?,
        skipped: d.u8()?,
        light: dec_summary(&mut d)?,
        deep: dec_summary(&mut d)?,
    })
}

/// Every record in this member's applied copy of the store.
pub fn load(db: &Db) -> Result<Records, String> {
    let txn = db.begin_read().map_err(|e| e.to_string())?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(Records::new());
    };
    let mut out = Records::new();
    for row in t.range(FIRST..END).map_err(|e| e.to_string())? {
        let (k, v) = row.map_err(|e| e.to_string())?;
        let Some(at) = parse_key(k.value()) else {
            continue;
        };
        // A record this build can't read is left for one that can.
        if let Ok(r) = decode(v.value()) {
            out.insert(at, r);
        }
    }
    Ok(out)
}

/// Writes records, and removes those given as `None`, in one write set.
/// Only the leader; it waits for the group to commit.
pub fn write(db: &Db, changes: &[((u32, u32), Option<PgScrubs>)]) -> Result<(), String> {
    db.write(Origin::Leader, |tx| -> Result<(), StoreError> {
        let mut t = tx.open_table(definition::<store::MapSchema>(Table::Settings))?;
        for ((pool, pg), r) in changes {
            match r {
                Some(r) => {
                    t.insert(key(*pool, *pg).as_str(), encode(r).as_slice())?;
                }
                None => {
                    t.remove(key(*pool, *pg).as_str())?;
                }
            }
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// PG names, the first ten, then how many more.
fn some_pgs(pgs: &[(u32, u32)]) -> String {
    let mut s: Vec<String> = pgs
        .iter()
        .take(10)
        .map(|(p, g)| format!("{p}.{g}"))
        .collect();
    if pgs.len() > 10 {
        s.push(format!("and {} more", pgs.len() - 10));
    }
    s.join(", ")
}

/// Health's scrub checks (28-scrub.md §31.5, D228) for each pool of `map`:
/// PGs overdue, PGs whose hole check was skipped twice running, and PGs
/// left damaged. `now` in Unix seconds.
pub fn health(map: &ClusterMap, records: &Records, now: u64) -> Vec<(Health, String)> {
    let mut out = Vec::new();
    for p in map.pools.values() {
        if p.kind == PoolKind::Cache {
            continue;
        }
        let (mut overdue, mut unchecked, mut damaged) = (Vec::new(), Vec::new(), Vec::new());
        for (&at, r) in records.range((p.id, 0)..=(p.id, u32::MAX)) {
            if !map.pgs.contains_key(&at) {
                continue;
            }
            if r.overdue(p, now).is_some() {
                overdue.push(at);
            }
            if r.unchecked() {
                unchecked.push(at);
            }
            if r.damaged() {
                damaged.push(at);
            }
        }
        if !damaged.is_empty() {
            out.push((
                Health::Error,
                format!(
                    "pool {}: {} PG(s) damaged, with no good copy to repair from: {}",
                    p.name,
                    damaged.len(),
                    some_pgs(&damaged)
                ),
            ));
        }
        if !overdue.is_empty() {
            out.push((
                Health::Warn,
                format!(
                    "pool {}: scrub overdue on {} PG(s): {}",
                    p.name,
                    overdue.len(),
                    some_pgs(&overdue)
                ),
            ));
        }
        if !unchecked.is_empty() {
            out.push((
                Health::Warn,
                format!(
                    "pool {}: holes unchecked in the last two scrubs of {} PG(s): {}",
                    p.name,
                    unchecked.len(),
                    some_pgs(&unchecked)
                ),
            ));
        }
    }
    out
}

/// What the leader writes for `map` next, given its records and which of
/// them changed: the changed ones, a new record (counted from `now`) for
/// each PG without one, and a removal for each record whose PG is gone.
/// Adds the new records to `records`.
pub fn changes(
    map: &ClusterMap,
    records: &mut Records,
    changed: &std::collections::BTreeSet<(u32, u32)>,
    now: u64,
) -> Changes {
    let mut out = Vec::new();
    for at in map.pgs.keys() {
        if !records.contains_key(at) {
            records.insert(*at, PgScrubs::new(now));
            out.push((*at, records.get(at).cloned()));
        } else if changed.contains(at) {
            out.push((*at, records.get(at).cloned()));
        }
    }
    let gone: Vec<(u32, u32)> = records
        .keys()
        .filter(|at| !map.pgs.contains_key(at))
        .copied()
        .collect();
    for at in gone {
        records.remove(&at);
        out.push((at, None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScrubSettings;
    use std::sync::Arc;

    fn summary(at: u64) -> Summary {
        Summary {
            at,
            compared: 10,
            ..Default::default()
        }
    }

    /// Pool 1, "vol", of 16 PGs, scrubbed every `hours`.
    fn map(hours: u16) -> ClusterMap {
        let mut m = crate::parse_cluster(
            "cluster 9\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             node 2 rack 0 127.0.0.1:9102\npool 1 vol replicated size 3 min 2 pgs 16 domain host\n",
        )
        .unwrap();
        // Without `.meta`, which every cluster has.
        m.pools.retain(|id, _| *id == 1);
        m.pgs.retain(|at, _| at.0 == 1);
        m.pools.get_mut(&1).unwrap().scrub = ScrubSettings {
            hours,
            ..Default::default()
        };
        m
    }

    fn pool(hours: u16) -> PoolInfo {
        map(hours).pools[&1].clone()
    }

    #[test]
    fn records_round_trip_and_survive_in_the_store() {
        let mut r = PgScrubs::new(100);
        r.note(summary(200), false);
        let mut deep = summary(150);
        deep.damaged = 1;
        deep.holes_skipped = "device 2 can't be reached".into();
        r.note(deep, true);
        assert_eq!(decode(&encode(&r)).unwrap(), r);
        assert!(decode(&[2]).is_err(), "an unknown format");

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("map.redb")).unwrap());
        assert!(load(&db).unwrap().is_empty());
        // The settings' other rows are left alone and not read as records.
        let settings = crate::settings::Settings::new(db.clone());
        settings.set_audit_days(30).unwrap();
        write(
            &db,
            &[((1, 2), Some(r.clone())), ((1, 3), Some(PgScrubs::new(5)))],
        )
        .unwrap();
        let got = load(&db).unwrap();
        assert_eq!(got.get(&(1, 2)), Some(&r));
        assert_eq!(got.len(), 2);
        write(&db, &[((1, 3), None)]).unwrap();
        assert_eq!(load(&db).unwrap().len(), 1);
        assert_eq!(settings.audit_days().unwrap(), 30);
    }

    #[test]
    fn overdue_unchecked_and_damaged() {
        let day = 86_400;
        let p = pool(24);
        // Counted from when it was made: not overdue before twice the
        // interval has passed.
        let r = PgScrubs::new(1_000_000);
        assert_eq!(r.overdue(&p, 1_000_000 + 2 * day), None);
        assert_eq!(r.overdue(&p, 1_000_000 + 2 * day + 1), Some(Overdue::Light));
        // A light scrub a day ago, the last deep one 15 days ago: the deep
        // interval is 7 days, so it is overdue at 14.
        let mut r = PgScrubs::new(0);
        let now = 100 * day;
        r.note(summary(now - day), false);
        r.note(summary(now - 13 * day), true);
        assert_eq!(r.overdue(&p, now), None);
        r.note(summary(now - 15 * day), true);
        assert_eq!(r.overdue(&p, now), Some(Overdue::Deep));
        assert_eq!(r.light_at(), now - day);
        // A deep scrub counts as a light one.
        r.note(summary(now), true);
        assert_eq!(r.light_at(), now);
        assert_eq!(r.overdue(&p, now + 2 * day + 1), Some(Overdue::Light));
        // Scrub off: warned of as reduced, not overdue.
        assert_eq!(PgScrubs::new(0).overdue(&pool(0), now), None);

        let mut skipped = summary(now);
        skipped.holes_skipped = "a member is from before Holds".into();
        r.note(skipped.clone(), false);
        assert!(!r.unchecked());
        r.note(skipped, false);
        assert!(r.unchecked());
        r.note(summary(now), false);
        assert!(!r.unchecked(), "a scrub that checked clears it");

        let mut bad = summary(now + 1);
        bad.damaged = 2;
        r.note(bad, true);
        assert!(r.damaged());
        r.note(summary(now + 2), false);
        assert!(!r.damaged(), "a clean scrub after it clears it");
    }

    #[test]
    fn health_names_pools_and_pgs_and_records_follow_the_map() {
        let day = 86_400;
        let mut map = map(24);
        let mut records = Records::new();
        let all = changes(&map, &mut records, &Default::default(), 0);
        assert_eq!(all.len(), 16, "a record for each PG, counted from now");
        assert!(health(&map, &records, 2 * day).is_empty());
        let h = health(&map, &records, 3 * day);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, Health::Warn);
        assert!(
            h[0].1
                .starts_with("pool vol: scrub overdue on 16 PG(s): 1.0, 1.1,")
                && h[0].1.ends_with("1.9, and 6 more"),
            "{}",
            h[0].1
        );
        let mut bad = summary(3 * day);
        bad.damaged = 1;
        records.get_mut(&(1, 4)).unwrap().note(bad, true);
        let h = health(&map, &records, 3 * day);
        assert!(h.iter().any(|(l, w)| *l == Health::Error
            && w == "pool vol: 1 PG(s) damaged, with no good copy to repair from: 1.4"));

        // Only what changed is written again; records of PGs gone go.
        map.pgs.remove(&(1, 15));
        let changed = [(1, 4)].into_iter().collect();
        let c = changes(&map, &mut records, &changed, 5);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, (1, 4));
        assert_eq!(c[1], ((1, 15), None));
        assert!(!records.contains_key(&(1, 15)));
    }
}
