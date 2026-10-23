//! The audit log (administration.md §23.9.4, decisions.md D112, D113):
//! every record goes to standard error (journald) at once, and to
//! append-only objects in `.meta` soon after.
//!
//! - **One object a day**, `audit/YYYY-MM-DD` (UTC), its records key–value
//!   entries: the key is the time counted down from the end of time (so a
//!   lookup from the start gives the newest first), then a per-process tag
//!   and a sequence number (unique across members); the value is the JSON
//!   line. `audit/days` lists the days there are.
//! - **Every member writes its own** records: they wait in a queue the
//!   writer empties every second, one operation list per day. If `.meta`
//!   can't be reached they wait (up to [`QUEUE_MAX`]; past that the oldest
//!   are dropped, and a record says how many); journald has them all.
//! - **hyperstor-mgmt's** records (people's logins, roles, tokens) arrive
//!   through `AuditService.AppendAudit` and join the queue.
//! - **Pruned** by the leader once a day: days older than the retention
//!   (a year by default, `0` forever) go whole.

use hyperstor_client::{Client, ClientError, NS_META};
use hyperstor_map::META_POOL;
use hyperstor_proto::WireOp;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Records waiting for `.meta`, at most.
pub const QUEUE_MAX: usize = 10_000;
/// Records written per operation list, at most.
const BATCH: usize = 500;
/// The days' index.
pub const INDEX: &[u8] = b"audit/days";

struct Entry {
    micros: u64,
    line: String,
}

static QUEUE: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
static DROPPED: AtomicU64 = AtomicU64::new(0);
static SEQ: AtomicU32 = AtomicU32::new(0);

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

/// A record from hyperstor-mgmt: refused unless it is one (a JSON object
/// with `audit`, `time`, `principal`, `op`, `target` and `result`).
pub fn append(line: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let time = v["time"]
        .as_f64()
        .or_else(|| v["time"].as_str().and_then(|t| t.parse().ok()))
        .ok_or("no time")?;
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

pub fn object(day: &str) -> Vec<u8> {
    format!("audit/{day}").into_bytes()
}

/// A record's key: newest first within its day, unique across members.
fn key(micros: u64, tag: u32, seq: u32) -> Vec<u8> {
    let mut k = (u64::MAX - micros).to_be_bytes().to_vec();
    k.extend_from_slice(&tag.to_be_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// Empties the queue into `.meta` every second, as a client through
/// `map_address`. Runs for good.
pub async fn writer(map_address: std::net::SocketAddr) {
    let tag = std::process::id() ^ (now_micros() as u32);
    let mut client: Option<Rc<Client>> = None;
    loop {
        hyperstor_reactor::time::sleep(Duration::from_secs(1)).await;
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
        if write(&c, &batch, tag).await.is_err() {
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

async fn write(c: &Rc<Client>, batch: &[Entry], tag: u32) -> Result<(), ClientError> {
    let _ = c.refresh_map().await;
    let mut by_day: BTreeMap<String, Vec<WireOp>> = BTreeMap::new();
    for e in batch {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        by_day
            .entry(day(e.micros / 1_000_000))
            .or_insert_with(|| vec![WireOp::Touch])
            .push(WireOp::KvPut(
                key(e.micros, tag, seq),
                e.line.clone().into_bytes(),
            ));
    }
    for (d, ops) in by_day {
        c.exec(META_POOL, NS_META, &object(&d), &ops).await?;
        c.exec(
            META_POOL,
            NS_META,
            INDEX,
            &[WireOp::Touch, WireOp::KvPut(d.into_bytes(), Vec::new())],
        )
        .await?;
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

/// Records matching `q`, newest first, and where to go on from if there
/// may be more.
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
        let mut from = match (&after_day, &after_key) {
            (Some(a), Some(k)) if a == d => {
                let mut k = k.clone();
                k.push(0);
                k
            }
            _ => Vec::new(),
        };
        loop {
            let Some(l) = c
                .lookup(META_POOL, NS_META, &object(d), &from, None, 1000)
                .await?
            else {
                break;
            };
            for (k, v) in &l.kv {
                let line = String::from_utf8_lossy(v).into_owned();
                if matches(&line, q) {
                    out.push(line);
                    if out.len() >= limit {
                        let next = format!("{d}:{}", to_hex(k));
                        return Ok((out, Some(next)));
                    }
                }
            }
            match l.kv.last() {
                Some((k, _)) if l.kv.len() == 1000 => {
                    from = k.clone();
                    from.push(0);
                }
                _ => break,
            }
        }
    }
    Ok((out, None))
}

fn matches(line: &str, q: &Query) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let t = v["time"].as_f64().unwrap_or(0.0) as u64;
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

/// The days there are records for, oldest first.
async fn days(c: &Client) -> Result<Vec<String>, ClientError> {
    let mut out = Vec::new();
    let mut from = Vec::new();
    loop {
        let Some(l) = c
            .lookup(META_POOL, NS_META, INDEX, &from, None, 1000)
            .await?
        else {
            break;
        };
        out.extend(
            l.kv.iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned()),
        );
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

/// Drops the days past the retention, once a day, on the leader.
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
            let gone = c
                .exec(META_POOL, NS_META, &object(&d), &[WireOp::Delete])
                .await;
            if gone.is_ok() || matches!(&gone, Err(ClientError::Server { .. })) {
                let _ = c
                    .exec(
                        META_POOL,
                        NS_META,
                        INDEX,
                        &[WireOp::KvDel(d.clone().into_bytes())],
                    )
                    .await;
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
}
