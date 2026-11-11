//! The service's reactors (04-decisions.md D290, D291): one per NUMA node
//! with CPUs, each pinned to one CPU. The first runs on the thread that
//! called [`crate::serve`] and serves the control socket and every disk's
//! socket; each runs the disks placed on it, through a cluster client of
//! its own that is the same client instance as all the others, connected
//! with its first disk.

use hyperstor_client::Client;
use hyperstor_reactor::chan;
use std::cell::RefCell;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

/// Work for a reactor, run on it.
type Job = Box<dyn FnOnce() + Send>;

/// What each reactor's client connects with: one instance (D291).
pub struct ClientSpec {
    pub meta: Vec<SocketAddr>,
    pub id: u64,
    pub instance: u64,
    pub networks: Vec<String>,
    pub host: String,
}

/// One reactor, as the others see it.
pub struct Reactor {
    /// The CPU it is pinned to, if any.
    pub cpu: Option<usize>,
    /// The NUMA node of that CPU, if the host says.
    pub node: Option<u32>,
    jobs: chan::Sender<Job>,
}

pub struct Reactors {
    pub all: Vec<Reactor>,
    spec: Arc<ClientSpec>,
}

thread_local! {
    /// This reactor's client, once it has one.
    static CLIENT: RefCell<Option<Rc<Client>>> = const { RefCell::new(None) };
}

/// Pins this thread to `cpu`, saying so if it can't (a CPU outside the
/// service's cgroup, say): it then runs where the kernel puts it.
fn pin(cpu: Option<usize>) {
    if let Some(c) = cpu
        && let Err(e) = hyperstor_reactor::pin_to_cpu(c)
    {
        eprintln!("hyperstor-vhost: CPU {c}: {e}; a reactor not pinned");
    }
}

/// Sets this reactor's completion spin: on, for the polling window,
/// while it runs a ring that polls (D293); otherwise off, or what
/// `HS_VHOST_SPIN_US` says (for measurements only, 02-benchmarks.md,
/// 2026-11-11, as hyperstord's `--spin-us`).
pub(crate) fn set_spin() {
    let base = std::env::var("HS_VHOST_SPIN_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_micros)
        .unwrap_or_default();
    hyperstor_reactor::set_spin(if crate::device::polling() {
        base.max(crate::device::poll_window())
    } else {
        base
    });
}

/// Runs jobs sent to this reactor until every sender is gone.
async fn run_jobs(mut rx: chan::Receiver<Job>) {
    while let Some(j) = rx.recv().await {
        j();
    }
}

impl Reactors {
    /// Starts a reactor on each of `cpus` (with its node) but the first,
    /// which is this thread's: pinned now, its jobs run by the future
    /// returned, which the caller spawns on its reactor. `client` is this
    /// thread's client, which the others share an instance with.
    pub fn start(
        cpus: &[(Option<usize>, Option<u32>)],
        spec: ClientSpec,
        client: Rc<Client>,
    ) -> (Reactors, impl Future<Output = ()> + use<>) {
        assert!(!cpus.is_empty());
        let mut all = Vec::new();
        let mut first = None;
        for (i, &(cpu, node)) in cpus.iter().enumerate() {
            let (tx, rx) = chan::channel::<Job>();
            if i == 0 {
                first = Some(rx);
            } else {
                std::thread::Builder::new()
                    .name(format!("hs-vhost-{i}"))
                    .spawn(move || {
                        pin(cpu);
                        set_spin();
                        hyperstor_reactor::block_on(run_jobs(rx));
                    })
                    .expect("a reactor's thread");
            }
            all.push(Reactor {
                cpu,
                node,
                jobs: tx,
            });
        }
        // After the others start: they mustn't inherit its CPU.
        pin(cpus[0].0);
        CLIENT.with(|c| *c.borrow_mut() = Some(client));
        set_spin();
        let r = Reactors {
            all,
            spec: Arc::new(spec),
        };
        (r, run_jobs(first.expect("a first reactor")))
    }

    /// The reactors' nodes, by reactor.
    pub fn nodes(&self) -> Vec<Option<u32>> {
        self.all.iter().map(|r| r.node).collect()
    }

    /// Runs `f` on reactor `r` and awaits what it returns; None if that
    /// reactor has stopped.
    pub async fn run_on<T, F, Fut>(&self, r: usize, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
    {
        let (tx, rx) = chan::oneshot();
        let job: Job = Box::new(move || {
            hyperstor_reactor::spawn(async move {
                let _ = tx.send(f().await);
            });
        });
        self.all.get(r)?.jobs.send(job).ok()?;
        rx.recv().await
    }

    /// What a job needs to give its reactor a client: [`client`].
    pub fn spec(&self) -> Arc<ClientSpec> {
        self.spec.clone()
    }
}

/// This reactor's client: connected on first use, as the service's one
/// instance (D291).
pub async fn client(spec: &ClientSpec) -> Result<Rc<Client>, String> {
    if let Some(c) = CLIENT.with(|c| c.borrow().clone()) {
        return Ok(c);
    }
    let c = Client::connect_members_as(spec.meta.clone(), spec.id, spec.instance)
        .await
        .map_err(|e| format!("map service: {e}"))?;
    c.set_networks(spec.networks.clone());
    c.set_host(&spec.host);
    // Another job may have connected meanwhile: the first one stays.
    Ok(CLIENT.with(|slot| slot.borrow_mut().get_or_insert(c).clone()))
}
