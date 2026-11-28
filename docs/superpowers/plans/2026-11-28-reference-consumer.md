# Reference consumer harness: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One in-process test runs a naive consumer and a correct consumer against one faulty mock run. It proves the naive consumer ends up wrong in the expected ways, and the correct one ends up exactly equal to the truth.

**Architecture:** A new multi-file test target `tests/reference_consumer/`. It has:
- a truth model built from the truth log,
- a fault-tolerant API client,
- a webhook server with a deterministic failure policy,
- the two consumers,
- a driver that steps the virtual clock and gives each consumer a turn.

No change to the mock's library code.

**Tech Stack:** Rust 2024, tokio, axum (webhook server), reqwest, serde_json; the `ductkit_stripe_mock` public API.

**Spec:** `docs/superpowers/specs/2026-11-28-reference-consumer-design.md`. Mock behaviour: `docs/stripe-mock-spec.md`, sections 7.3, 7.4, 8, 9 and 10.

## Global Constraints

- Tests only. Do not change `crates/stripe-mock/src/`.
- **Every command uses `-j 4`.** Higher job counts crash rustc 1.97.1 on this machine (an internal compiler error while reading the crate's metadata). Example: `cargo test -p ductkit-stripe-mock --test reference_consumer -j 4`.
- `cargo fmt --all` and `cargo clippy -p ductkit-stripe-mock --all-targets -j 4 -- -D warnings` must be clean before each commit.
- Every 2xx JSON body the harness receives from the Stripe API is checked with `common::assert_conforms`.
- The whole target runs in under 20 s of wall time.
- Commits go on the branch `test/reference-consumer`. Messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Decisions taken while planning

These refine the design. Each one is also recorded in the design doc by Task 5.

- **Location.** The harness lives in `tests/reference_consumer/main.rs` plus sibling modules, not in `tests/common/consumer.rs`. Putting it in `common` would compile it into all 52 test targets. `main.rs` pulls in the shared helpers with `#[path = "../common/mod.rs"] mod common;`.
- **The naive consumer's asserted failures are *stale* and *resurrected*, not *stale* and *missing*.** A failed first attempt is always retried, so the naive consumer eventually receives every event and never misses an object. What retries cause is an outdated snapshot applied last (stale), and an update retried after its object's delete (resurrected).
- **The `timeout` API fault stays off.** It holds a response for 80 s of real time. `reset`, 500, 503 and 429 are on.
- **Subscriptions are backfilled with `status=all`.** Without it the mock's list hides canceled and `incomplete_expired` ones (`src/api/resources.rs:315-321`).

## Review Focus

1. **A served object that matches no truth version.** For example an embedded list truncated differently in REST and in truth, or a field set only in one of them. Neither consumer may hold such an entry (`other == 0`). Task 2's test pins this for every compared type.
2. **A connected account's thin event.** Its fetch must carry `Stripe-Context: <context>`. Without the header it reads the platform scope and gets a 404, which the consumer would wrongly treat as a delete. Task 4's test pins this by requiring connected-scope objects in the converged state.
3. **A connection reset mid-backfill.** The client must retry the same page, not skip it or panic. `common::get` panics on a send error, so the harness's client must not use it. Task 2 pins this with `reset` on.
4. **A webhook arriving while a consumer is mid-turn.** Each consumer takes the accepted bodies after its own cursor, so nothing is applied twice by accident or dropped. Task 3 pins this.
5. **Thin fetch returns a deleted stub vs a 404.** A deleted customer returns `{id, object, deleted: true}`. A type with no stub may 404. Both mean "remove". Task 4 pins this with customer deletes on.

---

### Task 1: Truth model and divergence classifier

**Files:**
- Create: `crates/stripe-mock/tests/reference_consumer/main.rs` (module declarations and tests)
- Create: `crates/stripe-mock/tests/reference_consumer/truth.rs`

**Interfaces:**
- Produces, in `truth.rs`:
  - `pub type Key = (Option<String>, ObjectType, String)`: (scope, type, id).
  - `pub type Synced = BTreeMap<Key, Value>`: what a consumer holds.
  - `pub const COMPARED: [ObjectType; 5] = [Customer, Product, Price, Subscription, Invoice]`
  - `pub struct Truth { pub latest: Synced, history: BTreeMap<Key, Vec<Value>>, deleted: BTreeSet<Key> }`
  - `impl Truth { pub fn from_log(log: &[StateUpdate]) -> Truth }`: only `COMPARED` types. `latest` holds live objects only. `history` holds every non-tombstone state, in seq order. `deleted` holds keys whose last entry is `ChangeKind::Deleted`.
  - `#[derive(Debug, Default, PartialEq)] pub struct Divergence { pub equal: usize, pub stale: usize, pub missing: usize, pub resurrected: usize, pub other: Vec<Key> }`
  - `impl Truth { pub fn classify(&self, synced: &Synced) -> Divergence }`. For each key:
    - in both and equal to `latest`: `equal`;
    - in both but equal to an earlier state in `history`: `stale`;
    - in `latest` but not in `synced`: `missing`;
    - in `synced`, in `deleted`, and equal to some earlier state: `resurrected`;
    - anything else, including a key truth never had: pushed to `other`.
  - `impl Divergence { pub fn converged(&self) -> bool }`: true when `stale == missing == resurrected == 0` and `other` is empty.

- [ ] **Step 1: Write the failing test** in `main.rs`: `classify_counts_each_kind`.
  - Build a `Vec<StateUpdate>` by hand, using `serde_json::from_value` with the field names in `src/engine/mod.rs:61-79`:
    - `cus_a` with v1 then v2;
    - `cus_b` with v1 then a `Deleted` entry;
    - `cus_c` with v1.
  - Synced holds: `cus_a` = v1, `cus_b` = v1, `cus_x` = `{}`, and no `cus_c`.
  - Assert `Divergence { equal: 0, stale: 1, missing: 1, resurrected: 1, other: vec![(None, Customer, "cus_x".into())] }`.
  - Assert that `classify(&truth.latest)` is `converged()`.
- [ ] **Step 2: Run it and check that it fails.** Run `cargo test -p ductkit-stripe-mock --test reference_consumer -j 4`. Expect a compile error: `truth` module not found.
- [ ] **Step 3: Implement `truth.rs`** as specified above. `main.rs` starts with `#[path = "../common/mod.rs"] mod common;` and `mod truth;`.
- [ ] **Step 4: Run the test and check that it passes.** Same command; `classify_counts_each_kind` passes.
- [ ] **Step 5: Commit.** `test(stripe-mock): truth model and divergence classifier for the reference consumer`

### Task 2: Fault-tolerant API client and backfill

**Files:**
- Create: `crates/stripe-mock/tests/reference_consumer/client.rs`
- Modify: `crates/stripe-mock/tests/reference_consumer/main.rs` (add `mod client;` and a test)

**Interfaces:**
- Consumes: `truth::{Key, Synced, Truth, COMPARED}`.
- Produces, in `client.rs`:
  - `pub struct Client { http: reqwest::Client, base: String }`
  - `impl Client { pub fn new(m: &MockStripe) -> Client }`: uses key `sk_test_mock` and a per-request timeout of 5 s.
  - `pub enum Fetched { Object(Value), Gone }`. `Gone` covers a 404 or a body with `deleted: true`.
  - `pub async fn get(&self, path: &str, scope: Option<&str>) -> Fetched`
    - Retries on 429, 5xx and send errors (reset), up to 20 tries, with 10 ms backoff.
    - Panics on any other status, naming the path.
    - Runs `common::assert_conforms` on every 2xx body.
    - Sends `Stripe-Context: <scope>` when `scope` is `Some`.
  - `pub fn list_path(t: ObjectType) -> &'static str`: customers, products, prices, `subscriptions?status=all`, invoices.
  - `pub async fn backfill(&self, scopes: &[Option<String>]) -> Synced`
    - Walks every `COMPARED` list in every scope with `limit=100` and `starting_after`, using `&` when the path already has `?`.
    - Inserts each object under `(scope, t, id)`.
    - A later page's copy of an ID overwrites the earlier one.
  - `pub fn scopes(m: &MockStripe) -> Vec<Option<String>>`: `None` plus every connected account in `m.accounts()`.

- [ ] **Step 1: Write the failing test** `backfill_after_quiesce_matches_truth`.
  - Use `common::mock_with` with:
    - seed 7;
    - `[accounts] connect = true`, `connected_accounts = { initial = 2 }`;
    - `[history] duration = "2d"`;
    - the scenario's `[actions]` and `[churn.*]` rates (see `SCENARIO_TOML` in Task 4; define the const here and reuse it);
    - `[api] errors = { "500" = 0.1, "503" = 0.05, reset = 0.05 }` and `pagination_writes = { probability = 0.3 }`.
  - Step 6 h in 10 min steps, `quiesce`, `backfill`.
  - Assert `Truth::from_log(&m.truth_log()).classify(&synced).converged()`, printing the `Divergence` on failure.
  - Assert `synced` has at least one key in a connected scope for each of customer, subscription and invoice.
- [ ] **Step 2: Run it and check that it fails** (compile error: no `client` module).
- [ ] **Step 3: Implement `client.rs`** as specified above.
- [ ] **Step 4: Run it and check that it passes.**
  - **If it fails only through `other`:** a REST object differs from every truth state for some type. Find the field. Pick the side that follows the spec (section 7.3 for embedded lists, includable fields, expansion) and add `pub fn normalize(t: ObjectType, v: &mut Value)` in `truth.rs`. Apply it to both truth states and served objects, with a comment citing the spec line.
  - **If the difference is a mock bug** (the spec says REST equals truth), stop and report it instead of normalizing it away.
- [ ] **Step 5: Commit.** `test(stripe-mock): retrying client and backfill matches truth under API faults`

### Task 3: Webhook server with a deterministic failure policy

**Files:**
- Create: `crates/stripe-mock/tests/reference_consumer/hooks.rs`
- Modify: `crates/stripe-mock/tests/reference_consumer/main.rs`

**Interfaces:**
- Produces, in `hooks.rs`:
  - `pub const SECRET: &str = "whsec_ref"`
  - `pub fn fails_first(event_id: &str) -> bool`: FNV-1a 64 of the ID, `% 3 == 0`.
  - `pub struct HookServer` with:
    - `pub fn start() -> HookServer`, inside a tokio runtime;
    - `pub fn url(&self) -> String`;
    - `pub fn take_new(&self, cursor: &mut usize) -> Vec<Value>`.
  - The handler:
    - parses the body's `id`;
    - checks `Stripe-Signature` with `ductkit_stripe_mock::verify_signature(header, body, SECRET, 300, real_now())`, answering 400 if it fails;
    - answers 500 on the first attempt of an ID for which `fails_first` is true;
    - otherwise answers 200 and appends the parsed body to `accepted: Arc<Mutex<Vec<Value>>>`.
  - `take_new` returns `accepted[*cursor..]` and moves the cursor to the end.

- [ ] **Step 1: Write the failing test** `hook_server_fails_first_attempt_of_chosen_events`.
  - The mock has:
    - `new_customer = 20.0` for 1 h;
    - one snapshot destination to `HookServer::url()` with `secret = "whsec_ref"`;
    - `[webhooks] retry_schedule = ["30m"]`.
  - Step 1 h, then `quiesce`.
  - Assert every event in `m.event_timeline()` appears in `take_new` exactly once.
  - Assert that `delivery_log()` has a failed attempt for exactly the event IDs where `fails_first` is true, and that there is at least one such ID.
  - Assert that a second `take_new` with the same cursor is empty.
- [ ] **Step 2: Run it and check that it fails** (compile error).
- [ ] **Step 3: Implement `hooks.rs`**, modelled on `common::Receiver` (`tests/common/mod.rs:84-152`).
- [ ] **Step 4: Run it and check that it passes.**
- [ ] **Step 5: Commit.** `test(stripe-mock): webhook server that fails chosen first attempts`

### Task 4: Correct consumer, scenario driver and convergence test

**Files:**
- Create: `crates/stripe-mock/tests/reference_consumer/correct.rs`
- Create: `crates/stripe-mock/tests/reference_consumer/scenario.rs`
- Modify: `crates/stripe-mock/tests/reference_consumer/main.rs`

**Interfaces:**
- Consumes: `Client`, `Fetched`, `backfill`, `scopes`, `HookServer`, `SECRET`, `Truth`, `Synced`.
- Produces:
  - **In `correct.rs`:** `pub struct Correct { pub synced: Synced, hooks: HookServer, cursor: usize }` with:
    - `pub fn new(hooks: HookServer) -> Correct`;
    - `pub async fn backfill(&mut self, c: &Client, scopes: &[Option<String>])`, which replaces `synced`;
    - `pub async fn tick(&mut self, c: &Client)`. For each new thin event whose `related_object.type` is one of `COMPARED`, GET `related_object.url` with scope `event.context`. `Object(v)` inserts the object and `Gone` removes it. Other types are ignored.
    - `pub async fn reconcile(&mut self, c: &Client, scopes: &[Option<String>])`: a full backfill that replaces `synced`.
  - **In `scenario.rs`:**
    - `pub const SCENARIO_TOML: &str`. Use seed 7 and these settings:
      - **Accounts:** 2 initial connected accounts.
      - **History:** 2 days.
      - **Actions:** `new_customer = 6.0`, `start_subscription = 3.0`, `cancel_subscription = 0.5`, `catalog_change = 0.5`.
      - **Churn:** customer update 3.0, delete 0.5; product update 0.5; price update 0.5; subscription update 0.5; invoice update 0.5.
      - **Events:** `visibility_lag` lognormal (median 200 ms, p99 5000 ms), `late_tail = { probability = 0.05, min = "30s", max = "10m" }`, `same_second_shuffle = true`.
      - **Webhooks:** `duplicate_probability = 0.1`, `reorder_window = "5s"`, `retry_schedule = ["30m"]`.
      - **API:** `errors = { "500" = 0.05, "503" = 0.02, reset = 0.02 }`, `stale_read = { probability = 0.2, max_lag = "10m" }`, `pagination_writes = { probability = 0.2 }`.
      - Tune the rates only if the 20 s budget or Task 5's assertions need it.
    - `pub fn destinations(snapshot_url: &str, thin_url: &str) -> String`. It returns two `[[destinations]]` blocks with `secret = "whsec_ref"`: a snapshot one (`enabled_events = ["*"]`, `connect = true`), and a thin one (`enabled_events = ["*"]`, `events_from = ["@self", "@accounts"]`).
    - `pub async fn run(m: &MockStripe, mut each_tick: impl AsyncFnMut(&MockStripe))`. It runs 36 ticks of `m.step(10 min)` followed by `each_tick(m)`. Then it calls `quiesce`, then `m.step(max_lag + 1 min)`, which is legal because generation is off, so stale reads show the latest states. Then it calls `each_tick(m)` once more.
- Test in `main.rs`: `naive_diverges_and_correct_converges`.
  - It holds `m.replay_guard(env!("CARGO_TARGET_TMPDIR"))`.
  - It runs backfill first, then `scenario::run`, then `correct.reconcile`.
  - It asserts `truth.classify(&correct.synced).converged()`, printing the `Divergence` and the seed on failure.
  - It asserts `correct.synced` has keys in both connected scopes.

- [ ] **Step 1: Write the failing test** with the correct-consumer assertions only.
- [ ] **Step 2: Run it and check that it fails** (compile error).
- [ ] **Step 3: Implement `scenario.rs` and `correct.rs`** as specified above. Move `SCENARIO_TOML` from Task 2's test into `scenario.rs`.
- [ ] **Step 4: Run it and check that it passes.** Then run it without `correct.reconcile` and confirm it fails with `stale > 0`. That proves the reconcile step is needed. Restore the reconcile.
- [ ] **Step 5: Commit.** `test(stripe-mock): correct reference consumer converges under every fault`

### Task 5: Naive consumer and divergence assertions

**Files:**
- Create: `crates/stripe-mock/tests/reference_consumer/naive.rs`
- Modify: `crates/stripe-mock/tests/reference_consumer/main.rs`
- Modify: `docs/superpowers/specs/2026-11-28-reference-consumer-design.md` (record the planning decisions)

**Interfaces:**
- Consumes: `Client`, `HookServer`, `Synced`, `COMPARED`, `scopes`.
- Produces, in `naive.rs`: `pub struct Naive { pub synced: Synced, hooks: HookServer, cursor: usize, newest: BTreeMap<Option<String>, String> }` with:
  - `pub fn new(hooks: HookServer) -> Naive`;
  - `pub async fn backfill(&mut self, c: &Client, scopes: &[Option<String>])`;
  - `pub async fn tick(&mut self, c: &Client, scopes: &[Option<String>])`.

  `tick` does two things:
  - **Webhooks:** for each new snapshot event, it applies `data.object`. A `*.deleted` type removes the key; anything else upserts it. Only `COMPARED` types count, and the scope is the event's `account`.
  - **Polling:** for each scope, it polls `/v1/events?ending_before=<newest[scope]>&limit=100` (the first poll has no cursor), applies the events the same blind way, and sets `newest[scope]` to the first event's ID.

- [ ] **Step 1: Extend the test.**
  - Run `Naive` in the same run: snapshot destination for the naive consumer, thin destination for the correct one.
  - Let `d = truth.classify(&naive.synced)`. Assert `d.stale >= 1`, `d.resurrected >= 1` and `d.other.is_empty()`.
  - The failure message prints `d`.
- [ ] **Step 2: Run it.** If an assertion on `d` fails, change only the seed in `SCENARIO_TOML` until stale, resurrected and the correct consumer's convergence all hold. Record the seed used in a comment explaining why it was chosen.
- [ ] **Step 3: Check that the runtime stays under 20 s.** Run `cargo test -p ductkit-stripe-mock --test reference_consumer -j 4 -- --nocapture`.
- [ ] **Step 4: Update the design doc's Design section** with the four decisions under "Decisions taken while planning" in this plan.
- [ ] **Step 5: Run the full checks.** Run `cargo fmt --all`, `cargo clippy -p ductkit-stripe-mock --all-targets -j 4 -- -D warnings` and `cargo test -p ductkit-stripe-mock -j 4`. All pass.
- [ ] **Step 6: Commit.** `test(stripe-mock): naive reference consumer provably diverges`
