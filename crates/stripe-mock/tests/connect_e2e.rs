//! End to end: a sync-like consumer converges per scope (spec sections 4, 5.4 and 10).

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::{Receiver, get_with, mock_with};
use ductkit_stripe_mock::{ChangeKind, MockStripe, ObjectType, SubscribeFilter, SubscriptionItem};
use futures_util::StreamExt;
use serde_json::Value;

fn dest_block(name: &str, rx: &Receiver, payload: &str, extra: &str) -> String {
    format!(
        "[[destinations]]\nname = \"{name}\"\nurl = \"{}\"\npayload = \"{payload}\"\nenabled_events = [\"*\"]\nsecret = \"whsec_test\"\n{extra}\n",
        rx.url("/h")
    )
}

/// Every object of a list endpoint, following `starting_after`.
async fn walk(m: &MockStripe, path: &str, headers: &[(&str, &str)]) -> BTreeSet<String> {
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut ids = BTreeSet::new();
    let mut last: Option<String> = None;
    loop {
        let after = last
            .as_ref()
            .map(|l| format!("&starting_after={l}"))
            .unwrap_or_default();
        let r = get_with(m, &format!("{path}{sep}limit=100{after}"), headers).await;
        assert_eq!(r.status(), 200, "{path} {headers:?}");
        let v: Value = r.json().await.unwrap();
        for o in v["data"].as_array().unwrap() {
            last = Some(o["id"].as_str().unwrap().to_string());
            ids.insert(last.clone().unwrap());
        }
        if v["has_more"] != true {
            return ids;
        }
    }
}

/// The live IDs of `types` that the engine holds in `scope`.
async fn engine_ids(m: &MockStripe, scope: Option<&str>, types: &[ObjectType]) -> BTreeSet<String> {
    let mut s = m.subscribe(SubscribeFilter {
        object_types: Some(types.to_vec()),
        accounts: Some(vec![scope.map(String::from)]),
    });
    let mut ids = BTreeSet::new();
    while let Some(item) = s.next().await {
        match item {
            SubscriptionItem::Update(u) => {
                assert_eq!(u.account.as_deref(), scope);
                if u.kind != ChangeKind::Deleted {
                    ids.insert(u.object_id);
                }
            }
            SubscriptionItem::Synced => return ids,
            SubscriptionItem::Lagged => panic!("lagged"),
        }
    }
    panic!("stream ended");
}

#[tokio::test]
async fn sync_like_consumer_converges() {
    let (snap, thin) = (
        Receiver::start(|_| (200, Duration::ZERO)),
        Receiver::start(|_| (200, Duration::ZERO)),
    );
    let toml = format!(
        "seed = 7\n[actions]\nnew_customer = 4\nstart_subscription = 2\n[accounts]\nconnect = true\nconnected_accounts = {{ initial = 3, connect_rate = 0.5, deauthorize_rate = 0.25 }}\n[history]\nduration = \"6h\"\n{}{}",
        dest_block("snap", &snap, "snapshot", "connect = true"),
        dest_block(
            "thin",
            &thin,
            "thin",
            "events_from = [\"@self\", \"@accounts\"]"
        ),
    );
    let m = mock_with(&toml).await;
    m.step(Duration::from_secs(12 * 3600)).await.unwrap();
    m.quiesce().await.unwrap();

    let accounts = m.accounts();
    assert!(accounts.len() > 3, "accounts connected during the run");
    assert!(accounts.iter().any(|a| !a.connected), "some disconnected");
    assert!(!snap.requests().is_empty() && !thin.requests().is_empty());

    let resources = [
        ("/v1/customers", ObjectType::Customer),
        ("/v1/products", ObjectType::Product),
        ("/v1/prices", ObjectType::Price),
        ("/v1/subscriptions?status=all", ObjectType::Subscription),
        ("/v1/invoices", ObjectType::Invoice),
    ];
    let types: Vec<ObjectType> = resources.iter().map(|r| r.1).collect();

    let mut scopes: Vec<Option<String>> = vec![None];
    scopes.extend(
        accounts
            .iter()
            .filter(|a| a.connected)
            .map(|a| Some(a.id.clone())),
    );
    for scope in &scopes {
        let headers: Vec<(&str, &str)> = scope
            .as_deref()
            .map(|s| ("Stripe-Account", s))
            .into_iter()
            .collect();
        let mut seen = 0;
        for (path, t) in &resources {
            let api = walk(&m, path, &headers).await;
            let truth = engine_ids(&m, scope.as_deref(), &[*t]).await;
            assert_eq!(api, truth, "{path} in {scope:?}");
            seen += api.len();
        }
        assert!(seen > 0, "{scope:?} has data");
        let all = engine_ids(&m, scope.as_deref(), &types).await;
        assert!(!all.is_empty());
    }

    // The platform's account list is the connected accounts.
    let listed = walk(&m, "/v1/accounts", &[]).await;
    let live: BTreeSet<String> = accounts
        .iter()
        .filter(|a| a.connected)
        .map(|a| a.id.clone())
        .collect();
    assert_eq!(listed, live);
    assert_eq!(engine_ids(&m, None, &[ObjectType::Account]).await, live);

    for a in accounts.iter().filter(|a| !a.connected) {
        let r = get_with(&m, "/v1/customers", &[("Stripe-Account", &a.id)]).await;
        assert_eq!(r.status(), 403, "{}", a.id);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["code"], "account_invalid");
    }
}
