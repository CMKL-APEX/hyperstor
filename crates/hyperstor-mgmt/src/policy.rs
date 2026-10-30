//! Who may do what (spec/19-administration.md §23.9.2): Cedar, evaluated
//! here, deny by default, through `glidex-auth`'s engine (04-decisions.md
//! D96).
//!
//! - Every call a person makes is an action: the cluster API's RPCs, as
//!   `Service.Method`, and mgmt's own (`People.*`, `Dashboard.Read`). Each
//!   is in the group of the role that allows it ([`ACTIONS`]); a test
//!   fails when an RPC of the protos has no row.
//! - Roles: `viewer` reads; `operator` also drains, takes devices out and
//!   in, cancels operations, takes snapshots and restores from the trash
//!   (D114); `storage-admin` also manages volumes and
//!   members; `security-admin` manages people, tokens, certificates and
//!   backups, and reads the audit log, and nothing else.
//! - Step-up: what is hard to undo (deleting volumes, snapshots and
//!   members, rolling a volume back, tokens, revocations, backups, people's
//!   roles) needs a login of the last 10 minutes.
//!   An API token is never fresh (as glidex's personal tokens), so those
//!   calls need a person at a browser, the local socket, or a certificate.

use glidex_auth::cedar::{AuthContext, Decision, Engine, Model, PolicySource, Query};
use serde_json::{Value, json};
use std::fmt;

/// The roles there are.
pub const ROLES: [&str; 4] = ["viewer", "operator", "storage-admin", "security-admin"];

/// The group of an action: the role that allows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Read,
    Operate,
    Store,
    Secure,
    /// Everyone's own tokens.
    Own,
    /// Machines only: never through mgmt (joins, renewals, mgmt's rows).
    Machine,
}

/// Every action: its name, its group, and whether it needs step-up.
pub const ACTIONS: &[(&str, Group, bool)] = &[
    ("ClusterService.GetStatus", Group::Read, false),
    ("ClusterService.ListPgs", Group::Read, false),
    ("ClusterService.GetPgGrid", Group::Read, false),
    ("ClusterService.ListVolumes", Group::Read, false),
    ("ClusterService.ListSnapshots", Group::Read, false),
    ("TrashService.ListTrash", Group::Read, false),
    ("TrashService.GetTrashSettings", Group::Read, false),
    ("OperationService.GetOperation", Group::Read, false),
    ("OperationService.ListOperations", Group::Read, false),
    ("OperationService.WatchOperation", Group::Read, false),
    ("MemberService.ListMembers", Group::Read, false),
    ("Dashboard.Read", Group::Read, false),
    ("ClusterService.SetDeviceIn", Group::Operate, false),
    ("NodeService.DrainNode", Group::Operate, false),
    ("OperationService.CancelOperation", Group::Operate, false),
    // Operators protect data before maintenance and undo a mistaken
    // delete (D114).
    ("ClusterService.CreateSnapshot", Group::Operate, false),
    ("ClusterService.CreateGroupSnapshot", Group::Operate, false),
    ("TrashService.RestoreFromTrash", Group::Operate, false),
    ("ClusterService.CreateVolume", Group::Store, false),
    ("ClusterService.ExpandVolume", Group::Store, false),
    ("ClusterService.DeleteVolume", Group::Store, true),
    ("ClusterService.DeleteSnapshot", Group::Store, true),
    ("ClusterService.CloneVolume", Group::Store, false),
    ("ClusterService.RollbackVolume", Group::Store, true),
    ("ClusterService.FlattenVolume", Group::Store, false),
    ("ClusterService.DeleteGroupSnapshot", Group::Store, true),
    ("TrashService.PurgeFromTrash", Group::Store, true),
    ("TrashService.SetTrashSettings", Group::Store, true),
    ("MemberService.AddMember", Group::Store, false),
    ("MemberService.RemoveMember", Group::Store, true),
    ("TokenService.CreateToken", Group::Secure, true),
    ("TokenService.ListTokens", Group::Secure, false),
    ("TokenService.RevokeToken", Group::Secure, false),
    ("CaService.ListCertificates", Group::Secure, false),
    ("CaService.RevokeCertificate", Group::Secure, true),
    ("MapService.GetMapBackup", Group::Secure, true),
    ("AuditService.ListAudit", Group::Secure, false),
    ("AuditService.GetAuditSettings", Group::Secure, false),
    ("AuditService.SetAuditSettings", Group::Secure, true),
    ("People.ListUsers", Group::Secure, false),
    ("People.SetRoles", Group::Secure, true),
    ("People.DeleteUser", Group::Secure, true),
    ("People.CreateToken", Group::Own, true),
    ("People.ListTokens", Group::Own, false),
    ("People.RevokeToken", Group::Own, false),
    ("TokenService.Join", Group::Machine, false),
    ("CaService.RenewCertificate", Group::Machine, false),
    ("AuditService.AppendAudit", Group::Machine, false),
    ("MgmtStoreService.GetRow", Group::Machine, false),
    ("MgmtStoreService.PutRow", Group::Machine, false),
    ("MgmtStoreService.DeleteRow", Group::Machine, false),
    ("MgmtStoreService.ListRows", Group::Machine, false),
];

/// The row of `action`, if it is one.
pub fn action(name: &str) -> Option<(Group, bool)> {
    ACTIONS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(_, g, s)| (*g, *s))
}

const SCHEMA: &str = include_str!("../policies/hyperstor.cedarschema");
const BASE: &str = include_str!("../policies/base.cedar");

#[derive(Debug, Clone, PartialEq)]
pub enum Ent {
    User(String),
    Cluster,
}

impl fmt::Display for Ent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ent::User(u) => write!(f, "Hyperstor::User::{u:?}"),
            Ent::Cluster => write!(f, "Hyperstor::Cluster::\"cluster\""),
        }
    }
}

impl Ent {
    fn parts(&self) -> (&'static str, &str) {
        match self {
            Ent::User(u) => ("Hyperstor::User", u),
            Ent::Cluster => ("Hyperstor::Cluster", "cluster"),
        }
    }
}

/// A query's entities: the person and their roles, and the cluster.
#[derive(Debug, Clone)]
pub struct Ents {
    pub user: String,
    pub roles: Vec<String>,
}

pub struct Hyperstor;

impl Model for Hyperstor {
    type Ent = Ent;
    type Entities = Ents;
    const NAMESPACE: &'static str = "Hyperstor";
    const SCHEMA: &'static str = SCHEMA;
    const POLICIES: &'static [(&'static str, PolicySource)] = &[(BASE, PolicySource::Base)];

    fn uid(e: &Ent) -> cedar_policy::EntityUid {
        let (ty, id) = e.parts();
        cedar_policy::EntityUid::from_type_name_and_id(
            ty.parse().expect("type name"),
            cedar_policy::EntityId::new(id),
        )
    }

    fn json_ref(e: &Ent) -> Value {
        let (ty, id) = e.parts();
        json!({ "__entity": { "type": ty, "id": id } })
    }

    fn entities_json(es: &Ents) -> Value {
        json!([
            { "uid": { "type": "Hyperstor::User", "id": es.user },
              "attrs": { "roles": es.roles }, "parents": [] },
            { "uid": { "type": "Hyperstor::Cluster", "id": "cluster" },
              "attrs": {}, "parents": [] },
        ])
    }

    /// No break-glass through mgmt: that is the local socket (§23.6).
    fn is_break_glass(_: &Ents, _: &Ent) -> bool {
        false
    }
}

/// The engine with hyperstor's schema and shipped policies.
pub fn engine() -> Engine<Hyperstor> {
    let e = Engine::<Hyperstor>::new().expect("the shipped policies are valid");
    e.install(&[], &[]).expect("the shipped policies are valid");
    e
}

/// Whether `user` with `roles`, authenticated `age_secs` ago by `method`,
/// may do `action`. Machine-only and unknown actions are refused.
pub fn allowed(
    e: &Engine<Hyperstor>,
    user: &str,
    roles: &[String],
    action: &str,
    method: &str,
    age_secs: i64,
) -> Decision {
    if !matches!(self::action(action), Some((g, _)) if g != Group::Machine) {
        return Decision {
            allowed: false,
            policies: Vec::new(),
            errors: vec![format!("{action} isn't something people do")],
        };
    }
    let q = Query::<Hyperstor>::new(
        Ent::User(user.to_string()),
        action,
        Ent::Cluster,
        AuthContext {
            method: method.into(),
            transport: "https".into(),
            age_secs,
            mfa: false,
            source_ip: None,
        },
        Ents {
            user: user.to_string(),
            roles: roles.to_vec(),
        },
    );
    e.check(&q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn can(e: &Engine<Hyperstor>, roles: &[&str], action: &str, age: i64) -> bool {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        allowed(e, "ann", &roles, action, "pam", age).allowed
    }

    /// Each role gets its group, the roles up to storage-admin add up,
    /// security stays apart, and no one gets what is machines' only.
    #[test]
    fn roles_allow_their_groups() {
        let e = engine();
        for (name, group, _) in ACTIONS {
            let want = |roles: &[&str]| match group {
                Group::Read => roles
                    .iter()
                    .any(|r| matches!(*r, "viewer" | "operator" | "storage-admin")),
                Group::Operate => roles
                    .iter()
                    .any(|r| matches!(*r, "operator" | "storage-admin")),
                Group::Store => roles.contains(&"storage-admin"),
                Group::Secure => roles.contains(&"security-admin"),
                Group::Own => true,
                Group::Machine => false,
            };
            for roles in [
                &[][..],
                &["viewer"],
                &["operator"],
                &["storage-admin"],
                &["security-admin"],
                &["viewer", "security-admin"],
            ] {
                assert_eq!(can(&e, roles, name, 5), want(roles), "{name} for {roles:?}");
            }
        }
        assert!(!can(&e, &["storage-admin"], "Nothing.AtAll", 5));
    }

    /// Step-up: a stale login (or a token, never fresh) can't do what is
    /// hard to undo, and can still do the rest.
    #[test]
    fn step_up_needs_a_fresh_login() {
        let e = engine();
        let admin = ["storage-admin", "security-admin"];
        for (name, group, step_up) in ACTIONS {
            if *group == Group::Machine {
                continue;
            }
            assert!(can(&e, &admin, name, 599), "{name} fresh");
            assert_eq!(can(&e, &admin, name, 601), !step_up, "{name} stale");
            assert_eq!(
                can(&e, &admin, name, glidex_auth::session::NEVER_FRESH),
                !step_up,
                "{name} by token"
            );
        }
    }

    /// The table, the schema and the protos agree: every RPC has a row,
    /// every row but mgmt's own is an RPC, and every action people do is
    /// in the schema with its group.
    #[test]
    fn every_rpc_has_an_action() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../hyperstor-api/proto/hyperstor/v1"
        );
        let mut rpcs = Vec::new();
        for f in std::fs::read_dir(dir).unwrap() {
            let p = f.unwrap().path();
            if p.extension().is_some_and(|x| x == "proto") {
                let text = std::fs::read_to_string(&p).unwrap();
                let mut service = "";
                for line in text.lines().map(str::trim) {
                    if let Some(r) = line.strip_prefix("service ") {
                        service = r.split_whitespace().next().unwrap();
                    } else if let Some(r) = line.strip_prefix("rpc ") {
                        rpcs.push(format!("{service}.{}", r.split('(').next().unwrap().trim()));
                    }
                }
            }
        }
        for r in &rpcs {
            assert!(
                action(r).is_some(),
                "{r}: add it to policy::ACTIONS and the schema"
            );
        }
        for (name, group, step_up) in ACTIONS {
            let own = name.starts_with("People.") || name.starts_with("Dashboard.");
            assert!(
                own || rpcs.contains(&name.to_string()),
                "{name} isn't an RPC"
            );
            if *group == Group::Machine {
                assert!(
                    !SCHEMA.contains(&format!("\"{name}\"")),
                    "{name}: machines only"
                );
                continue;
            }
            let g = match group {
                Group::Read => "read",
                Group::Operate => "operate",
                Group::Store => "store",
                Group::Secure => "secure",
                Group::Own => "own",
                Group::Machine => unreachable!(),
            };
            let line = SCHEMA
                .lines()
                .find(|l| l.contains(&format!("action \"{name}\" ")))
                .unwrap_or_else(|| panic!("{name} isn't in the schema"));
            assert!(line.contains(&format!("Action::\"{g}\"")), "{name}: {line}");
            assert_eq!(
                line.contains("Action::\"step-up\""),
                *step_up,
                "{name}: {line}"
            );
        }
    }
}
