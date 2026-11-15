//! The space volumes use, on the leader (20-tenants.md §24.4, D335–D339).
//!
//! Each primary reports, every 30 s, what each branch of each lineage
//! holds in the PGs it is primary of ([`hyperstor_proto::Msg::PgSpace`]);
//! the leader keeps the latest report of each PG in memory, by PG, so a PG
//! that changes primary is never counted twice. Every 30 s the leader also
//! reads the volumes' headers (the *catalog*): it gives each branch its
//! volume, and the heads the primaries lay chunk versions over. Counts are
//! attributed when read: branch → volume → (pool, tenant, project,
//! state), so a project that moves to another tenant takes its counts
//! along (D303), and a new leader has them again after one report from
//! each primary.

use crate::{Volume, VolumeState};
use hyperstor_client::snapshot::Branch;
use hyperstor_proto::space::{BranchBlocks, Head, PgBranches, encode_heads};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const BLOCK: u64 = 4096;

/// What a volume uses, as the catalog last read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSpace {
    pub id: u128,
    pub pool: u32,
    /// As its header names them (D258, D300): resolved when read.
    pub tenant: u32,
    pub project: u32,
    pub state: VolumeState,
    pub name: String,
    pub size: u64,
    /// Bytes its head reads from branches it owns.
    pub head_bytes: u64,
    /// Bytes its branches hold that its head doesn't read: held only by
    /// its snapshots, or by its clones through them.
    pub snapshot_bytes: u64,
}

/// The leader's view of the space volumes use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpaceView {
    /// Every volume of the catalog, trash and deletes under way included.
    pub volumes: Vec<VolumeSpace>,
    /// Bytes of branches no volume of the catalog owns (a delete's last
    /// steps, or a volume newer than the catalog), by pool.
    pub unattributed: BTreeMap<u32, u64>,
    /// Whether every PG of every data pool has reported and the catalog
    /// has been read: until then the counts are short.
    pub complete: bool,
}

#[derive(Debug, Default)]
struct Catalog {
    volumes: Vec<Volume>,
    owners: HashMap<(u128, u32), u128>,
}

#[derive(Debug, Default)]
pub struct Space {
    /// Each PG's latest report: the device that sent it, and its branches.
    pgs: BTreeMap<(u32, u32), (u32, Vec<BranchBlocks>)>,
    catalog: Option<Catalog>,
    /// The heads sent to primaries, and their version (0: none yet).
    heads: Vec<u8>,
    heads_version: u64,
}

/// Whether a lineage's chunks are laid over as [`Head::simple`] says: one
/// volume, the lineage's first, on branch 0, reading through nothing and
/// skipping nothing.
fn simple(lineage: u128, vols: &[&Volume]) -> bool {
    matches!(vols, [v] if v.id == lineage && v.branch == 0 && v.chain.is_empty() && v.skip.is_empty())
}

/// The lineages of `volumes` whose heads the primaries must be told: the
/// ones the catalog reads branch records of.
pub fn lineages_to_read(volumes: &[Volume]) -> Vec<u128> {
    let mut by: BTreeMap<u128, Vec<&Volume>> = BTreeMap::new();
    for v in volumes {
        by.entry(v.lineage).or_default().push(v);
    }
    by.into_iter()
        .filter(|(l, vs)| !simple(*l, vs))
        .map(|(l, _)| l)
        .collect()
}

impl Space {
    /// Takes a primary's report and returns how many PGs it now has from
    /// `device`. A full report first drops the device's PGs: those it
    /// still leads are in it.
    pub fn note(&mut self, device: u32, full: bool, pgs: Vec<PgBranches>) -> u32 {
        if full {
            self.pgs.retain(|_, (d, _)| *d != device);
        }
        for (pool, pg, b) in pgs {
            self.pgs.insert((pool, pg), (device, b));
        }
        self.pgs.values().filter(|(d, _)| *d == device).count() as u32
    }

    /// What a new leader forgets: the reports, which every primary sends
    /// again in full. The catalog and the heads stay until read again.
    pub fn forget_reports(&mut self) {
        self.pgs.clear();
    }

    /// The heads' version, and the heads when `have` isn't it. Before the
    /// catalog is read, a device keeps what it has.
    pub fn heads_for(&self, have: u64) -> (u64, &[u8]) {
        if self.catalog.is_none() || have == self.heads_version {
            (have, &[])
        } else {
            (self.heads_version, &self.heads)
        }
    }

    /// Takes a new catalog: every volume of every data pool, and the
    /// branch records of the lineages [`lineages_to_read`] names.
    pub fn set_catalog(&mut self, volumes: Vec<Volume>, branches: HashMap<u128, Vec<Branch>>) {
        let mut by: BTreeMap<u128, Vec<&Volume>> = BTreeMap::new();
        for v in &volumes {
            by.entry(v.lineage).or_default().push(v);
        }
        let mut owners = HashMap::new();
        let mut heads: Vec<(u128, Vec<Head>)> = Vec::new();
        for (&lineage, vs) in &by {
            let records = branches.get(&lineage).map_or(&[][..], |b| &b[..]);
            // A former branch first, so the volume's current one wins.
            for r in records {
                owners.insert((lineage, r.branch), r.volume);
            }
            for v in vs {
                if v.id == lineage {
                    owners.insert((lineage, 0), v.id);
                }
            }
            for v in vs {
                owners.insert((lineage, v.branch), v.id);
            }
            if simple(lineage, vs) {
                continue;
            }
            let mut hs: Vec<(u128, Head)> = vs
                .iter()
                .map(|v| {
                    let mut owned: Vec<u32> = records
                        .iter()
                        .filter(|r| r.volume == v.id)
                        .map(|r| r.branch)
                        .chain(std::iter::once(v.branch))
                        .chain((v.id == lineage).then_some(0))
                        .collect();
                    owned.sort_unstable();
                    owned.dedup();
                    (
                        v.id,
                        Head {
                            layers: v.layers(),
                            owned,
                        },
                    )
                })
                .collect();
            hs.sort_by_key(|(id, _)| *id);
            heads.push((lineage, hs.into_iter().map(|(_, h)| h).collect()));
        }
        let encoded = encode_heads(&heads);
        if self.catalog.is_none() || encoded != self.heads {
            self.heads = encoded;
            self.heads_version = crate::wall_nanos().max(self.heads_version + 1);
        }
        self.catalog = Some(Catalog { volumes, owners });
    }

    /// The counts as of the last reports and catalog. `pools` are the data
    /// pools with their PG counts: reports of PGs outside them are left
    /// out (a deleted pool).
    pub fn view(&self, pools: &BTreeMap<u32, u32>) -> SpaceView {
        let Some(cat) = &self.catalog else {
            return SpaceView::default();
        };
        let mut per: HashMap<u128, (u64, u64)> = HashMap::new();
        let mut unattributed: BTreeMap<u32, u64> = BTreeMap::new();
        let mut reported = BTreeSet::new();
        for (&(pool, pg), (_, branches)) in &self.pgs {
            if !pools.get(&pool).is_some_and(|&n| pg < n) {
                continue;
            }
            reported.insert((pool, pg));
            for b in branches {
                match cat.owners.get(&(b.lineage, b.branch)) {
                    Some(id) => {
                        let e = per.entry(*id).or_default();
                        e.0 += b.head;
                        e.1 += b.blocks.saturating_sub(b.head);
                    }
                    None => *unattributed.entry(pool).or_default() += b.blocks * BLOCK,
                }
            }
        }
        let total: u32 = pools.values().sum();
        SpaceView {
            volumes: cat
                .volumes
                .iter()
                .map(|v| {
                    let (h, s) = per.get(&v.id).copied().unwrap_or_default();
                    VolumeSpace {
                        id: v.id,
                        pool: v.pool,
                        tenant: v.tenant,
                        project: v.project,
                        state: v.state,
                        name: v.name.clone(),
                        size: v.size,
                        head_bytes: h * BLOCK,
                        snapshot_bytes: s * BLOCK,
                    }
                })
                .collect(),
            unattributed,
            complete: reported.len() as u32 == total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::SmallRng;
    use rand::{RngExt, SeedableRng};

    fn volume(id: u128, lineage: u128, branch: u32, state: VolumeState) -> Volume {
        Volume {
            id,
            pool: 1,
            size: 1 << 30,
            lineage,
            state,
            name: format!("v{id}"),
            version: 1,
            holder: None,
            delegates: Vec::new(),
            holder_label: None,
            head_epoch: 3,
            branch,
            chain: if branch == 0 {
                Vec::new()
            } else {
                vec![(0, 1)]
            },
            snapshots: Vec::new(),
            skip: Vec::new(),
            incarnation: 1,
            trashed_unix: 0,
            tenant: 0,
            project: (id % 3) as u32,
        }
    }

    /// Primaries report as `hyperstord` does (the PGs that changed, all of
    /// them when the leader's count isn't theirs), while PGs move to other
    /// primaries, counts change, and leaders change (a new one forgets
    /// every report): once every primary has reported since, the leader's
    /// counts are the branches' as they are, each PG once.
    #[test]
    fn reports_converge_on_the_branches_through_moves_and_failovers() {
        const PGS: u32 = 16;
        for seed in 0..200u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            // Lineage 10: volume 10 and its clone 11 (branch 1); lineage
            // 20: volume 20 alone; branch 9 of lineage 20 is no one's.
            let vols = vec![
                volume(10, 10, 0, VolumeState::Active),
                volume(11, 10, 1, VolumeState::Trashed),
                volume(20, 20, 0, VolumeState::Deleting),
            ];
            let owner = |l: u128, b: u32| match (l, b) {
                (10, 0) => Some(10u128),
                (10, 1) => Some(11),
                (20, 0) => Some(20),
                _ => None,
            };
            let mut space = Space::default();
            let lineages = lineages_to_read(&vols);
            assert_eq!(lineages, vec![10]);
            space.set_catalog(vols.clone(), HashMap::new());
            let mut truth: BTreeMap<u32, Vec<BranchBlocks>> = BTreeMap::new();
            let mut primary: BTreeMap<u32, u32> = (0..PGS).map(|g| (g, g % 3)).collect();
            // Each device's last report, as the daemon keeps it.
            let mut sent: HashMap<u32, (bool, HashMap<u32, Vec<BranchBlocks>>)> =
                (0..3).map(|d| (d, (true, HashMap::new()))).collect();
            let pools: BTreeMap<u32, u32> = [(1, PGS)].into();
            for round in 0..30 {
                for _ in 0..rng.random_range(0..6) {
                    let pg = rng.random_range(0..PGS);
                    match rng.random_range(0..4) {
                        0 => {
                            primary.insert(pg, rng.random_range(0..3));
                        }
                        _ => {
                            let (l, b) =
                                [(10, 0), (10, 1), (20, 0), (20, 9)][rng.random_range(0..4)];
                            let blocks = rng.random_range(0..100);
                            let head = rng.random_range(0..=blocks);
                            let v = truth.entry(pg).or_default();
                            v.retain(|x| (x.lineage, x.branch) != (l, b));
                            if blocks > 0 {
                                v.push(BranchBlocks {
                                    lineage: l,
                                    branch: b,
                                    blocks,
                                    head,
                                });
                                v.sort_by_key(|x| (x.lineage, x.branch));
                            }
                        }
                    }
                }
                if rng.random_bool(0.15) {
                    space.forget_reports();
                }
                // Some devices miss a round (a report lost): it is then full.
                for d in 0..3 {
                    let mine: HashMap<u32, Vec<BranchBlocks>> = primary
                        .iter()
                        .filter(|(_, p)| **p == d)
                        .map(|(g, _)| (*g, truth.get(g).cloned().unwrap_or_default()))
                        .collect();
                    let (full, last) = sent.get_mut(&d).unwrap();
                    if rng.random_bool(0.1) {
                        *full = true;
                        continue;
                    }
                    let changed: Vec<PgBranches> = mine
                        .iter()
                        .filter(|(g, b)| *full || last.get(*g) != Some(*b))
                        .map(|(g, b)| (1, *g, b.clone()))
                        .collect();
                    let known = space.note(d, *full, changed);
                    *full = known as usize != mine.len();
                    *last = mine;
                }
                // A round where every device reports brings it level.
                if round % 5 == 4 {
                    for _ in 0..2 {
                        for d in 0..3 {
                            let mine: HashMap<u32, Vec<BranchBlocks>> = primary
                                .iter()
                                .filter(|(_, p)| **p == d)
                                .map(|(g, _)| (*g, truth.get(g).cloned().unwrap_or_default()))
                                .collect();
                            let (full, last) = sent.get_mut(&d).unwrap();
                            let changed: Vec<PgBranches> = mine
                                .iter()
                                .filter(|(g, b)| *full || last.get(*g) != Some(*b))
                                .map(|(g, b)| (1, *g, b.clone()))
                                .collect();
                            let known = space.note(d, *full, changed);
                            *full = known as usize != mine.len();
                            *last = mine;
                        }
                    }
                    let view = space.view(&pools);
                    let mut want: HashMap<u128, (u64, u64)> = HashMap::new();
                    let mut lost = 0;
                    for b in truth.values().flatten() {
                        match owner(b.lineage, b.branch) {
                            Some(id) => {
                                let e = want.entry(id).or_default();
                                e.0 += b.head * BLOCK;
                                e.1 += (b.blocks - b.head) * BLOCK;
                            }
                            None => lost += b.blocks * BLOCK,
                        }
                    }
                    for v in &view.volumes {
                        assert_eq!(
                            (v.head_bytes, v.snapshot_bytes),
                            want.get(&v.id).copied().unwrap_or_default(),
                            "seed {seed} round {round} volume {}",
                            v.id
                        );
                    }
                    assert_eq!(
                        view.unattributed.get(&1).copied().unwrap_or(0),
                        lost,
                        "seed {seed} round {round}"
                    );
                    assert!(view.complete, "seed {seed} round {round}");
                }
            }
        }
    }

    /// The heads primaries get: none for a lineage of one plain volume;
    /// each volume's layers and own branches otherwise; a new version only
    /// when they change, and a device keeps its own before a catalog.
    #[test]
    fn heads_name_lineages_with_clones_or_rollbacks() {
        let mut s = Space::default();
        assert_eq!(s.heads_for(42), (42, &[0u8; 0][..]));
        let plain = volume(20, 20, 0, VolumeState::Active);
        let parent = volume(10, 10, 0, VolumeState::Active);
        let clone = volume(11, 10, 1, VolumeState::Active);
        s.set_catalog(vec![plain.clone()], HashMap::new());
        let (v1, h) = s.heads_for(0);
        assert!(v1 > 0);
        assert!(hyperstor_proto::space::decode_heads(h).unwrap().is_empty());
        assert!(s.heads_for(v1).1.is_empty());
        let records = vec![
            Branch {
                branch: 1,
                volume: 11,
                chain: vec![(0, 1)],
                flattened: 0,
            },
            // A branch a rollback left the parent.
            Branch {
                branch: 2,
                volume: 10,
                chain: Vec::new(),
                flattened: 0,
            },
        ];
        s.set_catalog(
            vec![plain.clone(), parent.clone(), clone.clone()],
            [(10, records.clone())].into(),
        );
        let (v2, h) = s.heads_for(v1);
        assert!(v2 > v1);
        let heads = hyperstor_proto::space::decode_heads(h).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(
            heads[&10],
            vec![
                Head {
                    layers: parent.layers(),
                    owned: vec![0, 2],
                },
                Head {
                    layers: clone.layers(),
                    owned: vec![1],
                },
            ]
        );
        // The same catalog again: the same version.
        s.set_catalog(vec![plain, parent, clone], [(10, records)].into());
        assert_eq!(s.heads_for(v2).0, v2);
    }
}
