//! The control socket (04-decisions.md D117): how glidex, on this node,
//! attaches a volume and gets the socket its hypervisor connects to.
//!
//! One JSON object per line, as glidex's netd and stord take theirs:
//!
//! ```text
//! {"id": 1, "op": "hello"}
//! {"id": 2, "op": "attach", "args": {"volume": "<32 hex digits>", "serial": "…", "take": "attach"|"take_over"|"preempt", "poll": false}}
//! {"id": 3, "op": "detach", "args": {"volume": "…", "force": false}}
//! {"id": 4, "op": "list"}
//! {"id": 5, "op": "delegate", "args": {"volume": "…"}}
//! {"id": 6, "op": "attach", "args": {"volume": "…", "delegate": "<16 hex digits>", "holder": "…"}}
//! {"id": 7, "op": "promote", "args": {"volume": "…"}}
//! {"id": 8, "op": "undelegate", "args": {"volume": "…", "delegate": "…"}}
//! {"id": 9, "op": "attach", "args": {"volume": "…", "for": {"uid": 994, "gid": 984}}}
//! ```
//!
//! A volume's socket is for the user who attached it (D355): its group is
//! theirs, and only they, root and the service connect. Root attaching
//! for another user names them with `for` (`ctl attach --for USER`).
//!
//! A live migration (D350–D352; 11-frontends.md §11.4): glidex asks the
//! source for a `delegate` of the disk's lease, attaches it on the target
//! with that `delegate` (a standby that writes nothing, its socket ready
//! for the receiving hypervisor), and at the switch `promote`s it there,
//! which makes it the lease's holder. A cancelled migration detaches the
//! target and has the source `undelegate` it, which fences it.
//!
//! `hello` answers `{"protocol": N, "features": […], "identity": "…"}`:
//! the identity is the URI in the certificate the service shows storage
//! nodes (`--identity`), or null without one.
//!
//! Each gets `{"id": N, "ok": {…}}` or `{"id": N, "error": {"code": "…",
//! "message": "…"}}`. Codes: `invalid`, `denied`, `held` (another client
//! holds the lease: preempt, or wait), `not_found`, `not_attached`, `busy`
//! (a hypervisor is connected to the disk: stop it, or detach with
//! `force`, D256), `unavailable` (the cluster can't be reached),
//! `internal`.

use hyperstor_client::ClientError;
use serde_json::json;
use std::path::{Path, PathBuf};

pub use serde_json::Value;

/// The protocol's version, which `hello` returns: 2 refuses a detach
/// while a hypervisor is connected (D256); 3 adds `delegate`,
/// `undelegate`, `promote` and `attach` as a delegate, for live
/// migration (D352); 4 gives each socket to the user who attached it and
/// adds `for` to `attach` (D355).
pub const PROTOCOL: u64 = 4;

/// What `hello` lists besides the version: `live_migration`, dirty-page
/// logging (#397) and delegate handover (D350–D352).
pub const FEATURES: &[&str] = &["live_migration"];

/// How to take a volume's lease (06-metadata.md §9; D79 for glidex's use of
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
        /// Poll its rings while busy (D293): for latency-critical VMs.
        poll: bool,
        /// As this delegate of the holder's lease, given by the source's
        /// `delegate` (D350): a live migration's target, writing nothing
        /// until promoted. `take` doesn't apply.
        delegate: Option<u64>,
        /// The user (uid, gid) root attaches it for (D355); None: the one
        /// asking.
        user: Option<(u32, u32)>,
    },
    /// Adds a delegate to the lease of a volume attached here, for a
    /// migration's target (D350): answers its identity.
    Delegate {
        volume: u128,
    },
    /// Removes and fences delegate `delegate` of a volume attached here: a
    /// cancelled migration's target.
    Undelegate {
        volume: u128,
        delegate: u64,
    },
    /// Makes the delegate attached here the volume's holder (D351): a
    /// migration's switch.
    Promote {
        volume: u128,
    },
    Detach {
        volume: u128,
        /// Detach even while a hypervisor is connected (D256).
        force: bool,
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
    let delegate = |required: bool| -> Result<Option<u64>, String> {
        match args.get("delegate") {
            None | Some(Value::Null) if !required => Ok(None),
            None | Some(Value::Null) => Err("no \"delegate\"".into()),
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 16 => u64::from_str_radix(s, 16)
                .ok()
                .filter(|d| *d != 0)
                .map(Some)
                .ok_or_else(|| format!("delegate {s:?}: up to 16 hexadecimal digits, not 0")),
            Some(d) => Err(format!("delegate {d}: up to 16 hexadecimal digits")),
        }
    };
    let req = match op {
        "hello" => Request::Hello,
        "delegate" => Request::Delegate { volume: volume()? },
        "undelegate" => Request::Undelegate {
            volume: volume()?,
            delegate: delegate(true)?.unwrap_or_default(),
        },
        "promote" => Request::Promote { volume: volume()? },
        "list" => Request::List,
        "detach" => Request::Detach {
            volume: volume()?,
            force: match args.get("force") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(f) => return Err(format!("force {f}: true or false")),
            },
        },
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
            let poll = match args.get("poll") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(p) => return Err(format!("poll {p}: true or false")),
            };
            let id = |v: &Value, what: &str| {
                v.get(what)
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| format!("for: no \"{what}\" (a number)"))
            };
            let user = match args.get("for") {
                None | Some(Value::Null) => None,
                Some(f) => Some((id(f, "uid")?, id(f, "gid")?)),
            };
            Request::Attach {
                volume: volume()?,
                serial,
                take,
                holder,
                poll,
                delegate: delegate(false)?,
                user,
            }
        }
        o => {
            return Err(format!(
                "op {o:?}: hello, attach, detach, list, delegate, undelegate or promote"
            ));
        }
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

/// `hello`'s answer. `identity` is who this service's certificate says
/// it is on the storage network (D373), null when it runs without one:
/// glidex warns about such a node before the cluster requires
/// authenticated connections (glidex #665). Only the name: the
/// certificate is public and its key never leaves the service.
pub fn hello(identity: Option<&str>) -> Value {
    json!({"protocol": PROTOCOL, "features": FEATURES, "identity": identity})
}

/// A delegate's identity, as `delegate` answers it.
pub fn delegated(delegate: u64) -> Value {
    json!({"delegate": format!("{delegate:016x}")})
}

/// What `promote` answers: the identities fenced in the background (the
/// source's lease and its other delegates).
pub fn promoted(fenced: &[u64]) -> Value {
    let fenced: Vec<String> = fenced.iter().map(|f| format!("{f:016x}")).collect();
    json!({"fencing": fenced})
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
    /// The reactor that serves it (D291), and that reactor's NUMA node.
    pub reactor: usize,
    pub node: Option<u32>,
    /// Its rings are polled while busy (D293).
    pub poll: bool,
    /// Attached as a delegate waiting to be promoted, a live migration's
    /// target (D350): glidex's clean-up of stale holds leaves it.
    pub delegate: bool,
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
                "reactor": d.reactor,
                "node": d.node,
                "poll": d.poll,
                "role": if d.delegate { "delegate" } else { "holder" },
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
        // An older storage node (D352), or a delegate the holder no
        // longer lists.
        ClientError::Server {
            code: hyperstor_proto::Code::Invalid,
            ..
        }
        | ClientError::OldPeer { .. }
        | ClientError::BadVolume(_) => "invalid",
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
        let (id, r) = parse(br#"{"id": 7, "op": "attach", "args": {"volume": "ab", "serial": "disk-1", "take": "preempt", "poll": true}}"#).unwrap();
        assert_eq!(id, 7);
        assert_eq!(
            r,
            Request::Attach {
                volume: 0xab,
                serial: "disk-1".into(),
                take: Take::Preempt,
                holder: String::new(),
                poll: true,
                delegate: None,
                user: None,
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
                poll: false,
                delegate: None,
                user: None,
            }
        );
        // Root, for another user (D355).
        let (_, r) =
            parse(br#"{"op": "attach", "args": {"volume": "1", "for": {"uid": 994, "gid": 984}}}"#)
                .unwrap();
        assert!(matches!(
            r,
            Request::Attach {
                user: Some((994, 984)),
                ..
            }
        ));
        assert!(
            parse(br#"{"op": "attach", "args": {"volume": "1", "for": {"uid": 994}}}"#)
                .unwrap_err()
                .contains("gid")
        );
        assert!(
            parse(br#"{"op": "attach", "args": {"volume": "1", "for": {"uid": -1, "gid": 0}}}"#)
                .is_err()
        );
        assert_eq!(parse(br#"{"op": "list"}"#).unwrap().1, Request::List);
        // A live migration's ops (D350-D352).
        let (_, r) = parse(
            br#"{"op": "attach", "args": {"volume": "1", "delegate": "00ab", "holder": "n2"}}"#,
        )
        .unwrap();
        assert!(matches!(
            r,
            Request::Attach {
                delegate: Some(0xab),
                ..
            }
        ));
        assert_eq!(
            parse(br#"{"op": "delegate", "args": {"volume": "2"}}"#)
                .unwrap()
                .1,
            Request::Delegate { volume: 2 }
        );
        assert_eq!(
            parse(br#"{"op": "promote", "args": {"volume": "2"}}"#)
                .unwrap()
                .1,
            Request::Promote { volume: 2 }
        );
        assert_eq!(
            parse(br#"{"op": "undelegate", "args": {"volume": "2", "delegate": "ff"}}"#)
                .unwrap()
                .1,
            Request::Undelegate {
                volume: 2,
                delegate: 0xff
            }
        );
        let h = hello(None).to_string();
        assert!(
            h.contains(r#""protocol":4"#) && h.contains("live_migration"),
            "{h}"
        );
        // The field is always there, so that a client tells "none" from
        // a service too old to say.
        assert_eq!(hello(None)["identity"], Value::Null);
        assert_eq!(
            hello(Some("hyperstor://c/host/vm-1"))["identity"],
            "hyperstor://c/host/vm-1"
        );
        assert_eq!(
            parse(br#"{"op": "detach", "args": {"volume": "ff"}}"#)
                .unwrap()
                .1,
            Request::Detach {
                volume: 0xff,
                force: false
            }
        );
        assert_eq!(
            parse(br#"{"op": "detach", "args": {"volume": "ff", "force": true}}"#)
                .unwrap()
                .1,
            Request::Detach {
                volume: 0xff,
                force: true
            }
        );
        for bad in [
            &b"not json"[..],
            br#"{"op": "format"}"#,
            br#"{"op": "attach", "args": {}}"#,
            br#"{"op": "attach", "args": {"volume": "xyz"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "take": "steal"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "poll": "yes"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "serial": "this serial is far too long"}}"#,
            br#"{"op": "detach", "args": {"volume": "1", "force": "yes"}}"#,
            br#"{"op": "undelegate", "args": {"volume": "1"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "delegate": "0"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "delegate": "not hex"}}"#,
            br#"{"op": "attach", "args": {"volume": "1", "delegate": 5}}"#,
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let e = Reply::error(3, "held", "x").line();
        assert!(e.contains(r#""code":"held""#), "{e}");
    }
}
