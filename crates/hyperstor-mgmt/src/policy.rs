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
//!   (D114); `storage-admin` also manages volumes, members and S3
//!   placements; `security-admin` manages people, tokens, S3 keys,
//!   certificates and backups, and reads the audit log, and nothing else.
//! - Step-up: what is hard to undo (deleting volumes, snapshots and
//!   members, rolling a volume back, tokens, S3 keys, revocations, backups, people's
//!   roles) needs a login of the last 10 minutes.
//!   An API token is never fresh (as glidex's personal tokens), so those
//!   calls need a person at a browser, the local socket, or a certificate.
//! - Realms (D252, D260, D262–D265): a grant is a role, cluster-wide, or
//!   `role@realm`, that role in one realm. A grant in a realm reaches only
//!   the calls of [`IN_REALM`], and only that realm's tenants, volumes,
//!   buckets, keys, placements and people's grants: mgmt names the realms
//!   in [`hyperstor_api::REALMS_HEADER`] and the map service applies them.

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
    ("ClusterService.GetScrubStatus", Group::Read, false),
    ("ClusterService.ListPgScrubs", Group::Read, false),
    ("ClusterService.GetPgHeat", Group::Read, false),
    ("ClusterService.ListDevicePerf", Group::Read, false),
    ("ClusterService.ListTopVolumes", Group::Read, false),
    ("ClusterService.ListFindings", Group::Read, false),
    ("ClusterService.ListVolumes", Group::Read, false),
    ("ClusterService.ListSnapshots", Group::Read, false),
    ("TrashService.ListTrash", Group::Read, false),
    ("TrashService.GetTrashSettings", Group::Read, false),
    ("OperationService.GetOperation", Group::Read, false),
    ("OperationService.ListOperations", Group::Read, false),
    ("OperationService.WatchOperation", Group::Read, false),
    ("MemberService.ListMembers", Group::Read, false),
    ("S3Service.ListS3Placements", Group::Read, false),
    ("S3Service.ListS3Buckets", Group::Read, false),
    ("S3Service.GetS3Bucket", Group::Read, false),
    ("RealmService.ListRealms", Group::Read, false),
    ("Dashboard.Read", Group::Read, false),
    ("ClusterService.SetDeviceIn", Group::Operate, false),
    ("ClusterService.SetDeviceWeight", Group::Operate, false),
    ("ClusterService.SetFullRatios", Group::Operate, false),
    ("ClusterService.AddDevice", Group::Operate, false),
    ("ClusterService.SetDeviceClass", Group::Operate, false),
    // One-way (D274): a converted device can't go back to older builds.
    ("ClusterService.UpgradeDevices", Group::Operate, true),
    // A device's endpoints (D215): where its traffic goes.
    ("ClusterService.SetEndpoints", Group::Operate, false),
    ("ClusterService.ListPorts", Group::Read, false),
    ("ClusterService.SetPortRole", Group::Operate, false),
    ("ClusterService.SetLending", Group::Operate, false),
    ("ClusterService.ClaimPort", Group::Machine, false),
    ("ClusterService.RenewClaim", Group::Machine, false),
    ("ClusterService.ReleaseClaim", Group::Operate, false),
    ("ClusterService.ListClaims", Group::Read, false),
    // A device's removal is final once its PGs have moved (D186).
    ("ClusterService.RemoveDevice", Group::Operate, true),
    ("NodeService.DrainNode", Group::Operate, false),
    ("OperationService.CancelOperation", Group::Operate, false),
    // Operators protect data before maintenance and undo a mistaken
    // delete (D114).
    ("ClusterService.CreateSnapshot", Group::Operate, false),
    ("ClusterService.CreateGroupSnapshot", Group::Operate, false),
    ("TrashService.RestoreFromTrash", Group::Operate, false),
    ("ClusterService.CreatePool", Group::Store, false),
    ("ClusterService.SetPool", Group::Store, false),
    // Where new buckets go (D232).
    ("S3Service.SetS3Placement", Group::Store, false),
    ("S3Service.DeleteS3Placement", Group::Store, false),
    // Only an empty bucket (D255).
    ("S3Service.DeleteS3Bucket", Group::Store, false),
    // Realms (D248, D249): which pools a realm's tenants may use.
    ("RealmService.CreateRealm", Group::Store, false),
    ("RealmService.SetPoolRealms", Group::Store, false),
    ("RealmService.DeleteRealm", Group::Store, true),
    // Weakening scrub is a deliberate choice (D226): a fresh login.
    ("ClusterService.SetScrubSettings", Group::Store, false),
    ("ClusterService.WeakenScrubSettings", Group::Store, true),
    ("ClusterService.StartScrub", Group::Store, false),
    ("ClusterService.SetPoolDevice", Group::Store, false),
    ("ClusterService.DeletePool", Group::Store, true),
    ("ClusterService.CreateVolume", Group::Store, false),
    ("ClusterService.ExpandVolume", Group::Store, false),
    ("ClusterService.DeleteVolume", Group::Store, true),
    ("ClusterService.DeleteSnapshot", Group::Store, true),
    ("ClusterService.CloneVolume", Group::Store, false),
    ("ClusterService.RollbackVolume", Group::Store, true),
    ("ClusterService.FlattenVolume", Group::Store, false),
    ("ClusterService.CopyVolume", Group::Store, false),
    ("ClusterService.MoveVolume", Group::Store, false),
    ("ClusterService.DeleteGroupSnapshot", Group::Store, true),
    ("TrashService.PurgeFromTrash", Group::Store, true),
    ("TrashService.SetTrashSettings", Group::Store, true),
    ("MemberService.AddMember", Group::Store, false),
    ("MemberService.RemoveMember", Group::Store, true),
    ("TokenService.CreateToken", Group::Secure, true),
    ("TokenService.ListTokens", Group::Secure, false),
    ("TokenService.RevokeToken", Group::Secure, false),
    // S3's access keys are credentials, as tokens are (D231).
    ("S3Service.CreateS3Key", Group::Secure, true),
    ("S3Service.ListS3Keys", Group::Secure, false),
    ("S3Service.RevokeS3Key", Group::Secure, false),
    // Which gateway takes a tenant's keys (D250): a fresh login.
    ("RealmService.SetTenantRealm", Group::Secure, true),
    // Which volumes a host may attach (D251).
    ("RealmService.SetHostRealms", Group::Secure, true),
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
    // S3's secrets, for gateways alone (D231).
    ("S3Service.GetS3Gateway", Group::Machine, false),
    // The cluster key, for map service members alone (D162).
    ("CaService.GetClusterKey", Group::Machine, false),
];

/// What a grant in a realm reaches (D252, D264, D265): the calls about a
/// realm's tenants, volumes and snapshots, the trash's volumes, buckets,
/// keys, placements and realm, its people's grants, and the operations one
/// started oneself. Everything else
/// (devices, nodes, pools, members, hosts, others' operations, scrub, the audit,
/// certificates, tokens, moving tenants) is the cluster's as a whole.
pub const IN_REALM: &[&str] = &[
    "ClusterService.ListVolumes",
    "ClusterService.ListSnapshots",
    "TrashService.ListTrash",
    "S3Service.ListS3Placements",
    "S3Service.ListS3Buckets",
    "S3Service.GetS3Bucket",
    "RealmService.ListRealms",
    // One's own only: what a copy, flatten or purge started.
    "OperationService.GetOperation",
    "ClusterService.CreateSnapshot",
    "ClusterService.CreateGroupSnapshot",
    "TrashService.RestoreFromTrash",
    "ClusterService.CreateVolume",
    "ClusterService.ExpandVolume",
    "ClusterService.DeleteVolume",
    "ClusterService.DeleteSnapshot",
    "ClusterService.CloneVolume",
    "ClusterService.RollbackVolume",
    "ClusterService.FlattenVolume",
    "ClusterService.CopyVolume",
    "ClusterService.MoveVolume",
    "ClusterService.DeleteGroupSnapshot",
    "TrashService.PurgeFromTrash",
    "S3Service.SetS3Placement",
    "S3Service.DeleteS3Placement",
    "S3Service.DeleteS3Bucket",
    "S3Service.CreateS3Key",
    "S3Service.ListS3Keys",
    "S3Service.RevokeS3Key",
    "People.ListUsers",
    "People.SetRoles",
];

/// A grant's role and realm: `viewer`, cluster-wide, or `viewer@acme`.
/// `None` if the role isn't one or the realm isn't a name (1-63 of a-z,
/// 0-9 and `-`, "default" among them).
pub fn grant(g: &str) -> Option<(&str, Option<&str>)> {
    let (role, realm) = match g.split_once('@') {
        Some((r, realm)) => (r, Some(realm)),
        None => (g, None),
    };
    if !ROLES.contains(&role) {
        return None;
    }
    if let Some(r) = realm
        && hyperstor_api::parse_realms(r).is_none_or(|v| v.len() != 1)
    {
        return None;
    }
    Some((role, realm))
}

/// Where a call reaches (D262).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Cluster,
    /// Only these realms, by name.
    Realms(Vec<String>),
}

/// Where `user` with grants `roles` may do `action`: the cluster, if a
/// cluster-wide role allows it; otherwise the realms in which a grant,
/// with the cluster-wide roles, allows it, if `action` is one of
/// [`IN_REALM`]. With `only` (a person acting in one realm, `--realm`),
/// just that realm, whatever their roles reach, and nothing that isn't in
/// a realm. A refusal is the decision on every grant at once, so a
/// stale login still reads as step-up.
pub fn reach(
    e: &Engine<Hyperstor>,
    user: &str,
    roles: &[String],
    action: &str,
    method: &str,
    age_secs: i64,
    only: Option<&str>,
) -> Result<Reach, Decision> {
    let cluster: Vec<String> = roles.iter().filter(|r| !r.contains('@')).cloned().collect();
    let in_realm = IN_REALM.contains(&action);
    if only.is_none() && allowed(e, user, &cluster, action, method, age_secs).allowed {
        return Ok(Reach::Cluster);
    }
    let mut realms: Vec<&str> = roles
        .iter()
        .filter_map(|g| grant(g).and_then(|(_, r)| r))
        .collect();
    if let Some(o) = only {
        realms = vec![o];
    }
    realms.sort();
    realms.dedup();
    let mut out = Vec::new();
    if in_realm {
        for r in realms {
            let mut with: Vec<String> = cluster.clone();
            with.extend(
                roles
                    .iter()
                    .filter_map(|g| grant(g).filter(|(_, x)| *x == Some(r)))
                    .map(|(role, _)| role.to_string()),
            );
            if allowed(e, user, &with, action, method, age_secs).allowed {
                out.push(r.to_string());
            }
        }
    }
    if !out.is_empty() {
        return Ok(Reach::Realms(out));
    }
    let every: Vec<String> = roles
        .iter()
        .filter_map(|g| grant(g).map(|(role, _)| role.to_string()))
        .collect();
    let mut d = allowed(e, user, &every, action, method, age_secs);
    if d.allowed {
        d.allowed = false;
        d.errors.push(match only {
            Some(r) if in_realm => format!("{user} may not do {action} in realm {r}"),
            Some(r) => format!("{action} isn't done in a realm, and {user} acts in {r}"),
            None => format!("{user} may do {action} only in realms, and it isn't done in one"),
        });
    }
    Err(d)
}

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

    /// Grants in a realm reach the calls of IN_REALM there, with the
    /// cluster-wide roles; `--realm` narrows to one realm; step-up still
    /// holds; nothing outside IN_REALM is reached through a realm.
    #[test]
    fn grants_in_a_realm_reach_only_it() {
        let e = engine();
        let at = |roles: &[&str], action: &str, age: i64, only: Option<&str>| {
            let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
            reach(&e, "ann", &roles, action, "pam", age, only)
        };
        let r = |roles: &[&str], action: &str, only: Option<&str>| at(roles, action, 5, only).ok();
        let realms = |v: &[&str]| Some(Reach::Realms(v.iter().map(|s| s.to_string()).collect()));
        let sa = ["storage-admin@acme"];
        assert_eq!(
            r(&sa, "ClusterService.CreateVolume", None),
            realms(&["acme"])
        );
        assert_eq!(
            r(&sa, "ClusterService.ListVolumes", None),
            realms(&["acme"])
        );
        for cluster_only in [
            "ClusterService.CreatePool",
            "ClusterService.GetStatus",
            "RealmService.SetHostRealms",
            "RealmService.SetTenantRealm",
            "OperationService.ListOperations",
            "Dashboard.Read",
        ] {
            assert_eq!(r(&sa, cluster_only, None), None, "{cluster_only}");
        }
        // A cluster-wide role reaches everything it allows.
        let mixed = ["viewer", "storage-admin@acme"];
        assert_eq!(
            r(&mixed, "ClusterService.ListVolumes", None),
            Some(Reach::Cluster)
        );
        assert_eq!(
            r(&mixed, "ClusterService.CreateVolume", None),
            realms(&["acme"])
        );
        // Each realm its own roles.
        let two = ["storage-admin@acme", "operator@globex"];
        assert_eq!(
            r(&two, "ClusterService.CreateSnapshot", None),
            realms(&["acme", "globex"])
        );
        assert_eq!(
            r(&two, "ClusterService.DeleteVolume", None),
            realms(&["acme"])
        );
        // Security in a realm: keys and its people's grants, fresh.
        let sec = ["security-admin@acme"];
        assert_eq!(r(&sec, "S3Service.CreateS3Key", None), realms(&["acme"]));
        assert_eq!(r(&sec, "People.SetRoles", None), realms(&["acme"]));
        let stale = at(&sec, "People.SetRoles", 601, None).unwrap_err();
        assert!(stale.denied_by("base.step-up"), "{stale:?}");
        assert_eq!(r(&sec, "People.DeleteUser", None), None);
        assert_eq!(r(&sec, "AuditService.ListAudit", None), None);
        assert_eq!(r(&sec, "ClusterService.ListVolumes", None), None);
        // Everyone's own tokens, realm or not.
        assert_eq!(r(&sec, "People.ListTokens", None), Some(Reach::Cluster));
        // Acting in one realm.
        let admin = ["storage-admin"];
        assert_eq!(
            r(&admin, "ClusterService.CreateVolume", Some("acme")),
            realms(&["acme"])
        );
        assert_eq!(r(&admin, "ClusterService.GetStatus", Some("acme")), None);
        assert_eq!(r(&sa, "ClusterService.CreateVolume", Some("globex")), None);
        assert_eq!(
            r(&two, "ClusterService.CreateSnapshot", Some("globex")),
            realms(&["globex"])
        );
        // Bad grants count for nothing.
        assert_eq!(r(&["root@acme"], "ClusterService.ListVolumes", None), None);
    }

    /// What a realm reaches is something people do.
    #[test]
    fn realm_calls_are_people_calls() {
        for a in IN_REALM {
            assert!(
                matches!(action(a), Some((g, _)) if g != Group::Machine),
                "{a}"
            );
        }
        assert_eq!(grant("viewer@acme"), Some(("viewer", Some("acme"))));
        assert_eq!(grant("viewer"), Some(("viewer", None)));
        for bad in ["root", "viewer@", "viewer@a,b", "viewer@A", "@acme"] {
            assert_eq!(grant(bad), None, "{bad}");
        }
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
