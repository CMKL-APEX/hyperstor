//! The `hyperstor.v1` cluster API (spec/19-administration.md §23.5): messages,
//! service traits and clients generated from `proto/`.
//!
//! Messages are `buffa` types; their JSON is the proto3 JSON mapping with
//! the proto field names, so `curl` and `hyperstorctl --json` see the same
//! shapes (04-decisions.md D51).

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
pub mod proto {
    connectrpc::include_generated!();
}

pub use proto::hyperstor::v1::*;

/// The header in which `hyperstor-mgmt` names the person a call is for
/// (`user:NAME`). The map service believes it only from mgmt's
/// certificate, and then audits and rate-limits the call as
/// `<mgmt's identity> for user:NAME` (01-implementation-notes.md I135).
pub const ON_BEHALF_HEADER: &str = "x-hyperstor-on-behalf-of";

/// Whether `p` is a person as mgmt names them: `user:` and a user name
/// (letters, digits, `.`, `_`, `-`, `@`; 1 to 64 bytes).
pub fn valid_person(p: &str) -> bool {
    p.strip_prefix("user:").is_some_and(|n| {
        !n.is_empty()
            && n.len() <= 64
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
    })
}

/// The header with which `hyperstor-mgmt` limits a call it makes on a
/// person's behalf to realms, their names comma-separated (04-decisions.md
/// D260, D262): the person holds the role the call needs only in those.
/// The map service believes it only with [`ON_BEHALF_HEADER`] from mgmt's
/// certificate, then shows and changes only what is in those realms, and
/// refuses what reaches the cluster as a whole.
pub const REALMS_HEADER: &str = "x-hyperstor-realms";

/// The header with which a person asks `hyperstor-mgmt` to act in one
/// realm (`hyperstorctl --realm`, the UI's realm picker; D265): mgmt then
/// uses their cluster-wide roles and their grants in that realm, for
/// that realm alone.
pub const ACT_IN_REALM_HEADER: &str = "x-hyperstor-realm";

/// The realms of a [`REALMS_HEADER`]: one or more names, each 1-63 of
/// a-z, 0-9 and `-` ("default" among them); `None` if it isn't one.
pub fn parse_realms(v: &str) -> Option<Vec<String>> {
    let names: Vec<String> = v.split(',').map(str::to_string).collect();
    names
        .iter()
        .all(|n| {
            !n.is_empty()
                && n.len() <= 63
                && n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .then_some(names)
}

/// The domain of the API's error reasons.
pub const REASON_DOMAIN: &str = "hyperstor";

/// `e` with a stable reason attached as an [`ErrorInfo`] detail (glidex
/// grpc.md G-D5): the same on every transport, for scripts.
pub fn with_reason(e: connectrpc::ConnectError, reason: &str) -> connectrpc::ConnectError {
    let info = ErrorInfo {
        reason: reason.to_string(),
        domain: REASON_DOMAIN.to_string(),
        ..Default::default()
    };
    e.with_detail(connectrpc::ErrorDetail::from_message(
        "hyperstor.v1.ErrorInfo",
        &info,
    ))
}

/// The reason attached to `e` by [`with_reason`], if any.
pub fn reason_of(e: &connectrpc::ConnectError) -> Option<String> {
    use buffa::Message;
    e.details.iter().find_map(|d| {
        if d.type_url.trim_start_matches("type.googleapis.com/") != "hyperstor.v1.ErrorInfo" {
            return None;
        }
        let bytes = base64(d.value.as_deref()?)?;
        ErrorInfo::decode_from_slice(&bytes).ok().map(|i| i.reason)
    })
}

/// Standard base64, padded or not, as the Connect protocol sends details.
fn base64(s: &str) -> Option<Vec<u8>> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = A.iter().position(|x| *x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reasons_survive_their_encoding() {
        let e = super::with_reason(connectrpc::ConnectError::not_found("x"), "token_used");
        assert_eq!(super::reason_of(&e).as_deref(), Some("token_used"));
        // As a client decodes it from the Connect JSON form.
        let back: connectrpc::ConnectError = serde_json::from_slice(&e.to_json()).expect("json");
        assert_eq!(super::reason_of(&back).as_deref(), Some("token_used"));
        assert_eq!(
            super::reason_of(&connectrpc::ConnectError::not_found("x")),
            None
        );
    }

    #[test]
    fn realms_are_names_comma_separated() {
        assert_eq!(
            super::parse_realms("acme,default"),
            Some(vec!["acme".to_string(), "default".to_string()])
        );
        for bad in [
            "",
            "acme,",
            ",acme",
            "Acme",
            "a b",
            "acme;x",
            &"a".repeat(64),
        ] {
            assert_eq!(super::parse_realms(bad), None, "{bad}");
        }
    }

    #[test]
    fn persons_are_user_and_a_name() {
        for ok in ["user:ann", "user:ann@example.com", "user:a.b-c_d"] {
            assert!(super::valid_person(ok), "{ok}");
        }
        for bad in [
            "ann",
            "user:",
            "user:a b",
            "user:a\nb",
            "mgmt:x",
            "user:\u{e9}",
        ] {
            assert!(!super::valid_person(bad), "{bad}");
        }
        assert!(!super::valid_person(&format!("user:{}", "a".repeat(65))));
    }
}
