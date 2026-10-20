//! An unbounded multi-producer, single-consumer channel that works across
//! threads and reactors: senders may run anywhere, the receiver awaits on
//! its reactor.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

struct State<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    senders: usize,
    receiver: bool,
}

pub struct Sender<T> {
    s: Arc<Mutex<State<T>>>,
}

pub struct Receiver<T> {
    s: Arc<Mutex<State<T>>>,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let s = Arc::new(Mutex::new(State {
        queue: VecDeque::new(),
        waker: None,
        senders: 1,
        receiver: true,
    }));
    (Sender { s: s.clone() }, Receiver { s })
}

impl<T> Sender<T> {
    /// Sends `v`, or gives it back if the receiver is gone.
    pub fn send(&self, v: T) -> Result<(), T> {
        let w = {
            let mut s = self.s.lock().unwrap();
            if !s.receiver {
                return Err(v);
            }
            s.queue.push_back(v);
            s.waker.take()
        };
        if let Some(w) = w {
            w.wake();
        }
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.s.lock().unwrap().senders += 1;
        Self { s: self.s.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let w = {
            let mut s = self.s.lock().unwrap();
            s.senders -= 1;
            if s.senders == 0 { s.waker.take() } else { None }
        };
        if let Some(w) = w {
            w.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// The next value, or `None` once every sender is gone and the queue
    /// is empty.
    pub async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| {
            let mut s = self.s.lock().unwrap();
            if let Some(v) = s.queue.pop_front() {
                return Poll::Ready(Some(v));
            }
            if s.senders == 0 {
                return Poll::Ready(None);
            }
            s.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Takes everything queued now, without waiting.
    pub fn drain(&mut self) -> Vec<T> {
        self.s.lock().unwrap().queue.drain(..).collect()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.s.lock().unwrap().receiver = false;
    }
}

/// A one-shot value, settable from any thread.
pub fn oneshot<T>() -> (OneSender<T>, OneReceiver<T>) {
    let (tx, rx) = channel();
    (OneSender(tx), OneReceiver(rx))
}

pub struct OneSender<T>(Sender<T>);
pub struct OneReceiver<T>(Receiver<T>);

impl<T> OneSender<T> {
    pub fn send(self, v: T) -> Result<(), T> {
        self.0.send(v)
    }
}

impl<T> OneReceiver<T> {
    /// The value, or `None` if the sender was dropped without sending.
    pub async fn recv(mut self) -> Option<T> {
        self.0.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;

    #[test]
    fn across_threads() {
        let (tx, mut rx) = channel::<u64>();
        let producers: Vec<_> = (0..4)
            .map(|t| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for i in 0..1000 {
                        tx.send(t * 1000 + i).unwrap();
                    }
                })
            })
            .collect();
        drop(tx);
        let sum = block_on(async move {
            let mut sum = 0u64;
            while let Some(v) = rx.recv().await {
                sum += v;
            }
            sum
        });
        for p in producers {
            p.join().unwrap();
        }
        assert_eq!(sum, (0..4000u64).sum());
    }

    #[test]
    fn oneshot_between_reactors() {
        let (tx, rx) = oneshot::<&'static str>();
        let t = std::thread::spawn(move || {
            block_on(async move {
                crate::time::sleep(std::time::Duration::from_millis(10)).await;
                tx.send("hi").unwrap();
            })
        });
        assert_eq!(block_on(async move { rx.recv().await }), Some("hi"));
        t.join().unwrap();
    }
}
