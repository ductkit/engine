# Stripe mock: generated types and new entity types (phases 7–9)

Status: draft for approval · Crate: `crates/stripe-mock` · Spec: `docs/stripe-mock-spec.md` · Pinned API version: `2026-11-18.endive`

## 1. Intent

`ductkit` is a generic sync engine. A user picks any Stripe entities, maps their fields to database columns and gives it API and webhook secrets. The engine backfills those entities and then keeps them current. The mock is the engine's test fixture, so two things matter:

1. **Field-level fidelity.** Users map individual fields to columns. If a field is misnamed, mistyped or wrongly nullable in the mock, a mapping can pass against the mock and then fail against real Stripe. The mock's object shapes must therefore come from Stripe's own OpenAPI spec for the pinned version, and the test suite must prove it on every full run.
2. **Coverage of what a Stripe (Connect) integration needs.** Users sync the popular, necessary entities first. The mock should cover everything a typical integration builds on: payments, Connect money movement and billing. Every type also adds the sync situations it brings, such as silent changes, objects found only by listing, and detach instead of delete.

## 2. Goals, non-goals, anti-goals

**Goals**
- Every Stripe object the mock serves (REST responses, embedded and expanded objects, event `data.object`, thin-event related objects) is built from Rust types generated from the vendored OpenAPI spec.
- `cargo test --workspace` proves, offline and with no extra flags, that the committed generated code matches the vendored spec, and that every served payload fits the generated types.
- About 20 new entity types in three groups, each delivered as its own phase:
  - **Phase 7:** generated types for the existing 8 types, and group 1 (payments core).
  - **Phase 8:** group 2 (Connect money movement).
  - **Phase 9:** group 3 (billing extras).
- Paying an invoice produces real payment objects. Under Connect, it also produces the money movement of the configured charge pattern.

**Non-goals**
- Sharing the generated types, or any mock code, with the sync engine (`ductkit-core`). The user ruled this out; the types live inside `ductkit-stripe-mock`.
- Generating types for the whole spec. Only the allowlisted types and what they need are generated.
- Write-side fidelity (spec section 1 non-goal stays): the API stays read-only.
- Calibration against real Stripe (deferred, spec section 14).
- Multiple API versions.

**Anti-goals** (these would make it a failure even if it technically works)
- A test or build that needs the network, a token or a manual step to pass.
- Generated code that `cargo fmt --all` or `cargo clippy -D warnings` churns or rejects, so the freshness check fights the project's own commands.
- Hand-written JSON that bypasses the generated types for any served object. That would quietly reopen the drift the generated types exist to close.
- A build noticeably slower because of the generated code. Compile time is why the allowlist exists.

## 3. Constraints

- Rust 2024 workspace rules (`CLAUDE.md`): dependency versions only in root `[workspace.dependencies]`; fmt, clippy (`-D warnings`) and tests pass before each commit; LF line endings.
- The spec's existing behavior (truth log vs. projection, versions, events, faults, Connect scoping, replay determinism) must not change, except for the documented change in invoice-payment fan-out. All existing tests keep passing, except that tests asserting the old single-mutation invoice payment are updated to the new fan-out.
- Determinism: the same seed and config give the same run. New types draw only from the seeded RNG. An export taken before the change does not need to replay identically after it. (Exports are already tied to the mock's version; that is not new.)

## 4. Approaches considered

Three ways of tying the mock to the OpenAPI spec:

| | Approach | Trade-off |
|---|---|---|
| A | Hand-written strict runtime checker that walks each response alongside its schema and rejects unknown fields | A few hundred lines to own; errors caught only at test time |
| B | Off-the-shelf JSON Schema validator over a rewritten, closed copy of Stripe's schemas | Mature validator, but the rewrite is the hard part, and errors point at rewritten schemas |
| **C** | **Generate Rust types from the spec and build every object from them** | **Strongest guarantee (shape errors fail at compile time); costs a generator and a rewrite of the existing model code** |

**Chosen: C** (the user's choice, after first leaning to A). Rust types generated from the spec, committed to the repo, with the generator kept inside the mock crate.

## 5. Decided by the user

- Entity coverage follows what a Stripe Connect integration needs, most popular first. All three groups are in scope.
- The OpenAPI spec for the pinned version is vendored into the repo.
- Rust types are generated from the spec, and every object is built from them.
- The generated types live inside `ductkit-stripe-mock` and are not shared with the sync engine.
- OpenAPI conformance is part of the regular test suite (spec section 14, already updated).
- Calibration is deferred.

## 6. Design

### 6.1 The vendored spec

- **File:** `crates/stripe-mock/openapi/spec3.json`, copied verbatim from `stripe/openapi`, path `latest/openapi.spec3.json`, at commit `0f6d4dfafe157c10df393ce8a27d00b186ab3955` ("Update OpenAPI for 2026-11-18.endive"). That file covers v1 and v2, is 5,190,895 bytes, has `info.version` `2026-11-18.endive`, and has SHA-256 `e1fd0c9ed163b6ec53594ac30d60b251d7cef5a0ee911760bc89cdceafbe227b`. Do **not** take it from tag `v2602`: that tag's copy reports `2026-10-28.endive`.
- **Provenance:** `crates/stripe-mock/openapi/README.md` records the source repo, path, commit, SHA-256 of the file, and how to update it (replace the file, regenerate, fix what breaks).
- **`.gitattributes`:** marks the file `-diff linguist-generated`, with LF line endings.
- **Version test:** a test asserts the file's `info.version` equals the pinned version. The constant the API serves (`Stripe-Version` check) must equal it too.

### 6.2 Generated code

- **Where:** `crates/stripe-mock/src/stripe/generated/`, committed. The rest of the crate imports it through a `stripe` module (`crate::stripe::{Customer, Charge, …}`).
  - The files start with a "generated, do not edit" header.
  - They are excluded from rustfmt with `#![cfg_attr(rustfmt, rustfmt_skip)]` or an `ignore` in a crate-level `rustfmt.toml`. The generator emits already-formatted code (via `prettyplease`), so the files are readable and stable.
  - Clippy lints that generated code inevitably trips are allowed at the module level only.
- **The generator** lives in the crate's test tree: `crates/stripe-mock/tests/codegen/` (generator modules) plus an integration test, `tests/openapi_codegen.rs`.
  - The test generates the code in memory and compares it byte for byte with the committed files. It fails with a diff summary and the regenerate command.
  - Run with `DUCTKIT_REGEN=1`, the same test writes the files instead:
    ```
    DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen
    ```
  - This keeps the generator out of the runtime library and out of the binary, adds no crate, and makes freshness part of `cargo test --workspace`.
  - Generator-only dependencies (`syn`, `quote`, `prettyplease`, `proc-macro2`) are dev-dependencies.
- **Allowlist:** a single list in the generator names the root schemas to generate (section 6.5 lists them per phase). Measured against the vendored file, the roots of all three phases plus their non-expandable closure come to about 800 of the spec's 1,987 schemas, with about 3,700 properties in total. The generator emits that closure in full and never prunes branches to `serde_json::Value` to save compile time: field fidelity outranks build time. The plan records the clean `cargo build -p ductkit-stripe-mock` time before and after each phase's generation and reports it. The generator emits each root plus the transitive closure of **non-expandable** `$ref`s.
  - An expandable reference to a type that is not allowlisted is generated as ID-only. The mock can never serve it expanded, and expanding it in a request gets the same error as any field that isn't expandable.
- **Type mapping:**
  - An object schema becomes a struct, with fields in spec order and serde renames where a field name isn't a valid Rust identifier.
  - A required field with `nullable: true` becomes `Option<T>` and is always serialized (as `null` when `None`).
  - A field that is not required becomes `Option<T>` with `skip_serializing_if = "Option::is_none"` (absent when `None`).
  - A field that is both optional and nullable becomes `Option<Option<T>>` (absent, `null`, or a value).
  - A string `enum` becomes a Rust enum with serde renames. A one-value enum (the `object` field) becomes a unit-like type that always serializes to its value, so it can't be set wrong.
  - An `anyOf` of `string` and an expandable `$ref` (listed in `x-expandableFields`) becomes `Expandable<T>`: `Id(String)` or `Object(Box<T>)`.
  - An `anyOf` over several expandable resources (for example `balance_transaction.source`) becomes a generated enum over the allowlisted members, plus an ID-only variant.
  - Any other `anyOf` or `oneOf` becomes a generated untagged enum.
  - An `object: "list"` wrapper schema becomes a generic `List<T>` (`data`, `has_more`, `url`, and `total_count` where the schema has it). The existing `embedded_list` helper produces this type.
  - `metadata` and other string-to-string maps become `BTreeMap<String, String>`.
  - Integers become `i64`; `number` becomes `f64`; `format: decimal` strings stay `String`. Unix timestamps stay `i64`.
  - Free-form objects become `serde_json::Value`. This is the only place untyped JSON is allowed, and the generator lists each occurrence in a comment.
  - Deleted-object schemas (`deleted_customer` and so on) are generated like any other and used for tombstones.
  - v2 schemas (`v2.core.account`, `v2.core.event`, …) are generated the same way.
- **Serde:** every generated type derives `Serialize`, `Deserialize`, `Clone`, `Debug` and `PartialEq`. `Deserialize` uses `deny_unknown_fields`, for the round-trip check in section 6.3.

### 6.3 How objects are built, and the runtime round-trip check

- **The store keeps JSON.** The truth log, versions, `previous_attributes`, export and replay keep working on `serde_json::Value` as today.
- **Construction and every mutation go through the generated structs.** The model code builds a typed object, or deserializes the stored version, applies the change to the typed value and serializes it back.
  - `new_customer_json`-style functions return the typed struct, and serialization happens at the store boundary.
  - Direct edits to `Value` in model code are not allowed. The plan's review checks for them.
- **Round-trip check.** The generated types make it impossible to serialize a wrong shape. A test then proves nothing slipped past them:
  - It deserializes every payload the mock serves back into the generated type for that path or event, with unknown fields denied.
  - Payloads covered: REST responses for every endpoint, embedded lists, expanded fields, tombstones, error envelopes, events, thin events and webhook bodies.
  - It runs in two places:
    1. **The shared test helpers** (`tests/common`): every `get` that returns 2xx validates its body, so all existing tests check conformance for free.
    2. **A dedicated coverage test** (`tests/openapi_conformance.rs`): it runs a seeded workload with Connect on, every action and churn rate above zero, history and a webhook destination. It then checks every list, every retrieve, every expandable field expanded one level, every `/v1/events` and `/v2/core/events` payload and every delivered webhook body. It also asserts that every allowlisted type was served at least once, and that every `status` enum value the mock is documented to generate appeared.
  - The check doesn't depend on the payload's URL alone. It resolves the schema from the response's `object` field, and for lists from the item type.

### 6.4 Existing types (start of phase 7)

The existing 8 types are rewritten onto the generated structs before any new type is added:

| Type | Schema |
|---|---|
| customer | `customer` |
| product | `product` |
| price | `price` |
| subscription | `subscription` |
| subscription_item | `subscription_item` |
| invoice | `invoice` |
| invoice line item | `line_item` |
| account (v1) | `account` |
| account (v2) | `v2.core.account` |

Fields that today are missing, misnamed or wrongly typed get fixed as the compiler finds them. That is expected and is the point. Where the real schema has a field the mock can't populate meaningfully, it gets the schema's null or default, as today.

### 6.5 New entity types

The sync situation each type adds is the reason it is there. ID prefixes below are the usual Stripe ones; the builder confirms each against the spec's examples and fixtures, and a mismatch is fixed in favor of Stripe.

**Group 1: payments core (phase 7)**

| Type | Prefix | Removal style | Sync situation |
|---|---|---|---|
| `payment_intent` | `pi_` | `canceled` status | fast status churn (`requires_payment_method → processing → succeeded`, failures) |
| `charge` | `ch_` (`py_` on connected accounts for destination charges) | none (`refunded`, `disputed` flags) | updated by children (refunds, disputes) |
| `refund` | `re_` | `canceled` / `failed` status | appears standalone (`/v1/refunds`) and nested (`charge.refunds`, embedded in the pinned schema); `refund.created/updated/failed` events |
| `dispute` | `dp_` | `closed` statuses (`won`, `lost`, …) | lives for weeks of simulated time, with many updates; `charge.dispute.*` events |
| `balance_transaction` | `txn_` | none | **no events of its own**: found only by listing; `status` changes `pending → available` silently on `available_on` |
| `payment_method` | `pm_` | **detach** (`customer` becomes null, stays retrievable) | listing needs a customer (`/v1/payment_methods?customer=`, `/v1/customers/{id}/payment_methods`) |
| `setup_intent` | `seti_` | `canceled` status | creates payment methods |
| `checkout.session` | `cs_` | **expires** (`status: expired`) | dotted object name; `checkout.session.*` events; creates payment intent or subscription |
| `invoice_payment` | `inpay_` | none | the invoice-to-payment link in the pinned API (invoices list their `payments`); without it, invoice payment fan-out is wrong |

New fan-out for paying an invoice (replaces the single mutation in spec section 6.1), using the event names the pinned spec lists. Finalizing stays a separate step with today's timing; the payment flow starts where today's single payment mutation is:
1. `payment_intent.created`
2. `charge.succeeded`, with its `balance_transaction` (no event)
3. `payment_intent.succeeded`
4. `invoice_payment.paid`
5. `invoice.paid` and `invoice.payment_succeeded`

On failure (`payment_failure_probability`), the flow is `charge.failed` → `payment_intent.payment_failed` → `invoice.payment_failed`, followed by the existing retry or past-due logic.

New actions (spec section 4): the reserved keys `one_off_payment`, `refund` and `dispute` become real, and new keys are added:
- `one_off_payment`: a payment intent, with no invoice, that succeeds or fails.
- `refund`: full or partial, for a random succeeded charge.
- `dispute`: opened on a random charge; later updates and close follow a drawn timeline over days to weeks.
- `save_payment_method` (new key): a setup intent that attaches a payment method.
- `checkout` (new key): a session that completes or expires; some are in subscription mode.
- `[churn.payment_method] detach` (new key).

Endpoints: retrieve and list for each type, plus the sub-lists above. `balance_transaction` is listable at `/v1/balance_transactions`.

**Group 2: Connect money movement (phase 8)**

| Type | Prefix | Removal style | Sync situation |
|---|---|---|---|
| `transfer` | `tr_` | partial or full **reversal** | `transfer_reversal` is an embedded list and sub-list (`/v1/transfers/{id}/reversals`) |
| `transfer_reversal` | `trr_` | none | child |
| `application_fee` | `fee_` | **refund** | `fee_refund` children (`/v1/application_fees/{id}/refunds`); lives in the platform scope, describes connected-account charges |
| `fee_refund` | `fr_` | none | child |
| `payout` | `po_` | `canceled` / `failed` | own lifecycle (`pending → in_transit → paid`); creates balance transactions; runs on connected accounts too |
| external account: `bank_account`, `card` | `ba_`, `card_` | **hard delete** | polymorphic list (`/v1/accounts/{id}/external_accounts`); `account.external_account.*` events |
| `person` | `person_` | **hard delete** | sub-list only (`/v1/accounts/{id}/persons`); `person.*` events; changes `account.requirements` |

**Connect charge patterns** (decided by the user: patterns apply to platform payments).
- **Connected accounts keep direct charges.** Each connected account's generator keeps creating its own customers, invoices and payments in its own scope, which is a direct charge by construction. New config `accounts.application_fee = { probability, percent }` (default `{ probability = 0.0, percent = 10.0 }`) sets how often such a charge carries an `application_fee`, which lives on the platform.
- **The platform's own payments can route money to a connected account.** New config `accounts.platform_payments = { own, destination, separate }` is a mix that sums to 1 and defaults to `{ own = 1.0 }`. For each platform payment (invoice payment, `one_off_payment` and `checkout` alike), a pattern is drawn, and for a non-`own` pattern a random connected account is drawn as the recipient:
  - **own:** a plain platform charge, as without Connect.
  - **destination:** the charge carries `transfer_data.destination`. A `transfer` goes to the account, and a `py_` payment with its balance transaction lands in the account's scope.
  - **separate:** the charge carries a `transfer_group`. Later, after a drawn delay, an independent `transfer` with the same group goes to the account.
- **Symmetry holds.** Platform and connected accounts keep identical coverage of business actions. The only scope-specific objects are the transfers and `py_` payments caused by platform payments, and the application fees caused by connected-account charges.
- **With `connect = false`,** `platform_payments` other than `{ own = 1.0 }` and `application_fee.probability > 0` are config errors.
- **If no connected account is connected** when a non-`own` pattern is drawn, the payment falls back to `own`.

Each pattern's objects land in the right scope, and events go to the right destinations (spec section 5.4 rules unchanged).

New actions: `payout` (a natural daily schedule per account, like `renew_subscription_cycle = "natural"`), `transfer_reversal`, `application_fee_refund`, `[churn.person] update/delete` and `[churn.external_account] create/delete`.

**Group 3: billing extras (phase 9)**

| Type | Prefix | Removal style | Sync situation |
|---|---|---|---|
| `coupon` | (user-style IDs, e.g. random strings) | **hard delete** | `coupon.deleted`; referenced by discounts that outlive it |
| `promotion_code` | `promo_` | **deactivate only** (`active: false`) | no delete event |
| `discount` | `di_` | removed from its parent | embedded only (`customer.discount`, `subscription.discounts`), not a standalone list |
| `invoiceitem` | `ii_` | hard delete while pending | becomes an invoice line |
| `credit_note` | `cn_` | `void` | embedded `credit_note_line_item` list and sub-list |
| `credit_note_line_item` | `cnli_` | none | child |
| `tax_rate` | `txr_` | archive (`active: false`) | referenced from lines |
| `subscription_schedule` | `sub_sched_` | `released` / `canceled` / `completed` | drives subscription changes over phases |

New actions: `discount_change`, `credit_note`, `schedule_subscription` and `catalog_change` extended to coupons, promotion codes and tax rates.

**For every new type:** events for snapshot and thin destinations; Connect scoping identical for platform and connected accounts (spec section 5.4), except types that exist only on one side (`application_fee` on the platform, external accounts and persons as sub-resources of an account); history; faults; and latest-state subscription.

### 6.6 Spec updates

Each phase updates `docs/stripe-mock-spec.md` as it lands:
- section 4: config keys;
- section 5.1: types table (phase-2 candidates replaced by the real list);
- section 6.1: fan-out;
- section 7.2: endpoints;
- section 14: how generation and conformance are tested;
- section 16: phases 7, 8 and 9 as defined here.

## 7. Calls made after the builder check

- **The pinned schema wins over documented behavior.** If the generated type for an object disagrees with something the project spec documents or an existing test asserts, the mock follows the schema. The test and the project spec section are updated in the same task. There is no schema-override mechanism. A check of the vendored file found the shapes that looked most at risk all present: v1 `account.type` includes `none`, `account.controller` exists, `v2.core.account.closed` exists, `application` is `{id, name, object}`, and `deleted_account` is `{deleted, id, object}`.
- **Thin events** are generated from `v2.core.event`. The file has no separate `v1.*` thin-event schemas.
- **Event names** come from the 276-value `enabled_events` enum of `POST /v1/webhook_endpoints` in the vendored file. It includes `invoice_payment.paid` and `refund.created`, `refund.updated` and `refund.failed`.

## 8. Left to the builder

These are the builder's calls. The plan settles them and states them:
- Exact generator internals (module split, how names are de-collided, how to emit `Expandable`).
- Which clippy lints the generated module allows.
- The exact event lists per type. They come from the pinned spec's event enumeration (`enabled_events` of webhook endpoints, and the v2 event schemas), not from this document.
- Default rates for new actions and churn keys. They default to 0 (off), like other opt-in features, except where a type has to exist for fan-out (payment objects on invoice payment are always on).
- Realistic value draws (card brands, last4, bank names, dispute reasons) and timelines (dispute duration, payout arrival).
- How ID prefixes not fixed above are formed.
- How the work in each phase is split into tasks.
