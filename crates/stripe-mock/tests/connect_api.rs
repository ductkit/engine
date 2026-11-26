mod common;

use std::time::Duration;

use common::{get, get_as, get_ctx, get_with, mock_with};
use ductkit_stripe_mock::{
    AccountApi, AccountKind, Action, Configuration, EventFormat, MockStripe, NewCustomer,
};
use serde_json::Value;

const CONNECT: &str = "[accounts]\nconnect = true\n";
const NOT_ENABLED: &str = "Connected accounts are not enabled in this mock.";
const PAGINATION_WRITES: &str =
    "seed = 5\n[accounts]\nconnect = true\n[api]\npagination_writes = { probability = 1.0 }";

async fn json_of(r: reqwest::Response) -> (u16, Value) {
    let s = r.status().as_u16();
    (s, r.json().await.unwrap())
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

fn customer(m: &MockStripe, acct: &str) -> String {
    m.account(acct)
        .create_customer(NewCustomer::default())
        .unwrap()
}

#[tokio::test]
async fn stripe_account_scopes_customers() {
    let m = mock_with(CONNECT).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let c = customer(&m, &a);
    let (s, v) = json_of(get_as(&m, "/v1/customers", &a).await).await;
    assert_eq!((s, ids(&v)), (200, vec![c.clone()]));
    let (s, v) = json_of(get(&m, "/v1/customers").await).await;
    assert_eq!((s, ids(&v)), (200, vec![]));
    let (s, v) = json_of(get(&m, &format!("/v1/customers/{c}")).await).await;
    assert_eq!(s, 404);
    assert_eq!(v["error"]["code"], "resource_missing");
    let (s, v) = json_of(get_as(&m, &format!("/v1/customers/{c}"), &a).await).await;
    assert_eq!((s, v["id"].as_str()), (200, Some(c.as_str())));
}

#[tokio::test]
async fn scopes_do_not_leak() {
    let m = mock_with(CONNECT).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let b = m.connect_account(AccountApi::V1).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    let (ca, cb) = (customer(&m, &a), customer(&m, &b));
    for (acct, cust) in [(&a, &ca), (&b, &cb)] {
        let acct = m.account(acct);
        for _ in 0..3 {
            acct.run_action(Action::CatalogChange).unwrap();
        }
        acct.run_action(Action::OneOffInvoice {
            customer: Some(cust.clone()),
            outcome: None,
        })
        .unwrap();
    }
    let scopes: [Option<&str>; 3] = [None, Some(&a), Some(&b)];
    let mut first_ids = Vec::new();
    for scope in scopes {
        let headers: Vec<(&str, &str)> = scope.map(|s| ("Stripe-Account", s)).into_iter().collect();
        let (s, v1) = json_of(get_with(&m, "/v1/events?limit=100", &headers).await).await;
        assert_eq!(s, 200);
        assert!(!ids(&v1).is_empty(), "{scope:?}");
        for e in v1["data"].as_array().unwrap() {
            assert_eq!(e["account"], scope.map_or(Value::Null, Value::from), "{e}");
        }
        first_ids.push(ids(&v1)[0].clone());
        let headers: Vec<(&str, &str)> = scope.map(|s| ("Stripe-Context", s)).into_iter().collect();
        let (s, v2) = json_of(get_with(&m, "/v2/core/events?limit=100", &headers).await).await;
        assert_eq!(s, 200);
        assert!(!ids(&v2).is_empty(), "{scope:?}");
        for e in v2["data"].as_array().unwrap() {
            assert_eq!(e["context"], scope.map_or(Value::Null, Value::from), "{e}");
        }
    }
    // Another scope's event is not found, in either API.
    let (s, _) = json_of(get_as(&m, &format!("/v1/events/{}", first_ids[2]), &a).await).await;
    assert_eq!(s, 404);
    let (s, _) = json_of(get(&m, &format!("/v1/events/{}", first_ids[1])).await).await;
    assert_eq!(s, 404);
    let (s, v) = json_of(get_as(&m, &format!("/v1/events/{}", first_ids[1]), &a).await).await;
    assert_eq!((s, v["id"].as_str()), (200, Some(first_ids[1].as_str())));
    let (_, thin_b) = json_of(get_ctx(&m, "/v2/core/events?limit=1", &b).await).await;
    let thin_id = ids(&thin_b)[0].clone();
    let (s, _) = json_of(get_ctx(&m, &format!("/v2/core/events/{thin_id}"), &a).await).await;
    assert_eq!(s, 404);
    let (s, _) = json_of(get_ctx(&m, &format!("/v2/core/events/{thin_id}"), &b).await).await;
    assert_eq!(s, 200);
    // Expansion stays inside the scope.
    let (s, v) = json_of(get_as(&m, "/v1/invoices?expand[]=data.customer", &a).await).await;
    assert_eq!(s, 200);
    assert!(!ids(&v).is_empty());
    for inv in v["data"].as_array().unwrap() {
        assert_eq!(inv["customer"]["id"], ca.as_str(), "{inv}");
    }
    let (_, v) = json_of(get(&m, "/v1/invoices").await).await;
    assert!(ids(&v).is_empty());
}

#[tokio::test]
async fn unknown_account_is_403() {
    let m = mock_with(CONNECT).await;
    let r = get_as(&m, "/v1/customers", "acct_nope").await;
    assert!(r.headers().contains_key("request-id"));
    let (s, v) = json_of(r).await;
    assert_eq!(s, 403);
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "account_invalid");
    assert_eq!(
        v["error"]["message"],
        "The provided key does not have access to account 'acct_nope' (or that account does not exist). Application access may have been revoked."
    );
    let (s, v) = json_of(get_ctx(&m, "/v2/core/events", "acct_nope").await).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("account_invalid"))
    );
}

#[tokio::test]
async fn disconnect_mid_backfill_is_403() {
    let m = mock_with(CONNECT).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    for _ in 0..30 {
        customer(&m, &a);
    }
    let (s, v) = json_of(get_as(&m, "/v1/customers?limit=10", &a).await).await;
    assert_eq!((s, v["has_more"].clone()), (200, Value::from(true)));
    let last = ids(&v).pop().unwrap();
    m.disconnect_account(&a).unwrap();
    for _ in 0..2 {
        let path = format!("/v1/customers?limit=10&starting_after={last}");
        let (s, v) = json_of(get_as(&m, &path, &a).await).await;
        assert_eq!(s, 403);
        assert_eq!(v["error"]["code"], "account_invalid");
    }
}

#[tokio::test]
async fn header_rules() {
    let m = mock_with(CONNECT).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let b = m.connect_account(AccountApi::V1).unwrap();
    // Different values.
    let r = get_with(
        &m,
        "/v1/customers",
        &[("Stripe-Account", &a), ("Stripe-Context", &b)],
    )
    .await;
    let (s, v) = json_of(r).await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(
        v["error"]["message"],
        "Stripe-Account and Stripe-Context name different accounts."
    );
    // The same value.
    let r = get_with(
        &m,
        "/v1/customers",
        &[("Stripe-Account", &a), ("Stripe-Context", &a)],
    )
    .await;
    assert_eq!(r.status(), 200);
    // Stripe-Account on a v2 endpoint.
    let (s, v) = json_of(get_as(&m, "/v2/core/events", &a).await).await;
    assert_eq!(s, 400);
    assert_eq!(
        v["error"]["message"],
        "Use the Stripe-Context header with v2 endpoints."
    );
    // Stripe-Context scopes a v1 endpoint like Stripe-Account.
    let c = customer(&m, &a);
    let (s, v) = json_of(get_ctx(&m, "/v1/customers", &a).await).await;
    assert_eq!((s, ids(&v)), (200, vec![c]));
    // Accounts endpoints belong to the platform.
    let one = format!("/v1/accounts/{a}");
    for (path, header) in [
        ("/v1/accounts", "Stripe-Account"),
        ("/v1/accounts", "Stripe-Context"),
        ("/v2/core/accounts", "Stripe-Context"),
        (one.as_str(), "Stripe-Account"),
    ] {
        let (s, v) = json_of(get_with(&m, path, &[(header, &a)]).await).await;
        assert_eq!(s, 400, "{path} {header}");
        assert_eq!(
            v["error"]["message"],
            "Accounts endpoints must be called by the platform."
        );
    }
    // With connect off, any header is today's error.
    let off = mock_with("").await;
    for header in ["Stripe-Account", "Stripe-Context"] {
        let (s, v) = json_of(get_with(&off, "/v1/customers", &[(header, "acct_x")]).await).await;
        assert_eq!(s, 400);
        assert_eq!(v["error"]["message"], NOT_ENABLED);
    }
}

#[tokio::test]
async fn v1_accounts_list_both_kinds() {
    let m = mock_with(CONNECT).await;
    let v1 = m.connect_account(AccountApi::V1).unwrap();
    let v2 = m.connect_account(AccountApi::V2).unwrap();
    let (s, v) = json_of(get(&m, "/v1/accounts").await).await;
    assert_eq!(s, 200);
    assert_eq!(v["object"], "list");
    assert_eq!(v["url"], "/v1/accounts");
    assert_eq!(ids(&v), vec![v2.clone(), v1.clone()]);
    let two = v["data"][0].clone();
    assert_eq!(two["type"], "none");
    assert_eq!(two["controller"]["type"], "application");
    assert_eq!(two["object"], "account");
    let (s, one) = json_of(get(&m, &format!("/v1/accounts/{v1}")).await).await;
    assert_eq!((s, one["id"].as_str()), (200, Some(v1.as_str())));
    m.disconnect_account(&v1).unwrap();
    let (_, v) = json_of(get(&m, "/v1/accounts").await).await;
    assert_eq!(ids(&v), vec![v2.clone()]);
    let (s, e) = json_of(get(&m, &format!("/v1/accounts/{v1}")).await).await;
    assert_eq!(s, 404);
    assert_eq!(e["error"]["code"], "resource_missing");
}

#[tokio::test]
async fn v2_accounts_view() {
    let m = mock_with(CONNECT).await;
    let id = loop {
        // Standard is one of three v1 types; draw until one is.
        let id = m.connect_account(AccountApi::V1).unwrap();
        if m.accounts().last().unwrap().kind == AccountKind::Standard {
            break id;
        }
    };
    let (s, v) = json_of(get(&m, &format!("/v2/core/accounts/{id}")).await).await;
    assert_eq!(s, 200);
    assert_eq!(v["object"], "v2.core.account");
    assert_eq!(v["applied_configurations"], serde_json::json!(["merchant"]));
    assert_eq!(v["dashboard"], "full");
    assert_eq!(v["closed"], false);
    // Includable-only fields are absent: the mock serves no `include`.
    for absent in [
        "configuration",
        "defaults",
        "identity",
        "requirements",
        "future_requirements",
    ] {
        assert!(v.get(absent).is_none(), "{absent}: {v}");
    }
    let (_, one) = json_of(get(&m, &format!("/v1/accounts/{id}")).await).await;
    assert_eq!(
        one["controller"],
        serde_json::json!({"is_controller": false, "type": "account"})
    );
    let created = v["created"].as_str().unwrap();
    let b = created.as_bytes();
    assert_eq!(created.len(), 24, "{created}");
    assert!(created.ends_with('Z') && b[4] == b'-' && b[10] == b'T' && b[19] == b'.');
    let (s, e) = json_of(get(&m, "/v2/core/accounts/acct_nope").await).await;
    assert_eq!((s, e["error"]["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn v2_accounts_closed_filter() {
    let m = mock_with(CONNECT).await;
    let open = m.connect_account(AccountApi::V2).unwrap();
    let gone = m.connect_account(AccountApi::V2).unwrap();
    m.disconnect_account(&gone).unwrap();
    let (s, v) = json_of(get(&m, "/v2/core/accounts").await).await;
    assert_eq!((s, ids(&v)), (200, vec![open.clone()]));
    assert!(v["next_page_url"].is_null() && v["previous_page_url"].is_null());
    let (_, v) = json_of(get(&m, "/v2/core/accounts?closed=true").await).await;
    assert_eq!(ids(&v), vec![gone.clone()]);
    assert_eq!(v["data"][0]["closed"], true);
    let (s, v) = json_of(get(&m, &format!("/v2/core/accounts/{gone}")).await).await;
    assert_eq!((s, v["closed"].clone()), (200, Value::from(true)));
    assert_eq!(v["object"], "v2.core.account");
    let (s, _) = json_of(get(&m, "/v2/core/accounts?closed=maybe").await).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn v2_accounts_configuration_filter() {
    let m = mock_with(CONNECT).await;
    for _ in 0..25 {
        m.connect_account(AccountApi::V2).unwrap();
    }
    let all = m.accounts();
    let want: Vec<String> = all
        .iter()
        .filter(|a| a.configurations.contains(&Configuration::Recipient))
        .map(|a| a.id.clone())
        .rev()
        .collect();
    assert!(!want.is_empty() && want.len() < all.len(), "seed spread");
    let (s, v) = json_of(
        get(
            &m,
            "/v2/core/accounts?applied_configurations=recipient&limit=100",
        )
        .await,
    )
    .await;
    assert_eq!((s, ids(&v)), (200, want.clone()));
    for acct in v["data"].as_array().unwrap() {
        assert!(
            acct["applied_configurations"]
                .as_array()
                .unwrap()
                .contains(&Value::from("recipient"))
        );
    }
    // Any of several matches.
    let either: Vec<String> = all
        .iter()
        .filter(|a| {
            a.configurations
                .iter()
                .any(|c| matches!(c, Configuration::Recipient | Configuration::Customer))
        })
        .map(|a| a.id.clone())
        .rev()
        .collect();
    let (_, v) = json_of(
        get(
            &m,
            "/v2/core/accounts?applied_configurations=recipient,customer&limit=100",
        )
        .await,
    )
    .await;
    assert_eq!(ids(&v), either);
    // Page tokens walk every account exactly once.
    let mut seen = Vec::new();
    let (_, mut v) = json_of(get(&m, "/v2/core/accounts?limit=10").await).await;
    loop {
        seen.extend(ids(&v));
        let Some(next) = v["next_page_url"].as_str() else {
            break;
        };
        v = json_of(get(&m, next).await).await.1;
    }
    let mut every: Vec<String> = all.iter().map(|a| a.id.clone()).collect();
    every.reverse();
    assert_eq!(seen, every);
}

#[tokio::test]
async fn stale_reads_respect_scope() {
    let m = mock_with(
        "[accounts]\nconnect = true\n[api]\nstale_read = { probability = 1.0, max_lag = \"10s\" }",
    )
    .await;
    m.step(Duration::from_secs(60)).await.unwrap();
    let a = m.connect_account(AccountApi::V1).unwrap();
    let c = customer(&m, &a);
    for _ in 0..10 {
        let (s, v) = json_of(get(&m, "/v1/accounts").await).await;
        assert_eq!((s, ids(&v)), (200, vec![]));
        let (s, v) = json_of(get(&m, "/v2/core/accounts").await).await;
        assert_eq!((s, ids(&v)), (200, vec![]));
        let (_, v) = json_of(get(&m, "/v1/customers").await).await;
        assert!(ids(&v).is_empty());
    }
    m.step(Duration::from_secs(11)).await.unwrap();
    let (_, v) = json_of(get(&m, "/v1/accounts").await).await;
    assert_eq!(ids(&v), vec![a.clone()]);
    let (_, v) = json_of(get_as(&m, "/v1/customers", &a).await).await;
    assert_eq!(ids(&v), vec![c]);
    let (_, v) = json_of(get(&m, "/v1/customers").await).await;
    assert!(ids(&v).is_empty());
    // A stale v2 read of an account disconnected a moment ago is not closed yet.
    m.disconnect_account(&a).unwrap();
    for _ in 0..10 {
        let (s, v) = json_of(get(&m, &format!("/v2/core/accounts/{a}")).await).await;
        assert_eq!((s, v["closed"].clone()), (200, Value::from(false)));
    }
}

#[tokio::test]
async fn pagination_writes_stay_in_scope() {
    let m = mock_with(PAGINATION_WRITES).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    for _ in 0..30 {
        customer(&m, &a);
    }
    let (_, v) = json_of(get_as(&m, "/v1/customers?limit=10", &a).await).await;
    let last = ids(&v).pop().unwrap();
    let before = m.truth_log().len();
    let path = format!("/v1/customers?limit=10&starting_after={last}");
    let (s, _) = json_of(get_as(&m, &path, &a).await).await;
    assert_eq!(s, 200);
    let log = m.truth_log();
    assert!(log.len() > before, "the page was preceded by a write");
    for u in &log[before..] {
        assert_eq!(u.account.as_deref(), Some(a.as_str()), "{u:?}");
    }
}

#[tokio::test]
async fn pagination_writes_do_nothing_while_generation_is_off() {
    let m = mock_with(PAGINATION_WRITES).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    for _ in 0..30 {
        customer(&m, &a);
    }
    let (_, v) = json_of(get_as(&m, "/v1/customers?limit=10", &a).await).await;
    let last = ids(&v).pop().unwrap();
    m.quiesce().await.unwrap();
    let before = m.truth_log().len();
    let path = format!("/v1/customers?limit=10&starting_after={last}");
    let (s, _) = json_of(get_as(&m, &path, &a).await).await;
    assert_eq!(s, 200);
    let first = ids(&json_of(get(&m, "/v1/accounts").await).await.1).remove(0);
    let path = format!("/v1/accounts?limit=1&starting_after={first}");
    let (s, _) = json_of(get(&m, &path).await).await;
    assert_eq!(s, 200);
    assert_eq!(m.truth_log().len(), before);
}

#[tokio::test]
async fn destinations_are_platform_only() {
    let toml = format!(
        "{CONNECT}{}",
        common::dest("http://127.0.0.1:9/h", "snapshot", "*")
    );
    let m = mock_with(&toml).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let (_, v) = json_of(get(&m, "/v1/webhook_endpoints").await).await;
    assert_eq!(ids(&v).len(), 1, "the platform sees its destination");
    for header in ["Stripe-Account", "Stripe-Context"] {
        let r = get_with(&m, "/v1/webhook_endpoints", &[(header, &a)]).await;
        let (s, v) = json_of(r).await;
        assert_eq!(s, 200);
        assert_eq!(v["object"], "list");
        assert_eq!(v["has_more"], false);
        assert!(ids(&v).is_empty(), "{header}: {v}");
    }
    let (s, v) = json_of(get_ctx(&m, "/v2/core/event_destinations", &a).await).await;
    assert_eq!(s, 200);
    assert!(ids(&v).is_empty(), "{v}");
    assert!(v["next_page_url"].is_null());
}

const LATENCY: &str =
    "[accounts]\nconnect = true\n[api]\nlatency = { distribution = \"fixed\", ms = 300 }";

/// Starts `GET path` scoped to `acct` (`ctx` picks `Stripe-Context`) in a task, then
/// disconnects `acct` 100 ms in, inside the 300 ms latency window. Returns the response.
async fn disconnect_during_latency(
    m: &MockStripe,
    path: &str,
    acct: &str,
    ctx: bool,
) -> (u16, Value) {
    let header = if ctx {
        "Stripe-Context"
    } else {
        "Stripe-Account"
    };
    let req = reqwest::Client::new()
        .get(format!("{}{path}", m.api_url()))
        .bearer_auth("sk_test_mock")
        .header(header, acct);
    let task = tokio::spawn(async move { req.send().await.expect("request sent") });
    tokio::time::sleep(Duration::from_millis(100)).await;
    m.disconnect_account(acct).unwrap();
    json_of(task.await.unwrap()).await
}

#[tokio::test]
async fn disconnect_during_latency_is_403() {
    let m = mock_with(LATENCY).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let c = customer(&m, &a);
    let (s, v) = disconnect_during_latency(&m, "/v1/events", &a, false).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("account_invalid"))
    );

    let b = m.connect_account(AccountApi::V1).unwrap();
    let _ = customer(&m, &b);
    let (s, v) = disconnect_during_latency(&m, "/v2/core/events", &b, true).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("account_invalid"))
    );

    let d = m.connect_account(AccountApi::V1).unwrap();
    let dc = customer(&m, &d);
    let (s, v) = disconnect_during_latency(&m, &format!("/v1/customers/{dc}"), &d, false).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("account_invalid"))
    );

    let e = m.connect_account(AccountApi::V1).unwrap();
    let ev = m
        .event_timeline()
        .into_iter()
        .find(|x| x.account.as_deref() == Some(&*e) && x.format == EventFormat::Snapshot)
        .unwrap();
    let (s, v) = disconnect_during_latency(&m, &format!("/v1/events/{}", ev.id), &e, false).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("account_invalid"))
    );

    let f = m.connect_account(AccountApi::V1).unwrap();
    let ev = m
        .event_timeline()
        .into_iter()
        .find(|x| x.account.as_deref() == Some(&*f) && x.format == EventFormat::Thin)
        .unwrap();
    let (s, v) =
        disconnect_during_latency(&m, &format!("/v2/core/events/{}", ev.id), &f, true).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("account_invalid"))
    );
    let _ = c;
}

#[tokio::test]
async fn events_pages_after_disconnect_are_403() {
    let m = mock_with(CONNECT).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    for _ in 0..4 {
        customer(&m, &a);
    }
    let (s, v) = json_of(get_as(&m, "/v1/events?limit=2", &a).await).await;
    assert_eq!((s, v["has_more"].clone()), (200, Value::from(true)));
    let last = ids(&v).pop().unwrap();
    let (s, v) = json_of(get_ctx(&m, "/v2/core/events?limit=2", &a).await).await;
    assert_eq!(s, 200);
    let next = v["next_page_url"].as_str().unwrap().to_string();
    m.disconnect_account(&a).unwrap();
    let r = get_as(&m, &format!("/v1/events?limit=2&starting_after={last}"), &a).await;
    assert_eq!(r.status(), 403);
    let r = get_ctx(&m, &next, &a).await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn v2_accounts_cursor_survives_disconnect() {
    let m = mock_with(CONNECT).await;
    let made: Vec<String> = (0..4)
        .map(|_| m.connect_account(AccountApi::V2).unwrap())
        .collect();
    let (s, v) = json_of(get(&m, "/v2/core/accounts?limit=2").await).await;
    assert_eq!(s, 200);
    let page1 = ids(&v);
    assert_eq!(page1, vec![made[3].clone(), made[2].clone()]);
    let next = v["next_page_url"].as_str().unwrap().to_string();
    m.disconnect_account(&made[2]).unwrap();
    let (s, v) = json_of(get(&m, &next).await).await;
    assert_eq!(s, 200);
    assert_eq!(ids(&v), vec![made[1].clone(), made[0].clone()]);
    // Walking back from the closed cursor works too.
    let (s, v) = json_of(get(&m, "/v2/core/accounts?limit=2").await).await;
    assert_eq!((s, ids(&v)), (200, vec![made[3].clone(), made[1].clone()]));
}
