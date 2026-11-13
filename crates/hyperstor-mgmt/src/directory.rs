//! People from glidex directories (spec/19-administration.md §23.12,
//! 04-decisions.md D307–D309): who a login or a token of a trusted
//! directory is here, and what they may do.
//!
//! - **What logins are checked against** comes from the map service
//!   (`TrustService.GetLoginState`): each trusted directory's record (its
//!   issuer and keys, from its last feed), its realm, how fresh its feed
//!   is, and the login chains it ended. Kept for [`CACHE`].
//! - **Who a person is** (`TrustService.ResolvePerson`): in the
//!   directory's replica and enabled, and the grants its links give
//!   (D308). Kept for [`CACHE`] per person, so a feed's change reaches
//!   every mgmt within the feed's period and that.
//! - **Their account here** is `<their id>@<the directory's realm>`
//!   ([`account`]): a row as a local person's, which holds the roles a
//!   security administrator gives them, cluster-wide or in realms, and
//!   which count only while they are members of the operator tenant
//!   (D308). Their roles in tenants and projects come from the feed only.
//! - **Partitions** (D309, glidex D13): past the directory's
//!   `max_staleness` since its feed last answered (default
//!   [`STALE_MINUTES`]), its sessions, access tokens and API tokens are
//!   refused; local logins aren't.

use crate::members::Members;
use crate::people::{self, DirLogin, Principal};
use glidex_trust::{ClusterKey, FeedState, TrustedDirectory};
use hyperstor_api::TrustServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long what the map service said is trusted without asking again.
pub const CACHE: Duration = people::CACHE;
/// A directory's `max_staleness` by default (D309: an hour, as glidex's).
pub const STALE_MINUTES: u64 = 60;
/// Where directories send people's browser logins back (the map
/// service's `trust::LOGIN_CALLBACK`).
pub const CALLBACK: &str = "/auth/directory/callback";
/// The cookie binding a login begun at a directory to its browser.
pub const BROWSER_COOKIE: &str = "hs_dir";
/// The `method` of a session or token from a directory.
pub const METHOD: &str = "directory";
pub const TOKEN_METHOD: &str = "directory-token";

/// A trusted directory, as logins are checked against it.
#[derive(Debug, Clone)]
pub struct Dir {
    pub d: TrustedDirectory,
    pub realm: String,
    pub feed: FeedState,
    pub ended: HashSet<String>,
    pub callback: String,
}

/// What the map service said logins are checked against.
#[derive(Debug, Clone, Default)]
pub struct LoginState {
    pub cluster_id: String,
    pub dirs: Vec<Dir>,
}

impl LoginState {
    /// The directory with this trusted id or issuer.
    pub fn dir(&self, id_or_issuer: &str) -> Option<&Dir> {
        let issuer = glidex_trust::normalize_issuer(id_or_issuer).unwrap_or_default();
        self.dirs
            .iter()
            .find(|d| d.d.id == id_or_issuer || d.d.issuer == issuer)
    }
}

/// Who a directory's person is here, as the map service resolved them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub found: bool,
    pub disabled: bool,
    pub display_name: String,
    pub grants: Vec<String>,
    pub operator: bool,
    pub directory: String,
    pub subject: String,
    pub token_id: String,
    pub token_expires: u64,
    pub token_good: bool,
    pub service_account: bool,
}

impl From<pb::ResolvePersonResponse> for Resolved {
    fn from(r: pb::ResolvePersonResponse) -> Resolved {
        Resolved {
            found: r.found,
            disabled: r.disabled,
            display_name: r.display_name,
            grants: r.grants,
            operator: r.operator,
            directory: r.directory,
            subject: r.subject,
            token_id: r.token_id,
            token_expires: r.token_expires,
            token_good: r.token_good,
            service_account: r.service_account,
        }
    }
}

/// The account here of person `subject` of a directory in `realm`.
pub fn account(subject: &str, realm: &str) -> String {
    format!("{subject}@{realm}")
}

/// The account of a project's service account's token `id` (glidex §7.4).
pub fn token_account(id: &str, realm: &str) -> String {
    format!("token-{id}@{realm}")
}

/// Whether grant `g` is one a security administrator may give a
/// directory's person here: a role, cluster-wide or in a realm. Their
/// roles in tenants and projects are the feed's (D308).
pub fn grantable(g: &str) -> bool {
    !g.contains("@tenant:") && !g.contains("@project:")
}

/// The roles of a directory's person: what their links give, and, while
/// they are members of the operator tenant, those their account here
/// holds, cluster-wide or in realms (D308).
pub fn roles_of(r: &Resolved, account_roles: &[String]) -> Vec<String> {
    let mut out: Vec<String> = r.grants.clone();
    if r.operator && !r.service_account {
        out.extend(account_roles.iter().filter(|g| grantable(g)).cloned());
    }
    out.sort();
    out.dedup();
    out
}

/// Whether directory `d`'s credentials are good at `now` (D309): its feed
/// answered within its `max_staleness`, else `default_secs`.
pub fn fresh(d: &Dir, default_secs: u64, now: u64) -> bool {
    let lag = glidex_trust::cluster::lag(&d.d, &d.feed, now);
    let max = d.d.max_staleness_minutes.map_or(default_secs, |m| m * 60);
    lag <= max
}

/// `return_to` as a path on this host, else `/`.
pub fn local_path(return_to: Option<&str>) -> String {
    match return_to {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') => p.into(),
        _ => "/".into(),
    }
}

/// Logins begun at a directory: what [`crate::people::People::keep_login`]
/// keeps besides its state, nonce and PKCE verifier.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct After {
    /// The directory's trusted id.
    pub directory: String,
}

pub struct Directories {
    members: Arc<Members>,
    state: Mutex<Option<(Arc<LoginState>, Instant)>>,
    persons: Mutex<HashMap<String, (Resolved, Instant)>>,
    key: tokio::sync::Mutex<Option<(String, zeroize::Zeroizing<Vec<u8>>)>>,
    /// `max_staleness` when a directory has none of its own.
    default_secs: u64,
}

fn unavailable(e: connectrpc::ConnectError) -> people::Error {
    people::Error::Unavailable(e.message.unwrap_or_default())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Directories {
    /// With `max_staleness` [`STALE_MINUTES`] by default, or
    /// `HS_TRUST_STALE_SECS` seconds (for tests).
    pub fn new(members: Arc<Members>) -> Directories {
        let default_secs = std::env::var("HS_TRUST_STALE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(STALE_MINUTES * 60);
        Directories {
            members,
            state: Mutex::new(None),
            persons: Mutex::new(HashMap::new()),
            key: tokio::sync::Mutex::new(None),
            default_secs,
        }
    }

    /// What logins are checked against, asked at most once a [`CACHE`].
    pub async fn state(&self) -> Result<Arc<LoginState>, people::Error> {
        if let Some((s, at)) = &*lock(&self.state)
            && at.elapsed() < CACHE
        {
            return Ok(s.clone());
        }
        let r = self
            .members
            .each(TrustServiceClient::new, |c| async move {
                c.get_login_state(pb::GetLoginStateRequest::default())
                    .await
                    .map(|r| r.into_owned())
            })
            .await
            .map_err(unavailable)?;
        let dirs = r
            .directories
            .into_iter()
            .filter_map(|d| {
                Some(Dir {
                    d: serde_json::from_slice(&d.directory).ok()?,
                    realm: d.realm,
                    feed: FeedState {
                        checked_at: d.checked_unix,
                        applied_at: d.applied_unix,
                        ..Default::default()
                    },
                    ended: d.ended_sids.into_iter().collect(),
                    callback: d.ui_callback,
                })
            })
            .collect();
        let s = Arc::new(LoginState {
            cluster_id: r.cluster_id,
            dirs,
        });
        *lock(&self.state) = Some((s.clone(), Instant::now()));
        Ok(s)
    }

    /// Whether `d`'s credentials are good now (D309).
    pub fn fresh(&self, d: &Dir) -> bool {
        fresh(d, self.default_secs, glidex_trust::now())
    }

    fn stale(&self, d: &Dir) -> people::Error {
        let max =
            d.d.max_staleness_minutes
                .map_or(self.default_secs, |m| m * 60);
        people::Error::Stale(format!(
            "{} hasn't answered for over {} s: its people are refused until it does (local logins work)",
            d.d.issuer, max
        ))
    }

    /// Who person `subject` of directory `dir` is, or the owner of the
    /// gxt_ token whose hash is `hash`; asked at most once a [`CACHE`].
    pub async fn resolve(
        &self,
        dir: &str,
        subject: &str,
        hash: &str,
    ) -> Result<Resolved, people::Error> {
        let key = format!("{dir}\n{subject}\n{hash}");
        if let Some((r, at)) = lock(&self.persons).get(&key)
            && at.elapsed() < CACHE
        {
            return Ok(r.clone());
        }
        let (d, s, h) = (dir.to_string(), subject.to_string(), hash.to_string());
        let r: Resolved = self
            .members
            .each(TrustServiceClient::new, |c| {
                let (d, s, h) = (d.clone(), s.clone(), h.clone());
                async move {
                    c.resolve_person(pb::ResolvePersonRequest {
                        directory: d,
                        subject: s,
                        token_hash: h,
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned())
                }
            })
            .await
            .map_err(unavailable)?
            .into();
        let mut m = lock(&self.persons);
        // Old entries go as new ones come.
        m.retain(|_, (_, at)| at.elapsed() < CACHE);
        m.insert(key, (r.clone(), Instant::now()));
        Ok(r)
    }

    /// The cluster key towards directories, from the map service once
    /// (held in memory only, as the feed's puller holds it).
    pub async fn key(&self) -> Result<ClusterKey, people::Error> {
        let mut k = self.key.lock().await;
        if k.is_none() {
            let r = self
                .members
                .each(TrustServiceClient::new, |c| async move {
                    c.get_trust_key(pb::GetTrustKeyRequest::default())
                        .await
                        .map(|r| r.into_owned())
                })
                .await
                .map_err(unavailable)?;
            *k = Some((r.kid, zeroize::Zeroizing::new(r.pkcs8)));
        }
        let (kid, der) = k.as_ref().expect("fetched");
        ClusterKey::from_pkcs8(kid, der).map_err(people::Error::Unavailable)
    }

    /// The person `login` names, if they may still act (D307–D309): their
    /// directory trusted and fresh, their login chain not ended there,
    /// they in its replica and enabled. Their account here and its roles
    /// (`account_roles`) as [`roles_of`] combines them; `method`,
    /// `age_secs` and `csrf` as the caller found them.
    pub async fn principal(
        &self,
        people: &people::People,
        login: &DirLogin,
        method: &str,
        age_secs: i64,
        csrf: Option<String>,
    ) -> Result<Principal, people::Error> {
        let st = self.state().await?;
        let Some(d) = st.dir(&login.id) else {
            return Err(people::Error::Unauthenticated);
        };
        if !self.fresh(d) {
            return Err(self.stale(d));
        }
        if !login.sid.is_empty() && d.ended.contains(&login.sid) {
            return Err(people::Error::Unauthenticated);
        }
        let r = self.resolve(&d.d.id, &login.subject, "").await?;
        if !r.found || r.disabled {
            return Err(people::Error::Unauthenticated);
        }
        let name = account(&login.subject, &d.realm);
        let u = people
            .directory_user(&name, &d.d.id, &login.subject)
            .await?;
        if u.disabled {
            return Err(people::Error::Unauthenticated);
        }
        Ok(Principal {
            user: u.name,
            roles: roles_of(&r, &u.roles),
            method: method.to_string(),
            age_secs,
            csrf,
            directory: Some(login.clone()),
        })
    }

    /// The person a directory's access token is (glidex D55, D69): signed
    /// with its keys, addressed to this cluster, unexpired. Its
    /// `auth_time` is their login's, for step-up (I391).
    pub async fn access_token(
        &self,
        people: &people::People,
        jwt: &str,
    ) -> Result<Principal, people::Error> {
        let Some(iss) = glidex_trust::tokens::unverified_issuer(jwt) else {
            return Err(people::Error::Unauthenticated);
        };
        let st = self.state().await?;
        let Some(d) = st.dir(&iss) else {
            return Err(people::Error::Unauthenticated);
        };
        let Some(c) = glidex_trust::tokens::verify_access_token(jwt, &d.d, &st.cluster_id) else {
            return Err(people::Error::Unauthenticated);
        };
        let Some(sub) = c["sub"].as_str() else {
            return Err(people::Error::Unauthenticated);
        };
        let now = glidex_trust::now();
        let age = c["auth_time"]
            .as_u64()
            .map_or(glidex_auth::session::NEVER_FRESH, |t| {
                glidex_auth::session::age_secs(t, now)
            });
        let login = DirLogin {
            id: d.d.id.clone(),
            subject: sub.to_string(),
            sid: c["sid"].as_str().unwrap_or_default().to_string(),
        };
        self.principal(people, &login, TOKEN_METHOD, age, None)
            .await
    }

    /// The person a glidex API token (`gxt_…`) is (glidex §7.4): listed
    /// by its hash in a directory's replica, good on this cluster,
    /// unexpired. A person's token acts as they do; a project's service
    /// account's, with what its links give. Never fresh, as tokens here.
    pub async fn api_token(
        &self,
        people: &people::People,
        secret: &str,
    ) -> Result<Principal, people::Error> {
        let hash = glidex_trust::tokens::api_token_hash(secret);
        let r = self.resolve("", "", &hash).await?;
        if !r.found || !r.token_good || r.token_expires <= glidex_trust::now() {
            return Err(people::Error::Unauthenticated);
        }
        let never = glidex_auth::session::NEVER_FRESH;
        let login = DirLogin {
            id: r.directory.clone(),
            subject: r.subject.clone(),
            sid: String::new(),
        };
        if !r.service_account {
            return self
                .principal(people, &login, "directory-api-token", never, None)
                .await;
        }
        let st = self.state().await?;
        let Some(d) = st.dir(&r.directory) else {
            return Err(people::Error::Unauthenticated);
        };
        if !self.fresh(d) {
            return Err(self.stale(d));
        }
        Ok(Principal {
            user: token_account(&r.token_id, &d.realm),
            roles: roles_of(&r, &[]),
            method: "directory-api-token".into(),
            age_secs: never,
            csrf: None,
            directory: Some(login),
        })
    }

    /// Tells directory `id` that login chain `sid` ended here (glidex
    /// D72), in the background; it lapses there anyway.
    pub fn tell_logout(self: &Arc<Self>, id: String, sid: String) {
        let me = self.clone();
        tokio::spawn(async move {
            let (Ok(st), Ok(key)) = (me.state().await, me.key().await) else {
                return;
            };
            let Some(d) = st.dir(&id) else { return };
            if let Err(e) =
                glidex_trust::client::logout(&key, &st.cluster_id, &d.d, &sid, glidex_trust::now())
                    .await
            {
                eprintln!("hyperstor-mgmt: telling {} of a logout: {e}", d.d.issuer);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(max: Option<u64>, checked: u64) -> Dir {
        Dir {
            d: TrustedDirectory {
                id: "trusted-1".into(),
                issuer: "https://id.acme.example/d/acme".into(),
                directory: "acme".into(),
                api: "https://id.acme.example".into(),
                tls_sha256: "ab".repeat(32),
                jwks: serde_json::json!({ "keys": [] }),
                tenants: Vec::new(),
                created_at: 0,
                max_staleness_minutes: max,
            },
            realm: "acme".into(),
            feed: FeedState {
                checked_at: checked,
                ..Default::default()
            },
            ended: HashSet::new(),
            callback: String::new(),
        }
    }

    #[test]
    fn a_directory_is_fresh_within_its_max_staleness() {
        // The cluster's default.
        assert!(fresh(&dir(None, 1000), 3600, 4600));
        assert!(!fresh(&dir(None, 1000), 3600, 4601));
        // Its own.
        assert!(fresh(&dir(Some(2), 1000), 3600, 1120));
        assert!(!fresh(&dir(Some(2), 1000), 3600, 1121));
    }

    #[test]
    fn roles_come_from_links_and_the_operator_s_grants_from_here() {
        let mut r = Resolved {
            found: true,
            grants: vec!["viewer@tenant:acme".into()],
            ..Default::default()
        };
        let here = vec![
            "security-admin".to_string(),
            "viewer@north".into(),
            "storage-admin@tenant:lab".into(),
        ];
        // Not a member of the operator tenant: the links' alone.
        assert_eq!(roles_of(&r, &here), vec!["viewer@tenant:acme"]);
        // A member: also the roles given here, but never in a tenant or
        // project.
        r.operator = true;
        assert_eq!(
            roles_of(&r, &here),
            vec!["security-admin", "viewer@north", "viewer@tenant:acme"]
        );
        // A service account's token: the links' alone.
        r.service_account = true;
        assert_eq!(roles_of(&r, &here), vec!["viewer@tenant:acme"]);
    }

    #[test]
    fn logins_return_to_paths_of_this_host_only() {
        assert_eq!(local_path(Some("/volumes?x=1")), "/volumes?x=1");
        assert_eq!(local_path(Some("//evil.example/")), "/");
        assert_eq!(local_path(Some("https://evil.example/")), "/");
        assert_eq!(local_path(Some("/\\evil")), "/");
        assert_eq!(local_path(None), "/");
        assert_eq!(account("u-1", "acme"), "u-1@acme");
        assert!(crate::people::valid_name(&account(
            "3f1c0a3e-2b44-4c39-9a51-0e1f2a3b4c5d",
            &"r".repeat(63)
        )));
    }
}
