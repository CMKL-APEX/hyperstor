//! The per-core reactor (spec/08-engine.md §6.1, D3).
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
pub mod fd;
pub mod file;
pub mod net;
pub mod passthrough;
pub mod time;

use io_uring::{IoUring, opcode, squeue, types};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::Location;
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
    /// Buffers several operations write from (`file`), shared among them
    /// and freed with the last one's completion.
    Keep(#[allow(dead_code)] Rc<dyn std::any::Any>),
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

/// Where a task was spawned, to name it when it holds the thread.
type Site = &'static Location<'static>;

/// The longest the reactor on this thread spent in one place since the
/// last [`take_slowest`]: what to blame when its thread is held.
#[derive(Clone, Copy, Debug, Default)]
pub struct Slowest {
    /// The longest poll of one task, and where that task was spawned.
    pub poll: Option<(std::time::Duration, Site)>,
    /// The longest `io_uring_enter`. It waits for completions too, so
    /// only one longer than the soonest timer means it was held:
    /// submitting work the kernel runs inline.
    pub enter: std::time::Duration,
}

/// Takes, and starts afresh, what [`Slowest`] has seen on this thread's
/// reactor.
pub fn take_slowest() -> Slowest {
    current().slowest.take()
}

/// What the reactor on this thread has done since it started (D41): the
/// passes of its loop, its calls into the kernel, and its spins, the
/// stretches it polled idle instead of sleeping (`set_spin`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Passes of the loop: the tasks ready run, then one `io_uring_enter`.
    pub passes: u64,
    /// `io_uring_enter` calls from the loop; one a pass.
    pub enters: u64,
    /// Of those, the ones that waited for a completion.
    pub sleeps: u64,
    /// Spins that ended with work: a completion or a task woken.
    pub spin_hits: u64,
    /// Spins whose window ran out, followed by a sleep.
    pub spin_misses: u64,
    /// Time spent spinning, hits and misses, in nanoseconds.
    pub spin_ns: u64,
    /// The spin window now, in nanoseconds: `set_spin`'s, or where the
    /// adaptive window stands.
    pub window_ns: u64,
}

/// The [`Stats`] of the reactor on this thread so far.
pub fn stats() -> Stats {
    current().stats.get()
}

pub(crate) struct Inner {
    ring: RefCell<IoUring>,
    ops: RefCell<Vec<Option<OpSlot>>>,
    free_ops: RefCell<Vec<usize>>,
    /// Each task with where it was spawned and its waker, made once.
    tasks: RefCell<Vec<Option<(Task, Site, Waker)>>>,
    free_tasks: RefCell<Vec<usize>>,
    shared: Arc<Shared>,
    efd_buf: RefCell<Box<[u8; 8]>>,
    /// Operations submitted but not yet completed (abandoned ones included).
    in_flight: Cell<usize>,
    slowest: Cell<Slowest>,
    stats: Cell<Stats>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
    static SPIN: Cell<std::time::Duration> = const { Cell::new(std::time::Duration::ZERO) };
    static SPIN_ADAPTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Adaptive polling (04-decisions.md D41): after any activity, the reactor on
/// this thread keeps polling for completions for `spin` before it sleeps
/// in the kernel, so a request arriving soon after the last one doesn't
/// pay a thread wake-up. Polling still enters the kernel (without waiting)
/// so network completions are delivered. Zero, the default, turns it off;
/// storage reactors that own their cores turn it on. It takes effect on
/// the reactor's next pass, so a task may change it.
pub fn set_spin(spin: std::time::Duration) {
    SPIN.with(|s| s.set(spin));
}

/// Makes `set_spin`'s window a ceiling the reactor on this thread adapts
/// under, as KVM's halt polling does: after an idle stretch longer than
/// the window but within the ceiling (a longer window would have caught
/// it) the window doubles, from [`SPIN_GROW_START`]; after one longer than
/// the ceiling (no window would have) it halves, to zero under the start.
/// Off by default.
pub fn set_spin_adaptive(on: bool) {
    SPIN_ADAPTIVE.with(|s| s.set(on));
}

/// The smallest adaptive window; below it the reactor doesn't spin.
pub const SPIN_GROW_START: std::time::Duration = std::time::Duration::from_micros(10);

/// The adaptive window after an idle stretch of `idle` (see
/// [`set_spin_adaptive`]).
fn adapt_window(
    window: std::time::Duration,
    ceiling: std::time::Duration,
    idle: std::time::Duration,
) -> std::time::Duration {
    if idle <= window {
        window
    } else if idle <= ceiling {
        (window * 2).max(SPIN_GROW_START).min(ceiling)
    } else if window / 2 < SPIN_GROW_START {
        std::time::Duration::ZERO
    } else {
        window / 2
    }
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
            slowest: Cell::new(Slowest::default()),
            stats: Cell::new(Stats::default()),
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

    /// Handles every completion queued; returns how many there were.
    fn complete(&self) -> usize {
        let mut cqes = Vec::new();
        {
            let mut ring = self.ring.borrow_mut();
            for cqe in ring.completion() {
                cqes.push((cqe.user_data(), cqe.result()));
            }
        }
        let n = cqes.len();
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
        n
    }

    fn spawn_task(&self, fut: Task, site: Site) -> usize {
        let id = match self.free_tasks.borrow_mut().pop() {
            Some(i) => i,
            None => {
                let mut t = self.tasks.borrow_mut();
                t.push(None);
                t.len() - 1
            }
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            task: id,
            shared: self.shared.clone(),
        }));
        self.tasks.borrow_mut()[id] = Some((fut, site, waker));
        self.shared.push(id);
        id
    }

    /// Polls task `id` once. `t` is when the poll starts, and becomes when
    /// it ended: one clock read a poll (D41), the next poll's start.
    fn poll_task(&self, id: usize, t: &mut std::time::Instant) {
        let Some((mut fut, site, waker)) =
            self.tasks.borrow_mut().get_mut(id).and_then(Option::take)
        else {
            return; // already finished, or a stale wake-up
        };
        let mut cx = Context::from_waker(&waker);
        let r = fut.as_mut().poll(&mut cx);
        let end = std::time::Instant::now();
        let took = end - *t;
        *t = end;
        let mut s = self.slowest.get();
        if s.poll.is_none_or(|(d, _)| took > d) {
            s.poll = Some((took, site));
            self.slowest.set(s);
        }
        match r {
            Poll::Ready(()) => self.free_tasks.borrow_mut().push(id),
            Poll::Pending => self.tasks.borrow_mut()[id] = Some((fut, site, waker)),
        }
    }

    fn note_enter(&self, took: std::time::Duration) {
        let mut s = self.slowest.get();
        if took > s.enter {
            s.enter = took;
            self.slowest.set(s);
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
#[track_caller]
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let site = Location::caller();
    let state = Rc::new(RefCell::new((None, None::<Waker>)));
    let st = state.clone();
    current().spawn_task(
        Box::pin(async move {
            let v = fut.await;
            let w = {
                let mut s = st.borrow_mut();
                s.0 = Some(v);
                s.1.take()
            };
            if let Some(w) = w {
                w.wake();
            }
        }),
        site,
    );
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
#[track_caller]
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
    // The tasks to poll this pass, taken from the ready queue in one lock.
    let mut batch: Vec<usize> = Vec::new();
    // The clock is read once after each call into the kernel and once
    // after each task's poll (D41); the spin window, the longest poll and
    // the longest enter are all measured from these reads.
    let mut now = std::time::Instant::now();
    let mut last_activity = now;
    // Since when the reactor has been polling idle, while it spins.
    let mut spinning: Option<std::time::Instant> = None;
    // Since when it has been idle, spinning or asleep (the adaptive window).
    let mut idle_since: Option<std::time::Instant> = None;
    let mut window = SPIN.with(|s| s.get());
    let out = loop {
        // Read each pass: a task may turn it on or off (D293).
        let ceiling = SPIN.with(|s| s.get());
        let adaptive = SPIN_ADAPTIVE.with(|s| s.get());
        if !adaptive || window > ceiling {
            window = ceiling;
        }
        let spin = window;
        let mut st = inner.stats.get();
        st.passes += 1;
        // Run the tasks that are ready now. Tasks woken while they run
        // (including ones that yield) wait for the next pass, after I/O
        // completions have been collected: running until the queue is
        // empty would let a yielding task starve all I/O.
        batch.extend(inner.shared.ready.lock().unwrap().drain(..));
        if !batch.is_empty() {
            last_activity = now;
            end_spin(&mut st, &mut spinning, now, true);
            if let Some(from) = idle_since.take()
                && adaptive
            {
                window = adapt_window(window, ceiling, now.duration_since(from));
            }
            let mut t = std::time::Instant::now();
            for id in batch.drain(..) {
                inner.poll_task(id, &mut t);
            }
            now = t;
        }
        if let Some(v) = main.state.borrow_mut().0.take() {
            inner.stats.set(st);
            break v;
        }
        // Submit, and sleep in the kernel until something completes, unless
        // there was activity within the spin window: then only poll.
        {
            let ring = inner.ring.borrow_mut();
            let idle = inner.shared.ready.lock().unwrap().is_empty();
            let sleep = idle && (spin.is_zero() || now.duration_since(last_activity) >= spin);
            if sleep {
                end_spin(&mut st, &mut spinning, now, false);
                st.sleeps += 1;
            } else if idle && spinning.is_none() {
                spinning = Some(now);
            }
            if idle && idle_since.is_none() {
                idle_since = Some(now);
            }
            st.enters += 1;
            let before = now;
            let r = if sleep {
                ring.submit_and_wait(1)
            } else {
                ring.submit()
            };
            now = std::time::Instant::now();
            inner.note_enter(now - before);
            match r {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
                Err(e) => panic!("io_uring_enter: {e}"),
            }
        }
        if inner.complete() > 0 {
            last_activity = now;
            end_spin(&mut st, &mut spinning, now, true);
            if let Some(from) = idle_since.take()
                && adaptive
            {
                window = adapt_window(window, ceiling, now.duration_since(from));
            }
        }
        st.window_ns = window.as_nanos() as u64;
        inner.stats.set(st);
    };
    // Drop the remaining tasks while the reactor is still current (their
    // operations unregister themselves), then wait out operations still
    // in the kernel so no buffer is freed under it.
    let tasks: Vec<(Task, Site, Waker)> = inner.tasks.borrow_mut().drain(..).flatten().collect();
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

/// Ends the spin in progress, if any: with work (`hit`) or a sleep.
fn end_spin(
    st: &mut Stats,
    spinning: &mut Option<std::time::Instant>,
    now: std::time::Instant,
    hit: bool,
) {
    if let Some(from) = spinning.take() {
        if hit {
            st.spin_hits += 1;
        } else {
            st.spin_misses += 1;
        }
        st.spin_ns += now.duration_since(from).as_nanos() as u64;
    }
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

/// The CPUs the calling thread may run on (its affinity, as `taskset` or
/// a cgroup's cpuset leave it), in ascending order.
pub fn allowed_cpus() -> io::Result<Vec<usize>> {
    // SAFETY: cpu_set_t is plain data; sched_getaffinity writes at most
    // its size, and CPU_ISSET reads within it for indices below
    // CPU_SETSIZE.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((0..libc::CPU_SETSIZE as usize)
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect())
    }
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
    fn the_longest_poll_is_named_by_where_its_task_was_spawned() {
        block_on(async {
            take_slowest();
            let line = line!() + 1;
            let h = spawn(async { std::thread::sleep(std::time::Duration::from_millis(30)) });
            spawn(async {}).await;
            h.await;
            let s = take_slowest();
            let (took, site) = s.poll.unwrap();
            assert!(took >= std::time::Duration::from_millis(30), "{s:?}");
            assert_eq!((site.file(), site.line()), (file!(), line), "{s:?}");
            // Waiting for a timer is time in io_uring_enter, not a poll.
            crate::time::sleep(std::time::Duration::from_millis(20)).await;
            let s = take_slowest();
            assert!(s.enter >= std::time::Duration::from_millis(15), "{s:?}");
            assert!(
                s.poll.unwrap().0 < std::time::Duration::from_millis(15),
                "{s:?}"
            );
        });
    }

    #[test]
    fn spinning_reactor_still_sleeps_and_wakes() {
        set_spin(std::time::Duration::from_micros(200));
        block_on(async {
            for _ in 0..20 {
                crate::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let t = std::time::Instant::now();
            crate::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(t.elapsed() >= std::time::Duration::from_millis(19));
        });
        set_spin(std::time::Duration::ZERO);
    }

    #[test]
    fn spins_are_counted_as_ended_by_work_or_by_a_sleep() {
        use std::time::Duration;
        set_spin(Duration::from_millis(5));
        block_on(async {
            let s0 = stats();
            // A timer inside the window: the reactor polls until it fires.
            crate::time::sleep(Duration::from_millis(1)).await;
            let s1 = stats();
            assert!(s1.spin_hits > s0.spin_hits, "{s0:?} {s1:?}");
            assert!(s1.passes > s0.passes && s1.enters > s0.enters, "{s1:?}");
            // A timer past the window: it polls the window out, then sleeps.
            crate::time::sleep(Duration::from_millis(30)).await;
            let s2 = stats();
            assert!(s2.spin_misses > s1.spin_misses, "{s1:?} {s2:?}");
            assert!(s2.sleeps > s1.sleeps, "{s2:?}");
            assert!(s2.spin_ns - s1.spin_ns >= 4_000_000, "{s1:?} {s2:?}");
            // Far fewer passes than a spin through the whole 30 ms would take.
            assert!(s2.enters >= s2.sleeps, "{s2:?}");
        });
        set_spin(Duration::ZERO);
        // Without a spin, an idle reactor only sleeps.
        block_on(async {
            let s0 = stats();
            crate::time::sleep(Duration::from_millis(5)).await;
            let s1 = stats();
            assert_eq!(
                (s1.spin_hits, s1.spin_misses),
                (s0.spin_hits, s0.spin_misses)
            );
            assert!(s1.sleeps > s0.sleeps, "{s1:?}");
        });
    }

    #[test]
    fn the_adaptive_window_grows_toward_idle_it_would_catch_and_shrinks_past_the_ceiling() {
        use std::time::Duration;
        let us = Duration::from_micros;
        let c = us(200);
        // Caught: unchanged.
        assert_eq!(adapt_window(us(40), c, us(30)), us(40));
        // Missed, but within the ceiling: doubles, from the start, to the ceiling.
        assert_eq!(adapt_window(Duration::ZERO, c, us(50)), SPIN_GROW_START);
        assert_eq!(adapt_window(us(40), c, us(60)), us(80));
        assert_eq!(adapt_window(us(160), c, us(190)), c);
        // Past the ceiling: halves, then off.
        assert_eq!(adapt_window(c, c, us(900)), us(100));
        assert_eq!(adapt_window(us(15), c, us(900)), Duration::ZERO);
        assert_eq!(adapt_window(Duration::ZERO, c, us(900)), Duration::ZERO);
        // No ceiling, no spin.
        assert_eq!(
            adapt_window(Duration::ZERO, Duration::ZERO, us(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn an_adaptive_reactor_stops_spinning_when_idle_outlasts_the_ceiling() {
        use std::time::Duration;
        set_spin(Duration::from_millis(2));
        set_spin_adaptive(true);
        block_on(async {
            // Every idle stretch 10 ms, past the 2 ms ceiling: the window
            // halves to zero within a few, and then the reactor only sleeps.
            for _ in 0..12 {
                crate::time::sleep(Duration::from_millis(10)).await;
            }
            let s0 = stats();
            assert_eq!(s0.window_ns, 0, "{s0:?}");
            for _ in 0..5 {
                crate::time::sleep(Duration::from_millis(10)).await;
            }
            let s1 = stats();
            assert_eq!(s1.spin_ns, s0.spin_ns, "{s0:?} {s1:?}");
            // Short idle stretches within the ceiling grow it again.
            for _ in 0..20 {
                crate::time::sleep(Duration::from_micros(300)).await;
            }
            let s2 = stats();
            assert!(s2.window_ns > 0, "{s2:?}");
            assert!(s2.spin_hits > s1.spin_hits, "{s1:?} {s2:?}");
        });
        set_spin_adaptive(false);
        set_spin(Duration::ZERO);
    }

    #[test]
    fn a_task_keeps_one_waker_across_its_polls() {
        // The reactor makes a task's waker once, when it is spawned, and
        // polls it with that one every time (I457).
        block_on(async {
            let mut first: Option<Waker> = None;
            let mut polls = 0;
            std::future::poll_fn(|cx| {
                polls += 1;
                match &first {
                    None => first = Some(cx.waker().clone()),
                    Some(w) => assert!(w.will_wake(cx.waker()), "a new waker on poll {polls}"),
                }
                if polls < 5 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
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
