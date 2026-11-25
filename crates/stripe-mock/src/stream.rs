//! Latest-state subscription (spec section 10).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures_core::Stream;
use tokio::sync::mpsc;

use crate::engine::Scope;
use crate::{ObjectType, StateUpdate};

/// Which objects a subscription covers.
#[derive(Debug, Clone, Default)]
pub struct SubscribeFilter {
    /// `None` means every type.
    pub object_types: Option<Vec<ObjectType>>,
    /// Scopes to include: `None` is the platform, `Some(id)` a connected account.
    /// `None` for the whole option means every scope.
    pub accounts: Option<Vec<Scope>>,
}

impl SubscribeFilter {
    pub(crate) fn matches(&self, t: ObjectType) -> bool {
        self.object_types.as_ref().is_none_or(|ts| ts.contains(&t))
    }

    pub(crate) fn matches_scope(&self, scope: &Scope) -> bool {
        self.accounts.as_ref().is_none_or(|a| a.contains(scope))
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
        if !self.filter.matches(u.object_type) || !self.filter.matches_scope(&u.account) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::rng::RngStreams;
    use crate::{Config, SimTime};
    use serde_json::json;

    fn make(e: &mut Engine, id: &str) {
        let c = e.begin_action("t");
        e.create(
            ObjectType::Customer,
            id.into(),
            json!({"id": id, "object": "customer", "created": 0, "email": null}),
            c,
            &[],
        );
    }

    fn updates(s: &mut Subscription) -> Vec<StateUpdate> {
        let mut out = Vec::new();
        while let Some(item) = s.pending.pop_front() {
            if let SubscriptionItem::Update(u) = item {
                out.push(u);
            }
        }
        while let Ok(SubscriptionItem::Update(u)) = s.rx.try_recv() {
            out.push(u);
        }
        out
    }

    #[test]
    fn filter_by_account() {
        let mut e = Engine::new(SimTime(0), &Config::default(), &RngStreams::new(0));
        make(&mut e, "cus_p");
        e.in_scope(Some("acct_1".into()), |e| make(e, "cus_1"));
        e.in_scope(Some("acct_2".into()), |e| make(e, "cus_2"));
        let mut s = e.subscribe(
            SubscribeFilter {
                object_types: None,
                accounts: Some(vec![Some("acct_1".into())]),
            },
            &SubscribeOptions::default(),
        );
        make(&mut e, "cus_p2");
        e.in_scope(Some("acct_2".into()), |e| make(e, "cus_22"));
        e.in_scope(Some("acct_1".into()), |e| make(e, "cus_11"));
        let got = updates(&mut s);
        let ids: Vec<_> = got.iter().map(|u| u.object_id.as_str()).collect();
        assert_eq!(ids, ["cus_1", "cus_11"]);
        assert!(got.iter().all(|u| u.account.as_deref() == Some("acct_1")));

        let mut platform = e.subscribe(
            SubscribeFilter {
                object_types: None,
                accounts: Some(vec![None]),
            },
            &SubscribeOptions::default(),
        );
        let ids: Vec<_> = updates(&mut platform)
            .into_iter()
            .map(|u| u.object_id)
            .collect();
        assert_eq!(ids, ["cus_p", "cus_p2"]);
    }
}
