//! What a storage device reports about its load and its drive
//! (27-performance.md §30, D218–D222): the requests each PG it is primary
//! of served, with the volumes that sent most of them; how long its own
//! reads and commits took on the device; and, every 30 s, what the drive
//! says of itself. Sent as [`crate::Msg::PerfReport`] every 5 s.

use hyperstor_codec::{Dec, DecodeError, Enc};

/// Upper bounds of the service-time buckets, in µs: powers of two from
/// 16 µs to 65 ms, then a last bucket for anything slower (as the
/// metrics' request histograms).
pub const BUCKETS_US: [u32; 13] = [
    16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
/// [`BUCKETS_US`] and the last, unbounded one.
pub const BUCKETS: usize = BUCKETS_US.len() + 1;

/// What one PG served in the window, on its primary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PgHeat {
    pub pool: u32,
    pub pg: u32,
    pub reads: u32,
    pub writes: u32,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// The volumes that sent most of its requests, as (lineage, branch,
    /// requests, bytes), most first: at most [`TOP_VOLUMES`]. Counts are
    /// at least the true ones and overcount by at most the smallest kept
    /// (the space-saving sketch).
    pub top: Vec<(u128, u32, u32, u64)>,
}

/// Volumes kept per PG in a report.
pub const TOP_VOLUMES: usize = 5;

/// The device's own time for one kind of I/O in the window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Service {
    /// 0: reads; 1: commits (a round of writes and its flush).
    pub op: u8,
    pub count: u32,
    pub bytes: u64,
    pub buckets: [u32; BUCKETS],
    pub max_us: u32,
}

impl Service {
    pub const READ: u8 = 0;
    pub const COMMIT: u8 = 1;

    /// The time under which `q` of the window's I/Os completed (0 to 1),
    /// as the upper bound of its bucket; the max for the last bucket.
    pub fn quantile_us(&self, q: f64) -> u32 {
        if self.count == 0 {
            return 0;
        }
        let want = ((self.count as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += u64::from(*n);
            if seen >= want {
                return BUCKETS_US
                    .get(i)
                    .copied()
                    .unwrap_or(self.max_us)
                    .min(self.max_us.max(1));
            }
        }
        self.max_us
    }
}

/// What the drive's controller reports in its SMART / health log
/// (NVMe log page 0x02), as read by `hyperstor-smart`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Smart {
    pub critical_warning: u8,
    /// Composite temperature, kelvin.
    pub temperature_k: u16,
    pub available_spare: u8,
    pub spare_threshold: u8,
    pub percentage_used: u8,
    /// Units of 512,000 bytes.
    pub data_units_read: u64,
    pub data_units_written: u64,
    pub controller_busy_minutes: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub error_log_entries: u64,
    /// Minutes spent above the warning and critical composite temperatures.
    pub warning_temp_minutes: u32,
    pub critical_temp_minutes: u32,
    /// Thermal management: transitions to, and seconds at, the first and
    /// second throttling levels.
    pub throttle_transitions: [u32; 2],
    pub throttle_seconds: [u32; 2],
    /// The controller's warning and critical temperature thresholds,
    /// kelvin (from Identify; 0 if unknown).
    pub warning_threshold_k: u16,
    pub critical_threshold_k: u16,
}

/// What a device is, from sysfs, and what its drive says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Drive {
    pub model: String,
    pub firmware: String,
    /// Temperature from the kernel's hwmon, millidegrees Celsius; none if
    /// it has none (a file).
    pub temperature_mc: Option<i32>,
    /// PCIe link: current and maximum width (lanes) and speed (GT/s × 10).
    pub link: Option<(u16, u16, u16, u16)>,
    /// The NUMA node the drive hangs off, and the reactor's; -1 if unknown.
    pub numa: (i32, i32),
    /// The drive has a volatile write cache (flushes cost).
    pub write_cache: bool,
    pub smart: Option<Smart>,
    /// Why there is no SMART (not an NVMe drive, the helper not installed).
    pub smart_note: String,
}

/// A device's report to the map service's leader.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PerfReport {
    pub device: u32,
    /// How long the window was, in ms.
    pub window_ms: u32,
    pub pgs: Vec<PgHeat>,
    pub service: Vec<Service>,
    /// The most I/Os the device had in flight at once in the window.
    pub inflight_max: u32,
    /// Sent every 30 s, and with the first report.
    pub drive: Option<Drive>,
}

impl PerfReport {
    pub fn encode(&self, e: &mut Enc) {
        e.u32(self.device).u32(self.window_ms);
        e.seq(&self.pgs, |e, p| {
            e.u32(p.pool)
                .u32(p.pg)
                .u32(p.reads)
                .u32(p.writes)
                .u64(p.read_bytes)
                .u64(p.write_bytes);
            e.seq(&p.top, |e, (l, b, n, by)| {
                e.u128(*l).u32(*b).u32(*n).u64(*by);
            });
        });
        e.seq(&self.service, |e, s| {
            e.u8(s.op).u32(s.count).u64(s.bytes).u32(s.max_us);
            for b in s.buckets {
                e.u32(b);
            }
        });
        e.u32(self.inflight_max);
        match &self.drive {
            None => {
                e.bool(false);
            }
            Some(d) => {
                e.bool(true);
                encode_drive(e, d);
            }
        }
    }

    pub fn decode(d: &mut Dec) -> Result<PerfReport, DecodeError> {
        let device = d.u32()?;
        let window_ms = d.u32()?;
        let pgs = d.seq(32, |d| {
            Ok(PgHeat {
                pool: d.u32()?,
                pg: d.u32()?,
                reads: d.u32()?,
                writes: d.u32()?,
                read_bytes: d.u64()?,
                write_bytes: d.u64()?,
                top: d.seq(32, |d| Ok((d.u128()?, d.u32()?, d.u32()?, d.u64()?)))?,
            })
        })?;
        let service = d.seq(17 + 4 * BUCKETS, |d| {
            let mut s = Service {
                op: d.u8()?,
                count: d.u32()?,
                bytes: d.u64()?,
                max_us: d.u32()?,
                ..Default::default()
            };
            for b in s.buckets.iter_mut() {
                *b = d.u32()?;
            }
            Ok(s)
        })?;
        let inflight_max = d.u32()?;
        let drive = if d.bool()? {
            Some(decode_drive(d)?)
        } else {
            None
        };
        Ok(PerfReport {
            device,
            window_ms,
            pgs,
            service,
            inflight_max,
            drive,
        })
    }
}

fn encode_drive(e: &mut Enc, d: &Drive) {
    e.str(&d.model).str(&d.firmware);
    e.bool(d.temperature_mc.is_some())
        .u32(d.temperature_mc.unwrap_or(0) as u32);
    let (w, wm, s, sm) = d.link.unwrap_or_default();
    e.bool(d.link.is_some()).u16(w).u16(wm).u16(s).u16(sm);
    e.u32(d.numa.0 as u32).u32(d.numa.1 as u32);
    e.bool(d.write_cache).str(&d.smart_note);
    match &d.smart {
        None => {
            e.bool(false);
        }
        Some(s) => {
            e.bool(true)
                .u8(s.critical_warning)
                .u16(s.temperature_k)
                .u8(s.available_spare)
                .u8(s.spare_threshold)
                .u8(s.percentage_used)
                .u64(s.data_units_read)
                .u64(s.data_units_written)
                .u64(s.controller_busy_minutes)
                .u64(s.power_on_hours)
                .u64(s.unsafe_shutdowns)
                .u64(s.media_errors)
                .u64(s.error_log_entries)
                .u32(s.warning_temp_minutes)
                .u32(s.critical_temp_minutes)
                .u32(s.throttle_transitions[0])
                .u32(s.throttle_transitions[1])
                .u32(s.throttle_seconds[0])
                .u32(s.throttle_seconds[1])
                .u16(s.warning_threshold_k)
                .u16(s.critical_threshold_k);
        }
    }
}

fn decode_drive(d: &mut Dec) -> Result<Drive, DecodeError> {
    let model = d.str()?.to_string();
    let firmware = d.str()?.to_string();
    let has_t = d.bool()?;
    let t = d.u32()? as i32;
    let has_link = d.bool()?;
    let link = (d.u16()?, d.u16()?, d.u16()?, d.u16()?);
    let numa = (d.u32()? as i32, d.u32()? as i32);
    let write_cache = d.bool()?;
    let smart_note = d.str()?.to_string();
    let smart = if d.bool()? {
        Some(Smart {
            critical_warning: d.u8()?,
            temperature_k: d.u16()?,
            available_spare: d.u8()?,
            spare_threshold: d.u8()?,
            percentage_used: d.u8()?,
            data_units_read: d.u64()?,
            data_units_written: d.u64()?,
            controller_busy_minutes: d.u64()?,
            power_on_hours: d.u64()?,
            unsafe_shutdowns: d.u64()?,
            media_errors: d.u64()?,
            error_log_entries: d.u64()?,
            warning_temp_minutes: d.u32()?,
            critical_temp_minutes: d.u32()?,
            throttle_transitions: [d.u32()?, d.u32()?],
            throttle_seconds: [d.u32()?, d.u32()?],
            warning_threshold_k: d.u16()?,
            critical_threshold_k: d.u16()?,
        })
    } else {
        None
    };
    Ok(Drive {
        model,
        firmware,
        temperature_mc: has_t.then_some(t),
        link: has_link.then_some(link),
        numa,
        write_cache,
        smart,
        smart_note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_come_from_the_buckets() {
        let mut s = Service {
            count: 100,
            max_us: 9000,
            ..Default::default()
        };
        s.buckets[2] = 90; // ≤ 64 µs
        s.buckets[9] = 9; // ≤ 8192 µs
        s.buckets[13] = 1; // slower than 65 ms: the max
        assert_eq!(s.quantile_us(0.5), 64);
        assert_eq!(s.quantile_us(0.99), 8192);
        assert_eq!(s.quantile_us(1.0), 9000);
        assert_eq!(Service::default().quantile_us(0.99), 0);
    }
}
