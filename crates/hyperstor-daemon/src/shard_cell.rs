//! A reactor's shard, used by one commit at a time (I199).
//!
//! A commit round changes the index when it is planned and is durable only
//! once its writes and flush complete, which the reactor waits for without
//! holding its thread. Meanwhile nothing else may use the shard: it would
//! read blocks not yet written, or show state that isn't durable yet.
//! [`ShardCell::get`] waits for the commit to end; [`ShardCell::borrow`]
//! panics during one, so a path that forgot to wait fails in tests rather
//! than serving wrong data.
//!
//! Reads plan with the shard and then wait for the device on the ring
//! (D190): at most [`MAX_READS`] at a time, the rest in the order they
//! came ([`ShardCell::read_slot`]). Recovery, backfill and scrub read the
//! same way with a share of their own, [`MAX_BACKGROUND_READS`] (I257),
//! so neither kind waits behind the other.

use hyperstor_engine::{ReadPlan, Shard};
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Reads a device has in flight at most (D190): it bounds their buffers
/// and how long freed extents are held for them.
pub const MAX_READS: usize = 128;

/// Reads of recovery, backfill and scrub a device has in flight at most,
/// beside [`MAX_READS`] (I257): few, so that they can't take the device
/// from clients, and their own, so that clients can't starve them.
pub const MAX_BACKGROUND_READS: usize = 16;

/// Whose read a slot is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    /// A client's, or a member's for the primary ([`MAX_READS`]).
    Client,
    /// Recovery's, backfill's or scrub's ([`MAX_BACKGROUND_READS`]).
    Background,
}

/// The slots of one [`ReadKind`].
struct Slots {
    max: usize,
    reads: Cell<usize>,
    /// Reads waiting for a slot, in the order they came.
    queue: RefCell<VecDeque<(u64, Option<Waker>)>>,
    next: Cell<u64>,
}

impl Slots {
    fn new(max: usize) -> Self {
        Self {
            max,
            reads: Cell::new(0),
            queue: RefCell::new(VecDeque::new()),
            next: Cell::new(0),
        }
    }

    fn wake_next(&self) {
        if self.reads.get() < self.max
            && let Some((_, Some(w))) = self.queue.borrow_mut().front_mut()
        {
            w.wake_by_ref();
        }
    }
}

pub struct ShardCell {
    cell: RefCell<Shard>,
    busy: Cell<bool>,
    waiting: RefCell<Vec<Waker>>,
    client: Slots,
    background: Slots,
}

impl ShardCell {
    pub fn new(shard: Shard) -> Self {
        Self {
            cell: RefCell::new(shard),
            busy: Cell::new(false),
            waiting: RefCell::new(Vec::new()),
            client: Slots::new(MAX_READS),
            background: Slots::new(MAX_BACKGROUND_READS),
        }
    }

    fn slots(&self, kind: ReadKind) -> &Slots {
        match kind {
            ReadKind::Client => &self.client,
            ReadKind::Background => &self.background,
        }
    }

    /// A slot for one read of `kind` in flight, once fewer than its limit
    /// are and every read of its kind that came first has one.
    pub async fn read_slot(&self, kind: ReadKind) -> ReadSlot<'_> {
        let slots = self.slots(kind);
        if slots.reads.get() < slots.max && slots.queue.borrow().is_empty() {
            slots.reads.set(slots.reads.get() + 1);
            return ReadSlot { slots };
        }
        let id = slots.next.get();
        slots.next.set(id + 1);
        slots.queue.borrow_mut().push_back((id, None));
        WaitSlot {
            slots,
            id,
            done: false,
        }
        .await
    }

    /// Reads of `kind` in flight, with a slot.
    #[cfg(test)]
    pub fn reads(&self, kind: ReadKind) -> usize {
        self.slots(kind).reads.get()
    }

    /// Ends a read planned with the shard ([`Shard::read_end`]), also while
    /// a commit is in flight: it only lets extents the read held go.
    pub fn end_read(&self, plan: &ReadPlan) {
        self.cell.borrow_mut().read_end(plan);
    }

    /// Resolves once no commit is in flight. The caller must use the
    /// shard before its next `.await`, or wait again.
    pub async fn idle(&self) {
        std::future::poll_fn(|cx| {
            if self.busy.get() {
                self.waiting.borrow_mut().push(cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await
    }

    /// The shard, once no commit is in flight.
    pub async fn get(&self) -> Ref<'_, Shard> {
        self.idle().await;
        self.cell.borrow()
    }

    /// The shard to change, once no commit is in flight.
    pub async fn get_mut(&self) -> RefMut<'_, Shard> {
        self.idle().await;
        self.cell.borrow_mut()
    }

    /// The shard now, for code that runs between an [`ShardCell::idle`]
    /// and its next `.await`. Panics if a commit is in flight.
    pub fn borrow(&self) -> Ref<'_, Shard> {
        assert!(!self.busy.get(), "shard used while a commit is in flight");
        self.cell.borrow()
    }

    /// Takes the shard for a commit once none is in flight; until the
    /// returned guard drops, everyone else waits.
    pub async fn commit(&self) -> Committing<'_> {
        self.idle().await;
        self.busy.set(true);
        Committing { cell: self }
    }
}

/// A read's place among the reads of its kind in flight, given back on
/// drop.
pub struct ReadSlot<'a> {
    slots: &'a Slots,
}

impl Drop for ReadSlot<'_> {
    fn drop(&mut self) {
        self.slots.reads.set(self.slots.reads.get() - 1);
        self.slots.wake_next();
    }
}

struct WaitSlot<'a> {
    slots: &'a Slots,
    id: u64,
    done: bool,
}

impl<'a> Future for WaitSlot<'a> {
    type Output = ReadSlot<'a>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ReadSlot<'a>> {
        let slots = self.slots;
        let mut q = slots.queue.borrow_mut();
        if slots.reads.get() < slots.max && q.front().is_some_and(|(id, _)| *id == self.id) {
            q.pop_front();
            drop(q);
            slots.reads.set(slots.reads.get() + 1);
            self.done = true;
            // Another slot may be free too.
            slots.wake_next();
            return Poll::Ready(ReadSlot { slots });
        }
        if let Some(e) = q.iter_mut().find(|(id, _)| *id == self.id) {
            e.1 = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for WaitSlot<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.slots
                .queue
                .borrow_mut()
                .retain(|(id, _)| *id != self.id);
            self.slots.wake_next();
        }
    }
}

/// A commit's hold on the shard.
pub struct Committing<'a> {
    cell: &'a ShardCell,
}

impl Committing<'_> {
    /// The shard, for planning or ending a round; not across an `.await`.
    pub fn shard(&self) -> RefMut<'_, Shard> {
        self.cell.cell.borrow_mut()
    }
}

impl Drop for Committing<'_> {
    fn drop(&mut self) {
        self.cell.busy.set(false);
        for w in self.cell.waiting.borrow_mut().drain(..) {
            w.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_engine::{DeviceConfig, MemDev};
    use std::rc::Rc;
    use std::sync::Arc;

    fn shard() -> Shard {
        let dev: Arc<dyn hyperstor_engine::BlockDev> = Arc::new(MemDev::new(64 << 20));
        hyperstor_engine::format(&dev, &DeviceConfig::default()).unwrap();
        hyperstor_engine::open(&dev).unwrap().1.pop().unwrap()
    }

    #[test]
    fn readers_wait_for_a_commit_in_flight() {
        let cell = Rc::new(ShardCell::new(shard()));
        hyperstor_reactor::block_on({
            let cell = cell.clone();
            async move {
                let c = cell.commit().await;
                let reader = hyperstor_reactor::spawn({
                    let cell = cell.clone();
                    async move { cell.get().await.stats().commits }
                });
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(20)).await;
                assert!(!reader.is_finished(), "a reader ran during the commit");
                c.shard().commit_done();
                drop(c);
                assert_eq!(reader.await, 1, "the reader sees the commit's end");
            }
        });
    }

    /// D190: past [`MAX_READS`] in flight, reads wait, and get their slots
    /// in the order they came; one that gives up waiting holds no place.
    #[test]
    fn reads_past_the_limit_wait_in_order() {
        let cell = Rc::new(ShardCell::new(shard()));
        hyperstor_reactor::block_on({
            let cell = cell.clone();
            async move {
                let mut held = Vec::new();
                for _ in 0..MAX_READS {
                    held.push(cell.read_slot(ReadKind::Client).await);
                }
                let order = Rc::new(RefCell::new(Vec::new()));
                let mut waiters = Vec::new();
                for i in 0..3 {
                    let (cell, order) = (cell.clone(), order.clone());
                    waiters.push(hyperstor_reactor::spawn(async move {
                        let _s = cell.read_slot(ReadKind::Client).await;
                        order.borrow_mut().push(i);
                        hyperstor_reactor::time::sleep(std::time::Duration::from_millis(5)).await;
                    }));
                    hyperstor_reactor::yield_now().await;
                }
                // One that gives up while waiting.
                let gave_up = hyperstor_reactor::time::timeout(
                    std::time::Duration::from_millis(5),
                    cell.read_slot(ReadKind::Client),
                )
                .await;
                assert!(gave_up.is_err());
                assert_eq!(cell.reads(ReadKind::Client), MAX_READS);
                assert!(order.borrow().is_empty(), "none passed the limit");
                held.pop();
                held.pop();
                for w in waiters {
                    w.await;
                }
                assert_eq!(*order.borrow(), vec![0, 1, 2]);
                drop(held);
                assert_eq!(cell.reads(ReadKind::Client), 0);
                drop(cell.read_slot(ReadKind::Client).await);
            }
        });
    }

    /// I257: recovery's reads have their own slots: with every client
    /// slot taken they still get one, and past their own limit they wait
    /// while client reads go on.
    #[test]
    fn background_reads_have_a_share_of_their_own() {
        let cell = Rc::new(ShardCell::new(shard()));
        hyperstor_reactor::block_on({
            let cell = cell.clone();
            async move {
                let mut clients = Vec::new();
                for _ in 0..MAX_READS {
                    clients.push(cell.read_slot(ReadKind::Client).await);
                }
                let mut background = Vec::new();
                for _ in 0..MAX_BACKGROUND_READS {
                    background.push(cell.read_slot(ReadKind::Background).await);
                }
                assert_eq!(cell.reads(ReadKind::Background), MAX_BACKGROUND_READS);
                let waiting = hyperstor_reactor::spawn({
                    let cell = cell.clone();
                    async move { drop(cell.read_slot(ReadKind::Background).await) }
                });
                hyperstor_reactor::yield_now().await;
                assert!(!waiting.is_finished(), "past its own limit");
                // A client slot freed doesn't let a background read past.
                clients.pop();
                hyperstor_reactor::yield_now().await;
                assert!(!waiting.is_finished(), "took a client's slot");
                drop(cell.read_slot(ReadKind::Client).await);
                background.pop();
                waiting.await;
                drop(background);
                assert_eq!(cell.reads(ReadKind::Background), 0);
                assert_eq!(cell.reads(ReadKind::Client), MAX_READS - 1);
            }
        });
    }

    #[test]
    #[should_panic(expected = "while a commit is in flight")]
    fn using_the_shard_without_waiting_panics_during_a_commit() {
        let cell = ShardCell::new(shard());
        hyperstor_reactor::block_on(async move {
            let _c = cell.commit().await;
            let _ = cell.borrow().stats();
        });
    }
}
