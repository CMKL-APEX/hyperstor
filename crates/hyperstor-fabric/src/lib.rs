//! Hyperstor on hosts with several network ports (spec/26-networks.md
//! §29): networks and ports' roles, each reactor's endpoints ranked by
//! closeness to its device, the choice of a path per I/O, and the fabric
//! reconciler's decisions. Hosts are read through glidex's topology crate
//! (D204).

pub mod endpoints;
pub mod model;
pub mod path;
pub mod reconcile;

#[cfg(test)]
mod tests;
