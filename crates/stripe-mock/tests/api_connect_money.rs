//! Connect money objects: routing, empty lists, 404s, the header gate and read-only.

mod common;

use common::{get, get_with, mock_with};
use ductkit_stripe_mock::{AccountApi, MockStripe};
use serde_json::Value;

const CUSTOM: &str =
    "[accounts]\nconnect = true\naccount_types = { standard = 0.0, express = 0.0, custom = 1.0 }\n";

async fn json_of(r: reqwest::Response) -> (u16, Value) {
    let s = r.status().as_u16();
    (s, r.json().await.unwrap())
}

async fn setup() -> (MockStripe, String) {
    let m = mock_with(CUSTOM).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    (m, a)
}

fn list_paths(a: &str) -> Vec<String> {
    vec![
        "/v1/transfers".to_string(),
        "/v1/transfers/tr_1/reversals".to_string(),
        "/v1/application_fees".to_string(),
        "/v1/application_fees/fee_1/refunds".to_string(),
        "/v1/payouts".to_string(),
        format!("/v1/accounts/{a}/external_accounts"),
        format!("/v1/accounts/{a}/persons"),
    ]
}

#[tokio::test]
async fn new_lists_are_empty_and_well_formed() {
    let (m, a) = setup().await;
    for path in list_paths(&a) {
        // The sub-lists of a transfer or fee need the parent to exist.
        if path.contains("/tr_1/") || path.contains("/fee_1/") {
            let (s, v) = json_of(get(&m, &path).await).await;
            assert_eq!(
                (s, v["error"]["code"].as_str()),
                (404, Some("resource_missing"))
            );
            continue;
        }
        let (s, v) = json_of(get(&m, &path).await).await;
        assert_eq!(s, 200, "{path}");
        assert_eq!(v["object"], "list", "{path}");
        assert_eq!(v["data"], serde_json::json!([]), "{path}");
        assert_eq!(v["url"], path.as_str(), "{path}");
        assert_eq!(v["has_more"], false, "{path}");
    }
    for filtered in [
        "/v1/transfers?destination=acct_1&transfer_group=g",
        "/v1/application_fees?charge=ch_1",
        "/v1/payouts?status=paid&destination=ba_1",
        "/v1/charges?transfer_group=g",
    ] {
        let (s, v) = json_of(get(&m, filtered).await).await;
        assert_eq!(
            (s, v["data"].as_array().map(Vec::len)),
            (200, Some(0)),
            "{filtered}"
        );
    }
    let (s, v) = json_of(
        get(
            &m,
            &format!("/v1/accounts/{a}/external_accounts?object=card"),
        )
        .await,
    )
    .await;
    assert_eq!((s, v["data"].as_array().map(Vec::len)), (200, Some(0)));
    let (s, v) = json_of(
        get(
            &m,
            &format!("/v1/accounts/{a}/external_accounts?object=nope"),
        )
        .await,
    )
    .await;
    assert_eq!(s, 400, "{v}");
}

#[tokio::test]
async fn unknown_ids_404() {
    let (m, a) = setup().await;
    for path in [
        "/v1/transfers/tr_nope".to_string(),
        "/v1/application_fees/fee_nope".to_string(),
        "/v1/payouts/po_nope".to_string(),
        "/v1/transfers/tr_nope/reversals/trr_nope".to_string(),
        "/v1/application_fees/fee_nope/refunds/fr_nope".to_string(),
        format!("/v1/accounts/{a}/persons/person_nope"),
        format!("/v1/accounts/{a}/external_accounts/ba_nope"),
        "/v1/accounts/acct_nope/external_accounts".to_string(),
        "/v1/accounts/acct_nope/persons".to_string(),
        "/v1/accounts/acct_nope/persons/person_nope".to_string(),
    ] {
        let (s, v) = json_of(get(&m, &path).await).await;
        assert_eq!(s, 404, "{path}");
        assert_eq!(v["error"]["code"], "resource_missing", "{path}");
    }
    let (_, v) = json_of(get(&m, "/v1/accounts/acct_nope/persons").await).await;
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("acct_nope")
    );
    // A disconnected account is as good as unknown.
    m.disconnect_account(&a).unwrap();
    let (s, v) = json_of(get(&m, &format!("/v1/accounts/{a}/persons")).await).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("resource_missing"))
    );
}

#[tokio::test]
async fn account_sub_resources_refuse_account_header() {
    let (m, a) = setup().await;
    for sub in ["persons", "external_accounts"] {
        let r = get_with(
            &m,
            &format!("/v1/accounts/{a}/{sub}"),
            &[("Stripe-Account", &a)],
        )
        .await;
        let (s, v) = json_of(r).await;
        assert_eq!(s, 400, "{sub}");
        assert_eq!(
            v["error"]["message"],
            "Accounts endpoints must be called by the platform."
        );
    }
}

#[tokio::test]
async fn new_endpoints_are_read_only() {
    let (m, a) = setup().await;
    let mut paths = list_paths(&a);
    paths.push("/v1/transfers/tr_1".to_string());
    paths.push(format!("/v1/accounts/{a}/persons/person_1"));
    paths.push(format!("/v1/accounts/{a}/external_accounts/ba_1"));
    for path in paths {
        let r = reqwest::Client::new()
            .post(format!("{}{path}", m.api_url()))
            .bearer_auth("sk_test_mock")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{path}");
    }
}
