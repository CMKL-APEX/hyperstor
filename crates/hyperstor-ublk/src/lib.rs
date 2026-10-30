//! Serves a hyperstor volume as a ublk block device, `/dev/ublkbN`
//! (04-decisions.md D94, D95; 11-frontends.md §11.2's stopgap; 17-milestones.md
//! HS1's test devices).
//!
//! [`serve`] attaches the volume (its `rw` lease) on a reactor thread,
//! adds the device through the control ring, maps queue 0 and fetches
//! every tag, starts the device, and serves until told to stop; then it
//! stops the device, lets every fetch complete, detaches and deletes it.
//! One queue: enough for tests and the stopgap; more need one client
//! instance shared by every queue's reactor, so the lease and blocklist
//! cover them all.

pub mod ctrl;
pub mod queue;
pub mod sys;

use hyperstor_client::Client;
use hyperstor_client::volume::Volume;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

/// Requests are at most this long (`max_io_buf_bytes`).
pub const BUF_BYTES: u32 = 512 << 10;

/// How to take the volume's lease (06-metadata.md §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Refused while another instance holds it.
    Attach,
    /// From a holder that died or is cut off.
    TakeOver,
    /// From a live holder, which is blocklisted first.
    Preempt,
}

pub struct Options {
    pub meta: Vec<SocketAddr>,
    pub volume: u128,
    pub take: Take,
    /// Requests in flight at once.
    pub depth: u16,
}

/// A device being served.
pub struct Served {
    pub dev: u32,
    pub volume: Volume,
}

enum ToQueue {
    Dev(u32),
    Stop,
}

enum Event {
    Attached(Box<Volume>),
    Fetching,
    /// Every tag's fetch ended: the driver stopped the device.
    Ended,
    Stop,
    Done(Result<(), String>),
}

/// Serves `opts.volume` until `stop` receives (or is dropped), or the
/// device is stopped from outside. Calls `ready` once the block device
/// exists. Needs root and the `ublk_drv` module.
pub fn serve(
    opts: Options,
    ready: impl FnOnce(&Served),
    stop: mpsc::Receiver<()>,
) -> Result<(), String> {
    let mut ctrl = ctrl::Control::open().map_err(|e| {
        format!("/dev/ublk-control: {e} (load the module: modprobe ublk_drv; and run as root)")
    })?;
    let (ev, events) = mpsc::channel::<Event>();
    let fwd = ev.clone();
    std::thread::spawn(move || {
        let _ = stop.recv();
        let _ = fwd.send(Event::Stop);
    });
    let (to_queue, from_main) = hyperstor_reactor::chan::channel::<ToQueue>();
    let depth = opts.depth;
    let thread = std::thread::Builder::new()
        .name("hs-ublk-queue".into())
        .spawn(move || queue_thread(opts, ev, from_main))
        .map_err(|e| e.to_string())?;
    let finish = |r: Result<(), String>| {
        let _ = thread.join();
        r
    };
    let volume = match events.recv() {
        Ok(Event::Attached(v)) => *v,
        Ok(Event::Done(Err(e))) => return finish(Err(e)),
        _ => return finish(Err("the queue thread stopped".into())),
    };
    let info = match ctrl.add(1, depth, BUF_BYTES) {
        Ok(i) => i,
        Err(e) => {
            let _ = to_queue.send(ToQueue::Stop);
            return finish(Err(format!("adding the device: {e}")));
        }
    };
    let dev = info.dev_id;
    let fail = |ctrl: &mut ctrl::Control, e: String| {
        let _ = ctrl.stop(dev);
        let _ = to_queue.send(ToQueue::Stop);
        // Every queue command completes, and the queue closes the
        // character device, before the device can be deleted.
        let _ = events.recv_timeout(Duration::from_secs(15));
        let _ = ctrl.delete(dev);
        Err(e)
    };
    if let Err(e) = ctrl.set_params(dev, &ctrl::params(volume.size, BUF_BYTES)) {
        let r = fail(&mut ctrl, format!("setting the parameters: {e}"));
        return finish(r);
    }
    let _ = to_queue.send(ToQueue::Dev(dev));
    match events.recv() {
        Ok(Event::Fetching) => {}
        Ok(Event::Done(Err(e))) => {
            let _ = ctrl.delete(dev);
            return finish(Err(e));
        }
        _ => {
            let r = fail(&mut ctrl, "the queue thread stopped".into());
            return finish(r);
        }
    }
    if let Err(e) = ctrl.start(dev) {
        let r = fail(&mut ctrl, format!("starting the device: {e}"));
        return finish(r);
    }
    ready(&Served { dev, volume });
    // Until told to stop, or the device is stopped from outside.
    loop {
        match events.recv() {
            Ok(Event::Stop) | Ok(Event::Ended) | Err(_) => break,
            Ok(Event::Done(r)) => {
                let _ = ctrl.delete(dev);
                return finish(r);
            }
            Ok(_) => {}
        }
    }
    let _ = ctrl.stop(dev);
    let _ = to_queue.send(ToQueue::Stop);
    let r = loop {
        match events.recv_timeout(Duration::from_secs(15)) {
            Ok(Event::Done(r)) => break r,
            Ok(_) => {}
            Err(_) => break Err("the queue didn't stop".into()),
        }
    };
    let r = finish(r);
    ctrl.delete(dev)
        .map_err(|e| format!("deleting device {dev}: {e}"))?;
    r
}

fn queue_thread(
    opts: Options,
    ev: mpsc::Sender<Event>,
    mut from_main: hyperstor_reactor::chan::Receiver<ToQueue>,
) {
    let ev2 = ev.clone();
    let r = hyperstor_reactor::block_on(async move {
        let client = Client::connect_members(opts.meta.clone(), std::process::id() as u64)
            .await
            .map_err(|e| format!("map service: {e}"))?;
        client.set_lease_label("ublk");
        let vol = match opts.take {
            Take::Attach => client.attach(opts.volume).await,
            Take::TakeOver => client.take_over(opts.volume).await,
            Take::Preempt => client.preempt(opts.volume).await,
        }
        .map_err(|e| format!("volume {:032x}: {e}", opts.volume))?;
        let _ = ev2.send(Event::Attached(Box::new(vol.volume())));
        let dev = match from_main.recv().await {
            Some(ToQueue::Dev(d)) => d,
            _ => {
                let _ = vol.detach().await;
                return Ok(());
            }
        };
        let q = match queue::Queue::open(dev, 0, opts.depth, BUF_BYTES as usize).await {
            Ok(q) => Rc::new(q),
            Err(e) => {
                let _ = vol.detach().await;
                return Err(format!("/dev/ublkc{dev}: {e}"));
            }
        };
        let tags: Vec<_> = (0..q.depth())
            .map(|t| hyperstor_reactor::spawn(q.clone().serve_tag(t, vol.clone())))
            .collect();
        drop(q);
        let ended = Rc::new(std::cell::Cell::new(false));
        let (e2, ev3) = (ended.clone(), ev2.clone());
        hyperstor_reactor::spawn(async move {
            for t in tags {
                t.await;
            }
            e2.set(true);
            let _ = ev3.send(Event::Ended);
        });
        let _ = ev2.send(Event::Fetching);
        let _ = from_main.recv().await;
        // Stopped: every pending fetch completes as aborted.
        let t = std::time::Instant::now();
        while !ended.get() && t.elapsed() < Duration::from_secs(10) {
            hyperstor_reactor::time::sleep(Duration::from_millis(10)).await;
        }
        vol.detach().await.map_err(|e| format!("detaching: {e}"))
    });
    let _ = ev.send(Event::Done(r));
}

/// Parses a volume id: up to 32 hexadecimal digits.
pub fn parse_id(s: &str) -> Option<u128> {
    if s.is_empty() || s.len() > 32 {
        return None;
    }
    u128::from_str_radix(s, 16).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_are_hex() {
        assert_eq!(super::parse_id("abc"), Some(0xabc));
        assert_eq!(super::parse_id(&"f".repeat(32)), Some(u128::MAX));
        assert_eq!(super::parse_id(&"f".repeat(33)), None);
        assert_eq!(super::parse_id("xyz"), None);
        assert_eq!(super::parse_id(""), None);
    }

    #[test]
    fn parameters_follow_the_volume() {
        let p = super::ctrl::params(1 << 30, super::BUF_BYTES);
        assert_eq!(p.basic.dev_sectors, (1 << 30) / 512);
        assert_eq!(p.basic.max_sectors, super::BUF_BYTES / 512);
        assert_eq!(p.basic.logical_bs_shift, 12);
        assert_eq!(p.basic.attrs, 0, "no volatile cache: no flushes");
        assert_eq!(p.len as usize, size_of::<super::sys::Params>());
    }
}
