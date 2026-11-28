//! Coupons, promotion codes and tax rates (phase 9, Task 3): `billing_catalog_change`,
//! `churn.coupon.update` and the control hooks.

mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{Action, ChangeKind, MockError, MockStripe, ObjectType, StateUpdate};
use serde_json::Value;

const DAY: Duration = Duration::from_secs(86_400);

async fn list(m: &MockStripe, path: &str) -> Vec<Value> {
    let r = get(m, path).await;
    assert_eq!(r.status().as_u16(), 200, "{path}");
    let v: Value = r.json().await.unwrap();
    v["data"].as_array().unwrap().clone()
}

/// The newest state of every object of `t` in the log.
fn latest(m: &MockStripe, t: ObjectType) -> Vec<StateUpdate> {
    let mut ids: Vec<String> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == t)
        .map(|u| u.object_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids.iter().map(|id| m.get(t, id).unwrap()).collect()
}

fn active(m: &MockStripe, t: ObjectType) -> Vec<String> {
    latest(m, t)
        .into_iter()
        .filter(|u| u.state["active"] == true)
        .map(|u| u.object_id)
        .collect()
}

/// The snapshot event types announced by the entries of `cause`, in log order.
fn events_of(m: &MockStripe, cause: &ductkit_stripe_mock::Cause) -> Vec<String> {
    let timeline = m.event_timeline();
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .flat_map(|u| u.events.snapshot)
        .filter_map(|id| timeline.iter().find(|e| e.id == id))
        .map(|e| e.event_type.clone())
        .collect()
}

/// Runs `BillingCatalogChange` until `done` holds.
fn change_until(m: &MockStripe, done: impl Fn(&MockStripe) -> bool) {
    for _ in 0..200 {
        if done(m) {
            return;
        }
        m.run_action(Action::BillingCatalogChange).unwrap();
    }
    panic!("200 catalog changes did not reach the wanted state");
}

#[tokio::test]
async fn catalog_creates_coupons_codes_and_rates() {
    let m = mock_with("seed = 4\n[actions]\nbilling_catalog_change = 50.0").await;
    m.step(2 * DAY).await.unwrap();
    for path in ["/v1/coupons", "/v1/promotion_codes", "/v1/tax_rates"] {
        let data = list(&m, &format!("{path}?limit=100")).await;
        assert!(!data.is_empty(), "{path} lists something");
    }
    for t in [
        ObjectType::Coupon,
        ObjectType::PromotionCode,
        ObjectType::TaxRate,
    ] {
        for u in latest(&m, t) {
            check_payload(&u.state).unwrap_or_else(|e| panic!("{t:?} {}: {e}", u.object_id));
        }
    }
    let coupons: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Coupon && u.kind == ChangeKind::Created)
        .collect();
    assert!(coupons.iter().any(|u| !u.state["amount_off"].is_null()));
    assert!(coupons.iter().any(|u| !u.state["percent_off"].is_null()));
    for u in coupons {
        let s = &u.state;
        assert_ne!(
            s["percent_off"].is_null(),
            s["amount_off"].is_null(),
            "exactly one of percent_off and amount_off: {s}"
        );
        assert_eq!(
            s["currency"].is_string(),
            s["amount_off"].is_number(),
            "{s}"
        );
        assert_eq!(u.object_id.len(), 8, "coupon IDs are 8 characters");
    }
    for u in latest(&m, ObjectType::PromotionCode) {
        assert!(u.object_id.starts_with("promo_"));
        let code = u.state["code"].as_str().unwrap();
        assert!(
            code.len() == 8
                && code
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "{code}"
        );
    }
}

#[tokio::test]
async fn deleting_a_coupon_deactivates_its_codes() {
    let m = mock_with("seed = 9").await;
    change_until(&m, |m| !active(m, ObjectType::PromotionCode).is_empty());
    let promo = active(&m, ObjectType::PromotionCode).remove(0);
    let coupon = m.get(ObjectType::PromotionCode, &promo).unwrap().state["promotion"]["coupon"]
        .as_str()
        .unwrap()
        .to_string();
    let cause = m
        .run_action(Action::DeleteCoupon { id: coupon.clone() })
        .unwrap();
    let mine: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(&cause))
        .collect();
    let first_deactivation = mine
        .iter()
        .position(|u| u.object_id == promo)
        .expect("the code is deactivated");
    let deletion = mine
        .iter()
        .position(|u| u.object_id == coupon)
        .expect("the coupon is deleted");
    assert!(first_deactivation < deletion);
    assert_eq!(mine[first_deactivation].state["active"], false);
    assert_eq!(mine[deletion].kind, ChangeKind::Deleted);
    let events = events_of(&m, &cause);
    let updated = events.iter().position(|e| e == "promotion_code.updated");
    let deleted = events.iter().position(|e| e == "coupon.deleted");
    assert!(
        updated.is_some() && deleted.is_some() && updated < deleted,
        "{events:?}"
    );

    let r = get(&m, &format!("/v1/coupons/{coupon}")).await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v,
        serde_json::json!({"id": coupon, "object": "coupon", "deleted": true})
    );
    let r = get(&m, &format!("/v1/promotion_codes/{promo}")).await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["active"], false);

    // A second delete, and a delete of a coupon that never existed, are 409s naming it.
    for id in [coupon.clone(), "NoSuchOne".to_string()] {
        match m.run_action(Action::DeleteCoupon { id: id.clone() }) {
            Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&id), "{msg}"),
            other => panic!("expected invalid_action, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn archive_deactivates_codes_and_tax_rates() {
    let m = mock_with("seed = 2").await;
    change_until(&m, |m| {
        !active(m, ObjectType::PromotionCode).is_empty()
            && !active(m, ObjectType::TaxRate).is_empty()
    });
    for (t, event) in [
        (ObjectType::PromotionCode, "promotion_code.updated"),
        (ObjectType::TaxRate, "tax_rate.updated"),
    ] {
        let id = active(&m, t).remove(0);
        let cause = m.run_action(Action::Archive { id: id.clone() }).unwrap();
        let state = m.get(t, &id).unwrap();
        assert_eq!(state.state["active"], false);
        assert_eq!(events_of(&m, &cause), vec![event.to_string()]);
        match m.run_action(Action::Archive { id: id.clone() }) {
            Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&id), "{msg}"),
            other => panic!("expected invalid_action, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn coupon_churn_updates_metadata() {
    let m = mock_with("seed = 5\n[churn.coupon]\nupdate = 50.0").await;
    change_until(&m, |m| !latest(m, ObjectType::Coupon).is_empty());
    m.step(DAY).await.unwrap();
    let updates: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Coupon && u.kind == ChangeKind::Updated)
        .collect();
    assert!(!updates.is_empty());
    assert!(
        updates
            .iter()
            .all(|u| u.state["metadata"]["note"].is_string())
    );
    let timeline = m.event_timeline();
    assert!(
        updates
            .iter()
            .flat_map(|u| u.events.snapshot.iter())
            .any(|id| timeline
                .iter()
                .any(|e| &e.id == id && e.event_type == "coupon.updated"))
    );
}

#[tokio::test]
async fn default_config_creates_no_billing_catalog() {
    let m = mock_with("seed = 4").await;
    m.step(2 * DAY).await.unwrap();
    for path in ["/v1/coupons", "/v1/promotion_codes", "/v1/tax_rates"] {
        assert!(list(&m, path).await.is_empty(), "{path}");
    }
}

#[tokio::test]
async fn catalog_rates_are_validated() {
    for toml in [
        "[actions]\nbilling_catalog_change = -1.0",
        "[churn.coupon]\nupdate = -1.0",
    ] {
        let err = ductkit_stripe_mock::Config::from_toml_str(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("actions.billing_catalog_change") || msg.contains("churn.coupon.update"),
            "{msg}"
        );
    }
}
