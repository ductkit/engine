mod common;

use std::time::Duration;

use common::mock_with;
use ductkit_stripe_mock::{Action, ChangeKind, MockError, ObjectType};

#[tokio::test]
async fn catalog_change_creates_products_and_prices() {
    let m = mock_with("seed = 3").await;
    for _ in 0..20 {
        m.run_action(Action::CatalogChange).unwrap();
    }
    let log = m.truth_log();
    assert!(
        log.iter()
            .any(|u| u.object_type == ObjectType::Product && u.kind == ChangeKind::Created)
    );
    let prices: Vec<_> = log
        .iter()
        .filter(|u| u.object_type == ObjectType::Price)
        .collect();
    assert!(!prices.is_empty());
    for p in &prices {
        assert!(p.state["product"].as_str().unwrap().starts_with("prod_"));
        assert!(p.object_id.starts_with("price_"));
    }
    assert!(
        log.iter().any(|u| u.kind == ChangeKind::Archived),
        "20 changes at seed 3 archive at least once"
    );
}

#[tokio::test]
async fn one_action_shares_a_cause() {
    let m = mock_with("").await;
    let cause = m.run_action(Action::CatalogChange).unwrap();
    let mine: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(&cause))
        .collect();
    assert!(
        mine.len() >= 2,
        "product + price(s) (+ default_price update)"
    );
    assert!(mine.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
}

#[tokio::test]
async fn archived_price_event_has_previous_active() {
    let m = mock_with("seed = 5\n[churn.price]\narchive = 60.0").await;
    m.run_action(Action::CatalogChange).unwrap();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let u = m
        .truth_log()
        .into_iter()
        .find(|u| u.kind == ChangeKind::Archived)
        .unwrap();
    assert_eq!(u.state["active"], false);
    let ev = m
        .event_timeline()
        .into_iter()
        .find(|e| e.id == u.events.snapshot[0])
        .unwrap();
    assert_eq!(ev.event_type, "price.updated");
}

#[tokio::test]
async fn step_delay_spaces_follow_up_steps() {
    let m = mock_with("[actions]\nstep_delay = { distribution = \"fixed\", ms = 400 }").await;
    let cause = m.run_action(Action::CatalogChange).unwrap();
    let mine = |m: &ductkit_stripe_mock::MockStripe| {
        m.truth_log()
            .into_iter()
            .filter(|u| u.cause.as_ref() == Some(&cause))
            .map(|u| u.at)
            .collect::<Vec<_>>()
    };
    let first = mine(&m);
    m.step(Duration::from_secs(5)).await.unwrap();
    let all = mine(&m);
    assert!(all.len() > first.len(), "later steps waited for step_delay");
    assert!(
        all.windows(2)
            .all(|w| w[1].0 - w[0].0 == 400 || w[1].0 == w[0].0)
    );
}

#[tokio::test]
async fn archive_hook_archives_once() {
    let m = mock_with("").await;
    m.run_action(Action::CatalogChange).unwrap();
    let price = m
        .truth_log()
        .into_iter()
        .find(|u| u.object_type == ObjectType::Price)
        .unwrap()
        .object_id;
    m.run_action(Action::Archive { id: price.clone() }).unwrap();
    let latest = m.get(ObjectType::Price, &price).unwrap();
    assert_eq!(
        (latest.kind, latest.state["active"].clone()),
        (ChangeKind::Archived, serde_json::json!(false))
    );
    assert!(matches!(
        m.run_action(Action::Archive { id: price }),
        Err(MockError::InvalidAction(_))
    ));
}
