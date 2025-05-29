//! Prio retry consumes a stream and yields elements with exponential backoffs.
//!
//! An element that is enqueued will be yielded instantly if it is a new element.
//! Otherwise it will be delayed according to the number of times that it has been enqueued
//! consecutively.
//! New items will replace old items and start with a delay of 0.

use std::{
    collections::BinaryHeap,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use futures_core::Future;
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
    delayed_item: Option<DelayedItem<S::Item>>,
    buffer: BinaryHeap<S::Item>,
}

impl<S> PrioRetry<S>
where
    S: Stream,
    S::Item: Ord + Clone + Eq,
{
    pub fn new(stream: S, delay_duration: Duration) -> Self {
        Self {
            stream,
            delay: time::sleep(delay_duration),
            delay_duration,
            delayed_item: None,
            buffer: BinaryHeap::new(),
        }
    }
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
            match opt_item {
                Some(new_item) => {
                    if let Some(delayed_item) = this.delayed_item.as_ref() {
                        if new_item <= delayed_item.item {
                            this.buffer.push(new_item);
                        } else {
                            this.buffer.push(delayed_item.item.clone());
                            *this.delayed_item = Some(DelayedItem::new(new_item.clone(), *this.delay_duration));
                            this.delay.as_mut().reset(Instant::now() + *this.delay_duration);
                        }
                    } else {
                        *this.delayed_item = Some(DelayedItem::new(new_item.clone(), *this.delay_duration));
                        this.delay.as_mut().reset(Instant::now() + *this.delay_duration);
                    }
                }
                None => break,
            }
        }

        if this.delay.as_mut().poll(cx).is_ready() {
            if let Some(delayed_item) = this.delayed_item.take() {
                return Poll::Ready(Some(delayed_item.item));
            }
        }

        Poll::Pending
    }
}

#[pin_project]
struct DelayedItem<T> {
    item: T,
    #[pin]
    delay: Sleep,
}

impl<T> DelayedItem<T> {
    fn new(item: T, duration: Duration) -> Self {
        Self {
            item,
            delay: time::sleep(duration),
        }
    }
}
