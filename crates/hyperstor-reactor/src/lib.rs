//! The per-core reactor (spec/design.md §6.1, D3).
//!
//! One thread, one io_uring, one executor. [`block_on`] runs a future and
//! everything it [`spawn`]s on the current thread until it completes.
//! Futures are not `Send`: a task stays on the core that created it, as a
//! PG stays on its reactor.
//!
//! **Ownership of buffers.** An I/O operation takes ownership of its buffer
//! and gives it back with the result (as in `tokio-uring`). If the future
//! is dropped before the kernel completes the operation, the reactor keeps
//! the buffer until the completion arrives, so the kernel never writes into
//! freed memory.
//!
//! **Waking from other threads** ([`chan`], or any `Waker` cloned to
//! another thread) pushes the task on a shared queue and writes an eventfd
//! the ring always has a read armed on, so a reactor sleeping in
//! `io_uring_enter` wakes up.

pub mod chan;
pub mod net;
pub mod time;

use io_uring::{IoUring, opcode, squeue, types};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;

const EVENTFD_TOKEN: u64 = u64::MAX;
/// Completions of cancel requests, which carry no operation of their own.
const CANCEL_TOKEN: u64 = u64::MAX - 1;
const RING_ENTRIES: u32 = 1024;

/// What an operation keeps alive until the kernel is done with it.
pub(crate) enum Resource {
    None,
    Buf(Vec<u8>),
    /// Kept alive for the kernel, which reads it during the operation.
    Addr(#[allow(dead_code)] Box<(libc::sockaddr_storage, libc::socklen_t)>),
    /// Kept alive for the kernel, which reads it during the operation.
    Timespec(#[allow(dead_code)] Box<types::Timespec>),
}

struct OpSlot {
    result: Option<i32>,
    waker: Option<Waker>,
    res: Resource,
    /// The future was dropped; free the slot when the completion arrives.
    abandoned: bool,
}

/// Wake-ups shared with other threads.
struct Shared {
    ready: Mutex<VecDeque<usize>>,
    efd: OwnedFd,
    owner: ThreadId,
}

impl Shared {
    fn push(&self, task: usize) {
        self.ready.lock().unwrap().push_back(task);
        if std::thread::current().id() != self.owner {
            let one: u64 = 1;
            // SAFETY: writes 8 bytes from a valid u64 to an eventfd we own.
            unsafe {
                libc::write(
                    self.efd.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    8,
                );
            }
        }
    }
}

struct TaskWaker {
    task: usize,
    shared: Arc<Shared>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.shared.push(self.task);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.shared.push(self.task);
    }
}

type Task = Pin<Box<dyn Future<Output = ()>>>;

pub(crate) struct Inner {
    ring: RefCell<IoUring>,
    ops: RefCell<Vec<Option<OpSlot>>>,
    free_ops: RefCell<Vec<usize>>,
    tasks: RefCell<Vec<Option<Task>>>,
    free_tasks: RefCell<Vec<usize>>,
    shared: Arc<Shared>,
    efd_buf: RefCell<Box<[u8; 8]>>,
    /// Operations submitted but not yet completed (abandoned ones included).
    in_flight: Cell<usize>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

pub(crate) fn current() -> Rc<Inner> {
    CURRENT.with(|c| {
        c.borrow()
            .clone()
            .expect("called outside a hyperstor reactor (use block_on)")
    })
}

impl Inner {
    fn new() -> io::Result<Rc<Self>> {
        let ring = IoUring::new(RING_ENTRIES)?;
        // SAFETY: eventfd returns a new descriptor or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor that nothing else owns.
        let efd = unsafe { OwnedFd::from_raw_fd(fd) };
        let inner = Rc::new(Inner {
            ring: RefCell::new(ring),
            ops: RefCell::new(Vec::new()),
            free_ops: RefCell::new(Vec::new()),
            tasks: RefCell::new(Vec::new()),
            free_tasks: RefCell::new(Vec::new()),
            shared: Arc::new(Shared {
                ready: Mutex::new(VecDeque::new()),
                efd,
                owner: std::thread::current().id(),
            }),
            efd_buf: RefCell::new(Box::new([0; 8])),
            in_flight: Cell::new(0),
        });
        inner.arm_eventfd();
        Ok(inner)
    }

    fn arm_eventfd(&self) {
        let mut b = self.efd_buf.borrow_mut();
        let sqe = opcode::Read::new(types::Fd(self.shared.efd.as_raw_fd()), b.as_mut_ptr(), 8)
            .build()
            .user_data(EVENTFD_TOKEN);
        // SAFETY: the 8-byte buffer is boxed and lives as long as Inner.
        unsafe { self.push_sqe(&sqe) };
    }

    /// Pushes an SQE, submitting first if the queue is full.
    ///
    /// # Safety
    /// Whatever the SQE points to must stay valid until its completion.
    unsafe fn push_sqe(&self, sqe: &squeue::Entry) {
        let mut ring = self.ring.borrow_mut();
        // SAFETY: the caller guarantees the SQE's pointers stay valid.
        while unsafe { ring.submission().push(sqe) }.is_err() {
            ring.submit().expect("io_uring submit");
        }
    }

    /// Starts an operation. `sqe` must point only into `res` (or into
    /// memory that outlives the operation).
    pub(crate) fn submit(self: &Rc<Self>, sqe: squeue::Entry, res: Resource) -> Op {
        let id = match self.free_ops.borrow_mut().pop() {
            Some(i) => i,
            None => {
                let mut ops = self.ops.borrow_mut();
                ops.push(None);
                ops.len() - 1
            }
        };
        self.ops.borrow_mut()[id] = Some(OpSlot {
            result: None,
            waker: None,
            res,
            abandoned: false,
        });
        self.in_flight.set(self.in_flight.get() + 1);
        let sqe = sqe.user_data(id as u64);
        // SAFETY: the SQE points into `res`, now owned by the slot until the
        // completion arrives (see Op::drop), or into memory the caller keeps.
        unsafe { self.push_sqe(&sqe) };
        Op {
            inner: self.clone(),
            id,
            done: false,
        }
    }

    fn complete(&self) {
        let mut cqes = Vec::new();
        {
            let mut ring = self.ring.borrow_mut();
            for cqe in ring.completion() {
                cqes.push((cqe.user_data(), cqe.result()));
            }
        }
        for (ud, res) in cqes {
            if ud == EVENTFD_TOKEN {
                self.arm_eventfd();
                continue;
            }
            if ud == CANCEL_TOKEN {
                continue;
            }
            let id = ud as usize;
            self.in_flight.set(self.in_flight.get() - 1);
            let mut ops = self.ops.borrow_mut();
            let slot = ops[id].as_mut().expect("completion for a live op");
            if slot.abandoned {
                ops[id] = None;
                drop(ops);
                self.free_ops.borrow_mut().push(id);
                continue;
            }
            slot.result = Some(res);
            if let Some(w) = slot.waker.take() {
                drop(ops);
                w.wake();
            }
        }
    }

    fn spawn_task(&self, fut: Task) -> usize {
        let id = match self.free_tasks.borrow_mut().pop() {
            Some(i) => i,
            None => {
                let mut t = self.tasks.borrow_mut();
                t.push(None);
                t.len() - 1
            }
        };
        self.tasks.borrow_mut()[id] = Some(fut);
        self.shared.push(id);
        id
    }

    fn poll_task(&self, id: usize) {
        let Some(mut fut) = self.tasks.borrow_mut().get_mut(id).and_then(Option::take) else {
            return; // already finished, or a stale wake-up
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            task: id,
            shared: self.shared.clone(),
        }));
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(()) => self.free_tasks.borrow_mut().push(id),
            Poll::Pending => self.tasks.borrow_mut()[id] = Some(fut),
        }
    }
}

/// A submitted operation: resolves to the kernel's result and the
/// resources it held.
pub(crate) struct Op {
    inner: Rc<Inner>,
    id: usize,
    done: bool,
}

impl Future for Op {
    type Output = (i32, Resource);
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut ops = self.inner.ops.borrow_mut();
        let slot = ops[self.id].as_mut().expect("live op");
        match slot.result {
            Some(r) => {
                let res = std::mem::replace(&mut slot.res, Resource::None);
                ops[self.id] = None;
                drop(ops);
                self.inner.free_ops.borrow_mut().push(self.id);
                self.done = true;
                Poll::Ready((r, res))
            }
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut ops = self.inner.ops.borrow_mut();
        let Some(slot) = ops[self.id].as_mut() else {
            return;
        };
        if slot.result.is_some() {
            // Completed but never polled: free it now.
            ops[self.id] = None;
            drop(ops);
            self.inner.free_ops.borrow_mut().push(self.id);
        } else {
            // Still in the kernel: the reactor frees it on completion. Ask
            // the kernel to cancel it, so a pending recv doesn't wait forever.
            slot.abandoned = true;
            slot.waker = None;
            drop(ops);
            let cancel = opcode::AsyncCancel::new(self.id as u64)
                .build()
                .user_data(CANCEL_TOKEN);
            // SAFETY: a cancel request points at no memory.
            unsafe { self.inner.push_sqe(&cancel) };
        }
    }
}

/// Handle to a spawned task's result.
pub struct JoinHandle<T> {
    state: Rc<RefCell<(Option<T>, Option<Waker>)>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut s = self.state.borrow_mut();
        match s.0.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> JoinHandle<T> {
    /// True once the task has finished.
    pub fn is_finished(&self) -> bool {
        self.state.borrow().0.is_some()
    }
}

/// Spawns a task on the current reactor.
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let state = Rc::new(RefCell::new((None, None::<Waker>)));
    let st = state.clone();
    current().spawn_task(Box::pin(async move {
        let v = fut.await;
        let w = {
            let mut s = st.borrow_mut();
            s.0 = Some(v);
            s.1.take()
        };
        if let Some(w) = w {
            w.wake();
        }
    }));
    JoinHandle { state }
}

/// Yields to other ready tasks once.
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

/// Runs `fut` on a new reactor on this thread, with everything it spawns,
/// until `fut` completes. Tasks still running then are dropped.
pub fn block_on<F: Future + 'static>(fut: F) -> F::Output
where
    F::Output: 'static,
{
    let inner = Inner::new().expect("create io_uring reactor");
    CURRENT.with(|c| {
        assert!(c.borrow().is_none(), "nested block_on");
        *c.borrow_mut() = Some(inner.clone());
    });
    let main = spawn(fut);
    let out = loop {
        // Run the tasks that are ready now. Tasks woken while they run
        // (including ones that yield) wait for the next pass, after I/O
        // completions have been collected: running until the queue is
        // empty would let a yielding task starve all I/O.
        let n = inner.shared.ready.lock().unwrap().len();
        for _ in 0..n {
            let next = inner.shared.ready.lock().unwrap().pop_front();
            match next {
                Some(id) => inner.poll_task(id),
                None => break,
            }
        }
        if let Some(v) = main.state.borrow_mut().0.take() {
            break v;
        }
        // Submit, and sleep in the kernel until something completes.
        {
            let ring = inner.ring.borrow_mut();
            let r = if inner.shared.ready.lock().unwrap().is_empty() {
                ring.submit_and_wait(1)
            } else {
                ring.submit()
            };
            match r {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
                Err(e) => panic!("io_uring_enter: {e}"),
            }
        }
        inner.complete();
    };
    // Drop the remaining tasks while the reactor is still current (their
    // operations unregister themselves), then wait out operations still
    // in the kernel so no buffer is freed under it.
    let tasks: Vec<Task> = inner.tasks.borrow_mut().drain(..).flatten().collect();
    drop(tasks);
    while inner.in_flight.get() > 0 {
        {
            let ring = inner.ring.borrow_mut();
            let _ = ring.submit_and_wait(1);
        }
        inner.complete();
    }
    CURRENT.with(|c| *c.borrow_mut() = None);
    out
}

/// Turns a negative io_uring result into an error.
pub(crate) fn check(r: i32) -> io::Result<i32> {
    if r < 0 {
        Err(io::Error::from_raw_os_error(-r))
    } else {
        Ok(r)
    }
}

/// Pins the calling thread to one CPU.
pub fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    // SAFETY: cpu_set_t is plain data; CPU_SET writes within it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_join() {
        let v = block_on(async {
            let hs: Vec<_> = (0..100u64)
                .map(|i| {
                    spawn(async move {
                        yield_now().await;
                        i * 2
                    })
                })
                .collect();
            let mut sum = 0;
            for h in hs {
                sum += h.await;
            }
            sum
        });
        assert_eq!(v, 9900);
    }

    #[test]
    fn a_yielding_task_does_not_starve_io() {
        block_on(async {
            let stop = Rc::new(Cell::new(false));
            let s2 = stop.clone();
            // Spins with yield_now until a timer, which needs I/O
            // completions, fires.
            spawn(async move {
                while !s2.get() {
                    yield_now().await;
                }
            });
            crate::time::sleep(std::time::Duration::from_millis(20)).await;
            stop.set(true);
        });
    }

    #[test]
    fn reactors_on_many_threads() {
        let ts: Vec<_> = (0..4)
            .map(|i| std::thread::spawn(move || block_on(async move { i + 1 })))
            .collect();
        let s: i32 = ts.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(s, 10);
    }
}
