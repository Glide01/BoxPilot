//! Event-driven drains for the state entities' reader channels.
//!
//! Each sing-box API stream (and the stdout/stderr pipes) is read by a
//! dedicated blocking thread that pushes into a `futures_channel::mpsc`
//! unbounded channel. The entity's UI-thread task awaits the channel instead
//! of polling it on a timer: while nothing arrives there are no wakeups at
//! all. When something does, `coalesce` (usually a short executor timer)
//! lets the rest of a burst queue up so it lands as one batch — one
//! `cx.notify()`, one render.
//!
//! Time-based work (URL-test settling, held-back pipe log lines, the
//! Connections age column) races the channel against a timer only while that
//! work is pending: [`next_batch_or`].

use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;

/// What [`next_batch_or`] woke up for.
#[derive(Debug, PartialEq, Eq)]
pub enum Wake<T> {
    /// At least one item, oldest first.
    Batch(Vec<T>),
    /// The timer fired before anything arrived. Nothing was taken.
    Timer,
    /// Every sender is gone and the queue is empty.
    Closed,
}

/// Await the first item (no wakeups while idle), let `coalesce` pass, return
/// everything queued. `None` once the sender is gone and the queue is empty
/// (items queued before the close still come out first).
pub async fn next_batch<T, F: Future<Output = ()>>(
    rx: &mut UnboundedReceiver<T>,
    coalesce: impl FnOnce() -> F,
) -> Option<Vec<T>> {
    match next_batch_or(rx, None::<std::future::Pending<()>>, coalesce).await {
        Wake::Batch(batch) => Some(batch),
        Wake::Timer | Wake::Closed => None,
    }
}

/// [`next_batch`], but also wake when `timer` (if any) fires first. The
/// timer only races the wait for the *first* item: once one has been taken,
/// the batch is always completed and returned, so dropping the timer can
/// never lose an item.
pub async fn next_batch_or<T, D, F>(
    rx: &mut UnboundedReceiver<T>,
    timer: Option<D>,
    coalesce: impl FnOnce() -> F,
) -> Wake<T>
where
    D: Future<Output = ()>,
    F: Future<Output = ()>,
{
    let mut timer = pin!(timer);
    let first = poll_fn(|cx| {
        if let Poll::Ready(item) = rx.poll_next_unpin(cx) {
            return Poll::Ready(Some(item));
        }
        if let Some(timer) = timer.as_mut().as_pin_mut() {
            if timer.poll(cx).is_ready() {
                return Poll::Ready(None);
            }
        }
        Poll::Pending
    })
    .await;
    let first = match first {
        None => return Wake::Timer,
        Some(None) => return Wake::Closed,
        Some(Some(item)) => item,
    };

    coalesce().await;

    let mut batch = vec![first];
    // Stops at `Empty`, or at `Closed` — which the next call reports.
    while let Ok(item) = rx.try_recv() {
        batch.push(item);
    }
    Wake::Batch(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::unbounded;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::{Context, Waker};

    /// Poll once with a no-op waker.
    fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// Drive a future that never truly waits (every await is already
    /// ready, or ready after a few polls) to completion.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        for _ in 0..1000 {
            if let Poll::Ready(output) = poll_once(future.as_mut()) {
                return output;
            }
        }
        panic!("future did not complete");
    }

    #[test]
    fn batches_everything_queued() {
        let (tx, mut rx) = unbounded();
        for i in 0..3 {
            tx.unbounded_send(i).unwrap();
        }
        assert_eq!(
            block_on(next_batch(&mut rx, || async {})),
            Some(vec![0, 1, 2])
        );
    }

    #[test]
    fn items_sent_while_coalescing_join_the_batch() {
        let (tx, mut rx) = unbounded();
        tx.unbounded_send(1).unwrap();
        let tx2 = tx.clone();
        let coalesce = move || async move {
            tx2.unbounded_send(2).unwrap();
        };
        assert_eq!(block_on(next_batch(&mut rx, coalesce)), Some(vec![1, 2]));
    }

    #[test]
    fn queued_items_come_out_before_the_close() {
        let (tx, mut rx) = unbounded();
        tx.unbounded_send("last").unwrap();
        drop(tx);
        assert_eq!(
            block_on(next_batch(&mut rx, || async {})),
            Some(vec!["last"])
        );
        assert_eq!(block_on(next_batch(&mut rx, || async {})), None);
    }

    #[test]
    fn none_after_close() {
        let (tx, mut rx) = unbounded::<u8>();
        drop(tx);
        assert_eq!(block_on(next_batch(&mut rx, || async {})), None);
    }

    #[test]
    fn pending_while_empty() {
        let (tx, mut rx) = unbounded::<u8>();
        let coalesced = Rc::new(Cell::new(false));
        {
            let flag = coalesced.clone();
            let mut wait = pin!(next_batch(&mut rx, move || async move { flag.set(true) }));
            for _ in 0..3 {
                assert!(poll_once(wait.as_mut()).is_pending());
            }
            assert!(!coalesced.get(), "no coalescing before the first item");
            tx.unbounded_send(7).unwrap();
            assert_eq!(poll_once(wait.as_mut()), Poll::Ready(Some(vec![7])));
        }
        assert!(coalesced.get());
    }

    #[test]
    fn timer_wakes_an_empty_wait_without_taking_anything() {
        let (tx, mut rx) = unbounded::<u8>();
        let woke = block_on(next_batch_or(&mut rx, Some(async {}), || async {}));
        assert_eq!(woke, Wake::Timer);
        tx.unbounded_send(1).unwrap();
        assert_eq!(
            block_on(next_batch_or(
                &mut rx,
                None::<std::future::Pending<()>>,
                || async {}
            )),
            Wake::Batch(vec![1])
        );
    }

    #[test]
    fn queued_items_win_over_a_ready_timer() {
        let (tx, mut rx) = unbounded();
        tx.unbounded_send(1).unwrap();
        tx.unbounded_send(2).unwrap();
        let woke = block_on(next_batch_or(&mut rx, Some(async {}), || async {}));
        assert_eq!(woke, Wake::Batch(vec![1, 2]));
    }

    #[test]
    fn closed_reported_with_a_timer_too() {
        let (tx, mut rx) = unbounded::<u8>();
        drop(tx);
        let woke = block_on(next_batch_or(
            &mut rx,
            Some(std::future::pending()),
            || async {},
        ));
        assert_eq!(woke, Wake::Closed);
    }
}
