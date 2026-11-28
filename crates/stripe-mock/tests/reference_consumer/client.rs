//! A Stripe API client that survives the mock's API faults, and a full backfill.

use std::time::Duration;

use ductkit_stripe_mock::{MockStripe, ObjectType};
use serde_json::Value;

use crate::common::assert_conforms;
use crate::truth::{COMPARED, Synced};

pub struct Client {
    http: reqwest::Client,
    base: String,
}

/// A retrieve's outcome: the object, or gone (404 or a deleted stub).
pub enum Fetched {
    Object(Value),
    Gone,
}

/// The list path of a compared type. Subscriptions need `status=all`: the default list
/// hides canceled and `incomplete_expired` ones.
pub fn list_path(t: ObjectType) -> &'static str {
    match t {
        ObjectType::Customer => "/v1/customers",
        ObjectType::Product => "/v1/products",
        ObjectType::Price => "/v1/prices",
        ObjectType::Subscription => "/v1/subscriptions?status=all",
        ObjectType::Invoice => "/v1/invoices",
        other => panic!("{other:?} is not compared"),
    }
}

/// The platform, then every connected account.
pub fn scopes(m: &MockStripe) -> Vec<Option<String>> {
    std::iter::once(None)
        .chain(
            m.accounts()
                .into_iter()
                .filter(|a| a.connected)
                .map(|a| Some(a.id)),
        )
        .collect()
}

impl Client {
    pub fn new(m: &MockStripe) -> Client {
        Client {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base: m.api_url(),
        }
    }

    /// `GET path`, retrying 429, 5xx and dropped connections. Panics on any other
    /// non-2xx status.
    pub async fn get(&self, path: &str, scope: Option<&str>) -> Fetched {
        for _ in 0..20 {
            let mut req = self
                .http
                .get(format!("{}{path}", self.base))
                .bearer_auth("sk_test_mock");
            if let Some(s) = scope {
                req = req.header("Stripe-Context", s);
            }
            let Ok(resp) = req.send().await else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            let status = resp.status();
            if status == 429 || status.is_server_error() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            if status == 404 {
                return Fetched::Gone;
            }
            assert!(status.is_success(), "GET {path} ({scope:?}): {status}");
            let Ok(v) = resp.json::<Value>().await else {
                continue; // the connection dropped mid-body
            };
            assert_conforms(&v);
            return if v["deleted"] == true {
                Fetched::Gone
            } else {
                Fetched::Object(v)
            };
        }
        panic!("GET {path} ({scope:?}) kept failing");
    }

    /// Every compared object of every scope, walking each list with `starting_after`.
    /// A later page's copy of an ID overwrites the earlier one.
    pub async fn backfill(&self, scopes: &[Option<String>]) -> Synced {
        let mut out = Synced::new();
        for scope in scopes {
            for t in COMPARED {
                let path = list_path(t);
                let sep = if path.contains('?') { '&' } else { '?' };
                let mut after: Option<String> = None;
                loop {
                    let page_path = match &after {
                        Some(a) => format!("{path}{sep}limit=100&starting_after={a}"),
                        None => format!("{path}{sep}limit=100"),
                    };
                    let Fetched::Object(page) = self.get(&page_path, scope.as_deref()).await else {
                        panic!("list {page_path} is gone");
                    };
                    let data = page["data"].as_array().unwrap();
                    for o in data {
                        let id = o["id"].as_str().unwrap().to_string();
                        out.insert((scope.clone(), t, id), o.clone());
                    }
                    if page["has_more"] != true {
                        break;
                    }
                    after = data.last().map(|o| o["id"].as_str().unwrap().to_string());
                }
            }
        }
        out
    }
}
