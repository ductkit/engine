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
application_fee = { probability = 0.1, percent = 10.0 }   # share of connected accounts' payments that pay the platform a fee (needs connect = true)
platform_payments = { own = 0.6, destination = 0.2, separate = 0.2 }   # Connect charge patterns of the platform's own payments (needs connect = true)
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
application_fee_refund = 0.05          # a full or partial refund of a random application fee (platform scope)
transfer_reversal = 0.05               # a full or partial reversal of a random transfer (platform scope)
dispute = 0.05                         # a dispute on a random succeeded, unrefunded charge
save_payment_method = 1.0              # a setup intent that saves a new card for a random customer
checkout = 2.0                         # a Checkout session, completed or left to expire
catalog_change = 0.2                   # products/prices created, archived
billing_catalog_change = 0.2           # coupons, promotion codes and tax rates created, deactivated, archived; coupons deleted
invoice_item = 0.5                     # a pending invoice item, which joins the customer's next invoice
discount_change = 0.3                  # a discount added to a random customer or subscription, or one removed
credit_note = 0.2                      # a credit note on a random open or paid invoice, or a pre-payment note voided
schedule_subscription = 0.2            # a subscription managed by a schedule of 2-3 phases
one_off_invoice = 2.0                  # a manual invoice, with a drawn outcome
step_delay = { distribution = "lognormal", median_ms = 200, p99_ms = 2000 }   # gap between an action's steps
payment_failure_probability = 0.1      # per payment attempt (invoice or one-off)
payout = "daily"                       # automatic payouts of each scope's available balance at midnight UTC (section 5.5); default "off"
payout_failure_probability = 0.01      # per payout, drawn when it is created

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
[churn.coupon]
update = 0.2         # metadata.note of a random live coupon
[churn.invoiceitem]
delete = 0.1         # a random pending invoice item is deleted
[churn.account]
update = 0.2         # business profile, email and metadata of a random connected account
[churn.payment_method]
detach = 0.1         # a random attached payment method is detached
[churn.person]       # persons of controlled connected accounts (needs connect = true)
update = 0.2         # a new email and phone
delete = 0.05        # a person is deleted
[churn.external_account]   # external accounts of controlled connected accounts (needs connect = true)
create = 0.2         # a debit card or bank account is added
delete = 0.1         # a non-default external account is deleted

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
- `application_fee = { probability, percent }` (defaults `0.0` and `10.0`): `probability` is the chance that a payment intent created in a connected account's scope carries a platform application fee, and `percent` the fee as a share of its amount, in `(0, 100]` (anything else is a config error naming the key). With `connect = false`, a `probability` above 0 is a config error naming the key. The draw comes from the stream `generate.application_fee`, and there is none at probability 0 or in the platform's own scope, so the platform's payments never carry a fee. The fee is `round_half_up(amount * percent / 100)`, and a fee of 0 means none.
- `platform_payments = { own, destination, separate }` (default `{ own = 1.0, destination = 0.0, separate = 0.0 }`): the share of the platform's own payment intents made with each Connect charge pattern (section 5.5). The mix must sum to 1 (±1e-9) with no negative weight; keys left out of a given mix are 0, so `{ destination = 1.0 }` makes every platform payment a destination charge. With `connect = false`, any other mix than the default is a config error naming the key. The pattern and recipient draws come from the stream `generate.platform_payments`, and there are none in a connected account's scope or when the mix is all `own`.
- `[churn.person]` (`update`, `delete`) and `[churn.external_account]` (`create`, `delete`) (per hour, default 0) act on each controlled connected account (section 5.4). With `connect = false`, any of them above 0 is a config error naming the key. In a Standard account, or the platform's own scope, they do nothing.
- Initial accounts are connected at the start of history (`clock.start - history.duration`), so they get history too; their connect events fall inside history and are not delivered.

Destination rules:

- A snapshot destination receives events with no account label. With `connect = true` it also receives events labelled with a connected account (carrying `account` and `context`). `events_from` on a snapshot destination is a config error.
- A thin destination receives unlabelled events for `"@self"` and labelled events for `"@accounts"` (carrying `context`). `events_from` defaults to `["@self"]`. `connect` on a thin destination is a config error.
- `@organization_members…` values are rejected as not implemented (organizations are out of scope).

Limits (config files and the runtime `set_*_config` ops alike; a violation is a config error, 400 `config` on the control API): Every duration must be at most 100 years; `clock.speed` at most 1e6. That covers each duration string, each retry-schedule entry, the `ms` and `p99_ms` of every distribution, `api.stale_read.max_lag`, `history.duration` and `event_retention.window`. The error names the key and the limit. `POST /control/speed` applies the same speed cap.

`[actions]` payment rules:

- Every rate defaults to 0. The example above parses as is (a config test checks it).
- `one_off_payment` runs a payment with no invoice and no invoice payment: the payment method (the customer's card, or a new unattached card without a customer), a payment intent and one confirmation, with the events of section 6.1 from `payment_method.attached` to `payment_intent.succeeded` (or `charge.failed`, `payment_intent.payment_failed`). The customer is a random live one, or none with probability 0.2 (or when there is no live customer). The amount is drawn like a one-off invoice line: an active price with a non-zero unit amount (one-time prices first) times a quantity of 1–5, or a non-zero catalog amount in a random currency when there is no such price. No generated charge exceeds Stripe's per-charge maximum of 99,999,999 in the minor unit: the drawn quantity is lowered until it fits, and the same cap applies, line by line in order, to the quantities of a one-off invoice's lines and of a new subscription's (or subscription-mode Checkout session's) items, a line that no longer fits getting quantity 0. Capping draws nothing. A `one_off_payment` control action with an `amount` above the maximum is rejected. The outcome fails with `payment_failure_probability`. Draws come from the stream `generate.one_off_payment`, the payment objects' from `generate.payments`.
- `application_fee_refund` (per hour, default 0, stream `generate.application_fee_refund`) refunds a random application fee with something left (none: nothing happens), in the platform scope only: a connected account's generator does nothing. It refunds everything left with probability 0.7, otherwise a uniform amount in `[1, left]`. Under one cause: the platform's balance transaction (`type: application_fee_refund`, `-amount`, no fee, `available` at once), the `fee_refund` (no event), the fee (`amount_refunded` increased, `refunded` once fully refunded, the refund first in the embedded `refunds`, newest first and cut to `nested_list_page_size`; `application_fee.refunded`), then, in the account's scope, a balance transaction (`type: application_fee_refund`, `+amount`, `available` at once, no event). The IDs come from `generate.payments`. A disconnected account's fees are not refundable.
- `transfer_reversal` (per hour, default 0, stream `generate.transfer_reversal`) reverses a random transfer with something left whose destination is still connected (none: nothing happens), in the platform scope only: a connected account's generator does nothing. It reverses everything left with probability 0.7, otherwise a uniform amount in `[1, left]`. Under one cause: the platform's balance transaction (`type: transfer_refund`, `+amount`, no fee, `available` at once), the `transfer_reversal` (`transfer`, `balance_transaction`, `destination_payment_refund` the `pyr_` refund of the `py_` payment, `source_refund: null`; no event), the transfer (`amount_reversed` increased, `reversed` once fully reversed, the reversal first in the embedded `reversals`, newest first and cut to `nested_list_page_size`; `transfer.reversed`), then, in the destination's scope and with no events, the `py_` payment's refund (`pyr_`, `succeeded`, `charge` the payment, `source_transfer_reversal` the reversal) with its balance transaction (`type: payment_refund`, `-amount`, no fee, `available` at once), and the payment's `amount_refunded` and `refunded`. The IDs come from `generate.payments`. `transfer.reversals` is a required list the transfer always carries, so it is not an `expand` path. A transfer to a disconnected account is not reversible.
- `refund` refunds a random charge with `status: succeeded` and something left to refund, other than a `py_` payment (none: nothing happens). A `py_` payment's money comes back only through a reversal of its transfer (`transfer_reversal`). It refunds everything left with probability 0.7, otherwise a uniform amount in `[1, left]`. One mutation each, in order: the refund's balance transaction (`type: refund`, `-amount`, no fee, `available` at once, no event), the refund (`succeeded`, `refund.created`), and the charge (`amount_refunded` increased, `refunded` once fully refunded; `charge.refunded`; no `charge.refund.updated`). Draws come from the stream `generate.refund`, the IDs from `generate.payments`. A charge with a chargeback (a dispute with `is_charge_refundable: false`) is not refundable; an inquiry's charge is.
- `dispute` disputes a random charge with `status: succeeded`, a non-zero amount, nothing refunded and no dispute yet, other than a `py_` payment (paid from the platform's balance, not by a card; none: nothing happens), so a charge gets at most one dispute and a refunded charge none. Everything is drawn when the dispute opens: the `reason` (uniform over `fraudulent`, `product_not_received`, `duplicate`, `subscription_canceled`, `general`), the outcome (won 0.4, lost 0.5, `warning_closed` 0.1), the time to evidence (uniform 2–7 days) and the time from evidence to close (uniform 7–60 days), so a dispute closes 9–67 days after it opens. The timeline, all under the opening action's cause:
  1. Open. A chargeback (won or lost outcome): the withdrawal balance transaction (`type: adjustment`, `reporting_category: dispute`, `-amount`, fee 1500 (the dispute fee), `net` `-(amount + 1500)`, `available` at once with `available_on = created`, as Stripe's dispute adjustments are, no event), the dispute (`needs_response`, the withdrawal in `balance_transactions`, `is_charge_refundable: false`; `charge.dispute.created`, `charge.dispute.funds_withdrawn`), and the charge with `disputed: true` (no event). An inquiry (`warning_closed` outcome) moves no funds: the dispute is `warning_needs_response` with no balance transactions and `is_charge_refundable: true` (`charge.dispute.created` only).
  2. Evidence submitted: `under_review` (`warning_under_review` for an inquiry), `evidence_details.has_evidence`, `submission_count: 1` (`charge.dispute.updated`).
  3. Close: `won` adds the reinstating balance transaction (`reporting_category: dispute_reversal`, `+amount`, fee -1500, available at once) and announces `charge.dispute.closed`, `charge.dispute.funds_reinstated`, and sets `is_charge_refundable: true`, so what is left of the charge can be refunded again; `lost` (funds stay withdrawn) and `warning_closed` announce `charge.dispute.closed`.

  Draws come from the stream `generate.dispute`, the IDs from `generate.payments` (the reinstatement's ID when the dispute closes).
- `save_payment_method` saves a card for a random live customer (none: nothing happens) through a setup intent, in one instant: the setup intent (`requires_payment_method`, `usage: off_session`; `setup_intent.created`), the card payment method attached to the customer (`payment_method.attached`; brand drawn from `visa`, `mastercard` and `amex`, `last4` four random digits, expiry 1–5 years ahead of the clock), and the setup intent `succeeded` with `payment_method` set (`setup_intent.succeeded`). With `payment_failure_probability` the card is declined instead: no payment method is created, and the setup intent stays `requires_payment_method` with `last_setup_error` (`card_declined`; `setup_intent.setup_failed`). A later payment of that customer uses its newest attached card. Draws come from the stream `generate.save_payment_method`, the objects' from `generate.payments`.
- `[churn.payment_method] detach` (per hour, default 0) detaches a random attached payment method (none: nothing happens): `customer` becomes `null` (`payment_method.detached`). It stays retrievable, leaves `GET /v1/payment_methods?customer=…` and `GET /v1/customers/{id}/payment_methods`, and no later payment uses it (a customer left with no card gets a new one at its next payment). Draws come from the stream `generate.payment_method.detach`. Deleting a customer detaches each of its attached payment methods the same way first, in the deletion's cause, because the pinned schema's `payment_method.customer` cannot name a deleted customer.
- `checkout` opens a hosted card Checkout session for a random live customer (none: nothing happens): `status: open`, `payment_status: unpaid`, `expires_at` = `created` + 24 h, no event. Everything is drawn when it opens: whether it completes (probability 0.7), the mode (`subscription` with probability 0.3, else `payment`; `subscription` needs an active recurring price and otherwise falls back to `payment`), the amount (payment mode, drawn like a one-off payment's) or the subscription's prices and quantities (as `start_subscription` draws them; `amount_total` is their first invoice's total), and the time to completion (uniform 1–30 minutes). Under the opening action's cause, either:
  1. Completion. Payment mode runs a succeeding payment for `amount_total` (the payment sequence of `one_off_payment`, from `payment_intent.created` to `payment_intent.succeeded`) and links `payment_intent`. Subscription mode starts the subscription in the same instant (`customer.subscription.created`, the first invoice created, finalized and paid with its payment objects, the subscription `active`, its renewal queued) and links `subscription` and `invoice`. Then the session is `complete`, `payment_status: paid` (`no_payment_required` for a zero total), with `customer_details` from the customer and `url: null` (`checkout.session.completed`). If the customer was deleted meanwhile, nothing is bought and the session waits to expire.
  2. Expiry, at `expires_at`: `status: expired`, `url: null` (`checkout.session.expired`).

  `GET /v1/checkout/sessions` filters by `customer`, `payment_intent`, `subscription` and `status`. Draws come from the stream `generate.checkout`, the session's ID from `generate.payments`.

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

Phase 7 adds the payments core (behavior in sections 4, 5.5 and 6.1):

| Type | ID prefix | Delete style | Notable relations |
|------|-----------|--------------|-------------------|
| `payment_intent` | `pi_` | State: `canceled` (an invoice voided after a failed attempt) | customer, payment_method, latest_charge |
| `charge` | `ch_` | None; `refunded` and `disputed` flags | payment_intent, customer, balance_transaction; includable list `refunds` (`expand[]=refunds`) |
| `refund` | `re_` | None (refunds succeed at once) | charge, payment_intent, balance_transaction; listed in `charge.refunds` when expanded |
| `dispute` | `du_` | Closed statuses: `won`, `lost`, `warning_closed` | charge, payment_intent, balance_transactions |
| `balance_transaction` | `txn_` | None; `pending → available` at `available_on`, no event | source (charge, refund or dispute) |
| `payment_method` | `pm_` | Detach (`customer: null`, stays retrievable, leaves the customer's lists) | customer |
| `setup_intent` | `seti_` | None (`succeeded`, or `requires_payment_method` after a decline) | customer, payment_method |
| `checkout.session` | `cs_test_` | State: `expired` | customer; payment_intent, or subscription and invoice |
| `invoice_payment` | `inpay_` | State: `canceled` (its invoice voided) | invoice, payment.payment_intent; listed in `invoice.payments` when expanded |

Phase 8 adds Connect money movement (behavior in sections 4, 5.4, 5.5 and 6.1; prefixes follow ruling P3, decision 16 in section 17):

| Type | ID prefix | Delete style | Notable relations |
|------|-----------|--------------|-------------------|
| `transfer` | `tr_` | None; `amount_reversed` and `reversed` once fully reversed | destination (account), destination_payment (`py_`, in the account), source_transaction (charge), balance_transaction; embedded list `reversals` |
| `transfer_reversal` | `trr_` | None | transfer, balance_transaction, destination_payment_refund (`pyr_`, in the account); sub-list `/v1/transfers/{id}/reversals` |
| `application_fee` | `fee_` | None; `amount_refunded` and `refunded` once fully refunded | account, charge (in the account), balance_transaction; embedded list `refunds` |
| `fee_refund` | `fr_` | None | fee, balance_transaction; sub-list `/v1/application_fees/{id}/refunds` |
| `payout` | `po_` | Closed statuses: `paid`, `failed` (`canceled` is not generated) | destination (bank account), balance_transaction, failure_balance_transaction; sweeps balance transactions (`?payout=`) |
| `bank_account` | `ba_` | Hard delete (`deleted_bank_account` tombstone; the default never) | account; listed with cards under `/v1/accounts/{id}/external_accounts` |
| `card` (external account) | `card_` | Hard delete (`deleted_card` tombstone) | account; listed with bank accounts |
| `person` | `person_` | Hard delete (`deleted_person` tombstone) | account; sub-list `/v1/accounts/{id}/persons`; changes `account.requirements` |
| destination payment | `py_` | None (a `charge`; refunded only through its transfer's reversals) | source_transfer (on the platform), balance_transaction |
| its refund | `pyr_` | None (a `refund`) | charge (the `py_` payment), source_transfer_reversal (on the platform) |

Transfers, transfer reversals, application fees and fee refunds live in the platform scope; `py_` payments, `pyr_` refunds, external accounts and persons in the connected account's; payouts in every scope (ruling P1).

Phase 9 adds billing extras (section 5.5).

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
- **Shapes follow the pinned schema** (generated types, section 14). Every v1 account carries a `controller`: just `type: "account"` for Standard (the schema leaves `is_controller` and `requirement_collection` out when the platform does not control the account), and `type: "application"` with `is_controller: true` and its fee, loss, requirement-collection and dashboard split for Express, Custom and v2 accounts. Every other v1 account field is present on every account, `null` where unset: `business_type`, `groups`, `requirements`, `future_requirements`, `settings` and `tos_acceptance` among them. `external_accounts` (an empty list) appears only where `controller.is_controller` is true, so not on Standard accounts. `company` and `individual` are absent, as no business details are collected. `capabilities` lists `card_payments` and `transfers`, both active. The v2 view serves no `include`s, so the includable-only `configuration`, `defaults`, `identity`, `requirements` and `future_requirements` are absent; `applied_configurations` names the configurations.
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

**External accounts and persons of controlled accounts.** Only accounts the platform controls (`controller.is_controller: true`: Express, Custom and v2; ruling P4) have external accounts and persons. A Standard account and the platform have none, and their sub-lists are empty. Both live in the account's own scope (their events are labelled with the account) and are listed under `GET /v1/accounts/{acct}/external_accounts` (bank accounts and cards together, in creation order, `object=bank_account|card` filters) and `GET /v1/accounts/{acct}/persons` (`limit` and cursors only; a `relationship[...]` filter is a 400 `parameter_unknown`, ruling P8).

- **Setup at connect.** Right after a controlled account connects, in its scope and under the cause `account_setup`, it gets from the stream `generate.account_setup`:
  - one default `bank_account` (`account_holder_type: company`, `bank_name` uniform from `STRIPE TEST BANK`, `JPMORGAN CHASE`, `WELLS FARGO` and `BANK OF AMERICA`, `last4` four random digits, `routing_number: "110000000"`, `country: US`, `currency: usd`, `status: new`, `default_for_currency: true`, a 16-character `fingerprint`; `account.external_account.created`);
  - 1 to 3 persons with `first_name` and `last_name` from fixed lists and `email` `{first}.{last}@example.com` in lowercase; the first is the representative and an executive (`relationship.representative` and `executive` true; `person.created` each, ruling P8).
  - This is the one place a phase 7 run changes: Standard-only runs are identical.
- **Embedded list (ruling P9).** `account.external_accounts` is kept in step with the account's live external accounts: the default first, then the rest by creation, cut to `payloads.nested_list_page_size` (`has_more` when more exist). Each change is a silent account mutation (cause `account_update`, no event), so only `account.external_account.*` announces it.
- **External-account churn** (`[churn.external_account]`; streams `generate.external_account.create` and `.delete`).
  - `create`: a debit `card` with p = 0.3 (`brand` `visa` or `mastercard`, `funding: debit`, `currency: usd`, expiry 1–5 years ahead, never the default), otherwise a `bank_account`. A new bank account becomes the default with p = 0.3: the old default first gets `default_for_currency: false` (`account.external_account.updated`), then the new one is created (`account.external_account.created`).
  - `delete`: a random non-default external account is hard-deleted with its `deleted_bank_account` / `deleted_card` tombstone (`account.external_account.deleted`). The default is never deleted. With none deletable, nothing happens.
- **Person churn** (`[churn.person]`; streams `generate.person.update` and `.delete`).
  - `update`: a new `email` and `phone` (`person.updated`).
  - `delete`: a hard delete (`person.deleted`). When no representative is left, the account's `requirements.currently_due` and `eventually_due` become `["representative.first_name", "representative.last_name"]` (`account.updated`, labelled with the account). Persons are never created later, so a removed requirement is never cleared (ruling P8).
- A write during pagination of an external-account list creates an external account, and one during a persons list updates a person, in that account's scope. An unknown or disconnected account is a 404 before any write.

**Disconnect and closed semantics.**

- A disconnect is a delete in the store for both APIs; the v1 tombstone is `{"id", "object": "account", "deleted": true}`. `/v1/accounts` omits the account and `/v1/accounts/{id}` returns 404. `/v2/core/accounts/{id}` still renders the last live version with `closed: true`.
- `/v2/core/accounts` lists only open accounts by default. `closed=true` lists **only** closed accounts.
- A disconnected account's header is rejected with 403 `account_invalid` on every endpoint, so its events cannot be fetched through the Events APIs. No new events are created in its scope. Deliveries already queued for it still go out to connect destinations.
- A disconnected account's objects remain in the truth log and in subscriptions as they were, except that its pending balance transactions still become `available` at `available_on` in its scope (no event), since Stripe's settlement clock does not stop.

**Event coverage is identical for platform and connected accounts.** Every action, entity type, lifecycle and event type the mock generates for a platform account is generated for each connected account too, with the same rates. Connected-account events reach snapshot destinations with `connect = true` (carrying `account` and `context`) and thin destinations with `@accounts` in `events_from` (carrying `context`). All faults apply to them in the same way.

### 5.5 Entity types of phases 7–9

Coverage follows what a Stripe Connect integration needs, most-used first. Each type is there for the sync situation it adds. The full design is in `docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md`. ID prefixes are confirmed against Stripe's examples when each type is built; phase 7's are in section 5.1.

**Phase 7: payments core** (built).

| Type | Removal style | Sync situation |
|---|---|---|
| `payment_intent` | `canceled` status | fast status churn |
| `charge` | none (`refunded`, `disputed` flags) | updated by its children |
| `refund` | none: the mock's refunds succeed at once (Stripe's can end `canceled` / `failed`) | appears standalone and in an expanded `charge.refunds` |
| `dispute` | closed statuses (`won`, `lost`, `warning_closed`) | lives for weeks of simulated time, with many updates |
| `balance_transaction` | none | no events of its own: found only by listing; `pending → available` changes silently at `available_on` |
| `payment_method` | detach (`customer: null`, stays retrievable) | listing needs a customer |
| `setup_intent` | none: the mock's end `succeeded` or declined (Stripe's can be `canceled`) | creates payment methods |
| `checkout.session` | expires | dotted object name; creates a payment or a subscription |
| `invoice_payment` | `canceled` status | links an invoice to its payments (`invoice.payments`, when expanded) |

Paying an invoice creates, in one action: a payment intent, an invoice payment (`open` from the first attempt), a charge with its balance transaction, and the paid invoice. The customer's card payment method is created and attached first if it has none (`payment_method.attached`). Events: `payment_intent.created`, `charge.succeeded`, `payment_intent.succeeded`, `invoice_payment.paid`, `invoice.paid`, `invoice.payment_succeeded`. A failed attempt creates a failed charge instead. A zero-amount invoice creates no payment objects. Finalizing stays a separate, earlier step. New actions: `one_off_payment`, `refund` and `dispute` (until now reserved keys), plus `save_payment_method`, `checkout` and `[churn.payment_method] detach`.

**Phase 8: Connect money movement** (built).

| Type | Removal style | Sync situation |
|---|---|---|
| `transfer` and `transfer_reversal` | reversal | reversals as an embedded list and sub-list |
| `application_fee` and `fee_refund` | refund | platform-scope objects that describe connected-account charges |
| `payout` | `failed` status (`canceled` is not generated: Stripe cancels only manual payouts) | own lifecycle; creates balance transactions; runs on connected accounts too |
| external accounts (`bank_account`, `card`) | hard delete | polymorphic sub-list of an account |
| `person` | hard delete | sub-list of an account; changes `account.requirements` |

Charge patterns under Connect:
- **Connected accounts keep direct charges.** Their own payments stay in their own scope, which makes them direct charges by construction. `accounts.application_fee = { probability, percent }` (default `{ probability = 0.0, percent = 10.0 }`) adds a platform `application_fee` to some of them: see section 6.1 for what such a charge changes.
- **The platform's own payments can route money to a connected account.** `accounts.platform_payments = { own, destination, separate }` (default `{ own = 1.0 }`) draws a pattern for each platform payment, and for a non-`own` pattern, a recipient account:
  - **destination:** `transfer_data.destination`, a `transfer`, and a `py_` payment in the account's scope.
  - **separate:** separate charges and transfers: a `transfer_group`, then an independent `transfer`.
  - With no connected account available, the payment falls back to `own`.
  - The pattern is drawn once per payment intent, when it is created (stream `generate.platform_payments`, platform generator only), so a retry reuses it; the recipient is uniform over the connected accounts. A destination charge's intent and charges carry `transfer_data` (`destination`; the charge's `amount` is `null`, and the intent's is left out, as the schema does not let it be `null`) and `transfer_group: "group_{pi}"`; a separate charge's carry only the `transfer_group`. `invoice.transfer_data` stays `null`: the pattern belongs to the payment.
  - A transfer moves the charge's full amount. In one instant and under the payment's cause: the platform's balance transaction (`type: transfer`, `-amount`, no fee, `available` at once), the `transfer` (`destination`, `destination_payment` the `py_` payment, `source_transaction` the charge, `source_type: card`, the `transfer_group`, `reversals` an empty embedded list at `/v1/transfers/{id}/reversals`, `reversed: false`, `amount_reversed: 0`; `transfer.created`, an unlabelled platform event), then in the account's scope the `py_` payment's balance transaction (`type: payment`, `+amount`, no fee, `pending` for two days, settled in the account's scope) and the `py_` payment: a `charge` with `status: succeeded`, `paid` and `captured`, `source_transfer` the transfer, `payment_method_details { type: stripe_account }`, and `customer`, `payment_intent`, `payment_method` and `outcome` all `null`. The `py_` payment and the balance transactions have no events: the account finds its `py_` payments by listing `GET /v1/charges`.
  - A transfer's `destination_payment` expands from the platform, and a `py_` payment's `source_transfer` from the account (ruling P2), also when the same request expanded `destination` first.
  - A `py_` payment is never refunded or disputed on its own: the account's `refund` and `dispute` actions skip it, and a control action naming it is a 409. Its money comes back only through a reversal of its transfer, which refunds it with a `pyr_`. So a transfer's `amount_reversed` equals both the sum of its reversals and its payment's `amount_refunded`.
  - A transfer is never made to an account that has disconnected: a separate charge's queued transfer is skipped, and so is a destination charge whose account disconnected between the intent's creation and its success.
- With `connect = false`, non-default values of either key are config errors.

**Payouts** (ruling P5). `actions.payout = "daily"` (default `"off"`, which draws nothing) pays out every scope's balance at each simulated midnight UTC: the platform's and each connected account's, each run by that scope's generator. A scope's available balance in a currency (ruling P6, no conversion) is the sum of `net` over its `available` balance transactions in that currency that no payout swept yet, in creation order; a payout's own balance transaction (`type: payout`) never counts and is never swept. So, per scope and currency, the sum of `net` over every balance transaction equals the sum over those that are neither swept nor `type: payout`.

- **Created** at midnight, under one `payout` cause per scope, for each currency whose balance is above 0, in currency order (nothing happens for a balance at or below 0): the payout's balance transaction (`type: payout`, `-amount`, no fee, `available` at once, `source` the payout), then the `payout` (`po_`; `amount` the balance, `pending`, `automatic: true`, `method: standard`, `type: bank_account`, `source_type: card`, `arrival_date` two days later, `reconciliation_status: in_progress`; `payout.created`). Every balance transaction counted is recorded as swept by this payout, so `GET /v1/balance_transactions?payout=po_…` lists exactly them, and none is swept twice.
- **One hour later**: `in_transit` (`payout.updated`).
- **At `arrival_date`**: `paid` with `reconciliation_status: completed` (`payout.paid`, `payout.updated`, `payout.reconciliation_completed`), or, with `actions.payout_failure_probability` (drawn at creation from `generate.payout`), `failed` with `failure_code: account_closed`, `failure_message: "The bank account has been closed."` and a `failure_balance_transaction` (`type: payout_failure`, `+amount`, `available` at once; `payout.failed`, `payout.updated`). A failed payout's `reconciliation_status` stays `in_progress`: only a paid one becomes `completed`. That `+amount` counts toward the balance like any other, so the next midnight pays it out again.
- **Destination** (ruling P4): a controlled account's default bank account, which expands. The platform and Standard accounts name a `ba_` ID drawn once per scope from `generate.payout` that no store holds, so expanding it leaves the ID.
- **Continuations, not a settlement**: the midnight run, `in_transit` and the arrival stop while generation is off (one that fell due runs when it resumes), and a disconnect drops them, so the account's payout keeps its last status. Turning `payout` to `"daily"` at runtime queues the next midnight; `"off"` drops it, while payouts already made still arrive. IDs come from `generate.payments`, and payouts and their balance transactions live and are labelled in their own scope. `canceled` and instant payouts are not generated.

**Phase 9: billing extras.** `coupon` (hard delete), `promotion_code` (deactivate only, no delete event), `discount` (embedded only), `invoiceitem` (hard delete while pending), `credit_note` with `credit_note_line_item` (void), `tax_rate` (archive) and `subscription_schedule` (released, canceled or completed).

**Invoice items** (ruling Q4).
- **One-off lines:** every line of a one-off invoice bills its own `invoiceitem`, using the `ii_` ID its line already names. In one instant: the items, with `invoice` set to the draft (`invoiceitem.created`), then the lines, then the invoice.
- **Pending items** come from `actions.invoice_item` (stream `generate.invoice_item`): `invoice: null`, a random active one-time price (any active price when there is none), quantity 1-3, for a random customer.
- **Joining.** A customer's pending items in the invoice's currency join its next invoice of either kind, oldest first, after the invoice's own lines, except the first invoice of a Checkout subscription, whose amount the session fixed; an item that would push the subtotal above the maximum charge (99,999,999) stays pending. The item's `invoice` is set silently, and its line (`il_` plus the item's ID suffix) has `parent.invoice_item_details` naming the item and the subscription, if any.
- **Deleting** (`invoiceitem.deleted`, tombstone `deleted_invoiceitem`) happens only while an item is pending or on a draft: `churn.invoiceitem.delete` (stream `generate.invoice_item.delete`) deletes a random pending one, deleting a draft deletes its items, and deleting a customer deletes its pending items. There is no `invoiceitem.updated`, so every update is silent.

**Discounts** (rulings Q2, Q3 and Q7).
- **Embedded only.** A discount (`di_`) has no endpoint: it is `customer.discount` (the full object), `subscription.discounts` (IDs) and an invoice's and its lines' `discounts` (IDs; a removed one expands to `deleted_discount`, the full discount plus `deleted: true`, less `end`). A customer or subscription has at most one. A subscription invoice applies the subscription's discount, or else the customer's; a one-off invoice applies the customer's. The first invoice of a Checkout subscription, whose amount the session fixed, applies none.
- **Created** under one cause: the coupon's `times_redeemed` +1 (silent; `valid: false` once `max_redemptions` is reached), the promotion code's +1 (silent), the discount (`customer.discount.created`), then the parent: the customer's `discount` (silent) or the subscription's `discounts` (`customer.subscription.updated`).
- **Removed:** the parent first (silent for a customer, `customer.subscription.updated` for a subscription), then the discount (`customer.discount.deleted`). A `once` discount is removed in the instant its first invoice is finalized, even if that invoice is voided later; a `repeating` one at its `end` (`duration_in_months` after its start); a `forever` one has `end: null`. Deleting a customer removes its discount first; a canceled subscription keeps its own. A random removal picks only a discount whose parent is live (customer not deleted, subscription not `canceled` or `incomplete_expired`); when a `repeating` discount's end, or a named `remove_discount`, reaches the discount of an ended subscription, only the discount is deleted (`customer.discount.deleted`) and the subscription is left as it is. A deleted coupon's discounts outlive it and still apply its terms. `customer.discount.updated` is not generated.
- **Thin events** point `related_object.url` at the parent that embeds the discount: the subscription when set, else the customer.
- **`actions.discount_change`** (stream `generate.discount_change`) removes a random live discount whose parent is live with probability 0.3; otherwise, and whenever none exists, it adds one to a random customer or `active`/`past_due` subscription with no discount (50/50, the other kind when the drawn one has none; nothing when neither has one), redeeming an active promotion code whose coupon is valid (probability 0.5) or else a valid coupon (a new coupon when there is none).
- **Amounts.** Every invoice amount comes from one integer computation. The discount `D` is `min(subtotal, amount_off)` for an `amount_off` coupon in the invoice's currency (one in another currency applies nothing, and the invoice carries no discount), or `(subtotal × percent × 100 + 5_000) / 10_000` (half up). `D` is shared over the lines with `amount > 0` in order, each getting `D × amount / subtotal` rounded down and the last the remainder; a line's `amount` stays before discount. `total_discount_amounts = [{amount: D, discount}]`, `total_excluding_tax = subtotal − D`, `total = amount_due = amount_remaining = total_excluding_tax + tax`, and a paid invoice's `amount_paid` is the `amount_remaining` its payment charged.

**Tax rates in use** (rulings Q3 and Q5).
- **Which objects take one.** While at least one active tax rate exists, a new subscription (from `start_subscription`, a Checkout session's completion or `schedule_subscription`) and a new one-off invoice each take one with probability 0.5, drawn uniformly over the active rates from stream `generate.tax`. With no active rate nothing is drawn from that stream, so a run without tax rates draws exactly as before.
- **Where it is stored.** A subscription keeps it in `default_tax_rates` (`[rate]`), and its invoices tax every line with it (their own `default_tax_rates` stays `[]`). A one-off invoice keeps it in its own `default_tax_rates`. `tax_rates` on subscription items and invoice items stays `[]`, and `inclusive` is always `false`.
- **Snapshots.** The embedded rate is a copy taken when it was attached: archiving the rate does not rewrite it (an old embed still shows `active: true`), the subscription's later invoices are still taxed with it, and an archived rate is never attached again. Deleting an item from a taxed draft recomputes the draft's tax from that copy.
- **Amounts.** Each line's tax is `(taxable × bp + 5_000) / 10_000` (half up), where `taxable` is the line's amount less its discount share and `bp` the percentage in basis points (`8.875` is 888). A line's `taxes` is `[{amount, tax_behavior: exclusive, tax_rate_details: {tax_rate}, taxability_reason: standard_rated, taxable_amount, type: tax_rate_details}]`, and the invoice's `total_taxes` has one such entry with the amounts summed. An invoice whose total with tax would exceed the maximum charge (99,999,999) is created without its tax (`total_taxes: []`), though a one-off invoice still stores its rate.
- **Checkout.** The first invoice of a Checkout subscription is not taxed, because the session fixed its amount (`amount_paid` equals the session's `amount_total`); the subscription still stores its rate, and its renewal invoices are taxed.

**Credit notes** (ruling Q8).
- **`actions.credit_note`** (stream `generate.credit_note`) voids a random voidable note (an `issued` pre-payment note whose invoice is still `open`) with probability 0.25 when one exists; otherwise it credits a random eligible invoice: an `open` one with `amount_remaining ≥ 2` gets a **pre-payment** note for `max(1, amount_remaining × p / 100)` (`p` in 10–50, so it is never fully credited), and a `paid` one whose charge has something left to refund (`amount − amount_refunded`; a disputed charge has nothing) gets a **post-payment** note for `max(1, left × p / 100)` (`p` in 10–100). With no eligible invoice nothing happens.
- **Fields.** `number` is `{invoice number}-CN-{n:02}`, `n` counting the invoice's notes; `pdf` is `https://pay.stripe.com/credit_notes/mock/{id}/pdf`; `reason` is drawn from the enum; `subtotal = total = amount` with no discount and no tax; `pre_payment_amount` or `post_payment_amount` is the amount (the other 0); `type` is `pre_payment` or `post_payment` (`mixed` is not generated); `effective_at` is `created`.
- **Line items.** The amount is shared over the invoice's lines with `amount > 0` by the discount sharing rule (each `amount × line / sum` rounded down, the last the remainder), one `credit_note_line_item` per line: `type: invoice_line_item`, the line's ID and description, `taxes: []`, `tax_rates: []`, `quantity` and `unit_amount` `null`. A line item has no parent field, so the engine keeps each note's line items in an index; `/v1/credit_notes/{id}/lines` and the note's embedded `lines` page from it.
- **Order.** A post-payment note refunds first (`refund.created`, `charge.refunded`), so its `refunds = [{amount_refunded, refund, type: refund}]` names a refund that exists; then the line items (no event), the note (`credit_note.created`) and the invoice (`invoice.updated`): a pre-payment note lowers `amount_due` and `amount_remaining` and raises `pre_payment_credit_notes_amount` by the amount; a post-payment note raises `post_payment_credit_notes_amount`.
- **Voiding** sets the note `void` with `voided_at` (`credit_note.voided`), then reverses its invoice's changes (`invoice.updated`). A post-payment note, or one whose invoice is no longer open, is never voided.
- **Paying after a credit.** The payment of an open invoice charges its `amount_remaining`. When a retry finds an open invoice payment whose `amount_requested` differs (a note was issued or voided since the first attempt), that payment's `amount_requested` and its payment intent's `amount` (and application fee, recomputed without a draw) are updated silently before the new charge.

**Subscription schedules** (ruling Q9).
- **`actions.schedule_subscription`** (stream `generate.subscription.schedule`) draws a customer (as `start_subscription` does), 2-3 phases and an `end_behavior` (`release` 70%, `cancel` 30%). Each phase has 1-2 active recurring prices sharing the currency and interval of one drawn price (distinct while the pool lasts), quantities 1-5 capped like a subscription's, and lasts 1-3 of that interval. Every draw, each later phase's item IDs included, is made when the action runs.
- **Created** in one instant: the subscription, as `start_subscription` creates it (it may take a tax rate, and every phase repeats its `default_tax_rates`), with `schedule` set; then the schedule (`sub_sched_`): `active`, `subscription` the subscription, `phases` from the subscription's start (each `start_date` the previous `end_date`, each `end_date` its start plus its iterations by the add-months rule) and `current_phase` the first (`subscription_schedule.created`). The first invoice, payment and activation then run as for any subscription. `not_started` is not generated.
- **At each renewal** of a scheduled subscription (`active` or `past_due`), before the renewal invoice, when the period being closed ends at or after `current_phase.end_date`: with a next phase, the old items are deleted and the next phase's created (both unannounced) and `current_phase` moves on (`subscription_schedule.updated`), so the renewal invoice bills the new phase. After the last phase, `release` sets the schedule `released` with `released_at`, `released_subscription`, `subscription: null` and `current_phase: null` (`subscription_schedule.released`), then clears the subscription's `schedule` (`customer.subscription.updated`), and the renewal goes on; `cancel` cancels the subscription now (`customer.subscription.deleted`), then sets the schedule `completed` with `completed_at` (`subscription_schedule.completed`), and there is no renewal invoice.
- **Ending early.** When a scheduled subscription ends any other way, its schedule is `canceled` with `canceled_at` and `current_phase: null`, announced after the subscription: `cancel_subscription` (which cancels a scheduled subscription now, whatever `at_period_end` says), customer deletion and `incomplete_expired` give `subscription_schedule.canceled`; a failed dunning retry gives `subscription_schedule.aborted` (status still `canceled`). An ended subscription never moves a phase, and with renewals off (`renew_subscription_cycle = "off"`) a schedule never advances. Checkout subscriptions are never scheduled; `subscription_schedule.expiring` is not generated.
- **API.** `/v1/subscription_schedules` (filter `customer`) and `/v1/subscription_schedules/{id}`, expanding `customer` and `subscription`; a subscription's `schedule` expands while set. The schedule pagination write changes a random active schedule's `metadata.note` (`subscription_schedule.updated`).

Every new type gets events for both payload formats, history, faults and the latest-state subscription. It also gets the same Connect coverage as platform types, except the scope-specific money movement above.

## 6. Action generation

### 6.1 Cause-driven fan-out

In Stripe, one action produces many events across objects. The generator models **business actions**, and each action expands into the realistic sequence of mutations. For example, `start_subscription`:

1. `customer.subscription.created`
2. `invoice.created` → `invoice.finalized`
3. `payment_method.attached` (the customer's first payment only), `payment_intent.created`, `charge.succeeded`, `payment_intent.succeeded`, `invoice_payment.paid`
4. `invoice.paid`, `invoice.payment_succeeded`
5. `customer.subscription.updated` (status `incomplete → active`)

A tax rate drawn for the subscription (section 5.5, ruling Q5) is part of step 1 and adds no event; its invoices are taxed from step 2 on, except the first invoice of a Checkout subscription, whose amount the session fixed.

Paying an invoice runs the payment sequence of section 5.5, one mutation per step, all in the same simulated instant: the customer's card (created and attached only if it has none, `payment_method.attached`), the payment intent (`requires_confirmation`, `payment_intent.created`) and, linked to it, the invoice's `open` invoice payment (no event), the charge's `pending` balance transaction (no event; fee 2.9% + 30 in the minor unit, `available_on` two days later), the charge (`charge.succeeded`), the intent `succeeded` (`payment_intent.succeeded`), the invoice payment `paid` (`invoice_payment.paid`), and the paid invoice (`invoice.paid`, `invoice.payment_succeeded`). A failed attempt creates a `failed` charge (`card_declined`, `charge.failed`), returns the intent to `requires_payment_method` with its `payment_method` cleared and the card named in `last_payment_error` (`payment_intent.payment_failed`), then announces `invoice.payment_failed`; the invoice payment stays `open`. A retry finds the intent through that open invoice payment and confirms it again with the customer's card and a new charge. Voiding the invoice after a failure cancels the invoice payment (`canceled`, no event: the pinned API has no `invoice_payment.canceled`) and then the intent (`payment_intent.canceled`, `cancellation_reason: void_invoice`). Marking it uncollectible leaves both open, as Stripe keeps an uncollectible invoice payable. A zero-amount invoice creates no payment objects: it is paid directly, and an expanded `invoice.payments` is empty. At `available_on` the balance transaction becomes `available` without an event; this settlement clock runs even while generation is off. Payment draws come from the stream `generate.payments`; whether an attempt fails is still drawn from `generate.billing`.

**Application fees.** When a payment intent is created in a connected account's scope and `accounts.application_fee.probability` is above 0, a draw from `generate.application_fee` decides whether it carries a fee, stored as the intent's `application_fee_amount`. A successful confirmation then changes the sequence above: the charge's balance transaction is created with `fee` increased by the fee, a second `fee_details` entry (`type: application_fee`, `application` the platform's `ca_...`) and `net` reduced to match; the charge carries `application_fee_amount` and `application_fee` (the fee's ID, drawn before the charge). After `payment_intent.succeeded`, in the platform scope and under the same cause: the fee's balance transaction (`type: application_fee`, `+fee`, no fee, `pending` with the charge's `available_on`, its settlement queued with the platform as its scope, so it still settles after the account disconnects), then the `application_fee` (`account`, `application`, `charge`, `amount`, `currency`, an empty embedded `refunds` list at `/v1/application_fees/{id}/refunds`, `fee_source { charge, type: charge }`, `originating_transaction: null`; `application_fee.created`, an unlabelled platform event). A failed attempt carries no fee, and a retry reuses the intent's. `refunds` is a required embedded list, always present, so it is not an `expand[]` path of `application_fee`.

**Charge patterns.** A platform payment intent drawn as a destination charge or a separate charge (section 5.5) changes the sequence above after `payment_intent.succeeded`, and before any `invoice_payment.paid`. A destination charge's transfer follows at once in the same instant: the platform's `transfer` balance transaction, the transfer (`transfer.created`), the account's `payment` balance transaction and its `py_` payment (no events), then the charge's `transfer` set to it (a silent charge update, no event). A separate charge queues its transfer instead, 1–72 hours later (uniform, drawn at payment time from `generate.platform_payments`), as a continuation under the payment's cause; it makes the same transfer, with `source_transaction` the charge and the intent's `transfer_group`, unless the recipient has disconnected by then. A failed attempt transfers nothing.

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
- Phase 7 endpoints, each with retrieve `/{id}`; filters beyond `limit`, the cursors, `created` and `expand[]`:
  - `GET /v1/payment_intents`: `customer`.
  - `GET /v1/charges`: `customer`, `payment_intent`.
  - `GET /v1/refunds`: `charge`, `payment_intent`.
  - `GET /v1/disputes`: `charge`, `payment_intent`.
  - `GET /v1/balance_transactions`: `type`, `source`, `payout` (the balance transactions that payout swept, section 5.5; an unknown payout matches nothing).
  - `GET /v1/payment_methods`: `customer` (required), `type`. Sub-list `GET /v1/customers/{id}/payment_methods`: `type`.
  - `GET /v1/setup_intents`: `customer`, `payment_method`.
  - `GET /v1/checkout/sessions`: `customer`, `payment_intent`, `subscription`, `status`.
  - `GET /v1/invoice_payments`: `invoice`, `payment` (the paid payment intent's ID).
  - Includable lists: `charge.refunds` and `invoice.payments` (see section 7.3).
  - A list's `url` is its bare path, without the query string.
- Phase 8 endpoints, each with retrieve `/{id}`; filters beyond `limit`, the cursors, `created` and `expand[]`:
  - `GET /v1/transfers`: `destination`, `transfer_group`. Sub-list `GET /v1/transfers/{id}/reversals`, with retrieve `/{reversal}`.
  - `GET /v1/application_fees`: `charge`. Sub-list `GET /v1/application_fees/{id}/refunds`, with retrieve `/{refund}`.
  - `GET /v1/payouts`: `status`, `destination`. `GET /v1/balance_transactions?payout=po_…` lists the balance transactions a payout swept.
  - `GET /v1/charges` gains `transfer_group`.
  - `GET /v1/accounts/{id}/external_accounts` (bank accounts and cards together; `object=bank_account|card`), with retrieve `/{external_account}`; `GET /v1/accounts/{id}/persons` (`limit` and cursors only, ruling P8), with retrieve `/{person}`. Both are account endpoints, so platform-only, and empty for Standard accounts (ruling P4).
  - Transfers, application fees and their sub-lists serve the platform's objects; in a connected scope they are empty. `py_` payments and `pyr_` refunds are listed by the account under `/v1/charges` and `/v1/refunds`.
  - Cross-scope expansion (ruling P2): `transfer.destination_payment`, `transfer_reversal.destination_payment_refund` and `application_fee.charge` expand from the account's scope, and a direct charge's `application_fee`, a `py_` payment's `source_transfer` and a `pyr_` refund's `source_transfer_reversal` from the platform's.

### 7.3 List semantics

- Ordered by `created` descending; cursor pagination via `starting_after` / `ending_before`; `limit` 1–100, default 10; `has_more`.
- `created[gt|gte|lt|lte]` filters where Stripe supports them.
- Deleted objects are excluded; archived ones are included (with filters such as `active=`).
- Embedded lists (`subscription.items`, `invoice.lines`) truncated to `nested_list_page_size` with `has_more: true` and a `url` for the sub-list.
- Expandable fields return IDs unless `expand[]` asks for the object. An account that has disconnected stays its ID when expanded, and so does any object a cross-scope path would read from its scope: the platform has lost access, and no field naming an account is nullable.
- Includable lists, `charge.refunds` and `invoice.payments`, are absent unless `expand[]` names them (`refunds`, `payments`, or `data.refunds` / `data.payments` on a list), as live Stripe serves them. They never appear in events or truth. When expanded, they are built from the objects visible to the request (refunds newest first, invoice payments in creation order), truncated to `nested_list_page_size` with `has_more`, `total_count` and the sub-list `url`.
- Sub-lists list in creation order, ascending, so their first page equals the embedded list. The exceptions are a transfer's `/reversals` and an application fee's `/refunds`, which list newest first, like their embedded lists.

### 7.4 Consistency faults on the API

- **Stale reads** (optional): retrieve/list may briefly return an older version of an object.
- **Writes during pagination**: objects created, changed or deleted while a client pages through a list, so a backfill can see an object twice, miss a new one, or page past a deleted one.
- Rate limiting (429) with a token bucket, plus random 5xx, timeouts and connection resets per `[api]`.
- A `timeout` holds the response for 80 s (Stripe's own request limit) or until the mock shuts down, then drops the connection; a `reset` drops it at once. Either way the client gets no complete response.
- 429, 500 and 503 responses carry `Stripe-Should-Retry: true`, the usual error envelope and a `Request-Id`.
- Faults apply only to requests that pass authentication and the other request checks, in this order: rate limit (a forced storm, then the bucket), then a random error, then latency, then the handler.
- `api.pagination_writes`: when it fires on a list request that carries a cursor, one random action of the listed type runs before the page is served (`/v1/customers/{id}/payment_methods` writes like `/v1/payment_methods`), so the race happens in virtual time too. It does nothing while generation is off (after quiesce).

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

- Delivery targets come from `[[destinations]]` in the config or from the control API. Each is either a **snapshot** target (v1 webhook endpoint semantics) or a **thin** target (v2 event destination semantics), with its own `enabled_events` and its own signing secret. Snapshot targets can be account or Connect endpoints; Connect events carry `account` and `context` (snapshot) or `context` (thin).
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

`run_action` takes an `Action`, tagged by `type` in snake case: `new_customer`, `catalog_change`, `archive {id}` (a product, price, promotion code or tax rate), `billing_catalog_change`, `delete_coupon {id}`, `create_invoice_item {customer, price, quantity}`, `delete_invoice_item {id}`, `add_discount {customer, subscription, coupon, promotion_code}`, `remove_discount {id}`, `credit_note {invoice, amount}`, `void_credit_note {id}`, `schedule_subscription {customer, end_behavior}` (`end_behavior`: `release` or `cancel`), `cancel_subscription_schedule {id}`, `one_off_invoice {customer, outcome}`, `start_subscription {customer, prices}`, `cancel_subscription {id, at_period_end}`, `one_off_payment {customer, amount, succeed, pattern, recipient}` (`pattern`: `own`, `destination` or `separate`), `refund {charge, amount}`, `dispute {charge, outcome}` (`outcome`: `won`, `lost` or `warning_closed`), `save_payment_method {customer}`, `checkout {customer, mode, complete}` (`mode`: `payment` or `subscription`), `detach_payment_method {id}`, `create_external_account {kind}` (`kind`: `bank_account` or `card`), `delete_external_account {id}`, `update_person {id}`, `delete_person {id}`, `application_fee_refund {fee, amount}`, `transfer_reversal {transfer, amount}` and `payout`. Fields left out or `null` are drawn as the generated action draws them, for example `{"op": "run_action", "action": {"type": "refund", "charge": "ch_…", "amount": 500}}`. `refund` with a named charge that has not succeeded, is a `py_` payment, or has less than `amount` (or nothing) left to refund is a 409 `invalid_action` naming the charge, and so is a `refund` with no charge when no charge is refundable; a `one_off_payment` `amount` below 1 is a 409 `invalid_action` too, and so is a `dispute` on a named charge that cannot be disputed (not succeeded, refunded, already disputed or a `py_` payment; the message names it), or with no charge when none is disputable. `save_payment_method` and `checkout` with no customer when no customer is live are 409 `invalid_action`, and a named customer that does not exist is a 404. `detach_payment_method` on a named payment method that is not attached is a 409 `invalid_action` naming it (an unknown one is a 404), and so is one with no ID when none is attached. A given `pattern` of `one_off_payment` skips the draw of section 5.5, and a given `recipient` names the connected account a non-`own` pattern pays; a non-`own` `pattern` with no connected account, a `recipient` that is not connected (the message names it), a `recipient` with `pattern: own`, or either field in a connected account's scope is a 409 `invalid_action`. Both default to `null`, so older scripts still parse. The four Connect actions above run in a connected account's scope (`account` on the op) and only in an account the platform controls: run in the platform scope or in a Standard account, each is a 409 `invalid_action` naming the scope. `delete_external_account` with no ID when none is deletable, or with a named ID that is the default or not an external account of the account, is a 409 `invalid_action` naming it; `update_person` and `delete_person` with a named person that is not live, or with none when no person is live, are too. `application_fee_refund` runs in the platform scope only (in a connected account's scope it is a 409 `invalid_action`); a named fee that is fully refunded or whose account has disconnected, an `amount` outside `[1, left]`, or no fee when none is refundable are 409 `invalid_action` too (the message names the fee), and an unknown fee is a 404. `transfer_reversal` follows the same rules: it runs in the platform scope only; a named transfer that is fully reversed or whose destination has disconnected, an `amount` outside `[1, left]`, or no transfer when none is reversible are 409 `invalid_action` too (the message names the transfer), and an unknown transfer is a 404. `delete_coupon` on a coupon that does not exist or is already deleted, and `archive` of a promotion code or tax rate that is already inactive, are 409 `invalid_action` naming the ID (deleting a coupon first deactivates its active promotion codes). `create_invoice_item` for a customer that does not exist or is deleted, with a `price` that is not an active one-time price, or with `quantity: 0` or a quantity whose amount would exceed the maximum charge (99,999,999) is a 409 `invalid_action` naming it (`price` and `quantity` default to `null`, drawn). `delete_invoice_item` on an item that does not exist, is already deleted or is on an invoice that is not a draft is a 409 `invalid_action` naming it, and so is one with no ID when no item is pending; deleting an item on a draft also deletes its line and updates the draft's totals (`invoice.updated`). `add_discount` with both `customer` and `subscription`, or both `coupon` and `promotion_code`, is a 409 `invalid_action`; so is a named customer that does not exist or is deleted, a named subscription that does not exist or has ended, a named target that already has a discount (the message names it and its discount), a named coupon that does not exist, is deleted or is no longer valid, a named promotion code that does not exist or is inactive, and no target to draw when neither is named; fields left out are drawn as `discount_change` draws them. `remove_discount` on a discount that does not exist or is already removed is a 409 `invalid_action` naming it, and so is one with no ID when no discount is live. `credit_note` on a named invoice that does not exist, is neither `open` nor `paid`, or has nothing left to credit, or with an `amount` outside `[1, amount_remaining − 1]` (open) or `[1, left to refund]` (paid), is a 409 `invalid_action` naming the invoice; with no invoice named it picks a random eligible invoice the `amount` fits, and none is a 409 `invalid_action`. `void_credit_note` on a note that does not exist, is already void, is post-payment, or whose invoice is no longer open is a 409 `invalid_action` naming it, and so is one with no ID when no note is voidable. `schedule_subscription` for a customer that does not exist or is deleted is a 409 `invalid_action` naming it. `cancel_subscription_schedule` cancels the schedule (`subscription_schedule.canceled`), then its subscription now (`customer.subscription.deleted`; an incomplete one's open first invoice is voided); on a schedule that does not exist or is not `active` it is a 409 `invalid_action` naming it, and so is one with no ID when no schedule is active. `payout` pays out every currency of the scope's available balance now, as the midnight run of section 5.5 does, whatever `actions.payout` is (its failure draw comes from the control stream); with no balance above 0 it is a 409 `invalid_action` naming the scope.

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
  - Connect setup and churn streams (phase 8): `generate.account_setup` (the bank account and persons of a newly connected controlled account), `generate.external_account.create`, `generate.external_account.delete`, `generate.person.update` and `generate.person.delete`. All of them draw nothing unless a controlled account exists or its rate is above 0.
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
  - **Field presence.** The spec's `required` lists don't say which fields Stripe always returns. For example, `customer` lists only `created`, `id`, `livemode` and `object`. Generated structs therefore have no defaults, and the model code decides explicitly, for every field, whether it is present, `null` or absent. A test compares each type's served fields with the schema's, against a reviewed list of deliberate exclusions. Two phase 8 exclusions depend on the object: a destination charge that moved no money (a failed one) omits `charge.transfer`, which is not nullable, and a `pyr_` refund omits `destination_details`, which is not nullable and has no variant for a payment from a Stripe balance.
- **OpenAPI conformance is part of the regular test suite.** It runs on every full test run (`cargo test --workspace`), locally or in CI, with no extra command, flag or credentials, and offline. Three checks:
  - the freshness test above;
  - a round-trip check that deserializes every payload the tests receive into its generated type, with unknown fields denied;
  - a coverage test (`tests/openapi_conformance.rs`) that runs a busy Connect run (30 days of history, every action and churn rate on, three initial connected accounts, every charge pattern, application fees on half the connected accounts' payments, and daily payouts of which a fifth fail) and checks everything it serves: every list fully paginated, every sub-list (a transfer's reversals, a fee's refunds, an account's external accounts and persons, each holding exactly the live children of its type), every object by ID (children through their parent's path), every allowed expand path, every event page of both formats and every delivered webhook. It requires each of the 25 object types and the documented status values to appear, payouts `pending`, `in_transit`, `paid` and `failed` among them, as well as a fully reversed transfer, a fully refunded application fee and external accounts of both objects. Each scope expands every path some object in it can expand: a cross-scope path (ruling P2) only while its target account is connected, and `payout.destination` only where a store holds that bank account (ruling P4). Across scopes, that is every allowed path except a reviewed list of fields the mock never sets.
  - The same file holds the field-presence test above (phase 8 types and `py_` / `pyr_` objects included), a cross-object consistency check of the payment objects, and payment, transfer and payout lists paginated under stale reads and pagination writes.
  - A money check over the busy run's truth state: a transfer's amount is its `py_` payment's; its `amount_reversed` is the sum of its reversals and the payment's `amount_refunded`; an application fee's amount is its charge's `application_fee_amount` and an `application_fee` entry of that charge's balance-transaction fees; a payout's amount is the sum of the `net` of what it swept (recomputed from the truth log and compared with `?payout=`), always above 0, with no balance transaction swept twice; and per scope and currency, the sum of every balance transaction's `net` equals the sum over the unswept, non-payout ones.

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

Phases 1 to 8 are done; phase 9 is next.

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
| 14 | (P1) Each Connect object lives in one scope, and its events carry that scope's label: transfers, reversals, application fees and fee refunds on the platform (unlabelled events); `py_` payments, `pyr_` refunds, external accounts and persons in the account (`account.external_account.*` and `person.*` labelled with it); payouts in every scope. `py_` payments, `pyr_` refunds and balance transactions emit no events. | 5.1, 5.4, 5.5 |
| 15 | (P2) A fixed table of paths expands across scopes, each read at the request's view time: a transfer's `destination_payment`, a reversal's `destination_payment_refund` and a fee's `charge` from the account; a direct charge's `application_fee`, a `py_` payment's `source_transfer` and a `pyr_` refund's `source_transfer_reversal` from the platform. A disconnected account, and anything in its scope, stays an ID. | 5.5, 7.2, 7.3 |
| 16 | (P3) ID prefixes `tr_`, `trr_`, `fee_`, `fr_`, `po_`, `ba_`, `card_`, `person_`; a destination payment is a `charge` with `py_`, its refund a `refund` with `pyr_`. | 5.1 |
| 17 | (P4) Only accounts the platform controls (Express, Custom and v2) have external accounts and persons, set up at connect. The platform's and Standard accounts' payouts name a `ba_` destination no store holds. | 5.4, 5.5 |
| 18 | (P5) Payouts are automatic and daily (`actions.payout = "daily"`): at each midnight UTC, per scope and currency with a balance above 0; `in_transit` an hour later; `paid` or `failed` two days later. `canceled` and instant payouts are not generated. | 4, 5.5 |
| 19 | (P6) Balances are kept per currency with no conversion; a balance transaction's currency is its source's. No `balance.available` event. | 5.5 |
| 20 | (P7) The charge pattern is drawn once per platform payment intent, so a retry reuses it; a destination charge transfers at once, a separate one 1–72 hours later unless the recipient has disconnected. `invoice.transfer_data` stays `null`. A `py_` payment is refunded only through its transfer's reversals. | 5.5, 6.1 |
| 21 | (P8) A controlled account gets 1–3 persons at connect, the first its representative; deleting the last representative makes the representative's name due. Persons are never created later, and the persons sub-list has no `relationship` filters. | 5.4 |
| 22 | (P9) `account.external_accounts` is kept in step with the account's external accounts, the default first, through silent account mutations; only `account.external_account.*` announces a change. | 5.4 |

## 18. Open questions

- **v1/v2 account interoperability** (answered): both APIs read both kinds of account, and v2 accounts also emit v1 `account.updated` and `account.application.*` events alongside the thin `v2.core.account.*` events. Taken from Stripe's Accounts v2 and Connect webhooks documentation (docs.stripe.com); details in section 5.4.
- **v2 list details** (pagination parameters, filters beyond `types`): to be taken from the OpenAPI spec for the pinned version rather than decided here.
