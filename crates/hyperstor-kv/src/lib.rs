//! Key–value ranges (25-kv-ranges.md §28): a sorted keyspace kept in range
//! objects of each table's index pool and found through a root in `.meta`.
//!
//! A [`Store`] is one keyspace with its own root. It holds tables; a key
//! is a row and a column; a [`Batch`] changes one row's columns atomically,
//! with assertions on them (D200). Reads come from a range's primary under
//! its read lease. A scan across ranges reads each in turn and is not a
//! snapshot.
//!
//! The store caches the root's entries and the ranges' attributes. Every
//! write asserts the range's epoch, and every read checks the attributes
//! that come with its keys, so a cache out of date costs another hop,
//! never a wrong answer (§28.3). This stage doesn't split or merge ranges
//! (§28.8); it follows ranges that are split, held or merged by others.

pub mod jobs;
pub mod keys;

use hyperstor_client::{Client, ClientError};
use hyperstor_codec::{Dec, DecodeError, Enc};
use hyperstor_map::META_POOL;
use hyperstor_proto::{Code, Looked, WireOp};
use hyperstor_reactor::time::sleep;
use keys::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use thiserror::Error;

/// Namespace of range objects and roots (matches
/// `hyperstor_engine::Namespace::Kv`).
pub const NS_KV: u8 = 4;
/// The S3 gateways' store (§28.1).
pub const S3_STORE: u64 = 1;

/// Entries read per lookup.
const PAGE: u32 = 1000;
/// Attempts at one read or write before giving up, and the wait between
/// attempts at a range that is held (a split's frozen half, §28.4): about
/// ten seconds in all.
const ATTEMPTS: usize = 200;
const HELD_WAIT: Duration = Duration::from_millis(50);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum KvError {
    #[error(transparent)]
    Client(#[from] ClientError),
    /// An assertion of the batch failed; nothing was changed.
    #[error("an assertion of the batch doesn't hold")]
    Conflict,
    #[error("no table {0:?}")]
    NoTable(String),
    #[error("table {0:?} exists")]
    TableExists(String),
    #[error("bad record: {0}")]
    Corrupt(String),
    #[error("gave up after {0} attempts: {1}")]
    Exhausted(usize, String),
}

impl From<DecodeError> for KvError {
    fn from(e: DecodeError) -> Self {
        KvError::Corrupt(e.to_string())
    }
}

fn shown(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

pub(crate) fn conflict(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Server {
            code: Code::Conflict,
            ..
        }
    )
}

/// A table, as its record in the root has it (§28.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: Vec<u8>,
    pub id: u64,
    /// Where its ranges live.
    pub index_pool: u32,
    /// Where its user keeps data the rows point to (D198).
    pub data_pool: u32,
    /// Its user's own record (S3's bucket record), kept with it.
    pub info: Vec<u8>,
}

/// The record's format. Later fields are appended, and a reader ignores
/// those it doesn't know, so an older library still finds the table.
const TABLE_V1: u8 = 1;

impl Table {
    fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(TABLE_V1)
            .u64(self.id)
            .u32(self.index_pool)
            .u32(self.data_pool)
            .bytes(&self.info);
        e.into_vec()
    }

    pub(crate) fn decode(name: &[u8], b: &[u8]) -> Result<Table, KvError> {
        let mut d = Dec::new(b);
        if d.u8()? != TABLE_V1 {
            return Err(KvError::Corrupt(format!(
                "table {:?}'s record",
                shown(name)
            )));
        }
        Ok(Table {
            name: name.to_vec(),
            id: d.u64()?,
            index_pool: d.u32()?,
            data_pool: d.u32()?,
            info: if d.remaining() > 0 {
                d.bytes()?.to_vec()
            } else {
                Vec::new()
            },
        })
    }
}

/// What a range is doing (§28.2, §28.4, §28.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Being filled; serves nothing yet.
    Pending,
    Serving,
    /// Its rows from `at` on are frozen while they are copied to `into`.
    Splitting {
        at: Vec<u8>,
        into: u128,
    },
    /// Taking in the rows of its right neighbour `from`.
    Absorbing {
        from: u128,
    },
    /// Its rows are frozen while `into` takes them in.
    Merging {
        into: u128,
    },
}

impl State {
    /// Its name, without the rows it names (user data).
    pub fn name(&self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Serving => "serving",
            State::Splitting { .. } => "splitting",
            State::Absorbing { .. } => "absorbing",
            State::Merging { .. } => "merging",
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        match self {
            State::Pending => e.u8(1),
            State::Serving => e.u8(2),
            State::Splitting { at, into } => e.u8(3).u128(*into).bytes(at),
            State::Absorbing { from } => e.u8(4).u128(*from),
            State::Merging { into } => e.u8(5).u128(*into),
        };
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<State, KvError> {
        let mut d = Dec::new(b);
        let s = match d.u8()? {
            1 => State::Pending,
            2 => State::Serving,
            3 => {
                let into = d.u128()?;
                State::Splitting {
                    at: d.bytes()?.to_vec(),
                    into,
                }
            }
            4 => State::Absorbing { from: d.u128()? },
            5 => State::Merging { into: d.u128()? },
            s => return Err(KvError::Corrupt(format!("range state {s}"))),
        };
        d.finish()?;
        Ok(s)
    }
}

/// A range object's attributes (§28.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub id: u128,
    pub store: u64,
    pub table: u64,
    /// The first row it holds.
    pub start: Vec<u8>,
    /// The first row it doesn't; none: the table's end.
    pub end: Option<Vec<u8>>,
    /// Grows with every change of `start`, `end` or `state`.
    pub epoch: u64,
    pub state: State,
    /// After a split: the range now holding the rows from `end` on, until
    /// the root names it.
    pub next: Option<u128>,
    /// After a merge: the neighbour whose rows it took, until the root no
    /// longer names that one and it is deleted.
    pub merged: Option<u128>,
    /// For a range a split made: the range it was split from, by which a
    /// range not yet in the root is known not to be left over (§28.5).
    pub parent: Option<u128>,
}

/// Attribute names of a range object.
pub mod attr {
    pub const STORE: &[u8] = b"store";
    pub const TABLE: &[u8] = b"table";
    pub const START: &[u8] = b"start";
    pub const END: &[u8] = b"end";
    pub const EPOCH: &[u8] = b"epoch";
    pub const STATE: &[u8] = b"state";
    pub const NEXT: &[u8] = b"next";
    pub const MERGED: &[u8] = b"merged";
    pub const PARENT: &[u8] = b"parent";
}

fn be<const N: usize>(name: &[u8], v: Option<&Vec<u8>>) -> Result<[u8; N], KvError> {
    v.and_then(|v| <[u8; N]>::try_from(v.as_slice()).ok())
        .ok_or_else(|| KvError::Corrupt(format!("range attribute {}", shown(name))))
}

impl Range {
    /// The attributes that make `self`, as `SetAttr` operations.
    pub fn set_ops(&self) -> Vec<WireOp> {
        let mut ops = vec![
            WireOp::SetAttr(attr::STORE.into(), self.store.to_be_bytes().into()),
            WireOp::SetAttr(attr::TABLE.into(), self.table.to_be_bytes().into()),
            WireOp::SetAttr(attr::START.into(), self.start.clone()),
            WireOp::SetAttr(attr::EPOCH.into(), self.epoch.to_be_bytes().into()),
            WireOp::SetAttr(attr::STATE.into(), self.state.encode()),
        ];
        ops.push(match &self.end {
            Some(e) => WireOp::SetAttr(attr::END.into(), e.clone()),
            None => WireOp::RmAttr(attr::END.into()),
        });
        for (name, v) in [
            (attr::NEXT, self.next),
            (attr::MERGED, self.merged),
            (attr::PARENT, self.parent),
        ] {
            ops.push(match v {
                Some(n) => WireOp::SetAttr(name.into(), n.to_be_bytes().into()),
                None => WireOp::RmAttr(name.into()),
            });
        }
        ops
    }

    pub fn from_attrs(id: u128, attrs: &[(Vec<u8>, Vec<u8>)]) -> Result<Range, KvError> {
        let get = |n: &[u8]| attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v);
        Ok(Range {
            id,
            store: u64::from_be_bytes(be(attr::STORE, get(attr::STORE))?),
            table: u64::from_be_bytes(be(attr::TABLE, get(attr::TABLE))?),
            start: get(attr::START)
                .cloned()
                .ok_or_else(|| KvError::Corrupt("range attribute start".into()))?,
            end: get(attr::END).cloned(),
            epoch: u64::from_be_bytes(be(attr::EPOCH, get(attr::EPOCH))?),
            state: State::decode(
                get(attr::STATE).ok_or_else(|| KvError::Corrupt("range attribute state".into()))?,
            )?,
            next: match get(attr::NEXT) {
                Some(_) => Some(u128::from_be_bytes(be(attr::NEXT, get(attr::NEXT))?)),
                None => None,
            },
            merged: match get(attr::MERGED) {
                Some(_) => Some(u128::from_be_bytes(be(attr::MERGED, get(attr::MERGED))?)),
                None => None,
            },
            parent: match get(attr::PARENT) {
                Some(_) => Some(u128::from_be_bytes(be(attr::PARENT, get(attr::PARENT))?)),
                None => None,
            },
        })
    }

    /// The assertion every write to the range carries (§28.2).
    pub fn epoch_assertion(&self) -> WireOp {
        WireOp::AssertAttr {
            name: attr::EPOCH.into(),
            value: Some(self.epoch.to_be_bytes().into()),
        }
    }

    fn holds(&self, row: &[u8]) -> bool {
        self.start.as_slice() <= row && self.end.as_deref().is_none_or(|e| row < e)
    }

    /// Whether it serves reads of `row`, which it holds: not while pending,
    /// nor once its rows are being merged away.
    fn reads(&self) -> bool {
        !matches!(self.state, State::Pending | State::Merging { .. })
    }

    /// Whether it takes writes to `row`, which it holds: not to a frozen
    /// half (D201).
    fn writes(&self, row: &[u8]) -> bool {
        match &self.state {
            State::Serving | State::Absorbing { .. } => true,
            State::Splitting { at, .. } => row < at.as_slice(),
            State::Pending | State::Merging { .. } => false,
        }
    }

    /// The first key past its rows.
    pub fn end_key(&self) -> Vec<u8> {
        match &self.end {
            Some(e) => row_start(e),
            None => vec![USER + 1],
        }
    }
}

/// The name of range `id`'s object.
pub fn range_name(id: u128) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

/// A root entry's value: the range and its epoch when the entry was
/// written, a hint (§28.3).
pub fn map_value(id: u128, epoch: u64) -> Vec<u8> {
    let mut e = Enc::new();
    e.u128(id).u64(epoch);
    e.into_vec()
}

/// A root entry's range and epoch.
pub fn map_entry(v: &[u8]) -> Result<(u128, u64), KvError> {
    let mut d = Dec::new(v);
    Ok((d.u128()?, d.u64()?))
}

pub(crate) fn random_u128() -> u128 {
    (u128::from(hyperstor_client::random_u64()) << 64) | u128::from(hyperstor_client::random_u64())
}

/// One change to a row's columns, in a [`Batch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    /// The column holds exactly this value; none: it is absent (D194).
    Assert(Vec<u8>, Option<Vec<u8>>),
    /// Object `name` of namespace `ns` in `pool` is deleted once the
    /// batch is applied, by the range's primary (D192, §28.6).
    Collect(Doomed),
}

/// Changes to one row, applied together if every assertion holds (D200).
/// Assertions see the row as it was before the batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    pub changes: Vec<Change>,
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    pub fn put(&mut self, column: &[u8], value: &[u8]) -> &mut Self {
        self.changes.push(Change::Put(column.into(), value.into()));
        self
    }

    pub fn delete(&mut self, column: &[u8]) -> &mut Self {
        self.changes.push(Change::Delete(column.into()));
        self
    }

    pub fn assert(&mut self, column: &[u8], value: Option<&[u8]>) -> &mut Self {
        self.changes
            .push(Change::Assert(column.into(), value.map(|v| v.to_vec())));
        self
    }

    /// Object `name` of namespace `ns` in `pool` is to be deleted once
    /// the batch applies (§28.6).
    pub fn collect(&mut self, pool: u32, ns: u8, name: &[u8]) -> &mut Self {
        self.changes.push(Change::Collect(Doomed {
            pool,
            ns,
            name: name.into(),
            count: 0,
        }));
        self
    }

    /// Objects `prefix` + 0, `prefix` + 1, … `prefix` + (`count` − 1), each
    /// number a big-endian u32, are to be deleted once the batch applies:
    /// a large S3 object's chunks in one queue entry (§28.6).
    pub fn collect_series(&mut self, pool: u32, ns: u8, prefix: &[u8], count: u32) -> &mut Self {
        if count > 0 {
            self.changes.push(Change::Collect(Doomed {
                pool,
                ns,
                name: prefix.into(),
                count,
            }));
        }
        self
    }

    /// The batch as operations on its range. The objects it collects
    /// become one queue entry, under a random id made here, so that the
    /// batch sent again (after an error that leaves it unknown whether it
    /// applied) puts the same entry.
    fn ops(&self, row: &[u8]) -> Vec<WireOp> {
        let mut collect = Vec::new();
        let mut ops: Vec<WireOp> = Vec::new();
        for c in &self.changes {
            ops.push(match c {
                Change::Put(k, v) => WireOp::KvPut(user_key(row, k), v.clone()),
                Change::Delete(k) => WireOp::KvDel(user_key(row, k)),
                Change::Assert(k, v) => WireOp::AssertKv {
                    key: user_key(row, k),
                    value: v.clone(),
                },
                Change::Collect(d) => {
                    collect.push(d.clone());
                    continue;
                }
            });
        }
        if !collect.is_empty() {
            ops.push(WireOp::KvPut(
                queue_key(random_u128()),
                encode_queue(&collect),
            ));
        }
        ops
    }
}

/// Objects a queue entry names, in namespace `ns` of `pool`: `name`
/// alone if `count` is 0, else the series `name` + i (a big-endian u32)
/// for i below `count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doomed {
    pub pool: u32,
    pub ns: u8,
    pub name: Vec<u8>,
    pub count: u32,
}

impl Doomed {
    /// The names of the objects, in order.
    pub fn names(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        let series = (0..self.count).map(|i| [&self.name[..], &i.to_be_bytes()].concat());
        (self.count == 0)
            .then(|| self.name.clone())
            .into_iter()
            .chain(series)
    }
}

/// What a queue entry names.
pub type Collected = Vec<Doomed>;

pub fn encode_queue(objects: &[Doomed]) -> Vec<u8> {
    let mut e = Enc::new();
    e.seq(objects, |e, d| {
        e.u32(d.pool).u8(d.ns).bytes(&d.name).u32(d.count);
    });
    e.into_vec()
}

pub fn decode_queue(b: &[u8]) -> Result<Collected, KvError> {
    let mut d = Dec::new(b);
    let objects = d.seq(13, |d| {
        Ok(Doomed {
            pool: d.u32()?,
            ns: d.u8()?,
            name: d.bytes()?.to_vec(),
            count: d.u32()?,
        })
    })?;
    d.finish()?;
    Ok(objects)
}

/// A row's columns and their values, in column order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub row: Vec<u8>,
    pub columns: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Which rows a [`Store::scan`] reads.
#[derive(Debug, Clone, Default)]
pub struct Scan<'a> {
    /// The first row, if present.
    pub from: &'a [u8],
    /// The first row past them; none: the table's end.
    pub to: Option<&'a [u8]>,
    /// Only rows that begin with this.
    pub prefix: &'a [u8],
    /// At most this many rows (at least one).
    pub limit: usize,
}

/// A scan's rows, and where the next page starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub rows: Vec<Row>,
    /// The next page's `from`, if there are more rows.
    pub more: Option<Vec<u8>>,
}

/// A table's ranges by start row, as the root names them.
type Routes = Rc<Vec<(Vec<u8>, u128)>>;

/// A keyspace with its own root (§28.1).
pub struct Store {
    client: Rc<Client>,
    id: u64,
    /// Each table's ranges as the root names them, by start row.
    routes: RefCell<HashMap<u64, Routes>>,
    /// Ranges' attributes as last read.
    ranges: RefCell<HashMap<u128, Rc<Range>>>,
}

/// Why a range was read: what it must serve.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    Read,
    Write,
}

impl Store {
    /// Store `id`. Its root is made with its first table.
    pub fn open(client: Rc<Client>, id: u64) -> Store {
        Store {
            client,
            id,
            routes: RefCell::default(),
            ranges: RefCell::default(),
        }
    }

    pub fn client(&self) -> &Rc<Client> {
        &self.client
    }

    /// The root's object name: the store id (D197).
    fn root(&self) -> Vec<u8> {
        self.id.to_be_bytes().to_vec()
    }

    async fn root_exec(&self, ops: &[WireOp]) -> Result<(), ClientError> {
        self.client.exec(META_POOL, NS_KV, &self.root(), ops).await
    }

    /// The root's entries in `[from, to)`, every page.
    async fn root_scan(
        &self,
        from: &[u8],
        to: &[u8],
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        let mut out = Vec::new();
        let mut from = from.to_vec();
        while out.len() < limit {
            let want = (limit - out.len()).min(PAGE as usize) as u32;
            let Some(l) = self
                .client
                .lookup(META_POOL, NS_KV, &self.root(), &from, Some(to), want)
                .await?
            else {
                break;
            };
            let full = l.kv.len() == want as usize;
            if let Some((k, _)) = l.kv.last() {
                from = k.clone();
                from.push(0);
            }
            out.extend(l.kv);
            if !full {
                break;
            }
        }
        Ok(out)
    }

    /// Creates table `name`, its ranges in `index_pool` (§28.3).
    pub async fn create_table(
        &self,
        name: &[u8],
        index_pool: u32,
        data_pool: u32,
    ) -> Result<Table, KvError> {
        self.create_table_with(name, index_pool, data_pool, &[])
            .await
    }

    /// [`Self::create_table`], with its user's record `info`.
    pub async fn create_table_with(
        &self,
        name: &[u8],
        index_pool: u32,
        data_pool: u32,
        info: &[u8],
    ) -> Result<Table, KvError> {
        let table = Table {
            name: name.to_vec(),
            id: hyperstor_client::random_u64(),
            index_pool,
            data_pool,
            info: info.to_vec(),
        };
        let first = Range {
            id: random_u128(),
            store: self.id,
            table: table.id,
            start: Vec::new(),
            end: None,
            epoch: 1,
            state: State::Serving,
            next: None,
            merged: None,
            parent: None,
        };
        let mut ops = vec![WireOp::AssertAbsent];
        ops.extend(first.set_ops());
        self.client
            .exec(index_pool, NS_KV, &range_name(first.id), &ops)
            .await?;
        let made = self
            .root_exec(&[
                WireOp::AssertKv {
                    key: table_key(name),
                    value: None,
                },
                WireOp::KvPut(table_key(name), table.encode()),
                WireOp::KvPut(id_key(table.id), name.to_vec()),
                WireOp::KvPut(map_key(table.id, b""), map_value(first.id, first.epoch)),
            ])
            .await;
        match made {
            Ok(()) => Ok(table),
            Err(e) => {
                // Applied or not, unknown: the record says.
                if !conflict(&e) && self.table(name).await? == Some(table.clone()) {
                    return Ok(table);
                }
                // Nothing names the range: delete it now rather than leave it
                // to the leftover check (§28.5).
                let _ = self
                    .client
                    .exec(index_pool, NS_KV, &range_name(first.id), &[WireOp::Delete])
                    .await;
                Err(if conflict(&e) {
                    KvError::TableExists(shown(name))
                } else {
                    e.into()
                })
            }
        }
    }

    /// Table `name`, if it exists.
    pub async fn table(&self, name: &[u8]) -> Result<Option<Table>, KvError> {
        let k = table_key(name);
        let mut to = k.clone();
        to.push(0);
        match self.root_scan(&k, &to, 1).await?.first() {
            Some((_, v)) => Ok(Some(Table::decode(name, v)?)),
            None => Ok(None),
        }
    }

    /// Up to `limit` tables, by name, from `from` on.
    pub async fn tables(&self, from: &[u8], limit: usize) -> Result<Vec<Table>, KvError> {
        self.root_scan(&table_key(from), b"u", limit)
            .await?
            .iter()
            .map(|(k, v)| Table::decode(&k[1..], v))
            .collect()
    }

    /// Deletes table `name` and all its rows (§28.3). A delete that stops
    /// half-way leaves the name free and the rest to delete again.
    pub async fn delete_table(&self, name: &[u8]) -> Result<(), KvError> {
        let Some(table) = self.table(name).await? else {
            return Err(KvError::NoTable(shown(name)));
        };
        match self
            .root_exec(&[
                WireOp::AssertKv {
                    key: table_key(name),
                    value: Some(table.encode()),
                },
                WireOp::KvDel(table_key(name)),
            ])
            .await
        {
            Err(e) if conflict(&e) => return Err(KvError::NoTable(shown(name))),
            r => r?,
        }
        self.drop_ranges(&table).await
    }

    /// Deletes a table's ranges, then its entries in the root.
    async fn drop_ranges(&self, table: &Table) -> Result<(), KvError> {
        let p = map_prefix(table.id);
        let end = prefix_end(&p).expect("a map prefix is never all 0xFF");
        for (_, v) in self.root_scan(&p, &end, usize::MAX).await? {
            let (id, _) = map_entry(&v)?;
            if let Some(r) = self.read_range(table, id).await? {
                jobs::drain_all(&self.client, table.index_pool, &r).await?;
            }
            self.client
                .exec(table.index_pool, NS_KV, &range_name(id), &[WireOp::Delete])
                .await?;
            self.ranges.borrow_mut().remove(&id);
        }
        self.root_exec(&[WireOp::KvDelRange(p, end), WireOp::KvDel(id_key(table.id))])
            .await?;
        self.routes.borrow_mut().remove(&table.id);
        Ok(())
    }

    /// Table `t`'s ranges by start row, as the root has them.
    async fn routes(&self, t: &Table, fresh: bool) -> Result<Routes, KvError> {
        if !fresh && let Some(r) = self.routes.borrow().get(&t.id) {
            return Ok(r.clone());
        }
        let p = map_prefix(t.id);
        let end = prefix_end(&p).expect("a map prefix is never all 0xFF");
        let mut routes = Vec::new();
        for (k, v) in self.root_scan(&p, &end, usize::MAX).await? {
            let start =
                map_start(t.id, &k).ok_or_else(|| KvError::Corrupt("a root entry's key".into()))?;
            routes.push((start, map_entry(&v)?.0));
        }
        if routes.is_empty() {
            return Err(KvError::NoTable(shown(&t.name)));
        }
        let routes = Rc::new(routes);
        self.routes.borrow_mut().insert(t.id, routes.clone());
        Ok(routes)
    }

    /// Range `id`'s attributes, read again.
    async fn read_range(&self, t: &Table, id: u128) -> Result<Option<Rc<Range>>, KvError> {
        let l = self
            .client
            .lookup(t.index_pool, NS_KV, &range_name(id), b"", None, 0)
            .await?;
        Ok(match l {
            Some(l) => Some(self.remember(id, &l)?),
            None => {
                self.ranges.borrow_mut().remove(&id);
                None
            }
        })
    }

    fn remember(&self, id: u128, l: &Looked) -> Result<Rc<Range>, KvError> {
        let r = Rc::new(Range::from_attrs(id, &l.attrs)?);
        self.ranges.borrow_mut().insert(id, r.clone());
        Ok(r)
    }

    /// The range of table `t` that serves `row` for `what`, following
    /// splits, merges and holds (§28.3, §28.4).
    async fn range_for(&self, t: &Table, row: &[u8], what: Use) -> Result<Rc<Range>, KvError> {
        let mut fresh = false;
        // A range named by another's `next` rather than by the root.
        let mut hint: Option<u128> = None;
        let mut why = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 1 {
                sleep(HELD_WAIT).await;
            }
            let id = match hint.take() {
                Some(id) => id,
                None => {
                    let routes = self.routes(t, fresh).await?;
                    let i = routes.partition_point(|(s, _)| s.as_slice() <= row);
                    // The first range starts at the empty row, the least.
                    let Some(i) = i.checked_sub(1) else {
                        fresh = true;
                        why = "no range starts at or before the row".into();
                        continue;
                    };
                    routes[i].1
                }
            };
            let cached = self.ranges.borrow().get(&id).cloned();
            let r = match cached {
                Some(r) if !fresh => r,
                _ => match self.read_range(t, id).await? {
                    Some(r) => r,
                    None => {
                        fresh = true;
                        why = format!("range {id:032x} is gone");
                        continue;
                    }
                },
            };
            if r.store != self.id || r.table != t.id {
                return Err(KvError::Corrupt(format!(
                    "range {id:032x} belongs to store {} table {}",
                    r.store, r.table
                )));
            }
            if !r.holds(row) {
                if r.end.as_deref().is_some_and(|e| row >= e) {
                    hint = r.next;
                }
                self.ranges.borrow_mut().remove(&id);
                fresh = true;
                why = format!("range {id:032x} doesn't hold the row");
                continue;
            }
            let ok = match what {
                Use::Read => r.reads(),
                Use::Write => r.writes(row),
            };
            if !ok {
                // Held: wait and read it again.
                self.ranges.borrow_mut().remove(&id);
                why = format!("range {id:032x} is {:?}", r.state);
                continue;
            }
            return Ok(r);
        }
        Err(KvError::Exhausted(ATTEMPTS, why))
    }

    /// Applies `batch` to `row` of table `t` if its assertions hold:
    /// [`KvError::Conflict`] if one doesn't, and nothing changes (D200).
    pub async fn apply(&self, t: &Table, row: &[u8], batch: &Batch) -> Result<(), KvError> {
        let changes = batch.ops(row);
        for _ in 0..ATTEMPTS {
            let r = self.range_for(t, row, Use::Write).await?;
            let mut ops = vec![r.epoch_assertion()];
            ops.extend(changes.iter().cloned());
            match self
                .client
                .exec(t.index_pool, NS_KV, &range_name(r.id), &ops)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if conflict(&e) => {
                    // The epoch, or the caller's assertion? The range now says.
                    match self.read_range(t, r.id).await? {
                        Some(now) if now.epoch == r.epoch => return Err(KvError::Conflict),
                        _ => continue,
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(KvError::Exhausted(
            ATTEMPTS,
            "the range kept changing".into(),
        ))
    }

    /// Up to `limit` keys of table `t` in `[from, to)`, from the range that
    /// holds row `row`, which `from` must be in; `to` is cut at the range's
    /// end. The keys and the range's attributes come from one read, so the
    /// range is the one the keys were read under.
    async fn read_keys(
        &self,
        t: &Table,
        row: &[u8],
        from: &[u8],
        to: &[u8],
        limit: u32,
    ) -> Result<(Rc<Range>, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>), KvError> {
        for _ in 0..ATTEMPTS {
            let r = self.range_for(t, row, Use::Read).await?;
            let end = r.end_key();
            let to = to.min(end.as_slice()).to_vec();
            let l = self
                .client
                .lookup(
                    t.index_pool,
                    NS_KV,
                    &range_name(r.id),
                    from,
                    Some(&to),
                    limit,
                )
                .await?;
            let Some(l) = l else {
                self.ranges.borrow_mut().remove(&r.id);
                continue;
            };
            let now = self.remember(r.id, &l)?;
            // Bounds and state change only with the epoch.
            if now.epoch == r.epoch {
                return Ok((now, to, l.kv));
            }
        }
        Err(KvError::Exhausted(
            ATTEMPTS,
            "the range kept changing".into(),
        ))
    }

    /// The value of `column` in `row`, if present.
    pub async fn get(
        &self,
        t: &Table,
        row: &[u8],
        column: &[u8],
    ) -> Result<Option<Vec<u8>>, KvError> {
        let k = user_key(row, column);
        let mut to = k.clone();
        to.push(0);
        let (_, _, kv) = self.read_keys(t, row, &k, &to, 1).await?;
        Ok(kv.into_iter().next().map(|(_, v)| v))
    }

    /// Every column of `row`, in order.
    pub async fn row(&self, t: &Table, row: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        let start = row_start(row);
        let end = row_end(row);
        let mut from = start.clone();
        let mut out = Vec::new();
        loop {
            let (_, _, kv) = self.read_keys(t, row, &from, &end, PAGE).await?;
            let full = kv.len() == PAGE as usize;
            if let Some((k, _)) = kv.last() {
                from = k.clone();
                from.push(0);
            }
            out.extend(kv.into_iter().map(|(k, v)| (k[start.len()..].to_vec(), v)));
            if !full {
                return Ok(out);
            }
        }
    }

    /// The rows of table `t` that `scan` selects, in order, each whole.
    pub async fn scan(&self, t: &Table, scan: &Scan<'_>) -> Result<Page, KvError> {
        let limit = scan.limit.max(1);
        let first = scan.from.max(scan.prefix);
        let mut row = first.to_vec();
        let mut from = row_start(first);
        let mut to = prefix_end(&prefix_start(scan.prefix)).expect("user keys begin with r");
        if let Some(t) = scan.to {
            to = to.min(row_start(t));
        }
        let mut rows: Vec<Row> = Vec::new();
        // Until a row past the limit shows the last one is whole.
        while rows.len() <= limit && from < to {
            let (r, cut, kv) = self.read_keys(t, &row, &from, &to, PAGE).await?;
            let full = kv.len() == PAGE as usize;
            for (k, v) in kv {
                let (kr, column) =
                    split_key(&k).ok_or_else(|| KvError::Corrupt("a user key".into()))?;
                let column = column.to_vec();
                match rows.last_mut() {
                    Some(last) if last.row == kr => last.columns.push((column, v)),
                    _ => rows.push(Row {
                        row: kr,
                        columns: vec![(column, v)],
                    }),
                }
                from = k;
                from.push(0);
            }
            if let Some(last) = rows.last() {
                row = last.row.clone();
            }
            if !full {
                // The range had nothing more before `cut`: on to the next.
                match &r.end {
                    Some(e) if cut < to => {
                        row = e.clone();
                        from = row_start(e);
                    }
                    _ => break,
                }
            }
        }
        let more = (rows.len() > limit).then(|| rows[limit].row.clone());
        rows.truncate(limit);
        Ok(Page { rows, more })
    }
}
