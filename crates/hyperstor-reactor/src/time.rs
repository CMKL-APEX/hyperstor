//! Timers on the reactor.

use crate::{Resource, current};
use io_uring::{opcode, types};
use std::future::Future;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

/// Sleeps for `d`.
pub async fn sleep(d: Duration) {
    let ts = Box::new(
        types::Timespec::new()
            .sec(d.as_secs())
            .nsec(d.subsec_nanos()),
    );
    let sqe = opcode::Timeout::new(&*ts as *const types::Timespec).build();
    // -ETIME is the normal completion of a timeout.
    let _ = current().submit(sqe, Resource::Timespec(ts)).await;
}

/// Error from [`timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("timed out")
    }
}

impl std::error::Error for Elapsed {}

/// Runs `fut` for at most `d`.
pub async fn timeout<F: Future>(d: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let mut fut = pin!(fut);
    let mut timer = pin!(sleep(d));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;
    use std::time::Instant;

    #[test]
    fn sleep_and_timeout() {
        block_on(async {
            let t = Instant::now();
            sleep(Duration::from_millis(20)).await;
            assert!(t.elapsed() >= Duration::from_millis(19));
            assert_eq!(
                timeout(Duration::from_millis(5), sleep(Duration::from_secs(10))).await,
                Err(Elapsed)
            );
            assert_eq!(
                timeout(Duration::from_millis(500), async { 7 }).await,
                Ok(7)
            );
        });
    }
}
