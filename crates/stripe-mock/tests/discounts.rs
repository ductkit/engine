//! Discounts and computed invoice amounts (phase 9, Task 5, rulings Q2, Q3 and Q7).

mod common;

use std::time::Duration;

use common::{get, mock_with, recurring_prices};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{
    Action, Cause, ChangeKind, EventFormat, InvoiceOutcome, MockError, MockStripe, ObjectType,
    StateUpdate,
};
use serde_json::Value;

const DAY: Duration = Duration::from_secs(86_400);

/// The truth-log entries of `cause`, in log order.
fn entries(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
}

/// The snapshot event types announced by the entries of `cause`, in log order.
fn events_of(m: &MockStripe, cause: &Cause) -> Vec<String> {
    let timeline = m.event_timeline();
    entries(m, cause)
        .into_iter()
        .flat_map(|u| u.events.snapshot)
        .filter_map(|id| timeline.iter().find(|e| e.id == id))
        .map(|e| e.event_type.clone())
        .collect()
}

/// The ID of the object of `t` the entries of `cause` create first.
fn created_by(m: &MockStripe, cause: &Cause, t: ObjectType) -> String {
    entries(m, cause)
        .into_iter()
        .find(|u| u.object_type == t && u.kind == ChangeKind::Created)
        .unwrap_or_else(|| panic!("{cause:?} creates no {t:?}"))
        .object_id
}

fn state(m: &MockStripe, t: ObjectType, id: &str) -> Value {
    m.get(t, id).unwrap().state
}

/// The newest state of every object of `t` in the log, in first-seen order.
fn latest(m: &MockStripe, t: ObjectType) -> Vec<StateUpdate> {
    let mut ids: Vec<String> = Vec::new();
    for u in m.truth_log() {
        if u.object_type == t && !ids.contains(&u.object_id) {
            ids.push(u.object_id);
        }
    }
    ids.iter().map(|id| m.get(t, id).unwrap()).collect()
}

/// Runs `BillingCatalogChange` until a live, valid coupon matches `want`; returns its ID.
fn coupon_where(m: &MockStripe, want: impl Fn(&Value) -> bool) -> String {
    for _ in 0..1000 {
        if let Some(u) = latest(m, ObjectType::Coupon)
            .into_iter()
            .find(|u| u.kind != ChangeKind::Deleted && u.state["valid"] == true && want(&u.state))
        {
            return u.object_id;
        }
        m.run_action(Action::BillingCatalogChange).unwrap();
    }
    panic!("no matching coupon after 1000 billing catalog changes");
}

fn percent_coupon(m: &MockStripe, duration: &str) -> String {
    coupon_where(m, |c| {
        c["percent_off"].is_number() && c["duration"] == duration && c["max_redemptions"].is_null()
    })
}

fn add(customer: Option<&str>, subscription: Option<&str>, coupon: Option<&str>) -> Action {
    Action::AddDiscount {
        customer: customer.map(str::to_string),
        subscription: subscription.map(str::to_string),
        coupon: coupon.map(str::to_string),
        promotion_code: None,
    }
}

/// Adds a discount with `coupon` to a new customer; returns `(customer, discount)`.
fn discounted_customer(m: &MockStripe, coupon: &str) -> (String, String) {
    let cust = m.create_customer(Default::default()).unwrap();
    let cause = m.run_action(add(Some(&cust), None, Some(coupon))).unwrap();
    let di = created_by(m, &cause, ObjectType::Discount);
    (cust, di)
}

/// A paid one-off invoice for `customer`; returns `(cause, invoice)`.
async fn paid_invoice(m: &MockStripe, customer: &str, outcome: InvoiceOutcome) -> (Cause, String) {
    let cause = m
        .run_action(Action::OneOffInvoice {
            customer: Some(customer.to_string()),
            outcome: Some(outcome),
        })
        .unwrap();
    m.step(DAY).await.unwrap();
    let id = created_by(m, &cause, ObjectType::Invoice);
    (cause, id)
}

/// Checks ruling Q3's sums on an invoice carrying discount `di` (or none). The line sums
/// are checked when every line is on the embedded page.
fn assert_amounts(invoice: &Value, di: Option<&str>) {
    let all_lines = invoice["lines"]["has_more"] == false;
    let lines = invoice["lines"]["data"].as_array().unwrap().clone();
    let subtotal = if all_lines {
        lines.iter().map(|l| l["amount"].as_i64().unwrap()).sum()
    } else {
        invoice["subtotal"].as_i64().unwrap()
    };
    assert_eq!(invoice["subtotal"], subtotal);
    assert_eq!(invoice["subtotal_excluding_tax"], subtotal);
    let line_discounts: i64 = lines
        .iter()
        .flat_map(|l| l["discount_amounts"].as_array().unwrap().clone())
        .map(|d| d["amount"].as_i64().unwrap())
        .sum();
    let d = match di {
        Some(di) => {
            assert_eq!(invoice["discounts"], serde_json::json!([di]));
            let totals = invoice["total_discount_amounts"].as_array().unwrap();
            assert_eq!(totals.len(), 1, "{invoice}");
            assert_eq!(totals[0]["discount"], di);
            for l in &lines {
                assert_eq!(l["discounts"], serde_json::json!([di]));
                assert_eq!(l["discount_amounts"][0]["discount"], di);
            }
            totals[0]["amount"].as_i64().unwrap()
        }
        None => {
            assert_eq!(invoice["discounts"], serde_json::json!([]));
            assert_eq!(invoice["total_discount_amounts"], serde_json::json!([]));
            0
        }
    };
    if all_lines {
        assert_eq!(line_discounts, d);
    }
    assert_eq!(invoice["total_excluding_tax"], subtotal - d);
    assert_eq!(invoice["total"], subtotal - d);
    assert_eq!(invoice["amount_due"], subtotal - d);
}

#[tokio::test]
async fn customer_discount_applies_to_a_one_off_invoice() {
    let m = mock_with("seed = 4").await;
    let coupon = coupon_where(&m, |c| c["percent_off"] == 20.0);
    let (cust, di) = discounted_customer(&m, &coupon);
    let discount = state(&m, ObjectType::Customer, &cust)["discount"].clone();
    assert_eq!(discount["id"], di.as_str());
    assert_eq!(discount["customer"], cust.as_str());
    assert_eq!(discount["source"]["coupon"], coupon.as_str());
    check_payload(&state(&m, ObjectType::Discount, &di)).unwrap();

    let (cause, id) = paid_invoice(&m, &cust, InvoiceOutcome::Paid).await;
    let invoice = state(&m, ObjectType::Invoice, &id);
    assert_eq!(invoice["status"], "paid");
    assert_amounts(&invoice, Some(&di));
    let subtotal = invoice["subtotal"].as_i64().unwrap();
    assert_eq!(
        invoice["total_discount_amounts"][0]["amount"],
        (subtotal * 2000 + 5000) / 10000
    );
    // The charge takes what was due, and the invoice records what it charged.
    let charge = created_by(&m, &cause, ObjectType::Charge);
    let charge = state(&m, ObjectType::Charge, &charge);
    assert_eq!(charge["amount"], invoice["amount_due"]);
    assert_eq!(invoice["amount_paid"], invoice["amount_due"]);
    assert_eq!(invoice["amount_remaining"], 0);
    // The discount is announced before the invoice.
    let timeline = m.event_timeline();
    let pos = |t: &str, obj: &str| {
        timeline
            .iter()
            .position(|e| e.event_type == t && e.object_id == obj)
            .unwrap_or_else(|| panic!("no {t} for {obj}"))
    };
    assert!(pos("customer.discount.created", &di) < pos("invoice.created", &id));
}

#[tokio::test]
async fn subscription_discount_wins_over_customer() {
    let m = mock_with("seed = 5").await;
    let coupon = percent_coupon(&m, "forever");
    let (cust, cust_di) = discounted_customer(&m, &coupon);
    let prices = recurring_prices(&m, 1, "month");
    let start = m
        .run_action(Action::StartSubscription {
            customer: Some(cust.clone()),
            prices: Some(prices),
        })
        .unwrap();
    let sub = created_by(&m, &start, ObjectType::Subscription);
    m.step(DAY).await.unwrap();
    let first = created_by(&m, &start, ObjectType::Invoice);
    assert_amounts(&state(&m, ObjectType::Invoice, &first), Some(&cust_di));
    let cause = m.run_action(add(None, Some(&sub), Some(&coupon))).unwrap();
    let sub_di = created_by(&m, &cause, ObjectType::Discount);
    assert_eq!(
        events_of(&m, &cause),
        ["customer.discount.created", "customer.subscription.updated"]
    );
    let s = state(&m, ObjectType::Subscription, &sub);
    assert_eq!(s["discounts"], serde_json::json!([sub_di]));
    let d = state(&m, ObjectType::Discount, &sub_di);
    assert_eq!(d["subscription"], sub.as_str());
    assert_eq!(d["customer"], cust.as_str());
    // The renewal applies the subscription's discount.
    m.step(32 * DAY).await.unwrap();
    let renewal = state(&m, ObjectType::Subscription, &sub)["latest_invoice"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(renewal, first);
    assert_amounts(&state(&m, ObjectType::Invoice, &renewal), Some(&sub_di));
    // The customer's discount is untouched.
    assert_eq!(
        state(&m, ObjectType::Customer, &cust)["discount"]["id"],
        cust_di.as_str()
    );
    // `expand[]=discounts` on the subscription gives the discount.
    let r = get(&m, &format!("/v1/subscriptions/{sub}?expand[]=discounts")).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["discounts"][0]["id"], sub_di.as_str());
}

#[tokio::test]
async fn once_discount_ends_at_first_finalization() {
    let m = mock_with("seed = 6").await;
    let coupon = percent_coupon(&m, "once");
    let (cust, di) = discounted_customer(&m, &coupon);
    let (_, first) = paid_invoice(&m, &cust, InvoiceOutcome::Paid).await;
    assert_amounts(&state(&m, ObjectType::Invoice, &first), Some(&di));
    assert!(state(&m, ObjectType::Customer, &cust)["discount"].is_null());
    let deleted: Vec<_> = m
        .event_timeline()
        .into_iter()
        .filter(|e| {
            e.format == EventFormat::Snapshot
                && e.event_type == "customer.discount.deleted"
                && e.object_id == di
        })
        .collect();
    assert_eq!(deleted.len(), 1);
    // In the instant the invoice was finalized.
    let finalized = m
        .event_timeline()
        .into_iter()
        .find(|e| e.event_type == "invoice.finalized" && e.object_id == first)
        .unwrap();
    assert_eq!(deleted[0].created, finalized.created);
    let tomb = m.get(ObjectType::Discount, &di).unwrap();
    assert_eq!(tomb.kind, ChangeKind::Deleted);
    assert_eq!(tomb.state["deleted"], true);
    assert_eq!(tomb.state["object"], "discount");
    check_payload(&tomb.state).unwrap();

    let (_, later) = paid_invoice(&m, &cust, InvoiceOutcome::Paid).await;
    assert_amounts(&state(&m, ObjectType::Invoice, &later), None);
}

#[tokio::test]
async fn once_discount_ends_even_when_its_invoice_is_voided() {
    let m = mock_with("seed = 6").await;
    let coupon = percent_coupon(&m, "once");
    let (cust, di) = discounted_customer(&m, &coupon);
    let (_, first) = paid_invoice(&m, &cust, InvoiceOutcome::Void).await;
    let invoice = state(&m, ObjectType::Invoice, &first);
    assert_eq!(invoice["status"], "void");
    assert_amounts(&invoice, Some(&di));
    assert!(state(&m, ObjectType::Customer, &cust)["discount"].is_null());
    assert_eq!(
        m.get(ObjectType::Discount, &di).unwrap().kind,
        ChangeKind::Deleted
    );
}

#[tokio::test]
async fn repeating_discount_ends_on_time() {
    let m = mock_with("seed = 7").await;
    let coupon = coupon_where(&m, |c| {
        c["percent_off"].is_number() && c["duration"] == "repeating" && c["duration_in_months"] == 1
    });
    let (cust, di) = discounted_customer(&m, &coupon);
    let d = state(&m, ObjectType::Discount, &di);
    let (start, end) = (d["start"].as_i64().unwrap(), d["end"].as_i64().unwrap());
    // One month after the start.
    assert!((28 * 86_400..=31 * 86_400).contains(&(end - start)), "{d}");
    m.step(Duration::from_secs((end - start - 1) as u64))
        .await
        .unwrap();
    assert!(m.now().0 < end * 1000);
    assert_eq!(
        state(&m, ObjectType::Customer, &cust)["discount"]["id"],
        di.as_str()
    );
    m.step(Duration::from_secs(2)).await.unwrap();
    assert!(state(&m, ObjectType::Customer, &cust)["discount"].is_null());
    let e = m
        .event_timeline()
        .into_iter()
        .find(|e| e.event_type == "customer.discount.deleted" && e.object_id == di)
        .expect("the discount ended");
    assert_eq!(e.created.0, end * 1000);
}

#[tokio::test]
async fn deleted_discount_stays_expandable_on_old_invoices() {
    let m = mock_with("seed = 8").await;
    let coupon = percent_coupon(&m, "forever");
    let (cust, di) = discounted_customer(&m, &coupon);
    let (_, invoice) = paid_invoice(&m, &cust, InvoiceOutcome::Paid).await;
    let removal = m
        .run_action(Action::RemoveDiscount {
            id: Some(di.clone()),
        })
        .unwrap();
    assert_eq!(events_of(&m, &removal), ["customer.discount.deleted"]);
    assert!(state(&m, ObjectType::Customer, &cust)["discount"].is_null());
    let r = get(&m, &format!("/v1/invoices/{invoice}?expand[]=discounts")).await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["discounts"][0]["id"], di.as_str());
    assert_eq!(v["discounts"][0]["deleted"], true);
    assert_eq!(v["discounts"][0]["object"], "discount");
    // Removed again: a 409 naming it.
    match m.run_action(Action::RemoveDiscount {
        id: Some(di.clone()),
    }) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&di), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn customer_deletion_removes_its_discount_first() {
    let m = mock_with("seed = 9").await;
    let coupon = percent_coupon(&m, "forever");
    let (cust, di) = discounted_customer(&m, &coupon);
    let prices = recurring_prices(&m, 1, "month");
    m.run_action(Action::StartSubscription {
        customer: Some(cust.clone()),
        prices: Some(prices),
    })
    .unwrap();
    m.step(DAY).await.unwrap();
    m.delete_customer(&cust).unwrap();
    let deletion = m
        .truth_log()
        .into_iter()
        .find(|u| u.object_type == ObjectType::Customer && u.kind == ChangeKind::Deleted)
        .unwrap()
        .cause
        .unwrap();
    let events = events_of(&m, &deletion);
    assert_eq!(events[0], "customer.discount.deleted", "{events:?}");
    assert_eq!(events.last().unwrap(), "customer.deleted");
    assert_eq!(
        m.get(ObjectType::Discount, &di).unwrap().kind,
        ChangeKind::Deleted
    );
}

#[tokio::test]
async fn canceled_subscription_keeps_its_discount() {
    let m = mock_with("seed = 9").await;
    let coupon = percent_coupon(&m, "forever");
    let cust = m.create_customer(Default::default()).unwrap();
    let prices = recurring_prices(&m, 1, "month");
    let start = m
        .run_action(Action::StartSubscription {
            customer: Some(cust.clone()),
            prices: Some(prices),
        })
        .unwrap();
    let sub = created_by(&m, &start, ObjectType::Subscription);
    m.step(DAY).await.unwrap();
    let cause = m.run_action(add(None, Some(&sub), Some(&coupon))).unwrap();
    let di = created_by(&m, &cause, ObjectType::Discount);
    m.run_action(Action::CancelSubscription {
        id: Some(sub.clone()),
        at_period_end: Some(false),
    })
    .unwrap();
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["discounts"],
        serde_json::json!([di])
    );
    assert_eq!(
        m.get(ObjectType::Discount, &di).unwrap().kind,
        ChangeKind::Created
    );
}

#[tokio::test]
async fn foreign_currency_amount_off_applies_nothing() {
    let m = mock_with("seed = 10").await;
    let coupon = coupon_where(&m, |c| {
        c["amount_off"].is_number() && c["duration"] != "once"
    });
    let currency = state(&m, ObjectType::Coupon, &coupon)["currency"]
        .as_str()
        .unwrap()
        .to_string();
    // A monthly price in another currency.
    let mut price = None;
    for _ in 0..300 {
        price = latest(&m, ObjectType::Price).into_iter().find(|u| {
            u.state["active"] == true
                && u.state["recurring"]["interval"] == "month"
                && u.state["currency"] != currency.as_str()
        });
        if price.is_some() {
            break;
        }
        m.run_action(Action::CatalogChange).unwrap();
    }
    let price = price.expect("a price in another currency").object_id;
    let (cust, di) = discounted_customer(&m, &coupon);
    let start = m
        .run_action(Action::StartSubscription {
            customer: Some(cust.clone()),
            prices: Some(vec![price]),
        })
        .unwrap();
    m.step(DAY).await.unwrap();
    let invoice = created_by(&m, &start, ObjectType::Invoice);
    let invoice = state(&m, ObjectType::Invoice, &invoice);
    assert_ne!(invoice["currency"], currency.as_str());
    assert_amounts(&invoice, None);
    assert_eq!(invoice["total"], invoice["subtotal"]);
    // The discount stays for a later invoice in its currency.
    assert_eq!(
        state(&m, ObjectType::Customer, &cust)["discount"]["id"],
        di.as_str()
    );
}

#[tokio::test]
async fn deleted_coupon_leaves_its_discounts() {
    let m = mock_with("seed = 11").await;
    let coupon = percent_coupon(&m, "forever");
    let (cust, di) = discounted_customer(&m, &coupon);
    m.run_action(Action::DeleteCoupon { id: coupon.clone() })
        .unwrap();
    assert_eq!(
        m.get(ObjectType::Coupon, &coupon).unwrap().kind,
        ChangeKind::Deleted
    );
    let (_, invoice) = paid_invoice(&m, &cust, InvoiceOutcome::Paid).await;
    let invoice = state(&m, ObjectType::Invoice, &invoice);
    assert_amounts(&invoice, Some(&di));
    let r = get(
        &m,
        &format!("/v1/customers/{cust}?expand[]=discount.source.coupon"),
    )
    .await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["discount"]["source"]["coupon"], coupon.as_str());
}

#[tokio::test]
async fn discount_thin_event_points_at_the_parent() {
    let m = mock_with("seed = 12").await;
    let coupon = percent_coupon(&m, "forever");
    let (cust, cust_di) = discounted_customer(&m, &coupon);
    let prices = recurring_prices(&m, 1, "month");
    let start = m
        .run_action(Action::StartSubscription {
            customer: Some(cust.clone()),
            prices: Some(prices),
        })
        .unwrap();
    let sub = created_by(&m, &start, ObjectType::Subscription);
    let cause = m.run_action(add(None, Some(&sub), Some(&coupon))).unwrap();
    let sub_di = created_by(&m, &cause, ObjectType::Discount);
    for (di, url) in [
        (cust_di, format!("/v1/customers/{cust}")),
        (sub_di, format!("/v1/subscriptions/{sub}")),
    ] {
        let thin = m
            .event_timeline()
            .into_iter()
            .find(|e| {
                e.format == EventFormat::Thin
                    && e.event_type == "v1.customer.discount.created"
                    && e.object_id == di
            })
            .expect("a thin event");
        let r = get(&m, &format!("/v2/core/events/{}", thin.id)).await;
        assert_eq!(r.status().as_u16(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["related_object"]["id"], di.as_str());
        assert_eq!(v["related_object"]["type"], "discount");
        assert_eq!(v["related_object"]["url"], url.as_str());
    }
}

#[tokio::test]
async fn promotion_code_redemptions_count() {
    let m = mock_with("seed = 13").await;
    let mut promo = None;
    for _ in 0..500 {
        promo = latest(&m, ObjectType::PromotionCode).into_iter().find(|u| {
            u.state["active"] == true && {
                let c = state(
                    &m,
                    ObjectType::Coupon,
                    u.state["promotion"]["coupon"].as_str().unwrap(),
                );
                c["valid"] == true
            }
        });
        if promo.is_some() {
            break;
        }
        m.run_action(Action::BillingCatalogChange).unwrap();
    }
    let promo = promo.expect("an active promotion code");
    let coupon = promo.state["promotion"]["coupon"]
        .as_str()
        .unwrap()
        .to_string();
    let before = state(&m, ObjectType::Coupon, &coupon)["times_redeemed"]
        .as_i64()
        .unwrap();
    let cust = m.create_customer(Default::default()).unwrap();
    let cause = m
        .run_action(Action::AddDiscount {
            customer: Some(cust.clone()),
            subscription: None,
            coupon: None,
            promotion_code: Some(promo.object_id.clone()),
        })
        .unwrap();
    // Redemptions are silent; the discount is announced.
    assert_eq!(events_of(&m, &cause), ["customer.discount.created"]);
    let di = created_by(&m, &cause, ObjectType::Discount);
    let d = state(&m, ObjectType::Discount, &di);
    assert_eq!(d["promotion_code"], promo.object_id.as_str());
    assert_eq!(d["source"]["coupon"], coupon.as_str());
    assert_eq!(
        state(&m, ObjectType::PromotionCode, &promo.object_id)["times_redeemed"],
        1
    );
    assert_eq!(
        state(&m, ObjectType::Coupon, &coupon)["times_redeemed"],
        before + 1
    );
    let r = get(
        &m,
        &format!("/v1/customers/{cust}?expand[]=discount.promotion_code"),
    )
    .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["discount"]["promotion_code"]["id"],
        promo.object_id.as_str()
    );
}

#[tokio::test]
async fn max_redemptions_invalidates_the_coupon() {
    let m = mock_with("seed = 14").await;
    let coupon = coupon_where(&m, |c| {
        c["max_redemptions"].as_i64().is_some_and(|n| n <= 30) && c["times_redeemed"] == 0
    });
    let max = state(&m, ObjectType::Coupon, &coupon)["max_redemptions"]
        .as_i64()
        .unwrap();
    for _ in 0..max {
        discounted_customer(&m, &coupon);
    }
    let c = state(&m, ObjectType::Coupon, &coupon);
    assert_eq!(
        (c["times_redeemed"].as_i64(), &c["valid"]),
        (Some(max), &Value::Bool(false))
    );
    let cust = m.create_customer(Default::default()).unwrap();
    match m.run_action(add(Some(&cust), None, Some(&coupon))) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&coupon), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn add_discount_errors_name_their_target() {
    let m = mock_with("seed = 15").await;
    let coupon = percent_coupon(&m, "forever");
    let (cust, di) = discounted_customer(&m, &coupon);
    let expect_409 = |a: Action, needle: &str| match m.run_action(a) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(needle), "{msg}"),
        other => panic!("{other:?}"),
    };
    // Already discounted: the message names the target and its discount.
    expect_409(add(Some(&cust), None, Some(&coupon)), &di);
    expect_409(add(Some(&cust), None, Some(&coupon)), &cust);
    // Both targets.
    expect_409(
        add(Some(&cust), Some("sub_1"), Some(&coupon)),
        "subscription",
    );
    // Both a coupon and a promotion code.
    let other = m.create_customer(Default::default()).unwrap();
    expect_409(
        Action::AddDiscount {
            customer: Some(other.clone()),
            subscription: None,
            coupon: Some(coupon.clone()),
            promotion_code: Some("promo_1".into()),
        },
        "promotion",
    );
    // A missing coupon, a deleted one, an unknown code and a missing customer.
    expect_409(add(Some(&other), None, Some("NoSuchCp")), "NoSuchCp");
    m.run_action(Action::DeleteCoupon { id: coupon.clone() })
        .unwrap();
    expect_409(add(Some(&other), None, Some(&coupon)), &coupon);
    expect_409(
        Action::AddDiscount {
            customer: Some(other.clone()),
            subscription: None,
            coupon: None,
            promotion_code: Some("promo_missing".into()),
        },
        "promo_missing",
    );
    expect_409(add(Some("cus_missing"), None, None), "cus_missing");
    expect_409(add(None, Some("sub_missing"), None), "sub_missing");
    // Nothing was created by the failures.
    assert_eq!(
        m.truth_log()
            .into_iter()
            .filter(|u| u.object_type == ObjectType::Discount)
            .count(),
        1
    );
}

#[tokio::test]
async fn generated_discounts_conform_and_add_up() {
    let m = mock_with(
        "seed = 16
[actions]
new_customer = 5.0
billing_catalog_change = 3.0
discount_change = 4.0
one_off_invoice = 4.0
start_subscription = 2.0
invoice_item = 2.0
[churn.customer]
delete = 0.5",
    )
    .await;
    m.step(3 * DAY).await.unwrap();
    let discounts = latest(&m, ObjectType::Discount);
    assert!(discounts.len() >= 5, "{} discounts", discounts.len());
    assert!(discounts.iter().any(|u| u.kind == ChangeKind::Deleted));
    for u in &discounts {
        check_payload(&u.state).unwrap_or_else(|e| panic!("{}: {e}", u.object_id));
    }
    let mut discounted = 0;
    for u in latest(&m, ObjectType::Invoice) {
        if u.kind == ChangeKind::Deleted {
            continue;
        }
        check_payload(&u.state).unwrap_or_else(|e| panic!("{}: {e}", u.object_id));
        let di = u.state["discounts"][0].as_str().map(str::to_string);
        discounted += usize::from(di.is_some());
        assert_amounts(&u.state, di.as_deref());
        if u.state["status"] == "paid" {
            assert_eq!(u.state["amount_paid"], u.state["amount_due"]);
        }
    }
    assert!(discounted > 0, "no invoice carried a discount");
    // At most one live discount per parent.
    for t in [ObjectType::Customer, ObjectType::Subscription] {
        for u in latest(&m, t) {
            if t == ObjectType::Subscription {
                assert!(u.state["discounts"].as_array().unwrap().len() <= 1);
                for d in u.state["discounts"].as_array().unwrap() {
                    assert_eq!(
                        m.get(ObjectType::Discount, d.as_str().unwrap())
                            .unwrap()
                            .kind,
                        ChangeKind::Created,
                        "{} names a removed discount",
                        u.object_id
                    );
                }
            } else if let Some(d) = u.state["discount"]["id"].as_str() {
                assert_eq!(
                    m.get(ObjectType::Discount, d).unwrap().kind,
                    ChangeKind::Created
                );
            }
        }
    }
}
