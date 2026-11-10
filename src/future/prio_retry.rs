//! Prio retry consumes a stream and yields its best elements, backing off on
//! repeats.
//!
//! A share is only worth anything in the round it was mined in, so an element
//! that arrives is yielded at once. Holding it back for a moment to see
//! whether a better one turns up only risks the pool moving on to the next
//! round first: a share of ours was refused with "submission is for different
//! round" after waiting out the delay, although it had been enqueued 0.7s
//! before the pool's next round started.
//!
//! What the delay is for is repeats: `submit_nonce` re-enqueues a share
//! verbatim when the pool answers "busy", and those are held back instead of
//! hammering the pool. As in the original scavenger implementation the delay
//! grows exponentially with every consecutive repeat (base, 2 * base,
//! 4 * base, ...), capped at a maximum so a share never waits for longer than
//! that between two attempts.
//!
//! The elements themselves are compared, so nothing has to be told apart by
//! hand: an element better than everything yielded so far goes out at once, a
//! repeat of the best one waits out a delay, and an element worse than one
//! that has been yielded already - which is what a retry of a share looks like
//! once a better one has overtaken it - is dropped. That is also where
//! elements replaced by better ones end up; the old code pushed them into a
//! heap that was never read.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Future;
use futures_core::Stream;
use pin_project::pin_project;
use tokio::time::{self, Duration, Instant, Sleep};

#[pin_project]
pub struct PrioRetry<S>
where
    S: Stream,
    S::Item: Ord + Clone + Eq,
{
    #[pin]
    stream: S,
    #[pin]
    delay: Sleep,
    delay_duration: Duration,
    /// Upper bound for the exponential backoff.
    max_delay: Duration,
    /// Consecutive repeats of the current best element, 0 after a new one.
    attempt: u32,
    /// The best element that was sent so far, `None` before the first one.
    sent_best: Option<S::Item>,
    /// The element that is backing off after a repeat.
    waiting: Option<S::Item>,
}

impl<S> PrioRetry<S>
where
    S: Stream,
    S::Item: Ord + Clone + Eq,
{
    pub fn new(stream: S, delay_duration: Duration, max_delay: Duration) -> Self {
        Self {
            stream,
            delay: time::sleep(delay_duration),
            delay_duration,
            max_delay,
            attempt: 0,
            sent_best: None,
            waiting: None,
        }
    }
}

/// `base * 2^attempt`, capped at `max`.
fn backoff(base: Duration, max: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1u32 << attempt.min(16)).min(max)
}

impl<S> Stream for PrioRetry<S>
where
    S: Stream + Unpin,
    S::Item: Ord + Clone + Eq + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        while let Poll::Ready(opt_item) = this.stream.as_mut().poll_next(cx) {
            let Some(new_item) = opt_item else {
                break;
            };

            // The element that is already backing off came back: let it wait
            // out another, longer delay instead of retrying in a tight loop.
            if this
                .waiting
                .as_ref()
                .is_some_and(|waiting| *waiting == new_item)
            {
                let wait = backoff(*this.delay_duration, *this.max_delay, *this.attempt);
                *this.attempt += 1;
                this.delay.as_mut().reset(Instant::now() + wait);
                continue;
            }

            match this.sent_best.as_ref() {
                // The very element we sent last is back: that is the pool
                // answering "busy", so back off rather than hammer it.
                Some(sent_best) if *sent_best == new_item => {
                    *this.waiting = Some(new_item);
                    let wait = backoff(*this.delay_duration, *this.max_delay, *this.attempt);
                    *this.attempt += 1;
                    this.delay.as_mut().reset(Instant::now() + wait);
                }
                // Worse than an element that was already sent: not worth a
                // request. A retry of a share that a better one has overtaken
                // in the meantime lands here and is dropped.
                Some(sent_best) if *sent_best > new_item => {}
                // Better than anything sent so far, or the first element: out
                // it goes at once, because it is only worth anything in the
                // round it was mined in.
                _ => {
                    *this.sent_best = Some(new_item.clone());
                    *this.waiting = None;
                    *this.attempt = 0;
                    return Poll::Ready(Some(new_item));
                }
            }
        }

        // Only the element that is backing off can become ready on the clock.
        if this.waiting.is_some() && this.delay.as_mut().poll(cx).is_ready() {
            return Poll::Ready(this.waiting.take());
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::future::FutureExt;
    use futures_util::stream::StreamExt;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    /// A larger item is a better share, exactly like `SubmissionParameters`.
    fn stream_of(
        rx: mpsc::UnboundedReceiver<u64>,
    ) -> Pin<Box<PrioRetry<UnboundedReceiverStream<u64>>>> {
        Box::pin(PrioRetry::new(
            UnboundedReceiverStream::new(rx),
            Duration::from_secs(3),
            Duration::from_secs(20),
        ))
    }

    /// Sends `item` again as a "busy" retry and checks that it is held back
    /// for exactly `secs` seconds.
    async fn retry_waits<S: Stream<Item = u64> + Unpin>(
        tx: &mpsc::UnboundedSender<u64>,
        stream: &mut Pin<Box<PrioRetry<S>>>,
        item: u64,
        secs: u64,
    ) {
        tx.send(item).unwrap();
        assert_eq!(stream.next().now_or_never(), None);
        time::advance(Duration::from_secs(secs - 1)).await;
        assert_eq!(stream.next().now_or_never(), None, "out before {}s", secs);
        time::advance(Duration::from_secs(1)).await;
        assert_eq!(stream.next().now_or_never(), Some(Some(item)), "not out after {}s", secs);
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_repeats_back_off_exponentially_up_to_the_cap() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));

        // 3s, 6s, 12s as in scavenger, then capped at 20s.
        for secs in [3, 6, 12, 20, 20] {
            retry_waits(&tx, &mut stream, 5, secs).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_better_element_resets_the_backoff() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));
        retry_waits(&tx, &mut stream, 5, 3).await;
        retry_waits(&tx, &mut stream, 5, 6).await;

        // A new, better share starts over with the base delay.
        tx.send(9).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(9)));
        retry_waits(&tx, &mut stream, 9, 3).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_element_is_yielded_at_once() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        tx.send(5).unwrap();
        // No time passes: a share must not wait for the delay, or it can miss
        // the round it belongs to.
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_repeat_backs_off_instead_of_hammering_the_pool() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));

        // The pool answered "busy", so the very same share was enqueued again.
        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), None);
        time::advance(Duration::from_secs(2)).await;
        assert_eq!(stream.next().now_or_never(), None);
        time::advance(Duration::from_secs(1)).await;
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_better_element_goes_out_at_once_while_one_backs_off() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), None);

        // A better share while the previous one waits: it goes out now, and
        // the worse one it replaced is not sent later.
        tx.send(9).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(9)));
        time::advance(Duration::from_secs(10)).await;
        assert_eq!(stream.next().now_or_never(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_worse_element_is_dropped_while_one_backs_off() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));

        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), None);

        // Worse than the share that is already waiting - not worth a request.
        tx.send(3).unwrap();
        assert_eq!(stream.next().now_or_never(), None);

        // The waiting share still goes out once its delay is over.
        time::advance(Duration::from_secs(3)).await;
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_of_a_superseded_element_is_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stream = stream_of(rx);

        // A goes out and the pool answers "busy" for it, but by then a better
        // share B is already queued, so A's retry arrives after B.
        tx.send(5).unwrap();
        assert_eq!(stream.next().now_or_never(), Some(Some(5)));
        tx.send(9).unwrap();
        tx.send(5).unwrap();

        // B goes out and A is not sent again: a better share already has.
        assert_eq!(stream.next().now_or_never(), Some(Some(9)));
        assert_eq!(stream.next().now_or_never(), None);
        time::advance(Duration::from_secs(10)).await;
        assert_eq!(stream.next().now_or_never(), None);
    }
}
