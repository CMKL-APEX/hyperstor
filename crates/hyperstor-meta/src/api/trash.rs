//! `TrashService` (administration.md §23.9.3, decisions.md D54, D115):
//! what's in the trash, restoring, purging at once, and each pool's grace
//! period. Volume work runs on the map service's reactor as a client of
//! `.meta` ([`crate::TrashOp`]); settings are rows of the replicated store.

use super::admin::invalid;
use super::{Api, audit, parse_volume_id, volume_pb, volume_result, with_reason};
use crate::settings::{OnFull, Trash};
use crate::{Admin, TrashDone, TrashOp};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::TrashService;
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_client::trash::Trashed;
use hyperstor_reactor::chan::oneshot;

/// The longest grace period: a year.
const GRACE_MAX: u64 = 366 * 86400;

impl Api {
    async fn trash_op(&self, op: TrashOp) -> Result<TrashDone, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Trash { op, reply })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        volume_result(rx.recv().await)
    }

    /// When something trashed at `at` in `pool` is purged.
    pub(super) fn purge_at(&self, pool: u32, at: u64) -> u64 {
        at.saturating_add(self.trash_settings(pool).grace_secs)
    }

    fn settings_pb(pool: u32, t: Trash) -> pb::TrashSettings {
        pb::TrashSettings {
            pool,
            grace_seconds: t.grace_secs,
            on_full: t.on_full.as_str().into(),
            ..Default::default()
        }
    }
}

fn item_pb(api: &Api, t: &Trashed) -> pb::TrashItem {
    let (kind, v, epoch, name, group) = match t {
        Trashed::Volume(v) => ("volume", v, 0, v.name.clone(), String::new()),
        Trashed::Snapshot(v, s) => (
            "snapshot",
            v,
            s.epoch,
            s.name.clone(),
            if s.group == 0 {
                String::new()
            } else {
                format!("{:032x}", s.group)
            },
        ),
    };
    pb::TrashItem {
        kind: kind.into(),
        id: format!("{:032x}", v.id),
        epoch,
        name,
        pool: v.pool,
        size: v.size,
        trashed_unix: t.trashed_unix(),
        purge_unix: api.purge_at(v.pool, t.trashed_unix()),
        group,
        ..Default::default()
    }
}

#[allow(refining_impl_trait)]
impl TrashService for Api {
    async fn list_trash(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListTrashRequest>,
    ) -> ServiceResult<pb::ListTrashResponse> {
        self.authorize(&ctx, "ListTrash", "trash", false)?;
        let pool = (req.pool != 0).then_some(req.pool);
        let TrashDone::Items(items) = self.trash_op(TrashOp::List { pool }).await? else {
            return Err(ConnectError::internal("unexpected result"));
        };
        Ok(pb::ListTrashResponse {
            items: items.iter().map(|t| item_pb(self, t)).collect(),
            ..Default::default()
        }
        .into())
    }

    async fn restore_from_trash(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RestoreFromTrashRequest>,
    ) -> ServiceResult<pb::RestoreFromTrashResponse> {
        let id = parse_volume_id(req.id)?;
        let epoch = req.epoch;
        let target = match epoch {
            Some(e) => format!("volume {id:032x} snapshot {e}"),
            None => format!("volume {id:032x}"),
        };
        let who = self.authorize(&ctx, "RestoreFromTrash", &target, true)?;
        let name = (!req.name.is_empty()).then(|| req.name.to_string());
        let r = self.trash_op(TrashOp::Restore { id, epoch, name }).await;
        audit(
            &who,
            "RestoreFromTrash",
            &target,
            &match &r {
                Ok(TrashDone::Volume(v)) => format!("ok, as {:?}", v.name),
                Ok(_) => "ok".into(),
                Err(e) => format!("failed: {}", e.message.as_deref().unwrap_or("")),
            },
        );
        let volume = match r? {
            TrashDone::Volume(v) => Some(volume_pb(&v)),
            _ => None,
        };
        Ok(pb::RestoreFromTrashResponse {
            volume: volume.into(),
            ..Default::default()
        }
        .into())
    }

    async fn purge_from_trash(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::PurgeFromTrashRequest>,
    ) -> ServiceResult<pb::PurgeFromTrashResponse> {
        let id = parse_volume_id(req.id)?;
        let epoch = req.epoch;
        let target = super::ops::purge_target(id, epoch);
        let who = self.authorize(&ctx, "PurgeFromTrash", &target, true)?;
        // Checked now, run as an operation (D121): it deletes every chunk.
        let v = self.volume_any(id).await?;
        let refused = |e: String| with_reason(ConnectError::failed_precondition(e), "in_use");
        match epoch {
            None if v.state == crate::VolumeState::Active => {
                return Err(refused(format!("volume {id:032x} isn't in the trash")));
            }
            None => {}
            Some(e) => match v.snapshot(e) {
                None => {
                    return Err(with_reason(
                        ConnectError::not_found(format!("no snapshot at epoch {e}")),
                        "not_found",
                    ));
                }
                Some(s) if s.trashed_unix == 0 && !s.deleting => {
                    return Err(refused(format!(
                        "the snapshot at epoch {e} isn't in the trash"
                    )));
                }
                Some(_) => {}
            },
        }
        let op = self
            .start_op(
                &who,
                "PurgeFromTrash",
                crate::operations::Work::Purge { volume: id, epoch },
                target,
                "deleting its data".into(),
            )
            .await?;
        Ok(pb::PurgeFromTrashResponse {
            operation: buffa::MessageField::some(op),
            ..Default::default()
        }
        .into())
    }

    async fn get_trash_settings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetTrashSettingsRequest>,
    ) -> ServiceResult<pb::GetTrashSettingsResponse> {
        self.authorize(&ctx, "GetTrashSettings", "trash", false)?;
        Ok(pb::GetTrashSettingsResponse {
            settings: Some(Api::settings_pb(req.pool, self.trash_settings(req.pool))).into(),
            ..Default::default()
        }
        .into())
    }

    async fn set_trash_settings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetTrashSettingsRequest>,
    ) -> ServiceResult<pb::SetTrashSettingsResponse> {
        let pool = req.pool;
        let target = format!("pool {pool} trash");
        let who = self.authorize(&ctx, "SetTrashSettings", &target, true)?;
        if pool == crate::META_POOL {
            return Err(invalid(
                "the .meta pool has no trash: it holds no volumes",
                "invalid_argument",
            ));
        }
        if req.grace_seconds > GRACE_MAX {
            return Err(invalid(
                format!("the grace period is at most {GRACE_MAX} seconds"),
                "invalid_argument",
            ));
        }
        let on_full = if req.on_full.is_empty() {
            self.trash_settings(pool).on_full
        } else {
            OnFull::parse(req.on_full).ok_or_else(|| {
                invalid(
                    format!("{}: on_full is keep or purge", req.on_full),
                    "invalid_argument",
                )
            })?
        };
        let t = Trash {
            grace_secs: req.grace_seconds,
            on_full,
        };
        let store = self.settings_store()?;
        let r = tokio::task::spawn_blocking(move || store.set_trash(pool, t))
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))?;
        audit(
            &who,
            "SetTrashSettings",
            &target,
            &match &r {
                Ok(()) => format!("ok, {} s, {}", t.grace_secs, t.on_full.as_str()),
                Err(e) => format!("failed: {e:?}"),
            },
        );
        if let Err(e) = r {
            return Err(self.store_error(e).await);
        }
        Ok(pb::SetTrashSettingsResponse {
            settings: Some(Api::settings_pb(pool, t)).into(),
            ..Default::default()
        }
        .into())
    }
}
