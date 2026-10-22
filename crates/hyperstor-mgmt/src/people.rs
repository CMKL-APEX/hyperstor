//! People: accounts, sessions, API tokens and the first-boot claim
//! (spec/administration.md §23.7, §23.9.1), kept as rows in the map
//! service's store through the cluster API (decisions.md D98), with
//! glidex's rules (`glidex-auth`, D96).
//!
//! | Row | Holds |
//! |---|---|
//! | `user/<name>` | roles, created, disabled |
//! | `sess/<sha-256 of the id>` | user, created, last seen, logged in at, CSRF value |
//! | `tok/<sha-256 of the token>` | id, name, user, created, expires |
//! | `claim` | the first-boot claim code's SHA-256 and expiry |
//!
//! Secrets are never kept, only their SHA-256. A checked session or token
//! is cached for [`CACHE`] at most, so a logout or a revocation reaches
//! every mgmt within that.

use connectrpc::client::{ClientConfig, HttpClient};
use glidex_auth::secrets::{constant_eq, random_token, sha256_hex};
use glidex_auth::session::{self as rules, NEVER_FRESH, SessionRules};
use glidex_auth::token::{TokenRules, mint_token, token_hash};
use hyperstor_api::MgmtStoreServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a checked session or token is trusted without a read (D98).
pub const CACHE: Duration = Duration::from_secs(10);
/// API tokens: `hst_` (§23.9.1).
pub const TOKEN_PREFIX: &str = "hst_";
/// The first-boot claim code is valid an hour (§23.7).
pub const CLAIM_SECS: u64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub name: String,
    pub roles: Vec<String>,
    pub created: u64,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    user: String,
    created: u64,
    last_seen: u64,
    /// When the person last proved who they are (for step-up).
    authenticated_at: u64,
    csrf: String,
    method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub user: String,
    pub created: u64,
    pub expires: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claim {
    hash: String,
    expires: u64,
}

/// Who is calling, once checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub user: String,
    pub roles: Vec<String>,
    /// `pam` or `token`.
    pub method: String,
    /// Seconds since the person last proved who they are; never fresh for
    /// a token.
    pub age_secs: i64,
    /// A session's CSRF value; none for a token.
    pub csrf: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Not logged in, or the session or token is gone.
    Unauthenticated,
    /// The request is malformed (a bad name, lifetime or role).
    Invalid(String),
    NotFound,
    Exists,
    /// The map service can't be reached or has no leader.
    Unavailable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unauthenticated => write!(f, "authentication required"),
            Error::Invalid(m) | Error::Unavailable(m) => write!(f, "{m}"),
            Error::NotFound => write!(f, "not found"),
            Error::Exists => write!(f, "already exists"),
        }
    }
}

fn now() -> u64 {
    glidex_auth::now()
}

/// Where rows live: the map service, or memory (tests).
pub enum Rows {
    /// Every member's API; writes go to whichever answers as the leader.
    Api(Vec<MgmtStoreServiceClient<HttpClient>>),
    #[cfg_attr(not(test), allow(dead_code))] // the tests keep rows in memory
    Memory(Mutex<BTreeMap<String, Vec<u8>>>),
}

impl Rows {
    pub fn api(addrs: &[String], tls: std::sync::Arc<connectrpc::rustls::ClientConfig>) -> Rows {
        Rows::Api(
            addrs
                .iter()
                .map(|a| {
                    let uri: http::Uri = format!("https://{a}").parse().expect("address");
                    MgmtStoreServiceClient::new(
                        HttpClient::with_tls(tls.clone()),
                        ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5)),
                    )
                })
                .collect(),
        )
    }

    #[cfg_attr(not(test), allow(dead_code))] // the tests keep rows in memory
    pub fn memory() -> Rows {
        Rows::Memory(Mutex::new(BTreeMap::new()))
    }

    /// Calls `f` on each member in turn until one answers other than
    /// "unavailable" (down, or not the leader for a write).
    async fn each<T, F, Fut>(&self, f: F) -> Result<T, Error>
    where
        F: Fn(MgmtStoreServiceClient<HttpClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>>,
    {
        let Rows::Api(clients) = self else {
            unreachable!("memory rows are handled by the caller")
        };
        let mut last = String::from("no map service member configured");
        for c in clients {
            match f(c.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) if e.code == connectrpc::ErrorCode::Unavailable => {
                    last = e.message.unwrap_or_default();
                }
                Err(e) if e.code == connectrpc::ErrorCode::AlreadyExists => {
                    return Err(Error::Exists);
                }
                Err(e) => return Err(Error::Unavailable(e.message.unwrap_or_default())),
            }
        }
        Err(Error::Unavailable(last))
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        if let Rows::Memory(m) = self {
            return Ok(lock(m).get(key).cloned());
        }
        let key = key.to_string();
        self.each(|c| {
            let key = key.clone();
            async move {
                let r = c
                    .get_row(pb::GetRowRequest {
                        key,
                        ..Default::default()
                    })
                    .await?
                    .into_owned();
                Ok(r.found.then_some(r.value))
            }
        })
        .await
    }

    pub async fn put(&self, key: &str, value: Vec<u8>, if_absent: bool) -> Result<(), Error> {
        if let Rows::Memory(m) = self {
            let mut m = lock(m);
            if if_absent && m.contains_key(key) {
                return Err(Error::Exists);
            }
            m.insert(key.to_string(), value);
            return Ok(());
        }
        let key = key.to_string();
        self.each(|c| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                c.put_row(pb::PutRowRequest {
                    key,
                    value,
                    if_absent,
                    ..Default::default()
                })
                .await?;
                Ok(())
            }
        })
        .await
    }

    pub async fn delete(&self, key: &str) -> Result<bool, Error> {
        if let Rows::Memory(m) = self {
            return Ok(lock(m).remove(key).is_some());
        }
        let key = key.to_string();
        self.each(|c| {
            let key = key.clone();
            async move {
                Ok(c.delete_row(pb::DeleteRowRequest {
                    key,
                    ..Default::default()
                })
                .await?
                .into_owned()
                .found)
            }
        })
        .await
    }

    /// Every row under `prefix`, page by page.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, Error> {
        if let Rows::Memory(m) = self {
            return Ok(lock(m)
                .range(prefix.to_string()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect());
        }
        let mut out: Vec<(String, Vec<u8>)> = Vec::new();
        loop {
            let after = out.last().map(|(k, _)| k.clone()).unwrap_or_default();
            let prefix = prefix.to_string();
            let page = self
                .each(|c| {
                    let (prefix, after) = (prefix.clone(), after.clone());
                    async move {
                        Ok(c.list_rows(pb::ListRowsRequest {
                            prefix,
                            after,
                            limit: 1000,
                            ..Default::default()
                        })
                        .await?
                        .into_owned()
                        .rows)
                    }
                })
                .await?;
            let n = page.len();
            out.extend(page.into_iter().map(|r| (r.key, r.value)));
            if n < 1000 {
                return Ok(out);
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn decode<T: for<'de> Deserialize<'de>>(b: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(b).map_err(|e| Error::Unavailable(format!("a stored record: {e}")))
}

fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).expect("json")
}

/// A user name as PAM gives it, for a key.
fn check_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{name:?}: not a user name")))
    }
}

/// Which user an identity provider's person is, under
/// `oidc/<sha256(issuer, subject)>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Link {
    user: String,
    issuer: String,
}

/// A new session: the cookie's value and the CSRF value to send back.
pub struct NewSession {
    pub id: String,
    pub csrf: String,
}

pub struct People {
    rows: Rows,
    sessions: SessionRules,
    tokens: TokenRules,
    cache: Mutex<HashMap<String, (Principal, Instant)>>,
}

impl People {
    pub fn new(rows: Rows) -> People {
        People {
            rows,
            // §23.9.1: idle 30 minutes, absolute 12 hours; tokens 90 days
            // by default, 365 at most.
            sessions: SessionRules::new(30, 12),
            tokens: TokenRules {
                default_days: 90,
                max_days: 365,
            },
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn user(&self, name: &str) -> Result<Option<User>, Error> {
        check_name(name)?;
        match self.rows.get(&format!("user/{name}")).await? {
            Some(b) => Ok(Some(decode(&b)?)),
            None => Ok(None),
        }
    }

    pub async fn users(&self) -> Result<Vec<User>, Error> {
        self.rows
            .list("user/")
            .await?
            .iter()
            .map(|(_, v)| decode(v))
            .collect()
    }

    /// The user `name` after a password check, made on first login with
    /// no roles (a security administrator grants them).
    pub async fn logged_in_user(&self, name: &str) -> Result<User, Error> {
        if let Some(u) = self.user(name).await? {
            return Ok(u);
        }
        let u = User {
            name: name.to_string(),
            roles: Vec::new(),
            created: now(),
            disabled: false,
        };
        match self
            .rows
            .put(&format!("user/{name}"), encode(&u), true)
            .await
        {
            Ok(()) => Ok(u),
            // Made by a concurrent first login: that one.
            Err(Error::Exists) => self.user(name).await?.ok_or(Error::NotFound),
            Err(e) => Err(e),
        }
    }

    /// The user an identity provider's person is (`issuer`, `subject`),
    /// made on first login with no roles and named by their email, which
    /// the provider must vouch for. Tied to the subject, not the email: a
    /// changed email keeps the account (I136).
    pub async fn oidc_user(
        &self,
        issuer: &str,
        subject: &str,
        email: Option<&str>,
        email_verified: bool,
    ) -> Result<User, Error> {
        let key = format!(
            "oidc/{}",
            sha256_hex(format!("{issuer}\n{subject}").as_bytes())
        );
        if let Some(b) = self.rows.get(&key).await? {
            let link: Link = decode(&b)?;
            return self.logged_in_user(&link.user).await;
        }
        let name = match email {
            Some(e) if email_verified => e.to_ascii_lowercase(),
            Some(_) => {
                return Err(Error::Invalid(
                    "the identity provider doesn't vouch for this email".into(),
                ));
            }
            None => {
                return Err(Error::Invalid(
                    "the identity provider gave no email (scope `email`)".into(),
                ));
            }
        };
        check_name(&name)?;
        let link = Link {
            user: name.clone(),
            issuer: issuer.to_string(),
        };
        match self.rows.put(&key, encode(&link), true).await {
            Ok(()) | Err(Error::Exists) => {}
            Err(e) => return Err(e),
        }
        // A concurrent first login may have linked first: that one counts.
        let link: Link = decode(&self.rows.get(&key).await?.ok_or(Error::NotFound)?)?;
        self.logged_in_user(&link.user).await
    }

    /// Sets `name`'s roles (security administrators, step-up).
    pub async fn set_roles(&self, name: &str, roles: &[String]) -> Result<User, Error> {
        for r in roles {
            if !crate::policy::ROLES.contains(&r.as_str()) {
                return Err(Error::Invalid(format!("{r}: not a role")));
            }
        }
        let mut u = self.user(name).await?.ok_or(Error::NotFound)?;
        let mut roles = roles.to_vec();
        roles.sort();
        roles.dedup();
        u.roles = roles;
        self.rows
            .put(&format!("user/{name}"), encode(&u), false)
            .await?;
        self.forget(|p| p.user == name);
        Ok(u)
    }

    /// Removes `name`, their sessions and their tokens.
    pub async fn delete_user(&self, name: &str) -> Result<(), Error> {
        check_name(name)?;
        if !self.rows.delete(&format!("user/{name}")).await? {
            return Err(Error::NotFound);
        }
        for prefix in ["sess/", "tok/", "oidc/"] {
            for (k, v) in self.rows.list(prefix).await? {
                let owner = match prefix {
                    "sess/" => decode::<Session>(&v).map(|s| s.user),
                    "tok/" => decode::<Token>(&v).map(|t| t.user),
                    _ => decode::<Link>(&v).map(|l| l.user),
                };
                if owner.as_deref() == Ok(name) {
                    self.rows.delete(&k).await?;
                }
            }
        }
        self.forget(|p| p.user == name);
        Ok(())
    }

    fn forget(&self, f: impl Fn(&Principal) -> bool) {
        lock(&self.cache).retain(|_, (p, _)| !f(p));
    }

    /// A session for `user`, just authenticated by `method`.
    pub async fn new_session(&self, user: &str, method: &str) -> Result<NewSession, Error> {
        let s = rules::new_session();
        let t = now();
        let rec = Session {
            user: user.to_string(),
            created: t,
            last_seen: t,
            authenticated_at: t,
            csrf: s.csrf.clone(),
            method: method.to_string(),
        };
        self.rows
            .put(&format!("sess/{}", s.hash), encode(&rec), true)
            .await?;
        Ok(NewSession {
            id: s.id,
            csrf: s.csrf,
        })
    }

    /// The person a session cookie belongs to; `Unauthenticated` if none
    /// or expired (and then removed).
    pub async fn session(&self, cookie: &str) -> Result<Principal, Error> {
        let hash = rules::session_hash(cookie);
        let key = format!("sess/{hash}");
        if let Some(p) = self.cached(&key) {
            return Ok(p);
        }
        let Some(b) = self.rows.get(&key).await? else {
            return Err(Error::Unauthenticated);
        };
        let mut s: Session = decode(&b)?;
        let t = now();
        if self.sessions.expired(s.created, s.last_seen, t) {
            self.rows.delete(&key).await?;
            return Err(Error::Unauthenticated);
        }
        let u = self.user(&s.user).await?.ok_or(Error::Unauthenticated)?;
        if u.disabled {
            return Err(Error::Unauthenticated);
        }
        if rules::touch_due(Some(s.last_seen), t) {
            s.last_seen = t;
            // A lost update only shortens the idle time; not worth failing.
            let _ = self.rows.put(&key, encode(&s), false).await;
        }
        let p = Principal {
            user: u.name,
            roles: u.roles,
            method: s.method,
            age_secs: rules::age_secs(s.authenticated_at, t),
            csrf: Some(s.csrf),
        };
        lock(&self.cache).insert(key, (p.clone(), Instant::now()));
        Ok(p)
    }

    fn cached(&self, key: &str) -> Option<Principal> {
        let mut c = lock(&self.cache);
        match c.get(key) {
            Some((p, at)) if at.elapsed() < CACHE => Some(p.clone()),
            Some(_) => {
                c.remove(key);
                None
            }
            None => None,
        }
    }

    pub async fn end_session(&self, cookie: &str) -> Result<(), Error> {
        let key = format!("sess/{}", rules::session_hash(cookie));
        lock(&self.cache).remove(&key);
        self.rows.delete(&key).await?;
        Ok(())
    }

    /// A new API token for `user`: the secret, shown once, and its record.
    pub async fn create_token(
        &self,
        user: &str,
        name: &str,
        days: Option<u64>,
    ) -> Result<(String, Token), Error> {
        glidex_auth::token::check_name(name).map_err(|e| Error::Invalid(e.to_string()))?;
        let t = now();
        let expires = self
            .tokens
            .expiry(days, t)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let m = mint_token(TOKEN_PREFIX);
        let rec = Token {
            id: m.hash[..12].to_string(),
            name: name.to_string(),
            user: user.to_string(),
            created: t,
            expires,
        };
        self.rows
            .put(&format!("tok/{}", m.hash), encode(&rec), true)
            .await?;
        Ok((m.secret, rec))
    }

    pub async fn tokens_of(&self, user: &str) -> Result<Vec<Token>, Error> {
        Ok(self
            .rows
            .list("tok/")
            .await?
            .iter()
            .filter_map(|(_, v)| decode::<Token>(v).ok())
            .filter(|t| t.user == user)
            .collect())
    }

    /// Removes `user`'s token with id `id`.
    pub async fn revoke_token(&self, user: &str, id: &str) -> Result<(), Error> {
        for (k, v) in self.rows.list("tok/").await? {
            if let Ok(t) = decode::<Token>(&v)
                && t.user == user
                && t.id == id
            {
                self.rows.delete(&k).await?;
                lock(&self.cache).remove(&k);
                return Ok(());
            }
        }
        Err(Error::NotFound)
    }

    /// The person a bearer token belongs to.
    pub async fn token(&self, secret: &str) -> Result<Principal, Error> {
        if !secret.starts_with(TOKEN_PREFIX) {
            return Err(Error::Unauthenticated);
        }
        let key = format!("tok/{}", token_hash(secret));
        if let Some(p) = self.cached(&key) {
            return Ok(p);
        }
        let Some(b) = self.rows.get(&key).await? else {
            return Err(Error::Unauthenticated);
        };
        let t: Token = decode(&b)?;
        if t.expires <= now() {
            return Err(Error::Unauthenticated);
        }
        let u = self.user(&t.user).await?.ok_or(Error::Unauthenticated)?;
        if u.disabled {
            return Err(Error::Unauthenticated);
        }
        let p = Principal {
            user: u.name,
            roles: u.roles,
            method: "token".into(),
            // Never fresh: step-up needs a person (glidex security.md §7).
            age_secs: NEVER_FRESH,
            csrf: None,
        };
        lock(&self.cache).insert(key, (p.clone(), Instant::now()));
        Ok(p)
    }

    /// Whether anyone is a security administrator yet.
    pub async fn claimed(&self) -> Result<bool, Error> {
        Ok(self
            .users()
            .await?
            .iter()
            .any(|u| u.roles.iter().any(|r| r == "security-admin")))
    }

    /// A new first-boot claim code, valid [`CLAIM_SECS`], replacing any.
    pub async fn new_claim(&self) -> Result<String, Error> {
        let code = random_token(18);
        let c = Claim {
            hash: sha256_hex(code.as_bytes()),
            expires: now() + CLAIM_SECS,
        };
        self.rows.put("claim", encode(&c), false).await?;
        Ok(code)
    }

    /// Redeems the claim code for `user` (already authenticated): they
    /// become a security administrator, and the code is gone.
    pub async fn claim(&self, code: &str, user: &str) -> Result<User, Error> {
        let Some(b) = self.rows.get("claim").await? else {
            return Err(Error::Invalid("no claim is open".into()));
        };
        let c: Claim = decode(&b)?;
        if c.expires <= now() || !constant_eq(&c.hash, &sha256_hex(code.as_bytes())) {
            return Err(Error::Invalid("wrong or expired claim code".into()));
        }
        self.rows.delete("claim").await?;
        self.logged_in_user(user).await?;
        let mut roles = self.user(user).await?.ok_or(Error::NotFound)?.roles;
        roles.push("security-admin".into());
        self.set_roles(user, &roles).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn sessions_tokens_and_roles() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            assert!(!p.claimed().await.unwrap());
            let u = p.logged_in_user("ann").await.unwrap();
            assert!(u.roles.is_empty(), "no roles until granted");
            let s = p.new_session("ann", "pam").await.unwrap();
            let who = p.session(&s.id).await.unwrap();
            assert_eq!((who.user.as_str(), who.method.as_str()), ("ann", "pam"));
            assert_eq!(who.csrf.as_deref(), Some(s.csrf.as_str()));
            assert!(who.age_secs < 5);
            assert_eq!(p.session("nope").await, Err(Error::Unauthenticated));
            // Roles show at once on this mgmt (the cache is forgotten).
            p.set_roles("ann", &["viewer".into(), "viewer".into()])
                .await
                .unwrap();
            assert_eq!(p.session(&s.id).await.unwrap().roles, vec!["viewer"]);
            assert!(p.set_roles("ann", &["root".into()]).await.is_err());
            // Tokens: hst_, shown once, never fresh, revocable.
            let (secret, t) = p.create_token("ann", "ci", Some(30)).await.unwrap();
            assert!(secret.starts_with("hst_"));
            let by = p.token(&secret).await.unwrap();
            assert_eq!((by.user.as_str(), by.age_secs), ("ann", NEVER_FRESH));
            assert!(p.create_token("ann", "x", Some(400)).await.is_err());
            assert_eq!(p.tokens_of("ann").await.unwrap(), vec![t.clone()]);
            p.revoke_token("ann", &t.id).await.unwrap();
            assert_eq!(p.token(&secret).await, Err(Error::Unauthenticated));
            // Logout, and a deleted user's sessions go.
            p.end_session(&s.id).await.unwrap();
            assert_eq!(p.session(&s.id).await, Err(Error::Unauthenticated));
            let s2 = p.new_session("ann", "pam").await.unwrap();
            p.delete_user("ann").await.unwrap();
            assert_eq!(p.session(&s2.id).await, Err(Error::Unauthenticated));
            // Only what was stored is hashes.
            if let Rows::Memory(m) = &p.rows {
                let all = format!("{:?}", lock(m));
                assert!(!all.contains(&s2.id) && !all.contains(&secret));
            }
        });
    }

    /// An identity provider's person is their verified email, tied to the
    /// subject: a changed email keeps the account; an unverified or missing
    /// email gets none; deleting the user unties them.
    #[test]
    fn oidc_people_are_their_subject() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            let iss = "https://idp.example";
            let u = p
                .oidc_user(iss, "s1", Some("Ann@Example.com"), true)
                .await
                .unwrap();
            assert_eq!((u.name.as_str(), u.roles.len()), ("ann@example.com", 0));
            p.set_roles(&u.name, &["viewer".into()]).await.unwrap();
            let again = p
                .oidc_user(iss, "s1", Some("ann.new@example.com"), true)
                .await
                .unwrap();
            assert_eq!(again.name, "ann@example.com");
            assert_eq!(again.roles, ["viewer"]);
            // Another issuer's s1 is someone else.
            let other = p.oidc_user("https://other.example", "s1", Some("x@example.com"), true);
            assert_eq!(other.await.unwrap().name, "x@example.com");
            assert!(matches!(
                p.oidc_user(iss, "s2", Some("eve@example.com"), false).await,
                Err(Error::Invalid(_))
            ));
            assert!(matches!(
                p.oidc_user(iss, "s3", None, true).await,
                Err(Error::Invalid(_))
            ));
            p.delete_user("ann@example.com").await.unwrap();
            let back = p
                .oidc_user(iss, "s1", Some("ann.new@example.com"), true)
                .await
                .unwrap();
            assert_eq!(
                (back.name.as_str(), back.roles.len()),
                ("ann.new@example.com", 0)
            );
        });
    }

    #[test]
    fn the_first_claim_makes_a_security_admin_once() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            let code = p.new_claim().await.unwrap();
            assert!(p.claim("wrong", "bob").await.is_err());
            let u = p.claim(&code, "bob").await.unwrap();
            assert_eq!(u.roles, vec!["security-admin"]);
            assert!(p.claimed().await.unwrap());
            assert!(p.claim(&code, "eve").await.is_err(), "used once");
        });
    }
}
