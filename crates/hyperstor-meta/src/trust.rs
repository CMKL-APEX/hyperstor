//! Trust in glidex directories (19-administration.md §23.12, 04-decisions.md
//! D306): the records a cluster keeps of the directories it trusts, rows of
//! the replicated store's settings table, as S3's keys are
//! (`crate::s3keys`), each a JSON value:
//!
//! - `trust/key`: the cluster key towards directories (glidex D63), an
//!   ES256 key sealed with the cluster key (D162); made on first use.
//! - `trust/offer/<id>`: an offer to a directory's tenant (glidex D64): the
//!   hash of its secret, never the secret, and whether it is still open.
//! - `trust/dir/<trusted id>`: a directory that redeemed an offer (glidex
//!   D65), its tenants admitted here, and the realm it maps to
//!   (20-tenants.md §24.9, D299).
//! - `trust/feed/<trusted id>`: what was last applied from its feed (glidex
//!   D68), and what of it couldn't be, retried at every pull.
//! - `trust/row/<trusted id>/<table>/<key>`: the feed's rows, the replica
//!   every mgmt reads (stage 5's logins and grants).
//! - `trust/ending/<trusted id>/<glidex tenant>`: an end decided here that
//!   the directory hasn't been told of yet (glidex D66).
//! - `trust/operator`: the operator tenant (glidex §12.1), a tenant id.
//!
//! [`plan`] is what the feed asks of the tenants and projects here: pure,
//! so the API's run of it and the model test share it. [`person_of`] is
//! who a directory's person is here, from the replica (D307, D308): pure
//! too, for the same reasons.

use crate::store::{self, Db, Origin, Table};
use crate::tenancy::{ProjectState, Tenancy};
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{RecTable, StoreError, definition};
use glidex_trust::{FeedState, RemoteTenant, Row, TrustedDirectory};
use redb::ReadableTable as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const KEY: &str = "trust/key";
const OFFER: &str = "trust/offer/";
const DIR: &str = "trust/dir/";
const FEED: &str = "trust/feed/";
const ROW: &str = "trust/row/";
const ENDING: &str = "trust/ending/";
const OPERATOR: &str = "trust/operator";

/// The cluster key towards directories, sealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRec {
    pub kid: String,
    /// Its PKCS#8 DER, sealed by [`hyperstor_pki::ClusterKey::seal`] for
    /// [`key_context`].
    pub sealed: Vec<u8>,
}

/// What the cluster key towards directories is sealed for.
pub fn key_context(kid: &str) -> String {
    format!("hyperstor trust key {kid}")
}

/// An offer's state.
pub const OPEN: &str = "open";
pub const REDEEMED: &str = "redeemed";
pub const REVOKED: &str = "revoked";

/// An offer to one of a directory's tenants (glidex D64).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferRec {
    pub id: String,
    /// The glidex tenant's slug, and its directory's issuer (normalized).
    pub tenant: String,
    pub directory: String,
    /// The tenant's name here, and the realm its directory maps to.
    pub name: String,
    pub realm: String,
    /// Where the directory reaches mgmt, and its certificate's SHA-256.
    pub url: String,
    pub pin: String,
    /// The SHA-256 (hex) of its secret.
    pub secret_hash: String,
    pub created_unix: u64,
    pub created_by: String,
    pub expires_unix: u64,
    /// [`OPEN`], [`REDEEMED`] or [`REVOKED`].
    pub state: String,
    /// Where people's browsers reach mgmt (`https://host[:port]`): the
    /// directory sends their logins back there; empty: `url`.
    #[serde(default)]
    pub ui: String,
}

impl OfferRec {
    /// What a redeem may use it for.
    pub fn open_at(&self, now: u64) -> bool {
        self.state == OPEN && now < self.expires_unix
    }

    /// Its state as listed: "expired" once past, if still open.
    pub fn shown_state(&self, now: u64) -> &str {
        if self.state == OPEN && now >= self.expires_unix {
            "expired"
        } else {
            &self.state
        }
    }
}

/// A directory trusted here: glidex's record of it, and its realm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirRec {
    pub dir: TrustedDirectory,
    /// The realm it maps to (D299); never 0, "default".
    pub realm: u32,
    /// Where it sends people's logins back (glidex D69, D73): mgmt's
    /// [`LOGIN_CALLBACK`] at the last redeemed offer's `ui`.
    #[serde(default)]
    pub ui_callback: String,
}

/// mgmt's path that directories send people's browser logins back to.
pub const LOGIN_CALLBACK: &str = "/auth/directory/callback";

/// The callback a directory sends logins back to, for mgmt at `ui`.
pub fn callback_for(ui: &str) -> String {
    format!("{}{LOGIN_CALLBACK}", ui.trim_end_matches('/'))
}

/// What was last applied from a directory's feed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedRec {
    pub state: FeedState,
    /// What the last run couldn't do, each retried at the next.
    #[serde(default)]
    pub blocked: Vec<String>,
    /// The login chains the directory ended (its last feed's
    /// `ended_sids`, glidex D68): mgmt ends the sessions it made from
    /// them.
    #[serde(default)]
    pub ended: Vec<String>,
}

/// An end decided here, for the directory to be told of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndingRec {
    pub dir: TrustedDirectory,
    /// The glidex tenant's id.
    pub tenant: String,
}

fn json<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).expect("a record serializes")
}

fn parse<T: DeserializeOwned>(row: &str, v: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(v).map_err(|e| Error::Store(format!("{row}: {e}")))
}

fn refused(reason: &'static str, why: String) -> Error {
    Error::Refused { reason, why }
}

fn rows(
    t: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut out = Vec::new();
    for row in t.range(prefix..).map_err(StoreError::from)? {
        let (k, v) = row.map_err(StoreError::from)?;
        let Some(rest) = k.value().strip_prefix(prefix) else {
            break;
        };
        out.push((rest.to_string(), v.value().to_vec()));
    }
    Ok(out)
}

fn read<T>(
    db: &Db,
    f: impl FnOnce(&redb::ReadOnlyTable<&'static str, &'static [u8]>) -> Result<T, Error>,
    none: T,
) -> Result<T, Error> {
    let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(none);
    };
    f(&t)
}

fn write<T>(
    db: &Db,
    f: impl FnOnce(&mut RecTable<'_, store::MapSchema>) -> Result<T, Error>,
) -> Result<T, Error> {
    db.write(Origin::Leader, |tx| -> Result<T, Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        f(&mut t)
    })
}

fn put(t: &mut RecTable<'_, store::MapSchema>, key: &str, v: &[u8]) -> Result<(), Error> {
    t.insert(key, v).map_err(StoreError::from)?;
    Ok(())
}

fn get_in<T: DeserializeOwned>(
    t: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<T>, Error> {
    match t.get(key).map_err(StoreError::from)? {
        None => Ok(None),
        Some(v) => parse(key, v.value()).map(Some),
    }
}

/// The cluster key towards directories, if made.
pub fn key(db: &Db) -> Result<Option<KeyRec>, Error> {
    read(db, |t| get_in(t, KEY), None)
}

/// Keeps `k` as the cluster key towards directories unless one is kept
/// already; the one kept. Only the leader.
pub fn put_key(db: &Db, k: &KeyRec) -> Result<KeyRec, Error> {
    write(db, |t| {
        if let Some(old) = get_in::<KeyRec>(&**t, KEY)? {
            return Ok(old);
        }
        put(t, KEY, &json(k))?;
        Ok(k.clone())
    })
}

/// Every offer, from this member's copy.
pub fn offers(db: &Db) -> Result<Vec<OfferRec>, Error> {
    read(
        db,
        |t| {
            rows(t, OFFER)?
                .into_iter()
                .map(|(id, v)| parse(&format!("{OFFER}{id}"), &v))
                .collect()
        },
        Vec::new(),
    )
}

/// Adds offer `o`. Only the leader.
pub fn create_offer(db: &Db, o: &OfferRec) -> Result<(), Error> {
    let row = format!("{OFFER}{}", o.id);
    write(db, |t| {
        if t.get(row.as_str()).map_err(StoreError::from)?.is_some() {
            return Err(Error::Store(format!("offer {} exists", o.id)));
        }
        put(t, &row, &json(o))
    })
}

/// Revokes open offer `id`. Only the leader.
pub fn revoke_offer(db: &Db, id: &str) -> Result<(), Error> {
    let row = format!("{OFFER}{id}");
    write(db, |t| {
        let Some(mut o) = get_in::<OfferRec>(&**t, &row)? else {
            return Err(Error::NotFound);
        };
        if o.state != OPEN {
            return Err(refused(
                "offer_closed",
                format!("offer {id} is {} already", o.state),
            ));
        }
        o.state = REVOKED.into();
        put(t, &row, &json(&o))
    })
}

/// Spends offer `id` for a redeem at `now`, before anything else is
/// written (glidex D65): it is open no more, whatever comes after. The
/// offer as it was; `None` if there is none.
pub fn spend_offer(db: &Db, id: &str, now: u64) -> Result<Option<OfferRec>, Error> {
    let row = format!("{OFFER}{id}");
    write(db, |t| {
        let Some(o) = get_in::<OfferRec>(&**t, &row)? else {
            return Ok(None);
        };
        if o.open_at(now) {
            let spent = OfferRec {
                state: REDEEMED.into(),
                ..o.clone()
            };
            put(t, &row, &json(&spent))?;
        }
        Ok(Some(o))
    })
}

/// Every directory trusted, by id.
pub fn directories(db: &Db) -> Result<Vec<DirRec>, Error> {
    read(
        db,
        |t| {
            rows(t, DIR)?
                .into_iter()
                .map(|(id, v)| parse(&format!("{DIR}{id}"), &v))
                .collect()
        },
        Vec::new(),
    )
}

/// The directory trusted with this issuer, if any.
pub fn directory_of(db: &Db, issuer: &str) -> Result<Option<DirRec>, Error> {
    Ok(directories(db)?
        .into_iter()
        .find(|d| d.dir.issuer == issuer))
}

/// Keeps `d`, a directory as a redeem left it. Only the leader.
pub fn put_directory(db: &Db, d: &DirRec) -> Result<(), Error> {
    write(db, |t| put(t, &format!("{DIR}{}", d.dir.id), &json(d)))
}

/// A directory's feed record, default if none.
pub fn feed(db: &Db, id: &str) -> Result<FeedRec, Error> {
    read(
        db,
        |t| Ok(get_in(t, &format!("{FEED}{id}"))?.unwrap_or_default()),
        FeedRec::default(),
    )
}

/// The replica's rows of directory `id`, table `table`.
pub fn replica(db: &Db, id: &str, table: &str) -> Result<Vec<Row>, Error> {
    let prefix = format!("{ROW}{id}/{table}/");
    read(
        db,
        |t| {
            rows(t, &prefix)?
                .into_iter()
                .map(|(key, v)| {
                    Ok(Row {
                        table: table.to_string(),
                        key,
                        value: parse(&prefix, &v)?,
                    })
                })
                .collect()
        },
        Vec::new(),
    )
}

/// The rows of `snap` this cluster keeps of `d` (glidex D68): of every
/// table it knows, those about tenants admitted here only where a row
/// names its tenant.
pub fn admitted_rows(d: &TrustedDirectory, snap: &glidex_trust::Snapshot) -> Vec<Row> {
    let admitted: BTreeSet<&str> = d.tenants.iter().map(|t| t.id.as_str()).collect();
    let tenant_of = |r: &Row| -> Option<String> {
        match r.table.as_str() {
            "tenants" => r.value.get("id")?.as_str().map(str::to_string),
            "projects" | "tenant_members" => r.value.get("tenant_id")?.as_str().map(str::to_string),
            _ => None,
        }
    };
    snap.rows
        .iter()
        .filter(|r| glidex_trust::FEED_TABLES.contains(&r.table.as_str()))
        .filter(|r| match r.table.as_str() {
            "tenants" | "projects" | "tenant_members" => {
                tenant_of(r).is_some_and(|t| admitted.contains(t.as_str()))
            }
            _ => true,
        })
        .cloned()
        .collect()
}

/// Applies directory `id`'s feed at `now`: with `snap`, its rows become the
/// replica (those it no longer has removed) and its keys the directory's;
/// without, it only answered (unchanged). In one write. `false` if the
/// directory isn't trusted (any more).
pub fn apply_feed(
    db: &Db,
    id: &str,
    snap: Option<&glidex_trust::Snapshot>,
    now: u64,
) -> Result<bool, Error> {
    let dir_row = format!("{DIR}{id}");
    let feed_row = format!("{FEED}{id}");
    write(db, |t| {
        let Some(mut d) = get_in::<DirRec>(&**t, &dir_row)? else {
            return Ok(false);
        };
        let mut f: FeedRec = get_in(&**t, &feed_row)?.unwrap_or_default();
        f.state.checked_at = now;
        if let Some(snap) = snap {
            let keep = admitted_rows(&d.dir, snap);
            let new: BTreeSet<(String, String)> = keep
                .iter()
                .map(|r| (r.table.clone(), r.key.clone()))
                .collect();
            for (table, key) in &f.state.rows {
                if !new.contains(&(table.clone(), key.clone())) {
                    t.remove(format!("{ROW}{id}/{table}/{key}").as_str())
                        .map_err(StoreError::from)?;
                }
            }
            for r in &keep {
                put(
                    t,
                    &format!("{ROW}{id}/{}/{}", r.table, r.key),
                    &json(&r.value),
                )?;
            }
            f.state.rows = new.into_iter().collect();
            f.state.revision = snap.revision.clone();
            f.state.applied_at = now;
            f.ended = snap.ended_sids.clone();
            // The keys the feed brings are the directory's from now on
            // (glidex D63).
            if snap.jwks.get("keys").is_some_and(|k| k.is_array()) {
                d.dir.jwks = snap.jwks.clone();
                put(t, &dir_row, &json(&d))?;
            }
        }
        put(t, &feed_row, &json(&f))?;
        Ok(true)
    })
}

/// Records what the last run of directory `id`'s feed couldn't do.
pub fn set_blocked(db: &Db, id: &str, blocked: &[String]) -> Result<(), Error> {
    let feed_row = format!("{FEED}{id}");
    write(db, |t| {
        if t.get(format!("{DIR}{id}").as_str())
            .map_err(StoreError::from)?
            .is_none()
        {
            return Ok(());
        }
        let mut f: FeedRec = get_in(&**t, &feed_row)?.unwrap_or_default();
        if f.blocked != blocked {
            f.blocked = blocked.to_vec();
            put(t, &feed_row, &json(&f))?;
        }
        Ok(())
    })
}

/// Ends the trust in glidex tenant `tenant` of directory `id` (glidex D66):
/// drops it from the directory's tenants, and with the last one the
/// directory, its feed and its replica; with `tell`, keeps an end for mgmt
/// to tell the directory of. The directory as it was; `None` if it isn't
/// trusted or the tenant isn't admitted. Its tenants here are unlinked by
/// the caller.
pub fn end_tenant(db: &Db, id: &str, tenant: &str, tell: bool) -> Result<Option<DirRec>, Error> {
    let dir_row = format!("{DIR}{id}");
    write(db, |t| {
        let Some(d) = get_in::<DirRec>(&**t, &dir_row)? else {
            return Ok(None);
        };
        if !d.dir.tenants.iter().any(|x| x.id == tenant) {
            return Ok(None);
        }
        let mut left = d.clone();
        left.dir.tenants.retain(|x| x.id != tenant);
        if tell {
            put(
                t,
                &format!("{ENDING}{id}/{tenant}"),
                &json(&EndingRec {
                    dir: d.dir.clone(),
                    tenant: tenant.to_string(),
                }),
            )?;
        }
        if left.dir.tenants.is_empty() {
            t.remove(dir_row.as_str()).map_err(StoreError::from)?;
            t.remove(format!("{FEED}{id}").as_str())
                .map_err(StoreError::from)?;
            let gone: Vec<String> = rows(&**t, &format!("{ROW}{id}/"))?
                .into_iter()
                .map(|(k, _)| format!("{ROW}{id}/{k}"))
                .collect();
            for k in gone {
                t.remove(k.as_str()).map_err(StoreError::from)?;
            }
        } else {
            put(t, &dir_row, &json(&left))?;
        }
        Ok(Some(d))
    })
}

/// The ends mgmt is to tell directories of.
pub fn endings(db: &Db) -> Result<Vec<EndingRec>, Error> {
    read(
        db,
        |t| {
            rows(t, ENDING)?
                .into_iter()
                .map(|(k, v)| parse(&format!("{ENDING}{k}"), &v))
                .collect()
        },
        Vec::new(),
    )
}

/// The directory was told of the end of glidex tenant `tenant`.
pub fn ending_told(db: &Db, id: &str, tenant: &str) -> Result<(), Error> {
    write(db, |t| {
        t.remove(format!("{ENDING}{id}/{tenant}").as_str())
            .map_err(StoreError::from)?;
        Ok(())
    })
}

/// Sets directory `id`'s `max_staleness` (D309, glidex D13), in minutes;
/// `None`: the cluster's default. `false` if it isn't trusted. Only the
/// leader.
pub fn set_staleness(db: &Db, id: &str, minutes: Option<u64>) -> Result<bool, Error> {
    let dir_row = format!("{DIR}{id}");
    write(db, |t| {
        let Some(mut d) = get_in::<DirRec>(&**t, &dir_row)? else {
            return Ok(false);
        };
        d.dir.max_staleness_minutes = minutes;
        put(t, &dir_row, &json(&d))?;
        Ok(true)
    })
}

/// The operator tenant, 0 if none.
pub fn operator(db: &Db) -> Result<u32, Error> {
    read(db, |t| Ok(get_in(t, OPERATOR)?.unwrap_or(0)), 0)
}

/// Names the operator tenant; 0 clears it. Only the leader.
pub fn set_operator(db: &Db, tenant: u32) -> Result<(), Error> {
    write(db, |t| {
        if tenant == 0 {
            t.remove(OPERATOR).map_err(StoreError::from)?;
            Ok(())
        } else {
            put(t, OPERATOR, &json(&tenant))
        }
    })
}

/// `s` as a tenant's or project's name here (1-63 of a-z, 0-9 and `-`,
/// not "default"): lower case, every other character a `-`, runs of them
/// one, none at either end; "x" if nothing is left.
pub fn name_of(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        out.push(c);
    }
    out.truncate(63);
    let out = out.trim_end_matches('-').to_string();
    match out.as_str() {
        "" => "x".into(),
        crate::tenancy::DEFAULT_PROJECT => "default-glidex".into(),
        _ => out,
    }
}

/// `base`, or `base-2`, `base-3`… the first that `taken` doesn't hold,
/// within 63 characters.
fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2u32.. {
        let suffix = format!("-{n}");
        let mut b = base.to_string();
        b.truncate(63 - suffix.len());
        let b = format!("{}{suffix}", b.trim_end_matches('-'));
        if !taken(&b) {
            return b;
        }
    }
    unreachable!()
}

/// A tenant of the feed: its glidex id and slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedTenant {
    pub id: String,
    pub slug: String,
    pub suspended: bool,
}

/// A project of the feed: its glidex id, its tenant's, and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedProject {
    pub id: String,
    pub tenant: String,
    pub name: String,
}

/// The tenants and projects of directory `id`'s replica.
pub fn feed_records(db: &Db, id: &str) -> Result<(Vec<FeedTenant>, Vec<FeedProject>), Error> {
    let tenants = replica(db, id, "tenants")?
        .into_iter()
        .filter_map(|r| serde_json::from_value::<glidex_trust::feed::Tenant>(r.value).ok())
        .map(|t| FeedTenant {
            id: t.id,
            slug: t.slug,
            suspended: t.suspended,
        })
        .collect();
    let projects = replica(db, id, "projects")?
        .into_iter()
        .filter_map(|r| serde_json::from_value::<glidex_trust::feed::Project>(r.value).ok())
        .map(|p| FeedProject {
            id: p.id,
            tenant: p.tenant_id,
            name: p.name,
        })
        .collect();
    Ok((tenants, projects))
}

/// A change the feed asks of the records here (20-tenants.md §24.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A tenant for glidex tenant `glidex_id`, in the directory's realm.
    MakeTenant {
        glidex_id: String,
        name: String,
    },
    RenameTenant {
        id: u32,
        name: String,
    },
    /// Tenant `id` into the directory's realm.
    TenantRealm {
        id: u32,
    },
    MakeProject {
        tenant: u32,
        glidex_id: String,
        name: String,
    },
    RenameProject {
        id: u32,
        name: String,
    },
    /// Project `project` to tenant `to` (D303), as an operation; first
    /// renamed to `rename` when its name is taken there.
    Move {
        project: u32,
        to: u32,
        rename: Option<String>,
    },
    /// Project `id`, gone from the feed: deleted if empty, else unlinked.
    Drop {
        id: u32,
    },
    /// Tenant `id`'s link to a glidex tenant no longer admitted: cleared,
    /// with its projects'.
    Unlink {
        id: u32,
    },
    /// Project `id`'s glidex id, in a tenant linked to no directory (it
    /// moved there as its tenant was unlinked): cleared.
    UnlinkProject {
        id: u32,
    },
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::MakeTenant { glidex_id, name } => {
                write!(f, "make tenant {name} (glidex {glidex_id})")
            }
            Step::RenameTenant { id, name } => write!(f, "rename tenant {id} to {name}"),
            Step::TenantRealm { id } => write!(f, "put tenant {id} in the directory's realm"),
            Step::MakeProject {
                tenant,
                glidex_id,
                name,
            } => write!(
                f,
                "make project {name} of tenant {tenant} (glidex {glidex_id})"
            ),
            Step::RenameProject { id, name } => write!(f, "rename project {id} to {name}"),
            Step::Move { project, to, .. } => {
                write!(f, "move project {project} to tenant {to}")
            }
            Step::Drop { id } => write!(f, "drop project {id}"),
            Step::Unlink { id } => write!(f, "unlink tenant {id}"),
            Step::UnlinkProject { id } => write!(f, "unlink project {id}"),
        }
    }
}

/// What directory `issuer`'s feed asks of `now` (20-tenants.md §24.9): its
/// admitted tenants made, named and in `realm` (`in_realm` says whether a
/// tenant is); its projects made, named and in their tenants (a move when
/// one changed tenant); projects it no longer has dropped; tenants linked
/// to it but not admitted unlinked. Only records linked to the directory
/// change: a local tenant or project keeps its name, and a linked one that
/// would take its name gets a suffix. A project moving is left alone until
/// its move ends.
pub fn plan(
    issuer: &str,
    admitted: &[RemoteTenant],
    tenants: &[FeedTenant],
    projects: &[FeedProject],
    now: &Tenancy,
    in_realm: impl Fn(u32) -> bool,
) -> Vec<Step> {
    let mut out = Vec::new();
    let linked = |gid: &str| {
        now.tenants
            .iter()
            .find(|t| t.directory == issuer && t.glidex_id == gid)
    };
    let admitted_ids: BTreeSet<&str> = admitted.iter().map(|a| a.id.as_str()).collect();
    for t in now
        .tenants
        .iter()
        .filter(|t| t.directory == issuer && !admitted_ids.contains(t.glidex_id.as_str()))
    {
        out.push(Step::Unlink { id: t.id });
    }
    for a in admitted {
        let slug = tenants
            .iter()
            .find(|t| t.id == a.id)
            .map_or(a.slug.as_str(), |t| t.slug.as_str());
        let want = name_of(slug);
        match linked(&a.id) {
            None => {
                // A name another tenant holds: a suffix, as a local one
                // keeps its own.
                let name = free_name(&want, |n| now.tenant_named(n).is_some());
                out.push(Step::MakeTenant {
                    glidex_id: a.id.clone(),
                    name,
                });
            }
            Some(t) => {
                let name = free_name(&want, |n| now.tenant_named(n).is_some_and(|x| x.id != t.id));
                if name != t.name {
                    out.push(Step::RenameTenant { id: t.id, name });
                }
                if !in_realm(t.id) {
                    out.push(Step::TenantRealm { id: t.id });
                }
            }
        }
    }
    // Projects, in glidex id order so names given are stable.
    let mut wanted: Vec<&FeedProject> = projects
        .iter()
        .filter(|p| admitted_ids.contains(p.tenant.as_str()))
        .collect();
    wanted.sort_by(|a, b| a.id.cmp(&b.id));
    let ours = |gid: &str| {
        now.projects.iter().find(|p| {
            p.glidex_id == gid && now.tenant(p.tenant).is_some_and(|t| t.directory == issuer)
        })
    };
    // Names taken by the steps planned so far, per tenant.
    let mut planned: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for p in &wanted {
        let Some(t) = linked(&p.tenant) else {
            // Its tenant first; the project at the next pass.
            continue;
        };
        let base = name_of(&p.name);
        match ours(&p.id) {
            None => {
                let taken = planned.entry(t.id).or_default();
                let name = free_name(&base, |n| {
                    now.project_named(t.id, n).is_some() || taken.contains(n)
                });
                taken.insert(name.clone());
                out.push(Step::MakeProject {
                    tenant: t.id,
                    glidex_id: p.id.clone(),
                    name,
                });
            }
            Some(e) if e.state == ProjectState::Moving => {}
            Some(e) if e.tenant != t.id => {
                // A name free in both tenants, if its own is taken there.
                let taken = planned.entry(t.id).or_default();
                let rename = (now.project_named(t.id, &e.name).is_some()
                    || taken.contains(&e.name))
                .then(|| {
                    free_name(&base, |n| {
                        now.project_named(t.id, n).is_some()
                            || now.project_named(e.tenant, n).is_some_and(|x| x.id != e.id)
                            || taken.contains(n)
                    })
                });
                taken.insert(rename.clone().unwrap_or_else(|| e.name.clone()));
                out.push(Step::Move {
                    project: e.id,
                    to: t.id,
                    rename,
                });
            }
            Some(e) => {
                let taken = planned.entry(t.id).or_default();
                let name = free_name(&base, |n| {
                    now.project_named(t.id, n).is_some_and(|x| x.id != e.id) || taken.contains(n)
                });
                taken.insert(name.clone());
                if name != e.name {
                    out.push(Step::RenameProject { id: e.id, name });
                }
            }
        }
    }
    let want_ids: BTreeSet<&str> = wanted.iter().map(|p| p.id.as_str()).collect();
    for p in &now.projects {
        if !p.glidex_id.is_empty()
            && p.state != ProjectState::Moving
            && !want_ids.contains(p.glidex_id.as_str())
            && now.tenant(p.tenant).is_some_and(|t| {
                t.directory == issuer && admitted_ids.contains(t.glidex_id.as_str())
            })
        {
            out.push(Step::Drop { id: p.id });
        }
        if !p.glidex_id.is_empty()
            && p.state != ProjectState::Moving
            && now.tenant(p.tenant).is_some_and(|t| t.directory.is_empty())
        {
            out.push(Step::UnlinkProject { id: p.id });
        }
    }
    out
}

/// Rows written by the API that a test reads back.
#[cfg(test)]
pub(crate) fn all_rows(db: &Db) -> Vec<String> {
    read(
        db,
        |t| Ok(rows(t, "trust/")?.into_iter().map(|(k, _)| k).collect()),
        Vec::new(),
    )
    .unwrap()
}

#[cfg(test)]
mod tests;

/// The rows of a directory's replica that say who a person is and what
/// they may do (glidex D3, D12): its users, teams, tenants' members and
/// policy links.
#[derive(Debug, Clone, Default)]
pub struct People {
    pub users: Vec<glidex_trust::feed::User>,
    pub teams: Vec<glidex_trust::feed::Team>,
    pub members: Vec<glidex_trust::feed::Member>,
    pub links: Vec<glidex_trust::feed::Link>,
}

/// The rows of directory `id`'s replica that [`person_of`] reads.
pub fn people(db: &Db, id: &str) -> Result<People, Error> {
    fn of<T: DeserializeOwned>(db: &Db, id: &str, table: &str) -> Result<Vec<T>, Error> {
        Ok(replica(db, id, table)?
            .into_iter()
            .filter_map(|r| serde_json::from_value(r.value).ok())
            .collect())
    }
    Ok(People {
        users: of(db, id, "users")?,
        teams: of(db, id, "teams")?,
        members: of(db, id, "tenant_members")?,
        links: of(db, id, "policy_links")?,
    })
}

/// A gxt_ token of directory `id`'s replica, by its hash
/// ([`glidex_trust::tokens::api_token_hash`]).
pub fn api_token(
    db: &Db,
    id: &str,
    hash: &str,
) -> Result<Option<glidex_trust::feed::ApiToken>, Error> {
    let row = format!("{ROW}{id}/api_tokens/{hash}");
    read(
        db,
        |t| Ok(get_in::<serde_json::Value>(t, &row)?.and_then(|v| serde_json::from_value(v).ok())),
        None,
    )
}

/// Who a directory's person is here (D307, D308), from its replica.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Person {
    /// In the replica (a user row, or a token's), and disabled there.
    pub found: bool,
    pub disabled: bool,
    pub display_name: String,
    /// What its links give, sorted: `role@tenant:T`, `role@project:T/P`.
    pub grants: Vec<String>,
    /// A member of the operator tenant, directly or through a team.
    pub operator: bool,
}

/// The roles a glidex link's template gives here (D308): a tenant's
/// `tenant-admin` stores and secures, its `tenant-auditor` reads; a
/// project's `owner` and `editor` store, its `operator` operates, its
/// `viewer` reads. Any other template, nothing. glidex names templates
/// `role.<name>` (its Cedar schema's actions); the bare name reads the
/// same.
pub fn roles_of(template: &str, on_tenant: bool) -> &'static [&'static str] {
    let template = template.strip_prefix("role.").unwrap_or(template);
    match (on_tenant, template) {
        (true, "tenant-admin") => &["storage-admin", "security-admin"],
        (true, "tenant-auditor") => &["viewer"],
        (false, "owner" | "editor") => &["storage-admin"],
        (false, "operator") => &["operator"],
        (false, "viewer") => &["viewer"],
        _ => &[],
    }
}

/// Who `who` (a user, or a service account's token) of the directory with
/// issuer `issuer` is here, from its replica rows `p`, the records `now`
/// and the operator tenant `operator` (0: none). A user is found in the
/// `users` rows; a token always (the caller found its row). Its grants
/// come from the links that name it, or a team it is in, on a tenant or
/// project of this directory linked here; a link to anything not linked
/// here gives nothing.
pub fn person_of(
    issuer: &str,
    p: &People,
    now: &Tenancy,
    operator: u32,
    who: &glidex_trust::feed::Entity,
) -> Person {
    use glidex_trust::feed::Entity;
    let mut out = Person::default();
    match who {
        Entity::User(id) => {
            if let Some(u) = p.users.iter().find(|u| &u.id == id) {
                out.found = true;
                out.disabled = u.disabled;
                out.display_name = u.display_name.clone();
            }
        }
        Entity::Token(id) => {
            out.found = true;
            out.display_name = format!("token {id}");
        }
        _ => return out,
    }
    if !out.found {
        return out;
    }
    // The person, and the teams they are in.
    let mut is: Vec<&Entity> = vec![who];
    let teams: Vec<Entity> = match who {
        Entity::User(id) => p
            .teams
            .iter()
            .filter(|t| t.members.iter().any(|m| &m.user_id == id))
            .map(|t| Entity::Team(t.id.clone()))
            .collect(),
        _ => Vec::new(),
    };
    is.extend(teams.iter());
    let tenant_here = |gid: &str| {
        now.tenants
            .iter()
            .find(|t| t.directory == issuer && t.glidex_id == gid)
    };
    let mut grants = BTreeSet::new();
    for l in p.links.iter().filter(|l| is.contains(&&l.principal)) {
        match &l.resource {
            Entity::Tenant(gid) => {
                if let Some(t) = tenant_here(gid) {
                    for r in roles_of(&l.template, true) {
                        grants.insert(format!("{r}@tenant:{}", t.name));
                    }
                }
            }
            Entity::Project(gid) => {
                let Some(pr) = now
                    .projects
                    .iter()
                    .find(|x| !gid.is_empty() && &x.glidex_id == gid)
                else {
                    continue;
                };
                // Its tenant must be this directory's.
                let Some(t) = now.tenant(pr.tenant).filter(|t| t.directory == issuer) else {
                    continue;
                };
                for r in roles_of(&l.template, false) {
                    grants.insert(format!("{r}@project:{}/{}", t.name, pr.name));
                }
            }
            _ => {}
        }
    }
    out.grants = grants.into_iter().collect();
    // The operator tenant: one of this directory's, the person in it.
    if let Some(op) = now.tenant(operator).filter(|t| t.directory == issuer) {
        out.operator = p
            .members
            .iter()
            .any(|m| m.tenant_id == op.glidex_id && is.contains(&&m.principal));
    }
    out
}
