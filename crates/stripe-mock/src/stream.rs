//! Latest-state subscription (spec section 10).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures_core::Stream;
use tokio::sync::mpsc;

use crate::{ObjectType, StateUpdate};

/// Which objects a subscription covers.
#[derive(Debug, Clone, Default)]
pub struct SubscribeFilter {
    /// `None` means every type.
    pub object_types: Option<Vec<ObjectType>>,
}

impl SubscribeFilter {
    pub(crate) fn matches(&self, t: ObjectType) -> bool {
        self.object_types.as_ref().is_none_or(|ts| ts.contains(&t))
    }
}

/// Subscription tuning.
#[derive(Debug, Clone)]
pub struct SubscribeOptions {
    /// Live updates buffered before the subscriber counts as lagged.
    pub buffer: usize,
}

impl Default for SubscribeOptions {
    fn default() -> Self {
        SubscribeOptions { buffer: 1024 }
    }
}

/// One item from a subscription.
// Not boxed: items are moved straight to the consumer, and matching on
// `Update(u)` without a Box keeps test code simple.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionItem {
    /// An object's latest state (snapshot or live change).
    Update(StateUpdate),
    /// The initial snapshot is complete; live changes follow.
    Synced,
    /// The subscriber fell behind and was dropped; resubscribe for a fresh snapshot.
    Lagged,
}

/// The engine's side of a subscription.
#[derive(Debug)]
pub(crate) struct Subscriber {
    filter: SubscribeFilter,
    tx: mpsc::Sender<SubscriptionItem>,
    lagged: Arc<AtomicBool>,
}

impl Subscriber {
    /// Offers `u` without waiting. Returns `false` if this subscriber should be dropped.
    pub(crate) fn offer(&self, u: &StateUpdate) -> bool {
        if !self.filter.matches(u.object_type) {
            return true;
        }
        match self.tx.try_send(SubscriptionItem::Update(u.clone())) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.lagged.store(true, Ordering::SeqCst);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// A stream of [`SubscriptionItem`]s: snapshot, `Synced`, then live changes.
#[derive(Debug)]
pub struct Subscription {
    pending: VecDeque<SubscriptionItem>,
    rx: mpsc::Receiver<SubscriptionItem>,
    lagged: Arc<AtomicBool>,
    done: bool,
}

/// Builds both halves. `snapshot` must already be filtered.
pub(crate) fn pair(
    filter: SubscribeFilter,
    options: &SubscribeOptions,
    snapshot: Vec<StateUpdate>,
) -> (Subscriber, Subscription) {
    let (tx, rx) = mpsc::channel(options.buffer.max(1));
    let lagged = Arc::new(AtomicBool::new(false));
    let mut pending: VecDeque<SubscriptionItem> =
        snapshot.into_iter().map(SubscriptionItem::Update).collect();
    pending.push_back(SubscriptionItem::Synced);
    (
        Subscriber {
            filter,
            tx,
            lagged: lagged.clone(),
        },
        Subscription {
            pending,
            rx,
            lagged,
            done: false,
        },
    )
}

impl Stream for Subscription {
    type Item = SubscriptionItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(item) = this.pending.pop_front() {
            return Poll::Ready(Some(item));
        }
        if this.done {
            return Poll::Ready(None);
        }
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Ready(None) => {
                this.done = true;
                if this.lagged.load(Ordering::SeqCst) {
                    Poll::Ready(Some(SubscriptionItem::Lagged))
                } else {
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
