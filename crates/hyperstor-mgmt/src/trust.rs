//! Trust in glidex directories (spec/19-administration.md §23.12,
//! 04-decisions.md D306), mgmt's side: the address a directory reaches
//! and the calls mgmt makes to directories. The records and their checks
//! are the map service's (`TrustService`); mgmt holds nothing.
//!
//! - **Redeems and ends** (`POST /trust/v1/redeem`, `POST /trust/v1/end`,
//!   glidex D65, D66) come from directories, without a session or token:
//!   their peer assertion is their credential. mgmt relays the assertion
//!   and the body to the map service, which checks them, and answers the
//!   directory as it says.
//! - **The feed** (glidex D67): the mgmt on the host of the map service's
//!   leader (found from the member list, D276) pulls each trusted
//!   directory's feed every [`glidex_trust::cluster::FEED_EVERY`], signed
//!   with the cluster key towards directories that the map service hands
//!   it (kept in memory only), and hands back what it got. Another host's
//!   mgmt doesn't pull; after a failover the new leader's does. Two that
//!   pull at once (a leader moving) apply the same feed twice, which
//!   changes nothing.
//! - **Reports** (glidex D70, D85) go to each directory every
//!   [`REPORT_EVERY`], and after a feed changed: storage counts per
//!   tenant, which the map service makes.
//! - **Ends decided here** are told to their directory, and kept until it
//!   answered.

use crate::members::Members;
use glidex_trust::client::{self, Feed};
use glidex_trust::{ClusterKey, TrustedDirectory};
use hyperstor_api::TrustServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often each directory gets a report when its feed doesn't change.
pub const REPORT_EVERY: Duration = Duration::from_secs(300);

/// How often reports go when nothing changed: [`REPORT_EVERY`], or
/// `HS_TRUST_REPORT_MS` (for tests).
fn report_every() -> Duration {
    std::env::var("HS_TRUST_REPORT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(REPORT_EVERY)
}

/// How often feeds are pulled: glidex's 30 s, or `HS_TRUST_POLL_MS` (for
/// tests).
fn poll_every() -> Duration {
    std::env::var("HS_TRUST_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(glidex_trust::cluster::FEED_EVERY)
}

/// The paths directories call, under mgmt's address in their offer.
pub fn is_trust_path(path: &str) -> bool {
    path == glidex_trust::REDEEM_PATH || path == glidex_trust::END_PATH
}

/// Relays a directory's redeem or end to the map service: the status and
/// JSON body to answer it with.
pub async fn relay(
    members: &Members,
    path: &str,
    headers: &http::HeaderMap,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let refused = || (403, br#"{"code":"trust_refused"}"#.to_vec());
    let Some(assertion) = glidex_trust::peer::from_headers(headers) else {
        return refused();
    };
    let body = body.to_vec();
    let r = if path == glidex_trust::REDEEM_PATH {
        members
            .each(TrustServiceClient::new, |c| {
                let (assertion, body) = (assertion.clone(), body.clone());
                async move {
                    c.relay_redeem(pb::RelayRedeemRequest {
                        assertion,
                        body,
                        ..Default::default()
                    })
                    .await
                    .map(|r| {
                        let r = r.into_owned();
                        (r.status, r.body)
                    })
                }
            })
            .await
    } else {
        members
            .each(TrustServiceClient::new, |c| {
                let (assertion, body) = (assertion.clone(), body.clone());
                async move {
                    c.relay_end(pb::RelayEndRequest {
                        assertion,
                        body,
                        ..Default::default()
                    })
                    .await
                    .map(|r| {
                        let r = r.into_owned();
                        (r.status, r.body)
                    })
                }
            })
            .await
    };
    match r {
        Ok((status, body)) => (u16::try_from(status).unwrap_or(500), body),
        Err(e) if e.code == connectrpc::ErrorCode::Unavailable => {
            (503, br#"{"code":"unavailable"}"#.to_vec())
        }
        Err(e) => {
            eprintln!(
                "hyperstor-mgmt: a directory's {path}: {}",
                e.message.as_deref().unwrap_or("refused")
            );
            refused()
        }
    }
}

/// The cluster key towards directories, as the map service hands it.
async fn trust_key(members: &Members) -> Result<ClusterKey, String> {
    let r = members
        .each(TrustServiceClient::new, |c| async move {
            c.get_trust_key(pb::GetTrustKeyRequest::default())
                .await
                .map(|r| r.into_owned())
        })
        .await
        .map_err(|e| e.message.unwrap_or_default())?;
    ClusterKey::from_pkcs8(&r.kid, &r.pkcs8)
}

/// Pulls, applies and reports for good, while this host's map service
/// member leads.
pub async fn run(members: Arc<Members>) {
    let mut key: Option<ClusterKey> = None;
    let mut reported: HashMap<String, Instant> = HashMap::new();
    let mut said: HashMap<String, Instant> = HashMap::new();
    // At most once a minute per directory: a directory down for a day
    // would fill the log.
    let mut say = |what: &str, line: String| {
        if said
            .get(what)
            .is_none_or(|t| t.elapsed() > Duration::from_secs(60))
        {
            eprintln!("hyperstor-mgmt: {line}");
            said.insert(what.to_string(), Instant::now());
        }
    };
    loop {
        tokio::time::sleep(poll_every()).await;
        if !members.leader_here() {
            continue;
        }
        let work = match members
            .each(TrustServiceClient::new, |c| async move {
                c.get_feed_work(pb::GetFeedWorkRequest::default())
                    .await
                    .map(|r| r.into_owned())
            })
            .await
        {
            Ok(w) => w,
            Err(_) => continue,
        };
        if work.targets.is_empty() && work.ends.is_empty() {
            continue;
        }
        if key.is_none() {
            match trust_key(&members).await {
                Ok(k) => key = Some(k),
                Err(e) => {
                    say("key", format!("the cluster key towards directories: {e}"));
                    continue;
                }
            }
        }
        let k = key.as_ref().expect("a key");
        for t in &work.targets {
            let Ok(d) = serde_json::from_slice::<TrustedDirectory>(&t.directory) else {
                continue;
            };
            let now = glidex_trust::now();
            let pulled = client::pull_feed(k, &work.cluster_id, &d, &t.revision, now).await;
            let (snapshot, changed) = match pulled {
                Ok(Feed::Unchanged) => (Vec::new(), false),
                Ok(Feed::Changed(s)) => (serde_json::to_vec(&s).expect("json"), true),
                Err(e) => {
                    say(&d.id, format!("the feed of {}: {e}", d.issuer));
                    continue;
                }
            };
            let id = d.id.clone();
            let applied = members
                .each(TrustServiceClient::new, |c| {
                    let (id, snapshot) = (id.clone(), snapshot.clone());
                    async move {
                        c.apply_feed(pb::ApplyFeedRequest {
                            directory: id,
                            snapshot,
                            ..Default::default()
                        })
                        .await
                        .map(|r| r.into_owned())
                    }
                })
                .await;
            match applied {
                Ok(a) if !a.blocked.is_empty() => say(
                    &format!("{}-blocked", d.id),
                    format!(
                        "from the feed of {}, not done yet: {}",
                        d.issuer,
                        a.blocked.join("; ")
                    ),
                ),
                Ok(_) => {}
                Err(e) => {
                    say(
                        &format!("{}-apply", d.id),
                        format!(
                            "the feed of {}: {}",
                            d.issuer,
                            e.message.as_deref().unwrap_or("not applied")
                        ),
                    );
                    continue;
                }
            }
            let due = changed
                || reported
                    .get(&d.id)
                    .is_none_or(|t| t.elapsed() >= report_every());
            if !due {
                continue;
            }
            let id = d.id.clone();
            let report = members
                .each(TrustServiceClient::new, |c| {
                    let id = id.clone();
                    async move {
                        c.get_trust_report(pb::GetTrustReportRequest {
                            directory: id,
                            ..Default::default()
                        })
                        .await
                        .map(|r| r.into_owned())
                    }
                })
                .await;
            let Ok(report) = report else { continue };
            let Ok(report) = serde_json::from_slice::<serde_json::Value>(&report.report) else {
                continue;
            };
            // With where logins come back (glidex D73), as it is now.
            let callback = (!t.ui_callback.is_empty()).then_some(t.ui_callback.as_str());
            match client::push_report(k, &work.cluster_id, &d, &report, callback, now).await {
                Ok(()) => {
                    reported.insert(d.id.clone(), Instant::now());
                }
                Err(e) => say(
                    &format!("{}-report", d.id),
                    format!("the report to {}: {e}", d.issuer),
                ),
            }
        }
        for e in &work.ends {
            let Ok(d) = serde_json::from_slice::<TrustedDirectory>(&e.directory) else {
                continue;
            };
            match client::end_trust(k, &work.cluster_id, &d, &e.tenant, glidex_trust::now()).await {
                // Answered, whatever it said: it knows, or knew.
                Ok(_) => {
                    let (id, tenant) = (d.id.clone(), e.tenant.clone());
                    let _ = members
                        .each(TrustServiceClient::new, |c| {
                            let (id, tenant) = (id.clone(), tenant.clone());
                            async move {
                                c.end_noticed(pb::EndNoticedRequest {
                                    directory: id,
                                    tenant,
                                    ..Default::default()
                                })
                                .await
                                .map(|_| ())
                            }
                        })
                        .await;
                }
                Err(err) => say(
                    &format!("{}-end", d.id),
                    format!("telling {} of an end: {err}", d.issuer),
                ),
            }
        }
    }
}
