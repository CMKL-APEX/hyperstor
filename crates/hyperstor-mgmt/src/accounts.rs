//! Accounts in one browser (spec/19-administration.md §23.12, D310, as
//! glidex D71; 01-implementation-notes.md I399).
//!
//! A person may hold a local account and accounts of glidex directories
//! (I389): one session per **directory** in one browser, one of them
//! active. mgmt's own identity provider counts as one ("oidc"), and its
//! passwords and claim as another ("local").
//!
//! - The active session is the session cookie, as before. The others are
//!   in [`COOKIE`]: `HttpOnly`, `SameSite=Strict`, `Secure` over HTTPS,
//!   `Path=/auth/accounts`, so only these endpoints see it, at most
//!   [`MAX`] entries, kept [`DAYS`] days.
//! - mgmt keeps no record of which accounts a browser holds together: it
//!   answers from the cookies a request carries. A session that ended is
//!   dropped from the list; signing in to its directory again adds it.
//! - A login while another session of the same directory is listed
//!   replaces it: that session is ended when the list is next read.

/// The cookie with the browser's other sessions.
pub const COOKIE: &str = "hs_accounts";
/// Where the browser sends it.
pub const PATH: &str = "/auth/accounts";
/// The most sessions it holds besides the active one.
pub const MAX: usize = 5;
/// How long it is kept.
pub const DAYS: u64 = 30;
/// The directory key of a password's or the claim's account.
pub const LOCAL: &str = "local";
/// The directory key of an account of mgmt's own identity provider.
pub const OIDC: &str = "oidc";

/// The session ids in a request's [`COOKIE`], in order, without
/// duplicates or anything that isn't one.
pub fn held(headers: &http::HeaderMap) -> Vec<String> {
    let name = format!("{COOKIE}=");
    let v = headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(&name).map(String::from))
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for id in v.split('.') {
        if is_session_id(id) && !out.iter().any(|x| x == id) {
            out.push(id.to_string());
        }
    }
    out.truncate(MAX);
    out
}

/// Whether `id` has a session cookie's shape: base62, as glidex-auth makes
/// them.
fn is_session_id(id: &str) -> bool {
    (16..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The `Set-Cookie` value holding `ids` (none clears it).
pub fn set(ids: &[String], https: bool) -> String {
    let (value, age) = if ids.is_empty() {
        (String::new(), 0)
    } else {
        (ids.join("."), DAYS * 86_400)
    };
    let mut c = format!("{COOKIE}={value}; HttpOnly; SameSite=Strict; Path={PATH}; Max-Age={age}");
    if https {
        c.push_str("; Secure");
    }
    c
}

/// The directory key of a session logged in by `method`: its directory's
/// trusted id, [`OIDC`] or [`LOCAL`].
pub fn key(dir: Option<&str>, method: &str) -> String {
    match dir {
        Some(d) => d.to_string(),
        None if method == "oidc" => OIDC.into(),
        None => LOCAL.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_holds_session_ids_only() {
        let mut h = http::HeaderMap::new();
        let a = "a".repeat(32);
        let b = "B1".repeat(16);
        h.insert(
            http::header::COOKIE,
            format!("x=1; {COOKIE}={a}.{b}.{a}.bad!.short; y=2")
                .parse()
                .unwrap(),
        );
        assert_eq!(held(&h), vec![a.clone(), b.clone()]);
        let many: Vec<String> = (0..8).map(|i| format!("{i}").repeat(20)).collect();
        h.insert(
            http::header::COOKIE,
            format!("{COOKIE}={}", many.join(".")).parse().unwrap(),
        );
        assert_eq!(held(&h).len(), MAX);
        assert!(held(&http::HeaderMap::new()).is_empty());
        let c = set(&[a.clone(), b.clone()], true);
        assert!(c.starts_with(&format!("{COOKIE}={a}.{b};")), "{c}");
        for part in [
            "HttpOnly",
            "SameSite=Strict",
            "Path=/auth/accounts",
            "Secure",
        ] {
            assert!(c.contains(part), "{c}");
        }
        assert!(set(&[], false).contains("Max-Age=0"));
        assert!(!set(&[], false).contains("Secure"));
        assert_eq!(key(None, "pam"), "local");
        assert_eq!(key(None, "oidc"), "oidc");
        assert_eq!(key(Some("trusted-1"), "directory"), "trusted-1");
    }
}
