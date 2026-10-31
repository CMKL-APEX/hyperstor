//! A reactor's shard, used by one commit at a time (I199).
//!
//! A commit round changes the index when it is planned and is durable only
//! once its writes and flush complete, which the reactor waits for without
//! holding its thread. Meanwhile nothing else may use the shard: it would
//! read blocks not yet written, or show state that isn't durable yet.
//! [`ShardCell::get`] waits for the commit to end; [`ShardCell::borrow`]
//! panics during one, so a path that forgot to wait fails in tests rather
//! than serving wrong data.

use hyperstor_engine::Shard;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::task::{Poll, Waker};

pub struct ShardCell {
    cell: RefCell<Shard>,
    busy: Cell<bool>,
    waiting: RefCell<Vec<Waker>>,
}

impl ShardCell {
    pub fn new(shard: Shard) -> Self {
        Self {
            cell: RefCell::new(shard),
            busy: Cell::new(false),
            waiting: RefCell::new(Vec::new()),
        }
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
