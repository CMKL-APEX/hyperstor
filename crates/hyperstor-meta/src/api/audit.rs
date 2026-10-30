//! `AuditService` (19-administration.md §23.9.4, 04-decisions.md D112, D113):
//! reading the audit log in `.meta`, hyperstor-mgmt's records joining it,
//! and how long it is kept.

use super::admin::invalid;
use super::{Api, Need, audit, volume_result};
use crate::Admin;
use crate::audit_log::Query;
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::AuditService;
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_reactor::chan::oneshot;

/// Lines in one append, at most.
const APPEND_MAX: usize = 1000;
/// The longest retention: ten years.
const DAYS_MAX: u32 = 3660;

fn record_pb(line: &str) -> pb::AuditRecord {
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    pb::AuditRecord {
        time_ms: (crate::audit_log::time_of(&v).unwrap_or(0.0) * 1000.0).round() as u64,
        principal: s("principal"),
        op: s("op"),
        target: s("target"),
        result: s("result"),
        ..Default::default()
    }
}

impl Api {
    fn audit_days(&self) -> u32 {
        self.admin_settings()
            .and_then(|s| s.audit_days().ok())
            .unwrap_or(crate::settings::DEFAULT_AUDIT_DAYS)
    }
}

#[allow(refining_impl_trait)]
impl AuditService for Api {
    async fn list_audit(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListAuditRequest>,
    ) -> ServiceResult<pb::ListAuditResponse> {
        // Who did what is for administrators: not storage nodes or hosts.
        self.authorize_as(&ctx, "ListAudit", "audit", Need::Write)?;
        let query = Query {
            since: req.since_unix,
            until: req.until_unix,
            principal: req.principal.to_string(),
            op: req.op.to_string(),
            limit: if req.limit == 0 {
                100
            } else {
                req.limit as usize
            },
            after: req.after.to_string(),
        };
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::AuditRead { query, reply })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let (lines, next) = volume_result(rx.recv().await)?;
        Ok(pb::ListAuditResponse {
            records: lines.iter().map(|l| record_pb(l)).collect(),
            next: next.unwrap_or_default(),
            ..Default::default()
        }
        .into())
    }

    async fn append_audit(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::AppendAuditRequest>,
    ) -> ServiceResult<pb::AppendAuditResponse> {
        self.authorize_as(&ctx, "AppendAudit", "audit", Need::Mgmt)?;
        if req.lines.len() > APPEND_MAX {
            return Err(invalid(
                format!("at most {APPEND_MAX} lines at a time"),
                "invalid_argument",
            ));
        }
        for l in req.lines.iter() {
            crate::audit_log::append(l)
                .map_err(|e| invalid(format!("not an audit record: {e}"), "invalid_argument"))?;
        }
        Ok(pb::AppendAuditResponse::default().into())
    }

    async fn get_audit_settings(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetAuditSettingsRequest>,
    ) -> ServiceResult<pb::GetAuditSettingsResponse> {
        self.authorize_as(&ctx, "GetAuditSettings", "audit", Need::Write)?;
        Ok(pb::GetAuditSettingsResponse {
            settings: Some(pb::AuditSettings {
                retention_days: self.audit_days(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
        .into())
    }

    async fn set_audit_settings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetAuditSettingsRequest>,
    ) -> ServiceResult<pb::SetAuditSettingsResponse> {
        let days = req.retention_days;
        let who = self.authorize(&ctx, "SetAuditSettings", "audit", true)?;
        if days > DAYS_MAX {
            return Err(invalid(
                format!("at most {DAYS_MAX} days (0 keeps forever)"),
                "invalid_argument",
            ));
        }
        let store = self.settings_store()?;
        let r = tokio::task::spawn_blocking(move || store.set_audit_days(days))
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))?;
        audit(
            &who,
            "SetAuditSettings",
            "audit",
            &match &r {
                Ok(()) => format!("ok, {days} days"),
                Err(e) => format!("failed: {e:?}"),
            },
        );
        if let Err(e) = r {
            return Err(self.store_error(e).await);
        }
        Ok(pb::SetAuditSettingsResponse {
            settings: Some(pb::AuditSettings {
                retention_days: days,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
        .into())
    }
}
