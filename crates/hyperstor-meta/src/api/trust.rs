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
//! - **The feed** is pulled by the leader's mgmt (D306), signed with the
//!   cluster key towards directories, which this service hands mgmt alone:
//!   an ES256 key made on first use and kept sealed with the cluster key
//!   (D162). mgmt hands back each snapshot; it becomes the replica, and its
//!   tenants and projects the records here ([`crate::trust::plan`]): made,
//!   renamed, moved (as MoveProject's operation) and dropped. What can't be
//!   done is kept, shown and retried at the next pull.
//! - **Reports** count each admitted tenant's volumes, their provisioned
//!   size, buckets and their bytes, and S3 keys (glidex D85).

use super::{Api, Need, with_reason};
use crate::tenancy::{self, NewTenant, Tenancy};
use crate::trust::{self, DirRec, OfferRec, Step};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use glidex_trust::cluster::{self as gt_cluster, Offer};
use glidex_trust::{EndAtCluster, OfferToken, RedeemAnswer, RedeemRequest, Snapshot};
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
                tenant: &o.tenant,
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
        // The tenant: the one linked to this glidex tenant, else a new
        // one by the offer's name.
        let t = self.tenancy().await.map_err(fail)?;
        if !t
            .tenants
            .iter()
            .any(|x| x.directory == checked.issuer && x.glidex_id == b.tenant.id)
        {
            let n = NewTenant {
                name: offer.name.clone(),
                id: None,
                directory: checked.issuer.clone(),
                glidex_id: b.tenant.id.clone(),
                created_unix: now,
                created_by: format!("directory {}", checked.issuer),
            };
            let in_use = self.bare_tenants().await.map_err(fail)?;
            self.realm_rows("tenant", move |db| {
                let (id, _) = tenancy::create_tenant(db, &n, &in_use)?;
                crate::realms::set_tenant(db, id, realm, &[])
            })
            .await
            .map_err(fail)?;
            self.sync_realms().await.map_err(fail)?;
        }
        let d = DirRec {
            dir: gt_cluster::redeemed_directory(existing.map(|e| e.dir), b, &checked, now),
            realm,
        };
        self.realm_rows("directory", move |db| trust::put_directory(db, &d))
            .await
            .map_err(fail)?;
        Ok(RedeemAnswer {
            cluster_id,
            cluster_key: glidex_trust::key::jwks(&[&key]),
            ui_callback: None,
        })
    }
}

#[allow(refining_impl_trait)]
impl TrustService for Api {
    async fn create_offer(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateOfferRequest>,
    ) -> ServiceResult<pb::CreateOfferResponse> {
        let target = format!("tenant {} of {}", req.tenant, req.directory);
        let who = self.authorize(&ctx, "CreateOffer", &target, true)?;
        let r = async {
            let cluster_id = self.cluster_id()?;
            let slug = req.tenant.trim().to_string();
            if slug.is_empty()
                || slug.len() > 63
                || slug.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(invalid("a tenant's slug is 1 to 63 characters, no spaces"));
            }
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
            let Some(pin) = normalize_pin(req.pin) else {
                return Err(invalid(
                    "a certificate's SHA-256 is 64 hex digits (hyperstorctl reads it from --url)",
                ));
            };
            let name = if req.name.is_empty() {
                trust::name_of(&slug)
            } else {
                req.name.to_string()
            };
            if !tenancy::valid_tenant_name(&name) {
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
            if let Some(x) = t.tenant_named(&name)
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
            };
            let row = o.clone();
            self.realm_rows("offer", move |db| trust::create_offer(db, &row))
                .await?;
            let token = OfferToken {
                cluster_id: cluster_id.clone(),
                cluster_name: cluster_id,
                api: o.url.clone(),
                tls_sha256: o.pin.clone(),
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
                    .map(|x| {
                        let here = t
                            .tenants
                            .iter()
                            .find(|y| y.directory == d.dir.issuer && y.glidex_id == x.id);
                        pb::TrustedTenant {
                            id: x.id.clone(),
                            slug: x.slug.clone(),
                            tenant: here.map_or(0, |y| y.id),
                            name: here.map(|y| y.name.clone()).unwrap_or_default(),
                            operator: here.is_some_and(|y| y.id == operator),
                            ..Default::default()
                        }
                    })
                    .collect(),
                revision: f.state.revision,
                applied_unix: f.state.applied_at,
                checked_unix: f.state.checked_at,
                blocked: f.blocked,
                created_unix: d.dir.created_at,
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
        let target = format!("tenant {} of {}", req.tenant, req.directory);
        let who = self.authorize(&ctx, "EndTrust", &target, true)?;
        let r = async {
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
            let Some(x) = d
                .dir
                .tenants
                .iter()
                .find(|x| x.id == req.tenant || x.slug == req.tenant)
            else {
                return Err(with_reason(
                    ConnectError::not_found(format!(
                        "{} has no tenant {} admitted here",
                        d.dir.issuer, req.tenant
                    )),
                    "not_found",
                ));
            };
            let (id, gid) = (d.dir.id.clone(), x.id.clone());
            self.realm_rows("directory", move |db| {
                trust::end_tenant(db, &id, &gid, true).map(|_| ())
            })
            .await?;
            self.unlink_tenant(&d.dir.issuer, &x.id).await
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
                let target = format!(
                    "offer {} by {} for tenant {}",
                    b.offer, b.issuer, b.tenant.slug
                );
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
        let target = format!("tenant {} of {}", b.tenant, b.issuer);
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
                let (id, tenant) = (d.id.clone(), b.tenant.clone());
                self.realm_rows("directory", move |db| {
                    trust::end_tenant(db, &id, &tenant, false).map(|_| ())
                })
                .await?;
                let r = self.unlink_tenant(&d.issuer, &b.tenant).await;
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
        let report = glidex_trust::ClusterReport {
            at: super::admin::now_unix(),
            tenants: here
                .iter()
                .map(|(gid, id)| glidex_trust::StorageTenantReport {
                    tenant: gid.clone(),
                    suspended: tenants.iter().any(|x| &x.id == gid && x.suspended),
                    storage: counts.remove(id).unwrap_or_default(),
                })
                .collect(),
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
