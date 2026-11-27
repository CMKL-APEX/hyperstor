//! Datasets and their properties (29-hsfs.md §32.2, §32.3; 04-decisions.md
//! D360, D361): a volume, a filesystem or a bucket, named
//! `tenant/project/name`, with properties set at any level and inherited
//! by everything below unless set there.
//!
//! Properties are rows of the replicated store's settings table, as
//! tenants and projects are (`crate::tenancy`):
//!
//! - `dprop/t<id>/<property>`: set on tenant `id`;
//! - `dprop/p<id>/<property>`: on project `id`;
//! - `dprop/v<32 hex digits>/<property>`: on a volume, by its id;
//! - `dprop/b<table id>/<property>`: on a bucket, by its table's id, which
//!   a rename or a move keeps (D303).
//!
//! Each holds the value, when and by whom it was set. A value is checked
//! and made canonical when set (sizes in their largest exact unit). Rows of
//! a tenant or project go with it; rows of a volume or bucket that no
//! longer exists are never read, since their ids are never given again.
//!
//! Names are worked out from the volumes and buckets the cluster holds, so
//! there is no second record of them to keep in step. Names were never
//! unique, so two live datasets of one project that share one are each
//! shown and addressed with a qualifier, `name@<volume id>` or
//! `name@bucket`; a new one may not take a name its project has (I459).

use crate::store::{self, Db, Origin, Table};
use crate::tenancy::Tenancy;
use crate::tokens::TokenError as Error;
use glidex_raftstore::store::{RecTable, StoreError, definition};
use hyperstor_codec::{Dec, DecodeError, Enc};
use redb::ReadableTable;
use std::collections::BTreeMap;

const PROP: &str = "dprop/";

/// What a dataset is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Volume,
    Filesystem,
    Bucket,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Volume => "volume",
            Kind::Filesystem => "filesystem",
            Kind::Bucket => "bucket",
        }
    }
}

/// Where a property is set: a tenant, a project, or one dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Tenant(u32),
    Project(u32),
    Volume(u128),
    /// By its table's id.
    Bucket(u64),
}

impl Level {
    fn key(self) -> String {
        match self {
            Level::Tenant(id) => format!("t{id}"),
            Level::Project(id) => format!("p{id}"),
            Level::Volume(id) => format!("v{id:032x}"),
            Level::Bucket(id) => format!("b{id}"),
        }
    }

    fn parse(s: &str) -> Option<Level> {
        let (k, id) = s.split_at_checked(1)?;
        Some(match k {
            "t" => Level::Tenant(id.parse().ok()?),
            "p" => Level::Project(id.parse().ok()?),
            "v" if id.len() == 32 => Level::Volume(u128::from_str_radix(id, 16).ok()?),
            "b" => Level::Bucket(id.parse().ok()?),
            _ => return None,
        })
    }

    /// The kind of dataset it is, for a dataset's own level.
    fn kind(self) -> Option<Kind> {
        match self {
            Level::Volume(_) => Some(Kind::Volume),
            Level::Bucket(_) => Some(Kind::Bucket),
            Level::Tenant(_) | Level::Project(_) => None,
        }
    }
}

/// Which datasets a property applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    All,
    Filesystem,
    Bucket,
}

impl Applies {
    fn to(self, k: Kind) -> bool {
        match self {
            Applies::All => true,
            Applies::Filesystem => k == Kind::Filesystem,
            Applies::Bucket => k == Kind::Bucket,
        }
    }
}

/// How a property's values are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// Bytes, `none` for no limit.
    Size,
    /// A power of two from 4 KiB to 64 MiB.
    Chunk,
    /// A whole number in a range.
    Number(u64, u64),
    /// One of these words.
    Words(&'static [&'static str]),
    /// A pool, kept by its id: the API names it.
    Pool,
    /// Options passed on as written: up to 200 characters, no control
    /// characters.
    Text,
}

/// A property of §32.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Property {
    pub name: &'static str,
    pub applies: Applies,
    /// What a dataset has when nothing above it sets it: `-` for the
    /// pool's or the cluster's own.
    pub default: &'static str,
    /// The kinds of dataset anything acts on it for yet; for others it is
    /// kept and shown.
    pub enforced: &'static [Kind],
    check: Check,
}

/// Every property, in §32.3's order. Two act on volumes so far: `pool`,
/// the pool new volumes go to when their creation names none (I459), and
/// `readonly`, whose volumes' primaries refuse writes (D390).
pub const PROPERTIES: &[Property] = &[
    Property {
        name: "quota",
        applies: Applies::All,
        default: "none",
        enforced: &[],
        check: Check::Size,
    },
    Property {
        name: "refquota",
        applies: Applies::All,
        default: "none",
        enforced: &[],
        check: Check::Size,
    },
    Property {
        name: "reservation",
        applies: Applies::All,
        default: "none",
        enforced: &[],
        check: Check::Size,
    },
    Property {
        name: "pool",
        applies: Applies::All,
        default: "-",
        enforced: &[Kind::Volume, Kind::Bucket],
        check: Check::Pool,
    },
    Property {
        name: "chunk",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Chunk,
    },
    Property {
        name: "stripe",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Number(1, 256),
    },
    Property {
        name: "compression",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Words(&["off", "lz4", "zstd"]),
    },
    Property {
        name: "encryption",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Words(&["off", "on"]),
    },
    Property {
        name: "readonly",
        applies: Applies::All,
        default: "off",
        enforced: &[Kind::Volume, Kind::Bucket],
        check: Check::Words(&["off", "on"]),
    },
    Property {
        name: "reads",
        applies: Applies::All,
        default: "primary",
        enforced: &[],
        check: Check::Words(&["primary", "any"]),
    },
    Property {
        name: "weight",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Number(1, 10_000),
    },
    Property {
        name: "class",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Words(&["latency", "normal", "bulk"]),
    },
    Property {
        name: "snapdir",
        applies: Applies::Filesystem,
        default: "hidden",
        enforced: &[],
        check: Check::Words(&["hidden", "visible"]),
    },
    Property {
        name: "nfs",
        applies: Applies::Filesystem,
        default: "-",
        enforced: &[],
        check: Check::Text,
    },
    Property {
        name: "s3",
        applies: Applies::Bucket,
        default: "-",
        enforced: &[],
        check: Check::Text,
    },
    Property {
        name: "sync",
        applies: Applies::All,
        default: "-",
        enforced: &[],
        check: Check::Text,
    },
];

pub fn property(name: &str) -> Option<&'static Property> {
    PROPERTIES.iter().find(|p| p.name == name)
}

/// `bytes` in its largest exact unit of K, M, G, T, P (powers of 1024).
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["", "K", "M", "G", "T", "P"];
    let mut n = bytes;
    let mut u = 0;
    while u + 1 < UNITS.len() && n != 0 && n.is_multiple_of(1024) {
        n /= 1024;
        u += 1;
    }
    format!("{n}{}", UNITS[u])
}

/// Bytes, or with a K, M, G, T or P suffix (powers of 1024).
pub fn parse_size(s: &str) -> Option<u64> {
    let (n, shift) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 10),
        b'M' | b'm' => (&s[..s.len() - 1], 20),
        b'G' | b'g' => (&s[..s.len() - 1], 30),
        b'T' | b't' => (&s[..s.len() - 1], 40),
        b'P' | b'p' => (&s[..s.len() - 1], 50),
        _ => (s, 0),
    };
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse::<u64>().ok()?.checked_mul(1 << shift)
}

impl Property {
    /// `value` as stored, or why it isn't one of this property's. A pool
    /// is checked by the API, which knows the pools: here its id.
    pub fn canonical(&self, value: &str) -> Result<String, String> {
        let bad = |what: &str| format!("{}: {value:?} isn't {what}", self.name);
        match self.check {
            Check::Size if value == "none" => Ok(value.into()),
            Check::Size => match parse_size(value) {
                Some(b) if b > 0 => Ok(size_text(b)),
                _ => Err(bad("a size (bytes, or with K, M, G, T or P) or none")),
            },
            Check::Chunk => match parse_size(value) {
                Some(b) if b.is_power_of_two() && (4096..=64 << 20).contains(&b) => {
                    Ok(size_text(b))
                }
                _ => Err(bad("a power of two from 4K to 64M")),
            },
            Check::Number(lo, hi) => match value.parse::<u64>() {
                Ok(n) if (lo..=hi).contains(&n) && n.to_string() == value => Ok(value.into()),
                _ => Err(bad(&format!("a whole number from {lo} to {hi}"))),
            },
            Check::Words(w) if w.contains(&value) => Ok(value.into()),
            Check::Words(w) => Err(bad(&format!("one of {}", w.join(", ")))),
            Check::Pool => match value.parse::<u32>() {
                Ok(id) if id != hyperstor_map::META_POOL && id.to_string() == value => {
                    Ok(value.into())
                }
                _ => Err(bad("a pool")),
            },
            Check::Text if value.chars().count() <= 200 && !value.chars().any(char::is_control) => {
                Ok(value.into())
            }
            Check::Text => Err(bad("up to 200 characters with no control characters")),
        }
    }

    /// Whether a dataset of kind `k` has it.
    pub fn applies_to(&self, k: Kind) -> bool {
        self.applies.to(k)
    }

    /// Whether anything acts on it for a dataset of kind `k`, or, for a
    /// tenant or project (`None`), for any dataset below it.
    pub fn enforced_for(&self, k: Option<Kind>) -> bool {
        k.map_or(!self.enforced.is_empty(), |k| self.enforced.contains(&k))
    }

    /// Whether its value names a pool.
    pub fn is_pool(&self) -> bool {
        self.check == Check::Pool
    }
}

/// A value as set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    pub value: String,
    pub set_unix: u64,
    pub set_by: String,
}

impl Set {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.str(&self.value).u64(self.set_unix).str(&self.set_by);
        e.into_vec()
    }

    fn decode(b: &[u8]) -> Result<Set, DecodeError> {
        let mut d = Dec::new(b);
        Ok(Set {
            value: d.str()?.to_string(),
            set_unix: d.u64()?,
            set_by: d.str()?.to_string(),
        })
    }
}

/// Every property set, by level and name, from one read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Props {
    pub set: BTreeMap<Level, BTreeMap<String, Set>>,
}

/// Where a dataset's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Set on the level asked about.
    Local,
    /// Set on a level above it.
    Inherited(Level),
    Default,
}

/// A property's value at a level, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub property: &'static Property,
    pub value: String,
    pub source: Source,
    /// When and by whom, for one set somewhere.
    pub set: Option<Set>,
}

impl Props {
    /// Every property applying at `chain[0]` (a dataset of `kind`, or a
    /// tenant or project with none), each from the first level of `chain`
    /// that sets it: the level itself, then its project, then its tenant.
    pub fn values(&self, chain: &[Level], kind: Option<Kind>) -> Vec<Value> {
        PROPERTIES
            .iter()
            .filter(|p| kind.is_none_or(|k| p.applies.to(k)))
            .map(|p| self.value(chain, p))
            .collect()
    }

    fn value(&self, chain: &[Level], p: &'static Property) -> Value {
        for (i, l) in chain.iter().enumerate() {
            if let Some(s) = self.set.get(l).and_then(|m| m.get(p.name)) {
                return Value {
                    property: p,
                    value: s.value.clone(),
                    source: if i == 0 {
                        Source::Local
                    } else {
                        Source::Inherited(*l)
                    },
                    set: Some(s.clone()),
                };
            }
        }
        Value {
            property: p,
            value: p.default.into(),
            source: Source::Default,
            set: None,
        }
    }

    /// The value of `name` along `chain`, if any level sets it.
    pub fn get(&self, chain: &[Level], name: &str) -> Option<&str> {
        chain
            .iter()
            .find_map(|l| self.set.get(l)?.get(name))
            .map(|s| s.value.as_str())
    }
}

/// The levels a dataset's values come from: its own, its project's, its
/// tenant's. A tenant or project starts the chain at itself.
pub fn chain(own: Option<Level>, tenant: u32, project: u32) -> Vec<Level> {
    let mut out: Vec<Level> = own.into_iter().collect();
    if project != 0 {
        out.push(Level::Project(project));
    }
    if tenant != 0 {
        out.push(Level::Tenant(tenant));
    }
    out.dedup();
    out
}

fn scan(
    t: &impl ReadableTable<&'static str, &'static [u8]>,
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

fn read(t: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<Props, Error> {
    let mut out = Props::default();
    for (rest, v) in scan(t, PROP)? {
        let bad = |e: String| Error::Store(format!("{PROP}{rest}: {e}"));
        let (level, name) = rest
            .split_once('/')
            .and_then(|(l, n)| Some((Level::parse(l)?, n)))
            .ok_or_else(|| bad("not a property".into()))?;
        out.set.entry(level).or_default().insert(
            name.to_string(),
            Set::decode(&v).map_err(|e| bad(e.to_string()))?,
        );
    }
    Ok(out)
}

/// Every property set, from this member's copy.
pub fn load(db: &Db) -> Result<Props, Error> {
    let txn = db.begin_read().map_err(|e| Error::Store(e.to_string()))?;
    let Ok(t) = txn.open_table(definition::<store::MapSchema>(Table::Settings)) else {
        return Ok(Props::default());
    };
    read(&t)
}

fn refused(reason: &'static str, why: String) -> Error {
    Error::Refused { reason, why }
}

/// `value` for property `name` on `level`, made canonical, or why it
/// can't be: a dataset's own level takes only what applies to its kind.
pub fn check(level: Level, name: &str, value: &str) -> Result<String, String> {
    let p = property(name)
        .ok_or_else(|| format!("no property {name:?}: one of {}", names().join(", ")))?;
    if let Some(k) = level.kind()
        && !p.applies.to(k)
    {
        return Err(format!("{name} doesn't apply to a {}", k.name()));
    }
    p.canonical(value)
}

/// Sets property `name` on `level` to `value` ([`check`]ed); the value
/// stored.
pub fn set(
    db: &Db,
    level: Level,
    name: &str,
    value: &str,
    set_unix: u64,
    set_by: &str,
) -> Result<String, Error> {
    let value = check(level, name, value).map_err(|e| refused("invalid_argument", e))?;
    let row = Set {
        value: value.clone(),
        set_unix,
        set_by: set_by.into(),
    };
    db.write(Origin::Leader, |tx| -> Result<(), Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        t.insert(
            format!("{PROP}{}/{name}", level.key()).as_str(),
            row.encode().as_slice(),
        )
        .map_err(StoreError::from)?;
        Ok(())
    })?;
    Ok(value)
}

/// Clears property `name` on `level`, which then inherits it; whether it
/// was set.
pub fn inherit(db: &Db, level: Level, name: &str) -> Result<bool, Error> {
    if property(name).is_none() {
        return Err(refused("invalid_argument", format!("no property {name:?}")));
    }
    db.write(Origin::Leader, |tx| -> Result<bool, Error> {
        let mut t = tx
            .open_table(definition::<store::MapSchema>(Table::Settings))
            .map_err(StoreError::from)?;
        Ok(t.remove(format!("{PROP}{}/{name}", level.key()).as_str())
            .map_err(StoreError::from)?
            .is_some())
    })
}

/// Removes every property of `level`, in a write of the caller's: a
/// tenant or project deleted takes its rows with it.
pub(crate) fn drop_level(
    t: &mut RecTable<'_, store::MapSchema>,
    level: Level,
) -> Result<(), Error> {
    let prefix = format!("{PROP}{}/", level.key());
    for (name, _) in scan(&**t, &prefix)? {
        t.remove(format!("{prefix}{name}").as_str())
            .map_err(StoreError::from)?;
    }
    Ok(())
}

/// Every property's name.
pub fn names() -> Vec<&'static str> {
    PROPERTIES.iter().map(|p| p.name).collect()
}

/// Which dataset, by its own id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Id {
    Volume(u128),
    /// By its table's id.
    Bucket(u64),
}

impl Id {
    pub fn kind(self) -> Kind {
        match self {
            Id::Volume(_) => Kind::Volume,
            Id::Bucket(_) => Kind::Bucket,
        }
    }

    pub fn level(self) -> Level {
        match self {
            Id::Volume(v) => Level::Volume(v),
            Id::Bucket(b) => Level::Bucket(b),
        }
    }

    fn qualifier(self) -> String {
        match self {
            Id::Volume(v) => format!("{v:032x}"),
            Id::Bucket(_) => "bucket".into(),
        }
    }
}

/// A live dataset as the cluster holds it: its tenant and project as
/// [`Tenancy::tenant_of`] and [`Tenancy::project_of`] (or, for a bucket,
/// [`Tenancy::bucket_project`]) give them, and its name as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: Id,
    pub tenant: u32,
    pub project: u32,
    pub name: String,
    pub pool: u32,
}

/// A dataset's full name: `tenant/project/name`, by names where known,
/// `-` for none, with the qualifier when its project has another dataset
/// of its name (or its name is empty).
pub fn full_name(t: &Tenancy, e: &Entry, qualified: bool) -> String {
    let tenant = if e.tenant == 0 {
        "-".into()
    } else {
        t.tenant_label(e.tenant)
    };
    let project = match t.project(e.project) {
        Some(p) => p.name.clone(),
        None if e.project == 0 => "-".into(),
        None => format!("#{}", e.project),
    };
    if qualified || e.name.is_empty() {
        format!("{tenant}/{project}/{}@{}", e.name, e.id.qualifier())
    } else {
        format!("{tenant}/{project}/{}", e.name)
    }
}

/// Whether each entry needs its qualifier: another of its project has
/// its name.
pub fn qualified(entries: &[Entry]) -> Vec<bool> {
    let mut count: BTreeMap<(u32, u32, &str), usize> = BTreeMap::new();
    for e in entries {
        *count
            .entry((e.tenant, e.project, e.name.as_str()))
            .or_default() += 1;
    }
    entries
        .iter()
        .map(|e| count[&(e.tenant, e.project, e.name.as_str())] > 1)
        .collect()
}

/// Why a new dataset of `tenant`'s `project` may not be named `name`:
/// another live one there has it (D360, I459). Datasets in no project
/// (glidex's image volumes, a bare tenant's) and unnamed ones are not
/// checked.
pub fn clash(
    t: &Tenancy,
    entries: &[Entry],
    tenant: u32,
    project: u32,
    name: &str,
) -> Option<String> {
    if project == 0 || name.is_empty() {
        return None;
    }
    let e = entries
        .iter()
        .find(|e| e.tenant == tenant && e.project == project && e.name == name)?;
    Some(format!(
        "{} is taken, by a {}",
        full_name(t, e, false),
        e.id.kind().name()
    ))
}

/// The name a new dataset of `tenant`'s `project` takes for `name` when
/// its caller named none and `name` is taken there: `name-SUFFIX`, else
/// `name-SUFFIX-2`, `-3`… (I459): a copy's `-copy`, a restore's
/// `-restored`. In no project, or unnamed, `name` itself, as before.
pub fn free_name(entries: &[Entry], tenant: u32, project: u32, name: &str, suffix: &str) -> String {
    if project == 0 || name.is_empty() {
        return name.into();
    }
    let taken = |n: &str| {
        entries
            .iter()
            .any(|e| e.tenant == tenant && e.project == project && e.name == n)
    };
    let base = format!("{name}-{suffix}");
    if !taken(&base) {
        return base;
    }
    (2u32..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !taken(n))
        .unwrap_or(base)
}

/// What a dataset path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    Tenant(u32),
    Project(u32),
    Dataset(Id),
}

/// Why a path names nothing, or more than one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    NotFound(String),
    /// More than one dataset has the name: their qualified names.
    Ambiguous(Vec<String>),
}

/// Path `p`: `tenant`, `tenant/project`, or `tenant/project/name`, with
/// `@<volume id>` or `@bucket` to pick one of several of a name; a tenant
/// by its name or number. A name may itself hold `/` or `@`: the first
/// two `/` split the path, and `@` only before a qualifier.
pub fn resolve(t: &Tenancy, entries: &[Entry], p: &str) -> Result<Path, Unresolved> {
    let gone = || Unresolved::NotFound(format!("no dataset {p}"));
    let mut parts = p.splitn(3, '/');
    let tname = parts.next().unwrap_or_default();
    let tenant = match t.tenant_named(tname) {
        Some(x) => x.id,
        None => match tname.parse::<u32>() {
            Ok(id) if id != 0 => id,
            _ if tname == "-" => 0,
            _ => return Err(gone()),
        },
    };
    let Some(pname) = parts.next() else {
        return match t.tenant(tenant) {
            Some(_) => Ok(Path::Tenant(tenant)),
            None => Err(gone()),
        };
    };
    let project = match pname {
        "-" => 0,
        n => match t.project_named(tenant, n) {
            Some(x) => x.id,
            None => match n.strip_prefix('#').and_then(|i| i.parse::<u32>().ok()) {
                Some(id) => id,
                None => return Err(gone()),
            },
        },
    };
    let Some(name) = parts.next() else {
        return match t.project(project) {
            Some(x) if x.tenant == tenant => Ok(Path::Project(project)),
            _ => Err(gone()),
        };
    };
    let here = |e: &&Entry| e.tenant == tenant && e.project == project;
    // A qualifier, if what follows the last `@` is one.
    if let Some((n, q)) = name.rsplit_once('@') {
        let pick = entries
            .iter()
            .filter(here)
            .find(|e| e.name == n && e.id.qualifier() == q);
        if let Some(e) = pick {
            return Ok(Path::Dataset(e.id));
        }
        if q == "bucket" || (q.len() == 32 && q.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err(gone());
        }
    }
    let found: Vec<&Entry> = entries
        .iter()
        .filter(here)
        .filter(|e| e.name == name)
        .collect();
    match found[..] {
        [] => Err(gone()),
        [e] => Ok(Path::Dataset(e.id)),
        _ => Err(Unresolved::Ambiguous(
            found.iter().map(|e| full_name(t, e, true)).collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{self, NewTenant};

    fn tenancy_with(db: &Db) -> (Tenancy, u32, u32, u32) {
        let (acme, def) = tenancy::create_tenant(
            db,
            &NewTenant {
                name: "acme".into(),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let ml = tenancy::create_project(db, acme, "ml", "", 0, "test").unwrap();
        (tenancy::load(db).unwrap(), acme, def, ml)
    }

    fn vol(id: u128, tenant: u32, project: u32, name: &str) -> Entry {
        Entry {
            id: Id::Volume(id),
            tenant,
            project,
            name: name.into(),
            pool: 1,
        }
    }

    #[test]
    fn values_come_from_the_nearest_level_that_sets_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let (_, acme, _, ml) = tenancy_with(&db);
        let v = Level::Volume(7);
        let at = chain(Some(v), acme, ml);
        assert_eq!(at, [v, Level::Project(ml), Level::Tenant(acme)]);
        set(&db, Level::Tenant(acme), "quota", "1024G", 1, "a").unwrap();
        set(&db, Level::Tenant(acme), "class", "bulk", 1, "a").unwrap();
        set(&db, Level::Project(ml), "class", "latency", 2, "b").unwrap();
        set(&db, v, "readonly", "on", 3, "c").unwrap();
        let props = load(&db).unwrap();
        let vals = props.values(&at, Some(Kind::Volume));
        let of = |n: &str| vals.iter().find(|x| x.property.name == n).unwrap();
        // Canonical: 1024G is 1T.
        assert_eq!(of("quota").value, "1T");
        assert_eq!(of("quota").source, Source::Inherited(Level::Tenant(acme)));
        assert_eq!(of("class").value, "latency");
        assert_eq!(of("class").source, Source::Inherited(Level::Project(ml)));
        assert_eq!(of("readonly").source, Source::Local);
        assert_eq!(of("reads").value, "primary");
        assert_eq!(of("reads").source, Source::Default);
        // A volume has no snapdir, nfs or s3.
        assert!(
            vals.iter()
                .all(|x| !["snapdir", "nfs", "s3"].contains(&x.property.name))
        );
        // Cleared, it inherits again.
        assert!(inherit(&db, Level::Project(ml), "class").unwrap());
        assert!(!inherit(&db, Level::Project(ml), "class").unwrap());
        let props = load(&db).unwrap();
        assert_eq!(props.get(&at, "class"), Some("bulk"));
    }

    #[test]
    fn values_are_checked_and_made_canonical() {
        let p = |n| property(n).unwrap();
        assert_eq!(p("quota").canonical("2048").unwrap(), "2K");
        assert_eq!(p("quota").canonical("3000").unwrap(), "3000");
        assert_eq!(p("quota").canonical("none").unwrap(), "none");
        for bad in ["", "0", "-1", "1.5G", "G", "1X", "99999999999P"] {
            assert!(p("quota").canonical(bad).is_err(), "{bad:?}");
        }
        assert_eq!(p("chunk").canonical("4096").unwrap(), "4K");
        assert!(p("chunk").canonical("3K").is_err());
        assert!(p("chunk").canonical("128M").is_err());
        assert!(p("stripe").canonical("0").is_err());
        assert!(p("stripe").canonical("08").is_err());
        assert_eq!(p("stripe").canonical("16").unwrap(), "16");
        assert!(p("reads").canonical("all").is_err());
        assert!(p("pool").canonical("0").is_err());
        assert!(p("nfs").canonical("rw\nroot").is_err());
        assert!(p("nfs").canonical(&"x".repeat(201)).is_err());
    }

    #[test]
    fn a_dataset_takes_only_what_applies_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let reason = |r: Result<String, Error>| match r {
            Err(Error::Refused { reason, .. }) => reason,
            r => panic!("{r:?}"),
        };
        assert_eq!(
            reason(set(&db, Level::Volume(1), "nfs", "rw", 0, "")),
            "invalid_argument"
        );
        assert_eq!(
            reason(set(&db, Level::Volume(1), "s3", "x", 0, "")),
            "invalid_argument"
        );
        assert_eq!(
            reason(set(&db, Level::Bucket(1), "snapdir", "visible", 0, "")),
            "invalid_argument"
        );
        assert_eq!(
            reason(set(&db, Level::Tenant(1), "nope", "x", 0, "")),
            "invalid_argument"
        );
        // A tenant or project holds any, for what is below it.
        set(&db, Level::Tenant(1), "nfs", "rw", 0, "").unwrap();
        set(&db, Level::Bucket(1), "s3", "website", 0, "").unwrap();
    }

    #[test]
    fn a_deleted_project_takes_its_properties() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let (_, acme, _, ml) = tenancy_with(&db);
        set(&db, Level::Project(ml), "quota", "1G", 0, "").unwrap();
        set(&db, Level::Tenant(acme), "quota", "2G", 0, "").unwrap();
        tenancy::delete_project(&db, ml, &[]).unwrap();
        let props = load(&db).unwrap();
        assert!(!props.set.contains_key(&Level::Project(ml)));
        assert!(props.set.contains_key(&Level::Tenant(acme)));
    }

    #[test]
    fn names_are_qualified_only_where_they_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let (t, acme, def, ml) = tenancy_with(&db);
        let b = Entry {
            id: Id::Bucket(9),
            tenant: acme,
            project: ml,
            name: "data".into(),
            pool: 1,
        };
        let es = vec![
            vol(1, acme, ml, "data"),
            b.clone(),
            vol(2, acme, ml, "root"),
            vol(3, acme, def, "root"),
            vol(4, 0, 0, "image"),
            vol(5, 0, 0, "image"),
            vol(6, acme, ml, ""),
        ];
        let q = qualified(&es);
        let names: Vec<String> = es
            .iter()
            .zip(&q)
            .map(|(e, q)| full_name(&t, e, *q))
            .collect();
        assert_eq!(
            names,
            [
                format!("acme/ml/data@{:032x}", 1),
                "acme/ml/data@bucket".into(),
                "acme/ml/root".into(),
                "acme/default/root".into(),
                format!("-/-/image@{:032x}", 4),
                format!("-/-/image@{:032x}", 5),
                format!("acme/ml/@{:032x}", 6),
            ]
        );
        // Each name shown resolves to its own dataset.
        for (n, e) in names.iter().zip(&es) {
            assert_eq!(resolve(&t, &es, n), Ok(Path::Dataset(e.id)), "{n}");
        }
        assert_eq!(
            resolve(&t, &es, "acme/ml/root"),
            Ok(Path::Dataset(Id::Volume(2)))
        );
        assert_eq!(resolve(&t, &es, "acme"), Ok(Path::Tenant(acme)));
        assert_eq!(resolve(&t, &es, &acme.to_string()), Ok(Path::Tenant(acme)));
        assert_eq!(resolve(&t, &es, "acme/ml"), Ok(Path::Project(ml)));
        assert!(matches!(
            resolve(&t, &es, "acme/ml/data"),
            Err(Unresolved::Ambiguous(n)) if n.len() == 2
        ));
        for gone in ["nobody", "acme/none", "acme/ml/none", "acme/ml/root@bucket"] {
            assert!(
                matches!(resolve(&t, &es, gone), Err(Unresolved::NotFound(_))),
                "{gone}"
            );
        }
        // A name with a / or an @ of its own.
        let odd = vec![vol(8, acme, ml, "a/b@c")];
        assert_eq!(
            resolve(&t, &odd, "acme/ml/a/b@c"),
            Ok(Path::Dataset(Id::Volume(8)))
        );
    }

    #[test]
    fn a_new_name_its_project_has_is_refused_and_copies_get_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("map.redb")).unwrap();
        let (t, acme, def, ml) = tenancy_with(&db);
        let es = vec![
            vol(1, acme, ml, "root"),
            vol(2, acme, ml, "root-copy"),
            vol(3, 0, 0, "image"),
        ];
        assert!(clash(&t, &es, acme, ml, "root").is_some());
        assert_eq!(clash(&t, &es, acme, def, "root"), None);
        assert_eq!(clash(&t, &es, acme, ml, ""), None);
        // In no project, as before.
        assert_eq!(clash(&t, &es, 0, 0, "image"), None);
        assert_eq!(free_name(&es, acme, ml, "root", "copy"), "root-copy-2");
        assert_eq!(free_name(&es, acme, def, "root", "copy"), "root-copy");
        assert_eq!(
            free_name(&es, acme, ml, "root", "restored"),
            "root-restored"
        );
        assert_eq!(free_name(&es, 0, 0, "image", "copy"), "image");
    }
}
