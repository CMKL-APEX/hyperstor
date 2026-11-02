//! Choosing a path per I/O (D208, spec/26-networks.md §29.6): two random
//! choices, weighted by the reconciler, taking the one with fewer bytes
//! in flight, then lower RTT.

use rand::Rng;

/// What a sender knows about one path to a peer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathLoad {
    /// The reconciler's weight; 0 takes it out.
    pub weight: f64,
    pub bytes_in_flight: u64,
    /// Smoothed round-trip time, µs.
    pub rtt_us: u32,
}

/// The index of the path to use, or `None` when no path has weight.
pub fn choose(paths: &[PathLoad], rng: &mut impl Rng) -> Option<usize> {
    let total: f64 = paths.iter().map(|p| p.weight.max(0.0)).sum();
    if total <= 0.0 {
        return None;
    }
    let mut pick = || {
        let mut x = rng.random::<f64>() * total;
        for (i, p) in paths.iter().enumerate() {
            let w = p.weight.max(0.0);
            if x < w {
                return i;
            }
            x -= w;
        }
        // Rounding: the last path with weight.
        paths.iter().rposition(|p| p.weight > 0.0).unwrap_or(0)
    };
    let (a, b) = (pick(), pick());
    let key = |i: usize| (paths[i].bytes_in_flight, paths[i].rtt_us);
    Some(if key(b) < key(a) { b } else { a })
}
