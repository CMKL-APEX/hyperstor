//! Calls from glidex's UI at another origin (spec/19-administration.md
//! §23.12, 04-decisions.md D310; 01-implementation-notes.md I398).
//!
//! glidex's UI shows a tenant's volumes, buckets, S3 keys and usage per
//! project by calling mgmt's API from the person's browser, with a
//! directory access token addressed to this cluster as a bearer token.
//!
//! - **Who may:** only pages at the origins mgmt has learned
//!   ([`crate::directory::LoginState::ui_origins`]): each trusted
//!   directory's issuer's origin, and the origins an operator named on a
//!   management client's binding (`hyperstorctl realm client --origins`).
//!   Another origin gets no CORS headers, so its page can't read answers,
//!   and its writes are refused (`bad_origin`) as before.
//! - **What:** the cluster API (`POST /api/hyperstor.v1.*`) and
//!   `GET /auth/whoami`; nothing else (`cross_origin_path`).
//! - **How:** a directory's access token only (`Authorization: Bearer`). A
//!   cookie is never read on such a request, so mgmt's own session can't be
//!   driven from another page even on a sibling host; mgmt's `hst_` and
//!   glidex's `gxt_` API tokens are refused (`cross_origin_token`): they
//!   belong in scripts, not in a page.
//! - **Headers:** `Access-Control-Allow-Origin` names the origin (never
//!   `*`), with `Vary: Origin`; no `Access-Control-Allow-Credentials`, so a
//!   browser sends no cookie and reads no answer to a request that tried.
//!   A preflight (`OPTIONS`) is answered `204` with the methods and
//!   headers allowed, kept [`MAX_AGE`] seconds.

use hyper::body::Bytes;

/// Methods a page at another origin may use.
pub const ALLOW_METHODS: &str = "GET, POST";
/// Request headers it may send: the token, Connect's, and the realm acted
/// in (D265).
pub const ALLOW_HEADERS: &str =
    "authorization, content-type, connect-protocol-version, connect-timeout-ms, x-hyperstor-realm";
/// How long a browser keeps a preflight's answer, in seconds.
pub const MAX_AGE: &str = "600";

/// Whether a page at another origin may call `method` `path`.
pub fn path_allowed(method: &http::Method, path: &str) -> bool {
    (method == http::Method::POST && path.starts_with("/api/hyperstor.v1."))
        || (method == http::Method::GET && path == "/auth/whoami")
}

/// Whether bearer token `t` may come from a page at another origin: a
/// directory's access token, not an API token of mgmt's or glidex's.
pub fn bearer_allowed(t: &str) -> bool {
    !t.starts_with(crate::people::TOKEN_PREFIX)
        && !t.starts_with(glidex_trust::tokens::API_TOKEN_PREFIX)
}

/// `o` as compared with the origins learned: lower case, without a
/// default port; `None` if it isn't an HTTPS origin.
pub fn normalize(o: &str) -> Option<String> {
    let rest = o.trim().strip_prefix("https://")?;
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    let rest = rest.to_ascii_lowercase();
    Some(match rest.strip_suffix(":443") {
        Some(h) => format!("https://{h}"),
        None => format!("https://{rest}"),
    })
}

/// Whether `origin` is one of `learned`.
pub fn allowed(origin: &str, learned: &[String]) -> bool {
    normalize(origin).is_some_and(|o| learned.contains(&o))
}

/// Adds the headers that let the page at `origin` read `resp`.
pub fn decorate<B>(resp: &mut http::Response<B>, origin: &str) {
    let h = resp.headers_mut();
    if let Ok(v) = http::HeaderValue::from_str(origin) {
        h.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    h.append(http::header::VARY, http::HeaderValue::from_static("Origin"));
}

/// The answer to a preflight from allowed `origin` for `method` `path`:
/// `204` with what may be sent, or `403` with no CORS headers.
pub fn preflight(
    origin: &str,
    path: &str,
    asked: Option<&str>,
) -> http::Response<http_body_util::Full<Bytes>> {
    let method = asked.and_then(|m| http::Method::from_bytes(m.as_bytes()).ok());
    let ok = method.as_ref().is_some_and(|m| path_allowed(m, path));
    let mut r = http::Response::builder()
        .status(if ok { 204 } else { 403 })
        .header(http::header::VARY, "Origin")
        .header(http::header::CACHE_CONTROL, "no-store")
        .body(http_body_util::Full::new(Bytes::new()))
        .expect("response");
    if ok {
        decorate(&mut r, origin);
        let h = r.headers_mut();
        h.insert(
            http::header::ACCESS_CONTROL_ALLOW_METHODS,
            http::HeaderValue::from_static(ALLOW_METHODS),
        );
        h.insert(
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            http::HeaderValue::from_static(ALLOW_HEADERS),
        );
        h.insert(
            http::header::ACCESS_CONTROL_MAX_AGE,
            http::HeaderValue::from_static(MAX_AGE),
        );
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_compared_as_learned() {
        let learned = vec![
            "https://gx.example.org".to_string(),
            "https://id.example.org:8443".to_string(),
        ];
        for o in [
            "https://gx.example.org",
            "https://GX.example.org",
            "https://gx.example.org:443",
            "https://id.example.org:8443",
        ] {
            assert!(allowed(o, &learned), "{o}");
        }
        for o in [
            "http://gx.example.org",
            "https://gx.example.org.evil.example",
            "https://id.example.org",
            "https://gx.example.org/",
            "null",
            "",
        ] {
            assert!(!allowed(o, &learned), "{o}");
        }
    }

    #[test]
    fn only_the_api_and_whoami_with_a_directory_token() {
        assert!(path_allowed(
            &http::Method::POST,
            "/api/hyperstor.v1.ClusterService/ListVolumes"
        ));
        assert!(path_allowed(&http::Method::GET, "/auth/whoami"));
        for (m, p) in [
            (
                http::Method::GET,
                "/api/hyperstor.v1.ClusterService/ListVolumes",
            ),
            (http::Method::POST, "/auth/logout"),
            (http::Method::POST, "/auth/tokens"),
            (http::Method::GET, "/auth/users"),
            (http::Method::POST, "/auth/accounts/switch"),
            (http::Method::GET, "/"),
        ] {
            assert!(!path_allowed(&m, p), "{m} {p}");
        }
        assert!(bearer_allowed("eyJhbGciOiJFUzI1NiJ9.e30.sig"));
        assert!(!bearer_allowed("hst_abc"));
        assert!(!bearer_allowed("gxt_abc"));
    }

    #[test]
    fn a_preflight_says_what_may_be_sent() {
        let o = "https://gx.example.org";
        let r = preflight(
            o,
            "/api/hyperstor.v1.ClusterService/ListVolumes",
            Some("POST"),
        );
        assert_eq!(r.status(), 204);
        let h = r.headers();
        assert_eq!(h[http::header::ACCESS_CONTROL_ALLOW_ORIGIN], o);
        assert_eq!(h[http::header::VARY], "Origin");
        assert!(
            h[http::header::ACCESS_CONTROL_ALLOW_HEADERS]
                .to_str()
                .unwrap()
                .contains("authorization")
        );
        assert!(
            h.get(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none()
        );
        let r = preflight(o, "/auth/tokens", Some("POST"));
        assert_eq!(r.status(), 403);
        assert!(
            r.headers()
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
        assert_eq!(preflight(o, "/auth/whoami", None).status(), 403);
    }
}
