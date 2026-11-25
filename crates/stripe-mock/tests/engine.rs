mod common;

use std::time::Duration;

use common::mock_with;
use ductkit_stripe_mock::{
    ChangeKind, CustomerUpdate, MockError, MockStripe, NewCustomer, ObjectType,
};

const BUSY: &str =
    "seed = 7\n[actions]\nnew_customer = 10.0\n[churn.customer]\nupdate = 30.0\ndelete = 2.0";

fn hours(h: u64) -> Duration {
    Duration::from_secs(h * 3600)
}

#[tokio::test]
async fn same_seed_same_log() {
    let (a, b) = (mock_with(BUSY).await, mock_with(BUSY).await);
    a.step(hours(48)).unwrap();
    b.step(hours(48)).unwrap();
    let (la, lb) = (a.truth_log(), b.truth_log());
    assert!(la.len() > 100, "{}", la.len());
    assert_eq!(la, lb);
}

#[tokio::test]
async fn different_seed_different_log() {
    let a = mock_with(BUSY).await;
    let b = mock_with(&BUSY.replace("seed = 7", "seed = 8")).await;
    a.step(hours(48)).unwrap();
    b.step(hours(48)).unwrap();
    assert_ne!(a.truth_log(), b.truth_log());
}

#[tokio::test]
async fn creates_independent_of_update_rate() {
    let created = |m: &MockStripe| {
        m.truth_log()
            .into_iter()
            .filter(|u| u.kind == ChangeKind::Created)
            .map(|u| (u.object_id, u.at))
            .collect::<Vec<_>>()
    };
    let a = mock_with("seed = 7\n[actions]\nnew_customer = 10.0").await;
    let b = mock_with("seed = 7\n[actions]\nnew_customer = 10.0\n[churn.customer]\nupdate = 30.0")
        .await;
    a.step(hours(24)).unwrap();
    b.step(hours(24)).unwrap();
    assert_eq!(created(&a), created(&b));
}

#[tokio::test]
async fn rate_roughly_matches() {
    let m = mock_with("[actions]\nnew_customer = 10.0").await;
    m.step(hours(100)).unwrap();
    let n = m.truth_log().len();
    assert!((850..=1150).contains(&n), "{n}");
}

#[tokio::test]
async fn zero_rates_do_nothing() {
    let m = mock_with("").await;
    m.step(hours(1000)).unwrap();
    assert!(m.truth_log().is_empty());
}

#[tokio::test]
async fn clock_moves_exactly() {
    let m = mock_with("").await;
    let t0 = m.now();
    m.step(Duration::from_millis(1500)).unwrap();
    assert_eq!(m.now(), t0.plus_ms(1500));
    assert!(m.advance_to(m.now()).is_ok());
    assert!(matches!(
        m.advance_to(t0),
        Err(MockError::TimeTravel { .. })
    ));
}

#[tokio::test]
async fn control_calls() {
    let m = mock_with("").await;
    let id = m
        .create_customer(NewCustomer {
            email: Some("a@example.com".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(id.starts_with("cus_"));
    assert_eq!(
        m.update_customer(&id, CustomerUpdate::SetName(Some("A".into())))
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        m.update_customer(&id, CustomerUpdate::SetName(Some("A".into())))
            .unwrap(),
        None
    );
    m.delete_customer(&id).unwrap();
    let last = m.get(ObjectType::Customer, &id).unwrap();
    assert_eq!((last.kind, last.version), (ChangeKind::Deleted, 3));
    assert_eq!(last.cause.unwrap().action, "control.delete_customer");
}

#[tokio::test]
async fn control_on_deleted_customer_errors() {
    let m = mock_with("").await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&id).unwrap();
    let before = m.truth_log().len();
    assert!(matches!(
        m.update_customer(&id, CustomerUpdate::SetEmail(None)),
        Err(MockError::Deleted { .. })
    ));
    assert!(matches!(
        m.delete_customer(&id),
        Err(MockError::Deleted { .. })
    ));
    assert!(matches!(
        m.delete_customer("cus_nope"),
        Err(MockError::NoSuchObject { .. })
    ));
    assert_eq!(m.truth_log().len(), before);
}
