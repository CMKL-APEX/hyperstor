//! The space volumes use, through the API (20-tenants.md §24.4,
//! D335–D339): per volume, and summed per project and per tenant in each
//! pool, attributed when read so a moved project's counts go with it.

use super::{Api, meta_error};
use crate::space::{SpaceView, VolumeSpace};
use crate::tenancy::Tenancy;
use crate::{Admin, ClusterMap, VolumeState};
use connectrpc::ConnectError;
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_reactor::chan::oneshot;
use std::collections::BTreeMap;

impl Api {
    /// The leader's view of the space volumes use; another member answers
    /// `not_leader`.
    pub(super) async fn space(&self) -> Result<SpaceView, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Space(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        rx.recv()
            .await
            .ok_or_else(|| ConnectError::unavailable("map service stopped"))?
            .map_err(|e| meta_error(e, ConnectError::unavailable, "unavailable"))
    }
}

pub(super) fn state_name(s: VolumeState) -> &'static str {
    match s {
        VolumeState::Active => "active",
        VolumeState::Deleting => "deleting",
        VolumeState::Trashed => "trashed",
    }
}

/// A volume's bytes as (head, snapshot-only, trash): a volume in the
/// trash or being deleted counts every byte as trash until freed (D337).
fn split(v: &VolumeSpace) -> (u64, u64, u64) {
    match v.state {
        VolumeState::Active => (v.head_bytes, v.snapshot_bytes, 0),
        _ => (0, 0, v.head_bytes + v.snapshot_bytes),
    }
}

fn add(row: &mut pb::SpaceRow, v: &VolumeSpace) {
    let (h, s, t) = split(v);
    if v.state == VolumeState::Active {
        row.volumes += 1;
    }
    row.head_bytes += h;
    row.snapshot_bytes += s;
    row.trash_bytes += t;
    row.used_bytes += h + s + t;
}

/// The rows of `view` for the volumes `keep` passes (by their resolved
/// tenant and project), each volume's too if `volumes`, and the
/// unattributed bytes if `whole`.
pub(super) fn rows(
    view: &SpaceView,
    tenancy: &Tenancy,
    map: &ClusterMap,
    pool: u32,
    keep: impl Fn(u32, u32, u32) -> bool,
    whole: bool,
    volumes: bool,
) -> pb::GetSpaceResponse {
    let copies = |p: u32| map.pools.get(&p).map_or(0, |p| u32::from(p.size));
    let mut out = pb::GetSpaceResponse {
        complete: view.complete,
        ..Default::default()
    };
    let mut projects: BTreeMap<(u32, u32, u32), pb::SpaceRow> = BTreeMap::new();
    let mut tenants: BTreeMap<(u32, u32), pb::SpaceRow> = BTreeMap::new();
    for v in &view.volumes {
        if pool != 0 && v.pool != pool {
            continue;
        }
        let tenant = tenancy.tenant_of(v.tenant, v.project);
        let project = tenancy.project_of(tenant, v.project);
        if !keep(v.tenant, v.project, tenant) {
            continue;
        }
        let base = pb::SpaceRow {
            pool: v.pool,
            copies: copies(v.pool),
            tenant,
            tenant_name: if tenant == 0 {
                String::new()
            } else {
                tenancy.tenant_label(tenant)
            },
            ..Default::default()
        };
        if volumes {
            let mut row = pb::SpaceRow {
                project,
                project_name: tenancy.project_label(project),
                volume: format!("{:032x}", v.id),
                name: v.name.clone(),
                state: state_name(v.state).into(),
                ..base.clone()
            };
            add(&mut row, v);
            out.volumes.push(row);
        }
        add(
            projects
                .entry((v.pool, tenant, project))
                .or_insert_with(|| pb::SpaceRow {
                    project,
                    project_name: tenancy.project_label(project),
                    ..base.clone()
                }),
            v,
        );
        add(tenants.entry((v.pool, tenant)).or_insert(base), v);
    }
    out.projects = projects.into_values().collect();
    out.tenants = tenants.into_values().collect();
    if whole {
        out.unattributed = view
            .unattributed
            .iter()
            .filter(|(p, _)| pool == 0 || **p == pool)
            .map(|(&p, &b)| pb::SpaceRow {
                pool: p,
                copies: copies(p),
                used_bytes: b,
                ..Default::default()
            })
            .collect();
    }
    out
}

/// Each tenant's bytes over every pool, for the reports to glidex
/// directories (`volume_used_bytes`, D339): head, snapshot-only and trash
/// bytes together, one copy.
pub(super) fn used_by_tenant(view: &SpaceView, tenancy: &Tenancy) -> BTreeMap<u32, u64> {
    let mut out = BTreeMap::new();
    for v in &view.volumes {
        *out.entry(tenancy.tenant_of(v.tenant, v.project))
            .or_default() += used_bytes(v);
    }
    out
}

/// The bytes a volume uses, one copy, trash included (D339).
pub(super) fn used_bytes(v: &VolumeSpace) -> u64 {
    let (h, s, t) = split(v);
    h + s + t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(id: u128, tenant: u32, project: u32, state: VolumeState, h: u64, s: u64) -> VolumeSpace {
        VolumeSpace {
            id,
            pool: 1,
            tenant,
            project,
            state,
            name: format!("v{id}"),
            size: 1 << 30,
            head_bytes: h,
            snapshot_bytes: s,
        }
    }

    #[test]
    fn rows_sum_volumes_into_projects_and_tenants() {
        let view = SpaceView {
            volumes: vec![
                vol(1, 5, 0, VolumeState::Active, 100, 10),
                vol(2, 5, 0, VolumeState::Trashed, 7, 3),
                vol(3, 6, 0, VolumeState::Deleting, 1, 1),
            ],
            unattributed: [(1, 4096)].into(),
            complete: true,
        };
        let mut map = ClusterMap::default();
        map.pools.insert(1, crate::replicated_pool(1, "p", 3, 8));
        let t = Tenancy::default();
        let r = rows(&view, &t, &map, 0, |_, _, _| true, true, true);
        assert_eq!(r.volumes.len(), 3);
        assert_eq!(r.tenants.len(), 2);
        let t5 = &r.tenants[0];
        assert_eq!(
            (
                t5.tenant,
                t5.volumes,
                t5.head_bytes,
                t5.snapshot_bytes,
                t5.trash_bytes,
                t5.used_bytes
            ),
            (5, 1, 100, 10, 10, 120)
        );
        assert_eq!(t5.copies, 3);
        assert_eq!(r.unattributed[0].used_bytes, 4096);
        let only6 = rows(&view, &t, &map, 0, |t, _, _| t == 6, false, true);
        assert_eq!(only6.volumes.len(), 1);
        assert!(only6.unattributed.is_empty());
        // Without volume rows (skip_volumes, I413): the same sums.
        let sums = rows(&view, &t, &map, 0, |_, _, _| true, true, false);
        assert!(sums.volumes.is_empty());
        assert_eq!(sums.projects, r.projects);
        assert_eq!(sums.tenants, r.tenants);
        assert_eq!(sums.unattributed, r.unattributed);
        assert_eq!(used_by_tenant(&view, &t), [(5, 120), (6, 2)].into());
    }
}
