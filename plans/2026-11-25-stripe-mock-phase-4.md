# Stripe Mock Phase 4 (History, Backfill Races and API Faults) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mock can start with pre-generated history, some of it older than the event retention window, so a sync engine must backfill through list endpoints while live generation continues. The Stripe API surface gains the faults of spec section 7.4: rate limiting, random 5xx, timeouts, connection resets, latency, stale reads, and writes during pagination.

**Architecture:**
- **History is generation run early.** `MockStripe::start` puts the engine and generator at `clock.start - history.duration`, runs the generator with every rate multiplied by `history.scale` up to `clock.start`, then restores the configured rates. Only then are the config destinations added. History therefore has truth-log entries and events, but no webhook deliveries.
- **The store keeps indexes instead of scanning.** Live IDs per type, children by parent field, and the version positions of every object are maintained on write. Generation and list endpoints stay close to linear in the number of objects, which history at spec rates needs.
- **API faults live in one place.** A new `faults::api::ApiFaults` (under the sim lock) owns the runtime `[api]` config, the token bucket, the rate-limit storm and four RNG streams. The request gate asks it for one decision per request. Retrieve and list handlers ask it whether to read stale and whether to mutate before serving a page.
- **Stale reads are an "as of" view.** A read with lag `L` sees each object's latest version whose `at <= now - L`. The view is built from the store's version index into the truth log.

**Tech Stack:** The phase 3 stack. No new dependencies: the shutdown signal uses `tokio::sync::watch`, and broken responses use `axum::body::Body::from_stream` with `futures_util`.

**Spec:** [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md). Phase 4 is item 4 of section 16: "History and backfill races; API faults (rate limits, 5xx, stale reads, writes during pagination)". Sections 4 (`[history]`, `[api]`), 6.3, 7.1, 7.4, 11 (faults at runtime, rate-limit storm), 12 and 15 are the main references.

## Global Constraints

- **Rates** are per simulated hour (spec section 4). `history.scale` multiplies every action and churn rate during history only.
- **Determinism** (spec section 12): every decision comes from a named stream. The new streams are listed below; existing streams and their draw order must not change.
  - `"api.errors"`: exactly one `unit_f64` per request that passes the gate.
  - `"api.latency"`: exactly one `sample_ms` per request that passes the gate.
  - `"api.stale_reads"`: one `chance` per object retrieve or list request, plus one `uniform_ms` when it fires.
  - `"api.pagination_writes"`: one `chance` per object list request that carries a cursor (owned by `ApiFaults`).
  - `"api.pagination_writes.content"`: the kind choice and content draws of each write that fires (owned by the generator).

  API decisions are reproducible for the same request sequence; in wall mode, request timing is not (spec section 12).
- **Error envelope** (spec section 7.1): `{"error": {"type", "code"?, "message", "param"?}}`, plus a `Request-Id` header on every response, faulted ones included. `Stripe-Should-Retry: true` is set on 429, 500 and 503.
- **Faults never touch truth:** stale reads, errors and latency change only what a client observes (spec section 2). Writes during pagination are real mutations, recorded in the truth log like any other action.
- **The control surface is never faulted.** `MockStripe` methods read and change state directly.
- **Process:** `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` are clean before every commit. Dependency versions live only in the root `[workspace.dependencies]`. Commits end with the attribution footer.
- **Branching:** work on branch `stripe-mock-phase-4` in the existing checkout. Never use a worktree.
- **Windows:** edits must keep LF line endings (`docs/stripe-mock-spec.md` is parsed by a config test).

## Review Focus

1. **History subscriptions keep renewing after start.** A subscription started during history renews after `clock.start`, and those renewal events are delivered to destinations. Test `history_subscriptions_keep_renewing` (Task 2).
2. **History events never reach destinations, even through quiesce.** `quiesce()` right after start delivers nothing. Test `history_is_not_delivered` (Task 2).
3. **Faulted responses are still well-formed Stripe errors.** A 429, 500 or 503 carries `Request-Id`, the error envelope and `Stripe-Should-Retry`. A 401 is never replaced by an injected fault. Tests `rate_limit_returns_429_after_burst` and `faults_apply_after_auth` (Task 3).
4. **A stale page can use a cursor that is newer than its view.** `starting_after=<id created after now - lag>` still pages (200), because cursor positions come from the full order. Test `stale_page_with_new_cursor_still_pages` (Task 4).
5. **The thin Events API respects retention for history too.** `/v2/core/events` lists no event older than the window, exactly like `/v1/events`. Test `old_history_only_in_lists` (Task 2).

## Decisions made in this plan (not in the spec)

- **History has no deliveries.** Destinations (from config) are created at `clock.start`, as if the sync engine configured them when the run started. History events are listed by the Events APIs while inside retention.
- **The default history is empty:** `duration = "0s"`, `scale = 1.0`. The spec example's 90 days is opt-in.
- **Writes during pagination need a knob.** In virtual mode, nothing changes between two page requests unless the test steps time. A new key `api.pagination_writes = { probability = 0.0 }` makes the race happen: when it fires on a list request that carries a cursor, one random action of the listed type runs first. The actions per type are listed in Task 5. It does nothing while generation is off (after `quiesce`), so end-state comparisons stay stable.
- **Latency is a real sleep in both clock modes.** It never moves simulated time. With the default `fixed 0`, it is a no-op.
- **The rate limit runs on wall time in both modes.** A virtual-time bucket would never refill between steps. It is off by default (no `rate_limit` key); the spec's 25/25 is opt-in.
- **Timeout and reset are broken responses:**
  - `timeout` holds the response for 80 s (Stripe's own request limit) or until shutdown, then aborts the connection.
  - `reset` aborts the connection without a complete response.

  The client observes only that no complete response arrives.
- **Stale views also apply to `expand[]` and sub-lists** in the same request, so one response is consistent with one moment.
- **Faults apply only to requests that pass the existing gate** (auth, version, `Stripe-Account`, read-only), in this order: rate limit (storm, then bucket), then error, then latency, then the handler.
- **On-disk history snapshots** (spec section 15, "optional") are out of scope. Export and replay land in phase 6.

## File Structure

- `src/engine/store.rs`: live-ID lists, parent index, version positions, `list_order` without sorting (Task 1, Task 4).
- `src/generate/{mod,catalog,billing,subscriptions}.rs`: use the indexes; `Generator::scale_rates` (Task 2); `Generator::pagination_write` (Task 5).
- `src/api/list.rs`: `paginate` stops after `limit + 1` matches (Task 1).
- `src/config.rs`: `HistoryConfig`, the `ApiConfig` fault keys, and the unimplemented lists shrink (Tasks 2–5).
- `src/handle.rs`: history at start, `update_api_config`, `force_rate_limit`, shutdown signal (Tasks 2, 3).
- `src/faults/mod.rs`, `src/faults/api.rs` (new): `ApiFaults`, `TokenBucket`, `Fault` (Task 3).
- `src/api/mod.rs`: the gate applies faults (Task 3).
- `src/api/resources.rs`, `src/api/expand.rs`: the `View` (as-of time), stale reads and pagination writes (Tasks 4, 5).
- Tests (new): `tests/perf.rs`, `tests/history.rs`, `tests/api_faults.rs`, `tests/stale_reads.rs`, `tests/pagination_writes.rs`.

---

### Task 1: Store indexes and scan-free generation

**Files:**
- Modify: `src/engine/store.rs`, `src/generate/mod.rs`, `src/generate/catalog.rs`, `src/generate/billing.rs`, `src/generate/subscriptions.rs`, `src/api/list.rs`, `src/api/resources.rs`
- Create: `tests/perf.rs`

**Interfaces:**
- Produces:
  - `ObjectStore::live_ids(&self, t: ObjectType) -> &[String]`: maintained on insert and delete, in creation order. This replaces today's computed `Vec<&str>`.
  - `ObjectStore::children(&self, t, parent_field: &str, parent_id: &str) -> Vec<&StoredObject>`: served from an index, deleted children included, in creation order.
    - Indexed `(type, field)` pairs: `(SubscriptionItem, "subscription")`, `(InvoiceLineItem, "invoice")`, `(Subscription, "customer")`, `(Invoice, "customer")`, `(Price, "product")`.
    - Any other pair is a bug: `debug_assert!`, then fall back to a scan.
  - `ObjectStore::list_order(t)`: reverse creation order, no sort. `insert` asserts (`debug_assert!`) that `created` never decreases within a type.
  - `paginate`: same signature and results, but it evaluates `include` only until it has `limit + 1` matches (scanning backwards from the cursor for `ending_before`).

- [ ] **Step 1: Write failing tests**
  - Unit test `indexes_match_scans` in `store.rs`. Insert 50 customers, 10 subscriptions (customers chosen round-robin), 30 items (subscriptions chosen round-robin) and 40 invoices, then delete 5 customers and 7 items. Then:
    - `live_ids` equals a filter over `in_creation_order`;
    - `children(...)` equals a filter over `in_creation_order` for every parent;
    - `list_order` equals the old sort (created descending, later creation first on ties).
  - Unit test `paginate_matches_full_filter` in `list.rs`. On 30 items with a filter that keeps every third, every `limit` in 1..=12 and every cursor (`starting_after` and `ending_before`) gives the same page and `has_more` as a reference implementation that filters everything first. Keep that reference in the test.
  - `tests/perf.rs::spec_rates_90_days_is_fast`, marked `#[ignore]`:
    - Configure the spec example rates: `[actions]` new_customer 10, start_subscription 4, cancel_subscription 0.5, catalog_change 0.2, one_off_invoice 2, step_delay lognormal 200/2000, payment_failure_probability 0.1; churn customer 3/0.2, product 0.5/0.1, price 0.5/0.1, subscription 0.5, invoice 0.5.
    - `step` 90 days.
    - Assert the step took under 60 s of wall time and the store holds at least 20 000 customers.
    - Run it with `cargo test -p ductkit-stripe-mock --release --test perf -- --ignored`.
- [ ] **Step 2: Run.** Expected: the unit tests fail to compile (`live_ids` returns a `Vec`, and the reference helpers are missing). The perf test fails or exceeds 60 s. Record its time in the ledger.
- [ ] **Step 3: Implement.**
  - Add the indexes to `TypeStore` (`live: Vec<String>`; parent index `HashMap<(&'static str, String), Vec<String>>`). Lookups only: never iterate a `HashMap`.
  - Rewrite the scans in `items`, `delete_draft`, `delete_customer_cascade`, `pick_live`, `active_ids`, churn pickers, `cancel_subscription`, `set_actions_config` and the `price` filter in `resources.rs` to use the indexes.
  - Compare IDs with `.as_str() == Some(id)`, never by building `json!(id)`.
  - Pick order must stay creation order, so truth logs for a given seed are unchanged.
- [ ] **Step 4: Run** `cargo test --workspace` (green; the golden-value and determinism tests still pass unchanged) and the perf test (passes; ledger its time).
- [ ] **Step 5: Commit:** `perf(stripe-mock): store indexes for live IDs, children and list order`

---

### Task 2: `[history]`

**Files:**
- Modify: `src/config.rs`, `src/lib.rs`, `src/handle.rs`, `src/engine/mod.rs`, `src/generate/mod.rs`
- Create: `tests/history.rs`

**Interfaces:**
- Consumes: Task 1's indexes.
- Produces:
  - `pub struct HistoryConfig { pub duration: DurationStr, pub scale: f64 }`, with defaults `"0s"` and `1.0`. `scale` must be finite and `>= 0`. It becomes `Config::history`, is re-exported, and `"history"` leaves `UNIMPLEMENTED_SECTIONS`.
  - `Engine::new` no longer adds destinations. A new `Engine::add_config_destinations(&mut self, config: &Config)` adds them at the engine's current time.
  - `Generator::scale_rates(&mut self, factor: f64, now: SimTime)` sets every Poisson action's rate to its configured base rate × `factor`, and redraws `next_due` from `now` only where the rate changed. Base rates are kept in `Scheduled`; `set_actions_config` updates them.
  - In `MockStripe::start`, with `h = history.duration.ms()`:
    1. Build the engine and generator at `start - h`.
    2. `scale_rates(scale, start - h)`.
    3. `run_until(start)`.
    4. `scale_rates(1.0, start)`.
    5. `add_config_destinations`.

    With `h = 0`, this is exactly today's start.

- [ ] **Step 1: Write failing tests in `tests/history.rs`** (configs set `new_customer = 2.0` and `start_subscription = 1.0` unless noted; `start` = `clock.start`):
  - `history_runs_before_start`, with `[history] duration = "3d"`:
    - `m.now()` equals the clock start.
    - The truth log is non-empty, and every entry has `start - 3d <= at <= start`.
  - `old_history_only_in_lists`, with `duration = "10d"` and `[event_retention] window = "2d"`:
    - Walking `/v1/customers` (`limit=100` plus `starting_after`) finds customers created before `now - 2d`.
    - Walking `/v1/events` and `/v2/core/events` finds no event with `created <= now - 2d`.
    - Both walks find at least one event.
  - `history_is_not_delivered` (Review Focus 2):
    - `duration = "1d"`, plus a snapshot destination for `*` to a `Receiver`.
    - `quiesce()`: the receiver gets nothing within 500 ms.
    - `resume_generation()`, then `step` 2 h: every received event has `created >= start` in seconds, and at least one arrives.
  - `history_subscriptions_keep_renewing` (Review Focus 1):
    - `duration = "20d"`, `new_customer = 0.5`, `start_subscription = 0.5`, and a snapshot destination for `invoice.created`.
    - `step` 35 d, then `quiesce()`.
    - At least one received `invoice.created` has `data.object.billing_reason == "subscription_cycle"` for a subscription whose `created < start`.
  - `scale_multiplies_history_rates`:
    - With `scale = 0.0` and `duration = "5d"`, the truth log is empty at start.
    - `scale = -1.0` gives a config error naming `history.scale`.
  - `history_is_deterministic`: two mocks with `seed = 3` and `duration = "5d"` have equal `serde_json::to_value(truth_log())`.
  - New config unit test `history_defaults_are_empty`: default `duration` 0, `scale` 1.0, and `"[history]\nduration = \"90d\"\nscale = 0.5"` parses.
  - Note on `spec_example_without_top_level_sections_fails_clearly`: it strips the still-unimplemented sections from the spec example and expects the first unimplemented key. That key moves as tasks land: `api.latency` until Task 3, `api.stale_read` after Task 3, and `actions.one_off_payment` after Task 4. Each of those tasks updates the expectation.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test history`. Expected: fail (`[history]` is unimplemented).
- [ ] **Step 3: Implement** as in Interfaces.
- [ ] **Step 4: Run** the history tests, then `cargo test --workspace`. Expected: all green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): pre-generated history before the run starts`

---

### Task 3: Rate limits, errors, timeouts, resets and latency

**Files:**
- Create: `src/faults/mod.rs`, `src/faults/api.rs`, `tests/api_faults.rs`
- Modify: `src/config.rs`, `src/lib.rs`, `src/api/mod.rs`, `src/handle.rs`

**Interfaces:**
- Produces:
  - **Config** (`ApiConfig` gains these; all are re-exported):
    - `latency: Distribution`, default fixed 0;
    - `rate_limit: Option<RateLimit { requests_per_second: f64, burst: u32 }>`, default `None`;
    - `errors: ApiErrors`, with fields deserialized from the keys `"500"`, `"503"`, `timeout` and `reset`, each default 0.0.

    `ApiConfig::validate`:
    - each error probability must be in `0..=1`, and their sum `<= 1`;
    - `rate_limit` needs both values `> 0`;
    - the latency is validated as a distribution.

    `latency`, `rate_limit` and `errors` leave `UNIMPLEMENTED_KEYS` (`stale_read` stays until Task 4).
  - **`faults::api`:**
    - `pub(crate) struct TokenBucket` with `new(rate: f64, burst: u32, now: Instant)` and `try_take(&mut self, now: Instant) -> bool`.
    - `pub(crate) enum Fault { RateLimited, ServerError, Unavailable, Timeout, Reset }`.
    - `pub(crate) struct ApiFaults`:
      - `new(config: &ApiConfig, streams: &RngStreams)`;
      - `config(&self) -> &ApiConfig`;
      - `set_config(&mut self, c: ApiConfig, now: Instant)`, which rebuilds the bucket;
      - `storm(&mut self, until: Instant)`;
      - `decide(&mut self, now: Instant) -> (Option<Fault>, u64 /* latency ms */)`. It always draws once from `"api.errors"` and once from `"api.latency"` (Global Constraints), then applies the storm, then the bucket, then the drawn error. The error ranges are `[0, p500)` → ServerError, then `p503` → Unavailable, then `timeout`, then `reset`.
  - **`Sim`** gains `pub api: ApiFaults`. The gate reads keys from `sim.api.config().keys`, so they can change at runtime.
  - **`Shared`** gains `shutdown: tokio::sync::watch::Sender<bool>`. `MockStripe::shutdown` sends `true` first.
  - **`MockStripe`:**
    - `update_api_config(&self, f: impl FnOnce(&mut ApiConfig)) -> Result<(), MockError>` (validated);
    - `force_rate_limit(&self, d: Duration)`: every request gets 429 for `d` of wall time.
  - **Responses:**

    | Fault | Status | Body (`error.type` / `error.code`) | Message |
    |---|---|---|---|
    | RateLimited | 429 | `invalid_request_error` / `rate_limit` | `Too many requests hit the API too quickly. We recommend an exponential backoff of your requests.` |
    | ServerError | 500 | `api_error` | `An unknown error occurred.` |
    | Unavailable | 503 | `api_error` | `The server is temporarily unavailable. Please retry.` |

    Each of these three sets `Stripe-Should-Retry: true`. Timeout and Reset abort the connection as described in Decisions. A Timeout response is a body stream that waits for 80 s or the shutdown signal, then yields an `io::Error`. A Reset response yields an `io::Error` at once. Latency is applied as `tokio::time::sleep` before the handler.

- [ ] **Step 1: Write failing tests in `tests/api_faults.rs`:**
  - `rate_limit_returns_429_after_burst` (Review Focus 3): `rate_limit = { requests_per_second = 1, burst = 3 }`.
    - Five back-to-back `GET /v1/customers` return statuses `[200, 200, 200, 429, 429]`.
    - The 429s have `error.code == "rate_limit"`, header `stripe-should-retry == "true"`, and a non-empty `request-id`.
    - After 1.1 s, a request is 200 again.
  - `server_errors_follow_config`:
    - `errors = { "500" = 1.0 }`: status 500, `error.type == "api_error"`, `stripe-should-retry: true`.
    - After `update_api_config` sets `e500 = 0`, `e503 = 1`: status 503.
  - `faults_apply_after_auth` (Review Focus 3): with `"500" = 1.0`, a bad key still gets 401.
  - `reset_drops_connection`: with `reset = 1.0`, `async { client.get(..).send().await?.bytes().await }` is `Err`.
  - `timeout_hangs_until_client_gives_up`:
    - `timeout = 1.0`, and a client with a 300 ms timeout: the request is `Err` with `is_timeout()`.
    - `m.shutdown()` then completes within 2 s.
  - `latency_delays_responses`: latency fixed 300 ms means a 200 response took `>= 300` ms.
  - `force_rate_limit_storm`: `force_rate_limit(500 ms)` → 429. After 600 ms → 200.
  - `fault_sequence_is_deterministic`: two mocks with `seed = 9` and `"500" = 0.5`, each sent 30 sequential requests, return identical status sequences that contain both 200 and 500.
  - Unit tests in `faults/api.rs`:
    - `bucket_refills`: burst 2, rate 10/s → take, take, fail; then after 100 ms take succeeds.
    - `error_ranges`: deterministic fault counts for 10 000 draws with p500 = 0.1 and reset = 0.1 fall within ±2% of expected.
  - Config: `api_fault_probabilities_validated` (`"500" = 0.7`, `"503" = 0.5` is an error). `spec_example_without_top_level_sections_fails_clearly` now expects `api.stale_read`; Task 4 moves it to `actions.one_off_payment`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test api_faults`. Expected: fail (keys unimplemented).
- [ ] **Step 3: Implement** as in Interfaces.
- [ ] **Step 4: Run** the tests, then `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): rate limits, 5xx, timeouts, resets and latency on the API`

---

### Task 4: Stale reads

**Files:**
- Modify: `src/engine/store.rs`, `src/engine/mod.rs`, `src/config.rs`, `src/faults/api.rs`, `src/api/resources.rs`, `src/api/expand.rs`
- Create: `tests/stale_reads.rs`

**Interfaces:**
- Consumes: Task 3's `ApiFaults`; Task 1's indexes.
- Produces:
  - **Config:** `ApiConfig.stale_read: StaleRead { probability: f64, max_lag: DurationStr }`, defaults 0.0 and `"2s"`. The probability must be in `0..=1` and `max_lag >= 1ms`. `stale_read` leaves `UNIMPLEMENTED_KEYS`.
  - **Store:** `StoredObject` gains `versions: Vec<usize>`, the truth-log positions of its versions, maintained by `Engine::create`, `update` and `delete`.
  - **Engine:** `Engine::as_of(&self, t, id, at: SimTime) -> Option<&StateUpdate>` returns the latest version with `at <= at`, or `None` if the object did not exist yet.
  - **`ApiFaults::stale_lag(&mut self) -> Option<i64>`:** one `chance(probability)` from `"api.stale_reads"`. When it fires, one `uniform_ms(1, max_lag)`.
  - **`resources.rs`:**
    - `pub(crate) struct View<'a> { engine: &'a Engine, at: Option<SimTime> }` with `state(&self, o: &StoredObject) -> Option<&StateUpdate>` (latest, or `as_of`).
    - List, retrieve, `invoice_lines`, `matches` and `expand` take the view instead of reading `o.latest`.
    - Lists keep using the full order for cursor positions and exclude objects whose view state is `None` or `Deleted`.
    - Retrieve under a stale view:
      - `None` → 404 `resource_missing`;
      - a deleted customer → its stub;
      - another deleted object → 404.
  - Stale reads apply to object retrieve and list only, not to the Events APIs or destinations.

- [ ] **Step 1: Write failing tests in `tests/stale_reads.rs`:**
  - `stale_retrieve_never_shows_the_newest_version`: `stale_read = { probability = 1.0, max_lag = "10s" }`.
    1. Create a customer, `step` 5 s, `update_customer` (version 2).
    2. 20 retrieves: none shows the v2 change, at least one is 200 with v1's state, and the rest are 404.
    3. `step` 11 s: every retrieve shows v2.
  - `stale_list_omits_just_created`: same config. `create_customer`, then `/v1/customers` does not contain it. After `step` 11 s, it does.
  - `stale_page_with_new_cursor_still_pages` (Review Focus 4):
    1. Create 5 customers, `step` 20 s, create 1 more (`x`).
    2. With probability 1.0, `/v1/customers?starting_after=x` is 200, with the older customers in order.
  - `stale_reads_off_by_default`: with the default config, retrieve right after an update shows v2 in 20 out of 20 requests.
  - Unit test `as_of_picks_version_at_time` in `engine/mod.rs`, with versions at t = 0, 10 and 20 s:
    - at 5 s → v1;
    - at 10 s → v2;
    - at 25 s → v3;
    - before creation → `None`.
- [ ] **Step 2: Run.** Expected: fail (`api.stale_read` is unimplemented).
- [ ] **Step 3: Implement** as in Interfaces.
- [ ] **Step 4: Run** the tests, then `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): stale reads on retrieve and list`

---

### Task 5: Writes during pagination

**Files:**
- Modify: `src/config.rs`, `src/faults/api.rs`, `src/generate/mod.rs`, `src/api/resources.rs`
- Create: `tests/pagination_writes.rs`

**Interfaces:**
- Consumes: Task 3's `ApiFaults`; `perform` and `ActionKind` from phase 3.
- Produces:
  - **Config:** `ApiConfig.pagination_writes: PaginationWrites { probability: f64 }`, default 0.0, valid `0..=1`.
  - **`ApiFaults::pagination_write(&mut self) -> bool`:** one `chance` from `"api.pagination_writes"`. `pagination_writes` is a new key (Decisions), so the spec example gains it only in Task 6.
  - **`Generator::pagination_write(&mut self, engine: &mut Engine, t: ObjectType)`:** while generation is enabled, it picks one kind uniformly (`below`) for the listed type and runs `perform(kind, &mut self.pagination_rng, engine, &mut self.ctx)`, then any continuations due now. `pagination_rng` is the stream `"api.pagination_writes.content"`, separate from the decision stream in `ApiFaults`. Kinds per type:

    | Type | Kinds |
    |---|---|
    | Customer | NewCustomer, CustomerUpdate, CustomerDelete |
    | Product | CatalogChange, ProductUpdate, ProductArchive |
    | Price | CatalogChange, PriceUpdate, PriceArchive |
    | Subscription | StartSubscription, SubscriptionUpdate, CancelSubscription |
    | Invoice | OneOffInvoice, InvoiceUpdate |
    | SubscriptionItem, InvoiceLineItem | none: sub-lists never trigger writes |

  - **`resources::list`:** when the request has `starting_after` or `ending_before` and `pagination_write()` fires, the mutation runs under the same lock, before the page is cut.

- [ ] **Step 1: Write failing tests in `tests/pagination_writes.rs`:**
  - `writes_happen_between_pages`: `pagination_writes = { probability = 1.0 }`, all generation rates 0, 30 customers created with `create_customer`.
    - Walk `/v1/customers?limit=5`.
    - The truth log grew by at least one entry per page after the first.
    - Every new entry is a `Customer` (or, for a cascade, a `Subscription`/`Invoice` of that customer).
    - The first page alone writes nothing.
  - `writes_follow_the_listed_type`: with 3 products created through `run_action(Action::CatalogChange)` and probability 1.0, a second page of `/v1/products?limit=1` adds only `Product` and `Price` entries.
  - `no_writes_after_quiesce`: after `quiesce()`, walking every page adds nothing to the truth log.
  - `pagination_writes_are_deterministic`: two mocks with the same seed and the same walk give equal truth logs.
- [ ] **Step 2: Run.** Expected: fail (`pagination_writes` is an unknown key).
- [ ] **Step 3: Implement** as in Interfaces.
- [ ] **Step 4: Run** the tests, then `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Commit:** `feat(stripe-mock): writes during pagination`

---

### Task 6: Backfill race end to end, history perf, spec updates

**Files:**
- Modify: `tests/history.rs`, `tests/perf.rs`, `docs/stripe-mock-spec.md`

- [ ] **Step 1: Write the tests**
  - `backfill_race_converges` in `tests/history.rs`:
    - Config: `duration = "7d"`, retention `"2d"`, `new_customer = 4.0`, `[churn.customer] update = 2.0, delete = 0.2`, and `pagination_writes = { probability = 0.3 }`.
    - A mini sync:
      1. Record `t0 = m.now()` in seconds.
      2. Walk `/v1/customers?limit=20`, stepping 1 min between pages, into a map from ID to object.
      3. `quiesce()`.
      4. Walk `/v1/events?created[gte]=t0` completely, then apply each `customer.*` event in ascending `created` order (reverse list order). `created` and `updated` set the map entry to `data.object`; `deleted` removes the ID.
    - The resulting map equals `{id: m.get(Customer, id).state}` for every customer whose latest truth entry is not `Deleted`. This shows that backfill plus events converge despite the race. Customers have no unannounced mutations in this config (no invoices), so the last event always carries the latest state.
  - `history_90d_spec_rates_is_fast` in `tests/perf.rs`, marked `#[ignore]`: the Task 1 rates as `[history] duration = "90d"`. `MockStripe::start` returns in under 60 s (release).
- [ ] **Step 2: Run** both. Expected: they pass if Tasks 1–5 are complete; otherwise fix the gap and record a ruling.
- [ ] **Step 3: Update the spec:**
  - Section 4:
    - add `pagination_writes = { probability = 0.0 }` under `[api]`, with the comment `# a list page after the first may follow a fresh mutation of the listed type`;
    - note that `rate_limit` and `latency` are wall-time;
    - note that the default history is empty.
  - Section 6.3: add "History events are listed by the Events APIs while inside retention, but never delivered: destinations start at `clock.start`."
  - Section 7.4: add one line each for the timeout/reset behavior, `Stripe-Should-Retry`, the fault order, and the `pagination_writes` knob (with its no-op after quiesce).
  - Section 11: name `update_api_config` and `force_rate_limit` under "Faults at runtime".
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: all green.
- [ ] **Step 5: Commit:** `test(stripe-mock): backfill race end to end; spec updates for phase 4`
