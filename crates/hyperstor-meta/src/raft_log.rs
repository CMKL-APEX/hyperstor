//! The log of what the libraries under the map service say through
//! `tracing`: openraft's warnings and errors, and glidex-raftstore's.
//! Without it a leader that can't commit says only "not committed" and
//! nothing on why (I257, #177).
//!
//! `HS_LOG` sets the filter (`EnvFilter` syntax, such as
//! `warn,openraft=debug`); the default is `warn`. A warning or error from
//! one place in the code is printed at most once every [`EVERY`], so a
//! member that is down, which fails every heartbeat to it, doesn't fill
//! the journal: openraft warns twice per failed heartbeat, 40 times a
//! second at our 50 ms heartbeat.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing_subscriber::filter::{EnvFilter, LevelFilter, dynamic_filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

/// The shortest time between two warnings or errors from one place.
pub const EVERY: Duration = Duration::from_secs(5);

/// When each place last printed, by a hash of its file and line.
#[derive(Default)]
pub struct Limiter {
    last: Mutex<HashMap<u64, Instant>>,
}

impl Limiter {
    /// Whether the place `at` (file and line) may print at `now`.
    pub fn allow(&self, at: (&str, u32), now: Instant) -> bool {
        let mut h = DefaultHasher::new();
        at.hash(&mut h);
        let key = h.finish();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        match last.get(&key) {
            Some(t) if now.duration_since(*t) < EVERY => false,
            _ => {
                last.insert(key, now);
                true
            }
        }
    }
}

/// Send `tracing`'s events to standard error, as the service's own lines.
pub fn init() {
    let filter = EnvFilter::try_from_env("HS_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    subscriber(filter, std::io::stderr).init();
}

/// The subscriber [`init`] installs, writing to `w`.
fn subscriber<W>(filter: EnvFilter, w: W) -> impl tracing_core::Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let limiter = Limiter::default();
    // Dynamic: a plain `filter_fn` is asked once per place and its answer
    // kept, which would let every later line through.
    let limit = dynamic_filter_fn(move |m, _| {
        // Debug and trace are asked for with HS_LOG, and wanted whole.
        if !m.is_event() || *m.level() > LevelFilter::WARN {
            return true;
        }
        limiter.allow(
            (m.file().unwrap_or(""), m.line().unwrap_or(0)),
            Instant::now(),
        )
    });
    tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(w)
            .with_ansi(false)
            .without_time()
            .with_filter(filter)
            .with_filter(limit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_per_place_every_few_seconds() {
        let l = Limiter::default();
        let t = Instant::now();
        let a = ("replication/mod.rs", 258);
        let b = ("core/raft_core.rs", 1099);
        assert!(l.allow(a, t));
        assert!(!l.allow(a, t + Duration::from_millis(50)));
        // Another place isn't held back by the first.
        assert!(l.allow(b, t + Duration::from_millis(50)));
        assert!(!l.allow(a, t + EVERY - Duration::from_millis(1)));
        assert!(l.allow(a, t + EVERY));
        assert!(!l.allow(a, t + EVERY + Duration::from_secs(1)));
    }

    /// Real events through the subscriber: one warning of a burst is
    /// printed, and every debug line when HS_LOG asks for them.
    #[test]
    fn a_burst_of_warnings_prints_once() {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let w = buf.clone();
        let s = subscriber(EnvFilter::new("warn,burst=debug"), move || w.clone());
        tracing_core::dispatcher::with_default(&tracing_core::Dispatch::new(s), || {
            for i in 0..50 {
                tracing::warn!(target: "burst", "heartbeat error {i}");
                tracing::debug!(target: "burst", "detail {i}");
                tracing::info!(target: "other", "not asked for");
            }
            tracing::error!(target: "burst", "another place");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("heartbeat error").count(), 1, "{out}");
        assert!(out.contains("heartbeat error 0"), "{out}");
        assert_eq!(out.matches("detail").count(), 50, "{out}");
        assert!(!out.contains("not asked for"), "{out}");
        assert!(out.contains("another place"), "{out}");
    }
}
