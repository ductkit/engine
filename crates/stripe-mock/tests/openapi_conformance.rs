//! OpenAPI conformance coverage over a busy Connect run, plus the Review Focus checks:
//! documented field sets, cross-object consistency and payment lists under faults
//! (design sections 6.3 and 9, spec section 14).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use common::{Receiver, assert_conforms, mock_with};
use ductkit_stripe_mock::stripe::{check_payload, schema_fields};
use ductkit_stripe_mock::{ChangeKind, MockStripe, ObjectType, StateUpdate, expand_paths};
use serde_json::Value;

/// Every action and churn rate above zero, the new phase-7 ones included. Rates are low so
/// that 30 days of history in four scopes stay quick to serve and check in a debug build.
const BUSY_RATES: &str = "[actions]\n\
    new_customer = 0.4\nstart_subscription = 0.15\ncancel_subscription = 0.03\n\
    catalog_change = 0.05\none_off_invoice = 0.1\none_off_payment = 0.3\nrefund = 0.05\n\
    dispute = 0.04\nsave_payment_method = 0.1\ncheckout = 0.15\n\
    payment_failure_probability = 0.3\n\
    [churn.customer]\nupdate = 0.1\ndelete = 0.02\n\
    [churn.product]\nupdate = 0.02\narchive = 0.01\n\
    [churn.price]\nupdate = 0.02\narchive = 0.01\n\
    [churn.subscription]\nupdate = 0.05\n\
    [churn.invoice]\nupdate = 0.05\n\
    [churn.account]\nupdate = 0.02\n\
    [churn.payment_method]\ndetach = 0.02\n";

/// The busy run's config: seed 7, three initial connected accounts (half v1, half v2),
/// 30 days of history, one snapshot and one thin destination taking every event.
fn busy_config(snap: &str, thin: &str) -> String {
    format!(
        "seed = 7\n\
         [accounts]\nconnect = true\n\
         connected_accounts = {{ initial = 3, connect_rate = 0.004, deauthorize_rate = 0.002 }}\n\
         account_api = {{ v1 = 0.5, v2 = 0.5 }}\n\
         [history]\nduration = \"30d\"\n\
         {BUSY_RATES}\
         [[destinations]]\nname = \"snap\"\nurl = \"{snap}\"\npayload = \"snapshot\"\n\
         enabled_events = [\"*\"]\nconnect = true\n\
         [[destinations]]\nname = \"thin\"\nurl = \"{thin}\"\npayload = \"thin\"\n\
         enabled_events = [\"*\"]\nevents_from = [\"@self\", \"@accounts\"]\n"
    )
}

/// Starts the busy run, steps three simulated days and quiesces.
async fn busy_run(snap: &Receiver, thin: &Receiver) -> MockStripe {
    let m = mock_with(&busy_config(&snap.url("/s"), &thin.url("/t"))).await;
    m.step(Duration::from_secs(3 * 86_400)).await.unwrap();
    m.quiesce().await.unwrap();
    m
}

/// What the served payloads showed: how often each `object` appeared, and each
/// `(object, status)` pair, nested objects included.
#[derive(Default)]
struct Served {
    objects: BTreeMap<String, usize>,
    statuses: BTreeSet<(String, String)>,
}

impl Served {
    fn record(&mut self, v: &Value) {
        match v {
            Value::Object(o) => {
                if let Some(Value::String(object)) = o.get("object") {
                    *self.objects.entry(object.clone()).or_default() += 1;
                    if let Some(Value::String(status)) = o.get("status") {
                        self.statuses.insert((object.clone(), status.clone()));
                    }
                }
                o.values().for_each(|c| self.record(c));
            }
            Value::Array(a) => a.iter().for_each(|c| self.record(c)),
            _ => {}
        }
    }
}

/// The request headers that select `scope` on v1 (`Stripe-Account`) or v2
/// (`Stripe-Context`) endpoints.
fn scope_headers(scope: Option<&str>, v2: bool) -> Vec<(&'static str, &str)> {
    let name = if v2 {
        "Stripe-Context"
    } else {
        "Stripe-Account"
    };
    scope.map(|s| (name, s)).into_iter().collect()
}

thread_local! {
    /// One client per test thread, so connections are reused: thousands of fresh
    /// connections would exhaust the ephemeral ports on Windows.
    static CLIENT: reqwest::Client = reqwest::Client::new();
}

/// `GET path` with `headers`; checks a 2xx body against the schema and returns the status
/// and the body.
async fn get_json(m: &MockStripe, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    get_url(&m.api_url(), path, headers).await
}

/// [`get_json`] against the API at `base`.
async fn get_url(base: &str, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut req = CLIENT
        .with(|c| c.clone())
        .get(format!("{base}{path}"))
        .bearer_auth("sk_test_mock");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let r = req.send().await.expect("request sent");
    let status = r.status().as_u16();
    let v: Value = r.json().await.expect("JSON body");
    if (200..300).contains(&status) {
        assert_conforms(&v);
    }
    (status, v)
}

/// `GET path` in `scope`; asserts 200 (`get_json` has already checked the body against the
/// schema), records the body and returns it.
async fn fetch(m: &MockStripe, served: &mut Served, path: &str, scope: Option<&str>) -> Value {
    let headers = scope_headers(scope, path.starts_with("/v2/"));
    let (status, v) = get_json(m, path, &headers).await;
    assert_eq!(status, 200, "{path} in {scope:?}: {v}");
    served.record(&v);
    v
}

/// `fetch` for many paths at once, eight in flight, so the client-side schema checks run on
/// several threads.
async fn fetch_all(m: &MockStripe, served: &mut Served, paths: Vec<String>, scope: Option<&str>) {
    let base = m.api_url();
    let mut tasks = tokio::task::JoinSet::new();
    let mut pending = paths.into_iter();
    loop {
        while tasks.len() < 8 {
            let Some(path) = pending.next() else { break };
            let (base, scope) = (base.clone(), scope.map(String::from));
            tasks.spawn(async move {
                let headers = scope_headers(scope.as_deref(), path.starts_with("/v2/"));
                let (status, v) = get_url(&base, &path, &headers).await;
                assert_eq!(status, 200, "{path} in {scope:?}: {v}");
                v
            });
        }
        match tasks.join_next().await {
            Some(v) => served.record(&v.unwrap()),
            None => return,
        }
    }
}

/// Every item of a v1 list, following `starting_after` 100 at a time.
async fn walk_v1(
    m: &MockStripe,
    served: &mut Served,
    path: &str,
    scope: Option<&str>,
) -> Vec<Value> {
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut items = Vec::new();
    let mut after = String::new();
    loop {
        let page = fetch(m, served, &format!("{path}{sep}limit=100{after}"), scope).await;
        let data = page["data"].as_array().unwrap();
        items.extend(data.iter().cloned());
        if page["has_more"] != true {
            return items;
        }
        let last = data.last().unwrap()["id"].as_str().unwrap();
        after = format!("&starting_after={last}");
    }
}

/// Every item of a v2 list, following `next_page_url`.
async fn walk_v2(
    m: &MockStripe,
    served: &mut Served,
    path: &str,
    scope: Option<&str>,
) -> Vec<Value> {
    let mut items = Vec::new();
    let mut next = Some(path.to_string());
    while let Some(url) = next {
        let page = fetch(m, served, &url, scope).await;
        items.extend(page["data"].as_array().unwrap().iter().cloned());
        next = page["next_page_url"].as_str().map(String::from);
    }
    items
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

/// The latest truth-log entry of every object, keyed by `(scope, type, id)`.
fn latest(m: &MockStripe) -> BTreeMap<(Option<String>, ObjectType, String), StateUpdate> {
    let mut out = BTreeMap::new();
    for u in m.truth_log() {
        out.insert((u.account.clone(), u.object_type, u.object_id.clone()), u);
    }
    out
}

/// The top-level list paths of a scope (no parent filter needed).
const LISTS: &[&str] = &[
    "/v1/customers",
    "/v1/products",
    "/v1/prices",
    "/v1/subscriptions?status=all",
    "/v1/invoices",
    "/v1/payment_intents",
    "/v1/charges",
    "/v1/refunds",
    "/v1/disputes",
    "/v1/balance_transactions",
    "/v1/setup_intents",
    "/v1/checkout/sessions",
    "/v1/invoice_payments",
];

/// The value at a dotted `path` of `v`, stepping into the first item of a `data` array.
fn at_path<'a>(v: &'a Value, path: &str) -> &'a Value {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match (seg, cur) {
            ("data", Value::Object(o)) => {
                o.get("data").and_then(|d| d.get(0)).unwrap_or(&Value::Null)
            }
            (_, v) => v.get(seg).unwrap_or(&Value::Null),
        };
    }
    cur
}

/// Lists, retrieves, expansions and events of one scope (`None`: the platform).
async fn check_scope(
    m: &MockStripe,
    served: &mut Served,
    truth: &BTreeMap<(Option<String>, ObjectType, String), StateUpdate>,
    scope: Option<&str>,
) -> BTreeSet<(ObjectType, &'static str)> {
    let mut expanded = BTreeSet::new();
    let mut listed: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for path in LISTS {
        listed.insert(path, walk_v1(m, served, path, scope).await);
    }
    for s in &listed["/v1/subscriptions?status=all"] {
        let id = id_of(s);
        walk_v1(
            m,
            served,
            &format!("/v1/subscription_items?subscription={id}"),
            scope,
        )
        .await;
    }
    for i in &listed["/v1/invoices"] {
        walk_v1(
            m,
            served,
            &format!("/v1/invoices/{}/lines", id_of(i)),
            scope,
        )
        .await;
    }
    for c in &listed["/v1/customers"] {
        let id = id_of(c);
        walk_v1(
            m,
            served,
            &format!("/v1/payment_methods?customer={id}"),
            scope,
        )
        .await;
        walk_v1(
            m,
            served,
            &format!("/v1/customers/{id}/payment_methods"),
            scope,
        )
        .await;
    }

    // Every object by ID: live ones, and a deleted customer's stub.
    let owner = scope.map(String::from);
    let mut paths = Vec::new();
    for ((acct, t, id), u) in truth.range((owner.clone(), ObjectType::Customer, String::new())..) {
        if *acct != owner {
            break;
        }
        if *t == ObjectType::Account
            || (u.kind == ChangeKind::Deleted && *t != ObjectType::Customer)
        {
            continue;
        }
        if let Some(path) = t.api_path(id) {
            paths.push(path);
        }
    }
    fetch_all(m, served, paths, scope).await;

    // Every allowed expand path, once per type, on an object where the path leads somewhere;
    // the list form too, on the lists that need no parent.
    for ((acct, t, id), u) in truth.range((owner.clone(), ObjectType::Customer, String::new())..) {
        if *acct != owner {
            break;
        }
        if u.kind == ChangeKind::Deleted {
            continue;
        }
        for path in expand_paths(*t) {
            if !names_an_id(&u.state, path) || expanded.contains(&(*t, *path)) {
                continue;
            }
            let base = t.api_path(id).unwrap();
            let v = fetch(m, served, &format!("{base}?expand[]={path}"), scope).await;
            assert!(
                at_path(&v, path).is_object(),
                "{base} expand {path}: {}",
                at_path(&v, path)
            );
            if let Some(list) = LISTS
                .iter()
                .find(|l| l.split('?').next() == base.rsplit_once('/').map(|(c, _)| c))
            {
                let sep = if list.contains('?') { '&' } else { '?' };
                fetch(
                    m,
                    served,
                    &format!("{list}{sep}limit=3&expand[]=data.{path}"),
                    scope,
                )
                .await;
            }
            expanded.insert((*t, *path));
        }
    }

    // Every event page, both formats (a retrieve by ID serves the same payload).
    walk_v1(m, served, "/v1/events", scope).await;
    walk_v2(m, served, "/v2/core/events?limit=100", scope).await;
    expanded
}

/// Whether the field `path` names in `state` holds an ID, so expanding it serves an object.
fn names_an_id(state: &Value, path: &str) -> bool {
    let (parent, target) = path.rsplit_once('.').unwrap_or(("", path));
    let holder = if parent.is_empty() {
        state
    } else {
        at_path(state, parent)
    };
    holder[target].is_string()
}

/// The `(type, path)` pairs of `scope` that some live object can expand.
fn expandable_in(
    truth: &BTreeMap<(Option<String>, ObjectType, String), StateUpdate>,
    scope: &Option<String>,
) -> BTreeSet<(ObjectType, &'static str)> {
    truth
        .iter()
        .filter(|((acct, _, _), u)| acct == scope && u.kind != ChangeKind::Deleted)
        .flat_map(|((_, t, _), u)| {
            expand_paths(*t)
                .iter()
                .filter(|p| names_an_id(&u.state, p))
                .map(|p| (*t, *p))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_connect_run_conforms() {
    let started = Instant::now();
    let snap = Receiver::start(|_| (200, Duration::ZERO));
    let thin = Receiver::start(|_| (200, Duration::ZERO));
    let m = busy_run(&snap, &thin).await;
    let generated = started.elapsed();
    let truth = latest(&m);
    let mut served = Served::default();

    // Platform-only views: both Accounts APIs, closed accounts too, and the targets.
    let v1_accounts = walk_v1(&m, &mut served, "/v1/accounts", None).await;
    for a in &v1_accounts {
        fetch(&m, &mut served, &format!("/v1/accounts/{}", id_of(a)), None).await;
    }
    let mut v2_accounts = walk_v2(&m, &mut served, "/v2/core/accounts?limit=100", None).await;
    v2_accounts.extend(
        walk_v2(
            &m,
            &mut served,
            "/v2/core/accounts?limit=100&closed=true",
            None,
        )
        .await,
    );
    for a in &v2_accounts {
        fetch(
            &m,
            &mut served,
            &format!("/v2/core/accounts/{}", id_of(a)),
            None,
        )
        .await;
    }
    fetch(&m, &mut served, "/v1/webhook_endpoints", None).await;
    fetch(&m, &mut served, "/v2/core/event_destinations", None).await;

    let accounts = m.accounts();
    assert!(accounts.len() >= 3, "{}", accounts.len());
    let mut scopes: Vec<Option<String>> = vec![None];
    scopes.extend(
        accounts
            .iter()
            .filter(|a| a.connected)
            .map(|a| Some(a.id.clone())),
    );
    let mut expanded = BTreeMap::new();
    for scope in &scopes {
        let done = check_scope(&m, &mut served, &truth, scope.as_deref()).await;
        expanded.insert(scope.clone(), done);
    }

    // Every delivered webhook body, both formats.
    let (snap_bodies, thin_bodies) = (snap.requests(), thin.requests());
    assert!(!snap_bodies.is_empty() && !thin_bodies.is_empty());
    for r in snap_bodies.iter().chain(&thin_bodies) {
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        if let Err(e) = check_payload(&v) {
            panic!("delivered body does not conform: {e}\n{v:#}");
        }
        served.record(&v);
    }

    for t in ObjectType::ALL {
        assert!(
            served.objects.contains_key(t.as_str()),
            "{t:?} never served; served: {:?}",
            served.objects
        );
    }
    let want = [
        ("payment_intent", "requires_confirmation"),
        ("payment_intent", "requires_payment_method"),
        ("payment_intent", "succeeded"),
        ("payment_intent", "canceled"),
        ("charge", "succeeded"),
        ("charge", "failed"),
        ("dispute", "needs_response"),
        ("dispute", "under_review"),
        ("checkout.session", "open"),
        ("checkout.session", "complete"),
        ("checkout.session", "expired"),
        ("balance_transaction", "pending"),
        ("balance_transaction", "available"),
    ];
    for (object, status) in want {
        assert!(
            served
                .statuses
                .contains(&(object.to_string(), status.to_string())),
            "{object} {status} never served; served: {:?}",
            served.statuses
        );
    }
    let closed = ["won", "lost", "warning_closed"];
    assert!(
        closed.iter().any(|s| served
            .statuses
            .contains(&("dispute".to_string(), s.to_string()))),
        "no closed dispute served: {:?}",
        served.statuses
    );
    // Each scope expanded every path it holds an object for. On the platform that is every
    // allowed path, except those whose field the mock never sets.
    for scope in &scopes {
        assert_eq!(expanded[scope], expandable_in(&truth, scope), "{scope:?}");
        assert!(!expanded[scope].is_empty(), "{scope:?} expanded nothing");
    }
    let platform = &expanded[&None];
    let never_set = [
        // The mock sets no default payment method on subscriptions, invoices or customers:
        // payments use the customer's newest attached card.
        (ObjectType::Subscription, "default_payment_method"),
        (ObjectType::Invoice, "default_payment_method"),
        (
            ObjectType::Customer,
            "invoice_settings.default_payment_method",
        ),
        // Charges and refunds never fail after succeeding, so no reversal transaction.
        (ObjectType::Charge, "failure_balance_transaction"),
        (ObjectType::Refund, "failure_balance_transaction"),
        // Checkout sessions run in payment or subscription mode, never setup mode.
        (ObjectType::CheckoutSession, "setup_intent"),
    ];
    for t in ObjectType::ALL {
        for path in expand_paths(t) {
            assert_eq!(
                platform.contains(&(t, *path)),
                !never_set.contains(&(t, *path)),
                "{t:?} expand {path}"
            );
        }
    }
    println!(
        "busy run: generated in {generated:?}, checked in {:?}; {} scopes, {} payloads, \
         {} webhooks, {} expand paths",
        started.elapsed() - generated,
        scopes.len(),
        served.objects.values().sum::<usize>(),
        snap_bodies.len() + thin_bodies.len(),
        expanded.values().map(BTreeSet::len).sum::<usize>()
    );
}

/// Fields each type leaves out of its schema's field list, and why (Stripe's API
/// reference). Everything else in `schema_fields` must be present on every object served.
fn excluded(t: ObjectType, o: &Value) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = match t {
        ObjectType::Account => vec![
            // Business entity details: returned for `business_type: company` / `individual`
            // once collected. The mock collects no business details (not modelled).
            "company",
            "individual",
        ],
        ObjectType::Charge => vec![
            // Adaptive Pricing / presentment in another currency: not modelled.
            "presentment_details",
            // Only when a Radar session was supplied with the payment: not modelled.
            "radar_options",
            // Destination charges only (the field's own description): phase 8.
            "transfer",
        ],
        ObjectType::CheckoutSession => vec![
            // Only when the session was created with it (Ruling, Task 12).
            "excluded_payment_method_types",
            // Includable-only: served with `expand[]=line_items`, not by default.
            "line_items",
            // Only when the session was created with it (Ruling, Task 12).
            "name_collection",
            // Adaptive Pricing: not modelled.
            "presentment_details",
            // `ui_mode: embedded_page` / `elements` only; the mock's sessions are hosted.
            "redirect_on_completion",
            "return_url",
        ],
        ObjectType::Customer => vec![
            // Only when set; the mock sets `name` instead (not modelled).
            "business_name",
            "individual_name",
            // Includable-only (served with `expand[]`): cash balance, credit balances,
            // sources, subscriptions, tax and tax IDs (Ruling R1).
            "cash_balance",
            "invoice_credit_balance",
            "sources",
            "subscriptions",
            "tax",
            "tax_ids",
        ],
        ObjectType::Invoice => vec![
            // Includable-only: served with `expand[]=confirmation_secret`.
            "confirmation_secret",
            // Only with details for the current status (blocked/paused): not modelled.
            "status_details",
            // Only for `billing_reason: subscription_threshold`: not modelled.
            "threshold_reason",
        ],
        ObjectType::PaymentIntent => vec![
            // Only when hooks were configured at creation: not modelled.
            "hooks",
            // Only when payment details (customer reference, order reference) were
            // supplied (Ruling, Task 9: no order reference).
            "payment_details",
            // Adaptive Pricing: not modelled.
            "presentment_details",
        ],
        ObjectType::Price => vec![
            // Includable-only: served with `expand[]=currency_options` / `expand[]=tiers`.
            "currency_options",
            "tiers",
        ],
        ObjectType::Refund => vec![
            // Non-card refunds only (the field's own description); the mock refunds cards.
            "description",
            "instructions_email",
            "next_action",
            // Only after a refund fails; the mock's refunds succeed.
            "failure_balance_transaction",
            "failure_reason",
            // Only while a refund is `pending`; the mock's refunds succeed at once.
            "pending_reason",
            // Adaptive Pricing: not modelled.
            "presentment_details",
        ],
        ObjectType::SetupIntent => vec![
            // "If present": only when the intent was created with it (not modelled).
            "attach_to_self",
        ],
        ObjectType::Subscription => vec![
            // Adaptive Pricing: not modelled.
            "presentment_details",
            // Only with details for the current status (paused/cancel reasons): not modelled.
            "status_details",
        ],
        ObjectType::SubscriptionItem => vec![
            // Only for items billed ahead under flexible billing (`billing_mode`
            // `flexible` with a billed-until date): not modelled.
            "billed_until",
        ],
        ObjectType::PaymentMethod => {
            // Only when the payment method came from a Radar session: not modelled.
            let mut v = vec!["radar_options"];
            v.extend(PAYMENT_METHOD_TYPE_HASHES);
            v
        }
        ObjectType::Product
        | ObjectType::InvoiceLineItem
        | ObjectType::Dispute
        | ObjectType::BalanceTransaction
        | ObjectType::InvoicePayment => vec![],
    };
    // Only where the platform controls the account (the field's own description), so not
    // on Standard accounts.
    if t == ObjectType::Account && o["controller"]["is_controller"] != true {
        out.push("external_accounts");
    }
    out
}

/// A payment method carries the hash of its own `type` only (`card` for the mock's cards).
const PAYMENT_METHOD_TYPE_HASHES: &[&str] = &[
    "acss_debit",
    "affirm",
    "afterpay_clearpay",
    "alipay",
    "alma",
    "amazon_pay",
    "au_becs_debit",
    "bacs_debit",
    "bancontact",
    "billie",
    "bizum",
    "blik",
    "boleto",
    "card_present",
    "cashapp",
    "crypto",
    "custom",
    "customer_balance",
    "eps",
    "fpx",
    "giropay",
    "grabpay",
    "ideal",
    "interac_present",
    "kakao_pay",
    "klarna",
    "konbini",
    "kr_card",
    "link",
    "mb_way",
    "mobilepay",
    "multibanco",
    "naver_pay",
    "nz_bank_account",
    "oxxo",
    "p24",
    "pay_by_bank",
    "payco",
    "paynow",
    "paypal",
    "paypay",
    "payto",
    "pix",
    "promptpay",
    "revolut_pay",
    "samsung_pay",
    "satispay",
    "scalapay",
    "sepa_debit",
    "sequra",
    "sofort",
    "sunbit",
    "swish",
    "twint",
    "upi",
    "us_bank_account",
    "wechat_pay",
    "zip",
];

#[tokio::test]
async fn served_objects_carry_their_documented_fields() {
    let m = mock_with(
        "seed = 7\n\
         [accounts]\nconnect = true\nconnected_accounts = { initial = 6 }\n\
         [actions]\nnew_customer = 6.0\nstart_subscription = 3.0\ncatalog_change = 0.5\n\
         one_off_invoice = 2.0\none_off_payment = 6.0\nrefund = 2.0\ndispute = 1.0\n\
         save_payment_method = 2.0\ncheckout = 3.0\npayment_failure_probability = 0.3\n",
    )
    .await;
    m.step(Duration::from_secs(86_400)).await.unwrap();
    m.quiesce().await.unwrap();
    let mut served = Served::default();
    let mut objects: BTreeMap<ObjectType, Vec<Value>> = BTreeMap::new();
    for path in LISTS {
        let page = fetch(
            &m,
            &mut served,
            &format!(
                "{path}{}limit=100",
                if path.contains('?') { '&' } else { '?' }
            ),
            None,
        )
        .await;
        for o in page["data"].as_array().unwrap() {
            let t = ObjectType::ALL
                .into_iter()
                .find(|t| o["object"] == t.as_str())
                .unwrap();
            objects.entry(t).or_default().push(o.clone());
        }
    }
    let subs = objects[&ObjectType::Subscription].clone();
    for s in subs.iter().take(5) {
        let page = fetch(
            &m,
            &mut served,
            &format!("/v1/subscription_items?subscription={}", id_of(s)),
            None,
        )
        .await;
        objects
            .entry(ObjectType::SubscriptionItem)
            .or_default()
            .extend(page["data"].as_array().unwrap().iter().cloned());
    }
    let invoices = objects[&ObjectType::Invoice].clone();
    for i in invoices.iter().take(5) {
        let page = fetch(
            &m,
            &mut served,
            &format!("/v1/invoices/{}/lines", id_of(i)),
            None,
        )
        .await;
        objects
            .entry(ObjectType::InvoiceLineItem)
            .or_default()
            .extend(page["data"].as_array().unwrap().iter().cloned());
    }
    let customers = objects[&ObjectType::Customer].clone();
    for c in &customers {
        let page = fetch(
            &m,
            &mut served,
            &format!("/v1/customers/{}/payment_methods", id_of(c)),
            None,
        )
        .await;
        objects
            .entry(ObjectType::PaymentMethod)
            .or_default()
            .extend(page["data"].as_array().unwrap().iter().cloned());
    }
    let accounts = fetch(&m, &mut served, "/v1/accounts?limit=100", None).await;
    objects.insert(
        ObjectType::Account,
        accounts["data"].as_array().unwrap().clone(),
    );

    let mut failures = Vec::new();
    for t in ObjectType::ALL {
        let schema = t.as_str();
        let list = objects.get(&t).cloned().unwrap_or_default();
        assert!(!list.is_empty(), "no {t:?} served");
        let documented: BTreeSet<&str> = schema_fields(schema).unwrap().iter().copied().collect();
        for o in &list {
            let mut want = documented.clone();
            for f in excluded(t, o) {
                assert!(
                    documented.contains(f),
                    "{schema} has no field {f} to exclude"
                );
                want.remove(f);
            }
            let have: BTreeSet<&str> = o.as_object().unwrap().keys().map(String::as_str).collect();
            let missing: Vec<_> = want.difference(&have).collect();
            let extra: Vec<_> = have.difference(&want).collect();
            if !missing.is_empty() || !extra.is_empty() {
                failures.push(format!(
                    "{schema} {}: missing {missing:?}, extra {extra:?}",
                    o["id"]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn payment_objects_are_consistent() {
    let snap = Receiver::start(|_| (200, Duration::ZERO));
    let thin = Receiver::start(|_| (200, Duration::ZERO));
    let m = busy_run(&snap, &thin).await;
    let truth = latest(&m);
    let live = |t: ObjectType| {
        truth
            .iter()
            .filter(move |((_, ty, _), u)| *ty == t && u.kind != ChangeKind::Deleted)
            .map(|((acct, _, id), u)| (acct.clone(), id.clone(), &u.state))
    };
    let state = |acct: &Option<String>, t: ObjectType, id: &str| -> &Value {
        &truth[&(acct.clone(), t, id.to_string())].state
    };
    let (mut succeeded_pairs, mut paid_invoices, mut refunded, mut disputed) = (0, 0, 0, 0);

    for (acct, id, ch) in live(ObjectType::Charge) {
        if ch["status"] == "succeeded"
            && let Some(pi) = ch["payment_intent"].as_str()
        {
            let pi = state(&acct, ObjectType::PaymentIntent, pi);
            if pi["status"] == "succeeded" {
                assert_eq!(ch["amount"], pi["amount_received"], "{id} vs {}", pi["id"]);
                succeeded_pairs += 1;
            }
        }
        let refunds: i64 = live(ObjectType::Refund)
            .filter(|(a, _, r)| *a == acct && r["charge"] == id.as_str())
            .map(|(_, _, r)| r["amount"].as_i64().unwrap())
            .sum();
        assert_eq!(ch["amount_refunded"].as_i64().unwrap(), refunds, "{id}");
        assert!(refunds <= ch["amount"].as_i64().unwrap(), "{id}");
        refunded += usize::from(refunds > 0);
        let has_dispute =
            live(ObjectType::Dispute).any(|(a, _, d)| a == acct && d["charge"] == id.as_str());
        assert_eq!(ch["disputed"] == true, has_dispute, "{id}");
        disputed += usize::from(has_dispute);
    }

    for (acct, id, inv) in live(ObjectType::Invoice) {
        let paid: i64 = live(ObjectType::InvoicePayment)
            .filter(|(a, _, p)| *a == acct && p["invoice"] == id.as_str())
            .map(|(_, _, p)| p["amount_paid"].as_i64().unwrap_or(0))
            .sum();
        assert_eq!(inv["amount_paid"].as_i64().unwrap(), paid, "{id}");
        paid_invoices += usize::from(paid > 0);
    }

    let mut txns = 0;
    for (_, id, t) in live(ObjectType::BalanceTransaction) {
        let (amount, fee, net) = (t["amount"].as_i64(), t["fee"].as_i64(), t["net"].as_i64());
        assert_eq!(net.unwrap(), amount.unwrap() - fee.unwrap(), "{id}");
        txns += 1;
    }
    // Each relation is exercised, not vacuously true.
    for (what, n) in [
        ("succeeded charge/intent pairs", succeeded_pairs),
        ("paid invoices", paid_invoices),
        ("refunded charges", refunded),
        ("disputed charges", disputed),
        ("balance transactions", txns),
    ] {
        assert!(n > 5, "only {n} {what}");
    }
}

/// One full pagination of `path` (listing `t`), 10 per page: the IDs in order, and how
/// many served objects differed from the live state the moment their page was served (a
/// stale read: an older version, or an object missing from the live view's newest page).
async fn paginate_ids(m: &MockStripe, path: &str, t: ObjectType) -> (Vec<String>, usize) {
    let mut ids = Vec::new();
    let mut stale = 0;
    let mut after = String::new();
    loop {
        let (status, v) = get_json(m, &format!("{path}?limit=10{after}"), &[]).await;
        assert_eq!(status, 200, "{path}: {v}");
        let data = v["data"].as_array().unwrap();
        for o in data {
            let live = m.get(t, o["id"].as_str().unwrap()).unwrap().state;
            stale += usize::from(*o != live);
        }
        if after.is_empty() {
            // The first page shows the newest objects: a live one newer than the page's
            // first object but missing from it was created after the stale view's time.
            let first = data.first().map_or(0, |o| o["created"].as_i64().unwrap());
            let shown: BTreeSet<&str> = data.iter().map(|o| o["id"].as_str().unwrap()).collect();
            stale += latest(m)
                .into_values()
                .filter(|u| {
                    u.account.is_none()
                        && u.object_type == t
                        && u.state["created"].as_i64().unwrap() > first
                        && !shown.contains(u.object_id.as_str())
                })
                .count();
        }
        ids.extend(data.iter().map(id_of));
        if v["has_more"] != true || data.is_empty() {
            return (ids, stale);
        }
        after = format!("&starting_after={}", ids.last().unwrap());
    }
}

#[tokio::test]
async fn payment_lists_survive_faults() {
    let m = mock_with(
        "seed = 7
         [actions]
new_customer = 4.0
one_off_payment = 60.0
refund = 4.0
         payment_failure_probability = 0.3
         [api]
stale_read = { probability = 0.5, max_lag = \"5s\" }
         pagination_writes = { probability = 1.0 }
",
    )
    .await;
    m.step(Duration::from_secs(3600)).await.unwrap();
    let mut stale = 0;
    for round in 0..10 {
        assert!(m.generation_enabled());
        for (path, t) in [
            ("/v1/charges", ObjectType::Charge),
            ("/v1/payment_intents", ObjectType::PaymentIntent),
        ] {
            let before = m.truth_log().len();
            let (ids, s) = paginate_ids(&m, path, t).await;
            stale += s;
            assert!(ids.len() > 20, "{path}: {} ids", ids.len());
            // Every cursor page ran a pagination write first.
            assert!(
                m.truth_log().len() > before,
                "{path} round {round}: no pagination write"
            );
            let unique: BTreeSet<&String> = ids.iter().collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "{path} round {round}: an ID listed twice"
            );
        }
        m.step(Duration::from_secs(60)).await.unwrap();
    }
    assert!(stale > 0, "no stale read observed");
}
