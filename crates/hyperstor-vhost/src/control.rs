//! The control socket (decisions.md D117): how glidex, on this node,
//! attaches a volume and gets the socket its hypervisor connects to.
//!
//! One JSON object per line, as glidex's netd and stord take theirs:
//!
//! ```text
//! {"id": 1, "op": "hello"}
//! {"id": 2, "op": "attach", "args": {"volume": "<32 hex digits>", "serial": "…", "take": "attach"|"take_over"|"preempt"}}
//! {"id": 3, "op": "detach", "args": {"volume": "…"}}
//! {"id": 4, "op": "list"}
//! ```
//!
//! Each gets `{"id": N, "ok": {…}}` or `{"id": N, "error": {"code": "…",
//! "message": "…"}}`. Codes: `invalid`, `denied`, `held` (another client
//! holds the lease: preempt, or wait), `not_found`, `not_attached`,
//! `unavailable` (the cluster can't be reached), `internal`.

use hyperstor_client::ClientError;
use serde_json::json;
use std::path::{Path, PathBuf};

pub use serde_json::Value;

/// The protocol's version, which `hello` returns.
pub const PROTOCOL: u64 = 1;

/// How to take a volume's lease (metadata.md §9; D79 for glidex's use of
/// `preempt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Refused while another client holds it.
    Attach,
    /// From a holder that has died or is cut off: waits out its lease.
    TakeOver,
    /// From a holder that may still live: blocklists it first.
    Preempt,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Hello,
    Attach {
        volume: u128,
        serial: String,
        take: Take,
        /// Who asks, recorded with the lease as `vhost:<holder>` (D153):
        /// glidex names its node.
        holder: String,
    },
    Detach {
        volume: u128,
    },
    List,
}

/// The longest serial a disk may have (the virtio-blk id).
pub const SERIAL_MAX: usize = 20;
/// The longest `holder` of an `attach`: `vhost:` and it make a lease label
/// of at most `hyperstor_proto::LABEL_MAX` bytes.
pub const HOLDER_MAX: usize = 58;

/// Parses one request line: its id and the request.
pub fn parse(line: &[u8]) -> Result<(u64, Request), String> {
    let v: Value = serde_json::from_slice(line).map_err(|e| format!("not JSON: {e}"))?;
    let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
    let op = v.get("op").and_then(Value::as_str).ok_or("no \"op\"")?;
    let args = v.get("args").cloned().unwrap_or(json!({}));
    let volume = || -> Result<u128, String> {
        let s = args
            .get("volume")
            .and_then(Value::as_str)
            .ok_or("no \"volume\"")?;
        if s.is_empty() || s.len() > 32 {
            return Err(format!("volume {s:?}: up to 32 hexadecimal digits"));
        }
        u128::from_str_radix(s, 16)
            .map_err(|_| format!("volume {s:?}: up to 32 hexadecimal digits"))
    };
    let req = match op {
        "hello" => Request::Hello,
        "list" => Request::List,
        "detach" => Request::Detach { volume: volume()? },
        "attach" => {
            let serial = args
                .get("serial")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if serial.len() > SERIAL_MAX || !serial.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(format!(
                    "serial {serial:?}: up to {SERIAL_MAX} printable ASCII characters"
                ));
            }
            let take = match args.get("take").and_then(Value::as_str).unwrap_or("attach") {
                "attach" => Take::Attach,
                "take_over" => Take::TakeOver,
                "preempt" => Take::Preempt,
                t => return Err(format!("take {t:?}: attach, take_over or preempt")),
            };
            let holder = args
                .get("holder")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if holder.len() > HOLDER_MAX || !holder.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(format!(
                    "holder {holder:?}: up to {HOLDER_MAX} printable ASCII characters"
                ));
            }
            Request::Attach {
                volume: volume()?,
                serial,
                take,
                holder,
            }
        }
        o => return Err(format!("op {o:?}: hello, attach, detach or list")),
    };
    Ok((id, req))
}

pub struct Reply(Value);

impl Reply {
    pub fn ok(id: u64, v: Value) -> Self {
        Reply(json!({"id": id, "ok": v}))
    }

    pub fn error(id: u64, code: &str, message: &str) -> Self {
        Reply(json!({"id": id, "error": {"code": code, "message": message}}))
    }

    /// An error naming who holds the volume (`held`, D153): the label its
    /// holder took the lease with.
    pub fn held(id: u64, message: &str, holder: &str) -> Self {
        Reply(json!({
            "id": id,
            "error": {"code": "held", "message": message, "holder": holder},
        }))
    }

    pub fn line(&self) -> String {
        self.0.to_string()
    }
}

pub fn hello() -> Value {
    json!({"protocol": PROTOCOL})
}

pub fn attached(socket: &Path, size: u64) -> Value {
    json!({"socket": socket.display().to_string(), "size": size})
}

pub fn detached() -> Value {
    json!({})
}

/// An attached volume, as `list` gives it.
pub struct Listed {
    pub volume: u128,
    pub socket: PathBuf,
    pub size: u64,
    pub connected: bool,
    /// Its lease was taken by a take-over or preemption elsewhere (D150):
    /// its I/O fails until it is detached or attached again.
    pub lost: bool,
}

pub fn list(disks: Vec<Listed>) -> Value {
    let disks: Vec<Value> = disks
        .into_iter()
        .map(|d| {
            json!({
                "volume": format!("{:032x}", d.volume),
                "socket": d.socket.display().to_string(),
                "size": d.size,
                "connected": d.connected,
                "lost": d.lost,
            })
        })
        .collect();
    json!({ "disks": disks })
}

/// The code a failed attach answers with.
pub fn error_code(e: &ClientError) -> &'static str {
    match e {
        ClientError::LeaseHeld { .. } => "held",
        ClientError::NoVolume(_) => "not_found",
        ClientError::Rpc(_) | ClientError::NoPrimary(..) | ClientError::Exhausted(..) => {
            "unavailable"
        }
        _ => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_bad_ones_say_why() {
        let (id, r) = parse(br#"{"id": 7, "op": "attach", "args": {"volume": "ab", "serial": "disk-1", "take": "preempt"}}"#).unwrap();
        assert_eq!(id, 7);
        assert_eq!(
            r,
            Request::Attach {
                volume: 0xab,
                serial: "disk-1".into(),
                take: Take::Preempt,
                holder: String::new(),
            }
        );
        let (_, r) = parse(br#"{"op": "attach", "args": {"volume": "1"}}"#).unwrap();
        assert_eq!(
            r,
            Request::Attach {
                volume: 1,
                serial: String::new(),
                take: Take::Attach,
                holder: String::new(),
            }
        );
        assert_eq!(parse(br#"{"op": "list"}"#).unwrap().1, Request::List);
        assert_eq!(
            parse(br#"{"op": "detach", "args": {"volume": "ff"}}"#)
                .unwrap()
                .1,
            Request::Detach { volume: 0xff }
        );
        for bad in [
            &b"not json"[..],
            br#"{"op": "format"}"#,
            br#"{"op": "attach", "args": {}}"#,
            br#"{"op": "attach", "args": {"volume": "xyz"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "take": "steal"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "serial": "this serial is far too long"}}"#,
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let e = Reply::error(3, "held", "x").line();
        assert!(e.contains(r#""code":"held""#), "{e}");
    }
}
