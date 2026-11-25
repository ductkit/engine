mod common;

use std::time::Duration;

use common::{Receiver, dest, get_as, get_ctx, mock_with};
use ductkit_stripe_mock::{
    AccountApi, Action, ChangeKind, CustomerUpdate, DeliveryRecordKind, EventFormat, EventInfo,
    MockError, MockStripe, NewCustomer, ObjectType, SimTime,
};
use serde_json::Value;

const CONNECT: &str = "[accounts]\nconnect = true\n";

fn events_of(m: &MockStripe, id: &str) -> Vec<EventInfo> {
    m.event_timeline()
        .into_iter()
        .filter(|e| e.object_id == id)
        .collect()
}

fn types(events: &[EventInfo], format: EventFormat) -> Vec<&str> {
    events
        .iter()
        .filter(|e| e.format == format)
        .map(|e| e.event_type.as_str())
        .collect()
}

async fn payload(r: reqwest::Response) -> Value {
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test]
async fn connect_v1_emits_authorized() {
    let m = mock_with(CONNECT).await;
    let id = m.connect_account(AccountApi::V1).unwrap();
    assert!(id.starts_with("acct_"), "{id}");
    let evs = events_of(&m, &id);
    assert_eq!(
        types(&evs, EventFormat::Snapshot),
        ["account.application.authorized"]
    );
    assert_eq!(
        types(&evs, EventFormat::Thin),
        ["v1.account.application.authorized"]
    );
    assert!(evs.iter().all(|e| e.account.as_deref() == Some(&*id)));
    let snap = evs
        .iter()
        .find(|e| e.format == EventFormat::Snapshot)
        .unwrap();
    let body = payload(get_as(&m, &format!("/v1/events/{}", snap.id), &id).await).await;
    assert_eq!(body["data"]["object"]["object"], "application");
    assert_eq!(body["account"], id.as_str());
    let thin = evs.iter().find(|e| e.format == EventFormat::Thin).unwrap();
    let body = payload(get_ctx(&m, &format!("/v2/core/events/{}", thin.id), &id).await).await;
    assert_eq!(body["related_object"]["id"], id.as_str());
    assert_eq!(body["related_object"]["type"], "account");
    assert_eq!(body["context"], id.as_str());
    let created: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Account)
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].kind, ChangeKind::Created);
    assert_eq!(created[0].account, None);
}

#[tokio::test]
async fn connect_v2_also_emits_thin_created() {
    let m = mock_with(CONNECT).await;
    let id = m.connect_account(AccountApi::V2).unwrap();
    let evs = events_of(&m, &id);
    assert_eq!(
        types(&evs, EventFormat::Snapshot),
        ["account.application.authorized"]
    );
    assert_eq!(
        types(&evs, EventFormat::Thin),
        [
            "v1.account.application.authorized",
            "v2.core.account.created"
        ]
    );
    let created = evs
        .iter()
        .find(|e| e.event_type == "v2.core.account.created")
        .unwrap();
    assert_eq!(created.account, None);
    assert_eq!(created.pair_id, "");
}

#[tokio::test]
async fn disconnect_emits_deauthorized_and_closed() {
    let m = mock_with(CONNECT).await;
    let v1 = m.connect_account(AccountApi::V1).unwrap();
    let v2 = m.connect_account(AccountApi::V2).unwrap();
    m.disconnect_account(&v1).unwrap();
    m.disconnect_account(&v2).unwrap();
    let e1 = events_of(&m, &v1);
    assert_eq!(
        types(&e1, EventFormat::Snapshot),
        [
            "account.application.authorized",
            "account.application.deauthorized"
        ]
    );
    assert!(!types(&e1, EventFormat::Thin).contains(&"v2.core.account.closed"));
    let e2 = events_of(&m, &v2);
    assert_eq!(
        types(&e2, EventFormat::Snapshot),
        [
            "account.application.authorized",
            "account.application.deauthorized"
        ]
    );
    let closed = e2
        .iter()
        .find(|e| e.event_type == "v2.core.account.closed")
        .expect("closed event");
    assert_eq!(closed.account, None);
    let deauth = e2
        .iter()
        .find(|e| e.event_type == "account.application.deauthorized")
        .unwrap();
    assert_eq!(deauth.account.as_deref(), Some(&*v2));
    // The account is disconnected, so its events are no longer reachable.
    let r = get_as(&m, &format!("/v1/events/{}", deauth.id), &v2).await;
    assert_eq!(r.status(), 403);
    for a in m.accounts() {
        assert!(!a.connected, "{a:?}");
    }
    let last = m.get(ObjectType::Account, &v1).unwrap();
    assert_eq!(last.kind, ChangeKind::Deleted);
    assert_eq!(last.state["deleted"], true);
    assert!(matches!(
        m.disconnect_account(&v1),
        Err(MockError::UnknownAccount(id)) if id == v1
    ));
    assert!(matches!(
        m.disconnect_account("acct_nope"),
        Err(MockError::UnknownAccount(_))
    ));
}

#[tokio::test]
async fn connect_disabled_by_default() {
    let m = mock_with("").await;
    assert!(matches!(
        m.connect_account(AccountApi::V1),
        Err(MockError::ConnectDisabled)
    ));
    assert!(matches!(
        m.disconnect_account("acct_x"),
        Err(MockError::ConnectDisabled)
    ));
    assert!(m.accounts().is_empty());
}

#[tokio::test]
async fn initial_accounts_at_history_start() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "{CONNECT}connected_accounts = {{ initial = 3 }}\n[history]\nduration = \"1h\"\n{}",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    let accounts = m.accounts();
    assert_eq!(accounts.len(), 3);
    assert!(accounts.iter().all(|a| a.connected));
    let start = m.now().unix_seconds() - 3600;
    for a in &accounts {
        let u = m.get(ObjectType::Account, &a.id).unwrap();
        assert_eq!(u.state["created"], start);
        assert_eq!(u.at, SimTime(start * 1000));
    }
    m.quiesce().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.requests().is_empty(), "history was delivered");
}

#[tokio::test]
async fn account_rates_follow_config() {
    let m = mock_with(
        "[accounts]\nconnect = true\nconnected_accounts = { connect_rate = 10.0, deauthorize_rate = 2.0 }\n[churn.account]\nupdate = 5.0\n",
    )
    .await;
    m.step(Duration::from_secs(10 * 3600)).await.unwrap();
    let log = m.truth_log();
    let count = |kind: ChangeKind| {
        log.iter()
            .filter(|u| u.object_type == ObjectType::Account && u.kind == kind)
            .count()
    };
    let connects = count(ChangeKind::Created);
    assert!((60..=140).contains(&connects), "{connects}");
    assert!(count(ChangeKind::Deleted) >= 1);
    let updated = m
        .event_timeline()
        .iter()
        .filter(|e| e.event_type == "account.updated")
        .count();
    assert!(updated >= 20, "{updated}");
    assert_eq!(
        m.accounts().iter().filter(|a| a.connected).count(),
        connects - count(ChangeKind::Deleted)
    );
}

#[tokio::test]
async fn same_seed_same_accounts() {
    let cfg = "seed = 9\n[accounts]\nconnect = true\nconnected_accounts = { initial = 2, connect_rate = 3.0, deauthorize_rate = 1.0 }\n[churn.account]\nupdate = 2.0\n";
    let run = || async {
        let m = mock_with(cfg).await;
        m.step(Duration::from_secs(5 * 3600)).await.unwrap();
        (m.accounts(), m.truth_log())
    };
    let (a1, t1) = run().await;
    let (a2, t2) = run().await;
    assert!(a1.len() > 2);
    assert_eq!(a1, a2);
    assert_eq!(t1, t2);
}

#[tokio::test]
async fn v2_accounts_have_configurations() {
    let m = mock_with(CONNECT).await;
    for _ in 0..20 {
        m.connect_account(AccountApi::V2).unwrap();
    }
    for a in m.accounts() {
        assert!(!a.configurations.is_empty(), "{a:?}");
    }
}

fn customers_in(m: &MockStripe, scope: Option<&str>) -> usize {
    m.truth_log()
        .iter()
        .filter(|u| {
            u.object_type == ObjectType::Customer
                && u.kind == ChangeKind::Created
                && u.account.as_deref() == scope
        })
        .count()
}

#[tokio::test]
async fn connected_accounts_get_platform_rates() {
    let m = mock_with(
        "[actions]\nnew_customer = 20\n[accounts]\nconnect = true\nconnected_accounts = { initial = 2 }\n",
    )
    .await;
    m.step(Duration::from_secs(5 * 3600)).await.unwrap();
    let accounts = m.accounts();
    assert_eq!(accounts.len(), 2);
    let platform = customers_in(&m, None);
    assert!((60..=140).contains(&platform), "platform {platform}");
    for a in &accounts {
        let n = customers_in(&m, Some(&a.id));
        assert!((60..=140).contains(&n), "{} {n}", a.id);
    }
}

const TENTH: &str = "[actions]
new_customer = 1.0
start_subscription = 0.4
cancel_subscription = 0.05
catalog_change = 0.02
one_off_invoice = 0.2
[churn.customer]
update = 0.3
delete = 0.02
[churn.product]
update = 0.05
archive = 0.01
[churn.price]
update = 0.05
archive = 0.01
[churn.subscription]
update = 0.05
[churn.invoice]
update = 0.05
[accounts]
connect = true
connected_accounts = { initial = 1 }
";

#[tokio::test]
async fn connected_account_event_types_match_platform() {
    let m = mock_with(TENTH).await;
    m.step(Duration::from_secs(10 * 24 * 3600)).await.unwrap();
    let acct = m.accounts()[0].id.clone();
    let set_of = |scope: Option<&str>| -> std::collections::BTreeSet<String> {
        m.event_timeline()
            .into_iter()
            .filter(|e| e.account.as_deref() == scope)
            .map(|e| e.event_type)
            .filter(|t| !t.starts_with("account.") && !t.starts_with("v1.account."))
            .collect()
    };
    let (platform, account) = (set_of(None), set_of(Some(&acct)));
    assert!(platform.len() > 8, "{platform:?}");
    assert_eq!(platform, account);
}

#[tokio::test]
async fn disconnect_stops_continuations() {
    let m = mock_with(
        "[actions]\nstep_delay = { distribution = \"fixed\", ms = 60000 }\n[accounts]\nconnect = true\n",
    )
    .await;
    let id = m.connect_account(AccountApi::V1).unwrap();
    m.account(&id)
        .run_action(Action::StartSubscription {
            customer: None,
            prices: None,
        })
        .unwrap();
    // Continuations are pending: the first invoice has not been created yet.
    m.step(Duration::from_secs(30)).await.unwrap();
    let before = m.truth_log().len();
    m.disconnect_account(&id).unwrap();
    let at = m.now();
    m.step(Duration::from_secs(40 * 86400)).await.unwrap();
    let late: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.account.as_deref() == Some(&*id) && u.at > at)
        .collect();
    assert!(late.is_empty(), "{late:?}");
    assert!(
        m.truth_log().len() > before,
        "the disconnect itself is logged"
    );
    assert!(matches!(
        m.account(&id).create_customer(NewCustomer::default()),
        Err(MockError::UnknownAccount(_))
    ));
}

#[tokio::test]
async fn control_connect_and_disconnect_manage_generators() {
    let m = mock_with("[actions]\nnew_customer = 20\n[accounts]\nconnect = true\n").await;
    let id = m.connect_account(AccountApi::V2).unwrap();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let n = customers_in(&m, Some(&id));
    assert!(n > 5, "{n}");
    m.disconnect_account(&id).unwrap();
    m.step(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(customers_in(&m, Some(&id)), n);
}

#[tokio::test]
async fn quiesce_covers_accounts() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    // A `connect = false` destination still receives every event until Task 5.
    let m = mock_with(&format!(
        "{}[actions]\nnew_customer = 20\n[accounts]\nconnect = true\nconnected_accounts = {{ initial = 2 }}\n",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    let t0 = m.now();
    m.step(Duration::from_secs(3600)).await.unwrap();
    m.quiesce().await.unwrap();
    let log = m.delivery_log();
    let scheduled = |id: &str| {
        log.iter()
            .any(|r| r.event_id == id && matches!(r.kind, DeliveryRecordKind::Scheduled { .. }))
    };
    let events: Vec<_> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.account.is_some() && e.format == EventFormat::Snapshot && e.created > t0)
        .collect();
    assert!(events.len() > 20, "{}", events.len());
    for e in &events {
        assert!(scheduled(&e.id), "{e:?}");
    }
    let n = m.truth_log().len();
    m.step(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(m.truth_log().len(), n);
    m.resume_generation();
    m.step(Duration::from_secs(2 * 3600)).await.unwrap();
    let log = m.truth_log();
    assert!(log.len() > n);
    for a in m.accounts() {
        assert!(
            log[n..]
                .iter()
                .any(|u| u.account.as_deref() == Some(&*a.id)),
            "{}",
            a.id
        );
    }
    assert!(log[n..].iter().any(|u| u.account.is_none()));
    assert!(log.windows(2).all(|w| w[0].at <= w[1].at));
}

#[tokio::test]
async fn account_handle_scopes_writes() {
    let m = mock_with(CONNECT).await;
    let id = m.connect_account(AccountApi::V1).unwrap();
    let h = m.account(&id);
    let cus = h.create_customer(NewCustomer::default()).unwrap();
    let got = h.get(ObjectType::Customer, &cus).unwrap().unwrap();
    assert_eq!(got.account.as_deref(), Some(&*id));
    assert!(m.get(ObjectType::Customer, &cus).is_none());
    h.update_customer(&cus, CustomerUpdate::SetName(Some("N".into())))
        .unwrap();
    assert_eq!(
        h.get(ObjectType::Customer, &cus).unwrap().unwrap().version,
        2
    );
    h.delete_customer(&cus).unwrap();
    assert!(matches!(
        m.account("acct_nope")
            .create_customer(NewCustomer::default()),
        Err(MockError::UnknownAccount(_))
    ));
}
