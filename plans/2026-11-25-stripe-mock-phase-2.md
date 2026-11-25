# Stripe Mock Phase 2 (Events and Delivery) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every customer mutation produces a snapshot event and a thin event. The mock serves them from `/v1/events` and `/v2/core/events` with eventual-consistency visibility and retention, and delivers them as signed webhooks to configured snapshot and thin destinations. Delivery has realistic delays, duplicates, reordering, retries, timeouts, outages, endpoint disabling and signature faults, plus a delivery log and quiesce.

**Architecture:** A new `events` module inside the engine turns each truth-log entry into events. Events are stored with a hidden `visible_at`, and every Stripe-visible read filters on `now` (spec section 2: faults live only in the projection). A `delivery` module keeps a time-ordered queue of attempts. In virtual mode, `step`/`advance_to` become `async` and walk simulated time from one due attempt to the next, sending each batch over HTTP before continuing. In wall mode, the existing ticker spawns sends. Outcomes are fed back under the sim lock and schedule retries relative to each attempt's simulated time.

**Tech Stack:** Phase 1 stack plus `hmac` 0.13, `sha2` 0.11, `hex` 0.4. `reqwest` moves from dev-dependency to dependency (HTTP only, `json`).

**Spec:** [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md). Phase 2 is item 2 of section 16: "snapshot and thin event projection, `/v1/events` and `/v2/core/events`, visibility lag, retention, signatures, retries, duplicates, reordering; snapshot and thin destinations; delivery log". Quiesce was deferred here from phase 1.

## Global Constraints

- Pinned version `2026-11-18.endive`. Snapshot events carry `"api_version": "2026-11-18.endive"`.
- Still platform-only: `connect = true` on a destination, or `events_from` containing anything other than `"@self"`, is a config error naming phase 5.
- The Stripe-compatible API stays read-only. Destinations are managed only through config and the Rust control API (spec decision 2).
- Event types: snapshot `customer.created`, `customer.updated` and `customer.deleted`; thin `v1.customer.created`, `v1.customer.updated` and `v1.customer.deleted`.
- v1 timestamps are Unix seconds. v2 timestamps (`created` on thin events and destinations) are RFC 3339 with milliseconds, e.g. `"2026-01-01T00:00:00.000Z"`.
- Webhook signature: header `Stripe-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>`, with one `v1` per active secret. Secrets look like `whsec_<32 alnum>`.
- Determinism: every generated decision (event IDs, visibility lag, tie-break, delay, jitter, duplicate, faults, secrets) comes from a named RNG stream. Only the HTTP outcome and the signature timestamp may depend on the outside world.
- The phase 1 rules still apply: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean before every commit, and dependency versions live only in the root `[workspace.dependencies]`.
- Work on a branch in the existing checkout (`git switch -c stripe-mock-phase-2`). Never use a worktree.

## Review Focus

1. **A poller that pages with `ending_before` = newest event seen, while an older event becomes visible later.** The user expects that late event to show up in `/v1/events` behind the cursor (it is never returned by the poller's next `ending_before` call). This is the bug the fixture exists to expose. Test in Task 4.
2. **A real receiver verifying signatures while the mock runs in virtual time (simulated year 2026, any real date).** The user expects verification with the standard 300 s tolerance to succeed. So `t` must be real wall-clock time, not simulated time. Test in Task 7.
3. **A cursor or page token naming an event that has aged past the retention window.** The user expects a Stripe-style 400 `resource_missing`, so a poller can detect the gap, not an empty page. Test in Task 4 (v1) and Task 5 (v2).
4. **A receiver that returns 200 but slower than `response_timeout`.** The user expects the mock to count it as a failure and retry, so the receiver sees the same event twice. Test in Task 8.
5. **A destination disabled or removed while it has deliveries pending.** The user expects no further attempts, each pending delivery logged as abandoned with the reason, and new events not queued for it. Test in Task 8.

## Decisions made in this plan (not in the spec)

- **Default fault settings.** All new settings default to "no fault": lags and delays `fixed 0`, probabilities `0`, `reorder_window = "0s"`. Realism is opt-in through config, so a bare config stays exactly deterministic.
- **Snapshot event `request`.** Always `{"id": null, "idempotency_key": null}`. Thin events have `context: null` and `reason: null`. The mock has no API writes that would carry a request.
- **`customer.deleted`.** The snapshot `data.object` is the customer's last full state with `"deleted": true` added, as Stripe sends it. It is not the tombstone stub.
- **Thin full-event `changes`.** For updates it is `{"before": {...}, "after": {...}}` over the changed top-level fields, and `null` for created and deleted. The exact structure is not documented, so this is to be confirmed against OpenAPI in phase 7. `data` is `null`.
- **v2 list defaults and paging.** Default `limit` 10, max 100 (undocumented). The page token is opaque: hex-encoded JSON carrying the cursor and all filters. A `page` parameter cannot be combined with other filters.
- **v2 errors.** `{"error": {"type": "not_found", "code": "not_found", "message": ...}}` for a missing v2 event. Other v2 errors use `invalid_request_error`, like v1.
- **`events_from` values** follow Stripe: `@self`, `@accounts`. Spec section 4 is updated to match in Task 1.
- **Signature timestamp `t`.** Real Unix time at send (plus skew when that fault fires), so real receivers can verify it. Event `created` stays simulated.
- **Destination URLs** must be `http://` (TLS is disabled in reqwest). `https://` is a config error for now.
- **The `stripe_like` retry schedule** is the delays `1m, 5m, 30m, 2h, 5h`, then `10h` repeated. A retry is scheduled only while (its time − first attempt time) ≤ `max_retry_duration` (default `3d`). A list of duration strings is also accepted.
- **`disable_after_failures`.** `"stripe_like"` means 3 days. A duration string is also accepted. A destination is disabled when it has failed at least one attempt and has had no success for that long, measured in simulated time from its first failure since the last success.
- **Quiesce.** Turns generation off, makes every event visible now, then repeatedly attempts every pending delivery immediately, ignoring backoff. It stops when each delivery has succeeded or used up the attempts its schedule allows. Simulated time does not move. `resume_generation()` turns generation back on.
- **Breaking change.** `MockStripe::step` and `advance_to` become `async`. Phase 1 tests gain `.await`.

## File Structure

```
crates/stripe-mock/src/
├── config.rs            + DurationStr, Distribution, EventsConfig, EventRetentionConfig, WebhooksConfig, DestinationConfig
├── rng.rs               + id_len, chance, normal, sample(&Distribution)
├── engine/mod.rs        record() calls the event projector; Engine::new takes config + streams
├── engine/events.rs     NEW: SnapshotEvent / ThinEvent records, payload building, EventStore
├── delivery/mod.rs      NEW: destinations, delivery queue, scheduling, outcomes, log types
├── delivery/signing.rs  NEW: sign / pub verify_signature
├── delivery/sender.rs   NEW: async HTTP send of one attempt (reqwest)
├── handle.rs            async step/advance_to with delivery loop; control API for destinations, log, quiesce
├── api/list.rs          paginate becomes generic over a Cursorable trait
├── api/events_v1.rs     NEW: GET /v1/events, /v1/events/{id}
├── api/events_v2.rs     NEW: GET /v2/core/events, /v2/core/events/{id}
└── api/destinations.rs  NEW: GET /v1/webhook_endpoints, /v2/core/event_destinations
crates/stripe-mock/tests/
├── common/mod.rs        + Receiver test server
├── events_v1.rs, events_v2.rs, destinations.rs, delivery.rs, delivery_faults.rs, quiesce.rs
```

---

### Task 1: Config for events, retention, webhooks and destinations

**Files:**
- Modify: `src/config.rs`, `src/error.rs` (no new variants), `docs/stripe-mock-spec.md` (section 4 `events_from`)

**Interfaces:**
- Produces (all `pub`, re-exported from `lib.rs`):
  - `#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct DurationStr(pub i64)`: milliseconds. Deserializes from strings `"<int><unit>"` with unit `ms|s|m|h|d`, e.g. `"300s"`, `"3d"`. `DurationStr::ms(self) -> i64`.
  - `#[serde(tag = "distribution", rename_all = "lowercase")] pub enum Distribution { Fixed { ms: i64 }, Lognormal { median_ms: f64, p99_ms: f64 } }`, defaulting to `Fixed { ms: 0 }`
  - `pub struct LateTail { pub probability: f64, pub min: DurationStr, pub max: DurationStr }` (default `0`, `"0s"`, `"0s"`)
  - `pub struct EventsConfig { pub visibility_lag: Distribution, pub late_tail: LateTail, pub same_second_shuffle: bool }` (default: no lag, no tail, `false`)
  - `pub struct EventRetentionConfig { pub window: DurationStr }` (default `"30d"`)
  - `pub enum RetrySchedule { StripeLike, Explicit(Vec<DurationStr>) }`, deserialized from `"stripe_like"` or a list of duration strings
  - `pub enum DisableAfter { StripeLike, After(DurationStr) }`, from `"stripe_like"` or a duration string; `StripeLike` = 3 days
  - `pub struct ClockSkew { pub probability: f64, pub max: DurationStr }` (default `0`, `"10m"`)
  - `pub struct WebhooksConfig { pub delivery_delay: Distribution, pub duplicate_probability: f64, pub reorder_window: DurationStr, pub response_timeout: DurationStr, pub retry_schedule: RetrySchedule, pub max_retry_duration: DurationStr, pub disable_after_failures: DisableAfter, pub signature_tolerance: DurationStr, pub clock_skew: ClockSkew, pub invalid_signature_probability: f64 }`. Defaults: `Fixed 0`, `0`, `"0s"`, `"10s"`, `StripeLike`, `"3d"`, `StripeLike`, `"300s"`, the `ClockSkew` default, `0`.
  - `#[serde(rename_all = "lowercase")] pub enum PayloadFormat { Snapshot, Thin }`
  - `pub struct DestinationConfig { pub name: String, pub url: String, pub payload: PayloadFormat, pub enabled_events: Vec<String>, pub connect: bool, pub events_from: Option<Vec<String>>, pub secret: Option<String> }`
  - `Config` gains `pub events: EventsConfig`, `pub event_retention: EventRetentionConfig`, `pub webhooks: WebhooksConfig`, `pub destinations: Vec<DestinationConfig>`.
- `UNIMPLEMENTED_SECTIONS` becomes `["accounts", "history", "payloads"]`.

Validation adds:
- Every probability must be in `[0, 1]`.
- `Lognormal` needs `0 < median_ms <= p99_ms`. `Fixed` needs `ms >= 0`.
- `late_tail.min <= late_tail.max`.
- `clock_skew.max > signature_tolerance` when `clock_skew.probability > 0`.
- `response_timeout > 0`. An `Explicit` retry schedule must be non-empty and every delay `> 0`.
- Destinations:
  - Names must be unique and non-empty.
  - The `url` must start with `http://`.
  - `enabled_events` must be non-empty.
  - Snapshot event names must not start with `v1.`, and thin event names must, unless the entry is `*`.
  - `connect = true` gives the error "`destinations.connect` is not implemented yet (phase 5: Connect)". `events_from` other than `["@self"]` gives the same error with `destinations.events_from`. `events_from` on a snapshot destination is an error.
  - `secret`, if set, must start with `whsec_`.

- [ ] **Step 1: Write failing tests in `src/config.rs`**

```rust
#[test] fn durations_parse() {
    for (s, ms) in [("250ms", 250), ("300s", 300_000), ("10m", 600_000), ("2h", 7_200_000), ("3d", 259_200_000)] {
        let c = Config::from_toml_str(&format!("[event_retention]\nwindow = \"{s}\"")).unwrap();
        assert_eq!(c.event_retention.window.ms(), ms, "{s}");
    }
    for bad in ["10", "x5s", "5w", "-1s"] {
        assert!(Config::from_toml_str(&format!("[event_retention]\nwindow = \"{bad}\"")).is_err(), "{bad}");
    }
}
#[test] fn phase2_defaults_are_fault_free() {
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.events.visibility_lag, Distribution::Fixed { ms: 0 });
    assert_eq!(c.event_retention.window.ms(), 30 * 86_400_000);
    assert_eq!(c.webhooks.duplicate_probability, 0.0);
    assert_eq!(c.webhooks.response_timeout.ms(), 10_000);
    assert_eq!(c.webhooks.retry_schedule, RetrySchedule::StripeLike);
    assert!(c.destinations.is_empty());
}
#[test] fn parses_spec_phase2_sections() {
    let c = Config::from_toml_str(r#"
        [events]
        visibility_lag = { distribution = "lognormal", median_ms = 200, p99_ms = 5000 }
        late_tail = { probability = 0.001, min = "30s", max = "10m" }
        same_second_shuffle = true
        [webhooks]
        retry_schedule = ["1s", "2s"]
        disable_after_failures = "1h"
        clock_skew = { probability = 0.1, max = "10m" }
        [[destinations]]
        name = "thin"
        url = "http://127.0.0.1:4000/hook"
        payload = "thin"
        enabled_events = ["v1.customer.*"]
        events_from = ["@self"]
        secret = "whsec_test"
    "#).unwrap();
    assert_eq!(c.events.visibility_lag, Distribution::Lognormal { median_ms: 200.0, p99_ms: 5000.0 });
    assert_eq!(c.webhooks.retry_schedule, RetrySchedule::Explicit(vec![DurationStr(1000), DurationStr(2000)]));
    assert_eq!(c.webhooks.disable_after_failures, DisableAfter::After(DurationStr(3_600_000)));
    assert_eq!(c.destinations[0].payload, PayloadFormat::Thin);
}
#[test] fn rejects_bad_destinations() {
    let d = |url: &str, events: &str, extra: &str| format!("[[destinations]]
name = \"d\"
url = \"{url}\"
payload = \"snapshot\"
enabled_events = [\"{events}\"]
{extra}");
    for (toml, needle) in [
        (d("https://x", "*", ""), "http://"),
        (d("http://x", "*", "connect = true"), "phase 5"),
        (d("http://x", "v1.customer.created", ""), "v1."),
        (d("http://x", "*", "secret = \"nope\""), "whsec_"),
        (d("http://x", "*", "events_from = [\"@self\"]"), "events_from"),
    ] {
        let msg = Config::from_toml_str(&toml).unwrap_err().to_string();
        assert!(msg.contains(needle), "{toml}: {msg}");
    }
}
#[test] fn rejects_bad_fault_values() {
    for bad in [
        "[webhooks]\nduplicate_probability = 1.5",
        "[events]\nvisibility_lag = { distribution = \"lognormal\", median_ms = 500, p99_ms = 100 }",
        "[webhooks]\nclock_skew = { probability = 0.5, max = \"60s\" }",   // max <= 300s tolerance
        "[webhooks]\nretry_schedule = []",
    ] { assert!(Config::from_toml_str(bad).is_err(), "{bad}"); }
}
```

Update the existing `spec_example_config_fails_clearly` and `spec_example_without_top_level_sections_fails_clearly` tests. The first still expects `[accounts]`. The second removes only `["accounts", "history", "payloads"]` and now expects `api.latency`.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --lib -- config::` → compile errors.
- [ ] **Step 3: Implement** the types and validation in `config.rs`. `DurationStr` and `RetrySchedule`/`DisableAfter` use hand-written `Deserialize` impls (string or list).
- [ ] **Step 4: Run to verify they pass.** All `config::` tests pass.
- [ ] **Step 5: Update spec section 4.** In the example, change `events_from = ["self", "other_accounts"]` to `events_from = ["@self", "@accounts"]`, keeping the comment.
- [ ] **Step 6: fmt, clippy, commit** — `feat(stripe-mock): config for events, retention, webhooks and destinations`

---

### Task 2: Sampling helpers

**Files:**
- Modify: `src/rng.rs`

**Interfaces:**
- Consumes: `Distribution`, `DurationStr` (Task 1).
- Produces on `SimRng`:
  - `id_len(prefix: &str, len: usize) -> String`. Same alphabet as `id`. `id(prefix)` becomes `id_len(prefix, 14)`, so the golden value is unchanged.
  - `chance(p: f64) -> bool` (`unit_f64() < p`; always `false` for `p == 0`, without drawing)
  - `normal() -> f64`: Box–Muller, `sqrt(-2 ln(1 - u1)) * cos(2π u2)`, drawing `u1` then `u2`
  - `sample_ms(d: &Distribution) -> i64`:
    - `Fixed` returns `ms` without drawing.
    - `Lognormal` uses `mu = ln(median)` and `sigma = (ln(p99) − ln(median)) / 2.326348`, then returns `round(exp(mu + sigma · normal())).max(0)`. When `median == p99`, `sigma = 0`.
  - `uniform_ms(min: i64, max: i64) -> i64` (inclusive; `min` when `min == max`)

- [ ] **Step 1: Write failing tests in `src/rng.rs`**

```rust
#[test] fn id_len_and_golden_unchanged() {
    let mut r = RngStreams::new(1).stream("ids");
    let id = r.id_len("evt", 24); assert!(id.starts_with("evt_") && id.len() == 28);
    assert_eq!(RngStreams::new(42).stream("generate.customer.create").next_u64(), GOLDEN);
}
#[test] fn chance_zero_does_not_draw() {
    let s = RngStreams::new(2);
    let (mut a, mut b) = (s.stream("c"), s.stream("c"));
    assert!(!a.chance(0.0)); assert_eq!(a.next_u64(), b.next_u64());
}
#[test] fn lognormal_median_and_p99() {
    let mut r = RngStreams::new(3).stream("ln");
    let d = Distribution::Lognormal { median_ms: 200.0, p99_ms: 5000.0 };
    let mut v: Vec<i64> = (0..20_000).map(|_| r.sample_ms(&d)).collect(); v.sort();
    let (median, p99) = (v[10_000], v[19_800]);
    assert!((180..=220).contains(&median), "{median}");
    assert!((4000..=6200).contains(&p99), "{p99}");
    assert_eq!(r.sample_ms(&Distribution::Fixed { ms: 7 }), 7);
}
#[test] fn uniform_bounds() {
    let mut r = RngStreams::new(4).stream("u");
    assert!((0..1000).all(|_| (5..=9).contains(&r.uniform_ms(5, 9))));
    assert_eq!(r.uniform_ms(3, 3), 3);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --lib -- rng::`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass.** All `rng::` tests pass.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): sampling helpers`

---

### Task 3: Event projection

**Files:**
- Create: `src/engine/events.rs`
- Modify: `src/engine/mod.rs`, `src/handle.rs` (Engine construction), `src/lib.rs`

**Interfaces:**
- Consumes: Tasks 1–2, `StateUpdate`.
- Produces:
  - `Engine::new(start: SimTime, config: &Config, streams: &RngStreams) -> Engine`. Phase 1 unit tests switch to a helper `fn test_engine() -> Engine { Engine::new(SimTime(0), &Config::default(), &RngStreams::new(0)) }`.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)] #[serde(rename_all = "lowercase")] pub enum EventFormat { Snapshot, Thin }`
  - `pub(crate) struct EventRecord { pub id: String, pub format: EventFormat, pub event_type: String, pub created: SimTime, pub visible_at: SimTime, pub tiebreak: u64, pub object_type: ObjectType, pub object_id: String, pub object_version: u64, pub truth_seq: u64, pub payload: Value /* full event as served by the API */, pub pair_id: String /* the other format's event id */ }`
  - `pub(crate) struct EventStore`:
    - `push(&mut self, r: EventRecord)`
    - `get(&self, id: &str) -> Option<&EventRecord>`
    - `list_order(&self, f: EventFormat) -> Vec<&EventRecord>`: created desc, then tiebreak desc, then insertion desc
    - `is_listed(&self, r: &EventRecord, now: SimTime, retention_ms: i64) -> bool`: `visible_at <= now && created > now - retention`
    - `all(&self) -> &[EventRecord]`
  - `Engine::events(&self) -> &EventStore`, `Engine::events_mut(&mut self) -> &mut EventStore`, `Engine::retention_ms(&self) -> i64`
  - `pub struct EventInfo { pub id: String, pub format: EventFormat, pub event_type: String, pub created: SimTime, pub visible_at: SimTime, pub object_id: String, pub object_version: u64 }` (public, derives `Debug, Clone, PartialEq, Serialize`), and `MockStripe::event_timeline(&self) -> Vec<EventInfo>` in insertion order
  - `StateUpdate.events` is now filled: `EventRefs { snapshot: vec![snapshot_id], thin: vec![thin_id] }`.

Projection runs inside `Engine::record` for every mutation, using the previous state (`None` for create). Per mutation:
1. **IDs.** Snapshot ID `rng_ids.id_len("evt", 24)`, then thin ID `rng_ids.id_len("evt_test", 46)`. Stream `"events.ids"`.
2. **Visibility.** `lag = sample_ms(visibility_lag)`. Then, with stream `"events.visibility"`, `if chance(late_tail.probability) { lag += uniform_ms(min, max) }`. `visible_at = created + lag`, shared by both events.
3. **Tie-break.** `next_u64()` from stream `"events.shuffle"` if `same_second_shuffle`, else `0`. Shared by both events.
4. **Snapshot payload:** `{"id", "object": "event", "api_version": "2026-11-18.endive", "created": <unix s>, "data": {"object": <obj>, "previous_attributes"?: <diff>}, "livemode": false, "pending_webhooks": 0, "request": {"id": null, "idempotency_key": null}, "type"}`.
   - `obj` is the new state, or for a delete the previous full state with `"deleted": true` inserted.
   - `previous_attributes` appears only on `*.updated`. It covers every top-level key whose value changed, holding the old value. For object-valued keys (e.g. `metadata`), it recurses one level: each changed inner key holds its old value, or `null` if the key was added.
5. **Thin payload (full event):** `{"id", "object": "v2.core.event", "type": "v1.customer.<verb>", "created": "<RFC 3339 ms>", "livemode": false, "context": null, "reason": null, "related_object": {"id", "type": "customer", "url": "/v1/customers/<id>"}, "snapshot_event": <snapshot id>, "changes": <see Decisions>, "data": null}`.
   - The delivered notification is this object without `snapshot_event`, `changes` and `data`: `pub(crate) fn thin_notification(full: &Value) -> Value`.

- [ ] **Step 1: Write failing tests in `src/engine/events.rs`**

```rust
fn engine_with(toml: &str) -> Engine { let c = Config::from_toml_str(toml).unwrap(); Engine::new(SimTime(1_767_225_600_000), &c, &RngStreams::new(c.seed)) }
fn cust(id: &str) -> Value { json!({"id": id, "object": "customer", "created": 1767225600, "email": "a@example.com", "metadata": {"tier": "gold"}}) }

#[test] fn every_mutation_yields_a_pair() {
    let mut e = engine_with(""); let c = e.begin_action("t");
    let u = e.create(ObjectType::Customer, "cus_1".into(), cust("cus_1"), c);
    assert_eq!((u.events.snapshot.len(), u.events.thin.len()), (1, 1));
    let s = e.events().get(&u.events.snapshot[0]).unwrap();
    let t = e.events().get(&u.events.thin[0]).unwrap();
    assert_eq!((s.event_type.as_str(), t.event_type.as_str()), ("customer.created", "v1.customer.created"));
    assert!(s.id.starts_with("evt_") && s.id.len() == 28 && t.id.starts_with("evt_test_"));
    assert_eq!(s.payload["api_version"], "2026-11-18.endive"); assert_eq!(s.payload["created"], 1767225600);
    assert_eq!(s.payload["data"]["object"]["id"], "cus_1"); assert!(s.payload["data"].get("previous_attributes").is_none());
    assert_eq!(t.payload["created"], "2026-01-01T00:00:00.000Z"); assert_eq!(t.payload["snapshot_event"], s.id.as_str());
    assert_eq!(t.payload["related_object"], json!({"id": "cus_1", "type": "customer", "url": "/v1/customers/cus_1"}));
    assert_eq!((s.visible_at, s.tiebreak), (t.visible_at, t.tiebreak));
}
#[test] fn update_has_previous_attributes_and_changes() {
    let mut e = engine_with(""); let c = e.begin_action("t");
    e.create(ObjectType::Customer, "cus_1".into(), cust("cus_1"), c.clone());
    let u = e.update(ObjectType::Customer, "cus_1", |v| { v["email"] = json!("b@example.com"); v["metadata"]["plan"] = json!("pro"); }, c).unwrap().unwrap();
    let s = e.events().get(&u.events.snapshot[0]).unwrap();
    assert_eq!(s.payload["data"]["previous_attributes"], json!({"email": "a@example.com", "metadata": {"plan": null}}));
    let t = e.events().get(&u.events.thin[0]).unwrap();
    assert_eq!(t.payload["changes"]["before"]["email"], "a@example.com"); assert_eq!(t.payload["changes"]["after"]["email"], "b@example.com");
}
#[test] fn delete_event_carries_full_last_state() {
    let mut e = engine_with(""); let c = e.begin_action("t");
    e.create(ObjectType::Customer, "cus_1".into(), cust("cus_1"), c.clone());
    let u = e.delete(ObjectType::Customer, "cus_1", json!({"id": "cus_1", "object": "customer", "deleted": true}), c).unwrap();
    let s = e.events().get(&u.events.snapshot[0]).unwrap();
    assert_eq!(s.event_type, "customer.deleted");
    assert_eq!(s.payload["data"]["object"]["email"], "a@example.com"); assert_eq!(s.payload["data"]["object"]["deleted"], true);
}
#[test] fn visibility_lag_and_listing_window() {
    let mut e = engine_with("[events]\nvisibility_lag = { distribution = \"fixed\", ms = 5000 }\n[event_retention]\nwindow = \"1h\"");
    let c = e.begin_action("t");
    let u = e.create(ObjectType::Customer, "cus_1".into(), cust("cus_1"), c);
    let r = e.events().get(&u.events.snapshot[0]).unwrap().clone();
    assert_eq!(r.visible_at, r.created.plus_ms(5000));
    let ret = e.retention_ms();
    assert!(!e.events().is_listed(&r, r.created.plus_ms(4999), ret));
    assert!(e.events().is_listed(&r, r.created.plus_ms(5000), ret));
    assert!(!e.events().is_listed(&r, r.created.plus_ms(3_600_000), ret));
}
#[test] fn thin_notification_strips_full_fields() {
    let n = thin_notification(&json!({"id": "evt_test_1", "snapshot_event": "evt_1", "changes": null, "data": null, "type": "v1.customer.created"}));
    assert_eq!(n, json!({"id": "evt_test_1", "type": "v1.customer.created"}));
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --lib -- engine::`
- [ ] **Step 3: Implement** `events.rs`, the projector call in `record`, the `Engine::new` signature change, `event_timeline`, and the re-export of `EventInfo` and `EventFormat`.
- [ ] **Step 4: Run to verify they pass.** Then run the full crate suite: the phase 1 tests still pass (the subscription JSON-shape test now sees non-empty `events`; update its assertion to check that `events.snapshot` and `events.thin` each have one ID).
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): snapshot and thin event projection`

---

### Task 4: `/v1/events`

**Files:**
- Create: `src/api/events_v1.rs`, `tests/events_v1.rs`
- Modify: `src/api/list.rs` (generic pagination), `src/api/customers.rs`, `src/api/mod.rs` (routes)

**Interfaces:**
- Consumes: `EventStore`, `ListParams`, `parse_list_params`.
- Produces:
  - `pub(crate) trait Cursorable { fn cursor_id(&self) -> &str; }`, implemented for `StoredObject` and `EventRecord`
  - `paginate<'a, T: Cursorable>(ordered: &[&'a T], include: impl Fn(&T) -> bool, p: &ListParams) -> Result<(Vec<&'a T>, bool), ApiError>`, with the same semantics as phase 1
  - `parse_list_params` gains array support: `types[]=x` and `types[N]=x` collect into `extra_multi: BTreeMap<String, Vec<String>>` for keys listed in a new `allowed_multi: &[&str]` argument. Customers pass `&[]`.
  - Routes `GET /v1/events` (allowed extras `type`, `delivery_success`; multi `types`) and `GET /v1/events/{id}`

Rules:
- **Listing.** Order is `list_order(Snapshot)`. `include` = `is_listed(now, retention)` && type filter && `created` filter (Unix seconds of `created`).
  - `type` supports a trailing `*` wildcard (`customer.*`).
  - `type` together with `types` gives 400 "You may only specify one of these parameters: type, types.".
  - More than 20 `types` gives 400 with code `parameter_invalid` and param `types`.
- **Cursors.** A cursor must be an event that is currently listed. Otherwise it is 400 `resource_missing` with `param` set to the cursor name. This is how a poller detects a retention gap.
- **`delivery_success`.** Parsed as `true`/`false`, otherwise 400 with code `parameter_invalid_boolean`. Until Task 7 every event counts as delivered (no destinations exist). Task 7 wires the real semantics.
- **`pending_webhooks`** in served payloads comes from Task 7. Until then it is the stored `0`.
- **Retrieve.** A listed event returns its payload. Otherwise 404 with code `resource_missing`, param `id`, and message `"No such event: '<id>'"`.

- [ ] **Step 1: Write failing tests in `tests/events_v1.rs`**

```rust
async fn events(m: &MockStripe, q: &str) -> (u16, Value) { let r = get(m, &format!("/v1/events?{q}")).await; (r.status().as_u16(), r.json().await.unwrap()) }
fn ids(v: &Value) -> Vec<String> { v["data"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap().to_string()).collect() }

#[tokio::test] async fn lists_snapshot_events_newest_first() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(1)).await.unwrap();
    m.update_customer(&a, CustomerUpdate::SetName(Some("B".into()))).unwrap();
    let (s, v) = events(&m, "").await;
    assert_eq!(s, 200); assert_eq!(v["url"], "/v1/events");
    let types: Vec<_> = v["data"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["customer.updated", "customer.created"]);
    let one: Value = get(&m, &format!("/v1/events/{}", ids(&v)[0])).await.json().await.unwrap();
    assert_eq!(one["type"], "customer.updated");
}
#[tokio::test] async fn invisible_until_lag_passes() {
    let m = mock_with("[events]\nvisibility_lag = { distribution = \"fixed\", ms = 2000 }").await;
    m.create_customer(NewCustomer::default()).unwrap();
    assert!(ids(&events(&m, "").await.1).is_empty());
    m.step(Duration::from_millis(2000)).await.unwrap();
    assert_eq!(ids(&events(&m, "").await.1).len(), 1);
}
#[tokio::test] async fn late_event_appears_behind_ending_before_cursor() {   // Review Focus 1
    let m = mock_with("").await;
    m.update_events_config(|e| e.visibility_lag = Distribution::Fixed { ms: 60_000 }).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();                    // older, visible at +60 s
    m.step(Duration::from_secs(1)).await.unwrap();
    m.update_events_config(|e| e.visibility_lag = Distribution::Fixed { ms: 0 }).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();                    // newer, visible now
    let newest = ids(&events(&m, "").await.1); assert_eq!(newest.len(), 1);
    m.step(Duration::from_secs(60)).await.unwrap();
    assert!(ids(&events(&m, &format!("ending_before={}", newest[0])).await.1).is_empty());   // the poller sees nothing new
    let all = ids(&events(&m, "").await.1);
    assert_eq!(all.len(), 2); assert_eq!(all[0], newest[0]);               // the late event sits behind the cursor
}
#[tokio::test] async fn expired_cursor_is_resource_missing() {           // Review Focus 3
    let m = mock_with("[event_retention]\nwindow = \"1h\"").await;
    m.create_customer(NewCustomer::default()).unwrap();
    let id = ids(&events(&m, "").await.1)[0].clone();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let (s, v) = events(&m, &format!("starting_after={id}")).await;
    assert_eq!(s, 400); assert_eq!(v["error"]["code"], "resource_missing"); assert_eq!(v["error"]["param"], "starting_after");
    assert_eq!(get(&m, &format!("/v1/events/{id}")).await.status(), 404);
}
#[tokio::test] async fn type_filters() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap(); m.delete_customer(&a).unwrap();
    assert_eq!(ids(&events(&m, "type=customer.deleted").await.1).len(), 1);
    assert_eq!(ids(&events(&m, "type=customer.*").await.1).len(), 2);
    assert_eq!(ids(&events(&m, "types[]=customer.created&types[]=customer.deleted").await.1).len(), 2);
    assert_eq!(events(&m, "type=customer.created&types[]=customer.deleted").await.0, 400);
}
#[tokio::test] async fn same_second_shuffle_is_seeded() {
    let cfg = "seed = 5\n[events]\nsame_second_shuffle = true";
    let order = |m: MockStripe| async move { for _ in 0..20 { m.create_customer(NewCustomer::default()).unwrap(); } ids(&events(&m, "limit=100").await.1) };
    let (a, b) = (order(mock_with(cfg).await).await, order(mock_with(cfg).await).await);
    assert_eq!(a, b);
    let plain = order(mock_with("seed = 5").await).await;
    assert_ne!(a, plain);
}
```

These tests use `MockStripe::update_events_config(&self, f: impl FnOnce(&mut EventsConfig)) -> Result<(), MockError>`. Add it in this task: it applies `f` to a copy, validates it, then stores it in the engine. Lags already assigned are not recomputed. The tests also call `step(...).await`, so Step 2 makes `step`/`advance_to` async before anything else.

- [ ] **Step 2: Make `step`/`advance_to` async.** Change the signatures to `pub async fn step(&self, d: Duration) -> Result<(), MockError>` and `pub async fn advance_to(&self, t: SimTime) -> Result<(), MockError>`. For now they hold an internal `tokio::sync::Mutex<()>` step lock for their whole duration (this fixes the phase 1 deferred minor about concurrent `step`). Compute the target under the sim lock with `checked_add`, returning `TimeTravel` on overflow. Add `.await` to every phase 1 test call. Run the full suite: all green.
- [ ] **Step 3: Run the new tests to verify they fail** — `cargo test -p ductkit-stripe-mock --test events_v1`
- [ ] **Step 4: Implement** the generic `paginate`, array params, `events_v1.rs`, routes and `update_events_config`.
- [ ] **Step 5: Run to verify they pass.** 6 passed, and the full suite is green.
- [ ] **Step 6: fmt, clippy, commit** — `feat(stripe-mock): /v1/events with visibility and retention`

---

### Task 5: `/v2/core/events`

**Files:**
- Create: `src/api/events_v2.rs`, `tests/events_v2.rs`
- Modify: `src/api/mod.rs` (routes; `ApiError` gains `error_type: &'static str`, default `"invalid_request_error"`)

**Interfaces:**
- Consumes: `EventStore`, `Cursorable`, `paginate`.
- Produces: routes `GET /v2/core/events` and `GET /v2/core/events/{id}`.

Rules:
- **Parameters:** `limit` (1–100, default 10), `page`, `types` (repeated `types=`, `types[]=`, or `types[N]=`; at most 20), `object_id`, and `created[gt|gte|lt|lte]` with RFC 3339 values (otherwise 400 with code `parameter_invalid_timestamp`). Unknown keys get 400 `parameter_unknown`.
- **Page token.** `page` is hex(JSON `{"dir": "after"|"before", "cursor": "<event id>", "limit", "types", "object_id", "created": {...}}`). If `page` is given together with any other parameter, the result is 400 "page cannot be combined with other parameters". An undecodable token gives 400 `parameter_invalid`.
- **Ordering and filtering.** Order by `list_order(Thin)`, keeping events that are listed and match the filters. `object_id` matches `related_object.id`.
- **Response:** `{"data": [...], "next_page_url": "/v2/core/events?page=<token after the last item>" | null, "previous_page_url": "/v2/core/events?page=<token before the first item>" | null}`.
  - `next_page_url` is non-null iff `has_more` in the forward direction.
  - `previous_page_url` is non-null iff the request used a page token and items exist before the first item.
- **Expired cursor.** A token whose cursor event is no longer listed gets 400 `resource_missing` with param `page` (Review Focus 3).
- **Retrieve.** A listed event returns the full event. Otherwise 404 `{"error": {"type": "not_found", "code": "not_found", "message": "No such event: '<id>'"}}`.

- [ ] **Step 1: Write failing tests in `tests/events_v2.rs`**

```rust
async fn v2(m: &MockStripe, q: &str) -> (u16, Value) { let r = get(m, &format!("/v2/core/events?{q}")).await; (r.status().as_u16(), r.json().await.unwrap()) }

#[tokio::test] async fn lists_thin_events_with_page_urls() {
    let m = mock_with("").await;
    for _ in 0..3 { m.create_customer(NewCustomer::default()).unwrap(); }
    let (s, v) = v2(&m, "limit=2").await;
    assert_eq!(s, 200); assert_eq!(v["data"].as_array().unwrap().len(), 2);
    assert_eq!(v["data"][0]["object"], "v2.core.event"); assert_eq!(v["data"][0]["type"], "v1.customer.created");
    assert_eq!(v["previous_page_url"], Value::Null);
    let next = v["next_page_url"].as_str().unwrap();
    let page: Value = get(&m, next).await.json().await.unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["next_page_url"], Value::Null); assert!(page["previous_page_url"].is_string());
}
#[tokio::test] async fn filters_types_and_object_id() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap(); m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&a).unwrap();
    assert_eq!(v2(&m, "types=v1.customer.deleted").await.1["data"].as_array().unwrap().len(), 1);
    assert_eq!(v2(&m, &format!("object_id={a}")).await.1["data"].as_array().unwrap().len(), 2);
    assert_eq!(v2(&m, "page=zz&limit=2").await.0, 400);
    assert_eq!(v2(&m, "created[gte]=not-a-time").await.0, 400);
}
#[tokio::test] async fn retrieve_full_event_and_v2_not_found() {
    let m = mock_with("").await; m.create_customer(NewCustomer::default()).unwrap();
    let id = v2(&m, "").await.1["data"][0]["id"].as_str().unwrap().to_string();
    let full: Value = get(&m, &format!("/v2/core/events/{id}")).await.json().await.unwrap();
    assert!(full["snapshot_event"].as_str().unwrap().starts_with("evt_"));
    let r = get(&m, "/v2/core/events/evt_test_missing").await; assert_eq!(r.status(), 404);
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["type"], "not_found");
}
#[tokio::test] async fn expired_page_token_is_resource_missing() {
    let m = mock_with("[event_retention]\nwindow = \"1h\"").await;
    for _ in 0..2 { m.create_customer(NewCustomer::default()).unwrap(); }
    let next = v2(&m, "limit=1").await.1["next_page_url"].as_str().unwrap().to_string();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let r = get(&m, &next).await; assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["code"], "resource_missing");
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test events_v2`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass.** 4 passed.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): /v2/core/events`

---

### Task 6: Destinations and signing

**Files:**
- Create: `src/delivery/mod.rs` (destination part), `src/delivery/signing.rs`, `src/api/destinations.rs`, `tests/destinations.rs`
- Modify: `src/handle.rs`, `src/lib.rs`, root and crate `Cargo.toml` (`hmac`, `sha2`, `hex`)

**Interfaces:**
- Produces:
  - `pub struct DestinationInfo { pub id: String, pub name: String, pub url: String, pub payload: PayloadFormat, pub enabled_events: Vec<String>, pub enabled: bool, pub secret: String, pub created: SimTime }` (`Debug, Clone, PartialEq`)
  - `pub(crate) struct Destination { info: DestinationInfo, old_secrets: Vec<(String, SimTime /* expires */)>, outage_until: Option<SimTime>, first_failure_since_success: Option<SimTime> }` and `pub(crate) struct Destinations` with `add`, `remove`, `get`, `get_mut`, `iter` (in creation order), `matching(&self, format: EventFormat, event_type: &str) -> Vec<String /* ids */>` (enabled only)
  - `pub(crate) fn event_matches(pattern: &str, event_type: &str) -> bool`: `*`, exact match, or a prefix with a trailing `.*`
  - `pub(crate) fn sign(t: i64, body: &[u8], secrets: &[&str]) -> String`, which returns the header value with one `v1` per secret, in the given order
  - `pub fn verify_signature(header: &str, body: &[u8], secret: &str, tolerance_secs: i64, now_unix: i64) -> bool` (public, re-exported). It ignores unknown keys and any `v0`, accepts if any `v1` matches in constant time (`hmac::Mac::verify_slice`), and requires `|now − t| <= tolerance`.
  - On `MockStripe`:
    - `add_destination(&self, d: DestinationConfig) -> Result<DestinationInfo, MockError>` (validated like config)
    - `remove_destination(&self, id: &str) -> Result<(), MockError>`
    - `set_destination_enabled(&self, id: &str, enabled: bool) -> Result<(), MockError>`
    - `rotate_secret(&self, id: &str, overlap: Duration) -> Result<String, MockError>`: the old secret keeps signing until `now + overlap`
    - `destinations(&self) -> Vec<DestinationInfo>`
  - `MockError::NoSuchDestination(String)`
  - Routes `GET /v1/webhook_endpoints` (snapshot destinations) and `GET /v2/core/event_destinations` (thin destinations), both read-only.
    - **v1 object:** `{"id": "we_…", "object": "webhook_endpoint", "api_version": "2026-11-18.endive", "application": null, "created": <unix s>, "description": <name>, "enabled_events", "livemode": false, "metadata": {}, "status": "enabled"|"disabled", "url"}`, wrapped in a v1 list with no pagination params beyond `limit`.
    - **v2 object:** `{"id": "ed_test_…", "object": "v2.core.event_destination", "name", "description": "", "type": "webhook_endpoint", "event_payload": "thin", "enabled_events", "events_from": ["@self"], "snapshot_api_version": null, "status", "status_details": null, "webhook_endpoint": {"url", "signing_secret": null}, "livemode": false, "metadata": {}, "created", "updated"}`, wrapped in `{"data", "next_page_url": null, "previous_page_url": null}`.
    - Secrets never appear in API responses.

**Ownership.** `Destinations` lives in `Engine` as a field, with `Engine::destinations()` and `Engine::destinations_mut()`, so the projector can see it when events are created.

**ID and secret generation.** Destination IDs: `we` + 24 chars (snapshot) or `ed_test` + 24 chars (thin), from stream `"destinations.ids"`. Secrets: `whsec_` + 32 chars from stream `"destinations.secrets"`, unless config gives `secret`. Config destinations are created in config order at start.

- [ ] **Step 1: Write failing tests**

```rust
// src/delivery/signing.rs
#[test] fn sign_then_verify() {
    let h = sign(1_700_000_000, b"{\"a\":1}", &["whsec_new", "whsec_old"]);
    assert!(h.starts_with("t=1700000000,v1=")); assert_eq!(h.matches("v1=").count(), 2);
    assert!(verify_signature(&h, b"{\"a\":1}", "whsec_old", 300, 1_700_000_100));
    assert!(!verify_signature(&h, b"{\"a\":2}", "whsec_new", 300, 1_700_000_100));
    assert!(!verify_signature(&h, b"{\"a\":1}", "whsec_new", 300, 1_700_000_301));
    assert!(verify_signature(&format!("{h},v0=junk,x=y"), b"{\"a\":1}", "whsec_new", 300, 1_700_000_000));
}
// src/delivery/mod.rs
#[test] fn patterns() {
    assert!(event_matches("*", "customer.created")); assert!(event_matches("customer.*", "customer.deleted"));
    assert!(event_matches("v1.customer.*", "v1.customer.updated")); assert!(!event_matches("customer.*", "v1.customer.updated"));
    assert!(!event_matches("customer.created", "customer.updated"));
}
// tests/destinations.rs
#[tokio::test] async fn config_and_control_destinations_are_listed() {
    let m = mock_with("[[destinations]]\nname = \"snap\"\nurl = \"http://127.0.0.1:9/a\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]\nsecret = \"whsec_fixed\"").await;
    let thin = m.add_destination(DestinationConfig { name: "thin".into(), url: "http://127.0.0.1:9/b".into(), payload: PayloadFormat::Thin,
        enabled_events: vec!["v1.customer.*".into()], connect: false, events_from: None, secret: None }).unwrap();
    assert!(thin.id.starts_with("ed_test_") && thin.secret.starts_with("whsec_"));
    assert_eq!(m.destinations()[0].secret, "whsec_fixed");
    let v1: Value = get(&m, "/v1/webhook_endpoints").await.json().await.unwrap();
    assert_eq!(v1["data"][0]["object"], "webhook_endpoint"); assert!(!v1.to_string().contains("whsec_"));
    let v2: Value = get(&m, "/v2/core/event_destinations").await.json().await.unwrap();
    assert_eq!(v2["data"][0]["event_payload"], "thin"); assert_eq!(v2["data"][0]["events_from"], json!(["@self"]));
    m.set_destination_enabled(&thin.id, false).unwrap();
    let v2: Value = get(&m, "/v2/core/event_destinations").await.json().await.unwrap();
    assert_eq!(v2["data"][0]["status"], "disabled");
    m.remove_destination(&thin.id).unwrap();
    assert!(matches!(m.remove_destination(&thin.id), Err(MockError::NoSuchDestination(_))));
}
#[tokio::test] async fn rotate_keeps_old_secret_for_overlap() {
    let m = mock_with("[[destinations]]\nname = \"s\"\nurl = \"http://127.0.0.1:9/a\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]\nsecret = \"whsec_old\"").await;
    let id = m.destinations()[0].id.clone();
    let new = m.rotate_secret(&id, Duration::from_secs(60)).unwrap();
    assert_ne!(new, "whsec_old"); assert_eq!(m.destinations()[0].secret, new);
}
```

(The end-to-end signature checks on real deliveries, including Review Focus 2, are in Task 7.)

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ductkit-stripe-mock --lib -- delivery::` and `--test destinations`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): destinations and webhook signing`

---

### Task 7: Delivery: scheduling, sending, log

**Files:**
- Create: `src/delivery/sender.rs`, `tests/delivery.rs`
- Modify: `src/delivery/mod.rs`, `src/engine/mod.rs` (schedule on event creation), `src/handle.rs` (delivery loop in `advance_to`; wall ticker sends), `src/api/events_v1.rs` (`pending_webhooks`, `delivery_success`), `tests/common/mod.rs` (`Receiver`), crate `Cargo.toml` (reqwest becomes a normal dependency)

**Interfaces:**
- Consumes: Tasks 3 and 6.
- Produces:
  - **Public log types** (re-exported, `Debug, Clone, PartialEq, Serialize`):
    - `pub struct DeliveryRecord { pub seq: u64, pub at: SimTime, pub delivery_id: String, pub event_id: String, pub destination_id: String, pub kind: DeliveryRecordKind }`
    - `pub enum DeliveryRecordKind { Scheduled { attempt_at: SimTime, duplicate: bool, jitter_ms: i64, object_version: u64 }, Attempt { attempt: u32, outcome: AttemptOutcome, elapsed_ms: u64, object_version: u64, latest_version: u64, signature: SignatureFault, next_retry_at: Option<SimTime> }, Abandoned { reason: AbandonReason }, DestinationDisabled }`
    - `pub enum AttemptOutcome { Success { status: u16 }, HttpError { status: u16 }, Timeout, ConnectError { message: String }, Outage }`
    - `pub enum SignatureFault { None, Skewed { secs: i64 }, Invalid }`
    - `pub enum AbandonReason { RetriesExhausted, DestinationDisabled, DestinationRemoved }`
  - **`pub(crate) struct Deliveries`.** Holds a queue keyed `(due: SimTime, delivery_seq: u64)`. Methods:
    - `schedule_for_event(&mut self, ev: &EventRecord, dests: &Destinations, cfg: &WebhooksConfig, rngs)`
    - `due_batch(&mut self, upto: SimTime) -> Option<(SimTime, Vec<PreparedAttempt>)>`: the earliest due time ≤ `upto`, plus all attempts due at exactly that time, marked in flight
    - `record_outcome(&mut self, a: &PreparedAttempt, outcome: AttemptOutcome, elapsed_ms: u64, now: SimTime, ...)`
    - `next_due(&self) -> Option<SimTime>`
    - `log(&self) -> &[DeliveryRecord]`
    - `pending_for_event(&self, event_id) -> usize`
  - `pub(crate) struct PreparedAttempt { delivery_id, event_id, destination_id, url, body: Vec<u8>, secrets: Vec<String>, signature: SignatureFault, attempt: u32, scheduled_at: SimTime, timeout_ms: u64, object_version: u64 }`. The body is built at preparation time.
    - Snapshot body: the stored payload with `pending_webhooks` set to the current count.
    - Thin body: `thin_notification(payload)`.
  - `pub(crate) async fn send(client: &reqwest::Client, a: &PreparedAttempt) -> (AttemptOutcome, u64 /* elapsed ms */)`:
    - Sends POST with headers `Content-Type: application/json; charset=utf-8`, `User-Agent: Stripe/1.0 (+https://stripe.com/docs/webhooks)`, and `Stripe-Signature: sign(real_unix_now + skew, body, secrets)`. For `Invalid`, it signs with `"whsec_invalid"` instead.
    - Uses the timeout `timeout_ms` (real time). A 2xx is `Success`, any other status is `HttpError`, and a timeout is `Timeout`, even if the receiver later responds.
  - `MockStripe::delivery_log(&self) -> Vec<DeliveryRecord>`
  - **Test helper `tests/common::Receiver`:**
    - `Receiver::start(respond: impl Fn(usize) -> (u16, Duration) + Send + Sync + 'static) -> Receiver`. The argument is the 0-based request index.
    - `url(&self, path: &str) -> String`
    - `requests(&self) -> Vec<Received { headers: HeaderMap, body: Vec<u8>, at: Instant }>`
    - `wait_for(&self, n: usize, timeout: Duration) -> Vec<Received>`

**Ownership.** `Deliveries` also lives in `Engine`. `Engine::record` calls `schedule_for_event` right after projecting each event. The RNG streams named below are owned by `Deliveries`.

**Test helper `dest`.** Put it in `tests/common/mod.rs`, so Tasks 8–10 can reuse it: `pub fn dest(url: &str, payload: &str, events: &str) -> String`, which returns one `[[destinations]]` block with `secret = "whsec_test"`.

**Scheduling.** At event creation, for each matching enabled destination (snapshot destinations get snapshot events, thin destinations get thin events):
1. Draw `delay = sample_ms(delivery_delay)` from stream `"webhooks.delay"`.
2. Draw `jitter = uniform_ms(0, reorder_window)` from `"webhooks.reorder"` (no draw when the window is 0).
3. Set `attempt_at = created + delay + jitter`. Log `Scheduled { duplicate: false, … }`.
4. Then, with `"webhooks.duplicate"`: `if chance(duplicate_probability)`, schedule a second independent delivery (new `delivery_id`, its own delay and jitter draws) and log it with `duplicate: true`.

**Signature faults.** Drawn per attempt from `"webhooks.faults"`:
- With `chance(clock_skew.probability)`, the fault is `Skewed { secs }`. The sign comes from `below(2)`, and the magnitude from `uniform_ms(tolerance_ms + 1000, max_ms) / 1000`.
- Otherwise, with `chance(invalid_signature_probability)`, the fault is `Invalid`.
- Otherwise there is no fault.

**Delivery loop.**
- **Virtual time.** Under the step lock, `advance_to(t)` loops:
  1. Under the sim lock, let `d = deliveries.next_due()`.
  2. Run the generator to `min(t, d)` (or to `t` if there is no `d`). Generated events may schedule earlier attempts, so recompute `d`.
  3. Take `due_batch(min(t, d))`. If there is none, stop.
  4. Release the lock, send the batch concurrently (`futures_util::future::join_all`), then re-lock and `record_outcome` each result with `now` = the batch's due time.
  5. Repeat until there is no due attempt ≤ `t`, then set the time to `t`.
- **Wall mode.** Each tick, after `run_until`, take every `due_batch` ≤ now and `tokio::spawn` the sends. Each spawned task records its own outcome.
- **Control calls.** `create_customer`, etc. only schedule deliveries. Those are sent by the next `step` (virtual) or tick (wall).

**Success outcome.** Log `Attempt` with `next_retry_at: None` and mark the delivery done. The destination's `first_failure_since_success` is reset. Failure handling is Task 8. In this task, a failure logs `Attempt` and leaves the delivery pending with no retry.

**`/v1/events` additions.**
- `pending_webhooks` = the number of this event's deliveries not yet succeeded, excluding duplicates.
- `delivery_success=true` keeps events with `pending_webhooks == 0`; `false` keeps the rest.

- [ ] **Step 1: Write failing tests in `tests/delivery.rs`**

```rust
#[tokio::test] async fn snapshot_delivery_is_signed_with_real_time() {     // Review Focus 2
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/hook"), "snapshot", "*")).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(1, Duration::from_secs(5));
    let body = &got[0].body; let ev: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(ev["type"], "customer.created"); assert_eq!(ev["pending_webhooks"], 1);
    let sig = got[0].headers["stripe-signature"].to_str().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!(verify_signature(sig, body, "whsec_test", 300, now));
    let log = m.delivery_log();
    assert!(matches!(log.last().unwrap().kind, DeliveryRecordKind::Attempt { outcome: AttemptOutcome::Success { status: 200 }, .. }));
}
#[tokio::test] async fn thin_delivery_is_a_notification() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/thin"), "thin", "v1.customer.*")).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let ev: Value = serde_json::from_slice(&rx.wait_for(1, Duration::from_secs(5))[0].body).unwrap();
    assert_eq!(ev["object"], "v2.core.event"); assert!(ev.get("snapshot_event").is_none());
    assert_eq!(ev["related_object"]["type"], "customer");
}
#[tokio::test] async fn delay_is_simulated_and_pending_counts() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("{}[webhooks]\ndelivery_delay = {{ distribution = \"fixed\", ms = 30000 }}", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(29)).await.unwrap();
    assert!(rx.requests().is_empty());
    let pending: Value = get(&m, "/v1/events?delivery_success=false").await.json().await.unwrap();
    assert_eq!(pending["data"][0]["pending_webhooks"], 1);
    m.step(Duration::from_secs(1)).await.unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).len(), 1);
    let done: Value = get(&m, "/v1/events?delivery_success=true").await.json().await.unwrap();
    assert_eq!(done["data"].as_array().unwrap().len(), 1);
}
#[tokio::test] async fn filtered_events_are_not_delivered() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "customer.deleted")).await;
    let id = m.create_customer(NewCustomer::default()).unwrap(); m.delete_customer(&id).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(1, Duration::from_secs(5));
    assert_eq!(got.len(), 1);
    assert_eq!(serde_json::from_slice::<Value>(&got[0].body).unwrap()["type"], "customer.deleted");
}
#[tokio::test] async fn generated_traffic_is_delivered_in_step() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("seed = 1\n{}[actions]\nnew_customer = 60.0", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.step(Duration::from_secs(3600)).await.unwrap();
    let created = m.truth_log().len();
    assert!(created > 30);
    assert_eq!(rx.wait_for(created, Duration::from_secs(10)).len(), created);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test delivery`
- [ ] **Step 3: Implement** `Deliveries`, `sender.rs`, the loop changes, the `/v1/events` wiring and the `Receiver` helper.
- [ ] **Step 4: Run to verify they pass.** 5 passed, and the full suite is green.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): webhook delivery with delivery log`

---

### Task 8: Delivery failures: retries, timeouts, outages, disabling

**Files:**
- Create: `tests/delivery_faults.rs`
- Modify: `src/delivery/mod.rs`, `src/handle.rs`

**Interfaces:**
- Consumes: Task 7.
- Produces:
  - `pub(crate) fn retry_delays(schedule: &RetrySchedule) -> impl Iterator<Item = i64>`: the ms delays after attempts 1, 2, …
    - `StripeLike` yields `60_000, 300_000, 1_800_000, 7_200_000, 18_000_000`, then `36_000_000` forever.
    - `Explicit` yields its list once.
  - `MockStripe::start_outage(&self, id: &str, duration: Duration) -> Result<(), MockError>` and `end_outage(&self, id: &str) -> Result<(), MockError>`. During an outage, attempts to that destination are not sent. They are recorded as `Outage` and treated as failures.

**Failure handling** (`record_outcome` with a non-success outcome):
1. Set the destination's `first_failure_since_success` if it is unset.
2. Compute the next delay from `retry_delays` for this attempt number.
   - If there is one and `(scheduled_at + delay) − first_attempt_at <= max_retry_duration`, queue a retry at `scheduled_at + delay` and log `next_retry_at`.
   - Otherwise, log `Abandoned { RetriesExhausted }`.
3. If `now − first_failure_since_success >= disable_after`, disable the destination and log `DestinationDisabled` once. Every pending delivery for it is then abandoned with `DestinationDisabled`.

**Disabling or removing a destination.** Abandon each of its pending deliveries with `DestinationDisabled` or `DestinationRemoved`. An in-flight result that arrives later is still logged as an `Attempt`, but schedules nothing. Disabled destinations receive no new deliveries. Re-enabling does not revive abandoned deliveries.

- [ ] **Step 1: Write failing tests in `tests/delivery_faults.rs`**

```rust
#[tokio::test] async fn failing_endpoint_retries_on_schedule_then_gives_up() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&format!("{}[webhooks]\nretry_schedule = [\"1m\", \"5m\"]", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(rx.wait_for(3, Duration::from_secs(5)).len(), 3);
    let attempts: Vec<_> = m.delivery_log().into_iter().filter_map(|r| match r.kind { DeliveryRecordKind::Attempt { attempt, .. } => Some((attempt, r.at)), _ => None }).collect();
    let t0 = attempts[0].1;
    assert_eq!(attempts, vec![(1, t0), (2, t0.plus_ms(60_000)), (3, t0.plus_ms(360_000))]);
    assert!(matches!(m.delivery_log().last().unwrap().kind, DeliveryRecordKind::Abandoned { reason: AbandonReason::RetriesExhausted }));
}
#[tokio::test] async fn slow_200_counts_as_timeout_and_is_retried() {    // Review Focus 4
    let rx = Receiver::start(|i| (200, if i == 0 { Duration::from_millis(1500) } else { Duration::ZERO }));
    let m = mock_with(&format!("{}[webhooks]\nresponse_timeout = \"1s\"\nretry_schedule = [\"1m\"]", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(120)).await.unwrap();
    let got = rx.wait_for(2, Duration::from_secs(5));
    let id = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap()["id"].clone();
    assert_eq!(id(&got[0].body), id(&got[1].body), "same event delivered twice");
    let outcomes: Vec<_> = m.delivery_log().into_iter().filter_map(|r| match r.kind { DeliveryRecordKind::Attempt { outcome, .. } => Some(outcome), _ => None }).collect();
    assert_eq!(outcomes, vec![AttemptOutcome::Timeout, AttemptOutcome::Success { status: 200 }]);
}
#[tokio::test] async fn disabling_abandons_pending_and_stops_new() {     // Review Focus 5
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.destinations()[0].id.clone();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();                     // first attempt fails, retry queued
    m.set_destination_enabled(&id, false).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();                   // not queued
    m.step(Duration::from_secs(86_400)).await.unwrap();
    assert_eq!(rx.requests().len(), 1);
    assert!(m.delivery_log().iter().any(|r| matches!(r.kind, DeliveryRecordKind::Abandoned { reason: AbandonReason::DestinationDisabled })));
    assert_eq!(m.delivery_log().iter().filter(|r| matches!(r.kind, DeliveryRecordKind::Scheduled { .. })).count(), 1);
}
#[tokio::test] async fn removing_abandons_pending() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.destinations()[0].id.clone();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    m.remove_destination(&id).unwrap();
    m.step(Duration::from_secs(86_400)).await.unwrap();
    assert_eq!(rx.requests().len(), 1);
    assert!(m.delivery_log().iter().any(|r| matches!(r.kind, DeliveryRecordKind::Abandoned { reason: AbandonReason::DestinationRemoved })));
}
#[tokio::test] async fn persistent_failure_disables_destination() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&format!("{}[webhooks]\ndisable_after_failures = \"1h\"\nretry_schedule = [\"30m\", \"30m\", \"30m\"]", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(4 * 3600)).await.unwrap();
    assert!(!m.destinations()[0].enabled);
    assert_eq!(m.delivery_log().iter().filter(|r| matches!(r.kind, DeliveryRecordKind::DestinationDisabled)).count(), 1);
}
#[tokio::test] async fn outage_fails_without_http_then_recovers() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("{}[webhooks]\nretry_schedule = [\"1m\"]", dest(&rx.url("/h"), "snapshot", "*"))).await;
    let id = m.destinations()[0].id.clone();
    m.start_outage(&id, Duration::from_secs(30)).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(120)).await.unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).len(), 1);
    let outcomes: Vec<_> = m.delivery_log().into_iter().filter_map(|r| match r.kind { DeliveryRecordKind::Attempt { outcome, .. } => Some(outcome), _ => None }).collect();
    assert_eq!(outcomes, vec![AttemptOutcome::Outage, AttemptOutcome::Success { status: 200 }]);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test delivery_faults`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass.** 6 passed, and the full suite is green.
- [ ] **Step 5: fmt, clippy, commit** — `feat(stripe-mock): retries, timeouts, outages and endpoint disabling`

---

### Task 9: Duplicates, reordering and signature faults

**Files:**
- Modify: `tests/delivery_faults.rs`, `src/delivery/mod.rs` (if not already complete from Task 7)

**Interfaces:**
- Consumes: Task 7 scheduling and fault draws. This task proves them end to end, and finishes anything Task 7 left as a stub.

- [ ] **Step 1: Write failing tests (append to `tests/delivery_faults.rs`)**

```rust
#[tokio::test] async fn duplicates_deliver_same_event_twice() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("{}[webhooks]\nduplicate_probability = 1.0", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(2, Duration::from_secs(5));
    let id = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap()["id"].clone();
    assert_eq!(id(&got[0].body), id(&got[1].body));
    assert_eq!(m.delivery_log().iter().filter(|r| matches!(r.kind, DeliveryRecordKind::Scheduled { duplicate: true, .. })).count(), 1);
}
#[tokio::test] async fn reorder_window_can_swap_deliveries() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("seed = 11\n{}[webhooks]\nreorder_window = \"10s\"", dest(&rx.url("/h"), "snapshot", "*"))).await;
    let mut made = vec![];
    for _ in 0..20 { made.push(m.create_customer(NewCustomer::default()).unwrap()); m.step(Duration::from_millis(500)).await.unwrap(); }
    m.step(Duration::from_secs(20)).await.unwrap();
    let got: Vec<String> = rx.wait_for(20, Duration::from_secs(5)).iter().map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["data"]["object"]["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(got.len(), 20); assert_ne!(got, made, "seed 11 should reorder at least one pair");
    let mut sorted = got.clone(); sorted.sort(); let mut m2 = made.clone(); m2.sort(); assert_eq!(sorted, m2);
}
#[tokio::test] async fn skewed_and_invalid_signatures_fail_verification() {
    for (fault, expect_skew) in [("clock_skew = { probability = 1.0, max = \"10m\" }", true), ("invalid_signature_probability = 1.0", false)] {
        let rx = Receiver::start(|_| (200, Duration::ZERO));
        let m = mock_with(&format!("{}[webhooks]\n{fault}", dest(&rx.url("/h"), "snapshot", "*"))).await;
        m.create_customer(NewCustomer::default()).unwrap();
        m.step(Duration::from_millis(1)).await.unwrap();
        let r = &rx.wait_for(1, Duration::from_secs(5))[0];
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!(!verify_signature(r.headers["stripe-signature"].to_str().unwrap(), &r.body, "whsec_test", 300, now), "{fault}");
        let fault_logged = m.delivery_log().iter().find_map(|r| match &r.kind { DeliveryRecordKind::Attempt { signature, .. } => Some(signature.clone()), _ => None }).unwrap();
        assert_eq!(matches!(fault_logged, SignatureFault::Skewed { .. }), expect_skew);
    }
}
#[tokio::test] async fn rotated_secret_signs_with_both_during_overlap() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.destinations()[0].id.clone();
    let new = m.rotate_secret(&id, Duration::from_secs(60)).unwrap();
    m.create_customer(NewCustomer::default()).unwrap(); m.step(Duration::from_millis(1)).await.unwrap();
    m.step(Duration::from_secs(61)).await.unwrap();
    m.create_customer(NewCustomer::default()).unwrap(); m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(2, Duration::from_secs(5));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let sig = |i: usize| got[i].headers["stripe-signature"].to_str().unwrap().to_string();
    assert!(verify_signature(&sig(0), &got[0].body, "whsec_test", 300, now) && verify_signature(&sig(0), &got[0].body, &new, 300, now));
    assert!(!verify_signature(&sig(1), &got[1].body, "whsec_test", 300, now) && verify_signature(&sig(1), &got[1].body, &new, 300, now));
}
#[tokio::test] async fn delivery_decisions_are_deterministic() {
    let cfg = |url: &str| format!("seed = 9\n{}[actions]\nnew_customer = 30.0\n[webhooks]\nduplicate_probability = 0.2\nreorder_window = \"5s\"\ndelivery_delay = {{ distribution = \"lognormal\", median_ms = 300, p99_ms = 10000 }}", dest(url, "snapshot", "*"));
    let scheduled = |m: &MockStripe| m.delivery_log().into_iter().filter(|r| matches!(r.kind, DeliveryRecordKind::Scheduled { .. })).map(|r| (r.event_id, r.kind)).collect::<Vec<_>>();
    let (r1, r2) = (Receiver::start(|_| (200, Duration::ZERO)), Receiver::start(|_| (200, Duration::ZERO)));
    let (a, b) = (mock_with(&cfg(&r1.url("/h"))).await, mock_with(&cfg(&r2.url("/h"))).await);
    a.step(Duration::from_secs(7200)).await.unwrap(); b.step(Duration::from_secs(7200)).await.unwrap();
    assert!(!scheduled(&a).is_empty()); assert_eq!(scheduled(&a), scheduled(&b));
}
```

**`reorder_window` seed.** If seed 11 happens not to reorder any pair, pick the first seed in 1..100 that does and record the ruling. The assertion's intent is "the window can reorder deliveries"; the seed is incidental.

- [ ] **Step 2: Run to verify.** Any test that fails exposes a gap left by Task 7. If all pass immediately, record a ruling: "Task 9 tests passed without code changes; Task 7 implemented the behaviour". That is acceptable, because these tests are end-to-end proofs.
- [ ] **Step 3: Implement** the missing pieces.
- [ ] **Step 4: Run to verify they pass.** All `delivery_faults` tests pass.
- [ ] **Step 5: fmt, clippy, commit** — `test(stripe-mock): duplicates, reordering and signature faults`

---

### Task 10: Quiesce, generation control, runtime webhook settings, wall-mode delivery

**Files:**
- Create: `tests/quiesce.rs`
- Modify: `src/handle.rs`, `src/generate.rs` (an `enabled` flag)

**Interfaces:**
- Produces on `MockStripe`:
  - `pub async fn quiesce(&self) -> Result<(), MockError>`
  - `pub fn resume_generation(&self)`
  - `pub fn generation_enabled(&self) -> bool`
  - `pub fn update_webhooks_config(&self, f: impl FnOnce(&mut WebhooksConfig)) -> Result<(), MockError>` (validated; affects future scheduling and attempts only)
  - `Generator::set_enabled(&mut self, on: bool)`. While disabled, `run_until` only moves time. On re-enable, each action's next due time is redrawn from `now`, using its own stream.

**Quiesce algorithm.** Hold the step lock throughout.
1. Disable generation.
2. Set every event's `visible_at = min(visible_at, now)`.
3. Loop:
   - Move every pending delivery's due time to `now`.
   - Take `due_batch(now)`. If there is none, stop.
   - Send the batch and record outcomes. In quiesce mode, retries are queued at `now`, but the attempt count still follows `retry_delays` and `max_retry_duration` (computed from each attempt's original timeline, as if the backoff had elapsed).
   - Repeat until nothing is pending.
4. Simulated time does not move.

In wall mode, `quiesce` also pauses the ticker's generation (the clock keeps running).

- [ ] **Step 1: Write failing tests in `tests/quiesce.rs`**

```rust
#[tokio::test] async fn quiesce_flushes_lag_and_deliveries() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("{}[events]\nvisibility_lag = {{ distribution = \"fixed\", ms = 600000 }}\n[webhooks]\ndelivery_delay = {{ distribution = \"fixed\", ms = 600000 }}\n[actions]\nnew_customer = 60.0", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.step(Duration::from_secs(600)).await.unwrap();
    let t = m.now(); m.quiesce().await.unwrap();
    assert_eq!(m.now(), t); assert!(!m.generation_enabled());
    let n = m.truth_log().len();
    assert_eq!(rx.wait_for(n, Duration::from_secs(10)).len(), n);
    let listed: Value = get(&m, "/v1/events?limit=100").await.json().await.unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), n.min(100));
    m.step(Duration::from_secs(3600)).await.unwrap(); assert_eq!(m.truth_log().len(), n);
    m.resume_generation(); m.step(Duration::from_secs(3600)).await.unwrap(); assert!(m.truth_log().len() > n);
}
#[tokio::test] async fn quiesce_exhausts_failing_retries_without_waiting() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;            // stripe_like, 3d
    m.create_customer(NewCustomer::default()).unwrap();
    let started = std::time::Instant::now();
    m.quiesce().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(rx.requests().len(), 12);                                       // 1 + 5 + 6 retries within 3 days
    assert!(matches!(m.delivery_log().last().unwrap().kind, DeliveryRecordKind::Abandoned { reason: AbandonReason::RetriesExhausted }));
}
#[tokio::test] async fn runtime_webhook_settings_apply_to_new_events() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    m.update_webhooks_config(|w| w.delivery_delay = Distribution::Fixed { ms: 60_000 }).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(59)).await.unwrap(); assert!(rx.requests().is_empty());
    m.step(Duration::from_secs(1)).await.unwrap(); assert_eq!(rx.wait_for(1, Duration::from_secs(5)).len(), 1);
    assert!(m.update_webhooks_config(|w| w.duplicate_probability = 2.0).is_err());
}
#[tokio::test] async fn wall_mode_delivers() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!("clock = {{ mode = \"wall\", speed = 1.0 }}\n{}", dest(&rx.url("/h"), "snapshot", "*"))).await;
    m.create_customer(NewCustomer::default()).unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).len(), 1);
}
```

`quiesce_exhausts_failing_retries_without_waiting` checks the `stripe_like` arithmetic. Cumulative delays: 1m, 6m, 36m, 2h36m, 7h36m, then +10h each: 17h36m, 27h36m, 37h36m, 47h36m, 57h36m, 67h36m (≤ 72h). The next would be 77h36m (> 72h). That gives 11 retries plus the first attempt, so 12 requests.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ductkit-stripe-mock --test quiesce`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass.** 4 passed. Then run `cargo test --workspace`: all green.
- [ ] **Step 5: Update the spec.**
  - Section 11 "Quiesce": add "Simulated time does not move; pending retries are attempted immediately until they succeed or exhaust their schedule. Generation stays off until `resume_generation`."
  - Section 9: add "The signature timestamp `t` is real wall-clock time, so receivers can verify it; event `created` is simulated time."
- [ ] **Step 6: fmt, clippy, commit** — `feat(stripe-mock): quiesce, generation control and runtime webhook settings`
