//! How full devices and pools are, against the fullness thresholds
//! (04-decisions.md D27, D182), from the devices' space reports to the
//! leader.

use crate::{ClusterMap, DeviceSpace, FullRatios};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Nearfull,
    Backfillfull,
    Full,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Nearfull => "nearfull",
            Self::Backfillfull => "backfillfull",
            Self::Full => "full",
        }
    }

    /// As a pool shows it: past `backfillfull` is still nearfull (D27).
    pub fn pool_name(self) -> &'static str {
        match self {
            Self::Backfillfull => "nearfull",
            l => l.name(),
        }
    }
}

/// A device of `total` bytes with `free` left, against `r`.
pub fn level(r: &FullRatios, total: u64, free: u64) -> Level {
    let used = total.saturating_sub(free);
    let past = |t: u16| used > FullRatios::limit(total, t);
    if total == 0 {
        Level::Ok
    } else if past(r.full) {
        Level::Full
    } else if past(r.backfillfull) {
        Level::Backfillfull
    } else if past(r.nearfull) {
        Level::Nearfull
    } else {
        Level::Ok
    }
}

/// Each reported device's level and the percentage of it used.
pub fn devices(map: &ClusterMap, space: &DeviceSpace) -> BTreeMap<u32, (Level, f64)> {
    space
        .iter()
        .filter(|(d, _)| map.devices.contains_key(d))
        .map(|(&d, &(total, free))| {
            let pct = if total == 0 {
                0.0
            } else {
                100.0 * total.saturating_sub(free) as f64 / total as f64
            };
            (d, (level(&map.full_ratios, total, free), pct))
        })
        .collect()
}

/// Pool `pool`'s level, the fullest of the devices placement may use for
/// it, and that device; `None` until every one has reported.
pub fn pool(
    map: &ClusterMap,
    pool: u32,
    devices: &BTreeMap<u32, (Level, f64)>,
) -> Option<(Level, Option<u32>)> {
    let info = map.pools.get(&pool)?;
    let mut worst = (Level::Ok, None, 0.0);
    for d in crate::api::pool_devices(map, info) {
        let &(l, pct) = devices.get(&d.id)?;
        if (l, pct) > (worst.0, worst.2) {
            worst = (l, Some(d.id), pct);
        }
    }
    Some((worst.0, worst.1.filter(|_| worst.0 != Level::Ok)))
}

/// The pools now full, each with a reason a refused creation gives.
pub fn full_pools(map: &ClusterMap, space: &DeviceSpace) -> BTreeMap<u32, String> {
    let devices = devices(map, space);
    map.pools
        .keys()
        .filter_map(|&p| match pool(map, p, &devices)? {
            (Level::Full, Some(d)) => Some((
                p,
                format!(
                    "pool {p} is full: device {d} is {:.1}% used, past full ({:.1}%); free space or add devices",
                    devices[&d].1,
                    map.full_ratios.full as f64 / 10.0
                ),
            )),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_ratios() {
        let r = FullRatios::default();
        let at = |used: u64| level(&r, 1000, 1000 - used);
        assert_eq!(
            [at(0), at(851), at(901), at(951), at(1000)],
            [
                Level::Ok,
                Level::Nearfull,
                Level::Backfillfull,
                Level::Full,
                Level::Full
            ]
        );
        assert_eq!(at(850), Level::Ok, "at the line, not past it");
        assert_eq!(level(&r, 0, 0), Level::Ok, "no data area reported");
        assert_eq!(Level::Backfillfull.pool_name(), "nearfull");
    }
}
