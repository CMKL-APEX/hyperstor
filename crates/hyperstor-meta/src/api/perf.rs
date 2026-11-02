//! The load and drives in the cluster API (27-performance.md §30, D218–D222):
//! the leader's [`crate::perf::Perf`] as `GetPgHeat`, `ListDevicePerf`,
//! `ListTopVolumes` and `ListFindings` answer it.

use super::{Api, meta_error};
use crate::perf::{Context, Finding, Perf, Sample, Severity, VolumeKey, VolumeName, findings};
use crate::{Admin, ClusterMap};
use connectrpc::ConnectError;
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_proto::perf::{Drive, Service};
use hyperstor_reactor::chan::oneshot;
use std::collections::{BTreeMap, HashMap};

/// Volumes `ListTopVolumes` gives by default.
const TOP: usize = 20;

impl Api {
    /// The leader's load and drives, with the published map.
    pub(super) async fn perf_view(&self) -> Result<(Perf, ClusterMap), ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Perf(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        rx.recv()
            .await
            .ok_or_else(|| ConnectError::unavailable("map service stopped"))?
            .map_err(|e| meta_error(e, ConnectError::unavailable, "unavailable"))
    }

    /// The volumes behind (lineage, branch) pairs: id, name, pool.
    pub(super) async fn volumes_of(
        &self,
        keys: Vec<(u128, u32)>,
    ) -> HashMap<VolumeKey, VolumeName> {
        if keys.is_empty() {
            return HashMap::new();
        }
        let (reply, rx) = oneshot();
        if self.tx.send(Admin::VolumesOf { keys, reply }).is_err() {
            return HashMap::new();
        }
        rx.recv().await.unwrap_or_default().into_iter().collect()
    }

    /// `ListFindings`: the Advisor over the leader's view.
    pub(super) async fn advise(&self) -> Result<Vec<Finding>, ConnectError> {
        let (p, map) = self.perf_view().await?;
        let keys = p.top_volumes(5).iter().map(|v| (v.0, v.1)).collect();
        let volumes = self.volumes_of(keys).await;
        let kinds = kinds(&map);
        let pools = pools(&map);
        Ok(findings(
            &p,
            &Context {
                kinds: &kinds,
                volumes: &volumes,
                pools: &pools,
            },
        ))
    }
}

/// Each device's kind, by name.
pub(super) fn kinds(map: &ClusterMap) -> BTreeMap<u32, String> {
    map.devices
        .keys()
        .map(|d| {
            let k = map
                .kinds
                .get(d)
                .copied()
                .unwrap_or(hyperstor_map::DeviceKind::File);
            (*d, k.name().to_string())
        })
        .collect()
}

/// Data pools, and whether every device each uses is NVMe.
pub(super) fn pools(map: &ClusterMap) -> Vec<(u32, String, bool)> {
    map.pools
        .values()
        .filter(|p| p.id != hyperstor_map::META_POOL)
        .map(|p| {
            let mut used = map.devices.values().filter(|d| p.uses(d)).peekable();
            let any = used.peek().is_some();
            let nvme =
                any && used.all(|d| map.kinds.get(&d.id) == Some(&hyperstor_map::DeviceKind::Nvme));
            (p.id, p.name.clone(), nvme)
        })
        .collect()
}

/// `GetPgHeat`'s answer for `pool`.
pub(super) fn pg_heat(
    p: &Perf,
    map: &ClusterMap,
    pool: u32,
) -> Result<pb::GetPgHeatResponse, ConnectError> {
    let Some(info) = map.pools.get(&pool) else {
        return Err(super::with_reason(
            ConnectError::not_found(format!("no pool {pool}")),
            "not_found",
        ));
    };
    let n = info.pg_count as usize;
    let mut r = pb::GetPgHeatResponse {
        read_ops: vec![0.0; n],
        write_ops: vec![0.0; n],
        read_bytes: vec![0.0; n],
        write_bytes: vec![0.0; n],
        primaries: vec![u32::MAX; n],
        ..Default::default()
    };
    for w in p.pool(pool) {
        let i = w.heat.pg as usize;
        if i >= n {
            continue;
        }
        r.read_ops[i] = w.rate(u64::from(w.heat.reads)) as f32;
        r.write_ops[i] = w.rate(u64::from(w.heat.writes)) as f32;
        r.read_bytes[i] = w.rate(w.heat.read_bytes) as f32;
        r.write_bytes[i] = w.rate(w.heat.write_bytes) as f32;
        r.primaries[i] = w.device;
    }
    Ok(r)
}

/// PG `pg`'s top volumes, by (lineage, branch), with their rates.
pub(super) fn pg_top(p: &Perf, pool: u32, pg: u32) -> Vec<((u128, u32), f64, f64)> {
    p.pool(pool)
        .into_iter()
        .find(|w| w.heat.pg == pg)
        .map(|w| {
            w.heat
                .top
                .iter()
                .map(|(l, b, n, by)| ((*l, *b), w.rate(u64::from(*n)), w.rate(*by)))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn volume_heat(
    key: (u128, u32),
    ops: f64,
    bytes: f64,
    names: &HashMap<VolumeKey, VolumeName>,
) -> pb::VolumeHeat {
    let (id, name, pool) = match names.get(&key) {
        Some((id, n, p)) => (format!("{id:032x}"), n.clone(), *p),
        None => (
            String::new(),
            format!("lineage {:032x} branch {}", key.0, key.1),
            0,
        ),
    };
    pb::VolumeHeat {
        id,
        name,
        pool,
        ops: ops as f32,
        bytes: bytes as f32,
        ..Default::default()
    }
}

fn service(s: &Service) -> pb::ServiceTime {
    pb::ServiceTime {
        count: s.count,
        bytes: s.bytes,
        p50_us: s.quantile_us(0.5),
        p99_us: s.quantile_us(0.99),
        max_us: s.max_us,
        buckets: s.buckets.to_vec(),
        ..Default::default()
    }
}

fn sample(s: &Sample) -> pb::PerfSample {
    pb::PerfSample {
        unix_ms: s.unix_ms,
        window_ms: s.window_ms,
        read_ops: s.per_sec(u64::from(s.reads)) as f32,
        write_ops: s.per_sec(u64::from(s.writes)) as f32,
        read_bytes: s.per_sec(s.read_bytes) as f32,
        write_bytes: s.per_sec(s.write_bytes) as f32,
        reads: buffa::MessageField::some(service(&s.service[0])),
        commits: buffa::MessageField::some(service(&s.service[1])),
        inflight_max: s.inflight_max,
        ..Default::default()
    }
}

fn kelvin(k: u16) -> f32 {
    if k == 0 { 0.0 } else { f32::from(k) - 273.15 }
}

fn drive(d: &Drive) -> pb::Drive {
    pb::Drive {
        model: d.model.clone(),
        firmware: d.firmware.clone(),
        temperature_c: d.temperature_mc.map(|t| t as f32 / 1000.0),
        link_width: d.link.map(|l| u32::from(l.0)),
        link_width_max: d.link.map(|l| u32::from(l.1)),
        link_speed: d.link.map(|l| f32::from(l.2) / 10.0),
        link_speed_max: d.link.map(|l| f32::from(l.3) / 10.0),
        numa_node: d.numa.0,
        reactor_numa_node: d.numa.1,
        write_cache: d.write_cache,
        smart: match &d.smart {
            None => buffa::MessageField::none(),
            Some(s) => buffa::MessageField::some(pb::Smart {
                critical_warning: u32::from(s.critical_warning),
                temperature_c: kelvin(s.temperature_k),
                available_spare: u32::from(s.available_spare),
                spare_threshold: u32::from(s.spare_threshold),
                percentage_used: u32::from(s.percentage_used),
                bytes_read: s.data_units_read.saturating_mul(512_000),
                bytes_written: s.data_units_written.saturating_mul(512_000),
                power_on_hours: s.power_on_hours,
                unsafe_shutdowns: s.unsafe_shutdowns,
                media_errors: s.media_errors,
                warning_temp_minutes: s.warning_temp_minutes,
                critical_temp_minutes: s.critical_temp_minutes,
                throttle_seconds: s.throttle_seconds.to_vec(),
                warning_threshold_c: kelvin(s.warning_threshold_k),
                critical_threshold_c: kelvin(s.critical_threshold_k),
                ..Default::default()
            }),
        },
        smart_note: d.smart_note.clone(),
        ..Default::default()
    }
}

/// `ListDevicePerf`'s answer.
pub(super) fn device_perf(
    p: &Perf,
    map: &ClusterMap,
    only: Option<u32>,
) -> pb::ListDevicePerfResponse {
    let kinds = kinds(map);
    pb::ListDevicePerfResponse {
        devices: p
            .devices()
            .into_iter()
            .filter(|(d, ..)| only.is_none_or(|o| o == *d))
            .map(|(d, samples, dr)| pb::DevicePerf {
                device: d,
                node: map.devices.get(&d).map_or(d / 1000, |x| x.node),
                kind: kinds.get(&d).cloned().unwrap_or_else(|| "file".into()),
                samples: samples.iter().map(sample).collect(),
                drive: match &dr {
                    Some(x) => buffa::MessageField::some(drive(x)),
                    None => buffa::MessageField::none(),
                },
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

pub(super) fn finding_pb(f: &Finding) -> pb::Finding {
    pb::Finding {
        rule: f.rule.into(),
        severity: match f.severity {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
        .into(),
        subject: f.subject.clone(),
        title: f.title.clone(),
        evidence: f.evidence.clone(),
        action: f.action.clone(),
        command: f.command.clone(),
        ..Default::default()
    }
}

pub(super) fn top_limit(limit: u32) -> usize {
    if limit == 0 { TOP } else { limit as usize }
}
