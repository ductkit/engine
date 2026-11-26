# Stripe mock: specification

Status: draft · Crate: `crates/stripe-mock` (package `ductkit-stripe-mock`) · Pinned Stripe API version: `2026-11-18.endive`

Decisions taken so far are listed in [section 17](#17-decisions); open questions are in [section 18](#18-open-questions).

## 1. Purpose

`ductkit` syncs Stripe entities into SQL databases (Postgres first, MySQL and others later). The sync engine has to be correct under the conditions real Stripe produces: at-least-once and out-of-order webhooks, an eventually consistent Events API, stale event payloads, rate limits, and backfills that race live changes.

Those conditions are hard to reproduce against real Stripe and impossible to reproduce deterministically. This crate is a **stateful mock of the Stripe API and webhook sender** that:

- simulates one or more accounts (optionally Connect platforms with connected accounts),
- creates, updates and deletes entities realistically over time,
- exposes them through a Stripe-compatible REST API, a pollable Events API and signed webhooks,
- injects configurable disruptions, and
- offers a subscription to the **latest state of every object**, so tests can check the sync target *as changes happen*, not only by comparing final states.

### Goals

- Wire-level fidelity for the parts of Stripe the sync engine uses.
- Deterministic, replayable runs (seed + virtual clock).
- Both stochastic load and scripted, exact scenarios.
- Usable in-process from Rust tests and as a standalone server for end-to-end and CLI tests.
- Explainable failures: every delivery decision is logged with its reason.

### Non-goals

- **Accepting writes from the sync engine.** The Stripe-compatible API is read-only: every `POST`/`DELETE` returns a Stripe-style `invalid_request_error`. All mutations come from the mock's own generator, scripts or control API. Webhook endpoints and event destinations are configured through config or the control API, not through the Stripe API.
- Write-side fidelity for the Stripe API (payments flows, card validation, Radar, tax computation).
- **A verifier.** Checking that a sync target is correct belongs to the sync engine's test suite and will be built with it. The mock only provides what a verifier needs: a subscription to the latest state of every object (section 10).
- Covering every Stripe object type from day one.
- Replacing occasional calibration runs against real Stripe test mode.

## 2. Core model: truth vs. projection

The mock maintains two distinct layers. Keeping them separate is the central design rule.

```
          ┌────────────────────────────┐
 actions →│  Truth log (authoritative) │──► Latest-state subscription
          │  per-object versions       │
          └─────────────┬──────────────┘
                        │ projection (lossy, delayed, reordered, faulty)
          ┌─────────────▼──────────────┐
          │  Stripe-visible surface    │──► REST objects / lists
          │                            │──► Events API (/v1/events, /v2/core/events)
          │                            │──► Webhook deliveries
          └────────────────────────────┘
```

- **Truth log**: every mutation, in the order it really happened. Each object has a monotonically increasing `version` (1 on create). The truth log is never faulty.
- **Projection**: what a Stripe client can observe. All fault injection lives here. Faults never alter the truth log.

Tests (and, later, the verifier) assert the sync target against the truth log, using the latest-state subscription.

## 3. Architecture

```
ductkit-stripe-mock
├── engine      clock, RNG, truth log, entity store, scheduler
├── model       entity schemas + per-type lifecycle state machines
├── generate    action generators (stochastic + scripted scenarios)
├── project     snapshot + thin event creation, visibility scheduling, delivery queue
├── faults      fault policies applied by `project` and the HTTP layer
├── api         read-only Stripe-compatible HTTP API, v1 and v2 (axum)
├── control     control/admin API (separate router + Rust handle)
├── stream      latest-state subscription (async stream in-process; SSE/WebSocket over HTTP)
└── stripe      Rust types generated from the vendored OpenAPI spec, and the conformance check (section 14)
```

Two run modes share one engine:

| Mode | Use | Subscription | Clock |
|------|-----|---------------|-------|
| In-process (`MockStripe::start(config)` returns a handle) | Fast integration tests in `cargo test` | Rust `Stream` | Virtual (default) or wall |
| Standalone server (`ductkit-stripe-mock serve --config mock.toml`) | E2E tests, CLI tests, manual exploration | SSE and WebSocket on the control port | Wall (default) or virtual, driven via control API |

The Stripe-compatible API and the control API listen on **separate ports** (or routers), so a sync engine under test can never reach control endpoints by accident.

### 3.1 Packaging

One crate, `crates/stripe-mock` (package `ductkit-stripe-mock`), ships both:

- **A library** (`src/lib.rs`) that tests import, typically as a `[dev-dependencies]` entry. It exposes `MockStripe::start(config)`, the control handle, scripting and the latest-state subscription as Rust APIs.
- **A binary** (`src/main.rs`, command `ductkit-stripe-mock`) that runs the standalone server from a config file. It is a thin wrapper around the library.

No container image is produced.

The binary has three commands:

```
ductkit-stripe-mock serve  --config F [--api-addr A] [--control-addr A] [--clock virtual|wall] [--seed N] [--export-on-exit F]
ductkit-stripe-mock replay --run F [--api-addr A] [--control-addr A] [--export-on-exit F] [--drop-destinations]
ductkit-stripe-mock check  --config F
```

- **Addresses**: the Stripe-compatible API defaults to `127.0.0.1:8420` and the control API to `127.0.0.1:8421`. Port `0` picks a free port.
- **Ready line**: once both listeners are bound, the binary prints one JSON line to stdout, `{"api_url":"http://…","control_url":"http://…","seed":…}`. Tests and scripts read it, which makes port `0` usable. `replay` binds its listeners, and prints the line, only after the replay has reached `ended_at` (section 12.1).
- **Clock**: the standalone default is `wall`. If the config file has no `clock.mode` key, the binary sets `mode = "wall"` before parsing. `--clock` overrides both the file and that default. `replay` always runs on the virtual clock and takes its seed from the run file, so it has no `--clock` or `--seed` (section 12).
- **`--seed`** overrides the config's seed. **`--export-on-exit F`** writes the run export (section 12) when the process exits, whether by `POST /control/shutdown`, SIGINT (Ctrl-C) or SIGTERM. SIGTERM is handled on unix only; on Windows only Ctrl-C and `POST /control/shutdown` trigger the export.
- **`check`** parses and validates the config without starting anything. It prints `ok` and exits 0, or prints the error (with file line numbers where TOML gives them) to stderr and exits 1. Bad flags exit 2.
- The control API has no authentication and binds to loopback by default. The binary warns on stderr when `--control-addr` is not a loopback address (section 11).

## 4. Configuration

Configuration is a TOML file (or the equivalent Rust struct). All rates are per simulated hour unless stated. Every random choice draws from the seeded RNG.

```toml
seed = 42
api_version = "2026-11-18.endive"   # pinned; the only version the mock serves (section 7.1)
clock = { mode = "virtual", start = "2026-01-01T00:00:00Z", speed = 60.0 }  # mode: virtual (time moves only when advanced) | wall (real time × speed)

[accounts]
platforms = 1
connect = true
connected_accounts = { initial = 20, connect_rate = 0.5, deauthorize_rate = 0.1 }
account_api = { v1 = 0.5, v2 = 0.5 }                       # share of connected accounts created through each Accounts API
account_types = { standard = 0.5, express = 0.3, custom = 0.2 }   # v1 accounts
# v2 accounts get a mix of configurations (merchant, customer, recipient); see section 5.4

[history]
# Pre-generated data that exists before the sync starts.
# Some of it is older than the event retention window, so only backfill can see it.
# The default is no history (duration = "0s"); this example opts in.
duration = "90d"
scale = 1.0                          # multiplies every action and churn rate during history

[event_retention]
window = "30d"

# Business actions drive fan-out (see section 6).
[actions]
new_customer = 10.0
start_subscription = 4.0
cancel_subscription = 0.5
renew_subscription_cycle = "natural"   # driven by billing periods
one_off_payment = 6.0                  # a payment with no invoice
refund = 0.4                           # a full or partial refund of a random succeeded charge
dispute = 0.05                         # a dispute on a random succeeded, unrefunded charge
catalog_change = 0.2                   # products/prices created, archived
one_off_invoice = 2.0                  # a manual invoice, with a drawn outcome
step_delay = { distribution = "lognormal", median_ms = 200, p99_ms = 2000 }   # gap between an action's steps
payment_failure_probability = 0.1      # per payment attempt (invoice or one-off)

# Extra per-type churn on top of actions.
[churn.customer]
update = 3.0         # metadata, email, address
delete = 0.2
[churn.product]
update = 0.5
archive = 0.1
[churn.price]
update = 0.5
archive = 0.1
[churn.subscription]
update = 0.5         # metadata
[churn.invoice]
update = 0.5         # metadata
[churn.account]
update = 0.2         # business profile, email and metadata of a random connected account

[api]
keys = ["sk_test_mock"]   # accepted API keys (section 7.1)
latency = { distribution = "lognormal", median_ms = 40, p99_ms = 400 }   # real (wall) time in both clock modes
rate_limit = { requests_per_second = 25, burst = 25 }   # wall-time token bucket; off when absent
errors = { "500" = 0.001, "503" = 0.001, timeout = 0.0005, reset = 0.0005 }
stale_read = { probability = 0.0, max_lag = "2s" }   # retrieve/list may return an older version
pagination_writes = { probability = 0.0 }   # a list page after the first may follow a fresh mutation of the listed type

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
nested_list_page_size = 10           # force has_more on embedded lists

# Delivery targets. Configured here or via the control API, never via the Stripe API.
# Each target receives either snapshot or thin payloads, and has its own signing secret.
[[destinations]]
name = "snapshot"
url = "http://localhost:4000/webhooks/stripe"
payload = "snapshot"                 # v1 webhook endpoint semantics
enabled_events = ["*"]
connect = true                       # also receive events from connected accounts

[[destinations]]
name = "thin"
url = "http://localhost:4000/webhooks/stripe-thin"
payload = "thin"                     # v2 event destination semantics
enabled_events = ["v1.customer.*", "v1.invoice.*"]
events_from = ["@self", "@accounts"]   # @accounts: events from connected accounts
```

`[accounts]` defaults and rules:

- `platforms` must be `1`; any other value is rejected as not implemented.
- `connect` defaults to `false`. With `connect = false`, a connected-account count or rate above 0 (including `[churn.account] update`) is a config error, and any `Stripe-Account` or `Stripe-Context` header is rejected (section 7.1). The `connect` / `events_from` destination options are accepted, but there are no connected-account events to route.
- Defaults: `initial = 0`, `connect_rate = 0`, `deauthorize_rate = 0`, `account_api = { v1 = 0.5, v2 = 0.5 }`, `account_types = { standard = 0.5, express = 0.3, custom = 0.2 }`. Each mix must sum to 1 (±1e-9) with no negative weight.
- `[churn.account] update` (per hour, default 0) updates a random connected account.
- Initial accounts are connected at the start of history (`clock.start - history.duration`), so they get history too; their connect events fall inside history and are not delivered.

Destination rules:

- A snapshot destination receives events with no account label. With `connect = true` it also receives events labelled with a connected account (carrying `account`). `events_from` on a snapshot destination is a config error.
- A thin destination receives unlabelled events for `"@self"` and labelled events for `"@accounts"` (carrying `context`). `events_from` defaults to `["@self"]`. `connect` on a thin destination is a config error.
- `@organization_members…` values are rejected as not implemented (organizations are out of scope).

Limits (config files and the runtime `set_*_config` ops alike; a violation is a config error, 400 `config` on the control API): Every duration must be at most 100 years; `clock.speed` at most 1e6. That covers each duration string, each retry-schedule entry, the `ms` and `p99_ms` of every distribution, `api.stale_read.max_lag`, `history.duration` and `event_retention.window`. The error names the key and the limit. `POST /control/speed` applies the same speed cap.

`[actions]` payment rules:

- Every rate defaults to 0. The example above parses as is (a config test checks it).
- `one_off_payment` runs a payment with no invoice and no invoice payment: the payment method (the customer's card, or a new unattached card without a customer), a payment intent and one confirmation, with the events of section 6.1 from `payment_method.attached` to `payment_intent.succeeded` (or `charge.failed`, `payment_intent.payment_failed`). The customer is a random live one, or none with probability 0.2 (or when there is no live customer). The amount is drawn like a one-off invoice line: an active price with a non-zero unit amount (one-time prices first) times a quantity of 1–5, or a non-zero catalog amount in a random currency when there is no such price. The outcome fails with `payment_failure_probability`. Draws come from the stream `generate.one_off_payment`, the payment objects' from `generate.payments`.
- `refund` refunds a random charge with `status: succeeded` and something left to refund (none: nothing happens). It refunds everything left with probability 0.7, otherwise a uniform amount in `[1, left]`. One mutation each, in order: the refund's balance transaction (`type: refund`, `-amount`, no fee, `available` at once, no event), the refund (`succeeded`, `refund.created`), and the charge (`amount_refunded` increased, `refunded` once fully refunded, the refund first in the embedded `charge.refunds`; `charge.refunded`; no `charge.refund.updated`). Draws come from the stream `generate.refund`, the IDs from `generate.payments`. A charge with a chargeback (a dispute with `is_charge_refundable: false`) is not refundable; an inquiry's charge is.
- `dispute` disputes a random charge with `status: succeeded`, a non-zero amount, nothing refunded and no dispute yet (none: nothing happens), so a charge gets at most one dispute and a refunded charge none. Everything is drawn when the dispute opens: the `reason` (uniform over `fraudulent`, `product_not_received`, `duplicate`, `subscription_canceled`, `general`), the outcome (won 0.4, lost 0.5, `warning_closed` 0.1), the time to evidence (uniform 2–7 days) and the time from evidence to close (uniform 7–60 days), so a dispute closes 9–67 days after it opens. The timeline, all under the opening action's cause:
  1. Open. A chargeback (won or lost outcome): the withdrawal balance transaction (`type: adjustment`, `reporting_category: dispute`, `-amount`, fee 1500 (the dispute fee), `net` `-(amount + 1500)`, `available` at once with `available_on = created`, as Stripe's dispute adjustments are, no event), the dispute (`needs_response`, the withdrawal in `balance_transactions`, `is_charge_refundable: false`; `charge.dispute.created`, `charge.dispute.funds_withdrawn`), and the charge with `disputed: true` (no event). An inquiry (`warning_closed` outcome) moves no funds: the dispute is `warning_needs_response` with no balance transactions and `is_charge_refundable: true` (`charge.dispute.created` only).
  2. Evidence submitted: `under_review` (`warning_under_review` for an inquiry), `evidence_details.has_evidence`, `submission_count: 1` (`charge.dispute.updated`).
  3. Close: `won` adds the reinstating balance transaction (`reporting_category: dispute_reversal`, `+amount`, fee -1500, available at once) and announces `charge.dispute.closed`, `charge.dispute.funds_reinstated`; `lost` (funds stay withdrawn) and `warning_closed` announce `charge.dispute.closed`.

  Draws come from the stream `generate.dispute`, the IDs from `generate.payments` (the reinstatement's ID when the dispute closes).

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

Connected accounts themselves are covered in section 5.4.

Later phases add payments, Connect money movement and billing extras (section 5.5).

### 5.2 Lifecycle state machines

Each type has a state machine that only allows legal transitions, for example:

- `subscription.status`: `incomplete → active | incomplete_expired`, `active ⇄ past_due`, `active → canceled`, `trialing → active`, `active ⇄ paused`.
- `invoice.status`: `draft → open → paid | void | uncollectible`, `draft →` deleted.

Phase 3 generates `incomplete → active | incomplete_expired`, `active ⇄ past_due`, `active | past_due → canceled`; `trialing` and `paused` are not generated yet.

### 5.3 Value realism

- Integer amounts in the smallest currency unit, including zero-decimal currencies (JPY etc.) and very large amounts.
- Unix-second timestamps.
- Metadata with Unicode, maximum key/value lengths and empty strings.
- Nullable fields that really are null; optional fields that are absent.
- Bursts of many updates to one object within the same second.

### 5.4 Connect accounts

The mock models connected accounts created through **both** Accounts APIs, in the proportion set by `accounts.account_api`:

- **v1 accounts** (`/v1/accounts`): `type` standard, express or custom. Lifecycle events include `account.updated`, `account.application.authorized` and `account.application.deauthorized`, with thin `v1.` equivalents.
- **v2 accounts** (`/v2/core/accounts`): modelled with Accounts v2 configurations. Each of `merchant`, `customer` and `recipient` is applied with probability 0.5; an empty draw is rerolled to `["merchant"]`. They emit the thin `v2.core.account.created`, `.updated` and `.closed` events (platform scope, so no `context`, and no snapshot pair). They **also** emit the v1 events of the table below, including the v1 `account.updated` snapshot event, so a v1 consumer sees them too.
- Both kinds use `acct_` IDs and are addressed with `Stripe-Account` (v1 endpoints) or `Stripe-Context` (v1 and v2 endpoints); see section 7.1.
- **Both APIs read both kinds of account.** A v2-created account appears in `/v1/accounts` in the v1 shape with `type: "none"`; a v1-created account appears in `/v2/core/accounts` with `applied_configurations: ["merchant"]`. Sources: Stripe's Accounts v2 and Connect webhooks documentation (section 18).
- **Shapes follow the pinned schema** (generated types, section 14). Every v1 account carries a `controller`: just `type: "account"` for Standard (the schema leaves `is_controller` and `requirement_collection` out when the platform does not control the account), and `type: "application"` with `is_controller: true` and its fee, loss, requirement-collection and dashboard split for Express, Custom and v2 accounts. `external_accounts` (an empty list) appears only where `controller.is_controller` is true, so not on Standard accounts. `capabilities` lists `card_payments` and `transfers`, both active. The v2 view serves no `include`s, so the includable-only `configuration`, `defaults`, `identity`, `requirements` and `future_requirements` are absent; `applied_configurations` names the configurations.
- Accounts can be connected and disconnected during a run (`connect_rate`, `deauthorize_rate`, control API). The mock uses one "disconnect" for every account, although Stripe allows deauthorization only for Standard accounts.

| Lifecycle step | Events | Event account label |
|---|---|---|
| Connect, any account | `account.application.authorized` and thin `v1.account.application.authorized` | the account |
| Connect, v2 account | additionally thin `v2.core.account.created` | none |
| Update, any account | `account.updated` and thin `v1.account.updated` | the account |
| Update, v2 account | additionally thin `v2.core.account.updated` | none |
| Disconnect, any account | `account.application.deauthorized` and thin `v1.account.application.deauthorized` | the account |
| Disconnect, v2 account | additionally thin `v2.core.account.closed` | none |

The `data.object` of `account.application.*` is the platform application (`{"id": "ca_…", "object": "application", "name": …}`). The thin `related_object` of a `v1.`-prefixed or snapshot-paired account event is the account (`type: "account"`, `/v1/accounts/{id}`); that of a thin-only `v2.core.account.*` event is `{id, type: "v2.core.account", url: "/v2/core/accounts/{id}"}`, which still resolves after the account closes.

**Disconnect and closed semantics.**

- A disconnect is a delete in the store for both APIs; the v1 tombstone is `{"id", "object": "account", "deleted": true}`. `/v1/accounts` omits the account and `/v1/accounts/{id}` returns 404. `/v2/core/accounts/{id}` still renders the last live version with `closed: true`.
- `/v2/core/accounts` lists only open accounts by default. `closed=true` lists **only** closed accounts.
- A disconnected account's header is rejected with 403 `account_invalid` on every endpoint, so its events cannot be fetched through the Events APIs. No new events are created in its scope. Deliveries already queued for it still go out to connect destinations.
- A disconnected account's objects remain in the truth log and in subscriptions as they were.

**Event coverage is identical for platform and connected accounts.** Every action, entity type, lifecycle and event type the mock generates for a platform account is generated for each connected account too, with the same rates. Connected-account events reach snapshot destinations with `connect = true` (carrying `account`) and thin destinations with `@accounts` in `events_from` (carrying `context`). All faults apply to them in the same way.

### 5.5 Planned entity types (phases 7–9)

Coverage follows what a Stripe Connect integration needs, most-used first. Each type is there for the sync situation it adds. The full design is in `docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md`. ID prefixes are confirmed against Stripe's examples when each type is built.

**Phase 7: payments core.**

| Type | Removal style | Sync situation |
|---|---|---|
| `payment_intent` | `canceled` status | fast status churn |
| `charge` | none (`refunded`, `disputed` flags) | updated by its children |
| `refund` | `canceled` / `failed` status | appears standalone and embedded in `charge.refunds` |
| `dispute` | closed statuses | lives for weeks of simulated time, with many updates |
| `balance_transaction` | none | no events of its own: found only by listing; `pending → available` changes silently at `available_on` |
| `payment_method` | detach (`customer: null`, stays retrievable) | listing needs a customer |
| `setup_intent` | `canceled` status | creates payment methods |
| `checkout.session` | expires | dotted object name; creates a payment or a subscription |
| `invoice_payment` | none | links an invoice to its payments (`invoice.payments`) |

Paying an invoice creates, in one action: a payment intent, an invoice payment (`open` from the first attempt), a charge with its balance transaction, and the paid invoice. The customer's card payment method is created and attached first if it has none (`payment_method.attached`). Events: `payment_intent.created`, `charge.succeeded`, `payment_intent.succeeded`, `invoice_payment.paid`, `invoice.paid`, `invoice.payment_succeeded`. A failed attempt creates a failed charge instead. Finalizing stays a separate, earlier step. New actions: `one_off_payment`, `refund` and `dispute` (until now reserved keys), plus `save_payment_method`, `checkout` and `[churn.payment_method] detach`.

**Phase 8: Connect money movement.**

| Type | Removal style | Sync situation |
|---|---|---|
| `transfer` and `transfer_reversal` | reversal | reversals as an embedded list and sub-list |
| `application_fee` and `fee_refund` | refund | platform-scope objects that describe connected-account charges |
| `payout` | `canceled` / `failed` | own lifecycle; creates balance transactions; runs on connected accounts too |
| external accounts (`bank_account`, `card`) | hard delete | polymorphic sub-list of an account |
| `person` | hard delete | sub-list of an account; changes `account.requirements` |

Charge patterns under Connect:
- **Connected accounts keep direct charges.** Their own payments stay in their own scope, which makes them direct charges by construction. `accounts.application_fee = { probability, percent }` (default `{ probability = 0.0, percent = 10.0 }`) adds a platform `application_fee` to some of them.
- **The platform's own payments can route money to a connected account.** `accounts.platform_payments = { own, destination, separate }` (default `{ own = 1.0 }`) draws a pattern for each platform payment, and for a non-`own` pattern, a recipient account:
  - **destination:** `transfer_data.destination`, a `transfer`, and a `py_` payment in the account's scope.
  - **separate:** separate charges and transfers: a `transfer_group`, then an independent `transfer`.
  - With no connected account available, the payment falls back to `own`.
- With `connect = false`, non-default values of either key are config errors.

**Phase 9: billing extras.** `coupon` (hard delete), `promotion_code` (deactivate only, no delete event), `discount` (embedded only), `invoiceitem` (hard delete while pending), `credit_note` with `credit_note_line_item` (void), `tax_rate` (archive) and `subscription_schedule` (released, canceled or completed).

Every new type gets events for both payload formats, history, faults and the latest-state subscription. It also gets the same Connect coverage as platform types, except the scope-specific money movement above.

## 6. Action generation

### 6.1 Cause-driven fan-out

In Stripe, one action produces many events across objects. The generator models **business actions**, and each action expands into the realistic sequence of mutations. For example, `start_subscription`:

1. `customer.subscription.created`
2. `invoice.created` → `invoice.finalized`
3. `payment_method.attached` (the customer's first payment only), `payment_intent.created`, `charge.succeeded`, `payment_intent.succeeded`, `invoice_payment.paid`
4. `invoice.paid`, `invoice.payment_succeeded`
5. `customer.subscription.updated` (status `incomplete → active`)

Paying an invoice runs the payment sequence of section 5.5, one mutation per step, all in the same simulated instant: the customer's card (created and attached only if it has none, `payment_method.attached`), the payment intent (`requires_confirmation`, `payment_intent.created`) and, linked to it, the invoice's `open` invoice payment (no event), the charge's `pending` balance transaction (no event; fee 2.9% + 30 in the minor unit, `available_on` two days later), the charge (`charge.succeeded`), the intent `succeeded` (`payment_intent.succeeded`), the invoice payment `paid` (`invoice_payment.paid`), and the paid invoice with the payment in `invoice.payments` (`invoice.paid`, `invoice.payment_succeeded`). A failed attempt creates a `failed` charge (`card_declined`, `charge.failed`), returns the intent to `requires_payment_method` with its `payment_method` cleared and the card named in `last_payment_error` (`payment_intent.payment_failed`), then announces `invoice.payment_failed`, whose invoice lists the open invoice payment. A retry finds the intent through that open invoice payment and confirms it again with the customer's card and a new charge. Voiding the invoice or marking it uncollectible after a failure cancels the invoice payment (`canceled`, no event: the pinned API has no `invoice_payment.canceled`) and then the intent (`payment_intent.canceled`, `cancellation_reason: void_invoice`). A zero-amount invoice creates no payment objects: it is paid directly, with `invoice.payments` empty. At `available_on` the balance transaction becomes `available` without an event; this settlement clock runs even while generation is off. Payment draws come from the stream `generate.payments`; whether an attempt fails is still drawn from `generate.billing`.

Mutations inside one action are spaced over a short, configurable span (often the same second), which produces the cross-object races that matter. Per-type `churn` rates add independent updates and deletes on top.

### 6.2 Scripted scenarios

Alongside random generation, tests can script exact sequences against the virtual clock. A script is a list of control operations (`ControlOp`, section 11) applied in order: `MockStripe::run_script(&[ControlOp])` in-process, `POST /control/script` over HTTP. Each op is applied at the current simulated time; a script that needs to wait includes `step` or `advance_to` ops. An exported run (section 12) is itself a script: its journal is a list of `{at, op}` entries.

```json
[
  {"op": "create_customer", "customer": {"email": "a@example.com"}},
  {"op": "update_customer", "id": "cus_…", "update": {"type": "set_email", "value": "b@example.com"}}
]
```

`CustomerUpdate` is adjacently tagged: `{"type": "set_email", "value": …}`. A script stops at its first failing op and reports how many ops completed.

Projection-level script operations (delivering events out of order, making an event visible after a delay) are deferred. They need hooks into the delivery queue that are not built yet, and can be added later as new `ControlOp` variants.

Scripted scenarios are the regression suite for specific races; random runs look for new ones. A failing random run can be exported as a script (section 12).

### 6.3 Pre-existing history

Before the run starts, `history` generates data over a past period. Data older than the event retention window exists only through list endpoints, which forces the sync engine to backfill. Live generation continues during the backfill, so the race between backfill and live changes is always exercised.

History events are listed by the Events APIs while inside retention, but never delivered: destinations start at `clock.start`.

## 7. Stripe-compatible API surface

### 7.1 Requests and responses

- `Authorization: Bearer sk_test_…` (any key from config; unknown keys → 401).
- `Stripe-Account: acct_…` and `Stripe-Context: acct_…` select a connected account; objects, lists and events are scoped per account. Without a header the request is the platform's.
  - On `/v1/` endpoints both headers select the scope; on `/v2/` endpoints only `Stripe-Context` does, and `Stripe-Account` there gets a 400 (`Use the Stripe-Context header with v2 endpoints.`).
  - If both are present and differ: 400 (`Stripe-Account and Stripe-Context name different accounts.`). An empty header value is treated as absent.
  - An unknown or disconnected account: 403, `invalid_request_error`, code `account_invalid` (`The provided key does not have access to account '{acct}' (or that account does not exist). Application access may have been revoked.`).
  - The Accounts endpoints (`/v1/accounts`, `/v2/core/accounts`) are platform-only: a connected-account header gets 400 (`Accounts endpoints must be called by the platform.`). The delivery-target lists (`/v1/webhook_endpoints`, `/v2/core/event_destinations`) are platform-only too: a connected scope sees an empty list.
  - Event retrieve by ID outside the request's scope returns 404. With `connect = false`, any of these headers gets `Connected accounts are not enabled in this mock.`
  - Faults (rate limits, errors, stale reads, pagination writes) apply to connected scopes like the platform; a stale read or pagination write stays inside the request's scope.
- Stripe error envelope (`{"error": {"type", "code", "message", ...}}`), `Request-Id` header, `Stripe-Should-Retry` on retryable errors.
- **One API version**: `2026-11-18.endive`, the latest stable version when this spec was written. Requests with no `Stripe-Version` header are served at that version; a different `Stripe-Version` gets an error. All snapshot payloads use this version. Moving to a newer version is a deliberate change to the mock (update the OpenAPI spec it is generated from, section 14).
- **Read-only**: every `POST`, `PUT`, `PATCH` and `DELETE` returns a Stripe-style `invalid_request_error` explaining that the mock is read-only.

### 7.2 Endpoints (phase 1)

- Retrieve and list for every phase-1 type: `GET /v1/{type}/{id}`, `GET /v1/{type}`.
- Sub-lists for embedded collections: `GET /v1/subscription_items?subscription=…`, `GET /v1/invoices/{id}/lines`.
- Snapshot events: `GET /v1/events`, `GET /v1/events/{id}`.
- Thin events: `GET /v2/core/events`, `GET /v2/core/events/{id}` (section 8.2).
- Connect: `GET /v1/accounts`, `GET /v1/accounts/{id}`, `GET /v2/core/accounts`, `GET /v2/core/accounts/{id}`. Platform-only, and each API reads both kinds of account (section 5.4).
  - `/v1/accounts` lists connected accounts only; a disconnected account is absent and its retrieve is 404.
  - `/v2/core/accounts` lists open accounts by default; `closed=true` lists only closed (disconnected) accounts. Retrieve renders a closed account with `closed: true`.
- Read-only views of configured delivery targets: `GET /v1/webhook_endpoints` (snapshot targets) and `GET /v2/core/event_destinations` (thin targets), so the sync engine can check its setup.
- Phases 7–9 add retrieve and list for each of their types, plus their sub-lists (section 5.5). Some lists need a parent, the way `/v1/subscription_items` needs `subscription`: `/v1/payment_methods` needs `customer`, and is also served at `/v1/customers/{id}/payment_methods`.

### 7.3 List semantics

- Ordered by `created` descending; cursor pagination via `starting_after` / `ending_before`; `limit` 1–100, default 10; `has_more`.
- `created[gt|gte|lt|lte]` filters where Stripe supports them.
- Deleted objects are excluded; archived ones are included (with filters such as `active=`).
- Embedded lists (`subscription.items`, `invoice.lines`) truncated to `nested_list_page_size` with `has_more: true` and a `url` for the sub-list.
- Expandable fields return IDs unless `expand[]` asks for the object.
- Sub-lists list in creation order, ascending, so their first page equals the embedded list.

### 7.4 Consistency faults on the API

- **Stale reads** (optional): retrieve/list may briefly return an older version of an object.
- **Writes during pagination**: objects created, changed or deleted while a client pages through a list, so a backfill can see an object twice, miss a new one, or page past a deleted one.
- Rate limiting (429) with a token bucket, plus random 5xx, timeouts and connection resets per `[api]`.
- A `timeout` holds the response for 80 s (Stripe's own request limit) or until the mock shuts down, then drops the connection; a `reset` drops it at once. Either way the client gets no complete response.
- 429, 500 and 503 responses carry `Stripe-Should-Retry: true`, the usual error envelope and a `Request-Id`.
- Faults apply only to requests that pass authentication and the other request checks, in this order: rate limit (a forced storm, then the bucket), then a random error, then latency, then the handler.
- `api.pagination_writes`: when it fires on a list request that carries a cursor, one random action of the listed type runs before the page is served, so the race happens in virtual time too. It does nothing while generation is off (after quiesce).

## 8. Events API

Every truth-log mutation that Stripe would announce produces **one snapshot event and one thin event**. They share timing (`created`, visibility) and the thin event's `snapshot_event` points at the snapshot event's ID. Both formats are always generated; destinations choose which one they receive. The one exception is the thin-only `v2.core.account.*` events (section 5.4), which have no snapshot pair and no `snapshot_event`.

### 8.1 Snapshot events (v1)

- `evt_…` with `type` (e.g. `customer.updated`), `created`, `data.object` (snapshot at event time; for a `*.deleted` event, the last live object, unchanged, with no `deleted` flag), `data.previous_attributes` (for `*.updated`), `account` and `context` (Connect; both absent for the platform's own events), `api_version` (`2026-11-18.endive`), `livemode: false`, `request {id, idempotency_key}` and `pending_webhooks`.
- **Visibility scheduling**: each event gets a hidden `visible_at = created + visibility_lag (+ late_tail)`. `/v1/events` only returns events whose `visible_at` has passed. An event can therefore become visible *after* a newer one did, so "poll with `ending_before` = last seen" silently misses events. Catching this bug is a primary goal of the fixture.
- **Same-second ordering**: events sharing a `created` second have no guaranteed relative order (`same_second_shuffle`).
- **Retention**: events older than `event_retention.window` disappear from the API. A poller that falls further behind must detect the gap (e.g. its cursor event 404s) and fall back to a full backfill.
- `type` / `types[]` / `created` filters and `delivery_success`.

### 8.2 Thin events (v2)

Stripe made thin events for API v1 resources generally available in `2026-09-30.endive`. The mock implements them for every type it models.

- **Naming**: the snapshot type with a `v1.` prefix, e.g. `customer.created` → `v1.customer.created`.
- **Notification** (what a destination receives): `id`, `object: "v2.core.event"`, `type`, `created`, `livemode`, `context` (the connected account, for Connect; absent for the platform) and `related_object {id, type, url}`. `related_object.type` is the v1 object name (e.g. `customer`) and `url` is an API path such as `/v1/customers/cus_…`. There is no `reason`: the mock's API is read-only, so no request causes an event. The notification has **no** object data.
- **Full event**: `GET /v2/core/events/{id}` adds `snapshot_event` (the matching `evt_…`; absent for a thin-only `v2.core.account.*` event) and, for an update, `changes` (`before` and `after` of the changed top-level fields). `data` is absent: the mock's thin events carry no extra data. None of these fields is nullable in the pinned schema, so a field without a value is left out rather than `null`. Exact fields are generated from the OpenAPI spec (section 14).
- **List**: `GET /v2/core/events` with `types` filtering and v2-style pagination. It is subject to the same visibility lag, same-second shuffling and 30-day retention as snapshot events.
- **Why thin events matter for testing**: the consumer must fetch the related object, so it always reads the *current* state rather than a snapshot. That interacts with stale reads (section 7.4), deleted objects (the fetch returns the deleted stub or 404) and rate limits (every event costs an API call), all of which the mock can provoke.

## 9. Webhooks

- Delivery targets come from `[[destinations]]` in the config or from the control API. Each is either a **snapshot** target (v1 webhook endpoint semantics) or a **thin** target (v2 event destination semantics), with its own `enabled_events` and its own signing secret. Snapshot targets can be account or Connect endpoints; Connect events carry `account` (snapshot) or `context` (thin).
- All delivery behaviour below applies equally to both payload formats.
- **At-least-once delivery**: duplicates are a baseline behaviour (`duplicate_probability`), not only a fault.
- **Ordering**: deliveries inside `reorder_window` may be reordered; delivery delay comes from a distribution with a long tail.
- **Retries**: non-2xx, connection failure or a response slower than `response_timeout` counts as failure. Retries follow a Stripe-like exponential backoff up to `max_retry_duration`; in virtual time the schedule can be compressed. A slow-but-successful handler therefore receives a duplicate.
- **Disabling**: an endpoint that keeps failing is disabled, as Stripe does; the control API can re-enable it.
- **Signatures**: `Stripe-Signature: t=<ts>,v1=<hmac-sha256>`. Supports secret rotation (several `v1` values), timestamps outside tolerance (`clock_skew`), and an invalid-signature fault for negative tests. The signature timestamp `t` is real wall-clock time, so receivers can verify it; event `created` is simulated time.
- **Payload staleness**: because snapshot payloads are snapshots, a delayed or retried delivery carries an outdated object. Each delivery's truth-log version is recorded so tests can tell whether the consumer applied an old version over a newer one.
- **Endpoint downtime**: the control API can make deliveries to an endpoint fail for a period ("outage"), building a retry backlog that later floods in.

## 10. Latest-state subscription

Through the control API and the Rust handle, a test can subscribe to **the latest state of every object**. This is the mock's only verification-related feature; the verifier itself is out of scope (section 1).

### 10.1 Semantics

- On subscribe, the subscriber first receives the current state of every object (optionally filtered by object type and by account: `SubscribeFilter.accounts` lists scopes, `None` for the platform; an absent filter means every scope; snapshot and live updates are both filtered on the update's account), then a `synced` marker, then every subsequent change as it happens in the truth log.
- Every message carries the object's full state after the change and its per-object `version`.
- Deleted objects are sent as tombstones (`kind: "deleted"`, `state` = Stripe's deleted stub), so a subscriber can tell "deleted" from "never seen".
- A subscriber that falls too far behind is not allowed to slow the mock down. It is told it lagged and must resubscribe, which gives it a fresh snapshot. Because every message is a complete latest state, nothing is lost by skipping intermediate versions.
- The stream carries truth-log entries only, never projections.

### 10.2 Version contract for consumers

Per-object `version` starts at 1 and increases by one per mutation. A sync target may legitimately **skip** versions (e.g. it applies version 5 without ever seeing 3 or 4) but must never **regress** (go from 5 back to 4). The future verifier will enforce exactly this; the mock guarantees the versions needed to check it.

### 10.3 Message format

```jsonc
{
  "seq": 1042,                       // global, gap-free, monotonic
  "at": "2026-01-01T00:12:03.120Z",  // virtual time of the mutation
  "account": "acct_123",             // null for platform
  "object_type": "customer",
  "object_id": "cus_abc",
  "version": 3,                      // per-object, starts at 1
  "kind": "updated",                 // snapshot | created | updated | deleted | archived | state_changed
  "state": { /* full object after the mutation, API-shaped at the pinned version */ },
  "events": { "snapshot": ["evt_1"], "thin": ["evt_v2_1"] },   // events this mutation produced
  "cause": { "action": "start_subscription", "action_id": "act_77" }
}
```

`events.snapshot` and `events.thin` normally pair one-to-one; a thin-only `v2.core.account.*` event appears in `events.thin` with no snapshot counterpart. `account` is the object's scope, so an Account object's own truth entries have `account: null` (platform scope) while its v1 events carry the account's ID as their Connect label (section 5.4).

Messages in the initial snapshot have `kind: "snapshot"` (`"deleted"` for tombstones), keep the object's latest `seq`, `at` and `version`, and have empty `events` and no `cause`.

### 10.4 Transport

- In-process: `handle.subscribe(filter) -> impl Stream<Item = SubscriptionItem>`, where `SubscriptionItem` is `Update(StateUpdate) | Synced | Lagged`.
- Standalone: SSE (`GET /control/subscribe`) and WebSocket (`GET /control/subscribe/ws`) on the control port.

**Query parameters** (both transports; either may be absent, which means all):

- `types=customer,subscription_item`: comma-separated object types. The `object_type` names used in updates are accepted, and so are Stripe `object` names (`line_item` for `invoice_line_item`). Responses always use the `object_type` name: `GET /control/state` keys its objects by it, and `GET /control/objects/{type}/{id}` accepts either name in `{type}`.
- `account=platform,acct_…`: comma-separated scopes; `platform` means the platform scope.
- An unknown type, or an account that is unknown or disconnected, returns 400 (error envelope, section 11) before the stream starts.

**SSE framing** (`content-type: text/event-stream`):

- `event: update` with `data:` the `StateUpdate` JSON (section 10.3);
- `event: synced` with `data: {}`, once the initial snapshot is complete;
- `event: lagged` with `data: {}`, followed by the end of the stream; the consumer must resubscribe (section 10.1);
- a `: keep-alive` comment every 15 s of wall time.

**WebSocket framing**: one text frame per item: `{"type":"update","update":{…}}`, `{"type":"synced"}` or `{"type":"lagged"}`. After `lagged` the server sends a close frame. Messages from the client are ignored.

### 10.5 Delivery log (debugging)

Separately from the subscription, the control API exposes the **delivery log** (event became visible, delivery attempted, succeeded, failed, duplicated, reordered, abandoned after retries), so a failing test can be explained. It is a log to query, not part of the subscription.

## 11. Control API

Separate from the Stripe surface. Available as a Rust handle in-process and as HTTP under `/control` standalone.

- **Run control**: `pause`, `resume`, `step(duration)`, `advance_to(time)`, `set_speed`.
- **Quiesce**: stop generation, flush every pending webhook and retry, make every event visible, then return. Used before a final end-state comparison. Simulated time does not move; pending retries are attempted immediately until they succeed or exhaust their schedule. Generation stays off until `resume_generation`.
- **Triggers**: run a named action now; run a script.
- **Faults at runtime**: change any `[api]` (`update_api_config`), `[events]`, `[webhooks]` setting; start or stop an endpoint outage; force a rate-limit storm (`force_rate_limit`).
- **Delivery targets**: add, remove, enable or disable snapshot and thin destinations; rotate a signing secret.
- **Subscription**: the latest-state subscription (section 10).
- **Accounts**: connect a new v1 or v2 connected account, or disconnect one.
- **Inspection**: current truth state (by type/ID or full dump), delivery log with the reason for every decision, event visibility timeline, effective config.
- **Export**: snapshot state to JSON; export the run (seed + config + runtime changes + scripted steps) as a replayable script (section 12).

### 11.1 HTTP routes

All routes are under `/control` on the control port. Bodies are JSON. `{d}` is a duration string such as `"90s"`, and times are RFC 3339.

| Method and path | Body or query | Calls |
|---|---|---|
| `GET /now` | – | `{"now": time}` |
| `POST /step` | `{"duration": d}` | `step` |
| `POST /advance_to` | `{"time": t}` | `advance_to` |
| `POST /pause`, `POST /resume` | – | `pause` / `resume` |
| `POST /speed` | `{"speed": f64}` | `set_speed` |
| `POST /quiesce`, `POST /generation/resume`, `GET /generation` | – | `quiesce` / `resume_generation` / `{"enabled": bool}` |
| `POST /ops` | one `ControlOp` | `apply` → `OpResult` |
| `POST /script` | `{"steps": [ControlOp]}` | `run_script` → `[OpResult]`; stops at the first error (409 or 404, with `{"error", "completed": n}`) |
| `GET /config` | – | the effective config: the start config with `[api]`, `[events]`, `[webhooks]`, `[actions]` and `destinations` at their current runtime values |
| `POST /rate_limit` | `{"duration": d}` | `force_rate_limit` |
| `GET /destinations` | – | `destinations` |
| `GET /accounts` | – | `accounts` |
| `GET /objects/{type}/{id}` | `?account=` | one object; `null` → 404 |
| `GET /state` | `?account=&types=` | full dump of live objects |
| `GET /truth_log` | `?after_seq=&limit=` (default 1000, max 10000) | slice of the truth log, with `has_more` |
| `GET /events` | `?after=<evt id>&limit=` | slice of the event timeline |
| `GET /deliveries` | `?limit=` | the last `limit` entries of the delivery log |
| `GET /export` | – | the run export (section 12) |
| `GET /subscribe`, `GET /subscribe/ws` | `?types=&account=` | SSE / WebSocket (section 10.4) |
| `POST /shutdown` | – | 202, then a graceful shutdown of the whole server |

Mutating calls other than `step`, `advance_to`, `quiesce`, pause, resume and speed go through `POST /control/ops`. That gives one route, one schema and the same shape as scripts and the journal. `ControlOp` covers customer create, update and delete (with an optional `account`), `run_action`, connect and disconnect account, config updates (`set_api_config`, `set_events_config`, `set_webhooks_config`, `set_actions_config`), and destination add, remove, enable, outage start and end, and secret rotation.

`run_action` takes an `Action`, tagged by `type` in snake case: `new_customer`, `catalog_change`, `archive {id}`, `one_off_invoice {customer, outcome}`, `start_subscription {customer, prices}`, `cancel_subscription {id, at_period_end}`, `one_off_payment {customer, amount, succeed}`, `refund {charge, amount}` and `dispute {charge, outcome}` (`outcome`: `won`, `lost` or `warning_closed`). Fields left out or `null` are drawn as the generated action draws them, for example `{"op": "run_action", "action": {"type": "refund", "charge": "ch_…", "amount": 500}}`. `refund` with a named charge that has not succeeded or has less than `amount` (or nothing) left to refund is a 409 `invalid_action` naming the charge, and so is a `refund` with no charge when no charge is refundable; a `one_off_payment` `amount` below 1 is a 409 `invalid_action` too. So is a `dispute` on a named charge that cannot be disputed (not succeeded, refunded or already disputed; the message names it), or with no charge when none is disputable.

**Errors** use one JSON envelope: `{"error": {"code": "<snake_case error name>", "message": "<text>"}}`. Status by error:

- `no_such_object`, `no_such_destination`, `unknown_account` → 404;
- `clock_mode`, `time_travel`, `deleted`, `invalid_action`, `connect_disabled` → 409;
- `config`, and malformed bodies, queries or JSON → 400;
- `forbidden` (cross-origin, see Security) → 403;
- `not_found` (no such route) → 404, `method_not_allowed` → 405;
- `unsupported_media_type` (a `POST` without a JSON content type) → 415;
- anything else → 500.

**Shutdown**: `POST /control/shutdown` answers 202, then stops the control server and the API server. Shutdown is bounded: each server gets 2 s to finish in-flight requests, then is aborted. The process then exits 0, after writing the `--export-on-exit` file when one was given.

**Scripts are not atomic**: other control calls and generated actions (wall mode) can run between a script's ops. Each op is journalled when it applies, so the truth stays replayable.

**Security**: the control API has no authentication. It binds to loopback by default and is meant for test and development machines only. Never expose it on a shared network. So that web pages in a browser on the same machine cannot drive it, every control request, including the WebSocket upgrade, is checked:

- (a) the `Host` header must be a loopback name (`localhost`, `127.0.0.1` or `[::1]`, any port) or the host of the address the control API is bound to; otherwise 403 `forbidden`. This defeats DNS rebinding. When `--control-addr` is a non-loopback IP (the binary warns), clients on other machines reach it by that IP; a wildcard bind such as `0.0.0.0` only admits loopback names, so bind the specific interface IP for remote use.
- (b) a request with an `Origin` header must come from `http://localhost`, `http://127.0.0.1` or `http://[::1]` (any port); otherwise 403 `forbidden`.
- (c) every `POST`, even one without a body (`pause`, `resume`, `quiesce`, `generation/resume`, `shutdown`, …), must have `Content-Type: application/json` (parameters such as `charset` allowed); otherwise 415 `unsupported_media_type`. A browser cannot send that cross-origin without a CORS preflight, which the control API never grants.

## 12. Determinism and time

- One seeded RNG drives all generation and fault decisions. Separate RNG streams per subsystem (generation, API faults, event visibility, webhooks) so changing one setting doesn't reshuffle unrelated decisions.
  - Platform streams keep their names and draw order: a run with `connect = false` is identical to one before Connect existed.
  - Connect streams: `accounts.connect` (connect clock), `accounts.deauthorize` (disconnect clock and pick), `accounts.content` (API, type, configurations and fields of a new account), `accounts.update` (`[churn.account]` clock and content), `accounts.ids` (account IDs), `platform.application` (the `ca_` ID, drawn once at start and only with `connect = true`) and `api.pagination_writes.accounts` (pagination writes on the Accounts list).
  - Each connected account's generator uses every platform stream name prefixed `account/{acct}/`.
  - Ties at the same instant run in this order: platform, the accounts generator, then connected accounts in connection order.
- **Virtual clock** by default in-process: time only moves when the test advances it, or at a fixed multiple of wall time. Retry schedules, visibility lags and history all run on virtual time, so a "3 days of retries" scenario finishes in milliseconds.
- In wall-clock mode the HTTP layer still uses real latencies; such runs are not exactly reproducible, but the seed still governs generation.
- **Replay**: any run can be reproduced from its exported seed, config and runtime changes. Test failures print the seed and the replay command (see `ReplayGuard` below).

### 12.1 Run export and replay

`GET /control/export` (in-process: `MockStripe::export_run`) returns the run as JSON:

```json
{
  "format": "ductkit-stripe-mock/run@1",
  "seed": 11,
  "config": { "...": "..." },
  "ended_at": "2026-01-01T04:00:00.000Z",
  "journal": [{ "at": "2026-01-01T02:00:00.000Z", "op": { "op": "connect_account", "api": "v2" } }]
}
```

- `config` is the config the run **started** with; runtime changes are in the journal.
- **Journalled** (every op that changes truth, the projection or delivery): customer create, update and delete, `run_action`, connect and disconnect account, each config update (recorded as the full resulting section, because closures cannot be serialized), destination add, remove, enable, outage start and end and secret rotation, `quiesce` and `resume_generation`, `step` and `advance_to` (recorded as the time they reached), and pagination writes fired by API requests.
- **Not journalled**: `pause`, `resume` and `set_speed`, which only shape wall time (ops carry their own `at`); `force_rate_limit` and other API-only faults, which never touch truth; every read. A failed control call is not journalled and does not use up an action number.
- In wall mode every journalled control call first ticks the clock, so everything due by the op's `at` has already run.

**Replay** (`MockStripe::replay(export, options)`, or `ductkit-stripe-mock replay --run F`) always runs on the virtual clock. Before applying each op it advances time to the op's `at`, running every action and delivery due at or before it, then applies the op. After the last op it advances to `ended_at` and stops there. Replay rejects an unknown `format` with a config error. `MockStripe::replay_with` and the binary bind their listeners only after that, so no client can inject ops or pagination writes into a replay in progress; until then connections are refused.

- **Guarantee**: replay reproduces the **truth log** and the **event timeline** of the original. It does not guarantee the delivery log: destinations are kept, so deliveries are attempted again against the same URLs, and real HTTP outcomes can differ. `--drop-destinations` (`ReplayOptions { drop_destinations: true }`) removes them for offline replays. It also keeps a replay from blocking on destination URLs that are no longer reachable: a journalled `quiesce` waits for every pending delivery, and a re-run against a dead URL waits out each attempt's `response_timeout` and the retries.
- A wall-mode run replays exactly up to the tick boundary.
- **`ReplayGuard`**: `MockStripe::replay_guard(dir)` returns a guard that, when dropped during a panic, writes `<dir>/run-<seed>-<ended_at ms>.json` and prints to stderr `stripe-mock: run exported to <path>; replay with: ductkit-stripe-mock replay --run <path>`. Tests hold one so that a failure leaves a replayable file behind.

## 13. Future verifier (out of scope)

The verifier will be built with the sync engine, not in this crate. These notes record what it is expected to check, so the mock keeps providing what it needs:

- **Eventual convergence**: every object eventually reaches its latest truth version in the target (exactly, after quiesce).
- **No regression**: versions may be skipped, but a row never goes back to a lower version (section 10.2).
- **No resurrection**: a hard-deleted object never reappears.
- **Convergence latency**: time from a truth mutation to the target reflecting it.
- **Referential integrity**: whatever orphan policy the sync engine chooses.

The mock supports all of these through the latest-state subscription, per-object versions, tombstones and quiesce.

## 14. Fidelity

- **Generated types.** Every object the mock serves is built from Rust types generated from Stripe's published **OpenAPI spec** for the pinned version. That covers REST responses, embedded and expanded objects, event payloads, thin events, delivery-target views and error bodies. A misnamed, mistyped or wrongly nullable field is a compile error, not a drifted response. This matters because sync users map individual fields to database columns.
  - **The vendored spec** is `crates/stripe-mock/openapi/spec3.json`: `stripe/openapi` path `latest/openapi.spec3.json` at commit `0f6d4dfafe157c10df393ce8a27d00b186ab3955` ("Update OpenAPI for 2026-11-18.endive"). It covers v1 and v2. Its provenance, size and SHA-256 are in `openapi/README.md`, and a test pins them.
  - **The generated code** is committed under `crates/stripe-mock/src/stripe/generated/` and never edited by hand. Only an allowlist of root schemas, plus what they reference without expansion, is generated. An expandable reference to a type the mock doesn't model is generated as ID-only.
  - **The generator** lives in the crate's test tree. Its test regenerates the code in memory and fails if the committed files differ. Regenerate with `DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen`.
  - **The pinned schema wins.** Where it disagrees with behavior this spec documents, the mock follows the schema, and this spec is corrected.
  - **Field presence.** The spec's `required` lists don't say which fields Stripe always returns. For example, `customer` lists only `created`, `id`, `livemode` and `object`. Generated structs therefore have no defaults, and the model code decides explicitly, for every field, whether it is present, `null` or absent. A test compares each type's served fields with the schema's, against a reviewed list of deliberate exclusions.
- **OpenAPI conformance is part of the regular test suite.** It runs on every full test run (`cargo test --workspace`), locally or in CI, with no extra command, flag or credentials, and offline. Three checks:
  - the freshness test above;
  - a round-trip check that deserializes every payload the tests receive into its generated type, with unknown fields denied;
  - a coverage test that runs a busy Connect run and checks everything it serves.

  Updating the pinned version means replacing the vendored file and regenerating.
- Stripe's official `stripe-mock` is stateless and doesn't fit this purpose, but its OpenAPI-based fixtures can seed realistic example values.
- **Calibration** (deferred; not scheduled in any phase): periodically run equivalent scripted actions against real Stripe test mode and compare event fan-out, ordering and latency distributions with the mock's defaults. When it is built, calibration is a manual or scheduled job, not part of the regular test suite.
- One pinned API version (`2026-11-18.endive`). The OpenAPI spec for that version, covering both v1 and v2 endpoints, is the source for generated types. Multi-version payload rendering is not planned.

## 15. Performance

- Must sustain load tests: many accounts, thousands of mutations per simulated minute, large histories (hundreds of thousands of objects) for backfill testing.
- Storage is in memory by default, with an optional on-disk snapshot so large histories don't regenerate on every run.
- Subscribers must not be able to slow down generation; slow subscribers get bounded buffers and are told to resubscribe on overflow (section 10.1).

## 16. Phasing

1. **Engine skeleton**: clock, RNG, truth log, versions, latest-state subscription, control handle; `customer` only; read-only retrieve/list; in-process mode.
2. **Events and delivery**: snapshot and thin event projection, `/v1/events` and `/v2/core/events`, visibility lag, retention, signatures, retries, duplicates, reordering; snapshot and thin destinations; delivery log.
3. **Phase-1 types and fan-out**: product, price, subscription, items, invoice, lines; business actions; nested list truncation; deletion styles.
4. **History and backfill races**; API faults (rate limits, 5xx, stale reads, writes during pagination).
5. **Connect**: v1 and v2 connected accounts, `Stripe-Account`, connect/disconnect, full event coverage for connected accounts through snapshot and thin destinations.
6. **Standalone server binary**, SSE/WebSocket subscription, HTTP control API, run export and replay.
7. **Generated types and payments core**: the vendored OpenAPI spec, generated Rust types for every served object, and conformance checks in the regular test suite (section 14). Plus payment intents, charges, refunds, disputes, balance transactions, payment methods, setup intents, Checkout sessions and invoice payments, with invoice payments creating real payment objects (section 5.5).
8. **Connect money movement**: transfers and reversals, application fees and fee refunds, payouts, external accounts and persons, and the Connect charge patterns (section 5.5).
9. **Billing extras**: coupons, promotion codes, discounts, invoice items, credit notes, tax rates and subscription schedules (section 5.5).

Calibration against real Stripe test mode (section 14) is deferred to a later, unscheduled phase.

## 17. Decisions

| # | Decision | Where it applies |
|---|----------|------------------|
| 1 | Pin the latest stable Stripe API version, `2026-11-18.endive`. | 7.1, 14 |
| 2 | The mock accepts no writes from the sync engine. The Stripe API surface is read-only; delivery targets are configured via config or the control API. | 1, 7.1, 7.2, 9 |
| 3 | Version skips are allowed; regressions are forbidden. | 10.2, 13 |
| 4 | Thin events are in scope, alongside snapshot events. | 8.2, 9 |
| 5 | The verifier is out of scope. The mock provides a subscription to the latest state of every object via the control API and Rust bindings. | 1, 10, 13 |
| 6 | Connected accounts are modelled for both Accounts v1 and Accounts v2. | 4, 5.4, 7.2 |
| 7 | Connected accounts get the same event coverage as platform accounts, through both snapshot and thin destinations. | 5.4, 9 |
| 8 | Packaged as one crate with a library (imported by tests) and a binary (standalone server). No container image. | 3.1 |
| 9 | Object shapes come from Rust types generated from the vendored OpenAPI spec. The generated code is committed and kept fresh by a test. The types live in the mock crate and are not shared with the sync engine. | 3, 14 |
| 10 | Where the pinned OpenAPI schema disagrees with documented behavior, the schema wins and the spec is corrected. | 14 |
| 11 | Entity coverage follows what a Stripe Connect integration needs, delivered over phases 7–9. | 5.5, 16 |
| 12 | Under Connect, connected accounts keep direct charges. Destination charges and separate charges and transfers come from the platform's own payments. | 5.5 |
| 13 | Calibration against real Stripe test mode is deferred to an unscheduled phase. | 14, 16 |

## 18. Open questions

- **v1/v2 account interoperability** (answered): both APIs read both kinds of account, and v2 accounts also emit v1 `account.updated` and `account.application.*` events alongside the thin `v2.core.account.*` events. Taken from Stripe's Accounts v2 and Connect webhooks documentation (docs.stripe.com); details in section 5.4.
- **v2 list details** (pagination parameters, filters beyond `types`): to be taken from the OpenAPI spec for the pinned version rather than decided here.
