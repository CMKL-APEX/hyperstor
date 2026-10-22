//! The `hyperstor.v1` cluster API (spec/administration.md §23.5): messages,
//! service traits and clients generated from `proto/`.
//!
//! Messages are `buffa` types; their JSON is the proto3 JSON mapping with
//! the proto field names, so `curl` and `hyperstorctl --json` see the same
//! shapes (decisions.md D51).

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
pub mod proto {
    connectrpc::include_generated!();
}

pub use proto::hyperstor::v1::*;

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
}
