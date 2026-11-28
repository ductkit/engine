//! The correct consumer: backfill, then every thin event is a hint to refetch its
//! object, then a final reconcile. The reconcile is needed for two reasons:
//! - Stripe objects carry no version, so a refetch that hits a stale read can't be
//!   told from a fresh one;
//! - some changes announce no event at all (a customer's `next_invoice_sequence`
//!   when an invoice is created), so no hint ever arrives for them.

use ductkit_stripe_mock::ObjectType;

use crate::client::{Client, Fetched};
use crate::hooks::HookServer;
use crate::truth::{COMPARED, Synced};

pub struct Correct {
    pub synced: Synced,
    hooks: HookServer,
    cursor: usize,
}

impl Correct {
    pub fn new(hooks: HookServer) -> Correct {
        Correct {
            synced: Synced::new(),
            hooks,
            cursor: 0,
        }
    }

    pub async fn backfill(&mut self, c: &Client, scopes: &[Option<String>]) {
        self.synced = c.backfill(scopes).await;
    }

    /// Refetches the related object of every new thin event of a compared type.
    pub async fn tick(&mut self, c: &Client) {
        for e in self.hooks.take_new(&mut self.cursor) {
            let related = &e["related_object"];
            let Some(t) = COMPARED
                .into_iter()
                .find(|t| related["type"].as_str() == Some(stripe_object(*t)))
            else {
                continue;
            };
            let scope = e["context"].as_str().map(str::to_string);
            let id = related["id"].as_str().unwrap().to_string();
            let url = related["url"].as_str().unwrap();
            match c.get(url, scope.as_deref()).await {
                Fetched::Object(v) => {
                    self.synced.insert((scope, t, id), v);
                }
                Fetched::Gone => {
                    self.synced.remove(&(scope, t, id));
                }
            }
        }
    }

    pub async fn reconcile(&mut self, c: &Client, scopes: &[Option<String>]) {
        self.synced = c.backfill(scopes).await;
    }
}

/// The Stripe `object` name of a compared type, as `related_object.type` carries it.
pub fn stripe_object(t: ObjectType) -> &'static str {
    match t {
        ObjectType::Customer => "customer",
        ObjectType::Product => "product",
        ObjectType::Price => "price",
        ObjectType::Subscription => "subscription",
        ObjectType::Invoice => "invoice",
        other => panic!("{other:?} is not compared"),
    }
}
