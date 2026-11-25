# Stripe mock: specification

Status: draft · Crate: `crates/stripe-mock` (package `ductkit-stripe-mock`)

## 1. Purpose

`ductkit` syncs Stripe entities into SQL databases (Postgres first, MySQL and others later). The sync engine has to be correct under the conditions real Stripe produces: at-least-once and out-of-order webhooks, an eventually consistent Events API, stale event payloads, rate limits, and backfills that race live changes.

Those conditions are hard to reproduce against real Stripe and impossible to reproduce deterministically. This crate is a **stateful mock of the Stripe API and webhook sender** that:

- simulates one or more accounts (optionally Connect platforms with connected accounts),
- creates, updates and deletes entities realistically over time,
- exposes them through a Stripe-compatible REST API, a pollable Events API and signed webhooks,
- injects configurable disruptions, and
- publishes an authoritative **change stream** so tests can verify the sync target *as changes happen*, not only by comparing final states.

### Goals

- Wire-level fidelity for the parts of Stripe the sync engine uses.
- Deterministic, replayable runs (seed + virtual clock).
- Both stochastic load and scripted, exact scenarios.
- Usable in-process from Rust tests and as a standalone server for end-to-end and CLI tests.
- Explainable failures: every delivery decision is logged with its reason.

### Non-goals

- Write-side fidelity for the full Stripe API (payments flows, card validation, Radar, tax computation). The mock *generates* writes internally; its public write surface is limited to what the sync engine needs (e.g. webhook endpoint management).
- Covering every Stripe object type from day one.
- Replacing occasional calibration runs against real Stripe test mode.

## 2. Core model: truth vs. projection

The mock maintains two distinct layers. Keeping them separate is the central design rule.

```
          ┌────────────────────────────┐
 actions →│  Truth log (authoritative) │──► Change stream  (for test verification)
          │  per-object versions       │
          └─────────────┬──────────────┘
                        │ projection (lossy, delayed, reordered, faulty)
          ┌─────────────▼──────────────┐
          │  Stripe-visible surface    │──► REST objects / lists
          │                            │──► Events API (/v1/events)
          │                            │──► Webhook deliveries
          └────────────────────────────┘
```

- **Truth log**: every mutation, in the order it really happened. Each object has a monotonically increasing `version` (1 on create). The truth log is never faulty.
- **Projection**: what a Stripe client can observe. All fault injection lives here. Faults never alter the truth log.

Tests assert the sync target against the truth log, using the change stream.

## 3. Architecture

```
ductkit-stripe-mock
├── engine      clock, RNG, truth log, entity store, scheduler
├── model       entity schemas + per-type lifecycle state machines
├── generate    action generators (stochastic + scripted scenarios)
├── project     event creation, visibility scheduling, webhook queue
├── faults      fault policies applied by `project` and the HTTP layer
├── api         Stripe-compatible HTTP API (axum)
├── control     control/admin API (separate router + Rust handle)
└── stream      change stream (async channel in-process; SSE/WebSocket over HTTP)
```

Two run modes share one engine:

| Mode | Use | Change stream | Clock |
|------|-----|---------------|-------|
| In-process (`MockStripe::start(config)` returns a handle) | Fast integration tests in `cargo test` | `tokio::sync::broadcast` / `Stream` | Virtual (default) or wall |
| Standalone server (`ductkit-stripe-mock serve --config mock.toml`) | E2E tests, CLI tests, manual exploration | SSE and WebSocket on the control port | Wall (default) or virtual, driven via control API |

The Stripe-compatible API and the control API listen on **separate ports** (or routers), so a sync engine under test can never reach control endpoints by accident.

## 4. Configuration

Configuration is a TOML file (or the equivalent Rust struct). All rates are per simulated hour unless stated. Every random choice draws from the seeded RNG.

```toml
seed = 42
api_version = "2025-xx-xx"          # pinned Stripe API version stamped on payloads
clock = { mode = "virtual", start = "2026-01-01T00:00:00Z", speed = 60.0 }  # speed only for wall-paced virtual time

[accounts]
platforms = 1
connect = true
connected_accounts = { initial = 20, connect_rate = 0.5, deauthorize_rate = 0.1 }
account_types = { standard = 0.5, express = 0.3, custom = 0.2 }

[history]
# Pre-generated data that exists before the sync starts.
# Some of it is older than the event retention window, so only backfill can see it.
duration = "90d"
scale = 1.0

[event_retention]
window = "30d"

# Business actions drive fan-out (see section 6).
[actions]
new_customer = 10.0
start_subscription = 4.0
cancel_subscription = 0.5
renew_subscription_cycle = "natural"   # driven by billing periods
one_off_payment = 6.0
refund = 0.4
dispute = 0.05
catalog_change = 0.2                   # products/prices created, archived

# Extra per-type churn on top of actions.
[churn.customer]
update = 3.0         # metadata, email, address
delete = 0.2
[churn.product]
update = 0.5
archive = 0.1

[api]
latency = { distribution = "lognormal", median_ms = 40, p99_ms = 400 }
rate_limit = { requests_per_second = 25, burst = 25 }
errors = { "500" = 0.001, "503" = 0.001, timeout = 0.0005, reset = 0.0005 }
stale_read = { probability = 0.0, max_lag = "2s" }   # retrieve/list may return an older version

[events]
# Delay between an event's `created` and when it appears in /v1/events.
visibility_lag = { distribution = "lognormal", median_ms = 200, p99_ms = 5000 }
late_tail = { probability = 0.001, min = "30s", max = "10m" }
same_second_shuffle = true           # unspecified order within one second of `created`

[webhooks]
delivery_delay = { distribution = "lognormal", median_ms = 300, p99_ms = 10000 }
duplicate_probability = 0.01
reorder_window = "5s"                # deliveries within this window may swap
response_timeout = "10s"             # slower 2xx counts as failure, so a retry follows
retry_schedule = "stripe_like"       # exponential backoff; compressible in virtual time
max_retry_duration = "3d"
disable_after_failures = "stripe_like"
signature_tolerance = "300s"
clock_skew = { probability = 0.0, max = "10m" }    # signatures outside tolerance

[payloads]
snapshot_events = true               # data.object as of event time
thin_events = false                  # v2-style reference-only events
nested_list_page_size = 10           # force has_more on embedded lists
expand_probability = 0.0             # chance an expandable field is returned expanded unasked (should be 0 for fidelity)
```

Configuration can be changed at runtime through the control API (section 11). Changes are recorded in the truth log, so replays reproduce them.

## 5. Entities and lifecycles

### 5.1 Initial scope (phase 1)

Chosen to cover each deletion style and the main relationship shapes:

| Type | ID prefix | Delete style | Notable relations |
|------|-----------|--------------|-------------------|
| `customer` | `cus_` | Hard delete (retrieve returns `{id, object, deleted: true}`, dropped from lists) | parent of subscriptions, invoices |
| `product` | `prod_` | Archive only (`active: false`) | parent of prices |
| `price` | `price_` | Archive only (`active: false`), immutable amounts | product |
| `subscription` | `sub_` | State: `canceled` (stays retrievable) | customer, items, latest_invoice |
| `subscription_item` | `si_` | Hard delete; embedded list in subscription | subscription, price |
| `invoice` | `in_` | Drafts hard-delete; otherwise `void` / `uncollectible` | customer, subscription, lines |
| `invoice line item` | `il_` | Embedded list in invoice | invoice, price |

Phase 2 candidates: `payment_intent`, `charge`, `refund`, `dispute`, `payment_method`, `coupon`, `promotion_code`, `account` (Connect), `balance_transaction`, `payout`.

### 5.2 Lifecycle state machines

Each type has a state machine that only allows legal transitions, for example:

- `subscription.status`: `incomplete → active | incomplete_expired`, `active ⇄ past_due`, `active → canceled`, `trialing → active`, `active ⇄ paused`.
- `invoice.status`: `draft → open → paid | void | uncollectible`, `draft →` deleted.

### 5.3 Value realism

- Integer amounts in the smallest currency unit, including zero-decimal currencies (JPY etc.) and very large amounts.
- Unix-second timestamps.
- Metadata with Unicode, maximum key/value lengths and empty strings.
- Nullable fields that really are null; optional fields that are absent.
- Bursts of many updates to one object within the same second.

## 6. Action generation

### 6.1 Cause-driven fan-out

In Stripe, one action produces many events across objects. The generator models **business actions**, and each action expands into the realistic sequence of mutations. For example, `start_subscription`:

1. `customer.subscription.created`
2. `invoice.created` → `invoice.finalized`
3. `payment_intent.created` → `payment_intent.succeeded`, `charge.succeeded`
4. `invoice.paid`, `invoice.payment_succeeded`
5. `customer.subscription.updated` (status `incomplete → active`)

Mutations inside one action are spaced over a short, configurable span (often the same second), which produces the cross-object races that matter. Per-type `churn` rates add independent updates and deletes on top.

### 6.2 Scripted scenarios

Alongside random generation, tests can script exact sequences against the virtual clock, including projection-level decisions:

```rust
mock.script(|s| {
    let c = s.create_customer(json!({ "email": "a@example.com" }));
    s.update(&c, json!({ "metadata": { "tier": "gold" } }));    // v2
    s.update(&c, json!({ "metadata": { "tier": "silver" } }));  // v3
    s.webhooks().deliver_in_order([c.event(3), c.event(2)]);     // v3 before v2
    s.events().make_visible_after(c.event(2), Duration::from_secs(60));
});
```

Scripted scenarios are the regression suite for specific races; random runs look for new ones. A failing random run can be exported as a script.

### 6.3 Pre-existing history

Before the run starts, `history` generates data over a past period. Data older than the event retention window exists only through list endpoints, which forces the sync engine to backfill. Live generation continues during the backfill, so the race between backfill and live changes is always exercised.

## 7. Stripe-compatible API surface

### 7.1 Requests and responses

- `Authorization: Bearer sk_test_…` (any key from config; unknown keys → 401).
- `Stripe-Account: acct_…` for Connect; objects are scoped per account.
- Stripe error envelope (`{"error": {"type", "code", "message", ...}}`), `Request-Id` header, `Stripe-Should-Retry` on retryable errors.
- `Stripe-Version` header honoured only if it equals the pinned version (otherwise an error), until multi-version support exists.
- Form-encoded request bodies, as Stripe uses.

### 7.2 Endpoints (phase 1)

- Retrieve and list for every phase-1 type: `GET /v1/{type}/{id}`, `GET /v1/{type}`.
- Sub-lists for embedded collections: `GET /v1/subscription_items?subscription=…`, `GET /v1/invoices/{id}/lines`.
- `GET /v1/events`, `GET /v1/events/{id}`.
- `POST/GET/DELETE /v1/webhook_endpoints` (with `enabled_events`, `connect: true`, secret returned on create).
- `GET /v1/accounts`, `GET /v1/accounts/{id}` for Connect.

### 7.3 List semantics

- Ordered by `created` descending; cursor pagination via `starting_after` / `ending_before`; `limit` 1–100, default 10; `has_more`.
- `created[gt|gte|lt|lte]` filters where Stripe supports them.
- Deleted objects are excluded; archived ones are included (with filters such as `active=`).
- Embedded lists (`subscription.items`, `invoice.lines`) truncated to `nested_list_page_size` with `has_more: true` and a `url` for the sub-list.
- Expandable fields return IDs unless `expand[]` asks for the object.

### 7.4 Consistency faults on the API

- **Stale reads** (optional): retrieve/list may briefly return an older version of an object.
- **Writes during pagination**: objects created, changed or deleted while a client pages through a list, so a backfill can see an object twice, miss a new one, or page past a deleted one.
- Rate limiting (429) with a token bucket, plus random 5xx, timeouts and connection resets per `[api]`.

## 8. Events API

- Every truth-log mutation that Stripe would announce produces an event (`evt_…`) with `type`, `created`, `data.object` (snapshot at event time), `data.previous_attributes` (for `*.updated`), `account` (Connect), `api_version`, `livemode: false`, `request {id, idempotency_key}` and `pending_webhooks`.
- **Visibility scheduling**: each event gets a hidden `visible_at = created + visibility_lag (+ late_tail)`. `/v1/events` only returns events whose `visible_at` has passed. An event can therefore become visible *after* a newer one did, so "poll with `ending_before` = last seen" silently misses events. Catching this bug is a primary goal of the fixture.
- **Same-second ordering**: events sharing a `created` second have no guaranteed relative order (`same_second_shuffle`).
- **Retention**: events older than `event_retention.window` disappear from the API. A poller that falls further behind must detect the gap (e.g. its cursor event 404s) and fall back to a full backfill.
- `type` / `types[]` / `created` filters and `delivery_success`.
- Optional **thin events** (`payloads.thin_events`): reference-only payloads that force the consumer to fetch the current object.

## 9. Webhooks

- Endpoints registered through the API or config. Separate account and Connect endpoints; Connect events carry `account`.
- **At-least-once delivery**: duplicates are a baseline behaviour (`duplicate_probability`), not only a fault.
- **Ordering**: deliveries inside `reorder_window` may be reordered; delivery delay comes from a distribution with a long tail.
- **Retries**: non-2xx, connection failure or a response slower than `response_timeout` counts as failure. Retries follow a Stripe-like exponential backoff up to `max_retry_duration`; in virtual time the schedule can be compressed. A slow-but-successful handler therefore receives a duplicate.
- **Disabling**: an endpoint that keeps failing is disabled, as Stripe does; the control API can re-enable it.
- **Signatures**: `Stripe-Signature: t=<ts>,v1=<hmac-sha256>`. Supports secret rotation (several `v1` values), timestamps outside tolerance (`clock_skew`), and an invalid-signature fault for negative tests.
- **Payload staleness**: because payloads are snapshots, a delayed or retried delivery carries an outdated object. Each delivery's truth-log version is recorded so tests can tell whether the consumer applied an old version over a newer one.
- **Endpoint downtime**: the control API can make deliveries to an endpoint fail for a period ("outage"), building a retry backlog that later floods in.

## 10. Change stream

The change stream is the authoritative feed tests subscribe to. It carries truth-log entries, never projections.

```jsonc
{
  "seq": 1042,                       // global, gap-free, monotonic
  "at": "2026-01-01T00:12:03.120Z",  // virtual time of the mutation
  "account": "acct_123",             // null for platform
  "object_type": "customer",
  "object_id": "cus_abc",
  "version": 3,                      // per-object, starts at 1
  "kind": "updated",                 // created | updated | deleted | archived | state_changed
  "state": { /* full object after the mutation, API-shaped */ },
  "events": ["evt_1", "evt_2"],      // Stripe events this mutation produced
  "cause": { "action": "start_subscription", "action_id": "act_77" }
}
```

A second stream (or a flag on the same subscription) carries **projection records** (event became visible, delivery attempted, succeeded, failed, duplicated, reordered, dropped after retries), for debugging and delivery-aware assertions.

Subscribers can start from `seq = 0` (full replay) or from "now". In-process it is a Rust `Stream`; standalone it is SSE (`GET /control/stream?from=…`) and WebSocket.

## 11. Control API

Separate from the Stripe surface. Available as a Rust handle in-process and as HTTP under `/control` standalone.

- **Run control**: `pause`, `resume`, `step(duration)`, `advance_to(time)`, `set_speed`.
- **Quiesce**: stop generation, flush every pending webhook and retry, make every event visible, then return. Used before a final end-state comparison.
- **Triggers**: run a named action now; run a script.
- **Faults at runtime**: change any `[api]`, `[events]`, `[webhooks]` setting; start or stop an endpoint outage; force a rate-limit storm.
- **Accounts**: connect or deauthorize a Connect account.
- **Inspection**: current truth state (by type/ID or full dump), delivery log with the reason for every decision, event visibility timeline, effective config.
- **Export**: snapshot state to JSON; export the run (seed + config + runtime changes + scripted steps) as a replayable script.

## 12. Determinism and time

- One seeded RNG drives all generation and fault decisions. Separate RNG streams per subsystem (generation, API faults, event visibility, webhooks) so changing one setting doesn't reshuffle unrelated decisions.
- **Virtual clock** by default in-process: time only moves when the test advances it, or at a fixed multiple of wall time. Retry schedules, visibility lags and history all run on virtual time, so a "3 days of retries" scenario finishes in milliseconds.
- In wall-clock mode the HTTP layer still uses real latencies; such runs are not exactly reproducible, but the seed still governs generation.
- **Replay**: any run can be reproduced from its exported seed, config and runtime changes. Test failures should print the seed and the replay command.

## 13. Verification helpers

A companion module (`ductkit-stripe-mock::verify`) holds property checks that tests run against the change stream and a target database adapter:

- **Eventual convergence**: for every object, the target eventually holds the latest truth version (after quiesce, exactly).
- **No regression**: a row in the target never goes from version *n* to a lower version.
- **No resurrection**: a hard-deleted object never reappears.
- **Convergence latency**: time from truth mutation to target reflection stays under a bound (per percentile).
- **Referential integrity**: the target never holds a child whose parent is missing past a grace period (or the sync engine's chosen policy for orphans is respected).
- **Final state equality** after quiesce, as a last check.

The target adapter trait is database-agnostic, so the same checks apply to Postgres, MySQL and later targets.

## 14. Fidelity

- Generate object types and response shapes from Stripe's published **OpenAPI spec** for the pinned version, and validate every mock response against it in the mock's own tests. This prevents the mock from drifting away from real Stripe.
- Stripe's official `stripe-mock` is stateless and doesn't fit this purpose, but its OpenAPI-based fixtures can seed realistic example values.
- **Calibration**: periodically run equivalent scripted actions against real Stripe test mode and compare event fan-out, ordering and latency distributions with the mock's defaults. Calibration is a manual or scheduled job, not part of the regular test suite.
- One pinned API version at first. Multi-version payload rendering is a later extension.

## 15. Performance

- Must sustain load tests: many accounts, thousands of mutations per simulated minute, large histories (hundreds of thousands of objects) for backfill testing.
- Storage is in memory by default, with an optional on-disk snapshot so large histories don't regenerate on every run.
- Change stream subscribers must not be able to slow down generation; slow subscribers get bounded buffers and a clear error on overflow.

## 16. Phasing

1. **Engine skeleton**: clock, RNG, truth log, versions, change stream, control handle; `customer` only; retrieve/list; in-process mode.
2. **Events and webhooks**: event projection with visibility lag, retention, signatures, retries, duplicates, reordering; delivery log.
3. **Phase-1 types and fan-out**: product, price, subscription, items, invoice, lines; business actions; nested list truncation; deletion styles.
4. **History and backfill races**; API faults (rate limits, 5xx, stale reads, writes during pagination).
5. **Connect**: multiple accounts, `Stripe-Account`, connect/deauthorize.
6. **Standalone server**, SSE/WebSocket stream, HTTP control API, run export and replay.
7. **Verification helpers** wired against the Postgres sync target.
8. Phase-2 entity types, thin events, OpenAPI validation in CI, calibration job.

## 17. Open questions

- Which Stripe API version to pin first?
- Should the mock also accept writes from the sync engine (e.g. creating webhook endpoints only, or more), or remain strictly read-only beyond endpoint management?
- How should the verifier tolerate legitimate intermediate states (e.g. a target that skips version 2 and goes straight to 3)? Proposal: allow skips, forbid regressions.
- Should thin events / v2 event destinations be in scope for the first sync engine release?
- Orphan policy for out-of-order children: should the mock's verifier assume deferred foreign keys, placeholder parents, or a retry queue? This depends on sync engine design and should be decided there.
