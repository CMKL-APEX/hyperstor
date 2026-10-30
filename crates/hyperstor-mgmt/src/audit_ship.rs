//! hyperstor-mgmt's audit records (people's logins, roles, tokens; and
//! what they asked of the cluster API) into the cluster's audit log in
//! `.meta` (04-decisions.md D112): each also goes to standard error at once,
//! and waits here until a map service member takes it
//! (`AuditService.AppendAudit`), sent every second. Up to [`QUEUE_MAX`]
//! wait; past that the oldest are dropped (journald has them all).

use connectrpc::client::{ClientConfig, HttpClient};
use hyperstor_api::AuditServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const QUEUE_MAX: usize = 10_000;
const BATCH: usize = 500;

static QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Queues `line` for the cluster's audit log.
pub fn push(line: String) {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= QUEUE_MAX {
        q.pop_front();
    }
    q.push_back(line);
}

/// Sends what is queued, every second, to the first member of `addrs`
/// that takes it. Runs for good.
pub async fn ship(addrs: Vec<String>, tls: Arc<connectrpc::rustls::ClientConfig>) {
    let clients: Vec<AuditServiceClient<HttpClient>> = addrs
        .iter()
        .map(|a| {
            let uri: http::Uri = format!("https://{a}").parse().expect("address");
            AuditServiceClient::new(
                HttpClient::with_tls(tls.clone()),
                ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5)),
            )
        })
        .collect();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let batch: Vec<String> = {
            let q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            q.iter().take(BATCH).cloned().collect()
        };
        if batch.is_empty() {
            continue;
        }
        let req = pb::AppendAuditRequest {
            lines: batch.clone(),
            ..Default::default()
        };
        for c in &clients {
            if c.append_audit(req.clone()).await.is_ok() {
                let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                // Only what was sent: more may have come meanwhile.
                for _ in 0..batch.len().min(q.len()) {
                    q.pop_front();
                }
                break;
            }
        }
    }
}
