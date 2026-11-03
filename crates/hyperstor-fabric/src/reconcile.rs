//! The fabric reconciler's decisions (D203, D208, D209;
//! spec/26-networks.md §29.8): which ports storage may use, lent
//! `compute` ports and their revocation, and each port's weight.
//!
//! [`reconcile`] is pure: what it sees, the policy, the claims and the
//! current state in, changes out. [`step`] drives it through [`Inputs`]
//! and [`Store`], which the map service's leader implements over the map;
//! a fabric service of its own would implement them over the cluster API.

use crate::model::{Lending, Role};
use std::collections::BTreeMap;

/// A port, as `host/netdev`.
pub type PortId = String;

/// What the reconciler sees of a port, every sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observed {
    /// The role the host's owner gave it.
    pub role: Role,
    pub up: bool,
    /// Link speed, bytes/s.
    pub speed: u64,
    /// Traffic that isn't hyperstor's, bytes/s: the port's counters less
    /// hyperstor's own.
    pub other: u64,
}

/// A claim by an orchestrator (D205), live until `expires` (ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub holder: String,
    /// Exclusive: storage must leave the port. Shared: storage may stay,
    /// in its traffic class.
    pub exclusive: bool,
    pub expires: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub lending: Lending,
    /// The least time between two role changes of a port, and how long a
    /// port must be idle before it is lent (ms).
    pub dwell_ms: u64,
    /// A weight moves only when the new one differs by more than this
    /// fraction.
    pub hysteresis: f64,
    /// Other traffic under this fraction of the link counts as idle.
    pub idle_fraction: f64,
    /// How long senders have to drain a revoked port (ms).
    pub revoke_ms: u64,
}

impl Default for Policy {
    /// The starting values of §29.8, for G52 to tune.
    fn default() -> Policy {
        Policy {
            lending: Lending::Idle,
            dwell_ms: 60_000,
            hysteresis: 0.2,
            idle_fraction: 0.05,
            revoke_ms: 30_000,
        }
    }
}

/// The reconciler's record of a port: replicated state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PortState {
    pub role: Option<Role>,
    /// A `compute` port lent to storage.
    pub lent: bool,
    /// Lent, and being taken back: senders stop opening requests on it and
    /// it returns at this time (ms).
    pub revoking: Option<u64>,
    /// Senders' weight for it; 0 takes it out.
    pub weight: f64,
    /// The last lend or return (ms).
    pub last_change: Option<u64>,
    /// Idle since (ms).
    pub idle_since: Option<u64>,
}

impl PortState {
    /// Storage may open new requests on it.
    pub fn eligible(&self) -> bool {
        self.weight > 0.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A new port, or its role changed.
    Role {
        port: PortId,
        role: Role,
    },
    /// Idle since, or not idle.
    Idle {
        port: PortId,
        since: Option<u64>,
    },
    Lend {
        port: PortId,
        at: u64,
    },
    Revoke {
        port: PortId,
        until: u64,
    },
    Return {
        port: PortId,
        at: u64,
    },
    Weight {
        port: PortId,
        weight: f64,
    },
}

/// Weight for a port storage may use: the bandwidth other traffic leaves
/// it, in GB/s, at least a tenth of the link.
fn weight_of(o: &Observed) -> f64 {
    let free = o.speed.saturating_sub(o.other).max(o.speed / 10);
    free as f64 / 1e9
}

/// The changes that bring `state` in line with what is observed, the
/// claims and the policy, at `now` (ms).
pub fn reconcile(
    now: u64,
    policy: &Policy,
    observed: &BTreeMap<PortId, Observed>,
    claims: &BTreeMap<PortId, Claim>,
    state: &BTreeMap<PortId, PortState>,
) -> Vec<Change> {
    let mut out = Vec::new();
    let empty = PortState::default();
    for (port, o) in observed {
        let s = state.get(port).unwrap_or(&empty);
        let p = || port.clone();
        if s.role != Some(o.role) {
            out.push(Change::Role {
                port: p(),
                role: o.role,
            });
        }
        let claim = claims.get(port).filter(|c| c.expires > now);
        let exclusive = claim.is_some_and(|c| c.exclusive);
        let idle = o.up && (o.other as f64) <= policy.idle_fraction * o.speed as f64;
        let idle_since = if idle {
            s.idle_since.or(Some(now))
        } else {
            None
        };
        if idle_since != s.idle_since {
            out.push(Change::Idle {
                port: p(),
                since: idle_since,
            });
        }
        let settled = s
            .last_change
            .is_none_or(|t| now.saturating_sub(t) >= policy.dwell_ms);

        // Lending: only `compute` ports.
        let mut lent = s.lent && o.role == Role::Compute;
        let mut revoking = s.revoking;
        if lent {
            let keep = o.up
                && claim.is_none()
                && match policy.lending {
                    Lending::Off => false,
                    Lending::Idle => idle,
                    Lending::Always => true,
                };
            match revoking {
                // Taken back at once, whatever the dwell: a claim or traffic
                // always wins.
                None if !keep => {
                    let until = now + policy.revoke_ms;
                    out.push(Change::Revoke { port: p(), until });
                    revoking = Some(until);
                }
                Some(until) if now >= until => {
                    out.push(Change::Return { port: p(), at: now });
                    lent = false;
                    revoking = None;
                }
                _ => {}
            }
        } else if o.role == Role::Compute && o.up && claim.is_none() && settled {
            let lend = match policy.lending {
                Lending::Off => false,
                Lending::Always => true,
                Lending::Idle => idle_since.is_some_and(|t| now - t >= policy.dwell_ms),
            };
            if lend {
                out.push(Change::Lend { port: p(), at: now });
                lent = true;
            }
        }

        let usable =
            o.up && !exclusive && (o.role.storage_may_use() || (lent && revoking.is_none()));
        let target = if usable { weight_of(o) } else { 0.0 };
        let moved = if s.weight == 0.0 || target == 0.0 {
            s.weight != target
        } else {
            ((target - s.weight) / s.weight).abs() > policy.hysteresis
        };
        if moved {
            out.push(Change::Weight {
                port: p(),
                weight: target,
            });
        }
    }
    out
}

/// `state` with `changes` applied.
pub fn apply(state: &mut BTreeMap<PortId, PortState>, changes: &[Change]) {
    for c in changes {
        match c {
            Change::Role { port, role } => {
                state.entry(port.clone()).or_default().role = Some(*role)
            }
            Change::Idle { port, since } => {
                state.entry(port.clone()).or_default().idle_since = *since
            }
            Change::Lend { port, at } => {
                let s = state.entry(port.clone()).or_default();
                s.lent = true;
                s.last_change = Some(*at);
            }
            Change::Revoke { port, until } => {
                state.entry(port.clone()).or_default().revoking = Some(*until)
            }
            Change::Return { port, at } => {
                let s = state.entry(port.clone()).or_default();
                s.lent = false;
                s.revoking = None;
                s.last_change = Some(*at);
            }
            Change::Weight { port, weight } => {
                state.entry(port.clone()).or_default().weight = *weight
            }
        }
    }
}

/// What the reconciler reads.
pub trait Inputs {
    fn observed(&self) -> BTreeMap<PortId, Observed>;
    fn claims(&self) -> BTreeMap<PortId, Claim>;
    fn policy(&self) -> Policy;
}

/// Where its state lives: the map, in the leader.
pub trait Store {
    fn state(&self) -> BTreeMap<PortId, PortState>;
    fn write(&mut self, changes: &[Change]);
}

/// One pass: read, decide, write. Returns what changed.
pub fn step(now: u64, inputs: &impl Inputs, store: &mut impl Store) -> Vec<Change> {
    let changes = reconcile(
        now,
        &inputs.policy(),
        &inputs.observed(),
        &inputs.claims(),
        &store.state(),
    );
    if !changes.is_empty() {
        store.write(&changes);
    }
    changes
}
