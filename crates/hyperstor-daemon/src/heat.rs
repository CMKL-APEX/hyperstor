//! A reactor's load, for the map service's leader (27-performance.md §30,
//! D215, D216): what each PG served as primary in the window, with the
//! volumes that sent most of it, and how long the device itself took for
//! reads and commits. Plain additions on the reactor's own thread, with
//! no atomic, lock or system call: [`Heat::take`] hands the window over
//! every 5 s and starts the next.

use hyperstor_proto::perf::{BUCKETS, BUCKETS_US, PgHeat, Service, TOP_VOLUMES};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

/// Volumes tracked per PG: twice what is reported, so the reported ones
/// are rarely ones the sketch has just let in.
const SKETCH: usize = 2 * TOP_VOLUMES;

/// A volume chunk's name: lineage (16 bytes), chunk (8), then the branch
/// (4) unless it is branch 0 (`hyperstor_client::branch_name`).
pub fn volume_of(name: &[u8]) -> Option<(u128, u32)> {
    let lineage = u128::from_be_bytes(name.get(..16)?.try_into().ok()?);
    let branch = match name.len() {
        24 => 0,
        28 => u32::from_be_bytes(name[24..28].try_into().ok()?),
        _ => return None,
    };
    Some((lineage, branch))
}

/// The most frequent keys of a stream in `SKETCH` slots (Metwally et al.,
/// space-saving): a key not kept replaces the least counted one and takes
/// its count plus one, so a kept key's count is at least its true count
/// and over it by at most the count it took over.
#[derive(Default, Debug)]
struct TopK {
    slots: Vec<((u128, u32), u32, u64)>,
}

impl TopK {
    fn note(&mut self, key: (u128, u32), bytes: u64) {
        if let Some(s) = self.slots.iter_mut().find(|s| s.0 == key) {
            s.1 += 1;
            s.2 += bytes;
            return;
        }
        if self.slots.len() < SKETCH {
            self.slots.push((key, 1, bytes));
            return;
        }
        let min = (0..self.slots.len())
            .min_by_key(|i| self.slots[*i].1)
            .expect("full");
        let s = &mut self.slots[min];
        *s = (key, s.1 + 1, bytes);
    }

    fn top(mut self) -> Vec<(u128, u32, u32, u64)> {
        self.slots.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.slots
            .into_iter()
            .take(TOP_VOLUMES)
            .map(|((l, b), n, by)| (l, b, n, by))
            .collect()
    }
}

#[derive(Default)]
struct PgCount {
    reads: u32,
    writes: u32,
    read_bytes: u64,
    write_bytes: u64,
    top: TopK,
}

#[derive(Default)]
struct Window {
    pgs: HashMap<(u32, u32), PgCount>,
    service: [Service; 2],
    inflight: u32,
    inflight_max: u32,
}

/// One reactor's counters.
#[derive(Default)]
pub struct Heat {
    w: RefCell<Window>,
}

impl Heat {
    /// A client request served for PG `pool`.`pg`, on a chunk of volume
    /// `vol` (lineage, branch; [`volume_of`]) or another object.
    pub fn request(&self, pool: u32, pg: u32, vol: Option<(u128, u32)>, write: bool, bytes: u64) {
        let mut w = self.w.borrow_mut();
        let c = w.pgs.entry((pool, pg)).or_default();
        if write {
            c.writes = c.writes.saturating_add(1);
            c.write_bytes += bytes;
        } else {
            c.reads = c.reads.saturating_add(1);
            c.read_bytes += bytes;
        }
        if let Some(v) = vol {
            c.top.note(v, bytes);
        }
    }

    /// An I/O of kind `op` ([`Service::READ`], [`Service::COMMIT`]) starts
    /// on the device.
    pub fn begin(&self) {
        let mut w = self.w.borrow_mut();
        w.inflight += 1;
        w.inflight_max = w.inflight_max.max(w.inflight);
    }

    /// It ended after `took`, moving `bytes`.
    pub fn end(&self, op: u8, took: Duration, bytes: u64) {
        let mut w = self.w.borrow_mut();
        w.inflight = w.inflight.saturating_sub(1);
        let us = took.as_micros().min(u128::from(u32::MAX)) as u32;
        let s = &mut w.service[usize::from(op.min(1))];
        s.op = op;
        s.count = s.count.saturating_add(1);
        s.bytes += bytes;
        s.max_us = s.max_us.max(us);
        let i = BUCKETS_US
            .iter()
            .position(|b| us <= *b)
            .unwrap_or(BUCKETS - 1);
        s.buckets[i] = s.buckets[i].saturating_add(1);
    }

    /// The window so far, and a new one: the PGs that served anything,
    /// both kinds of I/O (empty ones too, so the leader sees an idle
    /// device), and the most I/Os in flight (what is in flight now
    /// carries over).
    pub fn take(&self) -> (Vec<PgHeat>, Vec<Service>, u32) {
        let mut w = self.w.borrow_mut();
        let inflight = w.inflight;
        let old = std::mem::take(&mut *w);
        w.inflight = inflight;
        w.inflight_max = inflight;
        let mut pgs: Vec<PgHeat> = old
            .pgs
            .into_iter()
            .map(|((pool, pg), c)| PgHeat {
                pool,
                pg,
                reads: c.reads,
                writes: c.writes,
                read_bytes: c.read_bytes,
                write_bytes: c.write_bytes,
                top: c.top.top(),
            })
            .collect();
        pgs.sort_by_key(|p| (p.pool, p.pg));
        let mut service = old.service;
        service[0].op = Service::READ;
        service[1].op = Service::COMMIT;
        (pgs, service.to_vec(), old.inflight_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(lineage: u128, branch: u32) -> Vec<u8> {
        let mut n = lineage.to_be_bytes().to_vec();
        n.extend_from_slice(&7u64.to_be_bytes());
        if branch != 0 {
            n.extend_from_slice(&branch.to_be_bytes());
        }
        n
    }

    #[test]
    fn chunk_names_give_their_volume() {
        assert_eq!(volume_of(&chunk(1 << 99, 0)), Some((1 << 99, 0)));
        assert_eq!(volume_of(&chunk(5, 3)), Some((5, 3)));
        assert_eq!(volume_of(b"h/0123"), None);
    }

    /// The space-saving sketch keeps the heavy hitters of a stream with
    /// many more keys than slots, each counted at least as often as it
    /// came and over by at most the smallest count kept.
    #[test]
    fn the_sketch_keeps_the_heaviest_volumes() {
        let mut t = TopK::default();
        let mut truth: HashMap<(u128, u32), u32> = HashMap::new();
        let mut rng = 1u64;
        for i in 0..20_000u64 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Volumes 1–3 send 60 % of requests; 500 others the rest.
            let v = if i % 5 < 3 {
                (i % 3) as u128 + 1
            } else {
                100 + u128::from(rng >> 33) % 500
            };
            t.note((v, 0), 4096);
            *truth.entry((v, 0)).or_default() += 1;
        }
        let floor = t.slots.iter().map(|s| s.1).min().unwrap();
        let top = t.top();
        let ids: Vec<u128> = top.iter().take(3).map(|x| x.0).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, [1, 2, 3], "{top:?}");
        for (l, b, n, _) in &top {
            let real = truth[&(*l, *b)];
            assert!(*n >= real && *n <= real + floor, "{l}: {n} vs {real}");
        }
    }

    #[test]
    fn a_window_is_taken_and_the_next_starts() {
        let h = Heat::default();
        h.request(1, 4, volume_of(&chunk(9, 0)), true, 8192);
        h.request(1, 4, volume_of(&chunk(9, 0)), false, 4096);
        h.request(1, 4, volume_of(b"h/x"), false, 100);
        h.begin();
        h.begin();
        h.end(Service::COMMIT, Duration::from_micros(300), 8192);
        h.begin();
        h.end(Service::READ, Duration::from_millis(70), 4096);
        let (pgs, service, inflight) = h.take();
        assert_eq!(pgs.len(), 1);
        let p = &pgs[0];
        assert_eq!(
            (p.reads, p.writes, p.read_bytes, p.write_bytes),
            (2, 1, 4196, 8192)
        );
        assert_eq!(p.top, vec![(9, 0, 2, 12288)]);
        assert_eq!(inflight, 2);
        assert_eq!(service[1].count, 1);
        assert_eq!(service[1].buckets[5], 1, "300 µs is under 512");
        assert_eq!(
            service[0].buckets[BUCKETS - 1],
            1,
            "70 ms is past the last bound"
        );
        assert_eq!(service[0].max_us, 70_000);
        // One I/O is still in flight: the next window starts with it.
        let (pgs, service, inflight) = h.take();
        assert!(pgs.is_empty() && service.iter().all(|s| s.count == 0));
        assert_eq!(inflight, 1);
    }
}
