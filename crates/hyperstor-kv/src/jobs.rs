//! Splits, merges, and taking up those that stopped (25-kv-ranges.md
//! §28.4, §28.5). `hyperstord` runs them for the ranges it is primary of,
//! through its client; nothing ties them to that client.
//!
//! Each step is one operation on one object and asserts what the step
//! before left, so a job that finds a range changed under it stops, and
//! of two jobs on one range the one behind fails its next step.
//! [`resume`] finishes or undoes what a stopped job left, from the
//! attributes alone: a split whose new range is open is finished, one
//! whose new range isn't is undone; a merge that extended its range is
//! finished, one that didn't is undone.

use crate::keys::*;
use crate::{
    KvError, NS_KV, Range, State, Table, attr, conflict, decode_queue, map_entry, map_value,
    random_u128, range_name,
};
use hyperstor_client::{Client, ClientError};
use hyperstor_map::META_POOL;
use hyperstor_proto::WireOp;
use std::cell::Cell;

/// When a range splits or merges (D202).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A range holding more than this splits.
    pub split_bytes: u64,
    /// A range and its right neighbour holding less than this together
    /// merge: a quarter of the split size, so halves don't merge back.
    pub merge_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            split_bytes: 64 << 20,
            merge_bytes: 16 << 20,
        }
    }
}

/// How many steps a job may take: all, or in tests a few, to leave it
/// where a crash would.
pub struct Steps(Cell<usize>);

impl Steps {
    pub fn all() -> Steps {
        Steps(Cell::new(usize::MAX))
    }

    pub fn only(n: usize) -> Steps {
        Steps(Cell::new(n))
    }

    fn take(&self) -> bool {
        let n = self.0.get();
        self.0.set(n.saturating_sub(1));
        n > 0
    }
}

/// What [`check`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Nothing,
    /// Split; the upper rows are now in this range.
    Split(u128),
    /// Took in this right neighbour's rows.
    Merged(u128),
}

/// Entries read per lookup while copying, and bytes per list of copies.
const COPY_PAGE: u32 = 256;
const COPY_BYTES: usize = 1 << 20;

fn state_is(s: &State) -> WireOp {
    WireOp::AssertAttr {
        name: attr::STATE.into(),
        value: Some(s.encode()),
    }
}

/// Range `id`'s attributes and its table's bytes.
pub async fn read(cl: &Client, pool: u32, id: u128) -> Result<Option<(Range, u64)>, KvError> {
    let name = range_name(id);
    match cl.lookup_sized(pool, NS_KV, &name, b"", None, 0).await? {
        None => Ok(None),
        Some(l) => Ok(Some((
            Range::from_attrs(id, &l.attrs)?,
            l.kv_bytes.unwrap_or(0),
        ))),
    }
}

async fn exec(cl: &Client, pool: u32, id: u128, ops: &[WireOp]) -> Result<(), ClientError> {
    cl.exec(pool, NS_KV, &range_name(id), ops).await
}

/// Whether a step went through: false on `Conflict`, when the range
/// changed and the job stops.
fn went(r: Result<(), ClientError>) -> Result<bool, KvError> {
    match r {
        Ok(()) => Ok(true),
        Err(e) if conflict(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Whether a range shows a job not finished, or one that stopped.
pub fn unfinished(r: &Range) -> bool {
    r.state != State::Serving || r.next.is_some() || r.merged.is_some()
}

/// A primary's look at range `id`: splits it or merges it with its right
/// neighbour by size (D202). A range in the middle of a job is left to
/// that job, or to [`resume`].
pub async fn check(cl: &Client, pool: u32, id: u128, limits: &Limits) -> Result<Done, KvError> {
    let Some((r, bytes)) = read(cl, pool, id).await? else {
        return Ok(Done::Nothing);
    };
    if unfinished(&r) {
        return Ok(Done::Nothing);
    }
    if bytes > limits.split_bytes {
        return Ok(match split(cl, pool, &r, &Steps::all()).await? {
            Some(n) => Done::Split(n),
            None => Done::Nothing,
        });
    }
    if bytes < limits.merge_bytes
        && let Some((n, more)) = right_neighbour(cl, pool, &r).await?
        && bytes + more < limits.merge_bytes
        && merge(cl, pool, &r, &n, &Steps::all()).await?
    {
        return Ok(Done::Merged(n.id));
    }
    Ok(Done::Nothing)
}

/// The root's name of store `store`.
fn root(store: u64) -> Vec<u8> {
    store.to_be_bytes().to_vec()
}

/// The root's entry for the range of `r`'s table that starts at `start`,
/// and its value.
async fn root_get(
    cl: &Client,
    r: &Range,
    start: &[u8],
) -> Result<Option<(u128, Vec<u8>)>, KvError> {
    let k = map_key(r.table, start);
    let mut to = k.clone();
    to.push(0);
    let name = root(r.store);
    let l = cl.lookup(META_POOL, NS_KV, &name, &k, Some(&to), 1).await?;
    match l.and_then(|l| l.kv.into_iter().next()) {
        Some((_, v)) => Ok(Some((map_entry(&v)?.0, v))),
        None => Ok(None),
    }
}

/// Makes the root name range `id` as starting at `start`: true once it
/// does. An entry there for a range that no longer exists is replaced.
async fn root_put(
    cl: &Client,
    pool: u32,
    r: &Range,
    start: &[u8],
    id: u128,
) -> Result<bool, KvError> {
    let k = map_key(r.table, start);
    let have = root_get(cl, r, start).await?;
    let value = match &have {
        Some((i, _)) if *i == id => return Ok(true),
        Some((i, _)) if read(cl, pool, *i).await?.is_some() => return Ok(false),
        h => h.as_ref().map(|(_, v)| v.clone()),
    };
    let ops = [
        WireOp::AssertKv {
            key: k.clone(),
            value,
        },
        WireOp::KvPut(k, map_value(id, 1)),
    ];
    went(cl.exec(META_POOL, NS_KV, &root(r.store), &ops).await)
}

/// The range that starts where `r` ends, if the root names it, it is of
/// the same table and nothing is under way in it, and its bytes.
async fn right_neighbour(
    cl: &Client,
    pool: u32,
    r: &Range,
) -> Result<Option<(Range, u64)>, KvError> {
    let Some(end) = &r.end else {
        return Ok(None);
    };
    let Some((id, _)) = root_get(cl, r, end).await? else {
        return Ok(None);
    };
    Ok(read(cl, pool, id).await?.filter(|(n, _)| {
        n.store == r.store && n.table == r.table && n.start == *end && !unfinished(n)
    }))
}

/// The row that halves `r` by bytes; none if it has one row. Never its
/// first row, so neither half is empty.
async fn middle(cl: &Client, pool: u32, r: &Range) -> Result<Option<Vec<u8>>, KvError> {
    // Each row, and the bytes before it.
    let mut rows: Vec<(Vec<u8>, u64)> = Vec::new();
    let (mut total, mut from, to) = (0u64, vec![USER], r.end_key());
    let name = range_name(r.id);
    loop {
        let l = cl.lookup(pool, NS_KV, &name, &from, Some(&to), COPY_PAGE);
        let Some(l) = l.await? else {
            return Ok(None);
        };
        for (k, v) in &l.kv {
            let (row, _) = split_key(k).ok_or_else(|| KvError::Corrupt("a user key".into()))?;
            if rows.last().is_none_or(|(r, _)| *r != row) {
                rows.push((row, total));
            }
            total += (k.len() + v.len()) as u64;
        }
        match l.kv.last() {
            Some((k, _)) if l.kv.len() == COPY_PAGE as usize => {
                from = k.clone();
                from.push(0);
            }
            _ => break,
        }
    }
    let past_half = rows.iter().skip(1).find(|(_, before)| *before >= total / 2);
    Ok(past_half
        .or(rows.iter().skip(1).last())
        .map(|(r, _)| r.clone()))
}

/// Copies range `from`'s keys in `[lo, hi)` into range `into`, each list
/// asserting `into` is still `state`: false if it isn't, or `from` is
/// gone.
async fn copy(
    cl: &Client,
    pool: u32,
    from: u128,
    (lo, hi): (&[u8], &[u8]),
    into: u128,
    state: &State,
) -> Result<bool, KvError> {
    let mut at = lo.to_vec();
    let name = range_name(from);
    loop {
        let l = cl.lookup(pool, NS_KV, &name, &at, Some(hi), COPY_PAGE);
        let Some(l) = l.await? else {
            return Ok(false);
        };
        let full = l.kv.len() == COPY_PAGE as usize;
        if let Some((k, _)) = l.kv.last() {
            at = k.clone();
            at.push(0);
        }
        let (mut ops, mut bytes) = (vec![state_is(state)], 0);
        for (k, v) in l.kv {
            bytes += k.len() + v.len();
            ops.push(WireOp::KvPut(k, v));
            if bytes >= COPY_BYTES {
                if !went(exec(cl, pool, into, &ops).await)? {
                    return Ok(false);
                }
                (ops, bytes) = (vec![state_is(state)], 0);
            }
        }
        if ops.len() > 1 && !went(exec(cl, pool, into, &ops).await)? {
            return Ok(false);
        }
        if !full {
            return Ok(true);
        }
    }
}

/// Splits `r` at its middle row (§28.4), within `steps`: the new range's
/// id once the first step is done, none if `r` has one row or changed.
pub async fn split(
    cl: &Client,
    pool: u32,
    r: &Range,
    steps: &Steps,
) -> Result<Option<u128>, KvError> {
    match middle(cl, pool, r).await? {
        Some(at) => split_at(cl, pool, r, &at, steps).await,
        None => Ok(None),
    }
}

/// Splits `r` at row `at`, which it holds and which isn't its first.
pub async fn split_at(
    cl: &Client,
    pool: u32,
    r: &Range,
    at: &[u8],
    steps: &Steps,
) -> Result<Option<u128>, KvError> {
    let into = random_u128();
    // 1. Freeze the upper half.
    let frozen = Range {
        epoch: r.epoch + 1,
        state: State::Splitting {
            at: at.to_vec(),
            into,
        },
        ..r.clone()
    };
    let mut ops = vec![r.epoch_assertion()];
    ops.extend(frozen.set_ops());
    if !steps.take() || !went(exec(cl, pool, r.id, &ops).await)? {
        return Ok(None);
    }
    // 2. Create the upper range, pending.
    let upper = Range {
        id: into,
        start: at.to_vec(),
        epoch: 1,
        state: State::Pending,
        next: None,
        merged: None,
        parent: Some(r.id),
        ..r.clone()
    };
    let mut ops = vec![WireOp::AssertAbsent];
    ops.extend(upper.set_ops());
    if !steps.take() || !went(exec(cl, pool, into, &ops).await)? {
        return Ok(Some(into));
    }
    // 3. Copy the frozen rows.
    let keys = (row_start(at), frozen.end_key());
    if !steps.take() || !copy(cl, pool, r.id, (&keys.0, &keys.1), into, &State::Pending).await? {
        return Ok(Some(into));
    }
    // 4. Open it.
    let ops = [
        state_is(&State::Pending),
        WireOp::SetAttr(attr::STATE.into(), State::Serving.encode()),
        WireOp::SetAttr(attr::EPOCH.into(), 2u64.to_be_bytes().into()),
    ];
    if !steps.take() || !went(exec(cl, pool, into, &ops).await)? {
        return Ok(Some(into));
    }
    finish_split(cl, pool, &frozen, steps).await?;
    Ok(Some(into))
}

/// Steps 5 and 6 of a split whose new range is open: shrink, then the
/// root.
async fn finish_split(
    cl: &Client,
    pool: u32,
    frozen: &Range,
    steps: &Steps,
) -> Result<(), KvError> {
    let State::Splitting { at, into } = &frozen.state else {
        unreachable!("only a splitting range is finished");
    };
    // 5. Shrink.
    let lower = Range {
        end: Some(at.clone()),
        next: Some(*into),
        epoch: frozen.epoch + 1,
        state: State::Serving,
        ..frozen.clone()
    };
    let mut ops = vec![frozen.epoch_assertion()];
    ops.extend(lower.set_ops());
    ops.push(WireOp::KvDelRange(row_start(at), frozen.end_key()));
    if !steps.take() {
        return Ok(());
    }
    if !went(exec(cl, pool, frozen.id, &ops).await)? {
        // Shrunk by another job, which named the new range, or it is
        // named by nothing and never will be: then it goes, if it is
        // still as opened.
        let now = read(cl, pool, frozen.id).await?;
        let named = now.is_some_and(|(r, _)| r.next == Some(*into))
            || root_get(cl, frozen, at)
                .await?
                .is_some_and(|(i, _)| i == *into);
        if !named {
            let opened = Range {
                id: *into,
                start: at.clone(),
                epoch: 2,
                state: State::Serving,
                next: None,
                merged: None,
                parent: Some(frozen.id),
                ..frozen.clone()
            };
            let ops = [opened.epoch_assertion(), WireOp::Delete];
            went(exec(cl, pool, *into, &ops).await)?;
        }
        return Ok(());
    }
    // 6. The root.
    if steps.take() {
        name_next(cl, pool, &lower).await?;
    }
    Ok(())
}

/// Makes the root name `r.next`, then clears it.
async fn name_next(cl: &Client, pool: u32, r: &Range) -> Result<(), KvError> {
    let (Some(next), Some(end)) = (r.next, &r.end) else {
        return Ok(());
    };
    if root_put(cl, pool, r, end, next).await? {
        let ops = [r.epoch_assertion(), WireOp::RmAttr(attr::NEXT.into())];
        went(exec(cl, pool, r.id, &ops).await)?;
    }
    Ok(())
}

/// Merges `l` with its right neighbour `n` (§28.5), within `steps`: true
/// once `l` holds `n`'s rows.
pub async fn merge(
    cl: &Client,
    pool: u32,
    l: &Range,
    n: &Range,
    steps: &Steps,
) -> Result<bool, KvError> {
    // 1. Mark the left range absorbing.
    let absorbing = Range {
        epoch: l.epoch + 1,
        state: State::Absorbing { from: n.id },
        ..l.clone()
    };
    let mut ops = vec![l.epoch_assertion()];
    ops.extend(absorbing.set_ops());
    if !steps.take() || !went(exec(cl, pool, l.id, &ops).await)? {
        return Ok(false);
    }
    // 2. Freeze the right one, if it still starts where the left ends.
    let merging = Range {
        epoch: n.epoch + 1,
        state: State::Merging { into: l.id },
        ..n.clone()
    };
    let mut ops = vec![
        n.epoch_assertion(),
        WireOp::AssertAttr {
            name: attr::START.into(),
            value: l.end.clone(),
        },
    ];
    ops.extend(merging.set_ops());
    if !steps.take() {
        return Ok(false);
    }
    if !went(exec(cl, pool, n.id, &ops).await)? {
        undo_absorbing(cl, pool, &absorbing).await?;
        return Ok(false);
    }
    // 3. Copy its rows; the left range's bounds still exclude them.
    if !steps.take()
        || !copy(
            cl,
            pool,
            n.id,
            // Its queue too (§28.6): the entries' ids are random, so
            // they don't collide with the left range's.
            (&[QUEUE], &n.end_key()),
            l.id,
            &absorbing.state,
        )
        .await?
    {
        return Ok(false);
    }
    // 4. Extend the left range over them.
    let extended = Range {
        end: n.end.clone(),
        epoch: absorbing.epoch + 1,
        state: State::Serving,
        merged: Some(n.id),
        ..absorbing.clone()
    };
    let mut ops = vec![absorbing.epoch_assertion()];
    ops.extend(extended.set_ops());
    if !steps.take() || !went(exec(cl, pool, l.id, &ops).await)? {
        return Ok(false);
    }
    finish_merge(cl, pool, &extended, steps).await?;
    Ok(true)
}

/// Steps 5–7 of a merge whose left range is extended: the root no longer
/// names the right one, which is then deleted, and `merged` is cleared.
async fn finish_merge(cl: &Client, pool: u32, l: &Range, steps: &Steps) -> Result<(), KvError> {
    let Some(n) = l.merged else {
        return Ok(());
    };
    // 5. The root. The right range starts where the left one ended, and
    // the entry there names it until this removes it.
    if let Some((nn, _)) = read(cl, pool, n).await? {
        if !steps.take() {
            return Ok(());
        }
        if let Some((id, v)) = root_get(cl, l, &nn.start).await?
            && id == n
        {
            let k = map_key(l.table, &nn.start);
            let ops = [
                WireOp::AssertKv {
                    key: k.clone(),
                    value: Some(v),
                },
                WireOp::KvDel(k),
            ];
            if !went(cl.exec(META_POOL, NS_KV, &root(l.store), &ops).await)? {
                return Ok(());
            }
        }
        // 6. Delete it.
        if !steps.take() {
            return Ok(());
        }
        let ops = [state_is(&State::Merging { into: l.id }), WireOp::Delete];
        if !went(exec(cl, pool, n, &ops).await)? {
            return Ok(());
        }
    }
    // 7. Done.
    if steps.take() {
        let ops = [l.epoch_assertion(), WireOp::RmAttr(attr::MERGED.into())];
        went(exec(cl, pool, l.id, &ops).await)?;
    }
    Ok(())
}

/// Undoes a merge before its left range was extended: the right range
/// serves again if it was frozen, and the left one drops what was copied
/// past its end.
async fn undo_absorbing(cl: &Client, pool: u32, l: &Range) -> Result<(), KvError> {
    let State::Absorbing { from } = l.state else {
        return Ok(());
    };
    if let Some((n, _)) = read(cl, pool, from).await?
        && n.state == (State::Merging { into: l.id })
    {
        serve_again(cl, pool, &n, Vec::new()).await?;
    }
    let past = match &l.end {
        Some(e) => vec![WireOp::KvDelRange(row_start(e), vec![USER + 1])],
        None => Vec::new(),
    };
    serve_again(cl, pool, l, past).await
}

/// Sets `r` serving at a new epoch, with `more` in the same list.
async fn serve_again(cl: &Client, pool: u32, r: &Range, more: Vec<WireOp>) -> Result<(), KvError> {
    let again = Range {
        epoch: r.epoch + 1,
        state: State::Serving,
        ..r.clone()
    };
    let mut ops = vec![r.epoch_assertion()];
    ops.extend(more);
    ops.extend(again.set_ops());
    went(exec(cl, pool, r.id, &ops).await)?;
    Ok(())
}

/// Finishes or undoes whatever stopped job `r` shows (§28.4, §28.5). Run
/// only on a range left so for a while, so that a job still under way
/// isn't undone under it; if one is, its next step fails.
pub async fn resume(cl: &Client, pool: u32, r: &Range) -> Result<(), KvError> {
    match &r.state {
        State::Splitting { into, .. } => match read(cl, pool, *into).await? {
            Some((n, _)) if n.state == State::Serving => {
                finish_split(cl, pool, r, &Steps::all()).await
            }
            n => {
                // Not opened: undone, the new range deleted first.
                if n.is_some() {
                    let ops = [state_is(&State::Pending), WireOp::Delete];
                    if !went(exec(cl, pool, *into, &ops).await)? {
                        return Ok(());
                    }
                }
                serve_again(cl, pool, r, Vec::new()).await
            }
        },
        State::Absorbing { .. } => undo_absorbing(cl, pool, r).await,
        State::Merging { into } => {
            // The left range drives the merge while it absorbs this one or
            // has taken it in; otherwise the merge was undone there.
            let driven = read(cl, pool, *into).await?.is_some_and(|(l, _)| {
                l.state == (State::Absorbing { from: r.id }) || l.merged == Some(r.id)
            });
            if driven {
                return Ok(());
            }
            serve_again(cl, pool, r, Vec::new()).await
        }
        // The split that made it opens it or deletes it.
        State::Pending => Ok(()),
        State::Serving if r.merged.is_some() => finish_merge(cl, pool, r, &Steps::all()).await,
        State::Serving => name_next(cl, pool, r).await,
    }
}

/// Entries of a range's deletion queue taken per look.
const DRAIN_PAGE: u32 = 64;

/// Deletes the objects in up to [`DRAIN_PAGE`] entries of range `r`'s
/// queue, then the entries (§28.6): how many entries were done. An
/// object is deleted before its entry, so a primary that dies between
/// leaves the entry, and the next deletes the object again, which is
/// nothing to delete.
pub async fn drain(cl: &Client, pool: u32, r: &Range) -> Result<usize, KvError> {
    let name = range_name(r.id);
    let l = cl.lookup(pool, NS_KV, &name, &[QUEUE], Some(&[QUEUE + 1]), DRAIN_PAGE);
    let Some(l) = l.await? else {
        return Ok(0);
    };
    for (k, v) in &l.kv {
        for d in decode_queue(v)? {
            for object in d.names() {
                cl.exec(d.pool, d.ns, &object, &[WireOp::Delete]).await?;
            }
        }
        exec(cl, pool, r.id, &[WireOp::KvDel(k.clone())]).await?;
    }
    Ok(l.kv.len())
}

/// [`drain`] until `r`'s queue is empty: before a range is deleted, so
/// that what its batches collected isn't left behind.
pub async fn drain_all(cl: &Client, pool: u32, r: &Range) -> Result<(), KvError> {
    while drain(cl, pool, r).await? > 0 {}
    Ok(())
}

/// The value of `key` in store `store`'s root.
async fn root_value(cl: &Client, store: u64, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
    let mut to = key.to_vec();
    to.push(0);
    let name = root(store);
    let l = cl
        .lookup(META_POOL, NS_KV, &name, key, Some(&to), 1)
        .await?;
    Ok(l.and_then(|l| l.kv.into_iter().next()).map(|(_, v)| v))
}

/// Whether `r`'s table exists: the root's `i` entry names it, and the
/// record under that name is the table's. Not while its delete is under
/// way, nor if a crash stopped its create before the root named it.
async fn table_alive(cl: &Client, r: &Range) -> Result<bool, KvError> {
    let Some(name) = root_value(cl, r.store, &id_key(r.table)).await? else {
        return Ok(false);
    };
    let record = root_value(cl, r.store, &table_key(&name)).await?;
    Ok(record.is_some_and(|b| Table::decode(&name, &b).is_ok_and(|t| t.id == r.table)))
}

/// Whether anything names range `r` (§28.5): its table must exist, and
/// the root name it, or the range it was split from, by `next` or as the
/// range it splits into. A range in the middle of a job is left to the
/// job; once taken up, it is looked at again.
pub async fn named(cl: &Client, pool: u32, r: &Range) -> Result<bool, KvError> {
    if matches!(
        r.state,
        State::Splitting { .. } | State::Absorbing { .. } | State::Merging { .. }
    ) {
        return Ok(true);
    }
    if !table_alive(cl, r).await? {
        return Ok(false);
    }
    if root_get(cl, r, &r.start)
        .await?
        .is_some_and(|(i, _)| i == r.id)
    {
        return Ok(true);
    }
    let Some(p) = r.parent else {
        return Ok(false);
    };
    Ok(read(cl, pool, p).await?.is_some_and(|(p, _)| {
        p.next == Some(r.id) || matches!(&p.state, State::Splitting { into, .. } if *into == r.id)
    }))
}

/// Deletes range `r`, which nothing names (§28.5), if it hasn't changed
/// since it was read: true if it did. A deleted table's root entries for
/// it go too, and the table's `i` entry with its last range, finishing a
/// delete that stopped half-way.
pub async fn delete_leftover(cl: &Client, pool: u32, r: &Range) -> Result<bool, KvError> {
    drain_all(cl, pool, r).await?;
    let ops = [r.epoch_assertion(), WireOp::Delete];
    if !went(exec(cl, pool, r.id, &ops).await)? {
        return Ok(false);
    }
    let root_ops =
        async |ops: &[WireOp]| went(cl.exec(META_POOL, NS_KV, &root(r.store), ops).await);
    if let Some((id, v)) = root_get(cl, r, &r.start).await?
        && id == r.id
    {
        let k = map_key(r.table, &r.start);
        root_ops(&[
            WireOp::AssertKv {
                key: k.clone(),
                value: Some(v),
            },
            WireOp::KvDel(k),
        ])
        .await?;
    }
    if !table_alive(cl, r).await? {
        let p = map_prefix(r.table);
        let end = prefix_end(&p).expect("a map prefix is never all 0xFF");
        let name = root(r.store);
        let left = cl
            .lookup(META_POOL, NS_KV, &name, &p, Some(&end), 1)
            .await?;
        let k = id_key(r.table);
        if left.is_none_or(|l| l.kv.is_empty())
            && let Some(v) = root_value(cl, r.store, &k).await?
        {
            root_ops(&[
                WireOp::AssertKv {
                    key: k.clone(),
                    value: Some(v),
                },
                WireOp::KvDel(k),
            ])
            .await?;
        }
    }
    Ok(true)
}
