//! hyperstor-mgmt's audit records (people's logins, roles, tokens; and
//! what they asked of the cluster API) into the cluster's audit log in
//! `.meta` (04-decisions.md D112): each also goes to standard error at once,
//! and waits here until a map service member takes it
//! (`AuditService.AppendAudit`), sent every second. Up to [`QUEUE_MAX`]
//! wait; past that the oldest are dropped (journald has them all).

use hyperstor_api::AuditServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const QUEUE_MAX: usize = 10_000;
const BATCH: usize = 500;
/// The longest line the map service takes (`audit_log::append`).
const LINE_MAX: usize = 4096;

/// The lines waiting, each with its number in the order queued: what a
/// send took is known by number, however many were dropped meanwhile.
struct Queue {
    lines: VecDeque<(u64, String)>,
    next: u64,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    lines: VecDeque::new(),
    next: 0,
});

/// Queues `line` for the cluster's audit log. One the map service would
/// refuse (over [`LINE_MAX`]) is dropped here: standard error has it.
pub fn push(line: String) {
    if line.len() > LINE_MAX {
        eprintln!(
            "hyperstor-mgmt: an audit line of {} bytes is too long for the map service; dropped",
            line.len()
        );
        return;
    }
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if q.lines.len() >= QUEUE_MAX {
        q.lines.pop_front();
    }
    let n = q.next;
    q.next += 1;
    q.lines.push_back((n, line));
}

/// Up to [`BATCH`] lines from the front, and the number of the last.
fn take() -> Option<(Vec<String>, u64)> {
    let q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let last = q.lines.get(q.lines.len().min(BATCH).checked_sub(1)?)?.0;
    Some((
        q.lines.iter().take(BATCH).map(|(_, l)| l.clone()).collect(),
        last,
    ))
}

/// Removes the lines numbered up to `last`: those sent, or dropped.
fn done(last: u64) {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    while q.lines.front().is_some_and(|(n, _)| *n <= last) {
        q.lines.pop_front();
    }
}

/// How a send went.
#[derive(Debug, PartialEq)]
enum Sent {
    Ok,
    /// The map service refused a line it will always refuse.
    Refused,
    /// No member took it: tried again later.
    Later,
}

/// Sends what is queued to `send`: a batch, and when the map service
/// refuses it as not audit records, each line alone, so that a line it
/// will never take is dropped (and counted) rather than holding back
/// those after it. What was taken or dropped leaves the queue.
async fn send_queued<F, Fut>(send: F)
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Sent>,
{
    let Some((batch, last)) = take() else {
        return;
    };
    match send(batch.clone()).await {
        Sent::Ok => done(last),
        Sent::Later => {}
        Sent::Refused => {
            let first = last + 1 - batch.len() as u64;
            let mut dropped = 0;
            for (i, line) in batch.into_iter().enumerate() {
                match send(vec![line]).await {
                    Sent::Ok => {}
                    Sent::Refused => dropped += 1,
                    // The rest waits for the next round.
                    Sent::Later => break,
                }
                done(first + i as u64);
            }
            if dropped > 0 {
                eprintln!(
                    "hyperstor-mgmt: the map service refused {dropped} audit line(s); dropped them (standard error has them)"
                );
            }
        }
    }
}

/// Sends what is queued, every second, to the map service's members in
/// turn, the leader first (D276). Runs for good.
pub async fn ship(members: Arc<crate::members::Members>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        send_queued(|lines| {
            let members = members.clone();
            async move {
                let req = pb::AppendAuditRequest {
                    lines,
                    ..Default::default()
                };
                let sent = members
                    .each(AuditServiceClient::new, |c| {
                        let req = req.clone();
                        async move { c.append_audit(req).await.map(|_| ()) }
                    })
                    .await;
                match sent {
                    Ok(()) => Sent::Ok,
                    Err(e) if e.code == connectrpc::ErrorCode::InvalidArgument => Sent::Refused,
                    Err(_) => Sent::Later,
                }
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn queued() -> Vec<String> {
        let q = QUEUE.lock().unwrap();
        q.lines.iter().map(|(_, l)| l.clone()).collect()
    }

    /// One test, as the queue is the process's.
    #[tokio::test]
    async fn a_refused_line_is_dropped_and_the_rest_are_sent() {
        let got = Mutex::new(Vec::<String>::new());
        let up = AtomicBool::new(true);
        let send = |lines: Vec<String>| {
            let r = if !up.load(Ordering::SeqCst) {
                Sent::Later
            } else if lines.iter().any(|l| l.contains("bad")) {
                Sent::Refused
            } else {
                got.lock().unwrap().extend(lines);
                Sent::Ok
            };
            std::future::ready(r)
        };
        // Too long for the map service: never queued.
        push("x".repeat(LINE_MAX + 1));
        assert!(queued().is_empty());
        // A line refused in a batch: the others arrive, it is dropped.
        for l in ["a", "bad", "c"] {
            push(l.into());
        }
        send_queued(send).await;
        assert_eq!(*got.lock().unwrap(), ["a", "c"]);
        assert!(queued().is_empty());
        // No member answering: kept for the next round.
        push("d".into());
        up.store(false, Ordering::SeqCst);
        send_queued(send).await;
        assert_eq!(queued(), ["d"]);
        up.store(true, Ordering::SeqCst);
        send_queued(send).await;
        assert_eq!(got.lock().unwrap().last().unwrap(), "d");
        // Lines dropped from a full queue while a batch was out: only
        // what was sent leaves; the ones queued meanwhile stay.
        got.lock().unwrap().clear();
        for i in 0..QUEUE_MAX {
            push(format!("old {i}"));
        }
        let (batch, last) = take().unwrap();
        assert_eq!(batch.len(), BATCH);
        for i in 0..BATCH + 10 {
            push(format!("new {i}"));
        }
        done(last);
        let q = queued();
        assert_eq!(q.len(), QUEUE_MAX);
        assert_eq!(q[0], format!("old {}", BATCH + 10));
        assert_eq!(q.last().unwrap(), &format!("new {}", BATCH + 9));
    }
}
