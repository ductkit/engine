# Stripe Mock Phase 3 (Billing Types and Fan-out) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mock gains products, prices, subscriptions, subscription items, invoices and invoice line items, each with its own lifecycle and deletion style. Business actions (`start_subscription`, `cancel_subscription`, natural renewals, `catalog_change`, plus a one-off invoice action) expand into realistic multi-object, multi-event sequences. Embedded lists are truncated as Stripe truncates them, and the read-only API serves every new type with its filters, sub-lists and `expand[]`.

**Architecture:**
- **Mutations announce their own events.** `Engine::create`, `update` and `delete` take an explicit list of snapshot event types. The list may be empty (subscription items, line items and invoice-number bumps announce nothing) or hold several types (`invoice.paid` plus `invoice.payment_succeeded`). The projector emits one snapshot and one thin event per listed type.
- **The generator owns a lifecycle queue.** Next to the Poisson-scheduled actions it keeps *continuations*, which are time-stamped follow-up steps (finalize this invoice, renew this subscription at period end, retry this payment). It runs Poisson actions and continuations in time order through the existing `run_until`.
- **Parents embed children.** Subscriptions and invoices embed their children (items, lines). The embedded lists are rendered from the child objects in the store whenever the parent is written.
- **The API becomes table-driven.** One generic resource module serves retrieve and list for every type from a per-type descriptor.

**Tech Stack:** The phase 2 stack, with no new dependencies. Calendar arithmetic uses `chrono` (`Months`).

**Spec:** [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md). Phase 3 is item 3 of section 16: "product, price, subscription, items, invoice, lines; business actions; nested list truncation; deletion styles". Sections 5.1–5.3, 6.1, 7.2 and 7.3 are the main references.

## Global Constraints

- **API version:** pinned `2026-11-18.endive`. Object shapes follow that version, which is after the 2025-03 "basil" changes:
  - Billing periods live on subscription items (`current_period_start`/`current_period_end`), not on the subscription.
  - An invoice links its subscription through `parent.subscription_details.subscription`.
  - A line item links its subscription item through `parent.subscription_item_details` and its price through `pricing.price_details`.
  - Shapes are hand-written here and validated against OpenAPI in phase 7.
- **ID prefixes** (spec section 5.1): `prod_`, `price_`, `sub_`, `si_`, `in_`, `il_`. Each is followed by 14 alphanumerics, like `cus_`.
- **Delete styles** (spec section 5.1):
  - Customer: hard delete; retrieve returns the stub.
  - Product and price: archive only (`active: false`).
  - Subscription: `canceled` and still retrievable.
  - Subscription item: hard delete, embedded in the subscription.
  - Invoice: drafts hard-delete (retrieve then returns 404 `resource_missing`); other invoices end `void` or `uncollectible`.
  - Line item: embedded in the invoice, and deleted together with its draft invoice.
- **State machines** (spec section 5.2), limited to the transitions phase 3 generates:
  - Subscription: `incomplete → active | incomplete_expired`, `active ⇄ past_due`, `active | past_due → canceled`.
  - Invoice: `draft → open → paid | void | uncollectible`, `draft →` deleted.
  - Any other transition is a bug: assert on it with `debug_assert!`.
- **Values** (spec section 5.3):
  - Integer amounts in the smallest unit.
  - Currencies `usd`, `eur` and `jpy` (zero-decimal; amounts are still integers).
  - Unix-second timestamps.
  - Metadata values that include `""` and a 500-character string (reuse `customer::random_metadata_value`).
- **Embedded lists** (spec section 7.3): `{"object": "list", "data": [...first N in creation order], "has_more": bool, "total_count": n, "url": "<sub-list path>"}`, with N = `payloads.nested_list_page_size` (default 10, valid 1..=100).
- **Events:** every announced mutation produces one snapshot and one thin event per listed type. Thin type = `v1.` + the snapshot type, and `related_object.url` = the type's API path.
- **Read-only API:** the Stripe API stays read-only (spec decision 2).
- **Determinism:** every generated decision comes from a named RNG stream (stream names are given per task). Stream `"generate.customer.create"` and the phase 1/2 golden values must not change.
- **Process:** the phase 1/2 rules still apply. `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean before every commit, and dependency versions live only in the root `[workspace.dependencies]`.
- **Branching:** work on a branch in the existing checkout (`git switch -c stripe-mock-phase-3`). Never use a worktree.

## Review Focus

1. **A subscription with more items than `nested_list_page_size`.** The user expects:
   - `subscription.items` shows the first N items, with `has_more: true`, `total_count` and `url`.
   - Following `url` (`/v1/subscription_items?subscription=sub_…`) with cursor pagination returns every item exactly once, starting with the same N.
   - The same holds for `invoice.lines` and `/v1/invoices/{id}/lines`.

   Tests in Task 5 (shape) and Task 7 (API).
2. **A customer deleted while it has live subscriptions.** The user expects the deletion to cancel every live subscription first, within the same action:
   - `customer.subscription.deleted` events come before `customer.deleted`, and the truth-log `seq` follows that order.
   - The canceled subscriptions stay retrievable, with `customer` still naming the deleted ID.
   - `expand[]=customer` on them returns the deleted stub.

   Tests in Task 6 and Task 8.
3. **Fetching the related object of a thin event after the object has moved on.**
   - A thin `v1.invoice.created` for a draft that was later deleted: `GET related_object.url` returns 404 `resource_missing`, unlike a deleted customer's stub.
   - A thin event for a subscription that has since changed: the fetch returns the current state.

   Tests in Task 7.
4. **One action producing many mutations in the same millisecond** (the default `step_delay` is 0). The user expects:
   - Every mutation of the action to share one `cause.action_id`.
   - Strictly increasing `seq`.
   - Contiguous per-object versions.
   - Webhooks to one destination delivered in truth order (the phase 2 per-destination ordering).

   Test in Task 5.
5. **An archived price that a live subscription still bills.** The user expects:
   - Renewals keep billing it (the invoice line references it).
   - `start_subscription` never picks an archived price.
   - `/v1/prices` lists it, and `active=true` hides it.

   Tests in Task 6 and Task 7.

## Decisions made in this plan (not in the spec)

- **Deferred actions.** `one_off_payment`, `refund` and `dispute` need `payment_intent`/`charge` (phase-2 entity types, spec section 16 item 7). They stay "not implemented yet" config errors, naming phase 7.
- **New action `one_off_invoice`** (rate per hour, default 0). It creates a manual invoice for a random customer and walks it to an outcome, so every invoice deletion style is exercised:
  - 10% of drafts are deleted;
  - of the finalized invoices, 80% are paid, 10% voided and 10% marked uncollectible.
- **New `[actions]` keys:**
  - `step_delay`: a `Distribution`, default `fixed 0`. It is the gap between consecutive steps of one action, which is spec 6.1's "configurable span".
  - `payment_failure_probability`, default `0.0`. It is the chance that an invoice payment attempt fails.
- **No payment objects.** "Paying" an invoice is a single mutation that sets it `paid` and announces `invoice.paid` and `invoice.payment_succeeded`. The spec 6.1 `payment_intent`/`charge` steps arrive with those types in phase 7.
- **Dunning:**
  - **Renewal invoice fails:** it announces `invoice.payment_failed` (`attempt_count` 1, `attempted: true`), and the subscription goes `past_due`. One retry is scheduled 3 days later:
    - success: the invoice is paid and the subscription goes back to `active`;
    - failure: the invoice is `uncollectible` and the subscription is `canceled`.
  - **First invoice fails** (`subscription_create`): the subscription stays `incomplete`. After 23 hours, the invoice is voided and the subscription becomes `incomplete_expired`.
- **Event table** (snapshot types; to be confirmed by calibration in phase 7):

  | Mutation | Events |
  |---|---|
  | product create / update / archive | `product.created` / `product.updated` / `product.updated` |
  | price create / update / archive | `price.created` / `price.updated` / `price.updated` |
  | subscription create | `customer.subscription.created` |
  | subscription status change to `active` or `past_due`, `cancel_at_period_end` change, metadata update, items re-rendered | `customer.subscription.updated` |
  | subscription to `canceled` or `incomplete_expired` | `customer.subscription.deleted` |
  | invoice create (draft) | `invoice.created` |
  | finalize | `invoice.finalized` |
  | pay | `invoice.paid`, `invoice.payment_succeeded` |
  | payment fails | `invoice.payment_failed` |
  | void | `invoice.voided` |
  | uncollectible | `invoice.marked_uncollectible` |
  | metadata update | `invoice.updated` |
  | draft delete | `invoice.deleted` |
  | subscription item, line item, customer `next_invoice_sequence` bump | none (truth log only) |

- **`ChangeKind`** gains `Archived` (product/price `active → false`) and `StateChanged` (a `status` change). These are the spec section 10.3 kinds. Other updates stay `Updated`.
- **`previous_attributes`** is included on every snapshot event of a non-create, non-delete mutation, not only on `*.updated`.
- **Invoice numbers.** Finalizing sets `number = "{customer.invoice_prefix}-{seq:04}"` using the customer's `next_invoice_sequence`. It then bumps that field in an unannounced customer mutation, as Stripe does.
- **Subscription item count.** A subscription gets 1–3 items. With probability 0.05 it gets 11–15 items instead, so truncation happens in normal runs.
- **Billing intervals.** Recurring prices are `month` (80%) or `year` (20%), with `interval_count` 1. Periods use calendar months (chrono `Months`, clamped to the month's end).
- **Prerequisites.** When an action needs something that doesn't exist yet, it creates it inside the same action (same `cause`). `start_subscription` creates a customer if none is live, and a product with a recurring price if no active recurring price exists.
- **Subscription list default.** `GET /v1/subscriptions` excludes `canceled` and `incomplete_expired` subscriptions unless `status=all` or an explicit status is given, as Stripe does.
- **Sub-list order.** `/v1/subscription_items` and `/v1/invoices/{id}/lines` list in creation order, ascending, so their first page equals the embedded list.
- **`expand[]`** supports these paths only (anything else → 400 `"This property cannot be expanded ({path})."`, param `expand`):
  - subscription: `customer`, `latest_invoice`, `items.data.price.product`
  - invoice: `customer`, `parent.subscription_details.subscription`
  - price and subscription item: `product` (subscription items: `price.product`)

  Lists take the same paths prefixed with `data.`. Expanding a deleted customer gives its stub; expanding a deleted draft invoice gives `null`.
- **Control trigger.** `MockStripe::run_action(Action) -> Result<Cause, MockError>` runs a named action now, together with every continuation due at or before now (spec section 11 "Triggers").
- **Quiesce.** Quiesce turns off continuations together with Poisson actions. Continuations that come due while generation is off run at the first `step` after `resume_generation`, in time order.

## File Structure

```
crates/stripe-mock/src/
├── config.rs              actions.{start_subscription, cancel_subscription, renew_subscription_cycle,
│                          catalog_change, one_off_invoice, step_delay, payment_failure_probability};
│                          churn.{product, price, subscription, invoice}; [payloads]
├── model/mod.rs           ObjectType + Product, Price, Subscription, SubscriptionItem, Invoice, InvoiceLineItem;
│                          as_str, id_prefix, api_path
├── model/embedded.rs      NEW: embedded_list(children, page_size, url)
├── model/catalog.rs       NEW: product/price JSON, random catalog content
├── model/invoice.rs       NEW: invoice + line item JSON, totals, transitions
├── model/subscription.rs  NEW: subscription + item JSON, periods, transitions
├── engine/mod.rs          ChangeKind::{Archived, StateChanged}; create/update/delete take event types
├── engine/events.rs       project() emits one pair per event type; per-type related_object url
├── generate.rs            action registry, continuation queue, run_action
├── generate/billing.rs    NEW: start/cancel/renew/dunning/one-off-invoice flows
├── generate/catalog.rs    NEW: catalog_change, product/price churn
├── handle.rs              run_action; delete_customer cascades
├── api/resources.rs       NEW: generic retrieve/list from per-type descriptors (replaces api/customers.rs)
├── api/expand.rs          NEW: expand[] paths
└── api/mod.rs             routes for every type and sub-list
crates/stripe-mock/tests/
├── catalog.rs, invoices.rs, subscriptions.rs, billing_cycle.rs, api_billing.rs, expand.rs
```

---

### Task 1: Config for phase 3

**Files:**
- Modify: `src/config.rs`, `src/lib.rs`

**Interfaces:**
- Produces:
  - `ActionsConfig` gains:
    - `start_subscription: f64`, `cancel_subscription: f64`, `catalog_change: f64`, `one_off_invoice: f64`;
    - `renew_subscription_cycle: RenewMode` (`enum RenewMode { Natural, Off }`, TOML `"natural"` | `"off"`, default `Natural`);
    - `step_delay: Distribution` (default `Fixed { ms: 0 }`);
    - `payment_failure_probability: f64` (0..=1).
  - `ChurnConfig` gains `product: CatalogChurn { update, archive }`, `price: CatalogChurn`, `subscription: UpdateChurn { update }` and `invoice: UpdateChurn`.
  - `PayloadsConfig { nested_list_page_size: usize }` (default 10, valid 1..=100) as `Config::payloads`.
  - All new types re-exported from `lib.rs`.
- `UNIMPLEMENTED_SECTIONS` becomes `["accounts", "history"]`. `UNIMPLEMENTED_KEYS` keeps `api.*` and becomes `actions.{one_off_payment, refund, dispute}`; `churn.*` leaves the list. The message for the actions keys reads `` `actions.refund` is not implemented yet (needs payment objects, phase 7) ``.

- [ ] **Step 1: Write failing config tests** (in `config.rs` `mod tests`)
  - `phase3_keys_parse`: parsing `[actions]\nstart_subscription = 4.0\nrenew_subscription_cycle = "natural"\nstep_delay = { distribution = "fixed", ms = 250 }\n[churn.product]\nupdate = 0.5\narchive = 0.1\n[payloads]\nnested_list_page_size = 3` gives those values.
  - `payment_objects_not_implemented`: `[actions]\nrefund = 0.4` is an error whose message contains `` `actions.refund` `` and `phase 7`.
  - `page_size_bounds`: `nested_list_page_size = 0` and `= 101` are errors naming `payloads.nested_list_page_size`.
  - `failure_probability_bounds`: `payment_failure_probability = 1.5` is an error.
  - The existing test that strips `UNIMPLEMENTED_SECTIONS` from the spec's example config keeps expecting `` `api.latency` ``: `api` is checked before `actions`, so the example's `one_off_payment` is not reported first.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --lib -- config::`. Expected: the new tests fail to compile or fail.
- [ ] **Step 3: Implement** the types and validation; rates must be finite and ≥ 0, like the existing ones.
- [ ] **Step 4: Run.** Expected: all config tests pass and the full suite is green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): phase 3 config`

---

### Task 2: Mutations announce their events; new object types and change kinds

**Files:**
- Modify: `src/model/mod.rs`, `src/engine/mod.rs`, `src/engine/events.rs`, `src/generate.rs`, `src/handle.rs`, `src/stream.rs` (only if `ChangeKind` matching needs it)
- Test: `src/engine/mod.rs` and `src/engine/events.rs` unit tests

**Interfaces:**
- Produces:
  - **`ObjectType`** gains `Product`, `Price`, `Subscription`, `SubscriptionItem`, `Invoice` and `InvoiceLineItem`:
    - `as_str`: `product`, `price`, `subscription`, `subscription_item`, `invoice`, `line_item`;
    - `id_prefix`: `prod`, `price`, `sub`, `si`, `in`, `il`;
    - `pub fn api_path(&self, id: &str) -> Option<String>`: `/v1/customers/{id}`, `/v1/products/{id}`, `/v1/prices/{id}`, `/v1/subscriptions/{id}`, `/v1/subscription_items/{id}`, `/v1/invoices/{id}`, and `None` for line items.
  - **`ChangeKind`** gains `Archived` and `StateChanged` (serde `archived`, `state_changed`).
  - **Engine mutations** take the snapshot event types they announce:
    - `pub(crate) fn create(&mut self, t, id, state, cause, events: &[&str]) -> StateUpdate`
    - `pub(crate) fn update(&mut self, t, id, f, cause, kind: ChangeKind, events: &[&str]) -> Result<Option<StateUpdate>, MockError>`; `kind` is `Updated`, `Archived` or `StateChanged`.
    - `pub(crate) fn delete(&mut self, t, id, tombstone, cause, events: &[&str]) -> Result<StateUpdate, MockError>`
  - **`Projector::project(&mut self, u: &StateUpdate, previous: Option<&Value>, types: &[&str]) -> Vec<(EventRecord, EventRecord)>`**, one pair per type, in order:
    - Each pair draws its own IDs, visibility and tiebreak, in that order per type, from the existing streams.
    - With an empty `types`, nothing is drawn.
    - The thin `related_object` is `{id, type: object_type.as_str(), url: api_path}`.
    - `previous_attributes` is set whenever `previous` is `Some` and the kind is not `Deleted`.
  - `EventRefs` lists every produced ID in order.
  - Customer call sites pass `["customer.created"]`, `["customer.updated"]` and `["customer.deleted"]`, so phase 1/2 behaviour and golden values do not change.

- [ ] **Step 1: Write failing unit tests**
  - `unannounced_mutation_has_no_events`: `create(.., &[])` gives empty `EventRefs`, and the event store is unchanged.
  - `two_types_two_pairs`: an update with `&["invoice.paid", "invoice.payment_succeeded"]` gives 2 snapshot and 2 thin IDs. Thin types are `v1.invoice.paid` and `v1.invoice.payment_succeeded`, both with `previous_attributes`, and `related_object.url == "/v1/invoices/in_x"`.
  - `state_changed_kind_is_recorded`: an update with `ChangeKind::StateChanged` stores that kind in the truth log.
  - `customer_events_unchanged`: the existing phase 2 event tests still pass unmodified, apart from the new call-site arguments.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --lib -- engine::`. Expected: compile failures.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the full suite. Expected: green, including every phase 1/2 integration test.
- [ ] **Step 5: Commit:** `refactor(stripe-mock): mutations name the events they announce`

---

### Task 3: Generator lifecycle queue, `run_action`, catalog

**Files:**
- Create: `src/model/catalog.rs`, `src/model/embedded.rs`, `src/generate/catalog.rs` (move `generate.rs` to `generate/mod.rs` with `git mv`), `tests/catalog.rs`
- Modify: `src/generate/mod.rs`, `src/handle.rs`, `src/lib.rs`

**Interfaces:**
- Produces:
  - **`pub enum Action`** (re-exported):
    - `NewCustomer`
    - `CatalogChange`
    - `StartSubscription { customer: Option<String>, prices: Option<Vec<String>> }`
    - `CancelSubscription { id: Option<String>, at_period_end: Option<bool> }`
    - `OneOffInvoice { customer: Option<String>, outcome: Option<InvoiceOutcome> }`, with `pub enum InvoiceOutcome { DeleteDraft, Paid, Void, Uncollectible }`
    - `Archive { id: String }`: archives a `prod_` or `price_` object now (`ChangeKind::Archived`, `product.updated`/`price.updated`); an already-archived object → `InvalidAction`

    A `None` field is drawn at random. `Archive` and the explicit outcome are control-only hooks for tests; the generator never uses them.
  - **`pub fn MockStripe::run_action(&self, a: Action) -> Result<Cause, MockError>`**: runs now, together with every continuation due at or before now. It returns `NoSuchObject` for a named ID that doesn't exist, and `MockError::InvalidAction(String)` (new variant) when the named object is in the wrong state, e.g. canceling a canceled subscription.
  - **Continuations:** `pub(crate) enum Continuation` (variants are added by Tasks 4–6). The queue is `BTreeMap<(SimTime, u64 /* seq */), (Continuation, Cause)>` in the `Generator`, with `pub(crate) fn schedule(&mut self, at: SimTime, c: Continuation, cause: Cause)`.
    - `next_due()` covers both Poisson actions and continuations.
    - `run_until` runs both in time order. On ties, continuations run before Poisson actions, then in seq order.
    - A continuation runs with the cause that scheduled it.
    - A continuation whose preconditions no longer hold (e.g. the subscription was canceled) does nothing and records nothing.
  - **`pub(crate) fn step_delay(&mut self) -> i64`**: draws `actions.step_delay` from stream `"generate.lifecycle"`. Every follow-up step of a flow is scheduled at `now + step_delay()`.
  - **`model::embedded::embedded_list(children: &[Value], page_size: usize, url: &str) -> Value`**
  - **Catalog JSON** (`model/catalog.rs`):
    - **Product:** `id, object "product", active, created, default_price (id|null), description, images [], livemode false, marketing_features [], metadata, name, package_dimensions null, shippable null, statement_descriptor null, tax_code null, type "service", unit_label null, updated, url null`. `updated` is the Unix seconds of the last mutation.
    - **Price:** `id, object "price", active, billing_scheme "per_unit", created, currency, custom_unit_amount null, livemode false, lookup_key null, metadata, nickname, product (id), recurring ({interval, interval_count 1, meter null, usage_type "licensed"} | null), tax_behavior "unspecified", tiers_mode null, transform_quantity null, type ("recurring"|"one_time"), unit_amount, unit_amount_decimal (string of unit_amount)`.
  - **`catalog_change`** (stream `"generate.catalog.change"`) picks one of:
    - 50%: a new product with 1–3 prices. At least one price is recurring; currencies come from one draw (`usd` 70%, `eur` 20%, `jpy` 10%). `unit_amount` is drawn from {0, 99, 500, 1999, 99_999_999}, multiplied by 1 for jpy. The product's `default_price` is the first price, set by a product update in the same action.
    - 30%: a new price for a random active product.
    - 20%: archive a random active price, or if none, a random active product.
  - **Churn:** `churn.product.update` (name/description/metadata; `product.updated`), `churn.product.archive` (`ChangeKind::Archived`, `product.updated`) and the same for `churn.price`. Price updates change only `nickname` or `metadata`, never amounts. Streams are `"generate.product.update"`, `"generate.product.archive"`, `"generate.price.update"` and `"generate.price.archive"`.
  - Existing action specs keep their order and stream names. New actions are appended, so customer draws are unchanged.

- [ ] **Step 1: Write failing tests in `tests/catalog.rs`**

```rust
#[tokio::test] async fn catalog_change_creates_products_and_prices() {
    let m = mock_with("seed = 3").await;
    for _ in 0..20 { m.run_action(Action::CatalogChange).unwrap(); }
    let log = m.truth_log();
    assert!(log.iter().any(|u| u.object_type == ObjectType::Product && u.kind == ChangeKind::Created));
    let prices: Vec<_> = log.iter().filter(|u| u.object_type == ObjectType::Price).collect();
    assert!(!prices.is_empty());
    for p in &prices { assert!(p.state["product"].as_str().unwrap().starts_with("prod_")); }
    assert!(log.iter().any(|u| u.kind == ChangeKind::Archived), "20 changes at seed 3 archive at least once");
}
#[tokio::test] async fn one_action_shares_a_cause() {
    let m = mock_with("").await;
    let cause = m.run_action(Action::CatalogChange).unwrap();
    let mine: Vec<_> = m.truth_log().into_iter().filter(|u| u.cause.as_ref() == Some(&cause)).collect();
    assert!(mine.len() >= 2);                                   // product + price(s) (+ default_price update)
    assert!(mine.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
}
#[tokio::test] async fn archived_price_event_has_previous_active() {
    let m = mock_with("seed = 5\n[churn.price]\narchive = 60.0").await;
    m.run_action(Action::CatalogChange).unwrap();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let u = m.truth_log().into_iter().find(|u| u.kind == ChangeKind::Archived).unwrap();
    assert_eq!(u.state["active"], false);
    let ev = m.event_timeline().into_iter().find(|e| e.id == u.events.snapshot[0]).unwrap();
    assert_eq!(ev.event_type, "price.updated");
}
#[tokio::test] async fn step_delay_spaces_follow_up_steps() {
    let m = mock_with("[actions]\nstep_delay = { distribution = \"fixed\", ms = 400 }").await;
    let cause = m.run_action(Action::CatalogChange).unwrap();
    let ats: Vec<_> = m.truth_log().into_iter().filter(|u| u.cause.as_ref() == Some(&cause)).map(|u| u.at).collect();
    m.step(Duration::from_secs(5)).await.unwrap();
    let all: Vec<_> = m.truth_log().into_iter().filter(|u| u.cause.as_ref() == Some(&cause)).map(|u| u.at).collect();
    assert!(all.len() > ats.len(), "later steps waited for step_delay");
    assert!(all.windows(2).all(|w| w[1].0 - w[0].0 == 400 || w[1].0 == w[0].0));
}
#[test] fn embedded_list_truncates() {   // unit test in model/embedded.rs
    let kids: Vec<Value> = (0..12).map(|i| json!({"id": i})).collect();
    let l = embedded_list(&kids, 10, "/v1/subscription_items?subscription=sub_1");
    assert_eq!((l["data"].as_array().unwrap().len(), l["has_more"].clone(), l["total_count"].clone()), (10, json!(true), json!(12)));
}
```

  In `step_delay_spaces_follow_up_steps`: the first step creates the product; the prices are the second step and the `default_price` update the third. `run_action` runs the first step and every continuation due at or before now, so with `step_delay = 400` it returns after the first step and the rest happen during `step`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test catalog`. Expected: compile failure.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: 5 tests pass, and the phase 1 golden-value and determinism tests are still green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): lifecycle queue, run_action and catalog`

---

### Task 4: Invoices and line items; `one_off_invoice`

**Files:**
- Create: `src/model/invoice.rs`, `src/generate/billing.rs`, `tests/invoices.rs`

**Interfaces:**
- Consumes: Task 3 (`Continuation`, `schedule`, `step_delay`, `embedded_list`), Task 2 (event lists).
- Produces:
  - **Invoice JSON:** `id, object "invoice", account_country "US", account_name "ductkit mock", amount_due, amount_paid, amount_remaining, attempt_count, attempted, auto_advance, billing_reason ("subscription_create"|"subscription_cycle"|"manual"), collection_method "charge_automatically", created, currency, customer, customer_email, customer_name, default_payment_method null, description null, discounts [], due_date null, effective_at (null until finalized), hosted_invoice_url null, invoice_pdf null, lines (embedded list, url "/v1/invoices/{id}/lines"), livemode false, metadata, next_payment_attempt null, number (null until finalized), parent (null | {"type": "subscription_details", "subscription_details": {"metadata": {}, "subscription": sub_id}, "quote_details": null}), period_end, period_start, status, status_transitions {finalized_at, marked_uncollectible_at, paid_at, voided_at}, subtotal, total`.
  - **Line JSON:** `id, object "line_item", amount, currency, description, discountable true, discounts [], invoice (id), livemode false, metadata {}, parent ({"type": "subscription_item_details", "subscription_item_details": {"invoice_item": null, "proration": false, "proration_details": {"credited_items": null}, "subscription": sub_id, "subscription_item": si_id}, "invoice_item_details": null} | {"type": "invoice_item_details", "invoice_item_details": {"invoice_item": "ii_…", "proration": false, "proration_details": {"credited_items": null}, "subscription": null}, "subscription_item_details": null}), period {start, end}, pricing {"type": "price_details", "price_details": {"price": price_id, "product": prod_id}, "unit_amount_decimal": "<unit_amount>"}, quantity`.
    - `amount = unit_amount × quantity`.
    - Invoice `subtotal = total = amount_due = Σ amount` and `amount_remaining = amount_due − amount_paid`.
  - **`pub(crate) fn create_draft_invoice(engine, gen, customer: &str, lines: &[LineSpec], reason: BillingReason, parent: Option<&str>, period: (i64, i64), cause: &Cause) -> String`**:
    - Creates each line first (unannounced, `ObjectType::InvoiceLineItem`), then the invoice (`invoice.created`) with its lines embedded.
    - `pub(crate) struct LineSpec { price: String, quantity: u64, subscription_item: Option<String>, period: (i64, i64) }` and `pub(crate) enum BillingReason { SubscriptionCreate, SubscriptionCycle, Manual }` (serialized `subscription_create`, `subscription_cycle`, `manual`).
  - **Transitions** (each is one engine mutation; kind `StateChanged` unless noted):
    - `finalize`:
      - status `open`, `effective_at` and `status_transitions.finalized_at` set to now;
      - `number` from the customer, plus an unannounced customer `next_invoice_sequence` bump;
      - announces `invoice.finalized`.
    - `pay`: status `paid`, `amount_paid = total`, `amount_remaining = 0`, `attempted: true`, `attempt_count += 1`, `paid_at`. Announces `invoice.paid` and `invoice.payment_succeeded`.
    - `fail_payment` (kind `Updated`): `attempted: true`, `attempt_count += 1`. Announces `invoice.payment_failed`.
    - `void`: `voided_at`; announces `invoice.voided`.
    - `mark_uncollectible`: `marked_uncollectible_at`; announces `invoice.marked_uncollectible`.
    - `delete_draft`: deletes the lines (tombstone `{id, object: "line_item", deleted: true}`, unannounced), then the invoice with tombstone `{id, object: "invoice", deleted: true}` and `invoice.deleted`.
    - Illegal transitions `debug_assert!`.
  - **`one_off_invoice`** (stream `"generate.invoice.one_off"`):
    - Picks a live customer (or creates one) and 1–3 active prices of one currency. A one-time price is preferred; if no active price exists, it creates a catalog product first.
    - The outcome is the action's `outcome`, or drawn when `None`. Each step is then scheduled `step_delay` apart:
      1. draft;
      2. with 10%, delete the draft and stop;
      3. finalize;
      4. pay attempt (fails with `payment_failure_probability`; a failed one-off is then voided);
      5. with 10% each, instead of paying: void, or mark uncollectible.
    - Continuation variants: `FinalizeInvoice{id}`, `SettleOneOff{id}`, `DeleteDraft{id}`.
  - `churn.invoice.update`: metadata on a random non-deleted invoice; announces `invoice.updated` (stream `"generate.invoice.update"`).

- [ ] **Step 1: Write failing tests in `tests/invoices.rs`**
  - `one_off_invoice_reaches_an_outcome`:
    - Run `Action::OneOffInvoice { customer: None, outcome: None }` 40 times at seed 4.
    - Every invoice's latest truth state is either deleted (draft delete) or has status in {`paid`, `void`, `uncollectible`}.
    - All four outcomes occur.
    - Every `paid` invoice has `amount_paid == total`, and `total == Σ lines.data[].amount` (use invoices with ≤ 10 lines).
  - `finalize_numbers_from_customer`:
    - Two one-off invoices for the same customer (via `customer: Some(id)`, `outcome: Some(Paid)`) get numbers `"{prefix}-0001"` and `"{prefix}-0002"`.
    - The customer's latest `next_invoice_sequence` is 3.
    - The customer truth entries that bumped it have empty `events`.
  - `paid_announces_two_events`: the truth entry with `state.status == "paid"` has `events.snapshot.len() == 2`, with timeline types `invoice.paid` and `invoice.payment_succeeded`.
  - `deleted_draft_takes_its_lines`: for a draft-deleted invoice, every line in its last pre-delete state has a `ChangeKind::Deleted` truth entry with no events. The invoice's delete entry announces `invoice.deleted`.
  - `line_shape`:
    - `lines.data[0]["object"] == "line_item"`;
    - `pricing.price_details.price` starts with `price_`;
    - a one-off line's `parent.type == "invoice_item_details"`;
    - `invoice.parent` is `null`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test invoices`. Expected: fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: 5 pass, full suite green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): invoices, line items and one-off invoices`

---

### Task 5: Subscriptions, items and `start_subscription`

**Files:**
- Create: `src/model/subscription.rs`, `tests/subscriptions.rs`
- Modify: `src/generate/billing.rs`

**Interfaces:**
- Consumes: Task 4 (`create_draft_invoice`, `finalize`, `pay`, `fail_payment`).
- Produces:
  - **Subscription JSON:** `id, object "subscription", application null, automatic_tax {"enabled": false, "liability": null}, billing_cycle_anchor, cancel_at null, cancel_at_period_end false, canceled_at null, cancellation_details {"comment": null, "feedback": null, "reason": null}, collection_method "charge_automatically", created, currency, customer, days_until_due null, default_payment_method null, description null, discounts [], ended_at null, items (embedded list, url "/v1/subscription_items?subscription={id}"), latest_invoice (id|null), livemode false, metadata, pending_update null, start_date, status, test_clock null, trial_end null, trial_start null`.
  - **Item JSON:** `id, object "subscription_item", created, current_period_end, current_period_start, discounts [], metadata {}, price (full price state at item creation), quantity, subscription (id), tax_rates []`.
  - **`pub(crate) fn period_end(start: i64, interval: &str) -> i64`**: adds 1 or 12 calendar months with chrono `Months`, clamped to the month's end. 2026-01-31 + 1 month = 2026-02-28.
  - **`start_subscription`** (stream `"generate.subscription.start"`):
    - Customer: the `Action` customer, or a random live one, or a new one.
    - Prices: 1–3 active recurring prices sharing currency and interval (0.05 chance of 11–15, which may reuse prices with distinct quantities); quantities 1–5.
    - Steps, each `step_delay` apart:
      1. Create the items (unannounced), then the subscription with status `incomplete` (`customer.subscription.created`). The subscription is created **after** its items so it can embed them; both carry the same `created`.
      2. `create_draft_invoice` (`subscription_create`; one line per item; period = the item period), then set `latest_invoice` on the subscription (`customer.subscription.updated`).
      3. `finalize`.
      4. The pay attempt:
         - **succeeds:** `pay`, then the subscription goes `active` (`StateChanged`, `customer.subscription.updated`), and a `Renew{sub}` continuation is scheduled at `current_period_end`;
         - **fails** (`payment_failure_probability`): `fail_payment`, then an `ExpireIncomplete{sub, invoice}` continuation is scheduled at +23h.
  - `ExpireIncomplete`: `void` the invoice, then the subscription goes `incomplete_expired` with `ended_at` (`customer.subscription.deleted`).
  - Continuation variants are added: `SubscriptionInvoice{sub}`, `FinalizeInvoice` (reused), `PayFirstInvoice{sub, invoice}`, `ExpireIncomplete{..}`, `Renew{sub}` (implemented in Task 6).

- [ ] **Step 1: Write failing tests in `tests/subscriptions.rs`**

```rust
#[tokio::test] async fn start_subscription_fans_out_in_order() {        // Review Focus 4
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let cause = m.run_action(Action::StartSubscription { customer: None, prices: None }).unwrap();
    let mine: Vec<_> = m.truth_log().into_iter().filter(|u| u.cause.as_ref() == Some(&cause)).collect();
    assert!(mine.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
    let types: Vec<String> = mine.iter().flat_map(|u| u.events.snapshot.clone())
        .map(|id| m.event_timeline().into_iter().find(|e| e.id == id).unwrap().event_type).collect();
    let expected_prefix = ["customer.created", "product.created", "price.created"];      // prerequisites, same cause
    let core = ["customer.subscription.created", "invoice.created", "customer.subscription.updated",
                "invoice.finalized", "invoice.paid", "invoice.payment_succeeded", "customer.subscription.updated"];
    let start = types.iter().position(|t| t == "customer.subscription.created").unwrap();
    assert_eq!(&types[start..], &core);
    assert!(types[..start].iter().all(|t| expected_prefix.contains(&t.as_str()) || t == "product.updated"));
    m.step(Duration::from_millis(1)).await.unwrap();
    let got: Vec<String> = rx.wait_for(types.len(), Duration::from_secs(5)).await.iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["type"].as_str().unwrap().to_string()).collect();
    assert_eq!(got, types, "one destination receives the fan-out in truth order");
    let sub = mine.iter().rev().find(|u| u.object_type == ObjectType::Subscription).unwrap();
    assert_eq!(sub.state["status"], "active");
    assert!(sub.state["latest_invoice"].as_str().unwrap().starts_with("in_"));
}
#[tokio::test] async fn many_items_are_truncated() {                        // Review Focus 1 (shape)
    let m = mock_with("[payloads]\nnested_list_page_size = 2").await;
    let prices = recurring_prices(&m, 3, "month");
    m.run_action(Action::StartSubscription { customer: None, prices: Some(prices) }).unwrap();
    let sub = m.truth_log().into_iter().rev().find(|u| u.object_type == ObjectType::Subscription).unwrap();
    let items = &sub.state["items"];
    assert_eq!((items["data"].as_array().unwrap().len(), items["has_more"].clone(), items["total_count"].clone()), (2, json!(true), json!(3)));
    let inv = m.get(ObjectType::Invoice, sub.state["latest_invoice"].as_str().unwrap()).unwrap();
    assert_eq!(inv.state["lines"]["data"].as_array().unwrap().len(), 2);
    assert_eq!(inv.state["lines"]["total_count"], 3);
}
#[tokio::test] async fn failed_first_payment_expires_after_23h() {
    let m = mock_with("[actions]\npayment_failure_probability = 1.0").await;
    let cause = m.run_action(Action::StartSubscription { customer: None, prices: None }).unwrap();
    let sub_id = m.truth_log().into_iter().find(|u| u.object_type == ObjectType::Subscription).unwrap().object_id;
    assert_eq!(m.get(ObjectType::Subscription, &sub_id).unwrap().state["status"], "incomplete");
    m.step(Duration::from_secs(23 * 3600)).await.unwrap();
    let s = m.get(ObjectType::Subscription, &sub_id).unwrap();
    assert_eq!((s.state["status"].clone(), s.kind), (json!("incomplete_expired"), ChangeKind::StateChanged));
    let inv = m.get(ObjectType::Invoice, s.state["latest_invoice"].as_str().unwrap()).unwrap();
    assert_eq!(inv.state["status"], "void");
    assert_eq!(s.cause.unwrap(), cause);
}
#[test] fn month_end_clamps() { assert_eq!(period_end(1_769_817_600 /* 2026-01-31 */, "month"), 1_772_236_800 /* 2026-02-28 */); }
```

  `recurring_prices` goes in `tests/common/mod.rs` so Tasks 6–9 reuse it: `pub fn recurring_prices(m: &MockStripe, n: usize, interval: &str) -> Vec<String>`. It calls `run_action(Action::CatalogChange)` until the truth log holds `n` active recurring prices with the same currency and the given interval, then returns their IDs. Bound it to 200 iterations, then panic.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test subscriptions` (and `--lib -- subscription`). Expected: fail.
- [ ] **Step 3: Implement.** `period_end` is a `pub(crate)` unit-tested function; put `month_end_clamps` in `model/subscription.rs`.
- [ ] **Step 4: Run.** Expected: all pass, full suite green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): subscriptions, items and start_subscription`

---

### Task 6: Renewals, cancellation, dunning, cascades, churn

**Files:**
- Create: `tests/billing_cycle.rs`
- Modify: `src/generate/billing.rs`, `src/generate/mod.rs`, `src/handle.rs`

**Interfaces:**
- Consumes: Tasks 4–5.
- Produces:
  - **`Renew{sub}`** at `current_period_end` (only when `renew_subscription_cycle = "natural"`, and only if the subscription is `active` or `past_due`):
    - **`cancel_at_period_end` is true:** status `canceled`, `canceled_at` = `ended_at` = now; announces `customer.subscription.deleted`. Stop.
    - **Otherwise:**
      1. Each item gets its next period, an unannounced item update.
      2. The subscription's `items` are re-rendered (`customer.subscription.updated`).
      3. `create_draft_invoice` (`subscription_cycle`, the new period), then `latest_invoice` is updated (`customer.subscription.updated`).
      4. `finalize`, then a pay attempt. Each step is `step_delay` apart.
      5. Success: `pay`, then schedule the next `Renew`.
      6. Failure: `fail_payment`, the subscription goes `past_due` (`customer.subscription.updated`), and `DunningRetry{sub, invoice}` is scheduled at +3 days.
  - **`DunningRetry`:**
    - success: `pay`, the subscription goes `active`, and the next `Renew` stays at the period end already set;
    - failure: `mark_uncollectible`, then the subscription goes `canceled` (`customer.subscription.deleted`).
  - **Renewal billing:** an item's price is billed from its embedded price at renewal, even if that price was archived since.
  - **`cancel_subscription`** (stream `"generate.subscription.cancel"`) applies to an `Action`-named or random subscription that is `active` or `past_due`. With `at_period_end` (drawn 50/50 when `None`):
    - **true:** sets `cancel_at_period_end: true` and `cancel_at = current_period_end` of the first item (`customer.subscription.updated`);
    - **false:** cancels immediately (`StateChanged`, `customer.subscription.deleted`).
  - Pending continuations for a canceled subscription become no-ops.
  - **Customer delete cascade:** `churn.customer.delete` and `MockStripe::delete_customer` first cancel every `active`/`past_due`/`incomplete` subscription of the customer immediately (incomplete ones go to `incomplete_expired`) and void its `open` invoices, all within the same action and before the customer delete.
  - `pub fn MockStripe::update_actions_config(&self, f: impl FnOnce(&mut ActionsConfig)) -> Result<(), MockError>`: validated like the config file; affects later draws only.
  - **Churn:** `churn.subscription.update` changes metadata on a random non-ended subscription (`customer.subscription.updated`; stream `"generate.subscription.update"`).

- [ ] **Step 1: Write failing tests in `tests/billing_cycle.rs`**
  - `monthly_subscription_renews_each_period`:
    - A subscription on one monthly price (`recurring_prices(&m, 1, "month")`) is started at 2026-01-01.
    - `step` 95 days.
    - The subscription has 4 paid invoices with `billing_reason` `subscription_create`, then 3× `subscription_cycle`.
    - Consecutive invoices' `period_start` equal the previous `period_end`.
    - The item's `current_period_start` is the last invoice's `period_start`.
  - `archived_price_is_still_billed` (Review Focus 5):
    - Start a subscription on price P, then `run_action(Action::Archive { id: P })`.
    - Step 32 days.
    - The renewal invoice's line `pricing.price_details.price == P`.
    - 20 later `StartSubscription { prices: None }` runs never pick P (no item with `price.id == P` among subscriptions created after the archive).
  - `cancel_at_period_end_then_ends`:
    - `run_action(CancelSubscription { id: Some(sub), at_period_end: Some(true) })`;
    - the status stays `active` with `cancel_at` set;
    - after stepping past the period end, the status is `canceled`, no `subscription_cycle` invoice exists, and the last event is `customer.subscription.deleted`.
  - `immediate_cancel_stops_renewal`: after cancelling immediately, step 40 days; there is no new invoice for the subscription and its status is `canceled`.
  - `past_due_then_recovers_or_ends`:
    - Start a subscription (failure probability 0), then `update_actions_config(|a| a.payment_failure_probability = 1.0)`.
    - Step past the period end: the subscription is `past_due` and the renewal invoice `open` with `attempt_count` 1.
    - Step 3 more days: the subscription is `canceled` and the invoice `uncollectible`.
  - `customer_delete_cancels_subscriptions_first` (Review Focus 2):
    - Start a subscription, then call `delete_customer`.
    - In that action's truth entries, every subscription `StateChanged` to `canceled` precedes the customer `Deleted` entry.
    - `get(Subscription)` is still `Some` with status `canceled` and `customer` = the deleted ID.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test billing_cycle`. Expected: fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: 6 pass, full suite green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): renewals, cancellation, dunning and delete cascade`

---

### Task 7: API for every type, filters and sub-lists

**Files:**
- Create: `src/api/resources.rs`, `tests/api_billing.rs`
- Delete: `src/api/customers.rs` (`decode` moves to `api/list.rs`; update imports in `events_v1.rs`/`events_v2.rs`)
- Modify: `src/api/mod.rs`, `src/api/list.rs`

**Interfaces:**
- Produces:
  - `pub(crate) struct Resource { t: ObjectType, path: &'static str, filters: &'static [&'static str], order: Order }` with `enum Order { CreatedDesc, CreationAsc }`.
  - One generic `retrieve` and one generic `list` handler, parameterized by `Resource`.
  - **Routes and filters** (each filter is an exact match on the state field unless noted):
    - `/v1/customers` (`email`)
    - `/v1/products` (`active` boolean)
    - `/v1/prices` (`active`, `product`, `currency`, `type`)
    - `/v1/subscriptions` (`customer`, `price` = any item's `price.id`, `status` incl. `all`; the default excludes `canceled` and `incomplete_expired`)
    - `/v1/subscription_items`: `subscription` is required; missing → 400 `parameter_missing`, param `subscription`. Order is `CreationAsc` and deleted items are excluded.
    - `/v1/invoices` (`customer`, `subscription` = `parent.subscription_details.subscription`, `status`)
    - `/v1/invoices/{id}/lines`: lines of a live invoice, `CreationAsc`; a deleted or unknown invoice → 404 `resource_missing`.
    - Retrieve for every type except line items. A deleted customer returns its stub. A deleted invoice returns 404 `resource_missing` with message `No such invoice: '{id}'`.
  - `created` filters apply to products, prices, subscriptions and invoices, as in phase 1.
  - Boolean filters accept `true`/`false`; anything else → 400 `parameter_invalid_boolean`, as in phase 2.
- **Ordering note:** `CreationAsc` must use the store's `creation_index` ascending, and `paginate` works on any ordered slice. Add `ObjectStore::children(t, parent_field, parent_id) -> Vec<&StoredObject>` in creation order, which filters on `state[parent_field] == parent_id`.

- [ ] **Step 1: Write failing tests in `tests/api_billing.rs`**
  - `every_type_lists_and_retrieves`:
    - After one `StartSubscription` and one `OneOffInvoice` (seed 6), each of `/v1/products`, `/v1/prices`, `/v1/subscriptions`, `/v1/invoices` and `/v1/customers` returns `object: "list"` with ≥ 1 item.
    - Retrieving `data[0].id` from each returns an equal object.
  - `sub_list_pages_through_all_items` (Review Focus 1):
    - With `nested_list_page_size = 2` and a 5-item subscription (via `recurring_prices`), `GET /v1/subscription_items?subscription=…&limit=2` followed by `starting_after` paging returns the 5 item IDs exactly once.
    - The first page equals `subscription.items.data` IDs.
    - The same holds for `/v1/invoices/{latest}/lines`.
  - `subscription_items_requires_subscription`: 400, `error.code == "parameter_missing"`, `error.param == "subscription"`.
  - `canceled_hidden_by_default`: after cancelling one of two subscriptions immediately, `/v1/subscriptions` has 1 and `?status=all` has 2.
  - `active_filter_hides_archived_prices` (Review Focus 5): after archiving a price, `/v1/prices` includes it and `/v1/prices?active=true` does not.
  - `thin_related_object_fetch` (Review Focus 3):
    - Under a thin destination with `enabled_events = ["v1.invoice.*", "v1.customer.subscription.*"]`, run `OneOffInvoice { customer: None, outcome: Some(DeleteDraft) }`.
    - `GET` its `v1.invoice.created` notification's `related_object.url` returns 404 `resource_missing`.
    - For a `v1.customer.subscription.updated` notification, the fetch returns the subscription's latest truth state.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test api_billing`. Expected: fail.
- [ ] **Step 3: Implement.** Keep the phase 1 customer tests passing unchanged; they pin unknown-parameter errors and the `email` filter.
- [ ] **Step 4: Run.** Expected: 6 pass, full suite green (including `api_customers`, `events_v1` and `events_v2`).
- [ ] **Step 5: Commit:** `feat(stripe-mock): API for billing types, filters and sub-lists`

---

### Task 8: `expand[]`

**Files:**
- Create: `src/api/expand.rs`, `tests/expand.rs`
- Modify: `src/api/resources.rs`, `src/api/list.rs` (accept `expand[]`/`expand[N]` as a multi parameter on every resource)

**Interfaces:**
- Produces:
  - **`pub(crate) fn expand(engine: &Engine, t: ObjectType, obj: &mut Value, paths: &[String]) -> Result<(), ApiError>`**:
    - Each path is split on `.`. A `data` segment over a list value applies the rest of the path to every element.
    - The final segment's string ID is replaced by that object's latest state, looked up by its ID prefix:
      - deleted customer → stub;
      - deleted invoice → `null`;
      - unknown ID → left as the string.
    - At most 4 levels.
  - **Allowed paths per type**, from the plan decisions:
    - subscription: `customer`, `latest_invoice`, `items.data.price.product`
    - invoice: `customer`, `parent.subscription_details.subscription`
    - price: `product`
    - subscription_item: `price.product`

    Lists accept the same paths prefixed with `data.`; a list path without the prefix is an error, as in Stripe.
  - **Errors:**
    - A disallowed path → 400 `invalid_request_error`, message `This property cannot be expanded ({path}).`, param `expand`.
    - A 5-level path → 400 with message `You cannot expand more than 4 levels of a property.`

- [ ] **Step 1: Write failing tests in `tests/expand.rs`**
  - `retrieve_expands_customer_and_invoice`: `GET /v1/subscriptions/{id}?expand[]=customer&expand[]=latest_invoice` has `customer.object == "customer"` and `latest_invoice.object == "invoice"`; without `expand`, both are strings.
  - `list_expands_with_data_prefix`: `GET /v1/prices?expand[]=data.product` has every `data[].product.object == "product"`. `?expand[]=product` on the list → 400.
  - `nested_items_price_product`: `?expand[]=items.data.price.product` makes `items.data[0].price.product` an object.
  - `deleted_customer_expands_to_stub` (Review Focus 2): after `delete_customer`, the canceled subscription with `expand[]=customer` has `customer == {"id", "object": "customer", "deleted": true}`.
  - `bad_expand_is_400`: `expand[]=status` → 400 with message `This property cannot be expanded (status).`
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test expand`. Expected: fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: 5 pass, full suite green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): expand[] for billing relations`

---

### Task 9: End-to-end determinism, quiesce with continuations, spec updates

**Files:**
- Modify: `tests/subscriptions.rs` (append), `tests/quiesce.rs` (append), `docs/stripe-mock-spec.md`

- [ ] **Step 1: Write the tests**
  - `billing_run_is_deterministic`:
    - Two mocks with `seed = 12` and `[actions]` set to `new_customer = 10.0`, `start_subscription = 4.0`, `cancel_subscription = 0.5`, `catalog_change = 0.2`, `one_off_invoice = 2.0`, `step_delay = { distribution = "lognormal", median_ms = 200, p99_ms = 2000 }`, `payment_failure_probability = 0.1`, plus `[churn.product]`, `[churn.price]`, `[churn.subscription]` and `[churn.invoice]` rates of 0.5 each.
    - Each steps 40 days.
    - Their truth logs are equal (compare `serde_json::to_value` of the whole log), and the log contains every object type and both `Archived` and `StateChanged` kinds.
  - `quiesce_pauses_continuations`:
    - `step_delay` is fixed 60000. `run_action(StartSubscription)`, then `quiesce` immediately.
    - The subscription stays `incomplete`, with no invoice finalized.
    - `step` 1h: the truth log is unchanged.
    - `resume_generation()` then `step` 10 minutes: the subscription is `active`.
- [ ] **Step 2: Run** them. Expected: they pass if Tasks 3–6 were complete. Otherwise fix the gap and record a ruling.
- [ ] **Step 3: Update the spec:**
  - Section 4: add the phase 3 `[actions]` keys (`one_off_invoice`, `step_delay`, `payment_failure_probability`) and `[churn.price]`, `[churn.subscription]` and `[churn.invoice]` to the example config.
  - Section 5.2: add "Phase 3 generates `incomplete → active | incomplete_expired`, `active ⇄ past_due`, `active | past_due → canceled`; `trialing` and `paused` are not generated yet."
  - Section 5.4: `other_accounts` → `@accounts` (deferred phase 2 minor).
  - Section 6.1: add "Until payment objects exist (phase 7), paying an invoice is one mutation announcing `invoice.paid` and `invoice.payment_succeeded`."
  - Section 7.3: add "Sub-lists list in creation order, ascending, so their first page equals the embedded list."
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: all green.
- [ ] **Step 5: Commit:** `test(stripe-mock): billing determinism and quiesce; spec updates`
