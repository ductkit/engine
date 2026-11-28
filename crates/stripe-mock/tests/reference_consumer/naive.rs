//! The naive consumer: it trusts every snapshot payload. It upserts `data.object` from
//! webhooks and from an `ending_before` poller, with no dedupe and no ordering check,
//! and removes an object on any `*.deleted` event.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::client::{Client, Fetched};
use crate::correct::stripe_object;
use crate::hooks::HookServer;
use crate::truth::{COMPARED, Synced};

pub struct Naive {
    pub synced: Synced,
    hooks: HookServer,
    cursor: usize,
    /// Per scope, the newest event ID the poller has seen.
    newest: BTreeMap<Option<String>, String>,
}

impl Naive {
    pub fn new(hooks: HookServer) -> Naive {
        Naive {
            synced: Synced::new(),
            hooks,
            cursor: 0,
            newest: BTreeMap::new(),
        }
    }

    pub async fn backfill(&mut self, c: &Client, scopes: &[Option<String>]) {
        self.synced = c.backfill(scopes).await;
    }

    /// Applies every new webhook, then polls each scope's events past its cursor.
    pub async fn tick(&mut self, c: &Client, scopes: &[Option<String>]) {
        for e in self.hooks.take_new(&mut self.cursor) {
            let scope = e["account"].as_str().map(str::to_string);
            self.apply(scope, &e);
        }
        for scope in scopes {
            let path = match self.newest.get(scope) {
                Some(id) => format!("/v1/events?limit=100&ending_before={id}"),
                None => "/v1/events?limit=100".to_string(),
            };
            let Fetched::Object(page) = c.get(&path, scope.as_deref()).await else {
                panic!("{path} is gone");
            };
            let data = page["data"].as_array().unwrap();
            if let Some(first) = data.first() {
                self.newest
                    .insert(scope.clone(), first["id"].as_str().unwrap().to_string());
            }
            for e in data.iter().rev() {
                self.apply(scope.clone(), e);
            }
        }
    }

    fn apply(&mut self, scope: Option<String>, e: &Value) {
        let obj = &e["data"]["object"];
        let Some(t) = COMPARED
            .into_iter()
            .find(|t| obj["object"].as_str() == Some(stripe_object(*t)))
        else {
            return;
        };
        let key = (scope, t, obj["id"].as_str().unwrap().to_string());
        if e["type"].as_str().unwrap().ends_with(".deleted") {
            self.synced.remove(&key);
        } else {
            self.synced.insert(key, obj.clone());
        }
    }
}
