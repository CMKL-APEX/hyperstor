//! The audit log (19-administration.md §23.9.4, 04-decisions.md D112, D113,
//! D147, D148): every record goes to standard error (journald) at once,
//! and to append-only objects in `.meta` soon after.
//!
//! - **Numbered parts a day** (D148): each member writes its records to
//!   its own parts, `audit/YYYY-MM-DD/<member>-<N>` (UTC), starting the
//!   next part when one reaches [`PART_BYTES`], and a start after a
//!   restart with a new one. A record is a key–value entry: the key is the
//!   time counted down from the end of time (so a lookup from the start
//!   gives the newest first), then a per-process tag and a sequence number
//!   (unique across members); the value is the JSON line. `audit/days`
//!   lists the days there are, `audit/parts` every day's parts (`DAY/<part>`).
//!   Days written before parts are one object, `audit/YYYY-MM-DD`, still
//!   read and pruned.
//! - **Every member writes its own** records: they wait in a queue the
//!   writer empties every second. If `.meta` can't be reached they wait
//!   (up to [`QUEUE_MAX`]; past that the oldest are dropped, and a record
//!   says how many); journald has them all.
//! - **Refusals are summarised** (D147): of the refusals of one principal's
//!   operation with one error in a minute, the first [`REFUSALS_KEPT`] are
//!   recorded; the rest are counted, and one `Refused` record a minute says
//!   how many. journald gets every one.
//! - **hyperstor-mgmt's** records (people's logins, roles, tokens) arrive
//!   through `AuditService.AppendAudit` and join the queue.
//! - **Pruned** by the leader: days older than the retention (a year by
//!   default, `0` forever) go whole, every part of them.

use hyperstor_client::{Client, ClientError, NS_META};
use hyperstor_map::META_POOL;
use hyperstor_proto::WireOp;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Records waiting for `.meta`, at most.
pub const QUEUE_MAX: usize = 10_000;
/// Records written per operation list, at most.
const BATCH: usize = 500;
/// The days' index.
pub const INDEX: &[u8] = b"audit/days";
/// The parts' index: `DAY/<member>-<N>` for every part of every day.
pub const PARTS: &[u8] = b"audit/parts";
/// A part is closed once it holds this many bytes of records (D148).
pub const PART_BYTES: u64 = 16 << 20;
/// Refusals of one kind recorded a minute before they are only counted
/// (D147).
pub const REFUSALS_KEPT: u32 = 10;
/// The window refusals are counted in.
const REFUSAL_WINDOW_SECS: u64 = 60;

type Kv = (Vec<u8>, Vec<u8>);

struct Entry {
    micros: u64,
    line: String,
}

static QUEUE: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
static DROPPED: AtomicU64 = AtomicU64::new(0);
static SEQ: AtomicU32 = AtomicU32::new(0);
static REFUSALS: LazyLock<Mutex<Refusals>> = LazyLock::new(Default::default);

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// One record as a JSON line: `time` in seconds with milliseconds.
pub fn line_at(micros: u64, principal: &str, op: &str, target: &str, result: &str) -> String {
    format!(
        "{{\"audit\":1,\"time\":{}.{:03},\"principal\":{},\"op\":{},\"target\":{},\"result\":{}}}",
        micros / 1_000_000,
        micros / 1000 % 1000,
        json_str(principal),
        json_str(op),
        json_str(target),
        json_str(result)
    )
}

fn enqueue(micros: u64, line: String) {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= QUEUE_MAX {
        q.pop_front();
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    q.push_back(Entry { micros, line });
}

/// Records what someone did: on standard error now, in `.meta` soon.
pub fn record(principal: &str, op: &str, target: &str, result: &str) {
    let micros = now_micros();
    let line = line_at(micros, principal, op, target, result);
    eprintln!("{line}");
    enqueue(micros, line);
}

/// Records a refused call (D147): on standard error always; in `.meta`
/// the first [`REFUSALS_KEPT`] of a principal's operation refused with
/// `code` in a minute, then a count ([`Refusals`]).
pub fn refused(principal: &str, op: &str, target: &str, code: &str, message: &str) {
    let micros = now_micros();
    let line = line_at(
        micros,
        principal,
        op,
        target,
        &format!("refused: {message}"),
    );
    eprintln!("{line}");
    let (keep, summary) = REFUSALS.lock().unwrap_or_else(|e| e.into_inner()).note(
        principal,
        op,
        code,
        target,
        message,
        micros / 1_000_000,
    );
    if let Some(s) = summary {
        s.record();
    }
    if keep {
        enqueue(micros, line);
    }
}

/// Refusals counted per principal, operation and error code, a minute at a
/// time (D147).
#[derive(Default)]
pub struct Refusals {
    windows: HashMap<(String, String, String), Window>,
}

struct Window {
    start: u64,
    recorded: u32,
    held: u64,
    target: String,
    message: String,
}

/// Refusals held back in a minute: written as one `Refused` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub principal: String,
    pub op: String,
    pub code: String,
    pub held: u64,
    pub target: String,
    pub message: String,
}

impl Summary {
    /// The record's operation, target and result.
    pub fn fields(&self) -> (String, String, String) {
        (
            "Refused".to_string(),
            format!("{} {}", self.op, self.target),
            format!(
                "refused {} more {} calls from {} in a minute ({}: {})",
                self.held, self.op, self.principal, self.code, self.message
            ),
        )
    }

    fn record(&self) {
        let (op, target, result) = self.fields();
        record(&self.principal, &op, &target, &result);
    }
}

impl Refusals {
    /// One refusal at `now` (Unix seconds): whether to record it, and the
    /// summary of an earlier minute of the same kind that ended.
    pub fn note(
        &mut self,
        principal: &str,
        op: &str,
        code: &str,
        target: &str,
        message: &str,
        now: u64,
    ) -> (bool, Option<Summary>) {
        let k = (principal.to_string(), op.to_string(), code.to_string());
        let mut summary = None;
        if let Some(w) = self.windows.get_mut(&k) {
            if now < w.start + REFUSAL_WINDOW_SECS {
                if w.recorded < REFUSALS_KEPT {
                    w.recorded += 1;
                    return (true, None);
                }
                w.held += 1;
                w.target = target.to_string();
                w.message = message.to_string();
                return (false, None);
            }
            summary = self.windows.remove(&k).and_then(|w| w.summary(&k));
        }
        self.windows.insert(
            k,
            Window {
                start: now,
                recorded: 1,
                held: 0,
                target: target.to_string(),
                message: message.to_string(),
            },
        );
        (true, summary)
    }

    /// The summaries of the minutes that ended by `now`; their windows go.
    pub fn due(&mut self, now: u64) -> Vec<Summary> {
        let ended: Vec<_> = self
            .windows
            .iter()
            .filter(|(_, w)| now >= w.start + REFUSAL_WINDOW_SECS)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for k in ended {
            if let Some(s) = self.windows.remove(&k).and_then(|w| w.summary(&k)) {
                out.push(s);
            }
        }
        out.sort_by(|a, b| (&a.principal, &a.op).cmp(&(&b.principal, &b.op)));
        out
    }
}

impl Window {
    fn summary(self, k: &(String, String, String)) -> Option<Summary> {
        (self.held > 0).then(|| Summary {
            principal: k.0.clone(),
            op: k.1.clone(),
            code: k.2.clone(),
            held: self.held,
            target: self.target,
            message: self.message,
        })
    }
}

/// A record from hyperstor-mgmt: refused unless it is one (a JSON object
/// with `audit`, `time`, `principal`, `op`, `target` and `result`).
pub fn append(line: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let time = time_of(&v).ok_or("no time")?;
    for k in ["principal", "op", "target", "result"] {
        if !v[k].is_string() {
            return Err(format!("no {k}"));
        }
    }
    if v["audit"] != 1 || line.len() > 4096 {
        return Err("not an audit record".into());
    }
    eprintln!("{line}");
    enqueue((time * 1e6) as u64, line.to_string());
    Ok(())
}

/// A record's time in Unix seconds: a number, or a string holding one,
/// as hyperstor-mgmt wrote it before 2026-10-30 (records keep it).
pub fn time_of(v: &serde_json::Value) -> Option<f64> {
    v["time"]
        .as_f64()
        .or_else(|| v["time"].as_str().and_then(|t| t.parse().ok()))
}

/// The UTC day of `unix` seconds, `YYYY-MM-DD`.
pub fn day(unix: u64) -> String {
    let z = (unix / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A day written before parts: one object.
pub fn object(day: &str) -> Vec<u8> {
    format!("audit/{day}").into_bytes()
}

/// A part's name, as `audit/parts` lists it under its day.
pub fn part_name(member: u64, n: u32) -> String {
    format!("{member}-{n}")
}

/// A part's object.
pub fn part_object(day: &str, part: &str) -> Vec<u8> {
    format!("audit/{day}/{part}").into_bytes()
}

/// A part's key in `audit/parts`.
fn part_key(day: &str, part: &str) -> Vec<u8> {
    format!("{day}/{part}").into_bytes()
}

/// A record's key: newest first within its day, unique across members.
pub fn key(micros: u64, tag: u32, seq: u32) -> Vec<u8> {
    let mut k = (u64::MAX - micros).to_be_bytes().to_vec();
    k.extend_from_slice(&tag.to_be_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// What a member writes to (D148): its part of each day it writes to,
/// that part's number and the bytes in it.
pub struct Parts {
    member: u64,
    tag: u32,
    part_bytes: u64,
    days: BTreeMap<String, (u32, u64)>,
}

impl Parts {
    /// Member `member`'s, closing a part at `part_bytes` ([`PART_BYTES`]).
    pub fn new(member: u64, part_bytes: u64) -> Parts {
        Parts {
            member,
            tag: std::process::id() ^ (now_micros() as u32),
            part_bytes,
            days: BTreeMap::new(),
        }
    }

    /// The part to write `day`'s records to: on the first write of a day,
    /// one after this member's last (its size isn't known), listed in the
    /// indexes before any record goes into it.
    async fn current(&mut self, c: &Client, day: &str) -> Result<(String, u64), ClientError> {
        if !self.days.contains_key(day) {
            let mine = format!("{}-", self.member);
            let next = parts_of(c, day)
                .await?
                .iter()
                .filter_map(|p| p.strip_prefix(&mine)?.parse::<u32>().ok())
                .max()
                .map_or(0, |n| n + 1);
            self.open(c, day, next).await?;
        }
        let (n, bytes) = self.days[day];
        Ok((part_name(self.member, n), bytes))
    }

    async fn open(&mut self, c: &Client, day: &str, n: u32) -> Result<(), ClientError> {
        let part = part_name(self.member, n);
        c.exec(
            META_POOL,
            NS_META,
            PARTS,
            &[
                WireOp::Touch,
                WireOp::KvPut(part_key(day, &part), Vec::new()),
            ],
        )
        .await?;
        c.exec(
            META_POOL,
            NS_META,
            INDEX,
            &[
                WireOp::Touch,
                WireOp::KvPut(day.as_bytes().to_vec(), Vec::new()),
            ],
        )
        .await?;
        self.days.insert(day.to_string(), (n, 0));
        // Only recent days are written to; forget the rest.
        while self.days.len() > 4 {
            let oldest = self.days.keys().next().cloned().expect("a day");
            self.days.remove(&oldest);
        }
        Ok(())
    }

    /// `day`'s part is full: the next.
    async fn roll(&mut self, c: &Client, day: &str) -> Result<(), ClientError> {
        let n = self.days[day].0 + 1;
        self.open(c, day, n).await
    }

    fn add(&mut self, day: &str, bytes: u64) {
        if let Some(p) = self.days.get_mut(day) {
            p.1 += bytes;
        }
    }
}

/// Empties the queue into `.meta` every second, as a client through
/// `map_address`, into this member's parts; and writes the refusals'
/// summaries of the minutes that ended. Runs for good.
pub async fn writer(map_address: std::net::SocketAddr, member: u64) {
    let mut client: Option<Rc<Client>> = None;
    let mut parts = Parts::new(member, PART_BYTES);
    loop {
        hyperstor_reactor::time::sleep(Duration::from_secs(1)).await;
        let due = REFUSALS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .due(now_micros() / 1_000_000);
        for s in due {
            s.record();
        }
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            record(
                "map-service",
                "AuditDropped",
                "audit log",
                &format!("{dropped} records not written to .meta (queue full); journald has them"),
            );
        }
        let batch: Vec<Entry> = {
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            let n = q.len().min(BATCH);
            q.drain(..n).collect()
        };
        if batch.is_empty() {
            continue;
        }
        let c = match &client {
            Some(c) => c.clone(),
            None => match Client::connect(map_address, 0).await {
                Ok(c) => {
                    client = Some(c.clone());
                    c
                }
                Err(_) => {
                    requeue(batch);
                    continue;
                }
            },
        };
        let _ = c.refresh_map().await;
        let records: Vec<(u64, &str)> = batch.iter().map(|e| (e.micros, e.line.as_str())).collect();
        if write(&c, &records, &mut parts).await.is_err() {
            requeue(batch);
        }
    }
}

/// Puts a batch that wasn't written back at the front, in order.
fn requeue(batch: Vec<Entry>) {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    for e in batch.into_iter().rev() {
        if q.len() >= QUEUE_MAX {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        q.push_front(e);
    }
}

/// Writes `records` (each its time in microseconds and its line) to
/// `parts`, starting a part where one is full.
pub async fn write(
    c: &Client,
    records: &[(u64, &str)],
    parts: &mut Parts,
) -> Result<(), ClientError> {
    let mut by_day: BTreeMap<String, Vec<Kv>> = BTreeMap::new();
    for (micros, line) in records {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        by_day
            .entry(day(micros / 1_000_000))
            .or_default()
            .push((key(*micros, parts.tag, seq), line.as_bytes().to_vec()));
    }
    for (d, recs) in by_day {
        let mut recs = recs.into_iter().peekable();
        while recs.peek().is_some() {
            let (part, bytes) = parts.current(c, &d).await?;
            // As many as fit in the part; at least one in an empty part.
            let mut ops = vec![WireOp::Touch];
            let mut added = 0u64;
            while let Some((k, v)) = recs.peek() {
                let size = (k.len() + v.len()) as u64;
                if bytes + added > 0 && bytes + added + size > parts.part_bytes {
                    break;
                }
                let (k, v) = recs.next().expect("peeked");
                ops.push(WireOp::KvPut(k, v));
                added += size;
            }
            if added == 0 {
                parts.roll(c, &d).await?;
                continue;
            }
            c.exec(META_POOL, NS_META, &part_object(&d, &part), &ops)
                .await?;
            parts.add(&d, added);
        }
    }
    Ok(())
}

/// What to read: newest first, from `until` back to `since` (Unix seconds;
/// 0: no bound), matching `principal` (a substring) and `op` (exactly)
/// when given, at most `limit`, after `after` (a previous answer's `next`).
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub since: u64,
    pub until: u64,
    pub principal: String,
    pub op: String,
    pub limit: usize,
    pub after: String,
}

/// One of a day's objects being read in key order, a page at a time.
struct Source {
    object: Vec<u8>,
    page: VecDeque<Kv>,
    from: Vec<u8>,
    done: bool,
}

impl Source {
    async fn fill(&mut self, c: &Client) -> Result<(), ClientError> {
        if self.done || !self.page.is_empty() {
            return Ok(());
        }
        let Some(l) = c
            .lookup(META_POOL, NS_META, &self.object, &self.from, None, 1000)
            .await?
        else {
            self.done = true;
            return Ok(());
        };
        if l.kv.len() < 1000 {
            self.done = true;
        }
        if let Some((k, _)) = l.kv.last() {
            self.from = k.clone();
            self.from.push(0);
        }
        self.page.extend(l.kv);
        Ok(())
    }
}

/// Records matching `q`, newest first, and where to go on from if there
/// may be more. A day's parts (and its one object from before parts) are
/// merged by key, which is the time counted down: the cursor is the day
/// and the last key, whichever part it came from.
pub async fn read(c: &Client, q: &Query) -> Result<(Vec<String>, Option<String>), ClientError> {
    let limit = q.limit.clamp(1, 1000);
    let days = days(c).await?;
    let until_day = if q.until == 0 {
        None
    } else {
        Some(day(q.until))
    };
    let since_day = (q.since != 0).then(|| day(q.since));
    // `after`: "DAY:HEXKEY".
    let (after_day, after_key) = match q.after.split_once(':') {
        Some((d, k)) => (Some(d.to_string()), hex(k)),
        None => (None, None),
    };
    let mut out = Vec::new();
    for d in days.iter().rev() {
        if until_day.as_ref().is_some_and(|u| d > u) || after_day.as_ref().is_some_and(|a| d > a) {
            continue;
        }
        if since_day.as_ref().is_some_and(|s| d < s) {
            break;
        }
        let from = match (&after_day, &after_key) {
            (Some(a), Some(k)) if a == d => {
                let mut k = k.clone();
                k.push(0);
                k
            }
            _ => Vec::new(),
        };
        let mut sources: Vec<Source> = std::iter::once(object(d))
            .chain(parts_of(c, d).await?.iter().map(|p| part_object(d, p)))
            .map(|object| Source {
                object,
                page: VecDeque::new(),
                from: from.clone(),
                done: false,
            })
            .collect();
        loop {
            for s in &mut sources {
                s.fill(c).await?;
            }
            let Some(i) = (0..sources.len())
                .filter(|i| !sources[*i].page.is_empty())
                .min_by(|a, b| sources[*a].page[0].0.cmp(&sources[*b].page[0].0))
            else {
                break;
            };
            let (k, v) = sources[i].page.pop_front().expect("not empty");
            let line = String::from_utf8_lossy(&v).into_owned();
            if matches(&line, q) {
                out.push(line);
                if out.len() >= limit {
                    let next = format!("{d}:{}", to_hex(&k));
                    return Ok((out, Some(next)));
                }
            }
        }
    }
    Ok((out, None))
}

fn matches(line: &str, q: &Query) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let t = time_of(&v).unwrap_or(0.0) as u64;
    (q.since == 0 || t >= q.since)
        && (q.until == 0 || t <= q.until)
        && (q.principal.is_empty()
            || v["principal"]
                .as_str()
                .is_some_and(|p| p.contains(&q.principal)))
        && (q.op.is_empty() || v["op"].as_str() == Some(q.op.as_str()))
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Every key of `object` in `[from, to)` (no end: to the last).
async fn keys(
    c: &Client,
    object: &[u8],
    from: &[u8],
    to: Option<&[u8]>,
) -> Result<Vec<Vec<u8>>, ClientError> {
    let mut out = Vec::new();
    let mut from = from.to_vec();
    loop {
        let Some(l) = c
            .lookup(META_POOL, NS_META, object, &from, to, 1000)
            .await?
        else {
            break;
        };
        out.extend(l.kv.iter().map(|(k, _)| k.clone()));
        match l.kv.last() {
            Some((k, _)) if l.kv.len() == 1000 => {
                from = k.clone();
                from.push(0);
            }
            _ => break,
        }
    }
    Ok(out)
}

/// The days there are records for, oldest first.
async fn days(c: &Client) -> Result<Vec<String>, ClientError> {
    Ok(keys(c, INDEX, b"", None)
        .await?
        .iter()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .collect())
}

/// `day`'s parts, as `audit/parts` lists them (`DAY/` then the part's
/// name; `/` sorts just before `0`, which ends the day's range).
pub async fn parts_of(c: &Client, day: &str) -> Result<Vec<String>, ClientError> {
    let (from, to) = (format!("{day}/"), format!("{day}0"));
    Ok(keys(c, PARTS, from.as_bytes(), Some(to.as_bytes()))
        .await?
        .iter()
        .filter_map(|k| {
            String::from_utf8_lossy(k)
                .strip_prefix(&from)
                .map(str::to_string)
        })
        .collect())
}

/// The days to drop: older than `keep` days before now (0: none).
pub fn old_days(days: &[String], keep: u32, now_unix: u64) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    // Today is one of the days kept.
    let oldest_kept = day(now_unix.saturating_sub(u64::from(keep - 1) * 86400));
    days.iter()
        .filter(|d| d.as_str() < oldest_kept.as_str())
        .cloned()
        .collect()
}

/// Whether a delete answered "nothing to delete" or done: either way the
/// object is gone.
fn gone(r: &Result<(), ClientError>) -> bool {
    r.is_ok() || matches!(r, Err(ClientError::Server { .. }))
}

/// Drops `day` whole (D113): every part, then the day's object from
/// before parts, then the indexes' entries. Whether all of it went.
pub async fn drop_day(c: &Client, day: &str) -> Result<bool, ClientError> {
    let mut all = true;
    for p in parts_of(c, day).await? {
        let r = c
            .exec(META_POOL, NS_META, &part_object(day, &p), &[WireOp::Delete])
            .await;
        if gone(&r) {
            c.exec(
                META_POOL,
                NS_META,
                PARTS,
                &[WireOp::KvDel(part_key(day, &p))],
            )
            .await?;
        } else {
            all = false;
        }
    }
    let r = c
        .exec(META_POOL, NS_META, &object(day), &[WireOp::Delete])
        .await;
    if !all || !gone(&r) {
        return Ok(false);
    }
    c.exec(
        META_POOL,
        NS_META,
        INDEX,
        &[WireOp::KvDel(day.as_bytes().to_vec())],
    )
    .await?;
    Ok(true)
}

/// Drops the days past the retention, on the leader, every `every`.
pub async fn pruner(
    meta: Rc<crate::Meta>,
    map_address: std::net::SocketAddr,
    settings: Option<std::sync::Arc<crate::settings::Settings>>,
    every: Duration,
) {
    let mut client: Option<Rc<Client>> = None;
    loop {
        hyperstor_reactor::time::sleep(every).await;
        if !meta.leads() {
            continue;
        }
        let keep = settings
            .as_ref()
            .and_then(|s| s.audit_days().ok())
            .unwrap_or(crate::settings::DEFAULT_AUDIT_DAYS);
        let c = match &client {
            Some(c) => c.clone(),
            None => match Client::connect(map_address, 0).await {
                Ok(c) => {
                    client = Some(c.clone());
                    c
                }
                Err(_) => continue,
            },
        };
        let _ = c.refresh_map().await;
        let Ok(all) = days(&c).await else { continue };
        let now = now_micros() / 1_000_000;
        for d in old_days(&all, keep, now) {
            if matches!(drop_day(&c, &d).await, Ok(true)) {
                record(
                    "map-service",
                    "PruneAudit",
                    &format!("audit {d}"),
                    &format!("ok, older than {keep} days"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hyperstor-mgmt wrote its records' time as a string until
    /// 2026-10-30: they are read as the time they say, so `--since` keeps
    /// them and the API doesn't date them 1970.
    #[test]
    fn a_time_written_as_a_string_is_read() {
        let l = r#"{"audit":1,"time":"1792454400.123","principal":"user:ann","op":"Login","target":"mgmt","result":"ok"}"#;
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert_eq!(time_of(&v), Some(1_792_454_400.123));
        let since = |since| Query {
            since,
            ..Default::default()
        };
        assert!(matches(l, &since(1_792_454_400)));
        assert!(!matches(l, &since(1_792_454_401)));
        assert!(append(l).is_ok());
    }

    #[test]
    fn days_keys_and_lines() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(951_782_400), "2000-02-29");
        assert_eq!(day(1_792_454_400 + 86399), "2026-10-20");
        // Newest first within a day.
        assert!(key(2_000_000, 1, 0) < key(1_000_000, 1, 0));
        assert!(key(1_000_000, 1, 0) < key(1_000_000, 1, 1));
        let l = line_at(1_792_454_400_123_000, "user:a\"b", "Op", "t", "ok");
        let v: serde_json::Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["principal"], "user:a\"b");
        assert_eq!(v["time"].as_f64().unwrap(), 1_792_454_400.123);
        assert!(append(&l).is_ok());
        assert!(append("{\"audit\":1}").is_err());
        assert!(append("not json").is_err());
        let q = Query {
            principal: "user:".into(),
            op: "Op".into(),
            ..Default::default()
        };
        assert!(matches(&l, &q));
        assert!(!matches(
            &l,
            &Query {
                op: "Other".into(),
                ..Default::default()
            }
        ));
        assert!(!matches(
            &l,
            &Query {
                since: 1_792_454_401,
                ..Default::default()
            }
        ));
        assert_eq!(hex(&to_hex(&key(5, 6, 7))), Some(key(5, 6, 7)));
        // A day's parts: their objects under the day, their keys in
        // `audit/parts` inside the day's range only.
        assert_eq!(
            part_object("2026-10-20", &part_name(2, 7)),
            b"audit/2026-10-20/2-7"
        );
        let k = part_key("2026-10-20", "2-7");
        assert!(
            k.as_slice() >= b"2026-10-20/".as_slice() && k.as_slice() < b"2026-10-200".as_slice()
        );
        assert!(part_key("2026-10-21", "1-0").as_slice() >= b"2026-10-200".as_slice());
    }

    #[test]
    fn retention_drops_whole_days() {
        let days: Vec<String> = ["2025-10-20", "2025-10-21", "2026-10-19", "2026-10-20"]
            .map(String::from)
            .to_vec();
        let now = 1_792_454_400 + 3600; // 2026-10-20
        assert_eq!(old_days(&days, 365, now), ["2025-10-20"]);
        assert!(old_days(&days, 0, now).is_empty(), "0 keeps forever");
        assert_eq!(old_days(&days, 1, now).len(), 3, "only today kept");
    }

    /// D147: the first ten refusals of a kind in a minute are recorded,
    /// the rest counted into one summary when the minute ends; other kinds
    /// are counted apart.
    #[test]
    fn repeated_refusals_are_summarised() {
        let mut r = Refusals::default();
        let t0 = 1_000;
        let mut kept = 0;
        for i in 0..250 {
            let (keep, s) = r.note(
                "p",
                "CreateVolume",
                "resource_exhausted",
                "pool 1",
                "busy",
                t0 + i / 10,
            );
            assert!(s.is_none());
            kept += u32::from(keep);
        }
        assert_eq!(kept, REFUSALS_KEPT);
        // Another operation, another code: counted apart.
        assert!(
            r.note(
                "p",
                "DeleteVolume",
                "resource_exhausted",
                "v",
                "busy",
                t0 + 5
            )
            .0
        );
        assert!(
            r.note("p", "CreateVolume", "permission_denied", "v", "no", t0 + 5)
                .0
        );
        assert!(r.due(t0 + 59).is_empty(), "the minute isn't over");
        let due = r.due(t0 + 60);
        assert_eq!(due.len(), 1, "only the kind with held refusals: {due:?}");
        assert_eq!(due[0].held, 240);
        let (op, target, result) = due[0].fields();
        assert_eq!(op, "Refused");
        assert_eq!(target, "CreateVolume pool 1");
        assert!(
            result.starts_with("refused 240 more CreateVolume calls from p"),
            "{result}"
        );
        assert!(r.due(t0 + 61).is_empty(), "summarised once");
        // The next minute starts again: recorded, and a refusal that comes
        // after an unsummarised minute brings its summary along.
        for _ in 0..REFUSALS_KEPT + 3 {
            r.note("q", "Op", "c", "t", "m", t0 + 100);
        }
        let (keep, s) = r.note("q", "Op", "c", "t", "m", t0 + 200);
        assert!(keep);
        assert_eq!(s.map(|s| s.held), Some(3));
    }
}
