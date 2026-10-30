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
