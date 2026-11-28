//! Subscription schedules (phase 9, Task 8, ruling Q9): phases that swap a subscription's
//! items at renewal, and the release, cancel and early endings.

mod common;

use std::time::Duration;

use chrono::{DateTime, Months};
use common::{get, mock_with, recurring_prices};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{
    Action, Cause, ChangeKind, MockError, MockStripe, ObjectType, RenewMode, ScheduleEndBehavior,
    SimTime, StateUpdate,
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

/// The snapshot event types announced by `log`, in order.
fn events_in(m: &MockStripe, log: &[StateUpdate]) -> Vec<String> {
    let timeline = m.event_timeline();
    log.iter()
        .flat_map(|u| u.events.snapshot.clone())
        .filter_map(|id| timeline.iter().find(|e| e.id == id))
        .map(|e| e.event_type.clone())
        .collect()
}

/// The snapshot event types announced by the entries of `cause`, in log order.
fn events_of(m: &MockStripe, cause: &Cause) -> Vec<String> {
    events_in(m, &entries(m, cause))
}

/// The `subscription_schedule.*` events of `cause`, without the prefix.
fn schedule_events(m: &MockStripe, cause: &Cause) -> Vec<String> {
    events_of(m, cause)
        .into_iter()
        .filter_map(|e| e.strip_prefix("subscription_schedule.").map(str::to_string))
        .collect()
}

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
fn latest(m: &MockStripe, t: ObjectType) -> Vec<Value> {
    let mut ids: Vec<String> = Vec::new();
    for u in m.truth_log() {
        if u.object_type == t && !ids.contains(&u.object_id) {
            ids.push(u.object_id);
        }
    }
    ids.iter().map(|id| m.get(t, id).unwrap().state).collect()
}

fn expect_409(m: &MockStripe, a: Action, needle: &str) {
    match m.run_action(a) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(needle), "{msg}"),
        other => panic!("expected a 409 naming {needle}: {other:?}"),
    }
}

fn schedule(customer: Option<&str>, end: Option<ScheduleEndBehavior>) -> Action {
    Action::ScheduleSubscription {
        customer: customer.map(str::to_string),
        end_behavior: end,
    }
}

/// A catalog whose only active recurring prices are monthly, so every schedule's phases
/// are months.
fn monthly_catalog(m: &MockStripe) {
    recurring_prices(m, 3, "month");
    for p in latest(m, ObjectType::Price) {
        if p["active"] == true && p["recurring"]["interval"] == "year" {
            m.run_action(Action::Archive {
                id: p["id"].as_str().unwrap().to_string(),
            })
            .unwrap();
        }
    }
}

/// `anchor` plus `n` intervals in calendar months, clamped to a shorter month's end.
fn add_intervals(anchor: i64, interval: &str, n: u32) -> i64 {
    let months = if interval == "year" { 12 * n } else { n };
    DateTime::from_timestamp(anchor, 0)
        .unwrap()
        .checked_add_months(Months::new(months))
        .unwrap()
        .timestamp()
}

/// The price IDs of each phase, in order.
fn phase_prices(s: &Value) -> Vec<Vec<String>> {
    s["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            p["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["price"].as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

/// The price IDs a subscription's embedded items bill, in order.
fn item_prices(sub: &Value) -> Vec<String> {
    sub["items"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["price"]["id"].as_str().unwrap().to_string())
        .collect()
}

/// Every invoice of `sub`, in creation order.
fn invoices_of(m: &MockStripe, sub: &str) -> Vec<Value> {
    latest(m, ObjectType::Invoice)
        .into_iter()
        .filter(|i| i["parent"]["subscription_details"]["subscription"] == sub)
        .collect()
}

/// The price IDs an invoice's lines bill and its first line's period start.
fn billed(invoice: &Value) -> (Vec<String>, i64) {
    let lines = invoice["lines"]["data"].as_array().unwrap();
    let prices = lines
        .iter()
        .map(|l| {
            l["pricing"]["price_details"]["price"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    (prices, lines[0]["period"]["start"].as_i64().unwrap())
}

/// Schedules a subscription with `end` and steps a day, so its first invoice is paid.
/// Returns `(cause, subscription, schedule)`.
async fn active_schedule(
    m: &MockStripe,
    customer: Option<&str>,
    end: Option<ScheduleEndBehavior>,
) -> (Cause, String, String) {
    let cause = m.run_action(schedule(customer, end)).unwrap();
    let sub = created_by(m, &cause, ObjectType::Subscription);
    let sched = created_by(m, &cause, ObjectType::SubscriptionSchedule);
    m.step(DAY).await.unwrap();
    assert_eq!(state(m, ObjectType::Subscription, &sub)["status"], "active");
    (cause, sub, sched)
}

fn last_end(s: &Value) -> i64 {
    s["phases"].as_array().unwrap().last().unwrap()["end_date"]
        .as_i64()
        .unwrap()
}

/// Steps until simulated time passes `t` (unix seconds) plus `extra_days`.
async fn step_past(m: &MockStripe, t: i64, extra_days: i64) {
    while m.now().0 / 1000 < t + extra_days * 86_400 {
        m.step(10 * DAY).await.unwrap();
    }
}

#[tokio::test]
async fn release_walks_every_phase() {
    let m = mock_with("seed = 81").await;
    monthly_catalog(&m);
    let (cause, sub, sched) = active_schedule(&m, None, Some(ScheduleEndBehavior::Release)).await;
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "active");
    assert_eq!(s["end_behavior"], "release");
    assert_eq!(s["subscription"], sub.as_str());
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["schedule"],
        sched.as_str()
    );
    let phases = s["phases"].as_array().unwrap().clone();
    assert!((2..=3).contains(&phases.len()), "{s}");
    assert_eq!(s["current_phase"]["start_date"], phases[0]["start_date"]);
    assert_eq!(s["current_phase"]["end_date"], phases[0]["end_date"]);
    let prices = phase_prices(&s);
    // The first phase's items are the subscription's.
    assert_eq!(
        item_prices(&state(&m, ObjectType::Subscription, &sub)),
        prices[0]
    );
    step_past(&m, last_end(&s), 70).await;

    // One update per boundary, then the release.
    let mut expected = vec!["created".to_string()];
    expected.extend((1..phases.len()).map(|_| "updated".to_string()));
    expected.push("released".to_string());
    assert_eq!(schedule_events(&m, &cause), expected);

    // At each boundary the schedule moves to the next phase, and the subscription's items
    // then carry that phase's prices.
    let log = entries(&m, &cause);
    let updates: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            u.object_type == ObjectType::SubscriptionSchedule && u.kind != ChangeKind::Created
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(updates.len(), phases.len());
    for (k, &i) in updates[..phases.len() - 1].iter().enumerate() {
        let u = &log[i];
        let phase = &phases[k + 1];
        assert_eq!(u.state["current_phase"]["start_date"], phase["start_date"]);
        assert!(u.at.0 / 1000 >= phase["start_date"].as_i64().unwrap());
        let next_sub = log[i..]
            .iter()
            .find(|e| e.object_type == ObjectType::Subscription)
            .unwrap();
        assert_eq!(
            item_prices(&next_sub.state),
            prices[k + 1],
            "phase {}",
            k + 1
        );
    }
    // The first phase's items were deleted.
    let deleted = log
        .iter()
        .filter(|u| u.object_type == ObjectType::SubscriptionItem && u.kind == ChangeKind::Deleted)
        .count();
    assert!(deleted >= prices[0].len(), "{deleted}");

    // Each invoice bills the prices of the phase its period starts in; after the release
    // the last phase's prices stay.
    let invoices = invoices_of(&m, &sub);
    assert!(invoices.len() > phases.len(), "{}", invoices.len());
    for inv in &invoices {
        let (billed_prices, start) = billed(inv);
        let k = phases
            .iter()
            .position(|p| {
                start >= p["start_date"].as_i64().unwrap()
                    && start < p["end_date"].as_i64().unwrap()
            })
            .unwrap_or(phases.len() - 1);
        assert_eq!(billed_prices, prices[k], "{}", inv["id"]);
    }

    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "released");
    assert_eq!(s["released_subscription"], sub.as_str());
    assert!(s["subscription"].is_null(), "{s}");
    assert!(s["current_phase"].is_null(), "{s}");
    assert!(s["released_at"].as_i64().unwrap() >= last_end(&s));
    let subscription = state(&m, ObjectType::Subscription, &sub);
    assert_eq!(subscription["status"], "active");
    assert!(subscription["schedule"].is_null(), "{subscription}");
    // The release comes before the subscription drops its schedule.
    let tail = events_of(&m, &cause);
    let at = tail
        .iter()
        .position(|e| e == "subscription_schedule.released")
        .unwrap();
    assert_eq!(tail[at + 1], "customer.subscription.updated");
}

#[tokio::test]
async fn cancel_completes_and_cancels() {
    let m = mock_with("seed = 82").await;
    monthly_catalog(&m);
    let (cause, sub, sched) = active_schedule(&m, None, Some(ScheduleEndBehavior::Cancel)).await;
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    assert_eq!(s["end_behavior"], "cancel");
    let end = last_end(&s);
    step_past(&m, end, 70).await;
    let events = events_of(&m, &cause);
    let deleted = events
        .iter()
        .position(|e| e == "customer.subscription.deleted")
        .unwrap();
    assert_eq!(events[deleted + 1], "subscription_schedule.completed");
    assert!(
        !events[deleted..].iter().any(|e| e == "invoice.created"),
        "{events:?}"
    );
    for inv in invoices_of(&m, &sub) {
        assert!(billed(&inv).1 < end, "{}", inv["id"]);
    }
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "completed");
    assert!(s["completed_at"].as_i64().unwrap() >= end);
    assert!(s["current_phase"].is_null(), "{s}");
    assert_eq!(s["subscription"], sub.as_str());
    let subscription = state(&m, ObjectType::Subscription, &sub);
    assert_eq!(subscription["status"], "canceled");
}

#[tokio::test]
async fn phase_dates_follow_the_interval() {
    // A start on the 31st: periods clamp at shorter months, and the phases follow them.
    let m = mock_with("seed = 83\n[clock]\nstart = \"2026-01-31T00:00:00Z\"").await;
    recurring_prices(&m, 2, "month");
    recurring_prices(&m, 1, "year");
    let mut behaviors = Vec::new();
    let mut monthly = Vec::new();
    for _ in 0..12 {
        let cause = m.run_action(schedule(None, None)).unwrap();
        let sub = created_by(&m, &cause, ObjectType::Subscription);
        let sched = created_by(&m, &cause, ObjectType::SubscriptionSchedule);
        let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
        check_payload(&s).unwrap();
        behaviors.push(s["end_behavior"].as_str().unwrap().to_string());
        let subscription = state(&m, ObjectType::Subscription, &sub);
        assert_eq!(s["customer"], subscription["customer"]);
        let phases = s["phases"].as_array().unwrap();
        assert!((2..=3).contains(&phases.len()), "{s}");
        let anchor = subscription["billing_cycle_anchor"].as_i64().unwrap();
        assert_eq!(phases[0]["start_date"], anchor);
        let first = &subscription["items"]["data"][0]["price"];
        let interval = first["recurring"]["interval"].as_str().unwrap();
        if interval == "month" {
            monthly.push((cause.clone(), sched.clone()));
        }
        // Each phase ends `n` intervals after the anchor, `n` growing by 1-3 per phase, and
        // the next phase starts there.
        let mut elapsed = 0;
        for (k, p) in phases.iter().enumerate() {
            if k > 0 {
                assert_eq!(p["start_date"], phases[k - 1]["end_date"]);
            }
            let end = p["end_date"].as_i64().unwrap();
            let step = (1..=3)
                .find(|n| add_intervals(anchor, interval, elapsed + n) == end)
                .unwrap_or_else(|| {
                    panic!("phase {k} of {sched} ends at {end}, not 1-3 {interval}s on")
                });
            elapsed += step;
            assert_eq!(p["currency"], first["currency"]);
            let items = p["items"].as_array().unwrap();
            assert!((1..=2).contains(&items.len()), "{p}");
            for i in items {
                let price = state(&m, ObjectType::Price, i["price"].as_str().unwrap());
                assert_eq!(price["currency"], first["currency"]);
                assert_eq!(price["recurring"]["interval"], interval);
                // Capped like a subscription's: 1-5, or less (down to 0) to keep a
                // period's charge within the maximum.
                assert!((0..=5).contains(&i["quantity"].as_i64().unwrap()), "{i}");
            }
        }
    }
    assert!(behaviors.iter().any(|b| b == "release"), "{behaviors:?}");
    assert!(behaviors.iter().any(|b| b == "cancel"), "{behaviors:?}");
    assert!(!monthly.is_empty());

    // Each boundary is the `current_period_end` of the renewal that swaps the phase: the
    // schedule steps at the boundary, and the new items' period starts there.
    let latest_end = monthly
        .iter()
        .map(|(_, sched)| last_end(&state(&m, ObjectType::SubscriptionSchedule, sched)))
        .max()
        .unwrap();
    step_past(&m, latest_end, 5).await;
    let mut swaps = 0;
    for (cause, sched) in &monthly {
        let s = state(&m, ObjectType::SubscriptionSchedule, sched);
        let phases = s["phases"].as_array().unwrap();
        let log = entries(&m, cause);
        for (i, u) in log.iter().enumerate() {
            if u.object_type != ObjectType::SubscriptionSchedule || u.kind == ChangeKind::Created {
                continue;
            }
            let k = if u.kind == ChangeKind::Updated {
                let start = &u.state["current_phase"]["start_date"];
                phases
                    .iter()
                    .position(|p| &p["start_date"] == start)
                    .unwrap()
            } else {
                phases.len()
            };
            let boundary = phases[k - 1]["end_date"].as_i64().unwrap();
            assert_eq!(u.at.0 / 1000, boundary, "{sched} phase {k}");
            if k < phases.len() {
                let next_sub = log[i..]
                    .iter()
                    .find(|e| e.object_type == ObjectType::Subscription)
                    .unwrap();
                for item in next_sub.state["items"]["data"].as_array().unwrap() {
                    assert_eq!(item["current_period_start"], boundary, "{sched}");
                }
                swaps += 1;
            }
        }
    }
    assert!(swaps > 0);
}

/// Archives every active recurring price that is yearly or bills nothing, after making sure
/// a paying monthly one exists, so a schedule's invoices are charged.
fn paying_monthly_catalog(m: &MockStripe) {
    for _ in 0..400 {
        let paying = latest(m, ObjectType::Price).into_iter().any(|p| {
            p["active"] == true
                && p["recurring"]["interval"] == "month"
                && p["unit_amount"].as_i64().is_some_and(|a| a > 0)
        });
        if paying {
            break;
        }
        m.run_action(Action::CatalogChange).unwrap();
    }
    for p in latest(m, ObjectType::Price) {
        let recurring = p["recurring"].is_object();
        let unpaid = p["unit_amount"].as_i64().is_none_or(|a| a == 0);
        if p["active"] == true && recurring && (p["recurring"]["interval"] == "year" || unpaid) {
            m.run_action(Action::Archive {
                id: p["id"].as_str().unwrap().to_string(),
            })
            .unwrap();
        }
    }
}

#[tokio::test]
async fn incomplete_expiry_cancels_the_schedule() {
    let m = mock_with("seed = 90\n[actions]\npayment_failure_probability = 1.0").await;
    paying_monthly_catalog(&m);
    let cause = m.run_action(schedule(None, None)).unwrap();
    let sub = created_by(&m, &cause, ObjectType::Subscription);
    let sched = created_by(&m, &cause, ObjectType::SubscriptionSchedule);
    m.step(DAY / 2).await.unwrap();
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["status"],
        "incomplete"
    );
    m.step(DAY).await.unwrap();
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["status"],
        "incomplete_expired"
    );
    let events = events_of(&m, &cause);
    let deleted = events
        .iter()
        .position(|e| e == "customer.subscription.deleted")
        .unwrap();
    assert_eq!(events[deleted + 1], "subscription_schedule.canceled");
    assert_eq!(schedule_events(&m, &cause), ["created", "canceled"]);
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "canceled");
    assert!(s["canceled_at"].is_i64(), "{s}");
    assert!(s["current_phase"].is_null(), "{s}");
}

/// A renewal while `past_due`: with on-time continuations the 3-day dunning retry settles
/// before the next monthly renewal, but after generation was paused the overdue renewal runs
/// first (queue order), while the subscription is `past_due`. The phase check still runs.
#[tokio::test]
async fn past_due_renewal_still_advances_the_phase() {
    let m = mock_with("seed = 91").await;
    paying_monthly_catalog(&m);
    let (cause, sub, sched) = active_schedule(&m, None, Some(ScheduleEndBehavior::Release)).await;
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    let phases = s["phases"].as_array().unwrap().clone();
    // The second phase ends at least two periods in, so a renewal opens the period before it.
    let boundary = phases[1]["end_date"].as_i64().unwrap();
    let anchor = phases[0]["start_date"].as_i64().unwrap();
    let n = (1..=6)
        .find(|n| add_intervals(anchor, "month", *n) == boundary)
        .unwrap();
    assert!(n >= 2);
    let before = add_intervals(anchor, "month", n - 1);
    m.advance_to(SimTime((before - 3_600) * 1000))
        .await
        .unwrap();
    m.update_actions_config(|a| a.payment_failure_probability = 1.0)
        .unwrap();
    m.step(DAY).await.unwrap();
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["status"],
        "past_due"
    );
    // Pause past the boundary, then let the overdue steps run.
    m.quiesce().await.unwrap();
    m.advance_to(SimTime((boundary + 86_400) * 1000))
        .await
        .unwrap();
    m.update_actions_config(|a| a.payment_failure_probability = 0.0)
        .unwrap();
    m.resume_generation();
    m.step(DAY).await.unwrap();
    let log = entries(&m, &cause);
    let i = log
        .iter()
        .position(|u| {
            u.object_type == ObjectType::SubscriptionSchedule
                && u.kind != ChangeKind::Created
                && u.at.0 / 1000 > before
        })
        .expect("the schedule stepped at the boundary");
    let sub_then = log[..i]
        .iter()
        .rev()
        .find(|u| u.object_type == ObjectType::Subscription)
        .unwrap();
    assert_eq!(sub_then.state["status"], "past_due");
    if phases.len() == 2 {
        assert_eq!(log[i].state["status"], "released");
    } else {
        assert_eq!(log[i].state["status"], "active");
        assert_eq!(log[i].state["current_phase"]["start_date"], boundary);
        let next_sub = log[i..]
            .iter()
            .find(|e| e.object_type == ObjectType::Subscription)
            .unwrap();
        assert_eq!(item_prices(&next_sub.state), phase_prices(&s)[2]);
    }
}

#[tokio::test]
async fn early_cancel_cancels_the_schedule() {
    let m = mock_with("seed = 84").await;
    monthly_catalog(&m);
    let (_, sub, sched) = active_schedule(&m, None, None).await;
    let cause = m
        .run_action(Action::CancelSubscription {
            id: Some(sub.clone()),
            at_period_end: Some(true),
        })
        .unwrap();
    assert_eq!(
        events_of(&m, &cause),
        [
            "customer.subscription.deleted",
            "subscription_schedule.canceled"
        ]
    );
    let subscription = state(&m, ObjectType::Subscription, &sub);
    assert_eq!(subscription["status"], "canceled");
    assert_eq!(subscription["cancel_at_period_end"], false);
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "canceled");
    assert_eq!(s["canceled_at"], subscription["canceled_at"]);
    assert!(s["current_phase"].is_null(), "{s}");
    // An ended subscription never advances a phase.
    let version = m
        .get(ObjectType::SubscriptionSchedule, &sched)
        .unwrap()
        .version;
    step_past(&m, last_end(&s), 40).await;
    assert_eq!(
        m.get(ObjectType::SubscriptionSchedule, &sched)
            .unwrap()
            .version,
        version
    );
    expect_409(
        &m,
        Action::CancelSubscriptionSchedule {
            id: Some(sched.clone()),
        },
        &sched,
    );
}

#[tokio::test]
async fn failed_dunning_aborts() {
    let m = mock_with("seed = 85").await;
    monthly_catalog(&m);
    let (cause, sub, sched) = active_schedule(&m, None, None).await;
    m.update_actions_config(|a| a.payment_failure_probability = 1.0)
        .unwrap();
    for _ in 0..60 {
        m.step(DAY).await.unwrap();
        if state(&m, ObjectType::Subscription, &sub)["status"] == "canceled" {
            break;
        }
    }
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["status"],
        "canceled"
    );
    let events = events_of(&m, &cause);
    let deleted = events
        .iter()
        .position(|e| e == "customer.subscription.deleted")
        .unwrap();
    assert_eq!(events[deleted + 1], "subscription_schedule.aborted");
    assert_eq!(schedule_events(&m, &cause).last().unwrap(), "aborted");
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "canceled");
    assert!(s["canceled_at"].is_i64(), "{s}");
}

#[tokio::test]
async fn customer_deletion_cancels_schedules() {
    let m = mock_with("seed = 86").await;
    monthly_catalog(&m);
    let c = m.run_action(Action::NewCustomer).unwrap();
    let customer = created_by(&m, &c, ObjectType::Customer);
    let (_, sub1, sched1) = active_schedule(&m, Some(&customer), None).await;
    // With generation off, the second's first invoice never runs, so it is still
    // incomplete: it expires.
    m.quiesce().await.unwrap();
    let cause = m.run_action(schedule(Some(&customer), None)).unwrap();
    let sub2 = created_by(&m, &cause, ObjectType::Subscription);
    let sched2 = created_by(&m, &cause, ObjectType::SubscriptionSchedule);
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub2)["status"],
        "incomplete"
    );
    let listed = get(
        &m,
        &format!("/v1/subscription_schedules?customer={customer}"),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), 2, "{listed}");
    m.delete_customer(&customer).unwrap();
    let deletion = m
        .truth_log()
        .into_iter()
        .rev()
        .find(|u| u.object_type == ObjectType::Customer && u.kind == ChangeKind::Deleted)
        .unwrap()
        .cause
        .unwrap();
    let events = events_of(&m, &deletion);
    let sub_events: Vec<&String> = events
        .iter()
        .filter(|e| {
            e.starts_with("customer.subscription.") || e.starts_with("subscription_schedule.")
        })
        .collect();
    assert_eq!(
        sub_events,
        [
            "customer.subscription.deleted",
            "subscription_schedule.canceled",
            "customer.subscription.deleted",
            "subscription_schedule.canceled",
        ]
    );
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub1)["status"],
        "canceled"
    );
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub2)["status"],
        "incomplete_expired"
    );
    for sched in [&sched1, &sched2] {
        let s = state(&m, ObjectType::SubscriptionSchedule, sched);
        check_payload(&s).unwrap();
        assert_eq!(s["status"], "canceled", "{s}");
    }
    // The schedule's customer expands to the deleted customer.
    let body = get(
        &m,
        &format!("/v1/subscription_schedules/{sched1}?expand[]=customer"),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(body["customer"]["deleted"], true, "{body}");
    // A deleted customer cannot be scheduled.
    expect_409(&m, schedule(Some(&customer), None), &customer);
    expect_409(&m, schedule(Some("cus_missing"), None), "cus_missing");
}

#[tokio::test]
async fn cancel_schedule_action() {
    let m = mock_with("seed = 87").await;
    monthly_catalog(&m);
    let (_, sub, sched) = active_schedule(&m, None, None).await;
    let cause = m
        .run_action(Action::CancelSubscriptionSchedule {
            id: Some(sched.clone()),
        })
        .unwrap();
    assert_eq!(
        events_of(&m, &cause),
        [
            "subscription_schedule.canceled",
            "customer.subscription.deleted"
        ]
    );
    let s = state(&m, ObjectType::SubscriptionSchedule, &sched);
    check_payload(&s).unwrap();
    assert_eq!(s["status"], "canceled");
    assert!(s["canceled_at"].is_i64(), "{s}");
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["status"],
        "canceled"
    );
    expect_409(
        &m,
        Action::CancelSubscriptionSchedule {
            id: Some(sched.clone()),
        },
        &sched,
    );
    expect_409(
        &m,
        Action::CancelSubscriptionSchedule {
            id: Some("sub_sched_missing".into()),
        },
        "sub_sched_missing",
    );
    expect_409(
        &m,
        Action::CancelSubscriptionSchedule { id: None },
        "subscription schedule",
    );
    // With one active, `None` cancels it.
    let (_, _, other) = active_schedule(&m, None, None).await;
    let cause = m
        .run_action(Action::CancelSubscriptionSchedule { id: None })
        .unwrap();
    assert_eq!(
        created_or_updated(&m, &cause, ObjectType::SubscriptionSchedule),
        [other]
    );
}

/// The IDs of `t` that the entries of `cause` touch, in first-touched order.
fn created_or_updated(m: &MockStripe, cause: &Cause, t: ObjectType) -> Vec<String> {
    let mut ids = Vec::new();
    for u in entries(m, cause) {
        if u.object_type == t && !ids.contains(&u.object_id) {
            ids.push(u.object_id);
        }
    }
    ids
}

#[tokio::test]
async fn renewals_off_freezes_phases() {
    let m = mock_with("seed = 88").await;
    monthly_catalog(&m);
    let (_, sub, sched) = active_schedule(&m, None, None).await;
    m.update_actions_config(|a| a.renew_subscription_cycle = RenewMode::Off)
        .unwrap();
    let before = m.get(ObjectType::SubscriptionSchedule, &sched).unwrap();
    let items = item_prices(&state(&m, ObjectType::Subscription, &sub));
    step_past(&m, last_end(&before.state), 40).await;
    let after = m.get(ObjectType::SubscriptionSchedule, &sched).unwrap();
    assert_eq!(after.version, before.version);
    assert_eq!(after.state["status"], "active");
    assert_eq!(after.state["current_phase"], before.state["current_phase"]);
    let subscription = state(&m, ObjectType::Subscription, &sub);
    assert_eq!(subscription["status"], "active");
    assert_eq!(item_prices(&subscription), items);
}

#[tokio::test]
async fn expand_subscription_schedule() {
    let m = mock_with("seed = 89").await;
    monthly_catalog(&m);
    let (_, sub, sched) = active_schedule(&m, None, None).await;
    let body = get(&m, &format!("/v1/subscriptions/{sub}?expand[]=schedule"))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        body["schedule"]["object"], "subscription_schedule",
        "{body}"
    );
    assert_eq!(body["schedule"]["id"], sched.as_str());
    assert_eq!(body["schedule"]["status"], "active");
    let body = get(
        &m,
        &format!("/v1/subscription_schedules/{sched}?expand[]=subscription&expand[]=customer"),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(body["subscription"]["id"], sub.as_str(), "{body}");
    assert_eq!(body["customer"]["object"], "customer", "{body}");
}
