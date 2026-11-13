//! People: accounts, sessions, API tokens and the first-boot claim
//! (spec/19-administration.md §23.7, §23.9.1), kept as rows in the map
//! service's store through the cluster API (04-decisions.md D98), with
//! glidex's rules (`glidex-auth`, D96).
//!
//! | Row | Holds |
//! |---|---|
//! | `user/<name>` | roles, created, disabled; for a directory's person, the directory and their id there |
//! | `sess/<sha-256 of the id>` | user, created, last seen, logged in at, CSRF value; for a login through a directory, its chain |
//! | `tok/<sha-256 of the token>` | id, name, user, created, expires |
//! | `claim` | the first-boot claim code's SHA-256 and expiry |
//! | `login/<sha-256 of the state>` | an identity provider's login begun and not finished, sealed |
//!
//! Secrets are never kept, only their SHA-256, but for a login begun with
//! an identity provider: its state ([`Pending`]) is kept where every mgmt
//! finds it, whichever the provider sends the browser back to, sealed
//! with a key only the browser that began it holds (its `hs_oidc`
//! cookie), for [`STATE_SECS`] at most, and used once. A checked session or token
//! is cached for [`CACHE`] at most, so a logout or a revocation reaches
//! every mgmt within that.

use connectrpc::client::HttpClient;
use glidex_auth::oidc::{Pending, STATE_SECS};
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
/// Logins begun with an identity provider, by their state's SHA-256.
const LOGIN: &str = "login/";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub name: String,
    pub roles: Vec<String>,
    pub created: u64,
    #[serde(default)]
    pub disabled: bool,
    /// A glidex directory's person (D307): the directory's trusted id,
    /// and their id there; empty for a local account.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub directory: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
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
    /// A login through a directory (D307): its trusted id, the person's id
    /// there, and the login's chain (`sid`), which the directory may end.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    directory: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    subject: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    sid: String,
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
    /// Who they are at a glidex directory, for a login or token from one:
    /// their roles there come from its feed ([`crate::directory`]).
    pub directory: Option<DirLogin>,
}

/// A person of a glidex directory, as a session or token names them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirLogin {
    /// The directory's trusted id (`trusted-…`).
    pub id: String,
    /// Their user id there (a service account's token id, for its
    /// token).
    pub subject: String,
    /// The login's chain; empty for an API token.
    pub sid: String,
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
    /// Too many logins begun and not finished: try again later.
    Busy,
    /// A directory's feed is older than its `max_staleness` (D309): its
    /// people are refused until it answers again.
    Stale(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unauthenticated => write!(f, "authentication required"),
            Error::Invalid(m) | Error::Unavailable(m) => write!(f, "{m}"),
            Error::NotFound => write!(f, "not found"),
            Error::Exists => write!(f, "already exists"),
            Error::Busy => write!(f, "too many logins in progress; try again later"),
            Error::Stale(m) => write!(f, "{m}"),
        }
    }
}

fn now() -> u64 {
    glidex_auth::now()
}

/// Where rows live: the map service, or memory (tests).
pub enum Rows {
    /// The map service's members; writes go to the leader first (D276).
    Api(std::sync::Arc<crate::members::Members>),
    #[cfg_attr(not(test), allow(dead_code))] // the tests keep rows in memory
    Memory(Mutex<BTreeMap<String, Vec<u8>>>),
}

impl Rows {
    pub fn api(members: std::sync::Arc<crate::members::Members>) -> Rows {
        Rows::Api(members)
    }

    #[cfg_attr(not(test), allow(dead_code))] // the tests keep rows in memory
    pub fn memory() -> Rows {
        Rows::Memory(Mutex::new(BTreeMap::new()))
    }

    /// Calls `f` on each member in turn, the leader first, until one
    /// answers other than "unavailable" (down, or not the leader for a
    /// write).
    async fn each<T, F, Fut>(&self, f: F) -> Result<T, Error>
    where
        F: Fn(MgmtStoreServiceClient<HttpClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>>,
    {
        let Rows::Api(members) = self else {
            unreachable!("memory rows are handled by the caller")
        };
        match members.each(MgmtStoreServiceClient::new, f).await {
            Ok(v) => Ok(v),
            Err(e) if e.code == connectrpc::ErrorCode::AlreadyExists => Err(Error::Exists),
            Err(e) => Err(Error::Unavailable(e.message.unwrap_or_default())),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        if let Rows::Memory(m) = self {
            // As a call to the map service would, so that concurrent
            // callers interleave in tests.
            tokio::task::yield_now().await;
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
            tokio::task::yield_now().await;
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

/// When a kept login began, if its row is whole (time, nonce, sealed).
fn begun(v: &[u8]) -> Option<u64> {
    let t = v.get(..8)?.try_into().ok()?;
    (v.len() > 8 + 12).then(|| u64::from_be_bytes(t))
}

/// The key a login is sealed with: from the browser-binding cookie, which
/// only the browser that began the login holds.
fn sealer(browser: &str) -> ring::aead::LessSafeKey {
    let k = ring::digest::digest(
        &ring::digest::SHA256,
        format!("hyperstor-mgmt oidc login\n{browser}").as_bytes(),
    );
    ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, k.as_ref()).expect("key"),
    )
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

/// Whether `name` has a user name's shape: 1-128 of letters, digits and
/// `._-@` (a directory's person is `<their id>@<realm>`, I388).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
}

/// A user name as PAM gives it, for a key.
fn check_name(name: &str) -> Result<(), Error> {
    if valid_name(name) {
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
    /// Session and token keys the map service just didn't have.
    unknown: crate::limits::Unknown,
    /// Whether anyone is a security administrator, and when asked.
    claimed: Mutex<Option<(bool, Instant)>>,
    /// Logins begun with an identity provider: when the rows were last
    /// swept, and about how many are pending since.
    logins: Mutex<(Option<Instant>, usize)>,
}

/// Logins begun with an identity provider and not finished that mgmt
/// keeps at most (each a row in the map service for [`STATE_SECS`]).
pub const PENDING_LOGINS: usize = 1000;
/// How often the pending logins' rows are listed and the old removed.
const SWEEP: Duration = Duration::from_secs(60);

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
            unknown: crate::limits::Unknown::new(
                crate::limits::UNKNOWN_TTL,
                crate::limits::UNKNOWN,
            ),
            claimed: Mutex::new(None),
            logins: Mutex::new((None, 0)),
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
            directory: String::new(),
            subject: String::new(),
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

    /// The account here of person `subject` of directory `dir` (D307):
    /// `name`, made on first use with no roles, as a password login's is.
    /// A local account of that name isn't theirs.
    pub async fn directory_user(
        &self,
        name: &str,
        dir: &str,
        subject: &str,
    ) -> Result<User, Error> {
        check_name(name)?;
        let mine = |u: &User| u.directory == dir && u.subject == subject;
        if let Some(u) = self.user(name).await? {
            return if mine(&u) {
                Ok(u)
            } else {
                Err(Error::Invalid(format!("{name}: a local account's name")))
            };
        }
        let u = User {
            name: name.to_string(),
            roles: Vec::new(),
            created: now(),
            disabled: false,
            directory: dir.to_string(),
            subject: subject.to_string(),
        };
        match self
            .rows
            .put(&format!("user/{name}"), encode(&u), true)
            .await
        {
            Ok(()) => Ok(u),
            Err(Error::Exists) => match self.user(name).await? {
                Some(u) if mine(&u) => Ok(u),
                _ => Err(Error::Invalid(format!("{name}: a local account's name"))),
            },
            Err(e) => Err(e),
        }
    }

    /// Sets `name`'s roles (security administrators, step-up): each a
    /// role, cluster-wide, or `role@realm` (D263).
    pub async fn set_roles(&self, name: &str, roles: &[String]) -> Result<User, Error> {
        for r in roles {
            if crate::policy::grant(r).is_none() {
                return Err(Error::Invalid(format!("{r}: not a role, nor role@realm")));
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
        *lock(&self.claimed) = None;
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
        *lock(&self.claimed) = None;
        Ok(())
    }

    fn forget(&self, f: impl Fn(&Principal) -> bool) {
        lock(&self.cache).retain(|_, (p, _)| !f(p));
    }

    /// A session for `user`, just authenticated by `method`.
    pub async fn new_session(&self, user: &str, method: &str) -> Result<NewSession, Error> {
        self.new_session_from(user, method, None, now()).await
    }

    /// A session for `user`, authenticated by `method` at
    /// `authenticated_at`, through directory login `dir` if any (D307).
    pub async fn new_session_from(
        &self,
        user: &str,
        method: &str,
        dir: Option<&DirLogin>,
        authenticated_at: u64,
    ) -> Result<NewSession, Error> {
        let s = rules::new_session();
        let t = now();
        let d = dir.cloned().unwrap_or_default();
        let rec = Session {
            user: user.to_string(),
            created: t,
            last_seen: t,
            authenticated_at: authenticated_at.min(t),
            csrf: s.csrf.clone(),
            method: method.to_string(),
            directory: d.id,
            subject: d.subject,
            sid: d.sid,
        };
        let key = format!("sess/{}", s.hash);
        self.rows.put(&key, encode(&rec), true).await?;
        self.unknown.remove(&key);
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
        if self.unknown.has(&key, Instant::now()) {
            return Err(Error::Unauthenticated);
        }
        let Some(b) = self.rows.get(&key).await? else {
            self.unknown.insert(&key, Instant::now());
            return Err(Error::Unauthenticated);
        };
        let mut s: Session = decode(&b)?;
        let t = now();
        if self.sessions.expired(s.created, s.last_seen, t) {
            self.rows.delete(&key).await?;
            self.unknown.insert(&key, Instant::now());
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
        let directory = (!s.directory.is_empty()).then(|| DirLogin {
            id: s.directory.clone(),
            subject: s.subject.clone(),
            sid: s.sid.clone(),
        });
        let p = Principal {
            user: u.name,
            roles: u.roles,
            method: s.method,
            age_secs: rules::age_secs(s.authenticated_at, t),
            csrf: Some(s.csrf),
            directory,
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
        if self.unknown.has(&key, Instant::now()) {
            return Err(Error::Unauthenticated);
        }
        let Some(b) = self.rows.get(&key).await? else {
            self.unknown.insert(&key, Instant::now());
            return Err(Error::Unauthenticated);
        };
        let t: Token = decode(&b)?;
        if t.expires <= now() {
            self.unknown.insert(&key, Instant::now());
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
            directory: None,
        };
        lock(&self.cache).insert(key, (p.clone(), Instant::now()));
        Ok(p)
    }

    /// Keeps a login begun with an identity provider for whichever mgmt
    /// the browser comes back to, and forgets those begun too long ago.
    /// At most [`PENDING_LOGINS`] are kept: past that, [`Error::Busy`].
    /// The rows are listed (and the old removed) once a [`SWEEP`], not
    /// for each login; in between, the count is this mgmt's own.
    pub async fn keep_login<A: Serialize>(&self, p: &Pending<A>) -> Result<(), Error> {
        let due = lock(&self.logins).0.is_none_or(|at| at.elapsed() >= SWEEP);
        if due {
            let t = now();
            let mut left = 0;
            for (k, v) in self.rows.list(LOGIN).await? {
                if begun(&v).is_none_or(|b| b + STATE_SECS <= t) {
                    self.rows.delete(&k).await?;
                } else {
                    left += 1;
                }
            }
            *lock(&self.logins) = (Some(Instant::now()), left);
        }
        if lock(&self.logins).1 >= PENDING_LOGINS {
            return Err(Error::Busy);
        }
        let key = format!("{LOGIN}{}", sha256_hex(p.state.as_bytes()));
        let mut nonce = [0u8; 12];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
            .map_err(|_| Error::Unavailable("no random numbers".into()))?;
        let mut sealed = encode(p);
        sealer(&p.browser)
            .seal_in_place_append_tag(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::from(key.as_bytes()),
                &mut sealed,
            )
            .map_err(|_| Error::Unavailable("sealing a login".into()))?;
        let mut v = p.created.to_be_bytes().to_vec();
        v.extend(nonce);
        v.extend(sealed);
        self.rows.put(&key, v, true).await?;
        lock(&self.logins).1 += 1;
        Ok(())
    }

    /// Takes the login begun with `state`, once, if `browser` began it;
    /// [`glidex_auth::oidc::OidcState::finish`] checks the rest.
    pub async fn take_login<A: for<'de> Deserialize<'de>>(
        &self,
        state: &str,
        browser: Option<&str>,
    ) -> Result<Pending<A>, Error> {
        let unknown = || Error::Invalid("unknown or expired login state".into());
        let other = || Error::Invalid("login state belongs to another browser".into());
        let key = format!("{LOGIN}{}", sha256_hex(state.as_bytes()));
        let v = self.rows.get(&key).await?.ok_or_else(unknown)?;
        // Whoever deletes it has it: a second callback with the same
        // state finds none.
        if !self.rows.delete(&key).await? {
            return Err(unknown());
        }
        {
            let mut l = lock(&self.logins);
            l.1 = l.1.saturating_sub(1);
        }
        let (Some(browser), Some(_)) = (browser, begun(&v)) else {
            return Err(other());
        };
        let (nonce, sealed) = v[8..].split_at(12);
        let mut sealed = sealed.to_vec();
        let plain = sealer(browser)
            .open_in_place(
                ring::aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| other())?,
                ring::aead::Aad::from(key.as_bytes()),
                &mut sealed,
            )
            .map_err(|_| other())?;
        decode(plain)
    }

    /// Whether anyone is a security administrator yet.
    /// Remembered for [`CACHE`]; a change of roles here asks again.
    pub async fn claimed(&self) -> Result<bool, Error> {
        if let Some((c, at)) = *lock(&self.claimed)
            && at.elapsed() < CACHE
        {
            return Ok(c);
        }
        let c = self
            .users()
            .await?
            .iter()
            .any(|u| u.roles.iter().any(|r| r == "security-admin"));
        *lock(&self.claimed) = Some((c, Instant::now()));
        Ok(c)
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
        // Whoever deletes it has it: of two redeeming the code at once,
        // only one finds the row still there (I371).
        if !self.rows.delete("claim").await? {
            return Err(Error::Invalid("no claim is open".into()));
        }
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

    fn pending(state: &str, browser: &str, created: u64) -> Pending<String> {
        Pending {
            state: state.into(),
            nonce: "n".into(),
            verifier: "the-verifier".into(),
            return_to: "/volumes".into(),
            browser: browser.into(),
            created,
            after: "claim".into(),
        }
    }

    #[test]
    fn logins_begun_are_kept_sealed_and_taken_once() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            p.keep_login(&pending("s1", "b1", now())).await.unwrap();
            // Kept under the state's digest, and sealed: no secret shows.
            let rows = p.rows.list(LOGIN).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].0, format!("{LOGIN}{}", sha256_hex(b"s1")));
            let raw = String::from_utf8_lossy(&rows[0].1);
            assert!(
                !raw.contains("the-verifier") && !raw.contains("claim"),
                "{raw}"
            );
            let got: Pending<String> = p.take_login("s1", Some("b1")).await.unwrap();
            assert_eq!(
                (got.verifier.as_str(), got.after.as_str()),
                ("the-verifier", "claim")
            );
            // Once.
            let again = p.take_login::<String>("s1", Some("b1")).await;
            assert!(matches!(again, Err(Error::Invalid(m)) if m.contains("unknown")));
            // Another browser can't open it, and has used it up.
            p.keep_login(&pending("s2", "b2", now())).await.unwrap();
            let other = p.take_login::<String>("s2", Some("b3")).await;
            assert!(matches!(other, Err(Error::Invalid(m)) if m.contains("another browser")));
            assert!(p.take_login::<String>("s2", Some("b2")).await.is_err());
            p.keep_login(&pending("s3", "b3", now())).await.unwrap();
            assert!(p.take_login::<String>("s3", None).await.is_err());
            // A new login forgets those begun too long ago (once a sweep
            // is due).
            p.keep_login(&pending("old", "b", now() - STATE_SECS))
                .await
                .unwrap();
            lock(&p.logins).0 = None;
            p.keep_login(&pending("s4", "b4", now())).await.unwrap();
            let left: Vec<String> = p
                .rows
                .list(LOGIN)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.0)
                .collect();
            assert_eq!(left, [format!("{LOGIN}{}", sha256_hex(b"s4"))]);
        });
    }

    #[test]
    fn pending_logins_are_bounded() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            for i in 0..PENDING_LOGINS {
                p.keep_login(&pending(&format!("s{i}"), "b", now()))
                    .await
                    .unwrap();
            }
            let more = p.keep_login(&pending("more", "b", now())).await;
            assert_eq!(more, Err(Error::Busy));
            // One finished makes room; a sweep counts what is there.
            p.take_login::<String>("s0", Some("b")).await.unwrap();
            p.keep_login(&pending("more", "b", now())).await.unwrap();
            lock(&p.logins).0 = None;
            let again = p.keep_login(&pending("again", "b", now())).await;
            assert_eq!(again, Err(Error::Busy));
        });
    }

    #[test]
    fn unknown_sessions_and_tokens_are_remembered() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            let s = p.new_session("ann", "pam").await.unwrap();
            p.logged_in_user("ann").await.unwrap();
            // A cookie the store doesn't have: asked once, then known.
            let key = format!("sess/{}", rules::session_hash("nope"));
            assert_eq!(p.session("nope").await, Err(Error::Unauthenticated));
            let Rows::Memory(m) = &p.rows else {
                unreachable!()
            };
            let row = lock(m)
                .get(&format!("sess/{}", rules::session_hash(&s.id)))
                .cloned()
                .unwrap();
            lock(m).insert(key, row);
            assert_eq!(p.session("nope").await, Err(Error::Unauthenticated));
            // So is a token.
            let secret = format!("{TOKEN_PREFIX}nope");
            assert_eq!(p.token(&secret).await, Err(Error::Unauthenticated));
            let (made, _) = p.create_token("ann", "t", None).await.unwrap();
            assert!(p.token(&made).await.is_ok());
            // Real sessions are unaffected.
            assert!(p.session(&s.id).await.is_ok());
        });
    }

    #[test]
    fn claimed_is_remembered_until_roles_change() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            p.logged_in_user("ann").await.unwrap();
            assert!(!p.claimed().await.unwrap());
            p.set_roles("ann", &["security-admin".into()])
                .await
                .unwrap();
            assert!(p.claimed().await.unwrap());
        });
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
            for bad in ["root@acme", "viewer@", "viewer@Acme", "viewer@a,b"] {
                assert!(p.set_roles("ann", &[bad.into()]).await.is_err(), "{bad}");
            }
            let u = p
                .set_roles("ann", &["viewer".into(), "storage-admin@acme".into()])
                .await
                .unwrap();
            assert_eq!(u.roles, ["storage-admin@acme", "viewer"]);
            p.set_roles("ann", &["viewer".into()]).await.unwrap();
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

    /// Two people redeeming the code at the same moment: one becomes the
    /// security administrator, the other is refused.
    #[test]
    fn a_claim_redeemed_twice_at_once_makes_one_admin() {
        rt().block_on(async {
            let p = People::new(Rows::memory());
            let code = p.new_claim().await.unwrap();
            let (a, b) = tokio::join!(p.claim(&code, "bob"), p.claim(&code, "eve"));
            let ok = [&a, &b].iter().filter(|r| r.is_ok()).count();
            assert_eq!(ok, 1, "{a:?} {b:?}");
            let refused = if a.is_ok() { &b } else { &a };
            assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
            let admins: Vec<String> = p
                .users()
                .await
                .unwrap()
                .into_iter()
                .filter(|u| u.roles.iter().any(|r| r == "security-admin"))
                .map(|u| u.name)
                .collect();
            assert_eq!(admins.len(), 1, "{admins:?}");
        });
    }
}
