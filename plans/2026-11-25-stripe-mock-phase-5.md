# Stripe Mock Phase 5 (Connect) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mock becomes a Connect platform. It has v1 and v2 connected accounts that connect and disconnect during the run. Each account has its own objects, generation and events, at the same rates as the platform. Accounts are reachable through `Stripe-Account` / `Stripe-Context`, the Accounts endpoints, and connect-aware snapshot and thin destinations.

**Architecture:**
- **Every object lives in an account scope.** `Engine` keeps one `ObjectStore` per scope (`None` = platform, `Some(acct)` = a connected account) and a current scope. Every write records `StateUpdate.account = scope`. One truth log, one event log and one delivery queue serve every scope, so ordering, determinism and quiesce work unchanged.
- **Accounts are platform objects.** `ObjectType::Account` is stored in the platform scope, with truth state in the v1 `account` shape. The v2 view is rendered from that state plus mock-private `AccountMeta` (API, configurations).
- **Generation is multiplexed.** `Generators` replaces `Sim.generator` and keeps its method names. It owns:
  - the platform `Generator`, with unchanged stream names;
  - an `AccountsGenerator` for connect, disconnect and account churn;
  - one `Generator` per connected account, running inside that account's scope with streams prefixed `account/{acct}/`.
- **Routing is by labels on the event.** Each event carries `account` (snapshot `account`, thin `context`). Destinations, Events APIs and subscriptions filter on it.

**Tech Stack:** The phase 4 stack. No new dependencies.

**Spec:** [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md). Phase 5 is item 5 of section 16: "Connect: v1 and v2 connected accounts, `Stripe-Account`, connect/disconnect, full event coverage for connected accounts through snapshot and thin destinations." The main references are sections 4 (`[accounts]`, destinations), 5.4, 7.1, 7.2, 8.1, 8.2, 9, 10.3, 11 and 18.

## Global Constraints

- **Event coverage is identical for platform and connected accounts**, with the same rates (spec 5.4). A connected account's generator is built from the same `[actions]`/`[churn]` config as the platform's. `update_actions_config` and `history.scale` apply to every generator.
- **Determinism** (spec 12). Platform streams keep their names and draw order: a run with `connect = false` produces the same truth log as phase 4 for the same seed, and the golden tests stay unchanged. New streams:
  - `"accounts.connect"`: the connect Poisson clock.
  - `"accounts.deauthorize"`: the disconnect Poisson clock, plus the pick of which account.
  - `"accounts.content"`: the API, type, configurations and fields of each new account.
  - `"accounts.update"`: the `[churn.account]` Poisson clock and its content.
  - `"platform.application"`: the platform's `ca_` ID, drawn once at engine creation, only when `connect = true`.
  - Each connected account's generator uses every platform stream name prefixed `account/{acct}/`.
  - Ties at the same instant run in this order: platform, then the accounts generator, then connected accounts in connection order.
- **IDs:** accounts use `acct_` with the existing ID generator. The platform application uses `ca_`.
- **Error envelope** (spec 7.1), with `Request-Id`. Copy fixed by this plan:
  - Unknown or disconnected account → **403**, `type: "invalid_request_error"`, `code: "account_invalid"`, `message: "The provided key does not have access to account '{acct}' (or that account does not exist). Application access may have been revoked."`
  - `Stripe-Account` and `Stripe-Context` both present and different → **400**, `type: "invalid_request_error"`, `message: "Stripe-Account and Stripe-Context name different accounts."`
  - `Stripe-Account` on a `/v2/` endpoint → **400**, `message: "Use the Stripe-Context header with v2 endpoints."`
  - An Accounts endpoint called with a connected-account header → **400**, `message: "Accounts endpoints must be called by the platform."`
  - Any header while `connect = false` → today's error (`"Connected accounts are not enabled in this mock."`).
- **Faults apply to connected accounts the same way** (spec 5.4): rate limits, errors, stale reads and pagination writes. A stale read or a pagination write stays inside the request's scope.
- **Process:**
  - `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` are clean before every commit. Run clippy and the suite as separate steps **before** `git commit`.
  - Dependency versions live only in the root `[workspace.dependencies]`.
  - Commits end with the attribution footer.
- **Branching:** work on branch `stripe-mock-phase-5` in the existing checkout. Never use a worktree.
- **Windows:** keep LF line endings. Python edits use `newline=''`. `docs/stripe-mock-spec.md` is parsed by a config test.

## Review Focus

1. **An account disconnected during a backfill gets 403 `account_invalid` mid-pagination.** Every page after the disconnect fails the same way. It never returns a 404 or an empty 200 page. Test `disconnect_mid_backfill_is_403` (Task 4).
2. **No cross-account leak.** An object, event or delivery from one scope is never visible in another:
   - the Events API, retrieve-by-ID and `expand[]` do not cross scopes;
   - a connected customer's ID requested as the platform is a 404.
   
   Tests `scopes_do_not_leak` (Task 4) and `deliveries_route_by_account` (Task 5).
3. **Disconnect stops that account's generation, including continuations already queued.** Examples are a pending renewal or a finalize step. No new truth entry is written in that scope after the disconnect. Test `disconnect_stops_continuations` (Task 3).
4. **`quiesce` and resume cover every account.** `quiesce()` stops every generator and flushes connected-account deliveries. `resume_generation()` restarts each one without moving time backwards. Test `quiesce_covers_accounts` (Task 3).
5. **A stale read never shows an account that was not connected at the read's moment.** It also never shows a connected account's object from the wrong scope. Test `stale_reads_respect_scope` (Task 4).

## Decisions made in this plan (not in the spec)

These settle spec 18's interoperability question, from Stripe's docs (docs.stripe.com, Accounts v2 and Connect webhooks pages):

- **Both APIs read both kinds of account.**
  - A v2-created account appears in `/v1/accounts` in the v1 shape, with `type: "none"` and a `controller`.
  - A v1-created account appears in `/v2/core/accounts` with `applied_configurations: ["merchant"]`.
- **v2 accounts emit v1 snapshot events too.** An update to a v2 account emits `account.updated` (Connected scope, `account` = the account) in addition to the thin `v2.core.account.updated` (platform scope, `context: null`).
  - This revises spec 5.4's "never snapshot events"; Task 6 updates the spec text.
  - `v2.core.account[configuration.*].updated` and other per-configuration events are out of scope, because the mock never changes a configuration after creation.
- **Event types and their labels.** `account` is the event's Connect label, used for routing.

  | Lifecycle step | Events | `account` |
  |---|---|---|
  | Connect (any account) | `account.application.authorized` (plus thin `v1.account.application.authorized`) | the account |
  | Connect (v2 account only) | additionally `v2.core.account.created`, thin only | `None` |
  | Account update | `account.updated` (plus thin `v1.account.updated`) | the account |
  | Account update (v2 account only) | additionally `v2.core.account.updated`, thin only | `None` |
  | Disconnect (any account) | `account.application.deauthorized` (plus thin `v1.`) | the account |
  | Disconnect (v2 account only) | additionally `v2.core.account.closed`, thin only | `None` |

  - `v2.`-prefixed types never get a snapshot pair.
  - The `data.object` of `account.application.*` is the application: `{"id": "ca_…", "object": "application", "name": "Ductkit Mock Platform"}`.
  - The thin `related_object` of every account event is the account (`type: "account"`, `url: "/v1/accounts/{id}"`).
- **Disconnect is a delete in the store**, for both APIs. A v1 account records the tombstone `{"id", "object": "account", "deleted": true}`.
  - `/v1/accounts` omits disconnected accounts, and `/v1/accounts/{id}` returns 404.
  - `/v2/core/accounts/{id}` renders the last live version with `closed: true`.
  - `/v2/core/accounts` lists closed accounts only when `closed=true` is passed.
- **Any account can be disconnected.** Stripe allows deauthorization only for Standard accounts. The mock uses one "disconnect" for every account, so the rate and the control API behave uniformly.
- **Deliveries already queued for a disconnected account still run.** They are already sent. No new events are created in its scope.
- **v1 account shape.** The truth state has these fields:
  - `id`, `object: "account"`, `type`, `controller {type: "application", is_controller: true}` (v2 and custom), `business_profile {name}`, `email`, `country: "US"`, `default_currency: "usd"`, `charges_enabled: true`, `payouts_enabled: true`, `details_submitted: true`, `created`, `metadata {}`.
  - Churn changes `business_profile.name`, `email` and `metadata`.
- **v2 view shape:**
  - `id`, `object: "v2.core.account"`, `applied_configurations` (sorted), `closed`, `configuration {customer, merchant, recipient}`, `contact_email` (= `email`), `created` (RFC 3339 with milliseconds, UTC `Z`), `dashboard`, `defaults: null`, `display_name` (= `business_profile.name`), `future_requirements: null`, `identity: null`, `livemode: false`, `metadata`, `requirements: null`.
  - Each `configuration` entry is `{"applied": true}` when applied, else `null`.
  - `dashboard` is `"full"` for v1 standard and every v2 account, `"express"` for express, and `"none"` for custom.
  - A v2 account's configurations are a non-empty subset of `merchant`, `customer` and `recipient`. Each is drawn with probability 0.5, and an empty draw is rerolled to `["merchant"]`.
- **Headers:**
  - On `/v1/` endpoints, both `Stripe-Account` and `Stripe-Context` select the scope.
  - On `/v2/` endpoints, only `Stripe-Context` selects it.
  - Both Events APIs list the scope's events: the platform's events have `account = None`, and an account's events have `account = Some(that account)`.
  - Event retrieve by ID outside its scope returns 404.
- **Destinations:**
  - A snapshot destination receives events whose `account` is `None` when `connect = false`. When `connect = true` it receives those **and** events with `Some(account)`. Spec 4: "*also* receive events from connected accounts".
  - A thin destination receives `None` for `"@self"` and `Some` for `"@accounts"`. `events_from` defaults to `["@self"]`.
  - `@organization_members…` values are rejected as not implemented (organizations are out of scope).
  - `connect` on a thin destination and `events_from` on a snapshot destination are config errors.
- **`[accounts]` config:**
  - `platforms` must be `1`; anything else is "not implemented".
  - `connect` defaults to `false`.
  - The other defaults are `initial = 0`, both rates `0`, `account_api = { v1 = 0.5, v2 = 0.5 }` and `account_types = { standard = 0.5, express = 0.3, custom = 0.2 }`.
  - Each mix must sum to 1 (±1e-9), with no negative weights.
  - Any connected-account count or rate above 0 while `connect = false` is a config error.
- **New config key `[churn.account] update`** (per hour, default 0): updates a random connected account.
- **Initial accounts are connected at history start** (`clock.start - history.duration`), before history runs, so they get history too. Their connect events are inside history, so they are not delivered.
- **Control API:**
  - `MockStripe::connect_account(api: AccountApi) -> Result<String, MockError>`; the v1 type is drawn from `account_types`.
  - `MockStripe::disconnect_account(id)`.
  - `MockStripe::accounts() -> Vec<AccountInfo>`.
  - `MockStripe::account(id) -> AccountHandle<'_>`, a scoped control surface.

## File Structure

- `src/engine/mod.rs`: per-scope stores, current scope, `in_scope`, scope-aware lookups (Task 1); `AccountMeta` registry and the application (Task 2).
- `src/engine/events.rs`: `EventRecord.scope` and `EventRecord.account`, snapshot `account`, thin `context`, thin-only `v2.` types (Task 1, Task 2).
- `src/model/mod.rs`, `src/model/account.rs` (new): `ObjectType::Account`, the v1 state builder and the v2 renderer (Task 2).
- `src/config.rs`: `AccountsConfig`, `ChurnConfig.account`, destination validation, unimplemented lists shrink (Task 2, Task 5).
- `src/generate/accounts.rs` (new): `AccountsGenerator` (Task 2, Task 3). `src/generate/mux.rs` (new): `Generators` (Task 3). `src/generate/mod.rs`: stream prefix (Task 3).
- `src/handle.rs`: `Generators` in `Sim`, connect/disconnect, `AccountHandle`, `accounts()` (Tasks 2, 3).
- `src/api/mod.rs`: scope from headers in the gate (Task 4). `src/api/accounts.rs` (new): the Accounts endpoints (Task 4). `src/api/resources.rs`, `events_v1.rs`, `events_v2.rs`, `expand.rs`: the `View` gains a scope (Task 4).
- `src/delivery/mod.rs`: `Destinations::matching` filters on `account` (Task 5).
- `src/stream.rs`: `SubscribeFilter.accounts` (Task 6).
- Tests (new): `tests/connect_engine.rs`, `tests/connect_generation.rs`, `tests/connect_api.rs`, `tests/connect_delivery.rs`, `tests/connect_e2e.rs`.

---

### Task 1: Engine account scopes

**Files:**
- Modify: `src/engine/mod.rs`, `src/engine/events.rs`, `src/handle.rs` (only callers that need `scope`)
- Create: `tests/connect_engine.rs` (unit tests may live in `engine/mod.rs` if they need private API)

**Interfaces:**
- Produces:
  - `pub type Scope = Option<String>;` in `engine/mod.rs` (re-exported).
  - `Engine { stores: BTreeMap<Scope, ObjectStore>, scope: Scope, … }`, replacing `store`.
    - `Engine::store(&self) -> &ObjectStore` returns the current scope's store (an empty static store when the scope has none).
    - `Engine::store_in(&self, scope: &Scope) -> Option<&ObjectStore>`.
  - `Engine::in_scope<R>(&mut self, scope: Scope, f: impl FnOnce(&mut Engine) -> R) -> R`: sets the scope, runs `f`, restores the previous scope (also on early return).
  - `Engine::scope(&self) -> &Scope`.
  - `record()` sets `StateUpdate.account = self.scope.clone()` and writes into that scope's store.
  - `latest(t, id)` and `as_of(t, id, at)` read the current scope.
  - `EventRecord` gains `pub scope: Scope` (where its object lives) and `pub account: Option<String>` (its Connect label).
    - Both default to the recording scope.
    - Task 2 overrides `account` for account events through `Engine::record_with_label(update, label: Option<String>)`. Make `record` call it with the scope.
  - The snapshot payload gains `"account": "<acct>"` only when `account` is `Some`. The thin payload sets `"context": "<acct>"` instead of `null` when `Some`.
  - `record_outcome` looks objects up in `ev.scope`.
  - `subscribe()` snapshots every scope (platform first, then scopes in `BTreeMap` order).

- [ ] **Step 1: Write failing tests** (in `engine/mod.rs` `#[cfg(test)]` if they need `Engine` directly):
  - `scoped_writes_are_isolated`:
    - Create a customer in the platform scope and one in `Some("acct_1")`.
    - `latest` in each scope finds only its own.
    - The truth log has both, with `account` `None` and `Some("acct_1")`.
  - `in_scope_restores`: nested `in_scope(Some("a"), |e| e.in_scope(Some("b"), …))` leaves `scope()` as `None` afterwards.
  - `connected_event_payloads_carry_account`:
    - A customer created in `Some("acct_1")` produces a snapshot payload with `"account": "acct_1"` and a thin payload with `"context": "acct_1"`.
    - The platform's snapshot payload has no `account` key, and its thin payload has `"context": null`.
  - `as_of_is_scoped`: the same ID requested from another scope returns `None`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --lib engine`. Expected: compile errors (`in_scope`, `scope` missing).
- [ ] **Step 3: Implement** the interfaces above. Every existing caller of `engine.store` uses the current scope, which is `None` unless set, so existing behaviour is unchanged.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green, including the golden/determinism tests unchanged.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): per-account object scopes in the engine`

---

### Task 2: `[accounts]` config, Account objects, connect and disconnect

**Files:**
- Modify: `src/config.rs`, `src/model/mod.rs`, `src/engine/mod.rs`, `src/engine/events.rs`, `src/handle.rs`, `src/lib.rs`
- Create: `src/model/account.rs`, `src/generate/accounts.rs`, `tests/connect_generation.rs`

**Interfaces:**
- Consumes: Task 1 `in_scope`, `record_with_label`, `EventRecord.account`.
- Produces:
  - `pub struct AccountsConfig { pub platforms: u32, pub connect: bool, pub connected_accounts: ConnectedAccounts, pub account_api: AccountApiMix, pub account_types: AccountTypeMix }` (`#[serde(default, deny_unknown_fields)]`), defaults as in Decisions.
    - `ConnectedAccounts { initial: u32, connect_rate: f64, deauthorize_rate: f64 }`, `AccountApiMix { v1: f64, v2: f64 }`, `AccountTypeMix { standard: f64, express: f64, custom: f64 }`.
    - `Config::accounts`, re-exported.
  - `ChurnConfig.account: UpdateChurn`.
  - Remove `UNIMPLEMENTED_SECTIONS` and its branch in `first_unimplemented`. Repoint `unimplemented_section_is_named`, `not_implemented_message_is_tidy` and the phase-5 lines around `config.rs:1003` at `actions.one_off_payment`, keeping each test's intent. `spec_example_config_fails_clearly` still fails, now on `actions.one_off_payment`.
  - `validate()`: `platforms != 1` → `` "`accounts.platforms` other than 1 is not implemented yet" ``; mixes must sum to 1; nonzero `initial`/rates with `connect = false` → `` "`accounts.connected_accounts` needs `accounts.connect = true`" ``.
  - `ObjectType::Account` (prefix `acct`, API object `"account"`). It is added to `ObjectType::ALL` last, so existing order and streams are unchanged. `parent_fields` → `&[]`.
  - `pub enum AccountApi { V1, V2 }`, `pub enum AccountKind { Standard, Express, Custom, V2 }`, `pub enum Configuration { Customer, Merchant, Recipient }`. All are re-exported.
  - `pub(crate) struct AccountMeta { pub id: String, pub api: AccountApi, pub kind: AccountKind, pub configurations: Vec<Configuration>, pub connected: bool }`, held in `Engine.accounts: Vec<AccountMeta>` in connection order.
    - `Engine::account_meta(&self, id) -> Option<&AccountMeta>`.
    - `Engine::connected_ids(&self) -> Vec<String>`.
  - `Engine.application: Option<serde_json::Value>`, set at creation when `connect = true`.
  - `model::account::v1_state(id, kind, created, content: &AccountContent) -> Value` and `model::account::v2_view(state: &Value, meta: &AccountMeta, closed: bool) -> Value`, with the shapes from Decisions.
  - `Engine::connect_account(&mut self, api: AccountApi, kind: AccountKind, configurations: Vec<Configuration>, content: AccountContent) -> String`: records the Account (Created) in the platform scope and the events from the Decisions table, then pushes `AccountMeta`.
  - `Engine::disconnect_account(&mut self, id) -> Result<(), EngineError>`: records the delete and the Decisions events, then sets `connected = false`. An unknown or already disconnected ID → `EngineError::UnknownAccount`.
  - `Engine::update_account(&mut self, id, content: AccountContent)`: records an Updated version and emits `account.updated`, plus `v2.core.account.updated` for v2 accounts.
  - `generate::accounts::AccountsGenerator`, with the streams from Global Constraints:
    - `new(cfg: &AccountsConfig, churn: &UpdateChurn, seed, now) -> Self`;
    - `next_due(&self) -> Option<SimTime>`;
    - `run_due(&mut self, engine, t) -> Vec<AccountChange>`, where `pub(crate) enum AccountChange { Connected(String), Disconnected(String) }` tells Task 3 which generators to add or drop;
    - `connect_initial(&mut self, engine, n) -> Vec<String>`;
    - `scale_rates(f64)`;
    - `draw_new(&mut self) -> (AccountApi, AccountKind, Vec<Configuration>, AccountContent)`, used by the control API too.
  - `MockStripe::connect_account(api: AccountApi) -> Result<String, MockError>` and `MockStripe::disconnect_account(&str) -> Result<(), MockError>`. Both return `MockError::ConnectDisabled` when `connect = false`; disconnect returns `MockError::UnknownAccount(id)` for an unknown ID.
  - `MockStripe::accounts() -> Vec<AccountInfo { id, api, kind, configurations, connected }>`.
  - Until Task 3, connected accounts have no generator. Task 2 only creates the accounts and their lifecycle events.

- [ ] **Step 1: Write failing tests**:
  - `config.rs` unit tests:
    - `accounts_defaults`;
    - `accounts_mix_must_sum_to_one`;
    - `accounts_need_connect`;
    - `platforms_other_than_one_not_implemented`;
    - `spec_example_accounts_section_parses`: parse the spec's `[accounts]` block alone with `[history]` defaults.
  - `tests/connect_generation.rs`:
    - `connect_v1_emits_authorized`:
      - `connect_account(AccountApi::V1)` → `event_timeline()` gains `account.application.authorized` (snapshot) and `v1.account.application.authorized` (thin).
      - Both have `account == Some(id)`, and the snapshot payload's `data.object.object == "application"`.
    - `connect_v2_also_emits_thin_created`: also `v2.core.account.created` with `account == None` and no snapshot pair.
    - `disconnect_emits_deauthorized_and_closed`:
      - A v1 account gets `account.application.deauthorized`.
      - A v2 account gets that plus `v2.core.account.closed`.
      - `accounts()` shows `connected: false`.
      - A second disconnect returns `MockError::UnknownAccount`.
    - `connect_disabled_by_default`: `connect_account` returns `MockError::ConnectDisabled`.
    - `initial_accounts_at_history_start`:
      - With `initial = 3`, `history.duration = "1h"`, 3 accounts exist at start, with `created == clock.start - 3600`.
      - No delivery is made for their events (reuse the history-is-not-delivered pattern).
    - `account_rates_follow_config`:
      - `connect_rate = 10`, `deauthorize_rate = 2`, `[churn.account] update = 5`; `step` 10 h.
      - Connects are in 60..=140, disconnects ≥ 1, and `account.updated` events ≥ 20.
    - `same_seed_same_accounts`: two runs with the same seed have equal `accounts()` and `truth_log()`.
    - `connect_false_truth_log_unchanged`: existing golden test(s) still pass. Name them in the ledger; no new code.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test connect_generation` and the config unit tests. Expected: compile errors.
- [ ] **Step 3: Implement.**
  - `AccountsGenerator` is driven from `advance_locked` after the platform generator for now; Task 3 moves it into `Generators`.
  - `[churn.account] update` picks uniformly from `connected_ids()`, and does nothing when there are none.
  - Disconnect picks uniformly from `connected_ids()`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): connected accounts with connect and disconnect lifecycle`

---

### Task 3: Per-account generation

**Files:**
- Modify: `src/generate/mod.rs`, `src/handle.rs`
- Create: `src/generate/mux.rs`; tests appended to `tests/connect_generation.rs`

**Interfaces:**
- Consumes: Task 2 `AccountsGenerator`, `AccountChange`, `Engine::in_scope`.
- Produces:
  - `Generator::new_with_prefix(…, prefix: &str)`: the existing constructor's parameters plus a stream-name prefix. The existing constructor calls it with `""`, so platform streams are unchanged.
  - `pub(crate) struct Generators { platform: Generator, accounts: AccountsGenerator, connected: Vec<(String, Generator)> }` in `generate/mux.rs`, replacing `Sim.generator`. It has the same methods with the same semantics:
    - `next_due()` is the minimum over all of them;
    - `run_until(engine, t)` runs every due action in time order, breaking ties as in Global Constraints. Each connected account's actions run inside `engine.in_scope(Some(acct))`;
    - `set_enabled(bool, now)`;
    - `actions_config()` and `set_actions_config(cfg, engine)`, which apply to all generators, each in its own scope;
    - `scale_rates(f64)`.
  - `pagination_write(scope: &Scope, t: ObjectType, engine)` runs on the generator of that scope. For an account's own `ObjectType::Account` list it runs `AccountsGenerator`'s update.
  - Applying an `AccountChange`:
    - `Connected(id)` adds a generator starting at the connect instant;
    - `Disconnected(id)` drops that generator, discarding its pending actions and continuations.
  - `MockStripe::connect_account` and `disconnect_account` go through the same path.
  - `AccountHandle<'a>`, returned by `MockStripe::account(&self, id: &str)`, with the same signatures as the matching `MockStripe` methods: `create_customer`, `update_customer`, `delete_customer`, `run_action` and `get`. Each runs in that account's scope; a disconnected or unknown account → `MockError::UnknownAccount`.

- [ ] **Step 1: Write failing tests** (`tests/connect_generation.rs`):
  - `connected_accounts_get_platform_rates`:
    - `[actions] new_customer = 20`, `connect = true`, `initial = 2`; `step` 5 h.
    - Each scope (the platform and both accounts) has 60..=140 customers in `truth_log()` (filtered by `account`).
  - `connected_account_event_types_match_platform`:
    - Spec-example `[actions]`/`[churn]` rates at one tenth, `initial = 1`; `step` 24 h.
    - The set of event types with `account == Some(acct)`, excluding `account.*` and `v1.account.*`, equals the platform's set.
  - `disconnect_stops_continuations` (Review Focus 3):
    - Start a subscription in an account through `account(id).run_action(Action::StartSubscription…)` (use the action variant the platform tests use), then `step` until a continuation is pending.
    - `disconnect_account`, then `step` 40 days.
    - No truth entry with `account == Some(id)` has `at` after the disconnect.
  - `quiesce_covers_accounts` (Review Focus 4):
    - With a snapshot destination `connect = true` and `initial = 2`, `quiesce()`.
    - Every event with `account` set has a delivery record.
    - `step` 1 h adds no truth entries.
    - `resume_generation()` then `step` 2 h adds entries in every connected scope, and every truth entry's `at` is non-decreasing.
  - `account_handle_scopes_writes`: `account(id).create_customer(..)` is visible via `account(id).get`, and `m.get` (platform) returns `None`.
  - `golden runs unchanged`: name the existing determinism/golden tests in the ledger and run them.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test connect_generation`. Expected: the new tests fail (no customers in account scopes; `account` missing).
- [ ] **Step 3: Implement** `Generators`, and switch `Sim.generator` to it. `set_enabled(true, now)` re-keys overdue continuations for every generator, as the platform does today.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): per-account generators with platform rates`

---

### Task 4: API scoping and the Accounts endpoints

**Files:**
- Modify: `src/api/mod.rs`, `src/api/resources.rs`, `src/api/expand.rs`, `src/api/events_v1.rs`, `src/api/events_v2.rs`
- Create: `src/api/accounts.rs`, `tests/connect_api.rs`

**Interfaces:**
- Consumes: Task 1 `store_in`, `in_scope`; Task 2 `AccountMeta`, `v2_view`; Task 3 `Generators::pagination_write(scope, …)`.
- Produces:
  - The gate resolves a `Scope` per request, following the header rules and error copy in Global Constraints, and passes it to handlers as an axum extension `RequestScope(Scope)`.
    - Resolution order: auth, then version, then the scope checks, then faults.
    - A connected scope is valid only if the account is connected **now**. This is checked on every request, so a mid-backfill disconnect fails the next page.
  - `View { engine, at, scope }`. Every retrieve, list, sub-list and `expand[]` reads `store_in(&scope)`. A missing store is treated as empty.
  - Events APIs list and retrieve only events with `ev.account == scope`. `/v1/events` uses the header scope; `/v2/core/events` uses `Stripe-Context`.
  - `GET /v1/accounts` and `GET /v1/accounts/{id}`: v1 state of connected accounts, list params as for other v1 lists. Disconnected → omitted / 404 `resource_missing`.
  - `GET /v2/core/accounts` and `GET /v2/core/accounts/{id}`.
    - The list takes `limit`, `page`, `closed` (bool, default false) and `applied_configurations` (comma-separated; an account matches if it has any of them). It uses the v2 list envelope that `/v2/core/events` uses.
    - Retrieving a closed account returns the `closed: true` view.
  - Stale reads (`View.at`) apply to accounts: an account created after `at` is not listed, and a stale read of a disconnected account shows its last live version.
- [ ] **Step 1: Write failing tests** (`tests/connect_api.rs`; reuse `tests/common` helpers, adding a `get_as(m, path, acct)` helper that sets `Stripe-Account`, and `get_ctx` for `Stripe-Context`):
  - `stripe_account_scopes_customers`:
    - `account(a).create_customer` → `GET /v1/customers` as `a` lists it.
    - As the platform it is not listed, and `GET /v1/customers/{id}` as the platform → 404 `resource_missing`.
  - `scopes_do_not_leak` (Review Focus 2):
    - Two accounts, each with a customer and events.
    - For each scope, `/v1/events` and `/v2/core/events` list only events whose payload `account`/`context` equals that scope.
    - `GET /v1/events/{other scope's id}` → 404.
    - `expand[]=data.customer` on another scope's invoice list stays within the scope.
  - `unknown_account_is_403`: `Stripe-Account: acct_nope` → 403 with `code: "account_invalid"`, the exact message, and a `Request-Id` header.
  - `disconnect_mid_backfill_is_403` (Review Focus 1):
    - With 30 customers in `a`, fetch page 1 (`limit=10`) as `a`.
    - `disconnect_account(a)`, then fetch page 2 with `starting_after`.
    - Expect 403 `account_invalid`, and the same again for page 3.
  - `header_rules`:
    - both headers different → 400;
    - both the same → 200;
    - `Stripe-Account` on `/v2/core/events` → 400;
    - `Stripe-Context` on `/v1/customers` scopes like `Stripe-Account`;
    - any header with `connect = false` → today's error;
    - `/v1/accounts` with a header → 400 with the exact copy.
  - `v1_accounts_list_both_kinds`:
    - one v1 standard and one v2 account → `/v1/accounts` lists both;
    - the v2 one has `type: "none"` and a `controller`;
    - after disconnecting one, it is omitted and its retrieve is 404.
  - `v2_accounts_view`:
    - `/v2/core/accounts/{v1 id}` has `applied_configurations == ["merchant"]` and `dashboard == "full"` (standard);
    - `object == "v2.core.account"`;
    - `created` matches `^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$`.
  - `v2_accounts_closed_filter`: a disconnected v2 account is absent by default, present with `closed=true` and `closed: true`, and retrievable with `closed: true`.
  - `v2_accounts_configuration_filter`: `applied_configurations=recipient` lists only accounts with `recipient`.
  - `stale_reads_respect_scope` (Review Focus 5):
    - With `stale_read` probability 1, max lag 10 s, connect an account and create a customer in it.
    - Immediately, `/v1/accounts` omits the new account, and `/v1/customers` as the platform never shows the account's customer.
    - After `step` 11 s, both appear in their own scopes.
  - `pagination_writes_stay_in_scope`:
    - With `pagination_writes` probability 1, page `/v1/customers?starting_after=…` as `a`.
    - The truth entries written by the request all have `account == Some(a)`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test connect_api`. Expected: failures (the header is rejected; routes are 404).
- [ ] **Step 3: Implement** the interfaces above. Remove the gate's blanket `stripe-account` rejection except when `connect = false`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): Stripe-Account scoping and Accounts endpoints`

---

### Task 5: Connect-aware destinations

**Files:**
- Modify: `src/config.rs`, `src/delivery/mod.rs`, `src/delivery/queue.rs`, `src/api/destinations.rs` (if it validates or creates destinations)
- Create: `tests/connect_delivery.rs`

**Interfaces:**
- Consumes: Task 1 `EventRecord.account`.
- Produces:
  - `validate()`:
    - accepts `connect = true` on snapshot destinations;
    - accepts `events_from` on thin destinations, as a non-empty subset of `"@self"` and `"@accounts"` with no duplicates;
    - `@organization_members` and `@organization_members/@accounts` → `` "`destinations.events_from` value `{v}` is not implemented yet (organizations)" ``;
    - `connect` on thin or `events_from` on snapshot → config error naming the key.
  - The same validation applies to `MockStripe::add_destination` and the destinations API.
  - `Destinations::matching(format, event_type, account: Option<&str>)`: the existing filters plus the rules in Decisions. `schedule_for_event` passes `ev.account`.
  - Delivered snapshot bodies carry `account`, and thin bodies carry `context` (from Task 1).
- [ ] **Step 1: Write failing tests**:
  - Config unit tests:
    - `connect_destination_validates`;
    - `events_from_rules` (each rule above);
    - `spec_example_destinations_parse`, built from the spec's two example destinations.
  - `tests/connect_delivery.rs`. All tests start with `initial = 1` and four destinations, then `step` 2 h and `quiesce()`:
    - snapshot `connect = false`;
    - snapshot `connect = true`;
    - thin `["@self"]`;
    - thin `["@self", "@accounts"]`.
    - `deliveries_route_by_account` (Review Focus 2):
      - The snapshot `connect = false` destination got only platform events.
      - The snapshot `connect = true` destination got both platform and account events, with the body `account` equal to the event's.
      - Thin `@self` got only `context: null`; the thin pair got both.
      - `v2.core.account.*` notifications reach `@self`, not `@accounts`-only destinations. Add a fifth destination `["@accounts"]` to assert this.
    - `account_lifecycle_delivered`: after a `connect_account` and a `disconnect_account` at runtime, `account.application.authorized` and `account.application.deauthorized` reach the `connect = true` snapshot destination, and their `v1.` pairs reach the `@accounts` thin destination.
    - `connected_deliveries_signed_and_retried`: a `start_outage` on the `connect = true` destination is followed by a redelivery of an account event after the outage ends. This reuses the phase 3 helpers.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test connect_delivery` and the config tests. Expected: config errors (connect not implemented) and failing assertions.
- [ ] **Step 3: Implement** the interfaces above. Remove the "phase 5" not-implemented errors for `connect` and `events_from`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): route connected-account events to connect destinations`

---

### Task 6: Subscription filter, end-to-end, spec

**Files:**
- Modify: `src/stream.rs`, `src/handle.rs`, `docs/stripe-mock-spec.md`
- Create: `tests/connect_e2e.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `SubscribeFilter { pub object_types: Option<Vec<ObjectType>>, pub accounts: Option<Vec<Scope>> }`. `None` means every scope. Snapshot and live updates both filter on `StateUpdate.account`. `Default` keeps today's behaviour.
  - Spec edits:
    - section 4: `[churn.account]`, the `[accounts]` defaults and the destination rules;
    - section 5.4: replace "never snapshot events" with the Decisions behaviour; disconnect and closed semantics;
    - section 7.1: `Stripe-Context`, header precedence, 403 `account_invalid`;
    - section 7.2: the `/v2/core/accounts` filters;
    - section 18: replace the open question with the answer and cite Stripe's Accounts v2 docs.
- [ ] **Step 1: Write failing tests**:
  - `stream.rs` unit test `filter_by_account`: subscribe with `accounts: Some(vec![Some("acct_1".into())])`. The snapshot and later updates contain only that scope.
  - `tests/connect_e2e.rs::sync_like_consumer_converges`, with the spec-example rates at one tenth:
    - `connect = true`, `initial = 3`, `connect_rate = 2`, `deauthorize_rate = 0.5`, `history.duration = "6h"`;
    - one snapshot destination with `connect = true` and one thin destination with `["@self", "@accounts"]`;
    - `step` 12 h, then `quiesce()`.
    
    For every scope that is still connected, plus the platform, a client that lists every type through the API with the right header gets the same live IDs as `engine` (via `subscribe` filtered to that scope).
    
    For every disconnected account, its header returns 403.
  - `spec_mentions_stripe_context`: the existing spec-parsing config test still parses the spec example. Add an assertion that the spec contains `Stripe-Context`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test connect_e2e` and `--lib stream`. Expected: compile error (`accounts` field) and the spec assertion fails.
- [ ] **Step 3: Implement** the filter and edit the spec, keeping LF line endings and checking with `git diff --stat` that there are no whole-file changes.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): account subscription filter, Connect e2e, spec updates`
