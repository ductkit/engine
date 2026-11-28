//! OpenAPI conformance coverage over a busy Connect run, plus the Review Focus checks:
//! documented field sets, cross-object consistency and payment lists under faults
//! (design sections 6.3 and 9, spec section 14).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use common::{Receiver, assert_conforms, expected_sweeps, mock_with};
use ductkit_stripe_mock::stripe::{check_payload, schema_fields};
use ductkit_stripe_mock::{ChangeKind, MockStripe, ObjectType, StateUpdate, expand_paths};
use serde_json::Value;

/// Every action and churn rate above zero, the phase-7 and phase-8 ones included, with daily
/// payouts of which a fifth fail. Rates are low so that 30 days of history in four scopes
/// stay quick to serve and check in a debug build.
const BUSY_RATES: &str = "[actions]\n\
    new_customer = 0.4\nstart_subscription = 0.15\ncancel_subscription = 0.03\n\
    catalog_change = 0.05\none_off_invoice = 0.1\none_off_payment = 0.3\nrefund = 0.05\n\
    dispute = 0.04\nsave_payment_method = 0.1\ncheckout = 0.15\n\
    application_fee_refund = 0.05\ntransfer_reversal = 0.05\n\
    payment_failure_probability = 0.3\n\
    payout = \"daily\"\npayout_failure_probability = 0.2\n\
    [churn.customer]\nupdate = 0.1\ndelete = 0.02\n\
    [churn.product]\nupdate = 0.02\narchive = 0.01\n\
    [churn.price]\nupdate = 0.02\narchive = 0.01\n\
    [churn.subscription]\nupdate = 0.05\n\
    [churn.invoice]\nupdate = 0.05\n\
    [churn.account]\nupdate = 0.02\n\
    [churn.payment_method]\ndetach = 0.02\n\
    [churn.person]\nupdate = 0.02\ndelete = 0.005\n\
    [churn.external_account]\ncreate = 0.02\ndelete = 0.01\n";

/// The busy run's config: seed 7, three initial connected accounts (half v1, half v2),
/// every Connect charge pattern, application fees on half the connected accounts'
/// payments, 30 days of history, one snapshot and one thin destination taking every event.
/// Accounts disconnect slowly enough that controlled ones stay connected, so their external
/// accounts and persons are reachable.
fn busy_config(snap: &str, thin: &str) -> String {
    format!(
        "seed = 7\n\
         [accounts]\nconnect = true\n\
         connected_accounts = {{ initial = 3, connect_rate = 0.004, deauthorize_rate = 0.001 }}\n\
         account_api = {{ v1 = 0.5, v2 = 0.5 }}\n\
         platform_payments = {{ own = 0.4, destination = 0.3, separate = 0.3 }}\n\
         application_fee = {{ probability = 0.5, percent = 10.0 }}\n\
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

/// The boolean fields whose `true` [`Served`] records: a fully reversed transfer, a fully
/// refunded charge or application fee.
const FLAGS: [&str; 2] = ["reversed", "refunded"];

/// What the served payloads showed: how often each `object` appeared, each
/// `(object, status)` pair and each `(object, flag)` seen `true` ([`FLAGS`]), nested
/// objects included.
#[derive(Default)]
struct Served {
    objects: BTreeMap<String, usize>,
    statuses: BTreeSet<(String, String)>,
    flags: BTreeSet<(String, String)>,
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
                    for flag in FLAGS {
                        if o.get(flag) == Some(&Value::Bool(true)) {
                            self.flags.insert((object.clone(), flag.to_string()));
                        }
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
type Truth = BTreeMap<(Option<String>, ObjectType, String), StateUpdate>;

/// The truth state at the end of the truth log.
fn latest(m: &MockStripe) -> Truth {
    let mut out = BTreeMap::new();
    for u in m.truth_log() {
        out.insert((u.account.clone(), u.object_type, u.object_id.clone()), u);
    }
    out
}

/// The live objects of type `t` in `scope`, by ID.
fn live_in<'a>(
    truth: &'a Truth,
    scope: &'a Option<String>,
    t: ObjectType,
) -> impl Iterator<Item = (&'a String, &'a Value)> + 'a {
    truth
        .iter()
        .filter(move |((acct, ty, _), u)| {
            acct == scope && *ty == t && u.kind != ChangeKind::Deleted
        })
        .map(|((_, _, id), u)| (id, &u.state))
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
    "/v1/transfers",
    "/v1/application_fees",
    "/v1/payouts",
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

/// Where an object of `t` in `scope` is retrieved: its path, and the scope the request
/// names. A transfer reversal or fee refund nests under its parent; an external account or
/// person lives in its account's scope but is retrieved by the platform under
/// `/v1/accounts/{acct}/…` (account endpoints refuse a `Stripe-Account` header). `None`
/// for line items.
fn retrieve_at(
    t: ObjectType,
    id: &str,
    state: &Value,
    scope: Option<&str>,
) -> Option<(String, Option<String>)> {
    if let Some(path) = t.api_path(id) {
        return Some((path, scope.map(String::from)));
    }
    let parent = match t {
        ObjectType::TransferReversal => state["transfer"].as_str()?,
        ObjectType::FeeRefund => state["fee"].as_str()?,
        ObjectType::BankAccount | ObjectType::Card | ObjectType::Person => {
            return Some((t.sub_path(scope?, id)?, None));
        }
        _ => return None,
    };
    Some((t.sub_path(parent, id)?, scope.map(String::from)))
}

/// What `check_scope` needs beyond the scope: the truth state, and the accounts still
/// connected (a disconnected account's objects are out of reach of cross-scope expansion).
struct Ctx<'a> {
    truth: &'a Truth,
    connected: BTreeSet<String>,
}

/// Lists, sub-lists, retrieves, expansions and events of one scope (`None`: the platform).
async fn check_scope(
    m: &MockStripe,
    served: &mut Served,
    ctx: &Ctx<'_>,
    scope: Option<&str>,
) -> BTreeSet<(ObjectType, &'static str)> {
    let truth = ctx.truth;
    let owner = scope.map(String::from);
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

    // Every child type through its parent's sub-list: together the sub-lists hold exactly
    // the scope's live children of that type.
    let mut children: BTreeMap<ObjectType, BTreeSet<String>> = BTreeMap::new();
    for (list, sub, t) in [
        ("/v1/transfers", "reversals", ObjectType::TransferReversal),
        ("/v1/application_fees", "refunds", ObjectType::FeeRefund),
    ] {
        for parent in &listed[list] {
            let path = format!("{list}/{}/{sub}", id_of(parent));
            for o in walk_v1(m, served, &path, scope).await {
                assert!(
                    children.entry(t).or_default().insert(id_of(&o)),
                    "{path}: {} listed twice",
                    o["id"]
                );
            }
        }
    }
    // An account's external accounts and persons, listed by the platform.
    if let Some(acct) = scope {
        for (sub, types) in [
            (
                "external_accounts",
                &[ObjectType::BankAccount, ObjectType::Card][..],
            ),
            ("persons", &[ObjectType::Person][..]),
        ] {
            let path = format!("/v1/accounts/{acct}/{sub}");
            for o in walk_v1(m, served, &path, None).await {
                let t = *types
                    .iter()
                    .find(|t| o["object"] == t.as_str())
                    .unwrap_or_else(|| panic!("{path}: {}", o["object"]));
                assert!(
                    children.entry(t).or_default().insert(id_of(&o)),
                    "{path}: {} listed twice",
                    o["id"]
                );
            }
        }
    }
    for t in [
        ObjectType::TransferReversal,
        ObjectType::FeeRefund,
        ObjectType::BankAccount,
        ObjectType::Card,
        ObjectType::Person,
    ] {
        let want: BTreeSet<String> = live_in(truth, &owner, t)
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(
            children.remove(&t).unwrap_or_default(),
            want,
            "{t:?} sub-lists in {scope:?}"
        );
    }

    // Every object by ID, children through `sub_path`: live ones, and a deleted customer's
    // stub.
    let (mut paths, mut platform_paths) = (Vec::new(), Vec::new());
    for ((acct, t, id), u) in truth.range((owner.clone(), ObjectType::Customer, String::new())..) {
        if *acct != owner {
            break;
        }
        if *t == ObjectType::Account
            || (u.kind == ChangeKind::Deleted && *t != ObjectType::Customer)
        {
            continue;
        }
        match retrieve_at(*t, id, &u.state, scope) {
            Some((path, None)) if scope.is_some() => platform_paths.push(path),
            Some((path, _)) => paths.push(path),
            None => {}
        }
    }
    fetch_all(m, served, paths, scope).await;
    fetch_all(m, served, platform_paths, None).await;

    // Every allowed expand path, once per type, on an object where the path leads somewhere;
    // the list form too, on the lists and sub-lists.
    for ((acct, t, id), u) in truth.range((owner.clone(), ObjectType::Customer, String::new())..) {
        if *acct != owner {
            break;
        }
        if u.kind == ChangeKind::Deleted {
            continue;
        }
        for path in expand_paths(*t) {
            if !expands(ctx, &owner, *t, &u.state, path) || expanded.contains(&(*t, *path)) {
                continue;
            }
            let (base, at) = retrieve_at(*t, id, &u.state, scope).unwrap();
            let v = fetch(m, served, &format!("{base}?expand[]={path}"), at.as_deref()).await;
            let got = at_path(&v, path);
            assert!(got.is_object(), "{base} expand {path}: {got}");
            // An ID expands to the object it names, from whichever scope holds it (P2).
            if let Some(want) = id_at(&u.state, path) {
                assert_eq!(got["id"], want, "{base} expand {path}");
            }
            let collection = base.rsplit_once('/').map(|(c, _)| c).unwrap();
            let list = match LISTS
                .iter()
                .find(|l| l.split('?').next() == Some(collection))
            {
                Some(l) => Some(l.to_string()),
                None if t.api_path(id).is_none() => Some(collection.to_string()),
                None => None,
            };
            if let Some(list) = list {
                let sep = if list.contains('?') { '&' } else { '?' };
                fetch(
                    m,
                    served,
                    &format!("{list}{sep}limit=3&expand[]=data.{path}"),
                    at.as_deref(),
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

/// Includable lists: absent unless expanded, and expandable on every such object.
const INCLUDABLE: [(ObjectType, &str); 2] = [
    (ObjectType::Charge, "refunds"),
    (ObjectType::Invoice, "payments"),
];

/// The ID the field `path` names in `state`, if it holds one.
fn id_at<'a>(state: &'a Value, path: &str) -> Option<&'a str> {
    let (parent, target) = path.rsplit_once('.').unwrap_or(("", path));
    let holder = if parent.is_empty() {
        state
    } else {
        at_path(state, parent)
    };
    holder[target].as_str()
}

/// The scope the object at `path` of `t`'s `state` (in `scope`) lives in: the cross-scope
/// paths of phase 8 ruling P2, else the object's own scope. `None` is the platform.
fn target_scope(
    truth: &Truth,
    scope: &Option<String>,
    t: ObjectType,
    state: &Value,
    path: &str,
) -> Option<String> {
    let account = |v: &Value| v.as_str().map(String::from);
    match (t, path) {
        (ObjectType::Transfer, "destination_payment") => account(&state["destination"]),
        (ObjectType::TransferReversal, "destination_payment_refund") => {
            let transfer = state["transfer"].as_str().unwrap().to_string();
            account(&truth[&(scope.clone(), ObjectType::Transfer, transfer)].state["destination"])
        }
        (ObjectType::ApplicationFee, "charge") => account(&state["account"]),
        (ObjectType::Charge, "application_fee" | "source_transfer")
        | (ObjectType::Refund, "source_transfer_reversal")
        | (ObjectType::BankAccount | ObjectType::Card, "account") => None,
        _ => scope.clone(),
    }
}

/// Whether expanding `path` on a `t` in `state` (in `scope`) serves an object: an includable
/// list, or a field holding an ID whose scope is in reach (a disconnected account's objects
/// stay IDs). A payout's `destination` expands only where a store holds that bank account:
/// the platform's and Standard accounts' payouts name one no store holds (P4).
fn expands(ctx: &Ctx, scope: &Option<String>, t: ObjectType, state: &Value, path: &str) -> bool {
    if INCLUDABLE.contains(&(t, path)) {
        return true;
    }
    let Some(id) = id_at(state, path) else {
        return false;
    };
    // A disconnected account stays its ID, wherever it is named.
    if id.starts_with("acct_") && !ctx.connected.contains(id) {
        return false;
    }
    let target = target_scope(ctx.truth, scope, t, state, path);
    if let Some(acct) = &target
        && !ctx.connected.contains(acct)
    {
        return false;
    }
    if (t, path) == (ObjectType::Payout, "destination") {
        return ctx
            .truth
            .get(&(target, ObjectType::BankAccount, id.to_string()))
            .is_some_and(|u| u.kind != ChangeKind::Deleted);
    }
    true
}

/// The `(type, path)` pairs of `scope` that some live object can expand.
fn expandable_in(ctx: &Ctx, scope: &Option<String>) -> BTreeSet<(ObjectType, &'static str)> {
    ctx.truth
        .iter()
        .filter(|((acct, _, _), u)| acct == scope && u.kind != ChangeKind::Deleted)
        .flat_map(|((_, t, _), u)| {
            expand_paths(*t)
                .iter()
                .filter(|p| expands(ctx, scope, *t, &u.state, p))
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
    let ctx = Ctx {
        truth: &truth,
        connected: scopes.iter().flatten().cloned().collect(),
    };
    let mut expanded = BTreeMap::new();
    for scope in &scopes {
        let done = check_scope(&m, &mut served, &ctx, scope.as_deref()).await;
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

    assert_eq!(ObjectType::ALL.len(), 33);
    // The last eight are phase 9's (ruling Q1). Nothing generates them yet; Task 9 drops
    // this skip once every one is served.
    for t in &ObjectType::ALL[..25] {
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
        ("payout", "pending"),
        ("payout", "in_transit"),
        ("payout", "paid"),
        ("payout", "failed"),
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
    for (object, flag) in [("transfer", "reversed"), ("application_fee", "refunded")] {
        assert!(
            served
                .flags
                .contains(&(object.to_string(), flag.to_string())),
            "no {object} with {flag}: true served; served: {:?}",
            served.flags
        );
    }
    // External accounts of both objects.
    for object in ["bank_account", "card"] {
        assert!(
            served.objects.get(object).is_some_and(|n| *n > 0),
            "{object}"
        );
    }

    // Each scope expanded every path it holds an object for.
    for scope in &scopes {
        assert_eq!(expanded[scope], expandable_in(&ctx, scope), "{scope:?}");
        assert!(!expanded[scope].is_empty(), "{scope:?} expanded nothing");
    }
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
        // Only a refund of a destination charge that reverses its transfer sets these; the
        // mock's refunds never reverse a transfer (reversals are their own action).
        (ObjectType::Refund, "transfer_reversal"),
        (ObjectType::TransferReversal, "source_refund"),
        // Only for fees taken outside a charge (`originating_transaction` stays `null`).
        (ObjectType::ApplicationFee, "originating_transaction"),
        // Payout fees, reversals of payouts and instant payouts are not generated (P5).
        (ObjectType::Payout, "application_fee"),
        (ObjectType::Payout, "original_payout"),
        (ObjectType::Payout, "reversed_by"),
        // An account's external account belongs to no customer.
        (ObjectType::BankAccount, "customer"),
        (ObjectType::Card, "customer"),
        // Phase 9 (ruling Q10): nothing generates these yet. Each task building a type
        // removes its paths here; Task 9 checks none is left.
        (ObjectType::Customer, "discount.promotion_code"),
        (ObjectType::Customer, "discount.source.coupon"),
        (ObjectType::Subscription, "schedule"),
        (ObjectType::Subscription, "discounts"),
        (ObjectType::Invoice, "discounts"),
        (ObjectType::PromotionCode, "customer"),
        (ObjectType::PromotionCode, "promotion.coupon"),
        (ObjectType::InvoiceItem, "customer"),
        (ObjectType::InvoiceItem, "invoice"),
        (ObjectType::InvoiceItem, "discounts"),
        (ObjectType::CreditNote, "customer"),
        (ObjectType::CreditNote, "invoice"),
        (ObjectType::SubscriptionSchedule, "customer"),
        (ObjectType::SubscriptionSchedule, "subscription"),
    ];
    // Paths whose objects live only in connected accounts: direct charges' fees, `py_`
    // payments and `pyr_` refunds, external accounts, and the controlled accounts' payout
    // destinations (the platform's name a bank account no store holds, P4).
    let accounts_only = [
        (ObjectType::Charge, "application_fee"),
        (ObjectType::Charge, "source_transfer"),
        (ObjectType::Refund, "source_transfer_reversal"),
        (ObjectType::BankAccount, "account"),
        (ObjectType::Card, "account"),
        (ObjectType::Payout, "destination"),
    ];
    // On the platform that is every allowed path, except those whose field the mock never
    // sets and those that only connected accounts hold; across scopes, every path but the
    // never-set ones.
    let platform = &expanded[&None];
    let anywhere: BTreeSet<(ObjectType, &str)> = expanded.values().flatten().copied().collect();
    for t in ObjectType::ALL {
        for path in expand_paths(t) {
            let pair = (t, *path);
            assert_eq!(
                platform.contains(&pair),
                !never_set.contains(&pair) && !accounts_only.contains(&pair),
                "{t:?} expand {path} on the platform"
            );
            assert_eq!(
                anywhere.contains(&pair),
                !never_set.contains(&pair),
                "{t:?} expand {path} in any scope"
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
            // Includable-only: served with `expand[]=refunds`.
            "refunds",
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
            // Includable-only: served with `expand[]=confirmation_secret` / `payments`.
            "confirmation_secret",
            "payments",
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
        | ObjectType::InvoicePayment
        // Phase 8: every documented field is present (a value or `null`) on each, so none
        // is excluded. `application_fee.refunds` and `transfer.reversals` are required
        // embedded lists, not includable ones.
        | ObjectType::Transfer
        | ObjectType::TransferReversal
        | ObjectType::ApplicationFee
        | ObjectType::FeeRefund
        | ObjectType::Payout
        | ObjectType::BankAccount
        | ObjectType::Card
        | ObjectType::Person => vec![],
        // Phase 9: nothing generates these yet; the task building each type records its
        // exclusions here.
        ObjectType::Coupon
        | ObjectType::PromotionCode
        | ObjectType::Discount
        | ObjectType::InvoiceItem
        | ObjectType::CreditNote
        | ObjectType::CreditNoteLineItem
        | ObjectType::TaxRate
        | ObjectType::SubscriptionSchedule => vec![],
    };
    // Only where the platform controls the account (the field's own description), so not
    // on Standard accounts.
    if t == ObjectType::Account && o["controller"]["is_controller"] != true {
        out.push("external_accounts");
    }
    // Destination charges only (the field's own description): not on the platform's own
    // or separate charges, direct charges or `py_` payments. Not nullable, so absent too on
    // a destination charge that moved no money: a failed one (P7).
    if t == ObjectType::Charge && (o["transfer_data"].is_null() || o["status"] != "succeeded") {
        out.push("transfer");
    }
    // A `pyr_` refunds a `py_` payment, paid from the platform's Stripe balance: the
    // per-method `destination_details` has no type for that, and is not nullable.
    if t == ObjectType::Refund && o["id"].as_str().is_some_and(|id| id.starts_with("pyr_")) {
        out.push("destination_details");
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

/// The first page (up to 100) of `path` in `scope`, each object filed under its type.
async fn first_page(
    m: &MockStripe,
    served: &mut Served,
    objects: &mut BTreeMap<ObjectType, Vec<Value>>,
    path: &str,
    scope: Option<&str>,
) -> Vec<Value> {
    let sep = if path.contains('?') { '&' } else { '?' };
    let page = fetch(m, served, &format!("{path}{sep}limit=100"), scope).await;
    let data = page["data"].as_array().unwrap().clone();
    for o in &data {
        let t = ObjectType::ALL
            .into_iter()
            .find(|t| o["object"] == t.as_str())
            .unwrap_or_else(|| panic!("unknown object {}", o["object"]));
        objects.entry(t).or_default().push(o.clone());
    }
    data
}

#[tokio::test]
async fn served_objects_carry_their_documented_fields() {
    let m = mock_with(
        "seed = 7\n\
         [accounts]\nconnect = true\nconnected_accounts = { initial = 6 }\n\
         platform_payments = { own = 0.4, destination = 0.3, separate = 0.3 }\n\
         application_fee = { probability = 0.5, percent = 10.0 }\n\
         [actions]\nnew_customer = 6.0\nstart_subscription = 3.0\ncatalog_change = 0.5\n\
         one_off_invoice = 2.0\none_off_payment = 6.0\nrefund = 2.0\ndispute = 1.0\n\
         save_payment_method = 2.0\ncheckout = 3.0\npayment_failure_probability = 0.3\n\
         application_fee_refund = 2.0\ntransfer_reversal = 2.0\n\
         payout = \"daily\"\npayout_failure_probability = 0.3\n\
         [churn.person]\nupdate = 0.5\n\
         [churn.external_account]\ncreate = 0.5\n",
    )
    .await;
    // Past the first payouts' arrival, so paid and failed ones are served too.
    m.step(Duration::from_secs(3 * 86_400)).await.unwrap();
    m.quiesce().await.unwrap();
    let mut served = Served::default();
    let mut objects: BTreeMap<ObjectType, Vec<Value>> = BTreeMap::new();
    for path in LISTS {
        first_page(&m, &mut served, &mut objects, path, None).await;
    }
    let subs = objects[&ObjectType::Subscription].clone();
    for s in subs.iter().take(5) {
        let path = format!("/v1/subscription_items?subscription={}", id_of(s));
        first_page(&m, &mut served, &mut objects, &path, None).await;
    }
    let invoices = objects[&ObjectType::Invoice].clone();
    for i in invoices.iter().take(5) {
        let path = format!("/v1/invoices/{}/lines", id_of(i));
        first_page(&m, &mut served, &mut objects, &path, None).await;
    }
    let customers = objects[&ObjectType::Customer].clone();
    for c in &customers {
        let path = format!("/v1/customers/{}/payment_methods", id_of(c));
        first_page(&m, &mut served, &mut objects, &path, None).await;
    }
    // The children of the platform's transfers and fees, through the sub-lists of ten
    // reversed transfers and ten refunded fees (most have none, so every page is searched).
    let mut reversed = Vec::new();
    for (list, sub, field) in [
        ("/v1/transfers", "reversals", "amount_reversed"),
        ("/v1/application_fees", "refunds", "amount_refunded"),
    ] {
        let parents: Vec<Value> = walk_v1(&m, &mut served, list, None)
            .await
            .into_iter()
            .filter(|p| p[field].as_i64().unwrap() > 0)
            .take(10)
            .collect();
        for p in &parents {
            let path = format!("{list}/{}/{sub}", id_of(p));
            first_page(&m, &mut served, &mut objects, &path, None).await;
        }
        if sub == "reversals" {
            reversed = parents;
        }
    }
    // The `py_` payments of those transfers and their `pyr_` refunds, in the destination's
    // scope (an ordinary page of an account's charges or refunds may hold none).
    for t in &reversed {
        let (acct, py) = (t["destination"].as_str(), t["destination_payment"].as_str());
        let path = format!("/v1/refunds?charge={}", py.unwrap());
        first_page(&m, &mut served, &mut objects, &path, acct).await;
        let p = fetch(
            &m,
            &mut served,
            &format!("/v1/charges/{}", py.unwrap()),
            acct,
        )
        .await;
        objects.entry(ObjectType::Charge).or_default().push(p);
    }
    let accounts = first_page(&m, &mut served, &mut objects, "/v1/accounts", None).await;
    // Each connected account's external accounts and persons, and the payments, refunds and
    // payouts in its scope: direct charges, `py_` payments and their `pyr_` refunds.
    for a in &accounts {
        let acct = id_of(a);
        for sub in ["external_accounts", "persons"] {
            first_page(
                &m,
                &mut served,
                &mut objects,
                &format!("/v1/accounts/{acct}/{sub}"),
                None,
            )
            .await;
        }
        for path in ["/v1/charges", "/v1/refunds", "/v1/payouts"] {
            first_page(&m, &mut served, &mut objects, path, Some(&acct)).await;
        }
    }
    let ids = |t: ObjectType| -> Vec<String> {
        objects
            .get(&t)
            .map_or(Vec::new(), |l| l.iter().map(id_of).collect())
    };
    assert!(
        ids(ObjectType::Charge)
            .iter()
            .any(|id| id.starts_with("py_"))
    );
    assert!(
        ids(ObjectType::Refund)
            .iter()
            .any(|id| id.starts_with("pyr_"))
    );
    assert!(
        ids(ObjectType::Refund)
            .iter()
            .any(|id| id.starts_with("re_"))
    );

    let mut failures = Vec::new();
    // The phase 9 types (the last eight, ruling Q1) are not generated yet; Task 9 drops
    // this skip.
    for t in ObjectType::ALL[..25].iter().copied() {
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

/// Every balance transaction `GET /v1/balance_transactions?payout=` lists in `scope`.
async fn swept_by(m: &MockStripe, payout: &str, scope: Option<&str>) -> Vec<String> {
    let headers = scope_headers(scope, false);
    let mut out = Vec::new();
    let mut after = String::new();
    loop {
        let path = format!("/v1/balance_transactions?payout={payout}&limit=100{after}");
        let (status, page) = get_json(m, &path, &headers).await;
        assert_eq!(status, 200, "{path} in {scope:?}: {page}");
        let data = page["data"].as_array().unwrap();
        out.extend(data.iter().map(id_of));
        if page["has_more"] != true {
            return out;
        }
        after = format!("&starting_after={}", out.last().unwrap());
    }
}

/// Review Focus 1 of phase 8: money adds up across scopes, over the busy run's truth state.
#[tokio::test]
async fn money_moves_consistently() {
    let snap = Receiver::start(|_| (200, Duration::ZERO));
    let thin = Receiver::start(|_| (200, Duration::ZERO));
    let m = busy_run(&snap, &thin).await;
    let log = m.truth_log();
    let truth = latest(&m);
    let state = |scope: &Option<String>, t: ObjectType, id: &str| -> &Value {
        &truth
            .get(&(scope.clone(), t, id.to_string()))
            .unwrap_or_else(|| panic!("{t:?} {id} not in {scope:?}"))
            .state
    };
    let platform: Option<String> = None;

    // A transfer moves its `py_` payment's amount, and its reversals are the payment's
    // refunds: `amount_reversed` is the sum of its reversals and the payment's
    // `amount_refunded`.
    let (mut transfers, mut reversed) = (0, 0);
    for (id, tr) in live_in(&truth, &platform, ObjectType::Transfer) {
        let dest = tr["destination"].as_str().map(String::from);
        assert!(dest.is_some(), "{id}");
        let py_id = tr["destination_payment"].as_str().unwrap();
        assert!(py_id.starts_with("py_"), "{id}: {py_id}");
        let py = state(&dest, ObjectType::Charge, py_id);
        assert_eq!(tr["amount"], py["amount"], "{id} vs {py_id}");
        assert_eq!(py["source_transfer"], id.as_str(), "{py_id}");
        let reversals: i64 = live_in(&truth, &platform, ObjectType::TransferReversal)
            .filter(|(_, r)| r["transfer"] == id.as_str())
            .map(|(_, r)| r["amount"].as_i64().unwrap())
            .sum();
        assert_eq!(tr["amount_reversed"], reversals, "{id}");
        assert_eq!(
            tr["amount_reversed"], py["amount_refunded"],
            "{id} vs {py_id}"
        );
        transfers += 1;
        reversed += usize::from(reversals > 0);
    }

    // An application fee is its charge's `application_fee_amount`, and part of the `fee` of
    // that charge's balance transaction.
    let mut fees = 0;
    for (id, fee) in live_in(&truth, &platform, ObjectType::ApplicationFee) {
        let acct = fee["account"].as_str().map(String::from);
        let ch_id = fee["charge"].as_str().unwrap();
        let ch = state(&acct, ObjectType::Charge, ch_id);
        assert_eq!(
            fee["amount"], ch["application_fee_amount"],
            "{id} vs {ch_id}"
        );
        assert_eq!(ch["application_fee"], id.as_str(), "{ch_id}");
        let bt_id = ch["balance_transaction"].as_str().unwrap();
        let bt = state(&acct, ObjectType::BalanceTransaction, bt_id);
        let details = bt["fee_details"].as_array().unwrap();
        let of_fee: Vec<&Value> = details
            .iter()
            .filter(|d| d["type"] == "application_fee")
            .collect();
        assert_eq!(of_fee.len(), 1, "{bt_id}: {details:?}");
        assert_eq!(of_fee[0]["amount"], fee["amount"], "{bt_id} vs {id}");
        let total: i64 = details.iter().map(|d| d["amount"].as_i64().unwrap()).sum();
        assert_eq!(bt["fee"], total, "{bt_id}");
        let refunds: i64 = live_in(&truth, &platform, ObjectType::FeeRefund)
            .filter(|(_, r)| r["fee"] == id.as_str())
            .map(|(_, r)| r["amount"].as_i64().unwrap())
            .sum();
        assert_eq!(fee["amount_refunded"], refunds, "{id}");
        fees += 1;
    }

    // A payout's amount is the sum of the `net` of what it swept, which is above 0 (no
    // payout for a balance at or below 0), and no balance transaction is swept twice. Where
    // the scope is still reachable, `?payout=` lists exactly those.
    let connected: BTreeSet<String> = m
        .accounts()
        .into_iter()
        .filter(|a| a.connected)
        .map(|a| a.id)
        .collect();
    let expected = expected_sweeps(&log);
    let mut seen: BTreeSet<(Option<String>, String)> = BTreeSet::new();
    let (mut payout_scopes, mut failed, mut listed) = (BTreeSet::new(), 0, 0);
    for (po, (scope, members)) in &expected {
        let p = state(scope, ObjectType::Payout, po);
        let sum: i64 = members
            .iter()
            .map(|id| {
                state(scope, ObjectType::BalanceTransaction, id)["net"]
                    .as_i64()
                    .unwrap()
            })
            .sum();
        assert!(sum > 0, "{po} in {scope:?}: swept {sum}");
        assert_eq!(p["amount"], sum, "{po} in {scope:?}");
        for id in members {
            assert!(
                seen.insert((scope.clone(), id.clone())),
                "{id} in two payouts"
            );
        }
        if scope.as_ref().is_none_or(|a| connected.contains(a)) {
            let mut got = swept_by(&m, po, scope.as_deref()).await;
            got.sort();
            let mut want = members.clone();
            want.sort();
            assert_eq!(got, want, "{po} in {scope:?}");
            listed += 1;
        }
        payout_scopes.insert(scope.clone());
        failed += usize::from(p["status"] == "failed");
    }
    // Every payout in truth was accounted for.
    let all_payouts: BTreeSet<&String> = truth
        .keys()
        .filter(|(_, t, _)| *t == ObjectType::Payout)
        .map(|(_, _, id)| id)
        .collect();
    assert_eq!(all_payouts, expected.keys().collect());

    // Per scope and currency, the sum of every balance transaction's `net` equals the sum
    // over those neither swept nor `type: payout`.
    let mut all: BTreeMap<(Option<String>, String), i64> = BTreeMap::new();
    let mut open: BTreeMap<(Option<String>, String), i64> = BTreeMap::new();
    for ((scope, t, id), u) in &truth {
        if *t != ObjectType::BalanceTransaction {
            continue;
        }
        let key = (
            scope.clone(),
            u.state["currency"].as_str().unwrap().to_string(),
        );
        let n = u.state["net"].as_i64().unwrap();
        *all.entry(key.clone()).or_default() += n;
        let counted = open.entry(key).or_default();
        if u.state["type"] != "payout" && !seen.contains(&(scope.clone(), id.clone())) {
            *counted += n;
        }
    }
    assert_eq!(all, open);

    // Each relation is exercised, not vacuously true.
    for (what, n, min) in [
        ("transfers", transfers, 5),
        ("reversed transfers", reversed, 1),
        ("application fees", fees, 5),
        ("payouts", expected.len(), 5),
        ("failed payouts", failed, 1),
        ("payouts listed by `?payout=`", listed, 5),
        ("scopes with payouts", payout_scopes.len(), 2),
    ] {
        assert!(n >= min, "only {n} {what}");
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
    let faults = "[api]\n\
        stale_read = { probability = 0.5, max_lag = \"5s\" }\n\
        pagination_writes = { probability = 1.0 }\n";
    // Payments: an hour of busy one-off payments. Connect money: platform payments routed to
    // three accounts over 25 days of history with daily payouts, so the platform holds many
    // transfers and payouts. A payout has no pagination write (spec section 7.4).
    let runs = [
        (
            format!(
                "seed = 7\n\
                 [actions]\nnew_customer = 4.0\none_off_payment = 60.0\nrefund = 4.0\n\
                 payment_failure_probability = 0.3\n{faults}"
            ),
            vec![
                ("/v1/charges", ObjectType::Charge, true),
                ("/v1/payment_intents", ObjectType::PaymentIntent, true),
            ],
        ),
        (
            format!(
                "seed = 7\n\
                 [accounts]\nconnect = true\nconnected_accounts = {{ initial = 3 }}\n\
                 platform_payments = {{ own = 0.5, destination = 0.25, separate = 0.25 }}\n\
                 [history]\nduration = \"25d\"\n\
                 [actions]\nnew_customer = 0.5\none_off_payment = 0.5\ntransfer_reversal = 0.1\n\
                 payout = \"daily\"\npayout_failure_probability = 0.2\n{faults}"
            ),
            vec![
                ("/v1/transfers", ObjectType::Transfer, true),
                ("/v1/payouts", ObjectType::Payout, false),
            ],
        ),
    ];
    for (config, lists) in runs {
        let m = mock_with(&config).await;
        m.step(Duration::from_secs(3600)).await.unwrap();
        let mut stale = 0;
        for round in 0..10 {
            assert!(m.generation_enabled());
            for (path, t, writes) in &lists {
                let before = m.truth_log().len();
                let (ids, s) = paginate_ids(&m, path, *t).await;
                stale += s;
                assert!(ids.len() > 20, "{path}: {} ids", ids.len());
                // Every cursor page ran a pagination write first, where the type has one.
                if *writes {
                    assert!(
                        m.truth_log().len() > before,
                        "{path} round {round}: no pagination write"
                    );
                }
                let unique: BTreeSet<&String> = ids.iter().collect();
                assert_eq!(
                    unique.len(),
                    ids.len(),
                    "{path} round {round}: an ID listed twice"
                );
            }
            m.step(Duration::from_secs(60)).await.unwrap();
        }
        assert!(stale > 0, "{lists:?}: no stale read observed");
    }
}
