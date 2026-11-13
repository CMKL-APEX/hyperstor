//! Where mgmt's calls to the map service go (04-decisions.md D276): the
//! leader first, so that people's changes, their sessions and the audit
//! lines reach it from whichever host's mgmt they used, behind a proxy or
//! not, with no redirect. The order comes from the member list, asked
//! every [`REFRESH`] and as soon as a call met a member that doesn't
//! lead: the leader's API, then every other member's (as each recorded
//! when it last led), then `--api`'s addresses not among them, so a call
//! that meets a member that doesn't lead goes on to the next.

use connectrpc::client::{ClientConfig, HttpClient};
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How often the member list is asked when nothing changed.
pub const REFRESH: Duration = Duration::from_secs(5);

/// Each call's limit, as before (`--api`'s clients).
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Members {
    /// `--api`'s addresses.
    seeds: Vec<String>,
    /// Where calls go, in turn: the leader's first.
    order: RwLock<Vec<String>>,
    /// The leader's API address, from the last member list that named one.
    leader: RwLock<Option<String>>,
    http: HttpClient,
    nudge: tokio::sync::Notify,
}

impl Members {
    pub fn new(seeds: Vec<String>, tls: Arc<connectrpc::rustls::ClientConfig>) -> Arc<Members> {
        Arc::new(Members {
            order: RwLock::new(seeds.clone()),
            leader: RwLock::new(None),
            seeds,
            http: HttpClient::with_tls(tls),
            nudge: tokio::sync::Notify::new(),
        })
    }

    /// Where calls go now, in turn.
    pub fn addrs(&self) -> Vec<String> {
        self.order.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the map service's leader is on this host, by its API
    /// address: the one mgmt that pulls directories' feeds (D306).
    pub fn leader_here(&self) -> bool {
        self.leader
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            .and_then(|a| a.parse::<std::net::SocketAddr>().ok())
            .is_some_and(|a| glidex_tls::is_local_ip(a.ip()))
    }

    /// The client config for the member at `addr`.
    pub fn config(addr: &str) -> ClientConfig {
        let uri: http::Uri = format!("https://{addr}").parse().expect("address");
        ClientConfig::new(uri).with_default_timeout(CALL_TIMEOUT)
    }

    /// The HTTP client every call shares (its connections are pooled per
    /// address).
    pub fn http(&self) -> HttpClient {
        self.http.clone()
    }

    /// Asks for the member list again now: a call met a member that
    /// doesn't lead, or none answered.
    pub fn stale(&self) {
        self.nudge.notify_one();
    }

    /// Calls `f` with a client (`make`) for each member in turn until one
    /// answers other than "unavailable" (down, or not the leader for a
    /// write); that answer, or the last "unavailable".
    pub async fn each<C, T, F, Fut>(
        &self,
        make: fn(HttpClient, ClientConfig) -> C,
        f: F,
    ) -> Result<T, connectrpc::ConnectError>
    where
        F: Fn(C) -> Fut,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>>,
    {
        let mut last = connectrpc::ConnectError::unavailable("no map service member configured");
        for (i, a) in self.addrs().iter().enumerate() {
            match f(make(self.http(), Self::config(a))).await {
                Err(e) if e.code == connectrpc::ErrorCode::Unavailable => last = e,
                r => {
                    if i > 0 {
                        // The first didn't serve it: the order is old.
                        self.stale();
                    }
                    return r;
                }
            }
        }
        self.stale();
        Err(last)
    }

    /// Asks each member in turn for the member list, and orders the
    /// addresses from the first answer. Whether one answered.
    pub async fn refresh(&self) -> bool {
        for a in self.addrs() {
            let c = hyperstor_api::MemberServiceClient::new(self.http(), Self::config(&a));
            if let Ok(r) = c.list_members(pb::ListMembersRequest::default()).await {
                let members = r.into_owned().members;
                let leader = members
                    .iter()
                    .find(|m| m.leader && !m.api_address.is_empty())
                    .map(|m| m.api_address.clone());
                *self.leader.write().unwrap_or_else(|e| e.into_inner()) = leader;
                let next = order(&members, &self.seeds);
                let mut cur = self.order.write().unwrap_or_else(|e| e.into_inner());
                if next.first() != cur.first() && !next.is_empty() {
                    eprintln!(
                        "hyperstor-mgmt: calls to the map service go to {} first",
                        next[0]
                    );
                }
                *cur = next;
                return true;
            }
        }
        false
    }

    /// Keeps the order current, for good.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.refresh().await;
            let _ = tokio::time::timeout(REFRESH, self.nudge.notified()).await;
        }
    }
}

/// The addresses to call in turn: the leader's API, the other members'
/// recorded ones, then `seeds` not among them. A member that never led
/// has no API recorded; `--api` names it if it is to be asked.
pub fn order(members: &[pb::Member], seeds: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let leaders = members.iter().filter(|m| m.leader);
    let others = members.iter().filter(|m| !m.leader);
    for m in leaders.chain(others) {
        if !m.api_address.is_empty() && !out.contains(&m.api_address) {
            out.push(m.api_address.clone());
        }
    }
    for s in seeds {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u64, leader: bool, api: &str) -> pb::Member {
        pb::Member {
            id,
            leader,
            voter: true,
            api_address: api.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_leader_comes_first_then_the_others_then_the_seeds() {
        let seeds = vec!["10.0.0.2:8850".to_string(), "10.0.0.9:8850".to_string()];
        let ms = [
            m(1, false, "10.0.0.1:8850"),
            m(2, false, "10.0.0.2:8850"),
            m(3, true, "10.0.0.3:8850"),
            m(4, false, ""),
        ];
        assert_eq!(
            order(&ms, &seeds),
            [
                "10.0.0.3:8850",
                "10.0.0.1:8850",
                "10.0.0.2:8850",
                "10.0.0.9:8850"
            ]
        );
        // No leader now, or one that hasn't recorded its API: the others,
        // then the seeds, all still asked in turn.
        let ms = [m(1, true, ""), m(2, false, "10.0.0.2:8850")];
        assert_eq!(order(&ms, &seeds), ["10.0.0.2:8850", "10.0.0.9:8850"]);
        assert_eq!(order(&[], &seeds), seeds);
    }
}
