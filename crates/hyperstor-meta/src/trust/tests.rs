use super::*;
use crate::tenancy::{self, NewTenant};
use std::sync::Arc;

const ISSUER: &str = "https://id.acme.example/d/acme";

fn db() -> (tempfile::TempDir, Arc<Db>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(dir.path().join("map.redb")).unwrap());
    (dir, db)
}

fn snapshot(rows: Vec<Row>) -> glidex_trust::Snapshot {
    let jwks = serde_json::json!({ "keys": [] });
    glidex_trust::Snapshot {
        revision: glidex_trust::revision_of(&jwks, &rows, &[]),
        jwks,
        rows,
        ended_sids: Vec::new(),
    }
}

fn row(table: &str, key: &str, value: serde_json::Value) -> Row {
    Row {
        table: table.into(),
        key: key.into(),
        value,
    }
}

fn dir_rec(tenants: &[(&str, &str)], realm: u32) -> DirRec {
    DirRec {
        dir: TrustedDirectory {
            id: glidex_trust::trusted_id(ISSUER),
            issuer: ISSUER.into(),
            directory: "acme".into(),
            api: "https://id.acme.example".into(),
            tls_sha256: "ab".repeat(32),
            jwks: serde_json::json!({ "keys": [] }),
            tenants: tenants
                .iter()
                .map(|(id, slug)| RemoteTenant {
                    id: id.to_string(),
                    slug: slug.to_string(),
                    display_name: String::new(),
                })
                .collect(),
            created_at: 1,
            max_staleness_minutes: None,
        },
        realm,
    }
}

#[test]
fn offers_are_spent_once_and_revoked_only_while_open() {
    let (_d, db) = db();
    let o = OfferRec {
        id: "o1".into(),
        tenant: "acme".into(),
        directory: ISSUER.into(),
        name: "acme".into(),
        realm: "acme".into(),
        url: "https://north.example:8852".into(),
        pin: "ab".repeat(32),
        secret_hash: glidex_trust::sha256_hex(b"s"),
        created_unix: 10,
        created_by: "hyperstor://c/admin/a".into(),
        expires_unix: 100,
        state: OPEN.into(),
    };
    create_offer(&db, &o).unwrap();
    assert!(create_offer(&db, &o).is_err());
    // Expired: listed so, and not spent.
    assert_eq!(offers(&db).unwrap()[0].shown_state(100), "expired");
    assert!(!spend_offer(&db, "o1", 100).unwrap().unwrap().open_at(100));
    assert_eq!(offers(&db).unwrap()[0].state, OPEN);
    // Spent once: the second redeem finds it closed.
    assert!(spend_offer(&db, "o1", 50).unwrap().unwrap().open_at(50));
    assert!(!spend_offer(&db, "o1", 50).unwrap().unwrap().open_at(50));
    assert!(spend_offer(&db, "o2", 50).unwrap().is_none());
    assert!(matches!(
        revoke_offer(&db, "o1"),
        Err(Error::Refused {
            reason: "offer_closed",
            ..
        })
    ));
    create_offer(
        &db,
        &OfferRec {
            id: "o3".into(),
            ..o.clone()
        },
    )
    .unwrap();
    revoke_offer(&db, "o3").unwrap();
    assert!(!spend_offer(&db, "o3", 50).unwrap().unwrap().open_at(50));
    assert!(matches!(revoke_offer(&db, "o4"), Err(Error::NotFound)));
}

#[test]
fn the_cluster_key_is_kept_once() {
    let (_d, db) = db();
    assert_eq!(key(&db).unwrap(), None);
    let a = KeyRec {
        kid: "c:1".into(),
        sealed: vec![1, 2],
    };
    let b = KeyRec {
        kid: "c:2".into(),
        sealed: vec![3],
    };
    assert_eq!(put_key(&db, &a).unwrap(), a);
    // A second made meanwhile (another leader's first use) loses.
    assert_eq!(put_key(&db, &b).unwrap(), a);
    assert_eq!(key(&db).unwrap(), Some(a));
}

#[test]
fn a_feed_keeps_rows_of_admitted_tenants_and_ends_drop_them() {
    let (_d, db) = db();
    let d = dir_rec(&[("t1", "acme"), ("t2", "lab")], 1);
    let id = d.dir.id.clone();
    // Not trusted: nothing applied.
    assert!(!apply_feed(&db, &id, None, 5).unwrap());
    put_directory(&db, &d).unwrap();
    let snap = snapshot(vec![
        row(
            "tenants",
            "t1",
            serde_json::json!({ "id": "t1", "slug": "acme" }),
        ),
        row(
            "tenants",
            "t9",
            serde_json::json!({ "id": "t9", "slug": "other" }),
        ),
        row(
            "projects",
            "p1",
            serde_json::json!({ "id": "p1", "name": "web", "tenant_id": "t1" }),
        ),
        row(
            "projects",
            "p9",
            serde_json::json!({ "id": "p9", "name": "x", "tenant_id": "t9" }),
        ),
        row("users", "u1", serde_json::json!({ "id": "u1" })),
        row("secrets", "s1", serde_json::json!({})),
    ]);
    assert!(apply_feed(&db, &id, Some(&snap), 7).unwrap());
    let (tenants, projects) = feed_records(&db, &id).unwrap();
    assert_eq!(tenants.len(), 1, "{tenants:?}");
    assert_eq!(projects.len(), 1, "{projects:?}");
    assert_eq!(replica(&db, &id, "users").unwrap().len(), 1);
    assert!(replica(&db, &id, "secrets").unwrap().is_empty());
    let f = feed(&db, &id).unwrap();
    assert_eq!((f.state.applied_at, f.state.checked_at), (7, 7));
    assert_eq!(f.state.revision, snap.revision);
    // Unchanged: only when it answered.
    apply_feed(&db, &id, None, 9).unwrap();
    let f = feed(&db, &id).unwrap();
    assert_eq!((f.state.applied_at, f.state.checked_at), (7, 9));
    // A row gone from the feed goes from the replica.
    let less = snapshot(vec![row(
        "tenants",
        "t1",
        serde_json::json!({ "id": "t1", "slug": "acme" }),
    )]);
    apply_feed(&db, &id, Some(&less), 11).unwrap();
    assert!(feed_records(&db, &id).unwrap().1.is_empty());
    assert!(replica(&db, &id, "users").unwrap().is_empty());
    set_blocked(&db, &id, &["x".into()]).unwrap();
    assert_eq!(feed(&db, &id).unwrap().blocked, vec!["x".to_string()]);

    // Ending one tenant, told; the last, not: the directory's rows go.
    assert!(end_tenant(&db, &id, "t9", true).unwrap().is_none());
    assert!(end_tenant(&db, &id, "t2", true).unwrap().is_some());
    assert_eq!(directories(&db).unwrap()[0].dir.tenants.len(), 1);
    assert_eq!(endings(&db).unwrap().len(), 1);
    assert!(end_tenant(&db, &id, "t1", false).unwrap().is_some());
    assert!(directories(&db).unwrap().is_empty());
    ending_told(&db, &id, "t2").unwrap();
    assert!(all_rows(&db).is_empty(), "{:?}", all_rows(&db));
}

#[test]
fn names_from_glidex_are_names_here() {
    assert_eq!(name_of("Web App"), "web-app");
    assert_eq!(name_of("--A__b--"), "a-b");
    assert_eq!(name_of("default"), "default-glidex");
    assert_eq!(name_of("ÅÄ"), "x");
    assert_eq!(name_of(&"a".repeat(70)).len(), 63);
    assert!(crate::tenancy::valid_tenant_name(&name_of("Ünïcode ok 1")));
    assert_eq!(free_name("web", |n| n == "web" || n == "web-2"), "web-3");
    let long = "a".repeat(63);
    assert_eq!(free_name(&long, |n| n == long).len(), 63);
}

/// A tiny random source, so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[self.below(of.len() as u64) as usize]
    }
}

/// What the test's world holds besides the records: projects that hold
/// containers (dropped, they stay unlinked), and moves started whose
/// operations haven't ended (a failover in between).
#[derive(Default)]
struct World {
    holding: BTreeSet<u32>,
    moving: Vec<(u32, u32)>,
}

/// Runs `steps` as the API does, but with moves as their records' writes
/// (no buckets here), stopping after `stop` of them (a failover);
/// refusals are what the API would report as blocked.
fn execute(db: &Db, w: &mut World, realm: u32, steps: &[Step], stop: usize) -> usize {
    let mut done = 0;
    for s in steps.iter().take(stop) {
        let r = match s {
            Step::MakeTenant { glidex_id, name } => tenancy::create_tenant(
                db,
                &NewTenant {
                    name: name.clone(),
                    directory: ISSUER.into(),
                    glidex_id: glidex_id.clone(),
                    created_unix: 1,
                    created_by: "trust".into(),
                    ..Default::default()
                },
                &[],
            )
            .and_then(|(id, _)| crate::realms::set_tenant(db, id, realm, &[])),
            Step::RenameTenant { id, name } => tenancy::rename_tenant(db, *id, name),
            Step::TenantRealm { id } => crate::realms::set_tenant(db, *id, realm, &[]),
            Step::MakeProject {
                tenant,
                glidex_id,
                name,
            } => tenancy::create_project(db, *tenant, name, glidex_id, 1, "trust").map(|_| ()),
            Step::RenameProject { id, name } => tenancy::rename_project(db, *id, name),
            Step::Move {
                project,
                to,
                rename,
            } => rename
                .as_ref()
                .map_or(Ok(()), |n| tenancy::rename_project(db, *project, n))
                .and_then(|_| tenancy::start_move(db, *project, *to))
                .map(|_| w.moving.push((*project, *to))),
            Step::Drop { id } => {
                if w.holding.contains(id) {
                    tenancy::set_project_link(db, *id, "")
                } else {
                    tenancy::delete_project(db, *id, &[])
                }
            }
            Step::UnlinkProject { id } => tenancy::set_project_link(db, *id, ""),
            Step::Unlink { id } => {
                let t = tenancy::load(db).unwrap();
                let mut r = tenancy::set_tenant_link(db, *id, "", "");
                for p in t.projects.iter().filter(|p| p.tenant == *id) {
                    if r.is_ok() && !p.glidex_id.is_empty() {
                        r = tenancy::set_project_link(db, p.id, "");
                    }
                }
                r
            }
        };
        if r.is_ok() {
            done += 1;
        }
    }
    done
}

/// One run of the feed: plan, then execute, as the API does, until a pass
/// changes nothing; stopped after `stop` steps in all.
fn run(
    db: &Db,
    w: &mut World,
    realm: u32,
    admitted: &[RemoteTenant],
    feed: &(Vec<FeedTenant>, Vec<FeedProject>),
    mut stop: usize,
) {
    for _ in 0..4 {
        let now = tenancy::load(db).unwrap();
        let realms = crate::realms::load(db).unwrap();
        let steps = plan(ISSUER, admitted, &feed.0, &feed.1, &now, |t| {
            realms.realm_of(t) == realm
        });
        if steps.is_empty() || stop == 0 {
            return;
        }
        let n = steps.len().min(stop);
        stop -= n;
        if execute(db, w, realm, &steps, n) == 0 {
            return;
        }
    }
}

/// Moves whose operations end now: their records' last write.
fn finish_moves(db: &Db, w: &mut World, rng: &mut Rng, all: bool) {
    let mut left = Vec::new();
    for (p, to) in std::mem::take(&mut w.moving) {
        if all || rng.chance(50) {
            tenancy::finish_move(db, p, to).unwrap();
        } else {
            left.push((p, to));
        }
    }
    w.moving = left;
}

#[test]
fn feeds_converge_on_the_records_through_failovers() {
    const SLUGS: [&str; 4] = ["acme", "local", "lab", "Lab"];
    const NAMES: [&str; 6] = ["web", "api", "default", "Web App", "web-2", "WEB"];
    const GLIDEX_TENANTS: [&str; 3] = ["t1", "t2", "t3"];
    const GLIDEX_PROJECTS: [&str; 7] = ["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let (_d, db) = db();
        let realm = crate::realms::create(
            &db,
            &crate::realms::Realm {
                id: 0,
                name: "acme".into(),
                description: String::new(),
                created_unix: 1,
                created_by: "test".into(),
            },
        )
        .unwrap();
        // Local records, which the feed never changes: a tenant whose
        // name a glidex tenant would take, and its projects.
        let (local, _) = tenancy::create_tenant(
            &db,
            &NewTenant {
                name: "local".into(),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        tenancy::create_project(&db, local, "web", "", 1, "op").unwrap();
        let mut w = World::default();
        let mut slugs: BTreeMap<&str, &str> = BTreeMap::new();
        let mut projects: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
        let mut admitted: BTreeSet<&str> = BTreeSet::new();
        // Local projects made in linked tenants by hand: never changed.
        let mut by_hand: Vec<(u32, u32, String)> = Vec::new();
        for _round in 0..12 {
            // The directory changes.
            for _ in 0..rng.below(4) {
                match rng.below(6) {
                    0 => {
                        let t = *rng.pick(&GLIDEX_TENANTS);
                        slugs.insert(t, rng.pick(&SLUGS));
                        admitted.insert(t);
                    }
                    1 => {
                        // An end: the tenant is no longer admitted.
                        let t = *rng.pick(&GLIDEX_TENANTS);
                        admitted.remove(t);
                    }
                    2 | 3 => {
                        let p = *rng.pick(&GLIDEX_PROJECTS);
                        projects.insert(p, (rng.pick(&GLIDEX_TENANTS), rng.pick(&NAMES)));
                    }
                    4 => {
                        let p = *rng.pick(&GLIDEX_PROJECTS);
                        projects.remove(p);
                    }
                    _ => {
                        if let Some(t) = admitted.iter().next().copied() {
                            slugs.insert(t, rng.pick(&SLUGS));
                        }
                    }
                }
            }
            // Containers in some projects; a project made by hand.
            let now = tenancy::load(&db).unwrap();
            if let Some(p) = now
                .projects
                .get(rng.below(now.projects.len() as u64 + 1) as usize)
                && rng.chance(30)
            {
                w.holding.insert(p.id);
            }
            if rng.chance(20)
                && let Some(t) = now.tenants.iter().find(|t| t.directory == ISSUER)
            {
                let name = format!("hand{}", by_hand.len());
                if let Ok(id) = tenancy::create_project(&db, t.id, &name, "", 1, "op") {
                    by_hand.push((id, t.id, name));
                }
            }
            let adm: Vec<RemoteTenant> = admitted
                .iter()
                .map(|t| RemoteTenant {
                    id: t.to_string(),
                    slug: slugs.get(t).copied().unwrap_or("acme").to_string(),
                    display_name: String::new(),
                })
                .collect();
            let feed = (
                adm.iter()
                    .map(|a| FeedTenant {
                        id: a.id.clone(),
                        slug: a.slug.clone(),
                        suspended: false,
                    })
                    .collect::<Vec<_>>(),
                projects
                    .iter()
                    .map(|(id, (t, n))| FeedProject {
                        id: id.to_string(),
                        tenant: t.to_string(),
                        name: n.to_string(),
                    })
                    .collect::<Vec<_>>(),
            );
            // A run cut short by a failover, then (sometimes) a second
            // puller's run over the same feed, and moves ending.
            let stop = if rng.chance(40) {
                rng.below(5) as usize
            } else {
                usize::MAX
            };
            run(&db, &mut w, realm, &adm, &feed, stop);
            if rng.chance(30) {
                run(&db, &mut w, realm, &adm, &feed, usize::MAX);
            }
            finish_moves(&db, &mut w, &mut rng, false);
        }
        // Quiet at last: every move ends, and runs reach a fixed point.
        let adm: Vec<RemoteTenant> = admitted
            .iter()
            .map(|t| RemoteTenant {
                id: t.to_string(),
                slug: slugs.get(t).copied().unwrap_or("acme").to_string(),
                display_name: String::new(),
            })
            .collect();
        let feed_t: Vec<FeedTenant> = adm
            .iter()
            .map(|a| FeedTenant {
                id: a.id.clone(),
                slug: a.slug.clone(),
                suspended: false,
            })
            .collect();
        let feed_p: Vec<FeedProject> = projects
            .iter()
            .map(|(id, (t, n))| FeedProject {
                id: id.to_string(),
                tenant: t.to_string(),
                name: n.to_string(),
            })
            .collect();
        let feed = (feed_t, feed_p);
        for _ in 0..6 {
            finish_moves(&db, &mut w, &mut rng, true);
            run(&db, &mut w, realm, &adm, &feed, usize::MAX);
        }
        finish_moves(&db, &mut w, &mut rng, true);
        let now = tenancy::load(&db).unwrap();
        let realms = crate::realms::load(&db).unwrap();
        let left = plan(ISSUER, &adm, &feed.0, &feed.1, &now, |t| {
            realms.realm_of(t) == realm
        });
        assert!(left.is_empty(), "seed {seed}: still to do {left:?}");

        // Each admitted tenant: one tenant here, in the realm, named
        // after its slug.
        for a in &adm {
            let ts: Vec<_> = now
                .tenants
                .iter()
                .filter(|t| t.directory == ISSUER && t.glidex_id == a.id)
                .collect();
            assert_eq!(ts.len(), 1, "seed {seed}: tenant {} {ts:?}", a.id);
            let base = name_of(&a.slug);
            assert!(
                ts[0].name == base || ts[0].name.starts_with(&format!("{base}-")),
                "seed {seed}: {} for {base}",
                ts[0].name
            );
            assert_eq!(realms.realm_of(ts[0].id), realm, "seed {seed}");
        }
        // No tenant linked but not admitted.
        assert!(
            now.tenants
                .iter()
                .all(|t| t.directory != ISSUER || admitted.contains(t.glidex_id.as_str())),
            "seed {seed}: {:?}",
            now.tenants
        );
        // Each project of an admitted tenant: one here, in its tenant,
        // active, named after it.
        for (gid, (gt, name)) in &projects {
            let ours: Vec<_> = now
                .projects
                .iter()
                .filter(|p| p.glidex_id == *gid)
                .collect();
            if !admitted.contains(gt) {
                assert!(
                    ours.iter()
                        .all(|p| now.tenant(p.tenant).is_none_or(|t| t.directory != ISSUER)),
                    "seed {seed}: {ours:?}"
                );
                continue;
            }
            assert_eq!(ours.len(), 1, "seed {seed}: project {gid} {ours:?}");
            let t = now.tenant(ours[0].tenant).unwrap();
            assert_eq!(
                (t.directory.as_str(), t.glidex_id.as_str()),
                (ISSUER, *gt),
                "seed {seed}: project {gid}"
            );
            assert_eq!(ours[0].state, ProjectState::Active, "seed {seed}");
            let base = name_of(name);
            assert!(
                ours[0].name == base || ours[0].name.starts_with(&format!("{base}-")),
                "seed {seed}: {} for {base}",
                ours[0].name
            );
        }
        // No linked project the feed doesn't have; dropped ones holding
        // containers stay, unlinked.
        for p in &now.projects {
            if !p.glidex_id.is_empty()
                && now.tenant(p.tenant).is_some_and(|t| t.directory == ISSUER)
            {
                assert!(
                    projects.contains_key(p.glidex_id.as_str()),
                    "seed {seed}: {p:?}"
                );
            }
        }
        // A project holding containers is never deleted by the feed.
        for id in &w.holding {
            assert!(now.project(*id).is_some(), "seed {seed}: {id}");
        }
        // Local records as they were.
        let l = now.tenant(local).unwrap();
        assert_eq!((l.name.as_str(), l.directory.as_str()), ("local", ""));
        assert!(now.project_named(local, "web").is_some(), "seed {seed}");
        for (id, tenant, name) in &by_hand {
            let p = now
                .project(*id)
                .unwrap_or_else(|| panic!("seed {seed}: {id}"));
            assert_eq!((p.tenant, &p.name), (*tenant, name), "seed {seed}");
        }
    }
}
