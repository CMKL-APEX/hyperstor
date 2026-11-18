//! `TrustService` (19-administration.md §23.12, 04-decisions.md D306): trust
//! in glidex directories, kept by `crate::trust`.
//!
//! - **Offers** are made here (`hyperstorctl admission offer`), the secret
//!   from glidex-trust's random source; the reply carries the `gxo_` token
//!   once, and the store keeps the secret's hash only (glidex D64).
//! - **Redeems and ends** reach hyperstor-mgmt, the address in the offer,
//!   which relays them here with the peer assertion they carry: the checks
//!   are glidex-trust's (glidex D65, D66), against the replay cache of the
//!   member that answers, the leader. A redeem spends the offer first,
//!   then makes the directory's realm if it has none, the tenant, and the
//!   trust record.
//! - **A directory trusted as a whole** (D345–D347) redeemed a directory
//!   offer, which named no tenant; its tenants opt in (`POST
//!   /trust/v1/admit`, relayed by mgmt as redeems are) and are admitted by
//!   its setting: at once (`open`), or once a security administrator
//!   approves (`approval`). An admitted tenant is made as a tenant offer's
//!   is, with a copy of the directory's default caps.
//! - **The feed** is pulled by the leader's mgmt (D306), signed with the
//!   cluster key towards directories, which this service hands mgmt alone:
//!   an ES256 key made on first use and kept sealed with the cluster key
//!   (D162). mgmt hands back each snapshot; it becomes the replica, and its
//!   tenants and projects the records here ([`crate::trust::plan`]): made,
//!   renamed, moved (as MoveProject's operation) and dropped. What can't be
//!   done is kept, shown and retried at the next pull.
//! - **Reports** count each admitted tenant's volumes, their provisioned
//!   size, buckets and their bytes, and S3 keys (glidex D85).
//! - **Logins** (D307–D309) are mgmt's to check; this service hands it
//!   what it checks them against ([`TrustService::get_login_state`]) and
//!   who a directory's person is here, with the grants the feed's links
//!   give ([`crate::trust::person_of`]).

use super::{Api, Need, with_reason};
use crate::tenancy::{self, NewTenant, Tenancy};
use crate::trust::{self, DirRec, OfferRec, PoolCap, Step};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use glidex_trust::cluster::{self as gt_cluster, Offer};
use glidex_trust::{
    AdmissionStatus, AdmitAnswer, AdmitRequest, AdmitSetting, EndAtCluster, OfferScope, OfferToken,
    RedeemAnswer, RedeemRequest, RemoteTenant, Snapshot,
};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{TrustService, TrustServiceExt};
use std::sync::Arc;

pub(super) fn register(api: Arc<Api>, router: connectrpc::Router) -> connectrpc::Router {
    TrustServiceExt::register(api, router)
}

fn invalid(msg: impl Into<String>) -> ConnectError {
    with_reason(
        ConnectError::invalid_argument(msg.into()),
        "invalid_argument",
    )
}

/// How long an offer is good for, in days, by default and at most (glidex
/// D64).
const OFFER_DAYS: u64 = glidex_trust::OFFER_DAYS;
const OFFER_MAX_DAYS: u64 = glidex_trust::OFFER_MAX_DAYS;

/// The longest `max_staleness` a directory may be given: a week.
const MAX_STALENESS_MINUTES: u32 = 7 * 24 * 60;

/// A run of the feed takes this many passes at most: each pass plans
/// against the records the last one left.
const PASSES: usize = 4;

/// A refusal of a directory's call: the same answer whatever the reason
/// (glidex D65), which the audit names.
fn trust_refused() -> (u32, Vec<u8>) {
    (403, br#"{"code":"trust_refused"}"#.to_vec())
}

/// `s` as a certificate's SHA-256: 64 hex digits, lower case, with or
/// without `:` between bytes.
pub fn normalize_pin(s: &str) -> Option<String> {
    let h: String = s
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// Whether `u` is where a directory may reach mgmt: `https://host[:port]`,
/// no path, query or user.
fn valid_url(u: &str) -> bool {
    u.parse::<http::Uri>().is_ok_and(|x| {
        x.scheme_str() == Some("https")
            && x.authority().is_some_and(|a| !a.as_str().contains('@'))
            && matches!(x.path(), "" | "/")
            && x.query().is_none()
    })
}

/// The realm a directory's issuer names by default: its last path segment
/// (glidex's issuers are `https://host/d/<directory>`), else its host.
pub fn realm_for(issuer: &str) -> String {
    let rest = issuer.trim_start_matches("https://");
    let last = rest
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(rest)
        .split(':')
        .next()
        .unwrap_or("");
    trust::name_of(last)
}

/// An admission setting as the API names it.
fn admit_name(a: AdmitSetting) -> &'static str {
    match a {
        AdmitSetting::Open => "open",
        AdmitSetting::Approval => "approval",
    }
}

/// An admission setting as given: `open`, `approval`, or empty for `none`.
fn admit_of(s: &str, none: Option<AdmitSetting>) -> Result<Option<AdmitSetting>, ConnectError> {
    match s {
        "" => Ok(none),
        "open" => Ok(Some(AdmitSetting::Open)),
        "approval" => Ok(Some(AdmitSetting::Approval)),
        _ => Err(invalid(format!(
            "{s:?}: a directory admits its tenants \"open\" or by \"approval\""
        ))),
    }
}

fn status_name(s: AdmissionStatus) -> &'static str {
    match s {
        AdmissionStatus::Admitted => "admitted",
        AdmissionStatus::Pending => "pending",
        AdmissionStatus::Refused => "refused",
    }
}

fn caps_pb(c: &[PoolCap]) -> Vec<pb::PoolCap> {
    c.iter()
        .map(|c| pb::PoolCap {
            pool: c.pool.clone(),
            bytes: c.bytes,
            ..Default::default()
        })
        .collect()
}

/// Caps as given: one per pool, each of a pool the map has, more than 0
/// bytes; sorted.
fn caps_of(
    c: impl IntoIterator<Item = (String, u64)>,
    pools: &[String],
) -> Result<Vec<PoolCap>, ConnectError> {
    let mut out: Vec<PoolCap> = Vec::new();
    for (pool, bytes) in c {
        if !pools.iter().any(|p| p == &pool) {
            return Err(invalid(format!("no pool {pool:?} for a cap")));
        }
        if bytes == 0 {
            return Err(invalid(format!("pool {pool}'s cap is 0 bytes")));
        }
        if out.iter().any(|y| y.pool == pool) {
            return Err(invalid(format!("pool {pool} capped twice")));
        }
        out.push(PoolCap { pool, bytes });
    }
    out.sort();
    Ok(out)
}

/// Whether two JWKS share a key: a directory trusted already that redeems
/// a directory offer must sign with keys it is known by (D345).
fn share_a_key(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    let keys = |v: &serde_json::Value| v["keys"].as_array().cloned().unwrap_or_default();
    let b = keys(b);
    keys(a).iter().any(|k| b.contains(k))
}

fn offer_pb(o: &OfferRec, now: u64) -> pb::TrustOffer {
    pb::TrustOffer {
        id: o.id.clone(),
        tenant: o.tenant.clone(),
        directory: o.directory.clone(),
        name: o.name.clone(),
        realm: o.realm.clone(),
        url: o.url.clone(),
        pin: o.pin.clone(),
        created_unix: o.created_unix,
        created_by: o.created_by.clone(),
        expires_unix: o.expires_unix,
        state: o.shown_state(now).to_string(),
        ui: o.ui.clone(),
        scope: match o.scope {
            OfferScope::Tenant => "tenant".into(),
            OfferScope::Directory => "directory".into(),
        },
        admit: if o.scope == OfferScope::Directory {
            admit_name(o.admit).into()
        } else {
            String::new()
        },
        caps: caps_pb(&o.caps),
        ..Default::default()
    }
}

impl Api {
    /// The cluster id certificates name: what directories know this
    /// cluster by.
    fn cluster_id(&self) -> Result<String, ConnectError> {
        self.cluster.clone().ok_or_else(|| {
            with_reason(
                ConnectError::failed_precondition(
                    "trust in directories needs the cluster's TLS (a cluster id)",
                ),
                "no_cluster_id",
            )
        })
    }

    /// The cluster key towards directories, made and kept on first use
    /// (only the leader writes it); opened with the cluster key.
    async fn trust_key(&self) -> Result<glidex_trust::ClusterKey, ConnectError> {
        let sealer = self.cluster_key()?;
        let kept = match self.realm_rows("trust key", trust::key).await? {
            Some(k) => k,
            None => {
                let kid = format!("{}:1", self.cluster_id()?);
                let k = glidex_trust::ClusterKey::generate(&kid).map_err(ConnectError::internal)?;
                let der = k.to_pkcs8().map_err(ConnectError::internal)?;
                let rec = trust::KeyRec {
                    sealed: sealer
                        .seal(&trust::key_context(&kid), &der)
                        .map_err(ConnectError::internal)?,
                    kid,
                };
                self.realm_rows("trust key", move |db| trust::put_key(db, &rec))
                    .await?
            }
        };
        let der = sealer
            .open(&trust::key_context(&kept.kid), &kept.sealed)
            .ok_or_else(|| {
                with_reason(
                    ConnectError::failed_precondition(
                        "the cluster key towards directories doesn't open with this member's cluster key",
                    ),
                    "no_cluster_key",
                )
            })?;
        glidex_trust::ClusterKey::from_pkcs8(&kept.kid, &der).map_err(ConnectError::internal)
    }

    /// Runs the feed's plan for directory `id` against the records until a
    /// pass changes nothing: what couldn't be done, each with why.
    async fn reconcile(&self, who: &str, id: &str) -> Result<Vec<String>, ConnectError> {
        let id_owned = id.to_string();
        let Some(d) = self
            .realm_rows("directory", move |db| {
                Ok(trust::directories(db)?
                    .into_iter()
                    .find(|d| d.dir.id == id_owned))
            })
            .await?
        else {
            return Ok(Vec::new());
        };
        let id_owned = id.to_string();
        let (tenants, projects) = self
            .realm_rows("feed", move |db| trust::feed_records(db, &id_owned))
            .await?;
        let actor = format!("{who} for directory {}", d.dir.issuer);
        let mut blocked = Vec::new();
        let mut synced = false;
        for _ in 0..PASSES {
            let t = self.tenancy().await?;
            let realms = self.realms().await?;
            let steps = trust::plan(
                &d.dir.issuer,
                &d.dir.tenants,
                &tenants,
                &projects,
                &t,
                |x| realms.realm_of(x) == d.realm,
            );
            blocked.clear();
            let mut done = 0;
            for s in &steps {
                match self.step(&actor, &d, &t, s).await {
                    Ok(()) => {
                        done += 1;
                        synced |= matches!(s, Step::MakeTenant { .. } | Step::TenantRealm { .. });
                    }
                    Err(e) => blocked.push(format!(
                        "{s}: {}",
                        e.message.as_deref().unwrap_or("refused")
                    )),
                }
            }
            if done == 0 {
                break;
            }
        }
        if synced {
            // Bound hosts check volumes by their tenant's realm (D259).
            self.sync_realms().await?;
        }
        Ok(blocked)
    }

    /// Does `s` for directory `d`, audited as `actor`'s.
    async fn step(
        &self,
        actor: &str,
        d: &DirRec,
        t: &Tenancy,
        s: &Step,
    ) -> Result<(), ConnectError> {
        let target = s.to_string();
        let realm = d.realm;
        let r = match s.clone() {
            Step::MakeTenant { glidex_id, name } => {
                let n = NewTenant {
                    name,
                    id: None,
                    directory: d.dir.issuer.clone(),
                    glidex_id,
                    created_unix: super::admin::now_unix(),
                    created_by: actor.to_string(),
                };
                let in_use = self.bare_tenants().await?;
                self.realm_rows("tenant", move |db| {
                    let (id, _) = tenancy::create_tenant(db, &n, &in_use)?;
                    crate::realms::set_tenant(db, id, realm, &[])
                })
                .await
            }
            Step::RenameTenant { id, name } => {
                self.realm_rows("such tenant", move |db| {
                    tenancy::rename_tenant(db, id, &name)
                })
                .await
            }
            Step::TenantRealm { id } => {
                let held = self.held(Some(id)).await?;
                self.realm_rows("such realm", move |db| {
                    crate::realms::set_tenant(db, id, realm, &held)
                })
                .await
            }
            Step::MakeProject {
                tenant,
                glidex_id,
                name,
            } => {
                let who = actor.to_string();
                self.realm_rows("such tenant", move |db| {
                    tenancy::create_project(
                        db,
                        tenant,
                        &name,
                        &glidex_id,
                        super::admin::now_unix(),
                        &who,
                    )
                    .map(|_| ())
                })
                .await
            }
            Step::RenameProject { id, name } => {
                self.realm_rows("such project", move |db| {
                    tenancy::rename_project(db, id, &name)
                })
                .await
            }
            Step::Move {
                project,
                to,
                rename,
            } => {
                if let Some(name) = rename {
                    self.realm_rows("such project", move |db| {
                        tenancy::rename_project(db, project, &name)
                    })
                    .await?;
                }
                let t = self.tenancy().await?;
                let target = format!("project {project} to tenant {to}");
                return self
                    .start_project_move(actor, &target, &t, project, to, false)
                    .await
                    .map(|_| ());
            }
            Step::Drop { id } => {
                let tenant = t.project(id).map_or(0, |p| p.tenant);
                let holds = self.holds(t, tenant, Some(id)).await?;
                if holds.is_empty() {
                    self.realm_rows("such project", move |db| {
                        tenancy::delete_project(db, id, &[])
                    })
                    .await
                } else {
                    // Kept with what it holds, as glidex keeps a tenant
                    // that still holds resources: unlinked, a local
                    // project from now on.
                    super::audit(
                        actor,
                        "TrustFeed",
                        &target,
                        &format!("kept, unlinked: it holds {}", holds.join(", ")),
                    );
                    self.realm_rows("such project", move |db| {
                        tenancy::set_project_link(db, id, "")
                    })
                    .await
                }
            }
            Step::Unlink { id } => {
                let linked: Vec<u32> = t
                    .projects
                    .iter()
                    .filter(|p| p.tenant == id && !p.glidex_id.is_empty())
                    .map(|p| p.id)
                    .collect();
                self.realm_rows("such tenant", move |db| {
                    tenancy::set_tenant_link(db, id, "", "")?;
                    for p in linked {
                        tenancy::set_project_link(db, p, "")?;
                    }
                    Ok(())
                })
                .await
            }
            Step::UnlinkProject { id } => {
                self.realm_rows("such project", move |db| {
                    tenancy::set_project_link(db, id, "")
                })
                .await
            }
        };
        Self::audited(actor, "TrustFeed", &target, r)
    }

    /// Tenants in use without records (on volumes, buckets and keys),
    /// which a new tenant's id must be past.
    async fn bare_tenants(&self) -> Result<Vec<u32>, ConnectError> {
        let mut in_use: Vec<u32> = self.held(None).await?.iter().map(|h| h.tenant).collect();
        in_use.extend(
            self.s3_rows("key", crate::s3keys::keys)
                .await?
                .iter()
                .map(|k| k.tenant),
        );
        Ok(in_use)
    }

    /// The tenant here of glidex tenant `glidex` of directory `issuer`, in
    /// realm `realm`: the one linked to it, else one made, named `name` or
    /// (with `name` empty) after its slug, with a suffix if that is taken
    /// (§24.9). Its id.
    async fn linked_tenant(
        &self,
        issuer: &str,
        glidex: &RemoteTenant,
        name: &str,
        realm: u32,
        now: u64,
    ) -> Result<u32, ConnectError> {
        let t = self.tenancy().await?;
        if let Some(x) = t
            .tenants
            .iter()
            .find(|x| x.directory == issuer && x.glidex_id == glidex.id)
        {
            return Ok(x.id);
        }
        let name = if name.is_empty() {
            trust::free_name(&trust::name_of(&glidex.slug), |n| {
                t.tenant_named(n).is_some()
            })
        } else {
            name.to_string()
        };
        let n = NewTenant {
            name,
            id: None,
            directory: issuer.to_string(),
            glidex_id: glidex.id.clone(),
            created_unix: now,
            created_by: format!("directory {issuer}"),
        };
        let in_use = self.bare_tenants().await?;
        let id = self
            .realm_rows("tenant", move |db| {
                let (id, _) = tenancy::create_tenant(db, &n, &in_use)?;
                crate::realms::set_tenant(db, id, realm, &[])?;
                Ok(id)
            })
            .await?;
        self.sync_realms().await?;
        Ok(id)
    }

    /// The directory trusted with issuer or id `d`, else `not_found`.
    async fn trusted(&self, d: &str) -> Result<DirRec, ConnectError> {
        let dirs = self.realm_rows("directory", trust::directories).await?;
        let issuer = glidex_trust::normalize_issuer(d).unwrap_or_default();
        dirs.into_iter()
            .find(|x| x.dir.id == d || x.dir.issuer == issuer)
            .ok_or_else(|| {
                with_reason(
                    ConnectError::not_found(format!("no trusted directory {d}")),
                    "not_found",
                )
            })
    }

    /// The pools' names, for caps.
    async fn pool_names(&self) -> Result<Vec<String>, ConnectError> {
        let (map, ..) = self.published().await?;
        Ok(map.pools.values().map(|p| p.name.clone()).collect())
    }

    /// Unlinks the tenants here of glidex tenant `glidex` of directory
    /// `issuer`, and their projects: they stay, local from now on.
    async fn unlink_tenant(&self, issuer: &str, glidex: &str) -> Result<(), ConnectError> {
        let t = self.tenancy().await?;
        for x in t
            .tenants
            .iter()
            .filter(|x| x.directory == issuer && x.glidex_id == glidex)
        {
            let id = x.id;
            let linked: Vec<u32> = t
                .projects
                .iter()
                .filter(|p| p.tenant == id && !p.glidex_id.is_empty())
                .map(|p| p.id)
                .collect();
            self.realm_rows("such tenant", move |db| {
                tenancy::set_tenant_link(db, id, "", "")?;
                for p in linked {
                    tenancy::set_project_link(db, p, "")?;
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    /// A directory's redeem, checked (glidex D65): the offer spent first,
    /// then the realm, the tenant and the trust record. The answer.
    async fn redeem_offer(
        &self,
        assertion: &str,
        b: &RedeemRequest,
        now: u64,
    ) -> Result<RedeemAnswer, (String, Option<ConnectError>)> {
        let cluster_id = self.cluster_id().map_err(|e| (String::new(), Some(e)))?;
        let id = b.offer.clone();
        let offer = self
            .realm_rows("offer", move |db| {
                Ok(trust::offers(db)?.into_iter().find(|o| o.id == id))
            })
            .await
            .map_err(|e| (String::new(), Some(e)))?;
        let checked = gt_cluster::check_redeem(
            assertion,
            b,
            offer.as_ref().map(|o| Offer {
                open: o.open_at(now),
                secret_hash: &o.secret_hash,
                directory: &o.directory,
                scope: o.scope,
                tenant: &o.tenant,
                admit: o.admit,
            }),
            &cluster_id,
            &self.replays,
            now,
        )
        .map_err(|why| (why, None))?;
        let offer = offer.expect("checked");
        // Spent before anything else is written: a second redeem of it,
        // even one racing this, finds it closed.
        let id = offer.id.clone();
        let spent = self
            .realm_rows("offer", move |db| trust::spend_offer(db, &id, now))
            .await
            .map_err(|e| (String::new(), Some(e)))?;
        if !spent.is_some_and(|o| o.open_at(now)) {
            return Err(("the offer was redeemed meanwhile".into(), None));
        }
        let key = self
            .trust_key()
            .await
            .map_err(|e| (String::new(), Some(e)))?;
        let fail = |e: ConnectError| (e.message.clone().unwrap_or_default(), Some(e));
        // The directory's realm: its own if trusted already, else the
        // offer's, made unless it exists.
        let issuer = checked.issuer.clone();
        let existing = self
            .realm_rows("directory", move |db| trust::directory_of(db, &issuer))
            .await
            .map_err(fail)?;
        // A directory trusted already gains the whole trust only with
        // keys it is known by (D345).
        if let Some(e) = &existing
            && checked.whole.is_some()
            && !share_a_key(&e.dir.jwks, &b.jwks)
        {
            return Err((
                "a directory offer redeemed with keys the directory isn't known by".into(),
                None,
            ));
        }
        let realm = match &existing {
            Some(d) => d.realm,
            None => {
                let name = offer.realm.clone();
                let who = format!("directory {}", checked.issuer);
                let made = self
                    .realm_rows("realm", move |db| {
                        let all = crate::realms::load(db)?;
                        match all.id(&name) {
                            Some(r) => Ok(r),
                            None => crate::realms::create(
                                db,
                                &crate::realms::Realm {
                                    id: 0,
                                    name: name.clone(),
                                    description: "a glidex directory's".into(),
                                    created_unix: now,
                                    created_by: who,
                                },
                            ),
                        }
                    })
                    .await
                    .map_err(fail)?;
                let others = self
                    .realm_rows("directory", trust::directories)
                    .await
                    .map_err(fail)?;
                if others.iter().any(|d| d.realm == made) {
                    return Err((
                        format!("realm {} is another directory's", offer.realm),
                        None,
                    ));
                }
                made
            }
        };
        // A tenant offer's tenant: the one linked to this glidex tenant,
        // else a new one by the offer's name. A directory offer admits
        // none (D345).
        if let Some(gt) = &b.tenant {
            self.linked_tenant(&checked.issuer, gt, &offer.name, realm, now)
                .await
                .map_err(fail)?;
        }
        // Logins come back to the address this offer names for people
        // (the last redeemed offer's), else to the directories' own.
        let ui_callback = trust::callback_for(if offer.ui.is_empty() {
            &offer.url
        } else {
            &offer.ui
        });
        let (caps, tenant_caps) = match existing.as_ref() {
            Some(e) => (e.caps.clone(), e.tenant_caps.clone()),
            None => Default::default(),
        };
        let d = DirRec {
            dir: gt_cluster::redeemed_directory(existing.map(|e| e.dir), b, &checked, now),
            realm,
            ui_callback: ui_callback.clone(),
            // A directory offer's caps are the defaults from now on (D347).
            caps: if checked.whole.is_some() {
                offer.caps.clone()
            } else {
                caps
            },
            tenant_caps,
        };
        self.realm_rows("directory", move |db| trust::put_directory(db, &d))
            .await
            .map_err(fail)?;
        Ok(RedeemAnswer {
            cluster_id,
            cluster_key: glidex_trust::key::jwks(&[&key]),
            ui_callback: Some(ui_callback),
            admit: checked.whole,
        })
    }

    /// A directory's opt-in, checked (D346): admitted, with its tenant made
    /// here, or pending. `Err` says why it was refused, for the audit.
    async fn admit_tenant(
        &self,
        assertion: &str,
        b: &AdmitRequest,
        now: u64,
    ) -> Result<AdmissionStatus, (String, Option<ConnectError>)> {
        let fail = |e: ConnectError| (e.message.clone().unwrap_or_default(), Some(e));
        let cluster_id = self.cluster_id().map_err(fail)?;
        let dirs = self
            .realm_rows("directory", trust::directories)
            .await
            .map_err(fail)?;
        let d = gt_cluster::check_admit(
            assertion,
            b,
            |iss| {
                dirs.iter()
                    .find(|d| d.dir.issuer == iss)
                    .map(|d| d.dir.clone())
            },
            &cluster_id,
            &self.replays,
            now,
        )
        .map_err(|why| (why, None))?;
        let (id, t) = (d.id.clone(), b.tenant.clone());
        let Some((rec, status)) = self
            .realm_rows("directory", move |db| trust::admit(db, &id, &t))
            .await
            .map_err(fail)?
        else {
            return Err(("not trusted as a whole any more".into(), None));
        };
        if status.is_admitted() {
            self.linked_tenant(&rec.dir.issuer, &b.tenant, "", rec.realm, now)
                .await
                .map_err(fail)?;
        }
        Ok(status)
    }
}

#[allow(refining_impl_trait)]
impl TrustService for Api {
    async fn create_offer(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateOfferRequest>,
    ) -> ServiceResult<pb::CreateOfferResponse> {
        // No tenant: an offer to the directory as a whole (D345).
        let whole = req.tenant.trim().is_empty();
        let target = if whole {
            format!("directory {}", req.directory)
        } else {
            format!("tenant {} of {}", req.tenant, req.directory)
        };
        let who = self.authorize(&ctx, "CreateOffer", &target, true)?;
        let r = async {
            let cluster_id = self.cluster_id()?;
            let slug = req.tenant.trim().to_string();
            if !whole
                && (slug.len() > 63 || slug.chars().any(|c| c.is_control() || c.is_whitespace()))
            {
                return Err(invalid("a tenant's slug is 1 to 63 characters, no spaces"));
            }
            if !whole && (!req.admit.is_empty() || !req.caps.is_empty()) {
                return Err(invalid(
                    "an admission setting and caps are a directory offer's (no tenant)",
                ));
            }
            if whole && !req.name.is_empty() {
                return Err(invalid(
                    "a directory offer names no tenant: its tenants are named after their slugs",
                ));
            }
            let admit = admit_of(req.admit, Some(AdmitSetting::Open))?.unwrap_or_default();
            let caps = if req.caps.is_empty() {
                Vec::new()
            } else {
                caps_of(
                    req.caps.iter().map(|c| (c.pool.to_string(), c.bytes)),
                    &self.pool_names().await?,
                )?
            };
            let Some(issuer) = glidex_trust::normalize_issuer(req.directory) else {
                return Err(invalid(format!(
                    "{:?} isn't a directory's issuer (https://host/d/NAME)",
                    req.directory
                )));
            };
            if !valid_url(req.url) {
                return Err(invalid(format!(
                    "{:?}: mgmt's address for directories is https://host[:port]",
                    req.url
                )));
            }
            if !req.ui.is_empty() && !valid_url(req.ui) {
                return Err(invalid(format!(
                    "{:?}: mgmt's address for people's browsers is https://host[:port]",
                    req.ui
                )));
            }
            let Some(pin) = normalize_pin(req.pin) else {
                return Err(invalid(
                    "a certificate's SHA-256 is 64 hex digits (hyperstorctl reads it from --url)",
                ));
            };
            let name = if whole {
                String::new()
            } else if req.name.is_empty() {
                trust::name_of(&slug)
            } else {
                req.name.to_string()
            };
            if !whole && !tenancy::valid_tenant_name(&name) {
                return Err(invalid(format!(
                    "{name:?}: a tenant is named by 1-63 of a-z, 0-9 and -, and isn't \"default\""
                )));
            }
            let days = match u64::from(req.days) {
                0 => OFFER_DAYS,
                d if d <= OFFER_MAX_DAYS => d,
                _ => return Err(invalid(format!("an offer lasts 1 to {OFFER_MAX_DAYS} days"))),
            };
            let dirs = self.realm_rows("directory", trust::directories).await?;
            let realms = self.realms().await?;
            let mine = dirs.iter().find(|d| d.dir.issuer == issuer);
            let realm = match (mine, req.realm) {
                (Some(d), "") => realms.name(d.realm),
                (Some(d), r) if realms.id(r) == Some(d.realm) => r.to_string(),
                (Some(d), r) => {
                    return Err(with_reason(
                        ConnectError::failed_precondition(format!(
                            "{issuer} is trusted in realm {}, not {r}",
                            realms.name(d.realm)
                        )),
                        "realm_mismatch",
                    ));
                }
                (None, "") => realm_for(&issuer),
                (None, r) => r.to_string(),
            };
            if !crate::realms::valid_name(&realm) {
                return Err(invalid(format!(
                    "{realm:?}: a directory's realm is named by 1-63 of a-z, 0-9 and -, and isn't \"default\""
                )));
            }
            if mine.is_none()
                && let Some(r) = realms.id(&realm)
                && dirs.iter().any(|d| d.realm == r)
            {
                return Err(with_reason(
                    ConnectError::failed_precondition(format!(
                        "realm {realm} is another directory's"
                    )),
                    "realm_mismatch",
                ));
            }
            // A local tenant keeps its name (§24.9): pass another.
            let t = self.tenancy().await?;
            if !whole
                && let Some(x) = t.tenant_named(&name)
                && !(x.directory == issuer && !x.glidex_id.is_empty())
            {
                return Err(with_reason(
                    ConnectError::already_exists(format!(
                        "a tenant is named {name}: pass another name (--name)"
                    )),
                    "tenant_exists",
                ));
            }
            let now = super::admin::now_unix();
            let id = format!(
                "o{}",
                super::admin::random_bytes(8)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let secret = glidex_trust::random_token(32);
            let o = OfferRec {
                id: id.clone(),
                tenant: slug.clone(),
                directory: issuer.clone(),
                name,
                realm,
                url: req.url.trim_end_matches('/').to_string(),
                pin,
                secret_hash: glidex_trust::sha256_hex(secret.as_bytes()),
                created_unix: now,
                created_by: who.clone(),
                expires_unix: now + days * 86400,
                state: trust::OPEN.into(),
                ui: req.ui.trim_end_matches('/').to_string(),
                scope: if whole {
                    OfferScope::Directory
                } else {
                    OfferScope::Tenant
                },
                admit,
                caps,
            };
            let row = o.clone();
            self.realm_rows("offer", move |db| trust::create_offer(db, &row))
                .await?;
            let token = OfferToken {
                cluster_id: cluster_id.clone(),
                cluster_name: cluster_id,
                api: o.url.clone(),
                tls_sha256: o.pin.clone(),
                scope: o.scope,
                tenant: slug,
                directory: issuer,
                offer: id,
                secret,
                expires_at: o.expires_unix,
            }
            .encode();
            Ok((o, token, now))
        }
        .await;
        // The audit names the offer, never its secret.
        let target = match &r {
            Ok((o, ..)) => format!("{target}, offer {}", o.id),
            Err(_) => target,
        };
        let (o, token, now) = Self::audited(&who, "CreateOffer", &target, r)?;
        Ok(pb::CreateOfferResponse {
            offer: buffa::MessageField::some(offer_pb(&o, now)),
            token,
            ..Default::default()
        }
        .into())
    }

    async fn list_offers(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListOffersRequest>,
    ) -> ServiceResult<pb::ListOffersResponse> {
        self.authorize(&ctx, "ListOffers", "offers", false)?;
        let now = super::admin::now_unix();
        let offers = self.realm_rows("offer", trust::offers).await?;
        Ok(pb::ListOffersResponse {
            offers: offers.iter().map(|o| offer_pb(o, now)).collect(),
            ..Default::default()
        }
        .into())
    }

    async fn revoke_offer(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RevokeOfferRequest>,
    ) -> ServiceResult<pb::RevokeOfferResponse> {
        let target = format!("offer {}", req.id);
        let who = self.authorize(&ctx, "RevokeOffer", &target, true)?;
        let id = req.id.to_string();
        let r = self
            .realm_rows("such offer", move |db| trust::revoke_offer(db, &id))
            .await;
        Self::audited(&who, "RevokeOffer", &target, r)?;
        Ok(pb::RevokeOfferResponse::default().into())
    }

    async fn list_trusted_directories(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::ListTrustedDirectoriesRequest>,
    ) -> ServiceResult<pb::ListTrustedDirectoriesResponse> {
        self.authorize(&ctx, "ListTrustedDirectories", "directories", false)?;
        let (dirs, feeds, operator) = self
            .realm_rows("directory", |db| {
                let dirs = trust::directories(db)?;
                let feeds = dirs
                    .iter()
                    .map(|d| trust::feed(db, &d.dir.id))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((dirs, feeds, trust::operator(db)?))
            })
            .await?;
        let t = self.tenancy().await?;
        let realms = self.realms().await?;
        let tenant_pb = |d: &DirRec, x: &RemoteTenant, s: AdmissionStatus| {
            let here = t
                .tenants
                .iter()
                .find(|y| y.directory == d.dir.issuer && y.glidex_id == x.id)
                .filter(|_| s.is_admitted());
            pb::TrustedTenant {
                id: x.id.clone(),
                slug: x.slug.clone(),
                tenant: here.map_or(0, |y| y.id),
                name: here.map(|y| y.name.clone()).unwrap_or_default(),
                operator: here.is_some_and(|y| y.id == operator),
                admission: status_name(s).into(),
                caps: caps_pb(d.tenant_caps.get(&x.id).map_or(&[][..], |c| c)),
                ..Default::default()
            }
        };
        let directories = dirs
            .iter()
            .zip(feeds)
            .map(|(d, f)| pb::TrustedDirectory {
                id: d.dir.id.clone(),
                issuer: d.dir.issuer.clone(),
                api: d.dir.api.clone(),
                realm: realms.name(d.realm),
                tenants: d
                    .dir
                    .tenants
                    .iter()
                    .map(|x| tenant_pb(d, x, AdmissionStatus::Admitted))
                    .collect(),
                whole: d.whole(),
                admit: d
                    .dir
                    .whole
                    .as_ref()
                    .map(|w| admit_name(w.admit).to_string())
                    .unwrap_or_default(),
                caps: caps_pb(&d.caps),
                pending: d
                    .dir
                    .whole
                    .iter()
                    .flat_map(|w| w.pending.iter())
                    .map(|x| tenant_pb(d, x, AdmissionStatus::Pending))
                    .collect(),
                refused: d
                    .dir
                    .whole
                    .iter()
                    .flat_map(|w| w.refused.iter())
                    .map(|x| tenant_pb(d, x, AdmissionStatus::Refused))
                    .collect(),
                revision: f.state.revision,
                applied_unix: f.state.applied_at,
                checked_unix: f.state.checked_at,
                blocked: f.blocked,
                created_unix: d.dir.created_at,
                ui_callback: d.ui_callback.clone(),
                max_staleness_minutes: d
                    .dir
                    .max_staleness_minutes
                    .map_or(0, |m| u32::try_from(m).unwrap_or(u32::MAX)),
                ..Default::default()
            })
            .collect();
        Ok(pb::ListTrustedDirectoriesResponse {
            directories,
            operator,
            ..Default::default()
        }
        .into())
    }

    async fn end_trust(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::EndTrustRequest>,
    ) -> ServiceResult<pb::EndTrustResponse> {
        // No tenant: the whole directory's trust (D345).
        let target = if req.tenant.is_empty() {
            format!("directory {}", req.directory)
        } else {
            format!("tenant {} of {}", req.tenant, req.directory)
        };
        let who = self.authorize(&ctx, "EndTrust", &target, true)?;
        let r = async {
            let d = self.trusted(req.directory).await?;
            let gid = if req.tenant.is_empty() {
                String::new()
            } else {
                let Some((x, _)) = d
                    .dir
                    .admissions()
                    .into_iter()
                    .find(|(x, _)| x.id == req.tenant)
                    .or_else(|| {
                        d.dir
                            .admissions()
                            .into_iter()
                            .find(|(x, _)| x.slug == req.tenant)
                    })
                else {
                    return Err(with_reason(
                        ConnectError::not_found(format!(
                            "{} has no tenant {} here",
                            d.dir.issuer, req.tenant
                        )),
                        "not_found",
                    ));
                };
                x.id.clone()
            };
            let id = d.dir.id.clone();
            let ended = self
                .realm_rows("directory", move |db| {
                    trust::end_tenant(db, &id, &gid, true)
                })
                .await?;
            for g in ended.map(|e| e.unlink).unwrap_or_default() {
                self.unlink_tenant(&d.dir.issuer, &g).await?;
            }
            Ok(())
        }
        .await;
        Self::audited(&who, "EndTrust", &target, r)?;
        Ok(pb::EndTrustResponse::default().into())
    }

    async fn set_operator(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetOperatorRequest>,
    ) -> ServiceResult<pb::SetOperatorResponse> {
        let target = format!("operator tenant {}", req.tenant);
        let who = self.authorize(&ctx, "SetOperator", &target, true)?;
        let tenant = req.tenant;
        let r = async {
            if tenant != 0 {
                let t = self.tenancy().await?;
                let dirs = self.realm_rows("directory", trust::directories).await?;
                let linked = t.tenant(tenant).is_some_and(|x| {
                    dirs.iter().any(|d| {
                        d.dir.issuer == x.directory
                            && d.dir.tenants.iter().any(|y| y.id == x.glidex_id)
                    })
                });
                if !linked {
                    return Err(with_reason(
                        ConnectError::failed_precondition(format!(
                            "tenant {tenant} isn't a tenant of a trusted directory"
                        )),
                        "not_linked",
                    ));
                }
            }
            self.realm_rows("operator", move |db| trust::set_operator(db, tenant))
                .await
        }
        .await;
        Self::audited(&who, "SetOperator", &target, r)?;
        Ok(pb::SetOperatorResponse::default().into())
    }

    async fn get_trust_key(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetTrustKeyRequest>,
    ) -> ServiceResult<pb::GetTrustKeyResponse> {
        self.authorize_as(&ctx, "GetTrustKey", "trust key", Need::Mgmt)?;
        let key = self.trust_key().await?;
        Ok(pb::GetTrustKeyResponse {
            cluster_id: self.cluster_id()?,
            kid: key.kid().to_string(),
            pkcs8: key.to_pkcs8().map_err(ConnectError::internal)?,
            ..Default::default()
        }
        .into())
    }

    async fn relay_redeem(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RelayRedeemRequest>,
    ) -> ServiceResult<pb::RelayRedeemResponse> {
        let who = self.authorize_as(&ctx, "RelayRedeem", "redeem", Need::Mgmt)?;
        let now = super::admin::now_unix();
        let (status, body) = match serde_json::from_slice::<RedeemRequest>(req.body) {
            Err(e) => {
                super::audit(&who, "RedeemOffer", "?", &format!("refused: {e}"));
                trust_refused()
            }
            Ok(b) => {
                let target = match &b.tenant {
                    Some(t) => format!("offer {} by {} for tenant {}", b.offer, b.issuer, t.slug),
                    None => format!("offer {} by {} for the directory", b.offer, b.issuer),
                };
                match self.redeem_offer(req.assertion, &b, now).await {
                    Ok(a) => {
                        super::audit(&who, "RedeemOffer", &target, "ok");
                        (200, serde_json::to_vec(&a).expect("json"))
                    }
                    Err((why, e)) => {
                        let why = match &e {
                            Some(e) => e.message.clone().unwrap_or(why),
                            None => why,
                        };
                        super::audit(&who, "RedeemOffer", &target, &format!("refused: {why}"));
                        match e {
                            // Not leading, or the store unavailable: the
                            // directory may try again.
                            Some(e) if e.code == connectrpc::ErrorCode::Unavailable => {
                                return Err(e);
                            }
                            _ => trust_refused(),
                        }
                    }
                }
            }
        };
        Ok(pb::RelayRedeemResponse {
            status,
            body,
            ..Default::default()
        }
        .into())
    }

    async fn relay_end(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RelayEndRequest>,
    ) -> ServiceResult<pb::RelayEndResponse> {
        let who = self.authorize_as(&ctx, "RelayEnd", "end", Need::Mgmt)?;
        let now = super::admin::now_unix();
        let cluster_id = self.cluster_id()?;
        let Ok(b) = serde_json::from_slice::<EndAtCluster>(req.body) else {
            super::audit(&who, "EndFromDirectory", "?", "refused: not an end");
            let (status, body) = trust_refused();
            return Ok(pb::RelayEndResponse {
                status,
                body,
                ..Default::default()
            }
            .into());
        };
        let target = if b.tenant.is_empty() {
            format!("directory {}", b.issuer)
        } else {
            format!("tenant {} of {}", b.tenant, b.issuer)
        };
        let dirs = self.realm_rows("directory", trust::directories).await?;
        let checked = gt_cluster::check_end(
            req.assertion,
            &b,
            |iss| {
                dirs.iter()
                    .find(|d| d.dir.issuer == iss)
                    .map(|d| d.dir.clone())
            },
            &cluster_id,
            &self.replays,
            now,
        );
        let (status, body) = match checked {
            Err(why) => {
                super::audit(
                    &who,
                    "EndFromDirectory",
                    &target,
                    &format!("refused: {why}"),
                );
                trust_refused()
            }
            Ok(d) => {
                // One tenant leaves, or with none the whole directory
                // (D345): every tenant admitted is unlinked (D316).
                let (id, tenant) = (d.id.clone(), b.tenant.clone());
                let ended = self
                    .realm_rows("directory", move |db| {
                        trust::end_tenant(db, &id, &tenant, false)
                    })
                    .await?;
                let unlink = match ended {
                    Some(e) => e.unlink,
                    // Nothing here of it: unlinked already, or again.
                    None if !b.tenant.is_empty() => vec![b.tenant.clone()],
                    None => Vec::new(),
                };
                let r = async {
                    for g in unlink {
                        self.unlink_tenant(&d.issuer, &g).await?;
                    }
                    Ok(())
                }
                .await;
                Self::audited(&who, "EndFromDirectory", &target, r)?;
                (200, b"{}".to_vec())
            }
        };
        Ok(pb::RelayEndResponse {
            status,
            body,
            ..Default::default()
        }
        .into())
    }

    async fn relay_admit(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RelayAdmitRequest>,
    ) -> ServiceResult<pb::RelayAdmitResponse> {
        let who = self.authorize_as(&ctx, "RelayAdmit", "admit", Need::Mgmt)?;
        let now = super::admin::now_unix();
        let (status, body) = match serde_json::from_slice::<AdmitRequest>(req.body) {
            Err(e) => {
                super::audit(&who, "AdmitFromDirectory", "?", &format!("refused: {e}"));
                trust_refused()
            }
            Ok(b) => {
                let target = format!("tenant {} ({}) of {}", b.tenant.slug, b.tenant.id, b.issuer);
                match self.admit_tenant(req.assertion, &b, now).await {
                    Ok(status) => {
                        super::audit(
                            &who,
                            "AdmitFromDirectory",
                            &target,
                            &format!("ok: {}", status_name(status)),
                        );
                        (
                            200,
                            serde_json::to_vec(&AdmitAnswer { status }).expect("json"),
                        )
                    }
                    Err((why, e)) => {
                        let why = match &e {
                            Some(e) => e.message.clone().unwrap_or(why),
                            None => why,
                        };
                        super::audit(
                            &who,
                            "AdmitFromDirectory",
                            &target,
                            &format!("refused: {why}"),
                        );
                        match e {
                            Some(e) if e.code == connectrpc::ErrorCode::Unavailable => {
                                return Err(e);
                            }
                            _ => trust_refused(),
                        }
                    }
                }
            }
        };
        Ok(pb::RelayAdmitResponse {
            status,
            body,
            ..Default::default()
        }
        .into())
    }

    async fn decide_admission(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DecideAdmissionRequest>,
    ) -> ServiceResult<pb::DecideAdmissionResponse> {
        let (op, done) = if req.approve {
            ("ApproveAdmission", "admitted")
        } else {
            ("RefuseAdmission", "refused")
        };
        let target = format!("tenant {} of {}", req.tenant, req.directory);
        let who = self.authorize(&ctx, op, &target, true)?;
        let approve = req.approve;
        let r = async {
            let d = self.trusted(req.directory).await?;
            let (id, tenant) = (d.dir.id.clone(), req.tenant.to_string());
            let Some((rec, x)) = self
                .realm_rows("directory", move |db| {
                    trust::decide(db, &id, &tenant, approve)
                })
                .await?
            else {
                return Err(with_reason(
                    ConnectError::not_found(format!(
                        "{} has no tenant {} pending here",
                        d.dir.issuer, req.tenant
                    )),
                    "not_found",
                ));
            };
            let here = if approve {
                self.linked_tenant(&rec.dir.issuer, &x, "", rec.realm, super::admin::now_unix())
                    .await?
            } else {
                0
            };
            Ok((x, here))
        }
        .await;
        let target = match &r {
            Ok((x, _)) => format!("tenant {} ({}) of {}: {done}", x.slug, x.id, req.directory),
            Err(_) => target,
        };
        let (_, here) = Self::audited(&who, op, &target, r)?;
        Ok(pb::DecideAdmissionResponse {
            tenant: here,
            ..Default::default()
        }
        .into())
    }

    async fn set_directory_admission(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDirectoryAdmissionRequest>,
    ) -> ServiceResult<pb::SetDirectoryAdmissionResponse> {
        let caps_text = if req.set_caps {
            let c: Vec<String> = req
                .caps
                .iter()
                .map(|c| format!("{}={}", c.pool, c.bytes))
                .collect();
            format!(", caps [{}]", c.join(","))
        } else {
            String::new()
        };
        let target = format!(
            "admission of {}: {}{caps_text}",
            req.directory,
            if req.admit.is_empty() {
                "unchanged"
            } else {
                req.admit
            }
        );
        let who = self.authorize(&ctx, "SetDirectoryAdmission", &target, true)?;
        let r = async {
            let admit = admit_of(req.admit, None)?;
            let caps = if req.set_caps {
                Some(caps_of(
                    req.caps.iter().map(|c| (c.pool.to_string(), c.bytes)),
                    &self.pool_names().await?,
                )?)
            } else {
                None
            };
            let d = self.trusted(req.directory).await?;
            let id = d.dir.id.clone();
            let set = self
                .realm_rows("directory", move |db| {
                    trust::set_admission(db, &id, admit, caps)
                })
                .await?;
            if !set {
                return Err(with_reason(
                    ConnectError::failed_precondition(format!(
                        "{} isn't trusted as a whole: offer it a directory offer (admission offer with no --tenant)",
                        d.dir.issuer
                    )),
                    "not_whole",
                ));
            }
            Ok(())
        }
        .await;
        Self::audited(&who, "SetDirectoryAdmission", &target, r)?;
        Ok(pb::SetDirectoryAdmissionResponse::default().into())
    }

    async fn get_feed_work(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetFeedWorkRequest>,
    ) -> ServiceResult<pb::GetFeedWorkResponse> {
        self.authorize_as(&ctx, "GetFeedWork", "feeds", Need::Mgmt)?;
        let cluster_id = self.cluster_id()?;
        let (targets, ends) = self
            .realm_rows("directory", |db| {
                let mut targets = Vec::new();
                for d in trust::directories(db)? {
                    let f = trust::feed(db, &d.dir.id)?;
                    targets.push(pb::FeedTarget {
                        directory: serde_json::to_vec(&d.dir).expect("json"),
                        revision: f.state.revision,
                        ui_callback: d.ui_callback.clone(),
                        ..Default::default()
                    });
                }
                let ends = trust::endings(db)?
                    .into_iter()
                    .map(|e| pb::EndNotice {
                        directory: serde_json::to_vec(&e.dir).expect("json"),
                        tenant: e.tenant,
                        ..Default::default()
                    })
                    .collect();
                Ok((targets, ends))
            })
            .await?;
        Ok(pb::GetFeedWorkResponse {
            cluster_id,
            targets,
            ends,
            ..Default::default()
        }
        .into())
    }

    async fn apply_feed(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ApplyFeedRequest>,
    ) -> ServiceResult<pb::ApplyFeedResponse> {
        let target = format!("feed of {}", req.directory);
        let who = self.authorize_as(&ctx, "ApplyFeed", &target, Need::Mgmt)?;
        let snap = if req.snapshot.is_empty() {
            None
        } else {
            let s: Snapshot = serde_json::from_slice(req.snapshot)
                .map_err(|e| invalid(format!("not a snapshot: {e}")))?;
            if glidex_trust::revision_of(&s.jwks, &s.rows, &s.ended_sids) != s.revision {
                return Err(invalid(
                    "a snapshot whose revision doesn't match its content",
                ));
            }
            Some(s)
        };
        let changed = snap.is_some();
        let (id, now) = (req.directory.to_string(), super::admin::now_unix());
        let trusted = self
            .realm_rows("directory", move |db| {
                trust::apply_feed(db, &id, snap.as_ref(), now)
            })
            .await?;
        if !trusted {
            return Err(with_reason(
                ConnectError::not_found(format!("no trusted directory {}", req.directory)),
                "not_found",
            ));
        }
        if changed {
            super::audit(&who, "ApplyFeed", &target, "ok: a new revision");
        }
        // Every pull, changed or not: what was blocked may go now.
        let blocked = self.reconcile(&who, req.directory).await?;
        let (id, b) = (req.directory.to_string(), blocked.clone());
        self.realm_rows("directory", move |db| trust::set_blocked(db, &id, &b))
            .await?;
        Ok(pb::ApplyFeedResponse {
            blocked,
            ..Default::default()
        }
        .into())
    }

    async fn get_trust_report(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetTrustReportRequest>,
    ) -> ServiceResult<pb::GetTrustReportResponse> {
        self.authorize_as(&ctx, "GetTrustReport", "report", Need::Mgmt)?;
        let id = req.directory.to_string();
        let (d, tenants) = self
            .realm_rows("directory", move |db| {
                let d = trust::directories(db)?.into_iter().find(|d| d.dir.id == id);
                let tenants = match &d {
                    Some(d) => trust::feed_records(db, &d.dir.id)?.0,
                    None => Vec::new(),
                };
                Ok((d, tenants))
            })
            .await?;
        let Some(d) = d else {
            return Err(with_reason(
                ConnectError::not_found(format!("no trusted directory {}", req.directory)),
                "not_found",
            ));
        };
        let t = self.tenancy().await?;
        let here: Vec<(String, u32)> = d
            .dir
            .tenants
            .iter()
            .filter_map(|x| {
                t.tenants
                    .iter()
                    .find(|y| y.directory == d.dir.issuer && y.glidex_id == x.id)
                    .map(|y| (x.id.clone(), y.id))
            })
            .collect();
        let mut counts: std::collections::BTreeMap<u32, glidex_trust::StorageReport> = here
            .iter()
            .map(|(_, id)| (*id, glidex_trust::StorageReport::default()))
            .collect();
        // Volumes (not in the trash) and their provisioned size.
        let (map, ..) = self.published().await?;
        let mut bytes: std::collections::BTreeMap<u32, u64> = Default::default();
        for pool in map.pools.keys().filter(|&&p| p != hyperstor_map::META_POOL) {
            let (reply, rx) = hyperstor_reactor::chan::oneshot();
            self.tx
                .send(crate::Admin::ListVolumes {
                    pool: *pool,
                    trash: false,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            for v in super::volume_result(rx.recv().await)? {
                let owner = t.tenant_of(v.tenant, v.project);
                if let Some(c) = counts.get_mut(&owner) {
                    c.volumes += 1;
                    *bytes.entry(owner).or_default() += v.size;
                }
            }
        }
        for (owner, b) in bytes {
            if let Some(c) = counts.get_mut(&owner) {
                c.volume_gib = b.div_ceil(1 << 30);
            }
        }
        // The bytes they use, trash included, one copy (D339; D313's 0
        // until the counters existed): the leader's counts, which this
        // call needs anyway (mgmt asks the leader).
        let view = self.space().await?;
        for (owner, b) in super::space::used_by_tenant(&view, &t) {
            if let Some(c) = counts.get_mut(&owner) {
                c.volume_used_bytes = b;
            }
        }
        // Buckets and their objects' bytes.
        let crate::BucketDone::List(buckets) = self
            .bucket_op(crate::BucketOp::List { tenant: None })
            .await?
        else {
            return Err(ConnectError::internal("not a list"));
        };
        for b in buckets {
            let owner = t.tenant_of(b.tenant, b.project);
            if let Some(c) = counts.get_mut(&owner) {
                c.buckets += 1;
                c.bucket_bytes += b.counts.map_or(0, |(_, bytes)| bytes);
            }
        }
        for k in self.s3_rows("key", crate::s3keys::keys).await? {
            if let Some(c) = counts.get_mut(&k.tenant) {
                c.s3_keys += 1;
            }
        }
        let mut reported: Vec<glidex_trust::StorageTenantReport> = here
            .iter()
            .map(|(gid, id)| glidex_trust::StorageTenantReport {
                tenant: gid.clone(),
                suspended: tenants.iter().any(|x| &x.id == gid && x.suspended),
                admission: AdmissionStatus::Admitted,
                storage: counts.remove(id).unwrap_or_default(),
                // Per project (glidex D89): not sent yet, so the
                // directory blocks no project's delete (I415, #390).
                project_storage: Vec::new(),
            })
            .collect();
        // Opt-ins not admitted: their status, nothing held (D346).
        reported.extend(
            d.dir
                .admissions()
                .into_iter()
                .filter(|(_, s)| !s.is_admitted())
                .map(|(x, s)| glidex_trust::StorageTenantReport {
                    tenant: x.id.clone(),
                    suspended: false,
                    admission: s,
                    storage: Default::default(),
                    project_storage: Vec::new(),
                }),
        );
        let report = glidex_trust::ClusterReport {
            at: super::admin::now_unix(),
            tenants: reported,
        };
        Ok(pb::GetTrustReportResponse {
            report: serde_json::to_vec(&report).expect("json"),
            ..Default::default()
        }
        .into())
    }

    async fn end_noticed(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::EndNoticedRequest>,
    ) -> ServiceResult<pb::EndNoticedResponse> {
        let target = format!("tenant {} of {}", req.tenant, req.directory);
        let who = self.authorize_as(&ctx, "EndNoticed", &target, Need::Mgmt)?;
        let (id, tenant) = (req.directory.to_string(), req.tenant.to_string());
        let r = self
            .realm_rows("directory", move |db| trust::ending_told(db, &id, &tenant))
            .await;
        Self::audited(&who, "EndToldDirectory", &target, r)?;
        Ok(pb::EndNoticedResponse::default().into())
    }

    async fn set_directory_staleness(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDirectoryStalenessRequest>,
    ) -> ServiceResult<pb::SetDirectoryStalenessResponse> {
        let target = format!("staleness of {}: {} min", req.directory, req.minutes);
        let who = self.authorize(&ctx, "SetDirectoryStaleness", &target, true)?;
        let r = async {
            if req.minutes > MAX_STALENESS_MINUTES {
                return Err(invalid(format!(
                    "max_staleness is 1 to {MAX_STALENESS_MINUTES} minutes (0: the default)"
                )));
            }
            let dirs = self.realm_rows("directory", trust::directories).await?;
            let issuer = glidex_trust::normalize_issuer(req.directory).unwrap_or_default();
            let Some(d) = dirs
                .iter()
                .find(|d| d.dir.id == req.directory || d.dir.issuer == issuer)
            else {
                return Err(with_reason(
                    ConnectError::not_found(format!("no trusted directory {}", req.directory)),
                    "not_found",
                ));
            };
            let (id, m) = (d.dir.id.clone(), u64::from(req.minutes));
            self.realm_rows("directory", move |db| {
                trust::set_staleness(db, &id, (m > 0).then_some(m)).map(|_| ())
            })
            .await
        }
        .await;
        Self::audited(&who, "SetDirectoryStaleness", &target, r)?;
        Ok(pb::SetDirectoryStalenessResponse::default().into())
    }

    async fn get_login_state(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetLoginStateRequest>,
    ) -> ServiceResult<pb::GetLoginStateResponse> {
        self.authorize_as(&ctx, "GetLoginState", "logins", Need::Mgmt)?;
        let cluster_id = self.cluster_id()?;
        let (dirs, feeds) = self
            .realm_rows("directory", |db| {
                let dirs = trust::directories(db)?;
                let feeds = dirs
                    .iter()
                    .map(|d| trust::feed(db, &d.dir.id))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((dirs, feeds))
            })
            .await?;
        let realms = self.realms().await?;
        let directories = dirs
            .iter()
            .zip(feeds)
            .map(|(d, f)| pb::LoginDirectory {
                directory: serde_json::to_vec(&d.dir).expect("json"),
                realm: realms.name(d.realm),
                checked_unix: f.state.checked_at,
                applied_unix: f.state.applied_at,
                ended_sids: f.ended,
                ui_callback: d.ui_callback.clone(),
                ..Default::default()
            })
            .collect();
        Ok(pb::GetLoginStateResponse {
            cluster_id,
            directories,
            ..Default::default()
        }
        .into())
    }

    async fn resolve_person(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ResolvePersonRequest>,
    ) -> ServiceResult<pb::ResolvePersonResponse> {
        use glidex_trust::feed::{Entity, TokenKind};
        self.authorize_as(&ctx, "ResolvePerson", "people", Need::Mgmt)?;
        let cluster_id = self.cluster_id()?;
        let (dir, subject, hash) = (
            req.directory.to_string(),
            req.subject.to_string(),
            req.token_hash.to_string(),
        );
        if hash.is_empty() == (dir.is_empty() || subject.is_empty()) {
            return Err(invalid("a directory and a subject, or a token's hash"));
        }
        let found = self
            .realm_rows("directory", move |db| {
                let dirs = trust::directories(db)?;
                let operator = trust::operator(db)?;
                if !hash.is_empty() {
                    for d in dirs {
                        if let Some(tok) = trust::api_token(db, &d.dir.id, &hash)? {
                            let people = trust::people(db, &d.dir.id)?;
                            return Ok(Some((d, Some(tok), people, operator)));
                        }
                    }
                    return Ok(None);
                }
                let Some(d) = dirs.into_iter().find(|d| d.dir.id == dir) else {
                    return Ok(None);
                };
                let people = trust::people(db, &d.dir.id)?;
                Ok(Some((d, None, people, operator)))
            })
            .await?;
        let Some((d, token, people, operator)) = found else {
            return Ok(pb::ResolvePersonResponse::default().into());
        };
        let (who, service_account) = match &token {
            None => (Entity::User(req.subject.to_string()), false),
            Some(t) => match &t.kind {
                TokenKind::Personal { owner } => (Entity::User(owner.clone()), false),
                TokenKind::ServiceAccount { .. } => (Entity::Token(t.id.clone()), true),
            },
        };
        let t = self.tenancy().await?;
        let p = trust::person_of(&d.dir.issuer, &people, &t, operator, &who);
        let subject = match &who {
            Entity::User(u) | Entity::Token(u) => u.clone(),
            _ => String::new(),
        };
        Ok(pb::ResolvePersonResponse {
            found: p.found,
            disabled: p.disabled,
            display_name: p.display_name,
            grants: p.grants,
            operator: p.operator,
            directory: d.dir.id.clone(),
            subject,
            token_id: token.as_ref().map(|t| t.id.clone()).unwrap_or_default(),
            token_expires: token.as_ref().map_or(0, |t| t.expires_at),
            token_good: token.as_ref().is_some_and(|t| t.good_on(&cluster_id)),
            service_account,
            ..Default::default()
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_urls_and_realms_read_as_offers_need() {
        let pin = "AB:".repeat(31) + "AB";
        assert_eq!(normalize_pin(&pin), Some("ab".repeat(32)));
        assert_eq!(normalize_pin("ab"), None);
        assert_eq!(normalize_pin(&"zz".repeat(32)), None);
        assert!(valid_url("https://north.example:8852"));
        assert!(valid_url("https://north.example/"));
        assert!(!valid_url("http://north.example"));
        assert!(!valid_url("https://north.example/x"));
        assert!(!valid_url("https://u@north.example"));
        assert_eq!(realm_for("https://id.acme.example/d/acme"), "acme");
        assert_eq!(realm_for("https://ID.Uni.example"), "id-uni-example");
        assert_eq!(realm_for("https://id.example:8841/d/Lab_A"), "lab-a");
    }
}
