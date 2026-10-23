//! Settings the map service keeps for the cluster, in the replicated store
//! (`Table::Settings`): for now each pool's trash (decisions.md D54,
//! D115). Not in the map: only the map service needs them. Writes go
//! through the leader; reads come from the member's applied copy.

use crate::store::{self, Db, Origin, Table};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{StoreError, definition};
use std::sync::Arc;

/// How long a deleted volume or snapshot stays in the trash, by default
/// (D54).
pub const DEFAULT_GRACE_SECS: u64 = 24 * 3600;

/// What a full pool does with its trash (D115; acts with D27).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFull {
    /// Keep it: never lose data an operator may still want.
    #[default]
    Keep,
    /// Purge the oldest first, so writes keep flowing.
    Purge,
}

impl OnFull {
    pub fn as_str(self) -> &'static str {
        match self {
            OnFull::Keep => "keep",
            OnFull::Purge => "purge",
        }
    }

    pub fn parse(s: &str) -> Option<OnFull> {
        match s {
            "keep" => Some(OnFull::Keep),
            "purge" => Some(OnFull::Purge),
            _ => None,
        }
    }
}

/// A pool's trash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trash {
    /// Seconds before a trashed item is purged; 0: at once.
    pub grace_secs: u64,
    pub on_full: OnFull,
}

impl Default for Trash {
    fn default() -> Self {
        Trash {
            grace_secs: DEFAULT_GRACE_SECS,
            on_full: OnFull::Keep,
        }
    }
}

pub struct Settings(Arc<Db>);

fn key(pool: u32) -> String {
    format!("pool/{pool}/trash")
}

impl Settings {
    pub fn new(db: Arc<Db>) -> Settings {
        Settings(db)
    }

    /// Pool `pool`'s trash settings (the defaults if never set).
    pub fn trash(&self, pool: u32) -> Result<Trash, Error> {
        let txn = self
            .0
            .begin_read()
            .map_err(|e| Error::Store(e.to_string()))?;
        let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
            return Ok(Trash::default());
        };
        let Some(v) = t
            .get(key(pool).as_str())
            .map_err(|e| Error::Store(e.to_string()))?
        else {
            return Ok(Trash::default());
        };
        // `<grace secs> <keep|purge>`.
        let text = String::from_utf8_lossy(v.value()).into_owned();
        let mut w = text.split_whitespace();
        let grace_secs = w.next().and_then(|g| g.parse().ok());
        let on_full = w.next().and_then(OnFull::parse);
        match (grace_secs, on_full) {
            (Some(grace_secs), Some(on_full)) => Ok(Trash {
                grace_secs,
                on_full,
            }),
            _ => Err(Error::Store(format!("{}: not trash settings", key(pool)))),
        }
    }

    /// Sets pool `pool`'s trash. Only the leader.
    pub fn set_trash(&self, pool: u32, t: Trash) -> Result<(), Error> {
        let value = format!("{} {}", t.grace_secs, t.on_full.as_str());
        self.0.write(Origin::Leader, |tx| -> Result<(), Error> {
            let mut tb = tx
                .open_table(definition::<store::MapSchema>(Table::Settings))
                .map_err(StoreError::from)?;
            tb.insert(key(pool).as_str(), value.as_bytes())
                .map_err(StoreError::from)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_until_set_then_what_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::new(Arc::new(Db::create(dir.path().join("map.redb")).unwrap()));
        assert_eq!(s.trash(1).unwrap(), Trash::default());
        assert_eq!(Trash::default().grace_secs, 86400);
        let t = Trash {
            grace_secs: 0,
            on_full: OnFull::Purge,
        };
        s.set_trash(1, t).unwrap();
        assert_eq!(s.trash(1).unwrap(), t);
        assert_eq!(s.trash(2).unwrap(), Trash::default(), "per pool");
        assert_eq!(OnFull::parse("keep"), Some(OnFull::Keep));
        assert_eq!(OnFull::parse("drop"), None);
    }
}
