//! Bytes a connection's requests may hold in buffers at once (I375).
//!
//! Each request holds a buffer of its data while it runs (up to
//! [`crate::blk::REQUEST_MAX`]), and a connection may have a queue's size
//! of requests in flight on each of its queues: without a bound, a guest
//! could make the service hold gigabytes. A request takes its buffer's
//! bytes from its connection's [`Budget`] first and gives them back when
//! its buffer is dropped; one that doesn't fit waits, in arrival order,
//! for others to finish. It is never refused, and a request alone always
//! runs (one larger than the whole budget takes all of it), so requests
//! can't deadlock on it.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// Bytes one connection's requests may hold in buffers at once: eight
/// requests of [`crate::blk::REQUEST_MAX`]. Linux guests send at most
/// 1280 KiB per request (`max_sectors_kb`), so 51 of theirs fit, more
/// than a queue keeps busy; a guest sending larger ones waits for its own
/// earlier ones, not for other guests.
pub const CONNECTION_BYTES: u64 = 64 << 20;

/// A connection's budget of bytes in buffers. Shared by the connection's
/// queues, and `Send`, as a connection moves between reactors (D292).
#[derive(Clone)]
pub struct Budget(Arc<Mutex<Inner>>);

struct Inner {
    max: u64,
    used: u64,
    /// Requests waiting, first come first: their ticket and waker.
    waiting: VecDeque<(u64, Option<Waker>)>,
    next: u64,
}

impl Budget {
    pub fn new(max: u64) -> Budget {
        Budget(Arc::new(Mutex::new(Inner {
            max,
            used: 0,
            waiting: VecDeque::new(),
            next: 0,
        })))
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes `bytes` (at most the whole budget), waiting until they fit.
    pub fn take(&self, bytes: u64) -> Take {
        Take {
            budget: self.clone(),
            bytes,
            ticket: None,
        }
    }

    /// Bytes taken now.
    pub fn used(&self) -> u64 {
        self.inner().used
    }

    /// Requests waiting now.
    pub fn waiting(&self) -> usize {
        self.inner().waiting.len()
    }
}

impl Inner {
    /// Wakes the first waiter, which takes its bytes if they fit and wakes
    /// the next in turn.
    fn wake_first(&mut self) {
        if let Some((_, Some(w))) = self.waiting.front() {
            w.wake_by_ref();
        }
    }
}

/// [`Budget::take`]'s future.
pub struct Take {
    budget: Budget,
    bytes: u64,
    /// Its place in line, once it had to wait.
    ticket: Option<u64>,
}

impl Future for Take {
    type Output = Taken;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Taken> {
        let budget = self.budget.clone();
        let mut b = budget.inner();
        let bytes = self.bytes.min(b.max);
        let first = match self.ticket {
            None => b.waiting.is_empty(),
            Some(t) => b.waiting.front().is_some_and(|w| w.0 == t),
        };
        if first && (b.used == 0 || b.used + bytes <= b.max) {
            b.used += bytes;
            if self.ticket.take().is_some() {
                b.waiting.pop_front();
                // The next may fit too.
                b.wake_first();
            }
            return Poll::Ready(Taken {
                budget: self.budget.clone(),
                bytes,
            });
        }
        let waker = Some(cx.waker().clone());
        match self.ticket {
            Some(t) => {
                if let Some(w) = b.waiting.iter_mut().find(|w| w.0 == t) {
                    w.1 = waker;
                }
            }
            None => {
                let t = b.next;
                b.next += 1;
                b.waiting.push_back((t, waker));
                drop(b);
                self.ticket = Some(t);
            }
        }
        Poll::Pending
    }
}

impl Drop for Take {
    /// A request that stops waiting (its ring stopped) leaves the line.
    fn drop(&mut self) {
        if let Some(t) = self.ticket {
            let mut b = self.budget.inner();
            let was_first = b.waiting.front().is_some_and(|w| w.0 == t);
            b.waiting.retain(|w| w.0 != t);
            if was_first {
                b.wake_first();
            }
        }
    }
}

/// Bytes taken from a [`Budget`], given back when dropped.
pub struct Taken {
    budget: Budget,
    bytes: u64,
}

impl Drop for Taken {
    fn drop(&mut self) {
        let mut b = self.budget.inner();
        b.used -= self.bytes;
        b.wake_first();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll(f: &mut Take, w: &Arc<Count>) -> Option<Taken> {
        let waker = Waker::from(w.clone());
        match Pin::new(f).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(t) => Some(t),
            Poll::Pending => None,
        }
    }

    fn woken(w: &Arc<Count>) -> usize {
        w.0.swap(0, Ordering::SeqCst)
    }

    #[test]
    fn requests_wait_for_room_in_arrival_order() {
        let b = Budget::new(10);
        let w = Arc::new(Count(AtomicUsize::new(0)));
        let a = poll(&mut b.take(6), &w).expect("fits");
        let mut big = b.take(6);
        assert!(poll(&mut big, &w).is_none(), "6 + 6 > 10: waits");
        // A small one that would fit waits behind the first in line, so
        // large requests aren't passed over for ever.
        let mut small = b.take(1);
        assert!(poll(&mut small, &w).is_none(), "behind the waiting one");
        assert_eq!((b.used(), b.waiting()), (6, 2));
        drop(a);
        assert_eq!(woken(&w), 1, "the first in line is woken");
        let big = poll(&mut big, &w).expect("room now");
        assert_eq!(woken(&w), 1, "and wakes the next");
        let s = poll(&mut small, &w).expect("6 + 1 fits");
        assert_eq!((b.used(), b.waiting()), (7, 0));
        drop((big, s));
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn a_request_alone_always_runs() {
        let b = Budget::new(10);
        let w = Arc::new(Count(AtomicUsize::new(0)));
        let all = poll(&mut b.take(25), &w).expect("alone, larger than the budget");
        assert_eq!(b.used(), 10, "takes the whole budget");
        let mut next = b.take(1);
        assert!(poll(&mut next, &w).is_none());
        drop(all);
        assert!(poll(&mut next, &w).is_some());
        // Nothing taken: never waits, even behind others.
        let held = poll(&mut b.take(10), &w).unwrap();
        assert!(poll(&mut b.take(0), &w).is_some());
        drop(held);
    }

    #[test]
    fn a_waiter_that_gives_up_leaves_the_line() {
        let b = Budget::new(10);
        let w = Arc::new(Count(AtomicUsize::new(0)));
        let a = poll(&mut b.take(8), &w).unwrap();
        let mut first = b.take(8);
        let mut second = b.take(2);
        assert!(poll(&mut first, &w).is_none());
        assert!(poll(&mut second, &w).is_none());
        drop(first);
        assert_eq!(woken(&w), 1, "the next is first now");
        assert!(poll(&mut second, &w).is_some(), "8 + 2 fits");
        assert_eq!(b.waiting(), 0);
        drop(a);
    }

    /// Many requests on a reactor, each holding its bytes a while: never
    /// more than the budget at once, and every one finishes.
    #[test]
    fn many_requests_keep_to_the_budget() {
        let b = Budget::new(64);
        let peak = Rc::new(std::cell::Cell::new(0));
        let done = Rc::new(std::cell::Cell::new(0));
        let (b2, done2) = (b.clone(), done.clone());
        let peak2 = peak.clone();
        hyperstor_reactor::block_on(async move {
            let (b, peak, done) = (b2, peak2, done2);
            for i in 0..200u64 {
                let (b, peak, done) = (b.clone(), peak.clone(), done.clone());
                hyperstor_reactor::spawn(async move {
                    let _t = b.take(1 + i % 40).await;
                    peak.set(peak.get().max(b.used()));
                    hyperstor_reactor::time::sleep(std::time::Duration::from_micros(200)).await;
                    done.set(done.get() + 1);
                });
            }
            while done.get() < 200 {
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });
        assert!(peak.get() <= 64, "{}", peak.get());
        assert_eq!(b.used(), 0);
    }

    use std::rc::Rc;
}
