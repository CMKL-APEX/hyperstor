//! Backups of the map service's replicated store (decisions.md D86): the
//! map, join tokens, issued certificates and revocations; not Raft's own
//! bookkeeping.
//!
//! - **Written by the leader** to `--backup-dir` when the store has
//!   changed, at most every `--backup-every` seconds, keeping the last
//!   `--backup-keep`. Files are `map-<unix time>-e<epoch>.hsbackup`, mode
//!   0600, written beside and renamed.
//! - **Fetched** through the API (`MapService.GetMapBackup`,
//!   `hyperstorctl map backup`).
//! - **Restored** by `hyperstor-meta --restore FILE` into an empty data
//!   directory, as a group of one: its first commit writes every row of
//!   the backup in one write set; other members join afterwards (D87).
//!
//! A backup is `HSBACK01`, then the cluster id, the map's epoch, when it
//! was written, and each table's rows (codec.md's encoding), then a
//! CRC32C of all that. The map holds no secret (tokens are hashes).

use crate::store::{Db, MapSchema, Table};
use glidex_raftstore::store::{Schema, StoreError};
use hyperstor_codec::{Dec, Enc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAGIC: &[u8; 8] = b"HSBACK01";

/// One table's rows: key, value.
pub type Rows = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone, PartialEq)]
pub struct Backup {
    pub cluster_id: u128,
    pub epoch: u64,
    pub written_unix: u64,
    /// By table id (store.rs's, which are fixed).
    pub tables: Vec<(u16, Rows)>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Backup {
    /// The store's replicated tables now. Fails if it holds no map yet.
    pub fn take(db: &Db) -> Result<Backup, String> {
        let map = crate::store::load(db)?.ok_or("the store holds no map yet")?;
        let tables = db
            .dump()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(t, rows)| (MapSchema::table_id(t), rows))
            .collect();
        Ok(Backup {
            cluster_id: map.cluster_id,
            epoch: map.epoch,
            written_unix: now_unix(),
            tables,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.raw(MAGIC)
            .u128(self.cluster_id)
            .u64(self.epoch)
            .u64(self.written_unix)
            .seq(&self.tables, |e, (id, rows)| {
                e.u16(*id).seq(rows, |e, (k, v)| {
                    e.str(k).bytes(v);
                });
            });
        let crc = hyperstor_codec::crc32c(e.as_slice());
        e.u32(crc);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Backup, String> {
        if b.len() < MAGIC.len() + 4 || &b[..MAGIC.len()] != MAGIC {
            return Err("not a hyperstor map backup".into());
        }
        let (body, crc) = b.split_at(b.len() - 4);
        let crc = u32::from_le_bytes(crc.try_into().expect("4 bytes"));
        if hyperstor_codec::crc32c(body) != crc {
            return Err("the backup is damaged (checksum)".into());
        }
        let mut d = Dec::new(&body[MAGIC.len()..]);
        let bad = |e: hyperstor_codec::DecodeError| format!("backup: {e}");
        let cluster_id = d.u128().map_err(bad)?;
        let epoch = d.u64().map_err(bad)?;
        let written_unix = d.u64().map_err(bad)?;
        let tables = d
            .seq(2, |d| {
                let id = d.u16()?;
                let rows = d.seq(2, |d| Ok((d.str()?.to_string(), d.bytes()?.to_vec())))?;
                Ok((id, rows))
            })
            .map_err(bad)?;
        d.finish().map_err(bad)?;
        Ok(Backup {
            cluster_id,
            epoch,
            written_unix,
            tables,
        })
    }

    /// The map in the backup.
    pub fn map(&self) -> Result<crate::ClusterMap, String> {
        let rows = self
            .tables
            .iter()
            .find(|(id, _)| *id == MapSchema::table_id(Table::Map))
            .map(|(_, r)| r.as_slice())
            .ok_or("the backup has no map")?;
        crate::store::map_from_rows(rows)?.ok_or_else(|| "the backup has no map".into())
    }

    /// Writes every row of the backup into `db` in one write (on a group's
    /// leader, one Raft entry). Tables this version doesn't know are
    /// refused rather than dropped.
    pub fn restore(&self, db: &Db) -> Result<(), String> {
        let mut tables = Vec::new();
        for (id, rows) in &self.tables {
            let t = MapSchema::table_from_id(*id)
                .ok_or_else(|| format!("the backup has table {id}, which this version lacks"))?;
            tables.push((t, rows));
        }
        db.write(
            crate::store::Origin::Leader,
            |tx| -> Result<(), StoreError> {
                for (t, rows) in &tables {
                    let mut tb =
                        tx.open_table(glidex_raftstore::store::definition::<MapSchema>(*t))?;
                    for (k, v) in rows.iter() {
                        tb.insert(k.as_str(), v.as_slice())?;
                    }
                }
                Ok(())
            },
        )
        .map_err(|e| e.to_string())
    }
}

/// Writes `b` to `dir` as `map-<unix>-e<epoch>.hsbackup` (mode 0600,
/// beside then renamed) and removes all but the newest `keep`.
pub fn write_file(dir: &Path, b: &Backup, keep: usize) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = format!("map-{}-e{}.hsbackup", b.written_unix, b.epoch);
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.new"));
    let io = |e: std::io::Error| format!("{}: {e}", tmp.display());
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io)?;
    f.write_all(&b.encode()).map_err(io)?;
    f.sync_all().map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut all: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("map-") && n.ends_with(".hsbackup"))
        })
        .collect();
    // Names sort by time: the unix time has the same number of digits
    // until the year 2286.
    all.sort();
    let n = all.len();
    for old in all.into_iter().take(n.saturating_sub(keep)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(path)
}

/// Where and how often the leader writes backups.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub dir: PathBuf,
    /// At most one backup per this much time.
    pub every: Duration,
    pub keep: usize,
}

/// Writes a backup whenever the store changed and `leads()` says this
/// member leads, at most once per `s.every`. Runs until dropped.
pub async fn writer(db: Arc<Db>, s: Schedule, leads: impl Fn() -> bool) {
    let mut last_revision = None;
    let mut last_written: Option<std::time::Instant> = None;
    loop {
        let wait = match last_written {
            Some(t) => s.every.saturating_sub(t.elapsed()),
            None => Duration::ZERO,
        }
        .max(Duration::from_secs(1));
        tokio::time::sleep(wait).await;
        let rev = db.revision();
        if !leads() || last_revision == Some(rev) {
            continue;
        }
        let (db2, dir, keep) = (db.clone(), s.dir.clone(), s.keep);
        let r = tokio::task::spawn_blocking(move || {
            let b = Backup::take(&db2)?;
            write_file(&dir, &b, keep)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        match r {
            Ok(_) => {
                last_revision = Some(rev);
                last_written = Some(std::time::Instant::now());
            }
            // No map yet is normal right after start; anything else is
            // worth the journal.
            Err(e) if e.contains("no map yet") => {}
            Err(e) => eprintln!("hyperstor-meta: backup: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_map() -> (tempfile::TempDir, Arc<Db>, crate::ClusterMap) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("map.redb")).unwrap());
        let m = crate::parse_cluster(
            "cluster 77\nnode 0 rack 0 127.0.0.1:7000\npool 1 v replicated size 1 min 1 pgs 8\n",
        )
        .unwrap();
        crate::store::save(&db, &m).unwrap();
        (dir, db, m)
    }

    /// A backup carries every replicated table, and restores into an
    /// empty store as it was; a damaged one is refused.
    #[test]
    fn backups_round_trip_and_restore() {
        let (_d, db, m) = store_with_map();
        let tokens = crate::tokens::Tokens::replicated(db.clone(), None);
        tokens
            .create(
                crate::tokens::Record {
                    hash: "ab".repeat(32),
                    id: "abababababab".into(),
                    role: "node".into(),
                    expires_unix: u64::MAX / 2,
                    used: false,
                    revoked: false,
                    created_by: "t".into(),
                    name: String::new(),
                },
                1,
            )
            .unwrap();
        let b = Backup::take(&db).unwrap();
        assert_eq!((b.cluster_id, b.epoch), (77, m.epoch));
        let bytes = b.encode();
        let back = Backup::decode(&bytes).unwrap();
        assert_eq!(back, b);
        assert_eq!(back.map().unwrap(), m);
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 1;
        assert!(Backup::decode(&bad).unwrap_err().contains("checksum"));
        assert!(Backup::decode(b"nonsense").is_err());
        // Into an empty store: the map and the token are back.
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = Arc::new(Db::create(dir2.path().join("map.redb")).unwrap());
        back.restore(&db2).unwrap();
        assert_eq!(crate::store::load(&db2).unwrap().unwrap(), m);
        let t2 = crate::tokens::Tokens::replicated(db2, None);
        assert_eq!(t2.list().unwrap().len(), 1);
    }

    #[test]
    fn files_are_private_and_the_newest_kept() {
        let (_d, db, _) = store_with_map();
        let out = tempfile::tempdir().unwrap();
        let mut b = Backup::take(&db).unwrap();
        for t in 0..5 {
            b.written_unix = 1_700_000_000 + t;
            write_file(out.path(), &b, 3).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(out.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names[0].starts_with("map-1700000002-e"), "{names:?}");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(out.path().join(&names[2]))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
