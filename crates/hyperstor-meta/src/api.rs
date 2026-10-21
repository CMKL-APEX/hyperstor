//! The cluster API, `hyperstor.v1.ClusterService` (spec/administration.md
//! §23.5, decisions.md D51), served with connectrpc on its own thread so
//! the control path never runs on the map service's reactor (D58).
//!
//! The map service's state is `Rc` on its reactor; every call is a
//! request on a cross-thread channel to [`Meta::admin`](crate::Meta::admin)
//! with a one-shot reply. Reads answer from the published map, writes
//! change the working map, which the publisher fences and publishes.
//!
//! Not yet: TLS and identities (D53, D56). Until then the API is plain
//! HTTP and unauthenticated, so `hyperstor-install` binds it to loopback.

use crate::health::{self, Health};
use crate::{Admin, ClusterMap, NO_DEVICE, PoolKind};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{ClusterService, ClusterServiceExt};
use hyperstor_placement::Domain;
use hyperstor_reactor::chan::{Sender, oneshot};
use std::net::SocketAddr;
use std::sync::Arc;

struct Api {
    tx: Sender<Admin>,
    map_address: String,
}

impl Api {
    async fn published(&self) -> Result<ClusterMap, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Published(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        rx.recv()
            .await
            .ok_or_else(|| ConnectError::unavailable("map service stopped"))
    }
}

fn health_pb(h: Health) -> pb::Health {
    match h {
        Health::Ok => pb::Health::HEALTH_OK,
        Health::Warn => pb::Health::HEALTH_WARN,
        Health::Error => pb::Health::HEALTH_ERROR,
    }
}

fn kind(k: PoolKind) -> String {
    match k {
        PoolKind::Replicated => "replicated".into(),
        PoolKind::Ec { k, m } => format!("ec{k}+{m}"),
        PoolKind::Cache => "cache".into(),
    }
}

fn domain(d: Domain) -> String {
    match d {
        Domain::Device => "device",
        Domain::Host => "host",
        Domain::Rack => "rack",
    }
    .into()
}

/// The status of `map`, as the API answers it.
pub fn status(map: &ClusterMap, map_address: &str) -> pb::GetStatusResponse {
    let r = health::report(map);
    pb::GetStatusResponse {
        cluster_id: map.cluster_id.to_string(),
        epoch: map.epoch,
        health: health_pb(r.health).into(),
        reasons: r
            .reasons
            .iter()
            .map(|(h, text)| pb::Reason {
                level: health_pb(*h).into(),
                text: text.clone(),
                ..Default::default()
            })
            .collect(),
        nodes: map
            .nodes
            .values()
            .map(|n| pb::Node {
                id: n.id,
                rack: n.rack,
                up: n.up,
                devices: map
                    .devices
                    .values()
                    .filter(|d| d.node == n.id)
                    .map(|d| pb::Device {
                        id: d.id,
                        addr: d.addr.to_string(),
                        up_in: d.up_in,
                        weight: d.weight,
                        class: d.class.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect(),
        pools: map
            .pools
            .values()
            .zip(&r.pools)
            .map(|(p, s)| pb::Pool {
                id: p.id,
                name: p.name.clone(),
                kind: kind(p.kind),
                size: p.size.into(),
                min_size: p.min_size.into(),
                pg_count: p.pg_count,
                chunk_size: p.chunk_size,
                domain: domain(p.domain),
                pgs: pb::PgCounts {
                    clean: s.clean,
                    degraded: s.degraded,
                    backfilling: s.backfilling,
                    inactive: s.inactive,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .collect(),
        map_address: map_address.to_string(),
        ..Default::default()
    }
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl ClusterService for Api {
    async fn get_status(
        &self,
        _ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetStatusRequest>,
    ) -> ServiceResult<pb::GetStatusResponse> {
        let map = self.published().await?;
        Ok(status(&map, &self.map_address).into())
    }

    async fn list_pgs(
        &self,
        _ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListPgsRequest>,
    ) -> ServiceResult<pb::ListPgsResponse> {
        let pool = req.pool;
        let map = self.published().await?;
        if !map.pools.contains_key(&pool) {
            return Err(ConnectError::not_found(format!("no pool {pool}")));
        }
        let pgs = map
            .pgs
            .range((pool, 0)..=(pool, u32::MAX))
            .map(|((pool, pg), info)| pb::Pg {
                pool: *pool,
                pg: *pg,
                epoch: info.epoch,
                acting: info.acting.clone(),
                up: info.up.clone(),
                ..Default::default()
            })
            .collect();
        debug_assert_ne!(NO_DEVICE, 0);
        Ok(pb::ListPgsResponse {
            pgs,
            ..Default::default()
        }
        .into())
    }

    async fn set_device_in(
        &self,
        _ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDeviceInRequest>,
    ) -> ServiceResult<pb::SetDeviceInResponse> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SetDeviceIn {
                device: req.device,
                up_in: req.up_in,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        match rx.recv().await {
            Some(Ok(map_epoch)) => Ok(pb::SetDeviceInResponse {
                map_epoch,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) => Err(ConnectError::not_found(e)),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }
}

/// Serves the cluster API on `listener` from a new thread with its own
/// single-threaded tokio runtime. `map_address` is reported to clients
/// as where the map service listens.
pub fn spawn(
    listener: std::net::TcpListener,
    tx: Sender<Admin>,
    map_address: SocketAddr,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hyperstor-api".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                listener.set_nonblocking(true).expect("nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                let api = Arc::new(Api {
                    tx,
                    map_address: map_address.to_string(),
                });
                let router = api.register(connectrpc::Router::new());
                if let Err(e) = connectrpc::Server::from_listener(listener)
                    .serve(router)
                    .await
                {
                    eprintln!("hyperstor-meta: cluster API: {e}");
                    std::process::exit(1);
                }
            })
        })
        .expect("spawn the API thread")
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_carries_health_and_pools() {
        let m = crate::parse_cluster(
            "cluster 5\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             pool 1 vol replicated size 2 min 1 pgs 4 domain host\n",
        )
        .unwrap();
        let s = super::status(&m, "127.0.0.1:8849");
        assert_eq!(s.cluster_id, "5");
        assert_eq!(s.health, super::pb::Health::HEALTH_OK);
        assert_eq!(s.nodes.len(), 2);
        assert_eq!(s.pools[0].pgs.clean, 4);
        assert_eq!(s.pools[0].kind, "replicated");
        assert_eq!(s.map_address, "127.0.0.1:8849");
    }
}
