# Stripe Mock Phase 1 (Engine Skeleton) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create the `ductkit-stripe-mock` library crate with a deterministic engine (virtual and wall clock, seeded RNG streams, truth log with per-object versions), stochastic and control-triggered `customer` create/update/delete, a latest-state subscription, and a read-only Stripe-compatible HTTP API for retrieving and listing customers, all running in-process.

**Architecture:** A single `Sim` (engine + generator + control RNG) sits behind one `std::sync::Mutex`, shared through `Arc` by the `MockStripe` handle, an axum server on `127.0.0.1:0`, and (in wall mode) a ticker task. Every mutation goes through `Engine`, which appends a `StateUpdate` to the truth log, updates the object store and fans out to subscribers with non-blocking `try_send`. The HTTP layer only reads.

**Tech Stack:** Rust edition 2024; tokio; axum; serde, serde_json, toml; chrono (with `serde`); rand_chacha (+ rand_core); thiserror; futures-core. Dev: reqwest (`json`), futures-util. Use the latest release of each, declared once in the root `[workspace.dependencies]`.

**Spec:** [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md). Phase 1 is item 1 of section 16: "clock, RNG, truth log, versions, latest-state subscription, control handle; `customer` only; read-only retrieve/list; in-process mode."

## Global Constraints

- Crate directory `crates/stripe-mock`, package `ductkit-stripe-mock`, imported as `ductkit_stripe_mock`. Phase 1 ships the library only; the binary arrives in phase 6.
- Pinned API version, exactly: `2026-11-18.endive` (`pub const PINNED_API_VERSION`).
- The Stripe-compatible API is read-only. Every `POST`, `PUT`, `PATCH` and `DELETE` returns a Stripe-style `invalid_request_error`.
- Stripe error envelope: `{"error": {"type", "message", "code"?, "param"?}}`. Every response carries a `Request-Id` header.
- Lists: ordered by `created` descending, `limit` 1–100 with default 10, cursors `starting_after` / `ending_before`, response `{"object": "list", "url", "has_more", "data"}`.
- Customer IDs have the prefix `cus_`. Every object has `livemode: false`. Timestamps in API objects are Unix seconds.
- Determinism: nothing in the engine or generator may read the system clock (except the wall-mode ticker), use thread-local RNG, or depend on `HashMap` iteration order. Same seed + same config + same control calls gives an identical truth log.
- Per-object `version` starts at 1 and increases by 1 per mutation. Global `seq` starts at 1 and is gap-free.
- Workspace conventions (CLAUDE.md): metadata inherited with `*.workspace = true`, dependency versions only in root `[workspace.dependencies]`, `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` clean before every commit.

## Review Focus

1. **The spec's full example config fed to phase 1**: the user expects a clear error naming the first unimplemented section (e.g. `[webhooks]`), not a generic serde error or silent ignoring. Test in Task 1.
2. **Many customers created within the same second, then paginated**: the user expects every customer exactly once across pages, in a stable order. Test in Task 8.
3. **A pagination cursor pointing at a customer that was deleted between pages**: the user expects the walk to continue from that position, not fail. Test in Task 8.
4. **Garbage or out-of-range `limit` (`0`, `101`, `abc`)**: the user expects a Stripe-style 400 naming `limit`, not a 500 or a silent clamp. Test in Task 8.
5. **A control call updating or deleting a customer that is already deleted**: the user expects an error and an unchanged truth log. Test in Task 5.

## Deferred (not in phase 1)

Events and webhooks (phase 2); other object types, fan-out actions and scripted scenarios (phase 3); history, API faults and stale reads (phase 4); Connect and the `Stripe-Account` header (phase 5); binary, HTTP control API, SSE/WebSocket, export/replay (phase 6); OpenAPI-generated types (phase 7). Quiesce is deferred to phase 2, because in phase 1 there is nothing to flush.

## File Structure

```
crates/stripe-mock/
├── Cargo.toml
├── src/
│   ├── lib.rs            public re-exports only
│   ├── error.rs          MockError
│   ├── config.rs         Config structs, TOML parsing, validation
│   ├── time.rs           SimTime
│   ├── rng.rs            fnv1a64, RngStreams, SimRng (sampling, IDs)
│   ├── model/mod.rs      ObjectType
│   ├── model/customer.rs customer JSON: build, update, deleted stub, random data
│   ├── engine/mod.rs     Engine: now, truth log, mutations, subscriber fan-out; StateUpdate types
│   ├── engine/store.rs   ObjectStore: latest state, creation order, list order
│   ├── generate.rs       Generator: Poisson scheduling of customer actions
│   ├── stream.rs         SubscribeFilter/Options, SubscriptionItem, Subscription
│   ├── handle.rs         MockStripe (public control handle), Sim, wall ticker
│   └── api/
│       ├── mod.rs        router, auth/version/read-only/Request-Id layer, ApiError
│       ├── list.rs       list query parsing and pagination
│       └── customers.rs  retrieve and list handlers
└── tests/
    ├── common/mod.rs     mock_with(toml), get(mock, path) helpers
    ├── engine.rs         determinism and control-call tests (Task 5)
    ├── subscription.rs   (Task 6)
    ├── api_basics.rs     (Task 7)
    ├── api_customers.rs  (Task 8)
    └── wall_clock.rs     (Task 9)
```

---

### Task 1: Crate scaffold, errors and config

**Files:**
- Create: `crates/stripe-mock/Cargo.toml`, `src/lib.rs`, `src/error.rs`, `src/config.rs`
- Modify: root `Cargo.toml` (`[workspace.dependencies]`), `README.md` (crate table), `CLAUDE.md` (crate list), `docs/stripe-mock-spec.md` (section 4 example)

**Interfaces:**
- Produces:
  - `pub const PINNED_API_VERSION: &str = "2026-11-18.endive";`
  - `pub enum MockError { Config(String), ClockMode(String), TimeTravel { requested: SimTime, now: SimTime }, NoSuchObject { object_type: ObjectType, id: String }, Deleted { object_type: ObjectType, id: String }, Io(std::io::Error) }`, deriving `thiserror::Error`. (`SimTime` and `ObjectType` come from Tasks 2–3. Add those variants in the task that defines their types.)
  - `pub struct Config { pub seed: u64, pub api_version: String, pub clock: ClockConfig, pub api: ApiConfig, pub actions: ActionsConfig, pub churn: ChurnConfig }`
  - `pub struct ClockConfig { pub mode: ClockMode, pub start: chrono::DateTime<Utc>, pub speed: f64 }`, `pub enum ClockMode { Virtual, Wall }` (serde `lowercase`)
  - `pub struct ApiConfig { pub keys: Vec<String> }`, `pub struct ActionsConfig { pub new_customer: f64 }`, `pub struct ChurnConfig { pub customer: CustomerChurn }`, `pub struct CustomerChurn { pub update: f64, pub delete: f64 }`
  - `impl Config { pub fn from_toml_str(s: &str) -> Result<Config, MockError>; pub fn from_file(path: &Path) -> Result<Config, MockError>; pub fn validate(&self) -> Result<(), MockError> }`
  - `impl Default for Config`: seed `0`, api_version pinned, clock `{ Virtual, 2026-01-01T00:00:00Z, 1.0 }`, keys `["sk_test_mock"]`, all rates `0.0`.

All config structs use `#[serde(default, deny_unknown_fields)]`. Rates are events per simulated hour.

- [ ] **Step 1: Create the crate and add it to the workspace**

`cargo new --lib --vcs none --name ductkit-stripe-mock crates/stripe-mock`, then switch its metadata to workspace inheritance. Add `serde` (derive), `toml`, `chrono` (serde), `thiserror` to root `[workspace.dependencies]` and to the crate as `x.workspace = true`.

- [ ] **Step 2: Write failing tests in `src/config.rs`**

```rust
#[test] fn defaults() {
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.api_version, "2026-11-18.endive");
    assert_eq!(c.clock.mode, ClockMode::Virtual);
    assert_eq!(c.clock.start.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    assert_eq!(c.api.keys, vec!["sk_test_mock".to_string()]);
    assert_eq!(c.actions.new_customer, 0.0);
}
#[test] fn parses_phase1_keys() {
    let c = Config::from_toml_str(r#"
        seed = 42
        clock = { mode = "wall", start = "2026-03-01T00:00:00Z", speed = 60.0 }
        [api]
        keys = ["sk_test_a"]
        [actions]
        new_customer = 10.0
        [churn.customer]
        update = 3.0
        delete = 0.2
    "#).unwrap();
    assert_eq!((c.seed, c.clock.mode, c.clock.speed), (42, ClockMode::Wall, 60.0));
    assert_eq!((c.churn.customer.update, c.churn.customer.delete), (3.0, 0.2));
}
#[test] fn rejects_other_api_version() {
    let e = Config::from_toml_str(r#"api_version = "2025-01-01.acacia""#).unwrap_err();
    assert!(e.to_string().contains("2026-11-18.endive"));
}
#[test] fn unimplemented_section_is_named() {          // Review Focus 1
    let e = Config::from_toml_str("[webhooks]\nduplicate_probability = 0.01").unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("[webhooks]") && msg.contains("not implemented"), "{msg}");
}
#[test] fn spec_example_config_fails_clearly() {      // Review Focus 1
    let spec = include_str!("../../../docs/stripe-mock-spec.md");
    let toml = spec.split("```toml\n").nth(1).unwrap().split("```").next().unwrap();
    let msg = Config::from_toml_str(toml).unwrap_err().to_string();
    assert!(msg.contains("[accounts]") && msg.contains("not implemented"), "{msg}");
}
#[test] fn rejects_bad_values() {
    for bad in ["[actions]\nnew_customer = -1.0", "clock = { speed = 0.0 }", "[api]\nkeys = []", "[api]\nkeys = [\"sk_live_x\"]"] {
        assert!(Config::from_toml_str(bad).is_err(), "{bad}");
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p ductkit-stripe-mock config::`
Expected: compile errors (types not defined).

- [ ] **Step 4: Implement `config.rs` and `error.rs`**

`from_toml_str` first parses into a `toml::Table`. If any top-level key is in `["accounts", "history", "event_retention", "events", "webhooks", "payloads", "destinations"]`, it returns `MockError::Config("`[<key>]` is not implemented yet (phase 1 supports seed, api_version, clock, api.keys, actions.new_customer, churn.customer)")`. Check keys in that list order so the result is deterministic. Otherwise it deserializes and then calls `validate`. `validate` rejects: `api_version != PINNED_API_VERSION` (the message names the pinned version); any rate that is not finite or is `< 0`; `speed` that is not finite or is `<= 0`; empty `keys`; any key not starting with `sk_test_`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p ductkit-stripe-mock config::`
Expected: 6 passed.

- [ ] **Step 6: Update docs**

- Spec section 4 example: add `keys = ["sk_test_mock"]   # accepted API keys (section 7.1)` to `[api]`.
- Spec section 4 example: change the clock comment to `# mode: virtual (time moves only when advanced) | wall (real time × speed)`.
- README crate table: add a row for `ductkit-stripe-mock` (library, `crates/stripe-mock`).
- CLAUDE.md crate list: add `crates/stripe-mock` (package `ductkit-stripe-mock`), the Stripe mock test fixture, see `docs/stripe-mock-spec.md`.

- [ ] **Step 7: fmt, clippy, commit**

`cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`, then `git commit -m "feat(stripe-mock): crate scaffold and config"`.

---

### Task 2: Simulated time and RNG streams

**Files:**
- Create: `src/time.rs`, `src/rng.rs`
- Modify: root and crate `Cargo.toml` (add `rand_chacha`, `rand_core`), `src/error.rs` (`TimeTravel`)

**Interfaces:**
- Produces:
  - `#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)] pub struct SimTime(pub i64)`: milliseconds since the Unix epoch.
  - `impl SimTime { pub fn from_datetime(dt: DateTime<Utc>) -> Self; pub fn to_datetime(self) -> DateTime<Utc>; pub fn unix_seconds(self) -> i64 /* floor */; pub fn plus_ms(self, ms: i64) -> Self }`
  - `SimTime` serializes as a string in `%Y-%m-%dT%H:%M:%S%.3fZ` format (e.g. `"2026-01-01T00:12:03.120Z"`).
  - `pub(crate) fn fnv1a64(bytes: &[u8]) -> u64` (offset `0xcbf29ce484222325`, prime `0x100000001b3`)
  - `pub(crate) struct RngStreams { seed: u64 }` with `new(seed: u64)` and `stream(&self, name: &str) -> SimRng`
  - `pub(crate) struct SimRng(ChaCha8Rng)` with `next_u64`, `unit_f64() -> f64` (in `[0,1)`), `below(n: u64) -> u64`, `exp_interval_ms(rate_per_hour: f64) -> Option<i64>`, `id(prefix: &str) -> String`, `alnum_upper(len: usize) -> String`, `choose<'a, T>(&mut self, items: &'a [T]) -> &'a T`

Algorithms (fixed here so that replays stay stable across dependency upgrades):
- `stream`: `ChaCha8Rng::seed_from_u64(seed)` followed by `set_stream(fnv1a64(name.as_bytes()))`.
- `unit_f64`: `(next_u64() >> 11) as f64 * 2f64.powi(-53)`.
- `below(n)`: rejection sampling. Draw `x = next_u64()` and reject while `x >= u64::MAX - (u64::MAX % n)`, then return `x % n`.
- `exp_interval_ms(rate)`: `None` if `rate == 0.0`, otherwise `Some(max(1, ceil(-ln(1 - u) / rate * 3_600_000)))` where `u = unit_f64()`.
- `id(prefix)`: `format!("{prefix}_{14 chars}")`, each char `ALNUM[below(62)]` with `ALNUM = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"`.
- `alnum_upper(len)`: chars from `b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"` via `below(36)`.

- [ ] **Step 1: Write failing tests (unit tests in each file)**

```rust
// time.rs
#[test] fn formats_ms() {
    let t = SimTime::from_datetime("2026-01-01T00:12:03.120Z".parse().unwrap());
    assert_eq!(serde_json::to_value(t).unwrap(), "2026-01-01T00:12:03.120Z");
    assert_eq!(t.unix_seconds(), 1767226323);
    assert_eq!(SimTime(-1).unix_seconds(), -1);
}
// rng.rs
#[test] fn fnv_vectors() {
    assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
    assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
}
#[test] fn streams_are_deterministic_and_independent() {
    let s = RngStreams::new(7);
    let a: Vec<u64> = (0..100).map({ let mut r = s.stream("x"); move |_| r.next_u64() }).collect();
    let b: Vec<u64> = (0..100).map({ let mut r = s.stream("x"); move |_| r.next_u64() }).collect();
    let c: Vec<u64> = (0..100).map({ let mut r = s.stream("y"); move |_| r.next_u64() }).collect();
    let d: Vec<u64> = (0..100).map({ let mut r = RngStreams::new(8).stream("x"); move |_| r.next_u64() }).collect();
    assert_eq!(a, b); assert_ne!(a, c); assert_ne!(a, d);
}
#[test] fn golden_first_value() {
    // Fails if a dependency upgrade changes the output, which would break replays of recorded runs.
    assert_eq!(RngStreams::new(42).stream("generate.customer.create").next_u64(), GOLDEN);
}
#[test] fn id_format() {
    let id = RngStreams::new(1).stream("ids").id("cus");
    assert!(id.starts_with("cus_") && id.len() == 18 && id[4..].bytes().all(|b| b.is_ascii_alphanumeric()));
}
#[test] fn exp_intervals() {
    let mut r = RngStreams::new(3).stream("exp");
    assert_eq!(r.exp_interval_ms(0.0), None);
    let n = 20_000;
    let mean = (0..n).map(|_| r.exp_interval_ms(60.0).unwrap()).sum::<i64>() as f64 / n as f64;
    assert!((57_000.0..63_000.0).contains(&mean), "{mean}");   // 60/h → mean 60 000 ms
    assert!((0..1000).all(|_| r.exp_interval_ms(1e12).unwrap() >= 1));
}
#[test] fn below_in_range() {
    let mut r = RngStreams::new(5).stream("b");
    assert!((0..10_000).all(|_| r.below(62) < 62));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ductkit-stripe-mock time:: rng::`
Expected: compile errors.

- [ ] **Step 3: Implement `time.rs` and `rng.rs`** using the signatures and algorithms above. Implement `golden_first_value` with `const GOLDEN: u64 = 0;` for now.

- [ ] **Step 4: Pin the golden value**

Run `cargo test -p ductkit-stripe-mock rng::golden_first_value`. It fails and prints the actual value. Paste that value into `GOLDEN`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p ductkit-stripe-mock time:: rng::`
Expected: 7 passed.

- [ ] **Step 6: fmt, clippy, commit** — `feat(stripe-mock): simulated time and seeded RNG streams`

---

### Task 3: Customer model

**Files:**
- Create: `src/model/mod.rs`, `src/model/customer.rs`
- Modify: `src/error.rs` (`NoSuchObject`, `Deleted`), add `serde_json` to the crate

**Interfaces:**
- Consumes: `SimRng` (Task 2).
- Produces:
  - `#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)] #[serde(rename_all = "snake_case")] pub enum ObjectType { Customer }` with `as_str() -> &'static str` (`"customer"`) and `id_prefix() -> &'static str` (`"cus"`). `Display` prints `as_str()`.
  - `pub struct NewCustomer { pub email: Option<String>, pub name: Option<String>, pub description: Option<String>, pub metadata: BTreeMap<String, String> }` (`Default`)
  - `pub enum CustomerUpdate { SetEmail(Option<String>), SetName(Option<String>), SetDescription(Option<String>), SetMetadata { key: String, value: String }, RemoveMetadata { key: String } }`
  - `pub(crate) fn new_customer_json(id: &str, created: i64, invoice_prefix: &str, p: &NewCustomer) -> Value`
  - `pub(crate) fn apply_update(state: &mut Value, u: &CustomerUpdate)`
  - `pub(crate) fn deleted_stub(id: &str) -> Value`: `{"id": id, "object": "customer", "deleted": true}`
  - `pub(crate) fn random_new_customer(rng: &mut SimRng) -> NewCustomer`
  - `pub(crate) fn random_update(rng: &mut SimRng, state: &Value) -> CustomerUpdate`

Customer fields, in this order (`serde_json` with the `preserve_order` feature). They are hand-written until phase 7 generates them from OpenAPI:

| Field | Value |
|---|---|
| `id` | argument |
| `object` | `"customer"` |
| `address` | `null` |
| `balance` | `0` |
| `created` | argument (Unix seconds) |
| `currency` | `null` |
| `default_source` | `null` |
| `delinquent` | `false` |
| `description` | `p.description` |
| `discount` | `null` |
| `email` | `p.email` |
| `invoice_prefix` | argument (8 chars, `alnum_upper`) |
| `invoice_settings` | `{"custom_fields": null, "default_payment_method": null, "footer": null, "rendering_options": null}` |
| `livemode` | `false` |
| `metadata` | `p.metadata` |
| `name` | `p.name` |
| `next_invoice_sequence` | `1` |
| `phone` | `null` |
| `preferred_locales` | `[]` |
| `shipping` | `null` |
| `tax_exempt` | `"none"` |
| `test_clock` | `null` |

Random data. Draw every choice from the given `rng`, in this order: email, name, description, metadata.
- **Names**: one of `["Test Customer", "Zoë Fischer", "山田 太郎", "O'Brien & Sons", "Ünïcödé Ltd"]`.
- **Email**: `user{below(1_000_000)}@example.com`.
- **Description**: `None` half the time.
- **Metadata on create**: 0–2 entries. Keys come from `["plan", "tier", "source", "note"]`. Values come from `["", "gold", "silver", "x".repeat(500)]`, where 500 is Stripe's maximum value length.
- **`random_update`**: picks uniformly among the five variants. `RemoveMetadata` falls back to `SetMetadata` when the metadata is empty.

- [ ] **Step 1: Write failing tests in `model/customer.rs`**

```rust
#[test] fn new_customer_fields() {
    let p = NewCustomer { email: Some("a@example.com".into()), ..Default::default() };
    let v = new_customer_json("cus_1", 1767225600, "ABCD1234", &p);
    assert_eq!(v["object"], "customer"); assert_eq!(v["created"], 1767225600);
    assert_eq!(v["email"], "a@example.com"); assert_eq!(v["name"], Value::Null);
    assert_eq!(v["livemode"], false); assert_eq!(v["metadata"], json!({}));
    assert_eq!(v.as_object().unwrap().keys().next().unwrap(), "id");
    assert_eq!(v.as_object().unwrap().len(), 22);
}
#[test] fn updates_apply() {
    let mut v = new_customer_json("cus_1", 0, "ABCD1234", &NewCustomer::default());
    apply_update(&mut v, &CustomerUpdate::SetMetadata { key: "tier".into(), value: "gold".into() });
    apply_update(&mut v, &CustomerUpdate::SetName(Some("Zoë".into())));
    assert_eq!(v["metadata"], json!({"tier": "gold"})); assert_eq!(v["name"], "Zoë");
    apply_update(&mut v, &CustomerUpdate::RemoveMetadata { key: "tier".into() });
    assert_eq!(v["metadata"], json!({}));
}
#[test] fn stub() { assert_eq!(deleted_stub("cus_1"), json!({"id": "cus_1", "object": "customer", "deleted": true})); }
#[test] fn random_is_deterministic() {
    let s = RngStreams::new(9);
    let (mut a, mut b) = (s.stream("c"), s.stream("c"));
    for _ in 0..50 {
        let (x, y) = (random_new_customer(&mut a), random_new_customer(&mut b));
        assert_eq!(format!("{x:?}"), format!("{y:?}"));
    }
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock model::` → compile errors.
- [ ] **Step 3: Implement** the signatures above (`NewCustomer` and `CustomerUpdate` derive `Debug, Clone`).
- [ ] **Step 4: Run to verify they pass** — 4 passed.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): customer model`

---

### Task 4: Engine, truth log and object store

**Files:**
- Create: `src/engine/mod.rs`, `src/engine/store.rs`

**Interfaces:**
- Consumes: `SimTime`, `ObjectType`, `MockError`.
- Produces (types are public and re-exported; `Engine` and `ObjectStore` are `pub(crate)`):
  - `#[serde(rename_all = "snake_case")] pub enum ChangeKind { Snapshot, Created, Updated, Deleted }`
  - `pub struct Cause { pub action: String, pub action_id: String }`
  - `#[derive(Default)] pub struct EventRefs { pub snapshot: Vec<String>, pub thin: Vec<String> }` (always empty in phase 1, always serialized)
  - `pub struct StateUpdate { pub seq: u64, pub at: SimTime, pub account: Option<String>, pub object_type: ObjectType, pub object_id: String, pub version: u64, pub kind: ChangeKind, pub state: Value, pub events: EventRefs, pub cause: Option<Cause> }`. Derives `Debug, Clone, PartialEq, Serialize`. `account` is always `None` in phase 1 and serializes as `null`.
  - `Engine::new(start: SimTime) -> Engine`, `now()`, `set_now(t: SimTime)` (`debug_assert!(t >= now)`)
  - `Engine::begin_action(&mut self, name: &str) -> Cause`: `action_id` is `format!("act_{n}")` with `n` from 1
  - `Engine::create(&mut self, t: ObjectType, id: String, state: Value, cause: Cause) -> StateUpdate`
  - `Engine::update(&mut self, t: ObjectType, id: &str, f: impl FnOnce(&mut Value), cause: Cause) -> Result<Option<StateUpdate>, MockError>`: returns `Ok(None)` and records nothing if `f` leaves the state unchanged.
  - `Engine::delete(&mut self, t: ObjectType, id: &str, tombstone: Value, cause: Cause) -> Result<StateUpdate, MockError>`
  - `Engine::latest(&self, t, id) -> Option<&StateUpdate>`, `Engine::log(&self) -> &[StateUpdate]`, `Engine::store(&self) -> &ObjectStore`
  - `pub(crate) struct StoredObject { pub latest: StateUpdate, pub created: i64, pub creation_index: usize, pub deleted: bool }`
  - `ObjectStore::get(t, id) -> Option<&StoredObject>`, `ObjectStore::list_order(t) -> Vec<&StoredObject>` (all objects including deleted, sorted by `created` desc, then `creation_index` desc), `ObjectStore::live_ids(t) -> Vec<&str>` (creation order, not deleted)

`created` is read from `state["created"]` at creation. Use `BTreeMap<ObjectType, …>` per type, with a `Vec<String>` for creation order and a `HashMap<String, StoredObject>` for lookups. The `HashMap` is never iterated.

- [ ] **Step 1: Write failing tests in `engine/mod.rs`**

```rust
fn cust(id: &str, created: i64) -> Value { json!({"id": id, "object": "customer", "created": created, "email": null}) }
#[test] fn seq_and_versions() {
    let mut e = Engine::new(SimTime(0));
    let c = e.begin_action("t");
    let a = e.create(ObjectType::Customer, "cus_a".into(), cust("cus_a", 0), c.clone());
    let b = e.update(ObjectType::Customer, "cus_a", |v| v["email"] = json!("x@example.com"), c.clone()).unwrap().unwrap();
    let d = e.delete(ObjectType::Customer, "cus_a", json!({"id": "cus_a", "object": "customer", "deleted": true}), c).unwrap();
    assert_eq!((a.seq, b.seq, d.seq), (1, 2, 3));
    assert_eq!((a.version, b.version, d.version), (1, 2, 3));
    assert_eq!((a.kind, b.kind, d.kind), (ChangeKind::Created, ChangeKind::Updated, ChangeKind::Deleted));
    assert_eq!(e.log().len(), 3);
    assert!(e.store().get(ObjectType::Customer, "cus_a").unwrap().deleted);
    assert!(e.store().live_ids(ObjectType::Customer).is_empty());
}
#[test] fn noop_update_records_nothing() {
    let mut e = Engine::new(SimTime(0)); let c = e.begin_action("t");
    e.create(ObjectType::Customer, "cus_a".into(), cust("cus_a", 0), c.clone());
    assert_eq!(e.update(ObjectType::Customer, "cus_a", |_| {}, c).unwrap(), None);
    assert_eq!(e.log().len(), 1);
}
#[test] fn mutating_deleted_or_missing_errors() {
    let mut e = Engine::new(SimTime(0)); let c = e.begin_action("t");
    assert!(matches!(e.update(ObjectType::Customer, "cus_x", |_| {}, c.clone()), Err(MockError::NoSuchObject { .. })));
    e.create(ObjectType::Customer, "cus_a".into(), cust("cus_a", 0), c.clone());
    e.delete(ObjectType::Customer, "cus_a", json!({}), c.clone()).unwrap();
    assert!(matches!(e.delete(ObjectType::Customer, "cus_a", json!({}), c), Err(MockError::Deleted { .. })));
    assert_eq!(e.log().len(), 2);
}
#[test] fn list_order_breaks_ties_by_creation() {
    let mut e = Engine::new(SimTime(0)); let c = e.begin_action("t");
    for (id, t) in [("cus_1", 5), ("cus_2", 5), ("cus_3", 4)] { e.create(ObjectType::Customer, id.into(), cust(id, t), c.clone()); }
    let ids: Vec<_> = e.store().list_order(ObjectType::Customer).iter().map(|o| o.latest.object_id.clone()).collect();
    assert_eq!(ids, ["cus_2", "cus_1", "cus_3"]);
}
#[test] fn begin_action_ids() {
    let mut e = Engine::new(SimTime(0));
    assert_eq!(e.begin_action("a").action_id, "act_1"); assert_eq!(e.begin_action("b").action_id, "act_2");
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock engine::` → compile errors.
- [ ] **Step 3: Implement** `engine/mod.rs` and `engine/store.rs` per the interfaces. Leave a `fn notify(&mut self, u: &StateUpdate) {}` hook called after every recorded mutation; Task 6 fills it in.
- [ ] **Step 4: Run to verify they pass** — 5 passed.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): engine truth log and object store`

---

### Task 5: Generator and control handle (virtual clock)

**Files:**
- Create: `src/generate.rs`, `src/handle.rs`, `tests/common/mod.rs`, `tests/engine.rs`
- Modify: `src/lib.rs` (re-exports), `src/error.rs` (`ClockMode`), add `tokio` (`rt-multi-thread`, `macros`, `sync`, `time`, `net`) to the crate

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces:
  - `pub(crate) struct Generator`, `Generator::new(config: &Config, streams: &RngStreams, start: SimTime) -> Generator`, `Generator::run_until(&mut self, engine: &mut Engine, target: SimTime)`
  - `pub(crate) struct Sim { pub engine: Engine, pub generator: Generator, pub control_rng: SimRng }`
  - `pub struct MockStripe { shared: Arc<Shared> }` where `pub(crate) struct Shared { pub sim: Mutex<Sim>, pub config: Config, … }` (Tasks 7 and 9 add fields)
  - `impl MockStripe`:
    - `pub async fn start(config: Config) -> Result<MockStripe, MockError>` (calls `config.validate()`)
    - `pub fn now(&self) -> SimTime`
    - `pub fn step(&self, d: std::time::Duration) -> Result<(), MockError>`
    - `pub fn advance_to(&self, t: SimTime) -> Result<(), MockError>`: `TimeTravel` if `t < now`; equal is a no-op `Ok`
    - `pub fn create_customer(&self, p: NewCustomer) -> Result<String, MockError>`
    - `pub fn update_customer(&self, id: &str, u: CustomerUpdate) -> Result<Option<u64>, MockError>` (new version, or `None` if nothing changed)
    - `pub fn delete_customer(&self, id: &str) -> Result<(), MockError>`
    - `pub fn get(&self, t: ObjectType, id: &str) -> Option<StateUpdate>`
    - `pub fn truth_log(&self) -> Vec<StateUpdate>`
  - `lib.rs` re-exports: `Config, ClockConfig, ClockMode, MockError, MockStripe, ObjectType, NewCustomer, CustomerUpdate, StateUpdate, ChangeKind, Cause, EventRefs, SimTime, PINNED_API_VERSION`
  - `tests/common/mod.rs`: `pub async fn mock_with(toml: &str) -> MockStripe`, which parses the config and starts the mock, panicking on error.

Generator rules:
- **Actions and streams.** The generator runs three actions in this fixed tie order:
  - `NewCustomer`: rate `actions.new_customer`, stream `"generate.customer.create"`
  - `CustomerUpdate`: rate `churn.customer.update`, stream `"generate.customer.update"`
  - `CustomerDelete`: rate `churn.customer.delete`, stream `"generate.customer.delete"`
- **Timing.** Each action draws its timing, target choice and content from its own stream. Its first due time is `start + exp_interval_ms(rate)`.
- **`run_until(target)`.** Repeatedly take the earliest due action, breaking ties by the order above, while `due <= target`. For each one, call `engine.set_now(due)`, perform the action, then reschedule it at `due + exp_interval_ms(rate)`. Finish with `engine.set_now(target)`.
- **Create.** The ID is `rng.id("cus")`, the invoice prefix is `rng.alnum_upper(8)`, and the content is `random_new_customer`. `created = now.unix_seconds()`. The cause is `begin_action("new_customer")`.
- **Update and delete.** The target is `live_ids[below(len)]`. When there are no live customers, record nothing and still reschedule. Update uses `random_update` with cause `"customer_update"`. Delete uses `deleted_stub` with cause `"customer_delete"`.
- **Control calls.** `create_customer` and friends act at `now`, draw IDs from `control_rng`, which is stream `"control"`, and use causes `"control.create_customer"`, `"control.update_customer"` and `"control.delete_customer"`.
- **Clock mode.** `step` and `advance_to` return `MockError::ClockMode` if `config.clock.mode == Wall`.

- [ ] **Step 1: Write failing tests in `tests/engine.rs`**

```rust
const BUSY: &str = "seed = 7\n[actions]\nnew_customer = 10.0\n[churn.customer]\nupdate = 30.0\ndelete = 2.0";
fn hours(h: u64) -> Duration { Duration::from_secs(h * 3600) }

#[tokio::test] async fn same_seed_same_log() {
    let (a, b) = (mock_with(BUSY).await, mock_with(BUSY).await);
    a.step(hours(48)).unwrap(); b.step(hours(48)).unwrap();
    let (la, lb) = (a.truth_log(), b.truth_log());
    assert!(la.len() > 100); assert_eq!(la, lb);
}
#[tokio::test] async fn different_seed_different_log() {
    let a = mock_with(BUSY).await; let b = mock_with(&BUSY.replace("seed = 7", "seed = 8")).await;
    a.step(hours(48)).unwrap(); b.step(hours(48)).unwrap();
    assert_ne!(a.truth_log(), b.truth_log());
}
#[tokio::test] async fn creates_independent_of_update_rate() {
    let created = |m: &MockStripe| m.truth_log().into_iter().filter(|u| u.kind == ChangeKind::Created).map(|u| (u.object_id, u.at)).collect::<Vec<_>>();
    let a = mock_with("seed = 7\n[actions]\nnew_customer = 10.0").await;
    let b = mock_with("seed = 7\n[actions]\nnew_customer = 10.0\n[churn.customer]\nupdate = 30.0").await;
    a.step(hours(24)).unwrap(); b.step(hours(24)).unwrap();
    assert_eq!(created(&a), created(&b));
}
#[tokio::test] async fn rate_roughly_matches() {
    let m = mock_with("[actions]\nnew_customer = 10.0").await;
    m.step(hours(100)).unwrap();
    assert!((850..=1150).contains(&m.truth_log().len()));
}
#[tokio::test] async fn zero_rates_do_nothing() {
    let m = mock_with("").await; m.step(hours(1000)).unwrap(); assert!(m.truth_log().is_empty());
}
#[tokio::test] async fn clock_moves_exactly() {
    let m = mock_with("").await; let t0 = m.now();
    m.step(Duration::from_millis(1500)).unwrap(); assert_eq!(m.now(), t0.plus_ms(1500));
    assert!(m.advance_to(m.now()).is_ok());
    assert!(matches!(m.advance_to(t0), Err(MockError::TimeTravel { .. })));
}
#[tokio::test] async fn control_calls() {
    let m = mock_with("").await;
    let id = m.create_customer(NewCustomer { email: Some("a@example.com".into()), ..Default::default() }).unwrap();
    assert!(id.starts_with("cus_"));
    assert_eq!(m.update_customer(&id, CustomerUpdate::SetName(Some("A".into()))).unwrap(), Some(2));
    assert_eq!(m.update_customer(&id, CustomerUpdate::SetName(Some("A".into()))).unwrap(), None);
    m.delete_customer(&id).unwrap();
    let last = m.get(ObjectType::Customer, &id).unwrap();
    assert_eq!((last.kind, last.version), (ChangeKind::Deleted, 3));
    assert_eq!(last.cause.unwrap().action, "control.delete_customer");
}
#[tokio::test] async fn control_on_deleted_customer_errors() {   // Review Focus 5
    let m = mock_with("").await;
    let id = m.create_customer(NewCustomer::default()).unwrap(); m.delete_customer(&id).unwrap();
    let before = m.truth_log().len();
    assert!(matches!(m.update_customer(&id, CustomerUpdate::SetEmail(None)), Err(MockError::Deleted { .. })));
    assert!(matches!(m.delete_customer(&id), Err(MockError::Deleted { .. })));
    assert!(matches!(m.delete_customer("cus_nope"), Err(MockError::NoSuchObject { .. })));
    assert_eq!(m.truth_log().len(), before);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test engine` → compile errors.
- [ ] **Step 3: Implement** `generate.rs`, `handle.rs` and the `lib.rs` re-exports. Never hold the `Sim` lock across an `.await`.
- [ ] **Step 4: Run to verify they pass** — 8 passed.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): generator and control handle`

---

### Task 6: Latest-state subscription

**Files:**
- Create: `src/stream.rs`, `tests/subscription.rs`
- Modify: `src/engine/mod.rs` (subscriber registry, `notify`), `src/handle.rs`, `src/lib.rs`, `docs/stripe-mock-spec.md` (sections 10.3, 10.4), add `futures-core` (crate) and `futures-util` (dev)

**Interfaces:**
- Consumes: `Engine`, `StateUpdate`.
- Produces:
  - `#[derive(Default, Clone)] pub struct SubscribeFilter { pub object_types: Option<Vec<ObjectType>> }`: `None` means all types.
  - `pub struct SubscribeOptions { pub buffer: usize }`, default `1024`
  - `#[derive(Debug, Clone, PartialEq)] pub enum SubscriptionItem { Update(StateUpdate), Synced, Lagged }`
  - `pub struct Subscription`, implementing `futures_core::Stream<Item = SubscriptionItem>` and `Unpin`
  - `MockStripe::subscribe(&self, f: SubscribeFilter) -> Subscription`, `MockStripe::subscribe_with(&self, f: SubscribeFilter, o: SubscribeOptions) -> Subscription`

Behaviour:
- **Registration.** Under one `Sim` lock, build the snapshot and register the subscriber, so there is no gap and no duplicate between the two.
- **Snapshot.** One item per matching object, in `ObjectType` order and then creation order. Each item is the object's latest `StateUpdate` with `seq`, `at` and `version` kept, `events` empty and `cause: None`. `kind` is `Snapshot` for live objects and `Deleted` for tombstones. The snapshot items plus a trailing `Synced` are held in a `VecDeque` inside the `Subscription`, so they don't count against `buffer`.
- **Live updates.** Each subscriber has a `tokio::sync::mpsc::channel(buffer)`. `Engine::notify` calls `try_send` on every matching subscriber.
  - `Full`: set that subscriber's `Arc<AtomicBool>` lagged flag and drop its sender.
  - `Closed`: drop its sender.
  - In both cases the engine never waits.
- **`poll_next`.** Drain the deque first, then the receiver. When the receiver ends, yield `Lagged` once if the flag is set, then `None`.

- [ ] **Step 1: Write failing tests in `tests/subscription.rs`**

```rust
async fn next(s: &mut Subscription) -> Option<SubscriptionItem> { tokio::time::timeout(Duration::from_secs(5), s.next()).await.unwrap() }

#[tokio::test] async fn snapshot_then_synced_then_live() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    let mut s = m.subscribe(SubscribeFilter::default());
    let SubscriptionItem::Update(u) = next(&mut s).await.unwrap() else { panic!() };
    assert_eq!((u.kind, u.object_id.as_str(), u.version, u.cause), (ChangeKind::Snapshot, a.as_str(), 1, None));
    assert_eq!(next(&mut s).await.unwrap(), SubscriptionItem::Synced);
    m.update_customer(&a, CustomerUpdate::SetName(Some("B".into()))).unwrap();
    let SubscriptionItem::Update(u) = next(&mut s).await.unwrap() else { panic!() };
    assert_eq!((u.kind, u.version, u.seq), (ChangeKind::Updated, 2, 2));
}
#[tokio::test] async fn snapshot_includes_tombstones() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap(); m.delete_customer(&a).unwrap();
    let mut s = m.subscribe(SubscribeFilter::default());
    let SubscriptionItem::Update(u) = next(&mut s).await.unwrap() else { panic!() };
    assert_eq!(u.kind, ChangeKind::Deleted); assert_eq!(u.state["deleted"], true); assert_eq!(u.version, 2);
}
#[tokio::test] async fn no_gap_between_snapshot_and_live() {
    let m = mock_with("seed = 3\n[actions]\nnew_customer = 60.0\n[churn.customer]\nupdate = 120.0\ndelete = 10.0").await;
    m.step(Duration::from_secs(3600)).unwrap();
    let cut = m.truth_log().len() as u64;
    let mut s = m.subscribe(SubscribeFilter::default());
    m.step(Duration::from_secs(3600)).unwrap();
    let mut live = vec![]; let mut synced = false;
    while live.len() < m.truth_log().len() - cut as usize {
        match next(&mut s).await.unwrap() {
            SubscriptionItem::Synced => synced = true,
            SubscriptionItem::Update(u) if synced => live.push(u.seq),
            _ => {}
        }
    }
    assert_eq!(live, ((cut + 1)..=m.truth_log().len() as u64).collect::<Vec<_>>());
}
#[tokio::test] async fn type_filter_excludes() {
    let m = mock_with("").await; m.create_customer(NewCustomer::default()).unwrap();
    let mut s = m.subscribe(SubscribeFilter { object_types: Some(vec![]) });
    assert_eq!(next(&mut s).await.unwrap(), SubscriptionItem::Synced);
}
#[tokio::test] async fn slow_subscriber_lags_without_blocking() {
    let m = mock_with("").await;
    let mut s = m.subscribe_with(SubscribeFilter::default(), SubscribeOptions { buffer: 4 });
    for _ in 0..100 { m.create_customer(NewCustomer::default()).unwrap(); }   // must not block
    let mut items = vec![]; while let Some(i) = next(&mut s).await { items.push(i); }
    assert_eq!(items.first(), Some(&SubscriptionItem::Synced));
    assert_eq!(items.last(), Some(&SubscriptionItem::Lagged));
    assert_eq!(items.len(), 1 + 4 + 1);
}
#[tokio::test] async fn json_shape_matches_spec() {
    let m = mock_with("").await; m.create_customer(NewCustomer::default()).unwrap();
    let v = serde_json::to_value(&m.truth_log()[0]).unwrap();
    assert_eq!(v["at"], "2026-01-01T00:00:00.000Z"); assert_eq!(v["account"], Value::Null);
    assert_eq!(v["object_type"], "customer"); assert_eq!(v["kind"], "created");
    assert_eq!(v["events"], json!({"snapshot": [], "thin": []}));
    assert_eq!(v["cause"]["action"], "control.create_customer"); assert_eq!(v["cause"]["action_id"], "act_1");
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test subscription` → compile errors.
- [ ] **Step 3: Implement** `stream.rs`, the registry in `Engine` and the `MockStripe` methods.
- [ ] **Step 4: Run to verify they pass** — 6 passed.
- [ ] **Step 5: Amend the spec**
  - Section 10.3: replace the last sentence with "Messages in the initial snapshot have `kind: "snapshot"` (`"deleted"` for tombstones), keep the object's latest `seq`, `at` and `version`, and have empty `events` and no `cause`."
  - Section 10.4: change the in-process signature to `handle.subscribe(filter) -> impl Stream<Item = SubscriptionItem>`, where `SubscriptionItem` is `Update(StateUpdate) | Synced | Lagged`.
- [ ] **Step 6: fmt, clippy, commit** — `feat(stripe-mock): latest-state subscription`

---

### Task 7: HTTP API foundation

**Files:**
- Create: `src/api/mod.rs`, `tests/api_basics.rs`
- Modify: `src/handle.rs` (bind and serve in `start`, `api_url`, `shutdown`), `tests/common/mod.rs`, add `axum` (crate) and `reqwest` with `json` (dev)

**Interfaces:**
- Consumes: `Shared` (Task 5).
- Produces:
  - `pub(crate) fn router(shared: Arc<Shared>) -> axum::Router`
  - `pub(crate) struct ApiError { pub status: StatusCode, pub message: String, pub code: Option<&'static str>, pub param: Option<String> }`, implementing `IntoResponse`, with body `{"error": {"type": "invalid_request_error", "message", "code"?, "param"?}}`. `ApiError::not_found(message)`, `ApiError::bad_request(message, code, param)`.
  - `MockStripe::api_url(&self) -> String` (`"http://127.0.0.1:<port>"`)
  - `MockStripe::shutdown(self) -> impl Future<Output = ()>`. It triggers a graceful axum shutdown via a oneshot and awaits the server task's `JoinHandle`, so the port is closed when it returns. It also ends all subscriptions by clearing the registry.
  - `tests/common/mod.rs`: `pub async fn get(m: &MockStripe, path: &str) -> reqwest::Response`, which sends `Authorization: Bearer sk_test_mock`

`start` binds `tokio::net::TcpListener::bind("127.0.0.1:0")` and spawns `axum::serve`. A middleware layer runs these checks in order:
1. **Request ID.** Set `Request-Id: req_<14 alnum>` on every response, drawn from stream `"api.request_ids"` under the lock.
2. **Authentication.** A missing or malformed `Authorization: Bearer <key>` header, or a key not in `config.api.keys`, gets 401 with message `"Invalid API Key provided."`.
3. **API version.** A `Stripe-Version` header present and not equal to `PINNED_API_VERSION` gets 400, with a message containing the pinned version.
4. **Connect.** A `Stripe-Account` header present gets 400 with message `"Connected accounts are not enabled in this mock."`.
5. **Read-only.** A method other than `GET` gets 400 with message `"This mock is read-only: <METHOD> <path> is not supported."`.

The fallback is 404 with `"Unrecognized request URL (GET: <path>)."`.

- [ ] **Step 1: Write failing tests in `tests/api_basics.rs`**

```rust
#[tokio::test] async fn auth_required() {
    let m = mock_with("").await; let c = reqwest::Client::new();
    let r = c.get(format!("{}/v1/customers", m.api_url())).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.headers()["request-id"].to_str().unwrap().starts_with("req_"));
    let r = c.get(format!("{}/v1/customers", m.api_url())).bearer_auth("sk_test_wrong").send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["type"], "invalid_request_error");
}
#[tokio::test] async fn unknown_route_404() {
    let m = mock_with("").await; let r = get(&m, "/v1/nope").await;
    assert_eq!(r.status(), 404);
    assert!(r.json::<Value>().await.unwrap()["error"]["message"].as_str().unwrap().contains("/v1/nope"));
}
#[tokio::test] async fn writes_rejected() {
    let m = mock_with("").await;
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let r = reqwest::Client::new().request(method, format!("{}/v1/customers", m.api_url())).bearer_auth("sk_test_mock").send().await.unwrap();
        assert_eq!(r.status(), 400);
        assert!(r.json::<Value>().await.unwrap()["error"]["message"].as_str().unwrap().contains("read-only"));
    }
}
#[tokio::test] async fn version_header() {
    let m = mock_with("").await; let c = reqwest::Client::new();
    let send = |v: &'static str| c.get(format!("{}/v1/nope", m.api_url())).bearer_auth("sk_test_mock").header("Stripe-Version", v).send();
    assert_eq!(send("2026-11-18.endive").await.unwrap().status(), 404);
    let r = send("2024-06-20").await.unwrap(); assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("2026-11-18.endive"));
}
#[tokio::test] async fn stripe_account_rejected() {
    let m = mock_with("").await;
    let r = reqwest::Client::new().get(format!("{}/v1/customers", m.api_url())).bearer_auth("sk_test_mock").header("Stripe-Account", "acct_1").send().await.unwrap();
    assert_eq!(r.status(), 400);
}
#[tokio::test] async fn shutdown_stops_server_and_subscriptions() {
    let m = mock_with("").await; let url = m.api_url();
    let mut s = m.subscribe(SubscribeFilter::default());
    assert_eq!(s.next().await, Some(SubscriptionItem::Synced));
    m.shutdown().await;
    assert_eq!(s.next().await, None);
    assert!(reqwest::get(format!("{url}/v1/customers")).await.is_err());
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test api_basics` → compile errors.
- [ ] **Step 3: Implement** `api/mod.rs` and the handle changes.
- [ ] **Step 4: Run to verify they pass** — 6 passed. Re-run the full crate suite.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): read-only HTTP API foundation`

---

### Task 8: Customer retrieve and list

**Files:**
- Create: `src/api/list.rs`, `src/api/customers.rs`, `tests/api_customers.rs`
- Modify: `src/api/mod.rs` (routes)

**Interfaces:**
- Consumes: `ApiError`, `ObjectStore::{get, list_order}`, `Shared`.
- Produces:
  - `pub(crate) struct ListParams { pub limit: usize, pub starting_after: Option<String>, pub ending_before: Option<String>, pub created: CreatedFilter, pub extra: BTreeMap<String, String> }`
  - `pub(crate) struct CreatedFilter { pub gt: Option<i64>, pub gte: Option<i64>, pub lt: Option<i64>, pub lte: Option<i64>, pub eq: Option<i64> }` with `matches(created: i64) -> bool`
  - `pub(crate) fn parse_list_params(pairs: Vec<(String, String)>, allowed_extra: &[&str]) -> Result<ListParams, ApiError>`
  - `pub(crate) fn paginate<'a>(ordered: &[&'a StoredObject], include: impl Fn(&StoredObject) -> bool, p: &ListParams) -> Result<(Vec<&'a StoredObject>, bool), ApiError>`, returning the page and `has_more`. An unknown cursor ID is a `resource_missing` error whose `param` is whichever cursor was given.
  - Routes: `GET /v1/customers/{id}`, `GET /v1/customers`

Rules:
- **Query parsing.** Read the raw query with `axum::extract::RawQuery` and decode it with `form_urlencoded`.
- **Accepted keys:** `limit`, `starting_after`, `ending_before`, `created`, `created[gt|gte|lt|lte]` and `allowed_extra` (`["email"]` for customers). Anything else gets 400 with code `parameter_unknown` and `param` = key.
- **`limit`.** Must parse as an integer in 1..=100, otherwise 400 with code `parameter_invalid_integer` and `param: "limit"`. The default is 10.
- **Created values.** Non-integers get 400 with code `parameter_invalid_integer`.
- **Both cursors given** gets 400 with message `"You may only specify one of these parameters: starting_after, ending_before."`.
- **Order and filters.** Use the full `list_order`, including deleted objects. `include` keeps objects that are not deleted, match `created`, and match `email` exactly if given.
- **`starting_after = X`.** `X` must be an object of this type that ever existed (deleted included), otherwise 400 with code `resource_missing` and `param: "starting_after"`. The candidates are the included objects positioned after `X` in the order. The page is the first `limit` of them, and `has_more` is true if more remain.
- **`ending_before = X`.** The candidates are the included objects positioned before `X`. The page is the last `limit` of them, kept in descending order, and `has_more` is true if more precede the page.
- **Response:** `{"object": "list", "url": "/v1/customers", "has_more", "data": [state...]}`.
- **Retrieve.** A live object returns its latest `state`. A deleted one returns 200 with the tombstone `state`. Missing returns 404 with message `"No such customer: 'cus_x'"`, code `resource_missing` and `param: "id"`.

- [ ] **Step 1: Write failing tests in `tests/api_customers.rs`**

```rust
async fn list(m: &MockStripe, q: &str) -> (u16, Value) { let r = get(m, &format!("/v1/customers?{q}")).await; (r.status().as_u16(), r.json().await.unwrap()) }
fn ids(v: &Value) -> Vec<String> { v["data"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect() }
fn make(m: &MockStripe, n: usize) -> Vec<String> { (0..n).map(|_| m.create_customer(NewCustomer::default()).unwrap()).collect() }  // all in the same second

#[tokio::test] async fn retrieve_live_deleted_missing() {
    let m = mock_with("").await; let id = m.create_customer(NewCustomer::default()).unwrap();
    let r: Value = get(&m, &format!("/v1/customers/{id}")).await.json().await.unwrap();
    assert_eq!((r["id"].as_str().unwrap(), r["object"].as_str().unwrap()), (id.as_str(), "customer"));
    m.delete_customer(&id).unwrap();
    let r: Value = get(&m, &format!("/v1/customers/{id}")).await.json().await.unwrap();
    assert_eq!(r, json!({"id": id, "object": "customer", "deleted": true}));
    let r = get(&m, "/v1/customers/cus_missing").await; assert_eq!(r.status(), 404);
    let e: Value = r.json().await.unwrap();
    assert_eq!((e["error"]["code"].as_str(), e["error"]["param"].as_str()), (Some("resource_missing"), Some("id")));
}
#[tokio::test] async fn default_page() {
    let m = mock_with("").await; let made = make(&m, 12);
    let (s, v) = list(&m, "").await;
    assert_eq!(s, 200); assert_eq!(v["object"], "list"); assert_eq!(v["url"], "/v1/customers");
    assert_eq!(v["has_more"], true);
    assert_eq!(ids(&v), made.iter().rev().take(10).cloned().collect::<Vec<_>>());
}
#[tokio::test] async fn same_second_forward_walk_is_exact() {      // Review Focus 2
    let m = mock_with("").await; let made = make(&m, 25);
    let mut seen = vec![]; let mut q = "limit=7".to_string();
    loop {
        let (_, v) = list(&m, &q).await; seen.extend(ids(&v));
        if v["has_more"] == false { break; }
        q = format!("limit=7&starting_after={}", seen.last().unwrap());
    }
    assert_eq!(seen, made.iter().rev().cloned().collect::<Vec<_>>());
}
#[tokio::test] async fn backward_walk() {
    let m = mock_with("").await; let made = make(&m, 5);   // list order: made[4]..made[0]
    let (_, v) = list(&m, &format!("limit=2&ending_before={}", made[0])).await;
    assert_eq!(ids(&v), vec![made[2].clone(), made[1].clone()]); assert_eq!(v["has_more"], true);
}
#[tokio::test] async fn cursor_on_deleted_customer_continues() {   // Review Focus 3
    let m = mock_with("").await; let made = make(&m, 6);
    let (_, v) = list(&m, "limit=3").await; let cursor = ids(&v)[2].clone();
    m.delete_customer(&cursor).unwrap();
    let (s, v) = list(&m, &format!("limit=3&starting_after={cursor}")).await;
    assert_eq!(s, 200); assert_eq!(ids(&v), made[..3].iter().rev().cloned().collect::<Vec<_>>());
}
#[tokio::test] async fn filters() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer { email: Some("a@example.com".into()), ..Default::default() }).unwrap();
    m.step(Duration::from_secs(10)).unwrap();
    let b = m.create_customer(NewCustomer::default()).unwrap();
    let t = m.now().unix_seconds();
    assert_eq!(ids(&list(&m, &format!("created[gte]={t}")).await.1), vec![b.clone()]);
    assert_eq!(ids(&list(&m, &format!("created[lt]={t}")).await.1), vec![a.clone()]);
    assert_eq!(ids(&list(&m, &format!("created={t}")).await.1), vec![b]);
    assert_eq!(ids(&list(&m, "email=a@example.com").await.1), vec![a]);
}
#[tokio::test] async fn bad_params() {                              // Review Focus 4
    let m = mock_with("").await; make(&m, 1);
    for (q, code, param) in [("limit=0", "parameter_invalid_integer", "limit"), ("limit=101", "parameter_invalid_integer", "limit"),
                             ("limit=abc", "parameter_invalid_integer", "limit"), ("colour=red", "parameter_unknown", "colour"),
                             ("starting_after=cus_nope", "resource_missing", "starting_after")] {
        let (s, v) = list(&m, q).await;
        assert_eq!(s, 400, "{q}"); assert_eq!(v["error"]["code"], code, "{q}"); assert_eq!(v["error"]["param"], param, "{q}");
    }
    assert_eq!(list(&m, "starting_after=a&ending_before=b").await.0, 400);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test api_customers` → 404s and assertion failures.
- [ ] **Step 3: Implement** `api/list.rs`, `api/customers.rs` and the routes.
- [ ] **Step 4: Run to verify they pass** — 7 passed.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): customer retrieve and list endpoints`

---

### Task 9: Wall clock mode, pause and resume

**Files:**
- Create: `tests/wall_clock.rs`
- Modify: `src/handle.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: `Generator::run_until`, `Shared`.
- Produces: `MockStripe::pause(&self) -> Result<(), MockError>`, `MockStripe::resume(&self) -> Result<(), MockError>`. Both return `ClockMode` in virtual mode, and are idempotent in wall mode.

In wall mode, `start` spawns a ticker task that runs every 10 ms while not paused. It computes `target = anchor_sim + (Instant::now() - anchor_real) * speed` (in ms) and calls `run_until(target)` under the lock. `pause` freezes time by stopping the ticker's advance. `resume` resets `anchor_real = Instant::now()` and `anchor_sim = now`, so simulated time does not jump. `shutdown` stops the ticker.

- [ ] **Step 1: Write failing tests in `tests/wall_clock.rs`**

```rust
const WALL: &str = "clock = { mode = \"wall\", speed = 3600.0 }\n[actions]\nnew_customer = 3600.0";   // 1 s real = 1 h sim, ~1 customer/s sim

#[tokio::test] async fn advances_and_generates() {
    let m = mock_with(WALL).await; let t0 = m.now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(m.now().0 - t0.0 >= 300 * 3600 / 2);   // at least half the expected sim time
    assert!(!m.truth_log().is_empty());
}
#[tokio::test] async fn pause_freezes_and_resume_does_not_jump() {
    let m = mock_with(WALL).await;
    m.pause().unwrap(); let frozen = m.now();
    tokio::time::sleep(Duration::from_millis(200)).await; assert_eq!(m.now(), frozen);
    m.resume().unwrap();
    assert!(m.now().0 - frozen.0 < 100 * 3600);   // resume did not add the paused 200 ms
    tokio::time::sleep(Duration::from_millis(100)).await; assert!(m.now() > frozen);
}
#[tokio::test] async fn mode_mismatch_errors() {
    let w = mock_with(WALL).await;
    assert!(matches!(w.step(Duration::from_secs(1)), Err(MockError::ClockMode(_))));
    let v = mock_with("").await;
    assert!(matches!(v.pause(), Err(MockError::ClockMode(_))));
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test wall_clock` → compile errors (no `pause`).
- [ ] **Step 3: Implement** the ticker and `pause`/`resume`.
- [ ] **Step 4: Run to verify they pass** — 3 passed. Then run the whole suite: `cargo test --workspace`, all green.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): wall clock mode with pause and resume`
