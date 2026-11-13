//! Moving a project to another tenant (20-tenants.md §24.9.1, 04-decisions.md
//! D303): the steps of the operation, over [`Steps`], which the operation
//! runner gives real effects and the tests a model.
//!
//! The project was made `moving` when the move was asked
//! (`crate::tenancy::start_move`). Then:
//!
//! 1. each of its buckets still under the old tenant moves, its table
//!    renamed with its id kept; the old tenant's buckets are listed again
//!    until none of the project's is left, so one made meanwhile (by a
//!    gateway's copy of a key from before) goes too;
//! 2. the project names its new tenant and is `active`, in one write
//!    (`crate::tenancy::finish_move`).
//!
//! Every step is done once: run again after a failover, it finds what is
//! done and goes on. A bucket that can't move (its name taken in the new
//! tenant meanwhile, or the operation cancelled) undoes the move: the
//! buckets moved go back, and the project is `active` where it was.

use std::future::Future;

/// What the steps need done.
pub trait Steps {
    /// The buckets of `tenant` whose record names `project`: (name, table).
    fn buckets(
        &self,
        tenant: u32,
        project: u32,
    ) -> impl Future<Output = Result<Vec<(String, u64)>, Stop>>;
    /// Moves bucket `name`, table `table`, from `from` to `to`.
    fn move_bucket(
        &self,
        from: u32,
        to: u32,
        name: &str,
        table: u64,
    ) -> impl Future<Output = Result<(), Stop>>;
    /// The project names `to` and is `active`.
    fn finish(&self, project: u32, to: u32) -> impl Future<Output = Result<(), Stop>>;
    /// The project is `active` where it was.
    fn undo(&self, project: u32) -> impl Future<Output = Result<(), Stop>>;
    /// Whether the operation is still to run (not cancelled).
    fn running(&self) -> impl Future<Output = bool>;
}

/// Why the steps stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// This member no longer runs it (a failover): the next leader resumes.
    Lost,
    /// It can't go on: undone where it could be.
    Failed(String),
}

/// How many times the old tenant's buckets are listed again before the
/// last write: one made after that stays with its tenant (I379).
const SWEEPS: usize = 8;

/// Moves `project` from tenant `from` to `to`; how many buckets moved now.
/// A failure undoes it: the buckets moved go back, the project stays.
pub async fn run<S: Steps>(s: &S, project: u32, from: u32, to: u32) -> Result<u32, Stop> {
    match forward(s, project, from, to).await {
        Ok(n) => Ok(n),
        Err(Stop::Lost) => Err(Stop::Lost),
        Err(Stop::Failed(why)) => {
            back(s, project, from, to).await?;
            Err(Stop::Failed(why))
        }
    }
}

async fn forward<S: Steps>(s: &S, project: u32, from: u32, to: u32) -> Result<u32, Stop> {
    let mut moved = 0;
    for _ in 0..SWEEPS {
        let left = s.buckets(from, project).await?;
        if left.is_empty() {
            break;
        }
        for (name, table) in left {
            if !s.running().await {
                return Err(Stop::Failed("cancelled".into()));
            }
            s.move_bucket(from, to, &name, table).await?;
            moved += 1;
        }
    }
    s.finish(project, to).await?;
    Ok(moved)
}

/// The undo: the project's buckets under `to` back to `from`, then the
/// project `active` where it was. Done once too.
async fn back<S: Steps>(s: &S, project: u32, from: u32, to: u32) -> Result<(), Stop> {
    for (name, table) in s.buckets(to, project).await? {
        match s.move_bucket(to, from, &name, table).await {
            Ok(()) => {}
            Err(Stop::Lost) => return Err(Stop::Lost),
            // Its old name taken meanwhile: it stays where it is, still the
            // project's, and the failure says so.
            Err(Stop::Failed(_)) => {}
        }
    }
    s.undo(project).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A bucket in the model: its table, project and objects.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct B {
        table: u64,
        project: u32,
        objects: u64,
    }

    /// The model: buckets by (tenant, name), the project's tenant and
    /// state; `budget` effects before a failover strikes.
    #[derive(Debug, Default)]
    struct World {
        buckets: BTreeMap<(u32, String), B>,
        tenant: u32,
        moving: bool,
        budget: Option<u32>,
        cancelled: bool,
        next_table: u64,
    }

    struct Fake(RefCell<World>);

    impl Fake {
        /// One effect: fails with a failover once the budget is spent.
        fn spend(&self) -> Result<(), Stop> {
            let mut w = self.0.borrow_mut();
            match &mut w.budget {
                Some(0) => Err(Stop::Lost),
                Some(n) => {
                    *n -= 1;
                    Ok(())
                }
                None => Ok(()),
            }
        }
    }

    impl Steps for Fake {
        async fn buckets(&self, tenant: u32, project: u32) -> Result<Vec<(String, u64)>, Stop> {
            self.spend()?;
            Ok(self
                .0
                .borrow()
                .buckets
                .iter()
                .filter(|((t, _), b)| *t == tenant && b.project == project)
                .map(|((_, n), b)| (n.clone(), b.table))
                .collect())
        }

        async fn move_bucket(
            &self,
            from: u32,
            to: u32,
            name: &str,
            table: u64,
        ) -> Result<(), Stop> {
            self.spend()?;
            let mut w = self.0.borrow_mut();
            let key = |t: u32| (t, name.to_string());
            match (w.buckets.get(&key(to)), w.buckets.get(&key(from))) {
                (Some(b), _) if b.table == table => Ok(()),
                (Some(_), _) => Err(Stop::Failed(format!("{name} is taken"))),
                (None, Some(b)) if b.table == table => {
                    let b = w.buckets.remove(&key(from)).unwrap();
                    w.buckets.insert(key(to), b);
                    Ok(())
                }
                // Gone, or another bucket of that name since: left alone.
                _ => Ok(()),
            }
        }

        async fn finish(&self, _project: u32, to: u32) -> Result<(), Stop> {
            self.spend()?;
            let mut w = self.0.borrow_mut();
            w.tenant = to;
            w.moving = false;
            Ok(())
        }

        async fn undo(&self, _project: u32) -> Result<(), Stop> {
            self.spend()?;
            self.0.borrow_mut().moving = false;
            Ok(())
        }

        async fn running(&self) -> bool {
            !self.0.borrow().cancelled
        }
    }

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A small deterministic random source (xorshift).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const P: u32 = 7;
    const OTHER: u32 = 8;
    const A: u32 = 1;
    const B_: u32 = 2;

    /// Transfers against the model (D303): writes to buckets (by table, as
    /// a gateway's request in flight does) and bucket creates interleaved
    /// with the steps, failovers at random points, each resumed by running
    /// again, sometimes a cancel or a name taken in the new tenant. At the
    /// end the move is done or undone, whole: every bucket of the project
    /// with its objects under one tenant, the project's, which names it,
    /// and active; nothing else touched; no object lost.
    #[test]
    fn transfers_match_a_model_through_failovers() {
        for seed in 1..400u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1);
            let mut w = World {
                tenant: A,
                moving: true,
                next_table: 1,
                ..Default::default()
            };
            let mut objects: BTreeMap<u64, u64> = BTreeMap::new();
            let names = ["a", "b", "c", "d", "e", "f"];
            let make = |w: &mut World,
                        objects: &mut BTreeMap<u64, u64>,
                        tenant: u32,
                        project: u32,
                        rng: &mut Rng| {
                let name = names[rng.below(names.len() as u64) as usize].to_string();
                if w.buckets.contains_key(&(tenant, name.clone())) {
                    return;
                }
                let table = w.next_table;
                w.next_table += 1;
                let n = rng.below(5);
                objects.insert(table, n);
                w.buckets.insert(
                    (tenant, name),
                    B {
                        table,
                        project,
                        objects: n,
                    },
                );
            };
            for _ in 0..rng.below(5) {
                make(&mut w, &mut objects, A, P, &mut rng);
            }
            for _ in 0..rng.below(3) {
                make(&mut w, &mut objects, A, OTHER, &mut rng);
            }
            // The API's check: no name of the project's taken in B.
            let ours: Vec<String> = w
                .buckets
                .keys()
                .filter(|(t, n)| *t == A && w.buckets[&(*t, n.clone())].project == P)
                .map(|(_, n)| n.clone())
                .collect();
            for _ in 0..rng.below(3) {
                let name = names[rng.below(names.len() as u64) as usize].to_string();
                if !ours.contains(&name) && !w.buckets.contains_key(&(B_, name.clone())) {
                    let table = w.next_table;
                    w.next_table += 1;
                    objects.insert(table, 1);
                    w.buckets.insert(
                        (B_, name),
                        B {
                            table,
                            project: 9,
                            objects: 1,
                        },
                    );
                }
            }
            let fake = Fake(RefCell::new(w));
            let mut outcome = None;
            let mut cancel_at = (rng.below(6) == 0).then(|| rng.below(4));
            let mut clash_at = (rng.below(6) == 0).then(|| rng.below(4));
            for round in 0..50u64 {
                // Between attempts: writes, creates (a straggler of the
                // project under A before its key was retargeted), maybe a
                // cancel or a clash.
                {
                    let mut w = fake.0.borrow_mut();
                    for _ in 0..rng.below(4) {
                        let tables: Vec<u64> = w.buckets.values().map(|b| b.table).collect();
                        if let Some(&t) = tables.get(rng.below(tables.len().max(1) as u64) as usize)
                        {
                            *objects.get_mut(&t).unwrap() += 1;
                            for b in w.buckets.values_mut().filter(|b| b.table == t) {
                                b.objects += 1;
                            }
                        }
                    }
                    if w.moving && rng.below(3) == 0 {
                        make(&mut w, &mut objects, A, P, &mut rng);
                    }
                    if cancel_at == Some(round) {
                        w.cancelled = true;
                        cancel_at = None;
                    }
                    if clash_at == Some(round) {
                        // Someone makes, in B, a name the project's bucket
                        // still under A has.
                        let left: Vec<String> = w
                            .buckets
                            .iter()
                            .filter(|((t, _), b)| *t == A && b.project == P)
                            .map(|((_, n), _)| n.clone())
                            .collect();
                        if let Some(n) = left.first()
                            && !w.buckets.contains_key(&(B_, n.clone()))
                        {
                            let table = w.next_table;
                            w.next_table += 1;
                            objects.insert(table, 0);
                            w.buckets.insert(
                                (B_, n.clone()),
                                B {
                                    table,
                                    project: 9,
                                    objects: 0,
                                },
                            );
                        }
                        clash_at = None;
                    }
                    w.budget = (rng.below(3) != 0).then(|| rng.below(8) as u32);
                }
                match block_on(run(&fake, P, A, B_)) {
                    Err(Stop::Lost) => continue,
                    r => {
                        outcome = Some(r);
                        break;
                    }
                }
            }
            // A last run with no failover settles it.
            let outcome = match outcome {
                Some(r) => r,
                None => {
                    fake.0.borrow_mut().budget = None;
                    block_on(run(&fake, P, A, B_))
                }
            };
            let w = fake.0.borrow();
            assert!(!w.moving, "seed {seed}: still moving");
            let home = w.tenant;
            match &outcome {
                Ok(_) => assert_eq!(home, B_, "seed {seed}"),
                Err(Stop::Failed(_)) => assert_eq!(home, A, "seed {seed}"),
                Err(Stop::Lost) => unreachable!(),
            }
            // Every bucket of the project is under its tenant, but one
            // whose name was taken there (stays where it was, reported).
            let away: Vec<_> = w
                .buckets
                .iter()
                .filter(|((t, _), b)| b.project == P && *t != home)
                .collect();
            if outcome.is_ok() {
                assert!(away.is_empty(), "seed {seed}: {away:?}");
            } else {
                for ((t, n), _) in &away {
                    let other = if *t == A { B_ } else { A };
                    assert!(
                        w.buckets.contains_key(&(other, n.clone())),
                        "seed {seed}: {n} away with nothing in its way"
                    );
                }
            }
            // Others' buckets never moved; no object lost or doubled.
            assert!(
                w.buckets
                    .iter()
                    .all(|((t, _), b)| b.project == P || (b.project == OTHER) == (*t == A)),
                "seed {seed}: {:?}",
                w.buckets
            );
            let mut seen = std::collections::BTreeSet::new();
            for b in w.buckets.values() {
                assert!(seen.insert(b.table), "seed {seed}: table {} twice", b.table);
                assert_eq!(b.objects, objects[&b.table], "seed {seed}");
            }
            assert_eq!(seen.len(), objects.len(), "seed {seed}: a bucket lost");
        }
    }
}
