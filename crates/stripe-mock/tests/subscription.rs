mod common;

use std::time::Duration;

use common::mock_with;
use ductkit_stripe_mock::{
    ChangeKind, CustomerUpdate, NewCustomer, SubscribeFilter, SubscribeOptions, Subscription,
    SubscriptionItem,
};
use futures_util::StreamExt;
use serde_json::{Value, json};

async fn next(s: &mut Subscription) -> Option<SubscriptionItem> {
    tokio::time::timeout(Duration::from_secs(5), s.next())
        .await
        .expect("subscription item within 5s")
}

#[tokio::test]
async fn snapshot_then_synced_then_live() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    let mut s = m.subscribe(SubscribeFilter::default());
    let Some(SubscriptionItem::Update(u)) = next(&mut s).await else {
        panic!("expected snapshot item")
    };
    assert_eq!(
        (u.kind, u.object_id.as_str(), u.version, u.cause),
        (ChangeKind::Snapshot, a.as_str(), 1, None)
    );
    assert_eq!(next(&mut s).await, Some(SubscriptionItem::Synced));
    m.update_customer(&a, CustomerUpdate::SetName(Some("B".into())))
        .unwrap();
    let Some(SubscriptionItem::Update(u)) = next(&mut s).await else {
        panic!("expected live item")
    };
    assert_eq!((u.kind, u.version, u.seq), (ChangeKind::Updated, 2, 2));
}

#[tokio::test]
async fn snapshot_includes_tombstones() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&a).unwrap();
    let mut s = m.subscribe(SubscribeFilter::default());
    let Some(SubscriptionItem::Update(u)) = next(&mut s).await else {
        panic!("expected snapshot item")
    };
    assert_eq!(u.kind, ChangeKind::Deleted);
    assert_eq!(u.state["deleted"], true);
    assert_eq!(u.version, 2);
}

#[tokio::test]
async fn no_gap_between_snapshot_and_live() {
    let m = mock_with(
        "seed = 3\n[actions]\nnew_customer = 60.0\n[churn.customer]\nupdate = 120.0\ndelete = 10.0",
    )
    .await;
    m.step(Duration::from_secs(3600)).unwrap();
    let cut = m.truth_log().len() as u64;
    let mut s = m.subscribe(SubscribeFilter::default());
    m.step(Duration::from_secs(3600)).unwrap();
    let total = m.truth_log().len() as u64;
    assert!(total > cut);
    let mut live = vec![];
    let mut synced = false;
    while (live.len() as u64) < total - cut {
        match next(&mut s).await.expect("stream open") {
            SubscriptionItem::Synced => synced = true,
            SubscriptionItem::Update(u) if synced => live.push(u.seq),
            SubscriptionItem::Update(_) => {}
            SubscriptionItem::Lagged => panic!("lagged"),
        }
    }
    assert_eq!(live, ((cut + 1)..=total).collect::<Vec<_>>());
}

#[tokio::test]
async fn type_filter_excludes() {
    let m = mock_with("").await;
    m.create_customer(NewCustomer::default()).unwrap();
    let mut s = m.subscribe(SubscribeFilter {
        object_types: Some(vec![]),
    });
    assert_eq!(next(&mut s).await, Some(SubscriptionItem::Synced));
}

#[tokio::test]
async fn slow_subscriber_lags_without_blocking() {
    let m = mock_with("").await;
    let mut s = m.subscribe_with(SubscribeFilter::default(), SubscribeOptions { buffer: 4 });
    for _ in 0..100 {
        m.create_customer(NewCustomer::default()).unwrap();
    }
    let mut items = vec![];
    while let Some(i) = next(&mut s).await {
        items.push(i);
    }
    assert_eq!(items.first(), Some(&SubscriptionItem::Synced));
    assert_eq!(items.last(), Some(&SubscriptionItem::Lagged));
    assert_eq!(items.len(), 1 + 4 + 1);
}

#[tokio::test]
async fn json_shape_matches_spec() {
    let m = mock_with("").await;
    m.create_customer(NewCustomer::default()).unwrap();
    let v = serde_json::to_value(&m.truth_log()[0]).unwrap();
    assert_eq!(v["at"], "2026-01-01T00:00:00.000Z");
    assert_eq!(v["account"], Value::Null);
    assert_eq!(v["object_type"], "customer");
    assert_eq!(v["kind"], "created");
    assert_eq!(v["events"]["snapshot"].as_array().unwrap().len(), 1);
    assert_eq!(v["events"]["thin"].as_array().unwrap().len(), 1);
    assert_eq!(v["cause"]["action"], "control.create_customer");
    assert_eq!(v["cause"]["action_id"], "act_1");
}
