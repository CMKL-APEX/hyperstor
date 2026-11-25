//! The cluster's sealing keys (22-nfs.md §26.7, 29-hsfs.md §32.6;
//! 04-decisions.md D133, D363), rows of the replicated store's settings
//! table, as S3's keys are (D231), so a member from before them still
//! applies every entry:
//!
//! - `sealkey/<seq>`: a key's id, when and by whom it was made, and its
//!   32 bytes sealed with the cluster key (D162), bound to its sequence
//!   number and id. `<seq>` is 20 digits, so rows sort in the order the
//!   keys were made.
//!
//! The newest [`KEEP`] rows are kept: the newest key seals, and the one
//! before it still opens, so a rotation invalidates nothing at once. A
//! key's id is one byte (an NFS stateid has room for no more), taken in
//! turn from 1 to 255, so an id returns only after 255 rotations, long
//! after its last key was dropped.
//!
//! Writes go through the leader; reads come from the member's applied
//! copy, so any member answers a storage node.

use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable as _;

const KEY: &str = "sealkey/";

/// How many keys are valid at once: the one that seals and the one before.
pub const KEEP: usize = 2;

/// A sealing key as stored: its bytes sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealKeyRow {
    /// Which key this is of all the cluster ever made, from 1.
    pub seq: u64,
    /// What sealed statements name it by.
    pub id: u8,
    pub created_unix: u64,
    pub created_by: String,
    /// The key, sealed by [`hyperstor_pki::ClusterKey::seal`] for
    /// [`context`].
    pub sealed: Vec<u8>,
}

impl SealKeyRow {
    fn row(seq: u64) -> String {
        format!("{KEY}{seq:020}")
    }

    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(self.id)
            .u64(self.created_unix)
            .str(&self.created_by)
            .bytes(&self.sealed);
        e.into_vec()
    }

    fn decode(seq: u64, b: &[u8]) -> Result<SealKeyRow, DecodeError> {
        let mut d = Dec::new(b);
        let k = SealKeyRow {
            seq,
            id: d.u8()?,
            created_unix: d.u64()?,
            created_by: d.str()?.to_string(),
            sealed: d.bytes()?.to_vec(),
        };
        d.finish()?;
        Ok(k)
    }
}

/// The id of the `seq`th key: 1 to 255 in turn, never 0.
pub fn id_of(seq: u64) -> u8 {
    ((seq.max(1) - 1) % 255) as u8 + 1
}

/// What a key's bytes are sealed for: its sequence number and id, so a
/// sealed key copied to another row doesn't open.
pub fn context(seq: u64, id: u8) -> String {
    format!("hyperstor sealing key {seq} id {id}")
}

fn bad(row: &str, e: impl std::fmt::Display) -> Error {
    Error::Store(format!("{row}: {e}"))
}

/// Every key kept, oldest first, from this member's copy.
pub fn keys(db: &Db) -> Result<Vec<SealKeyRow>, Error> {
    let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in t.range(KEY..).map_err(|e| Error::Store(e.to_string()))? {
        let (k, v) = row.map_err(|e| Error::Store(e.to_string()))?;
        let Some(seq) = k.value().strip_prefix(KEY) else {
            break;
        };
        let n: u64 = seq.parse().map_err(|e| bad(k.value(), e))?;
        out.push(SealKeyRow::decode(n, v.value()).map_err(|e| bad(k.value(), e))?);
    }
    Ok(out)
}

/// Adds `k` as the newest key and drops all but the newest [`KEEP`];
/// the keys kept, oldest first. Only the leader.
///
/// `k.seq` must be one more than the newest key's (1 for the first), and
/// its id that number's: whoever made `k` read the keys first, and if
/// another rotation got in between, this one is refused rather than
/// leaving the cluster with two keys neither caller knows are both there.
pub fn add(db: &Db, k: &SealKeyRow) -> Result<Vec<SealKeyRow>, Error> {
    db.write(Origin::Leader, |tx| -> Result<Vec<SealKeyRow>, Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        let mut have = Vec::new();
        for row in t.range(KEY..).map_err(StoreError::from)? {
            let (key, v) = row.map_err(StoreError::from)?;
            let Some(seq) = key.value().strip_prefix(KEY) else {
                break;
            };
            let n: u64 = seq.parse().map_err(|e| bad(key.value(), e))?;
            have.push(SealKeyRow::decode(n, v.value()).map_err(|e| bad(key.value(), e))?);
        }
        let next = have.last().map_or(1, |l| l.seq + 1);
        if k.seq != next || k.id != id_of(next) {
            return Err(Error::Store(format!(
                "the sealing keys changed meanwhile: the next is {next}, not {}",
                k.seq
            )));
        }
        t.insert(SealKeyRow::row(k.seq).as_str(), k.encode().as_slice())
            .map_err(StoreError::from)?;
        have.push(k.clone());
        let drop = have.len().saturating_sub(KEEP);
        for old in have.drain(..drop) {
            t.remove(SealKeyRow::row(old.seq).as_str())
                .map_err(StoreError::from)?;
        }
        Ok(have)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seq: u64) -> SealKeyRow {
        SealKeyRow {
            seq,
            id: id_of(seq),
            created_unix: 1_700_000_000 + seq,
            created_by: "hyperstor://9/admin/local".into(),
            sealed: vec![seq as u8; 60],
        }
    }

    #[test]
    fn the_newest_two_keys_are_kept_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        assert!(keys(&db).unwrap().is_empty());
        assert_eq!(add(&db, &key(1)).unwrap(), vec![key(1)]);
        assert_eq!(add(&db, &key(2)).unwrap(), vec![key(1), key(2)]);
        assert_eq!(add(&db, &key(3)).unwrap(), vec![key(2), key(3)]);
        assert_eq!(keys(&db).unwrap(), vec![key(2), key(3)]);
        // Rows sort by sequence past a digit's turn.
        for seq in 4..=12 {
            add(&db, &key(seq)).unwrap();
        }
        assert_eq!(keys(&db).unwrap(), vec![key(11), key(12)]);
    }

    #[test]
    fn a_rotation_that_lost_a_race_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        // Not the first.
        assert!(add(&db, &key(2)).is_err());
        add(&db, &key(1)).unwrap();
        // Two callers read key 1 and both made key 2: one wins.
        add(&db, &key(2)).unwrap();
        assert!(add(&db, &key(2)).is_err());
        // Nor a key with another's id.
        let mut wrong = key(3);
        wrong.id = 9;
        assert!(add(&db, &wrong).is_err());
        assert_eq!(keys(&db).unwrap(), vec![key(1), key(2)]);
    }

    #[test]
    fn ids_turn_from_1_to_255_and_never_0() {
        assert_eq!((id_of(1), id_of(2), id_of(255)), (1, 2, 255));
        assert_eq!((id_of(256), id_of(510), id_of(511)), (1, 255, 1));
        assert_eq!(id_of(0), 1);
        assert!((1..2_000).all(|s| id_of(s) != 0));
        // Two keys valid at once never share an id.
        assert!((1..2_000).all(|s| id_of(s) != id_of(s + 1)));
        assert_ne!(context(1, 1), context(256, 1));
        assert_ne!(context(1, 1), context(1, 2));
    }

    #[test]
    fn s3_rows_beside_them_are_not_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        add(&db, &key(1)).unwrap();
        crate::s3keys::set_placement(
            &db,
            &crate::s3keys::Placement {
                name: "default".into(),
                data_pool: 1,
                index_pool: 3,
                realm: 0,
            },
        )
        .unwrap();
        // "sealkey0…" would sort after "sealkey/…": a prefix's neighbours.
        db.write(Origin::Leader, |tx| -> Result<(), Error> {
            let mut t = tx
                .open_table(definition::<store::MapSchema>(Table::Settings))
                .map_err(StoreError::from)?;
            t.insert("sealkez", &[1u8][..]).map_err(StoreError::from)?;
            t.insert("sealke", &[1u8][..]).map_err(StoreError::from)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(keys(&db).unwrap(), vec![key(1)]);
        assert_eq!(add(&db, &key(2)).unwrap().len(), 2);
    }
}
