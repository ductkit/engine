# Stripe Mock Phase 8 (Connect Money Movement) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mock gains Connect money movement: transfers and their reversals, application fees and fee refunds, payouts, external accounts (bank accounts and cards) and persons. Platform payments can follow the destination or separate-charges-and-transfers pattern, and connected-account charges can carry application fees.

**Architecture:**
- **New object types on the existing engine.** Truth log, scopes, events, faults, history and the generated types work as in phase 7. Each new type is a generated struct, built and changed only through `create_obj` / `update_obj`.
- **Money crosses scopes with `Engine::in_scope`.** Each object lives where Stripe keeps it:
  - Transfers, reversals, application fees and fee refunds live in the platform scope.
  - `py_` payments and `pyr_` payment refunds, external accounts and persons live in the connected account's scope.
  - Payouts live in the scope whose balance they pay out.
  - Code running in one scope writes another scope's objects inside `in_scope`, so the events of each object carry that scope's label, with no new routing rule.
- **The settlement clock learns scopes.** A queued settlement names the scope its balance transaction lives in. A platform payment can then settle a `py_` payment's balance transaction in the account's scope, and a connected-account charge can settle the platform's application-fee balance transaction.
- **Payouts sweep the available balance.** A daily schedule per scope pays out each currency's available, unswept balance. An engine-side index records which balance transactions each payout swept, and serves the `payout` filter of `/v1/balance_transactions`.
- **Default config draws nothing new.** Every new feature defaults off and draws only when on, so phase 7 seeds and configs replay identically.

**Tech Stack:** Unchanged from phase 7. No new dependencies.

**Spec:**
- The design: [docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md](../docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md), the binding authority for this plan. Phase 8 covers group 2 of section 6.5, the phase-8 parts of 6.6, and sections 7 and 8 as they apply.
- The project spec: [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md), sections 4, 5.1, 5.4, 5.5, 6.1, 7.2, 7.3, 11, 14 and 16.

## Global Constraints

- **Generated types:** every new object comes from the vendored `crates/stripe-mock/openapi/spec3.json`. Regenerate with `DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen`; never hand-edit `src/stripe/generated/`.
- **Only typed code writes objects.** Every new object is built and changed through the generated structs with `create_obj` / `update_obj` / `delete`. No `json!` for a served object.
- **Field presence (phase 7 ruling R1):** model code decides every field explicitly. No `Default` and no `..` struct update. A non-required field is present (a value or `null`) unless Stripe documents it as includable-only or expandable-only.
- **The pinned schema wins** over documented behavior or an existing test. Fix the test and the spec section in the same task, and ledger it as a ruling.
- **Determinism:**
  - Same seed and config give the same run.
  - New draws come from new streams only (names below).
  - New `ActionKind` and `ObjectType` variants are appended at the end.
  - **With every new key at its default, no new draw happens anywhere**, so a phase 7 config replays identically. Task 1's guard test pins it.
  - The one exception is the setup of a newly connected **controlled** account (Task 3, P4): it always gets its bank account and persons, as payment objects are always created on invoice payment. Runs with only Standard accounts are unchanged.
- **Config:** new keys default to off:
  - every rate `0.0`;
  - `actions.payout = "off"`;
  - `accounts.platform_payments = { own = 1.0, destination = 0.0, separate = 0.0 }`;
  - `accounts.application_fee = { probability = 0.0, percent = 10.0 }`.

  With `accounts.connect = false`, a non-default `platform_payments`, an `application_fee.probability > 0`, or any `[churn.person]` / `[churn.external_account]` rate above 0 is a config error naming the key. The spec's section 4 example TOML must keep parsing (the config test checks it).
- **Events** come from the pinned `enabled_events` enum:
  - `transfer.created`, `transfer.reversed`;
  - `application_fee.created`, `application_fee.refunded`;
  - `payout.created`, `payout.updated`, `payout.paid`, `payout.failed`, `payout.reconciliation_completed`;
  - `account.external_account.created`, `account.external_account.updated`, `account.external_account.deleted`;
  - `person.created`, `person.updated`, `person.deleted`.

  Thin `v1.` events follow from the projector as for every type.
- **Connect rules unchanged** (spec 5.4):
  - Nothing new happens in a disconnected account's scope except settlement.
  - Account endpoints, including the new sub-resources, refuse a `Stripe-Account` / `Stripe-Context` header with the existing 400.
- **Process:**
  - `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` run as separate steps before each commit, with `-j 2` on Windows if the paging file runs out (os error 1455).
  - Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
  - Branch `stripe-mock-phase-8` in the existing checkout. Never use a worktree.
- **Windows:** LF line endings. Python edits use `newline=''`. `grep -c $'\r'` is 0 for every edited file.

## Rulings made while planning

Section 8 of the design leaves these to the builder. Each is stated once here; tasks refer to them by number.

- **P1, where objects live and who emits.** Each object lives in the scope listed in the Architecture section, and its events carry that scope's label:
  - Transfer and fee events are unlabelled platform events.
  - `account.external_account.*` and `person.*` are labelled with the account.
  - `py_` payments, `pyr_` payment refunds and every balance transaction emit no event (as phase 7's balance transactions don't).
  - Cost if wrong: a consumer expecting a `charge.succeeded` for a `py_` payment in the account's scope never sees one. They are found by listing, which is the sync situation this type adds.
- **P2, cross-scope expansion.** Stripe lets a platform expand a transfer's `destination_payment` even though it lives in the account. `expand` gains a fixed table of foreign-scope paths. Each lookup reads the other scope's store through the same `View` time (stale reads included):

  | Type.path | Target scope |
  |---|---|
  | `transfer.destination_payment` | `transfer.destination` |
  | `transfer_reversal.destination_payment_refund` | the reversal's transfer's `destination` |
  | `application_fee.charge` | `application_fee.account` |
  | `charge.application_fee` (direct charge) | platform |
  | `charge.source_transfer` (`py_`) | platform |
  | `refund.source_transfer_reversal` (`pyr_`) | platform |

  Any other path resolves in the request's scope, as today. Cost if wrong: an extra resolvable path Stripe refuses, which is harmless for a sync user.
- **P3, ID prefixes.**

  | Type | Prefix | Note |
  |---|---|---|
  | `transfer` | `tr` | |
  | `transfer_reversal` | `trr` | |
  | `application_fee` | `fee` | |
  | `fee_refund` | `fr` | |
  | `payout` | `po` | |
  | `bank_account` | `ba` | |
  | `card` (external account) | `card` | |
  | `person` | `person` | |
  | destination payment | `py` | a `charge` |
  | its refund | `pyr` | a `refund` |

  `expand::type_of_id` maps `py_` to `Charge` and `pyr_` to `Refund` (longest prefix wins). Confirm each against Stripe's API reference examples before committing Task 2, and ledger any change.
- **P4, which accounts have external accounts and persons.** Only accounts the platform controls (`controller.is_controller: true`: Express, Custom and v2). That matches the phase 7 rule for `external_accounts`. A Standard account and the platform have none; their payouts name a `ba_` destination drawn once per scope that isn't in any store, so expanding it leaves the ID. Cost if wrong: Standard accounts' bank accounts aren't listable, which is true in Stripe too.
- **P5, payout lifecycle** (`actions.payout = "daily"`). Draws come from stream `{prefix}generate.payout`; IDs from `generate.payments`.
  - **Created:** at each simulated midnight UTC, for each currency whose available, unswept balance is above 0, in currency order:
    - the payout: `pending`, `automatic: true`, `method: standard`, `type: bank_account`, `source_type: card`, `arrival_date` = that midnight + 2 days, `reconciliation_status: in_progress` (`payout.created`);
    - its balance transaction: `type: payout`, `-amount`, fee 0, `available` at once, `source` the payout.

    Every available balance transaction not yet swept in that currency is recorded as swept by this payout.
  - **One hour later:** `in_transit` (`payout.updated`).
  - **At `arrival_date`:** either
    - `paid`, `reconciliation_status: completed` (`payout.paid`, `payout.updated`, `payout.reconciliation_completed`);
    - or, with `actions.payout_failure_probability` (drawn at creation), `failed`, with `failure_code: account_closed`, the failure message `The bank account has been closed.`, and a `failure_balance_transaction` (`type: payout_failure`, `+amount`, `available` at once) (`payout.failed`, `payout.updated`).
  - **Disconnect:** the in-transit and arrival steps are continuations, so a disconnect drops them like a dispute's. The payout keeps its last status, because no events may be created in that scope.
  - **Not generated:** `canceled` (Stripe cancels only manual payouts) and instant payouts.
  - Cost if wrong: a consumer that relies on `payout.updated` timing sees a fixed one-hour gap.
- **P6, balance and currency.**
  - Balances are kept per currency, with no conversion.
  - A balance transaction's currency is its source's currency. This settles the phase 7 deferred minor "BT currency = charge currency" as the intended behavior.
  - The JPY flat-fee minor stays deferred.
  - Stripe's `balance.available` event is not emitted (the `balance` object is not a modelled type).
  - Cost if wrong: multi-currency platforms see payouts per presentment currency.
- **P7, charge pattern details.**
  - The pattern is drawn once per payment intent, when it is created, from stream `generate.platform_payments` (platform generator only). The recipient is uniform over `engine.connected_ids()`, and with none connected the pattern falls back to `own`.
  - **Destination:**
    - The intent and charge carry `transfer_data { amount: null, destination }` and `transfer_group: "group_{pi}"`.
    - On success, the full amount transfers, and the charge's `transfer` is set.
  - **Separate:**
    - The intent and charge carry `transfer_group: "group_{pi}"` only.
    - On success, a continuation `SeparateTransfer` runs 1–72 hours later (uniform, drawn at payment time). It transfers the full amount with `source_transaction` set to the charge and the same `transfer_group`.
    - If the recipient has disconnected by then, no transfer is made.
  - Retries reuse the intent's pattern.
  - `invoice.transfer_data` stays `null`, because the pattern belongs to the payment, not the invoice.
  - Cost if wrong: a subscription invoice under destination doesn't show its destination.
- **P8, persons and requirements.**
  - At connect, a controlled account gets 1–3 persons. The first has `relationship.representative` and `executive` set.
  - Deleting the account's last representative sets the account's `requirements.currently_due` and `eventually_due` to `["representative.first_name", "representative.last_name"]` (`account.updated`, labelled with the account).
  - Persons are never created later, so a removed requirement is never cleared.
  - The persons sub-list takes `limit` and cursors only; the `relationship[...]` filters are a 400 `parameter_unknown`.
  - Cost if wrong: requirement churn is one-way.
- **P9, the account's embedded `external_accounts`.** It is kept in step with the account's external accounts: newest default first, then by creation, truncated to `nested_list_page_size`. Each change is a silent account mutation (`events = &[]`), so only `account.external_account.*` announces it. Cost if wrong: a consumer diffing `account.updated` never sees the list change, which matches Stripe.

## Review Focus

1. **Money doesn't add up across scopes.** Each of these must hold:
   - `transfer.amount` equals its `py_` payment's amount;
   - `transfer.amount_reversed` equals the sum of its reversals and the `py_` payment's `amount_refunded`;
   - an application fee's amount equals its charge's `application_fee_amount`, and appears in that charge's balance-transaction `fee`;
   - a payout's amount equals the sum of the `net` of the balance transactions it swept;
   - per scope and currency, the sum of every balance transaction's `net` equals the sum over the unswept, non-payout ones (Task 8).
   - no payout is created for a balance at or below 0.

   Test `money_moves_consistently` (Task 9).
2. **An object or event lands in the wrong scope** (P1). Examples: a `py_` payment in the platform, a transfer event labelled with the account, a person in the platform store. Test `connect_objects_land_in_their_scope` (Task 6).
3. **An account disconnects mid-flow.** A separate transfer is still queued for it, a payout is in transit, and an application-fee balance transaction is pending. Expected: no panic, no event in its scope after the disconnect, the platform's pending balance transactions still settle, and the queued transfer is skipped. Test `disconnect_mid_money_flow` (Task 8).
4. **The polymorphic external-account list.** Mixed `bank_account` and `card` pages must paginate by cursor with no gaps or repeats. `object=card` filters, a deleted account drops out, and the default one is never deleted. Test `external_account_list_is_polymorphic` (Task 3).
5. **A balance transaction is swept twice, or the `payout` filter is wrong.** `GET /v1/balance_transactions?payout=po_…` must return exactly that payout's swept transactions, including under `pagination_writes`. No transaction may appear in two payouts. Test `payout_sweeps_each_transaction_once` (Task 8).

---

### Task 1: Generate the phase 8 types

**Files:**
- Modify: `tests/codegen/mod.rs` (`ROOTS`), `tests/codegen/ir.rs` (closure-count test), `src/stripe/generated/` (regenerated), `src/model/account.rs` and `src/model/payments.rs` (whatever the regenerated types break)
- Test: `tests/openapi_codegen.rs`, `tests/replay.rs`

**Interfaces:**
- Produces: the generated structs `Transfer`, `TransferReversal`, `ApplicationFee`, `FeeRefund`, `Payout`, `BankAccount`, `DeletedBankAccount`, `Card`, `DeletedCard`, `Person` and `DeletedPerson`.
- Fields that were `IdOnly` become `Expandable<…>` because their targets are now roots. Examples: `charge.transfer`, `charge.application_fee`, `charge.source_transfer`, and the new variants of `balance_transaction.source`. `account.external_accounts` items become the generated bank-account/card union.

- [ ] **Step 1: Write the guard test `default_config_replays_phase_7_run`** in `tests/replay.rs`. Use seed 11 with every phase 7 rate on and `connect = true`, with 2 initial accounts and `account_types = { standard = 1.0, express = 0.0, custom = 0.0 }` (controlled accounts get setup objects, per the exception above). Step 2 days, and assert the truth log's SHA-256 equals a constant recorded from `main` at this plan's base commit. Run it on the base commit first to get the constant. It pins the "default config draws nothing new" constraint for every later task.
- [ ] **Step 2: Append the roots** to `ROOTS`: `transfer, transfer_reversal, application_fee, fee_refund, payout, bank_account, deleted_bank_account, card, deleted_card, person, deleted_person`.
  - Rename `phase_7_roots_resolve` to `roots_resolve`. Run it and expect FAIL on the count.
  - Set the count to what the unchanged closure rule now yields, and put the old and new counts in the commit body.
- [ ] **Step 3: Regenerate.** Run `DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen`, then `cargo build -p ductkit-stripe-mock`. Fix what stops compiling in the model code, keeping each changed field's current value (an `IdOnly` string becomes `Expandable::Id`).
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the guard test.
- [ ] **Step 5: Record the clean build time** (`cargo clean -p ductkit-stripe-mock && cargo build -p ductkit-stripe-mock`) before and after, in the commit body and the ledger.
- [ ] **Step 6: Commit** with the message `feat(stripe-mock): generate Connect money movement types`.

### Task 2: Object types, stores and read endpoints

**Files:**
- Modify: `src/model/mod.rs`, `src/engine/store.rs` (`parent_fields`), `src/api/resources.rs`, `src/api/accounts.rs` (account sub-resources), `src/api/mod.rs` (routes), `src/api/expand.rs` (allowed paths, P2 and P3), `src/generate/mod.rs` (`pagination_write` arms)
- Create: `src/model/connect.rs` (constructors for the new types, filled in by later tasks)
- Test: `tests/api_connect_money.rs`, `tests/expand.rs`

**Interfaces:**
- Produces:
  - `ObjectType` gains, appended in this order: `Transfer`, `TransferReversal`, `ApplicationFee`, `FeeRefund`, `Payout`, `BankAccount`, `Card`, `Person`. `ALL` has length 25.
    - `as_str`: `transfer`, `transfer_reversal`, `application_fee`, `fee_refund`, `payout`, `bank_account`, `card`, `person`.
    - Prefixes are from P3.
    - `api_path` returns `/v1/transfers/{id}`, `/v1/application_fees/{id}` and `/v1/payouts/{id}`, and `None` for the four child types.
  - `pub fn sub_path(&self, parent: &str, id: &str) -> Option<String>` on `ObjectType`:
    - `TransferReversal` → `/v1/transfers/{parent}/reversals/{id}`
    - `FeeRefund` → `/v1/application_fees/{parent}/refunds/{id}`
    - `BankAccount` and `Card` → `/v1/accounts/{parent}/external_accounts/{id}`
    - `Person` → `/v1/accounts/{parent}/persons/{id}`
    - any other type → `None`
  - `parent_fields`: `TransferReversal` → `["transfer"]`, `FeeRefund` → `["fee"]`, `Transfer` → `["destination"]`, `ApplicationFee` → `["account"]`.
  - Resources, with filters beyond `limit`, the cursors, `created` and `expand[]`:

    | Endpoint | Filters | Scope it reads |
    |---|---|---|
    | `GET /v1/transfers` | `destination`, `transfer_group` | request scope |
    | `GET /v1/transfers/{id}/reversals` (+ `/{rid}`) | — | request scope |
    | `GET /v1/application_fees` | `charge` | request scope |
    | `GET /v1/application_fees/{id}/refunds` (+ `/{rid}`) | — | request scope |
    | `GET /v1/payouts` | `status`, `destination` | request scope |
    | `GET /v1/accounts/{acct}/external_accounts` (+ `/{id}`) | `object` (`bank_account` or `card`) | the account's scope, platform requests only |
    | `GET /v1/accounts/{acct}/persons` (+ `/{id}`) | — | the account's scope, platform requests only |

    - `GET /v1/charges` also accepts `transfer_group`.
    - `GET /v1/balance_transactions?payout=` reads the engine's payout index (Task 8). Until then it matches nothing, as in phase 7.
    - An unknown or disconnected `{acct}` in the account sub-resources is a 404 `resource_missing` naming the account, as `/v1/accounts/{id}` is.
    - Sub-lists list in creation order, ascending (spec 7.3).
  - `expand::allowed`: each new type's modelled expandable fields, plus `charge.transfer`, `charge.application_fee`, `charge.source_transfer`, `charge.transfer_data.destination`, `refund.source_transfer_reversal` and `refund.transfer_reversal`.
  - `fn foreign_scope(view: &View, t: ObjectType, obj: &Value, path: &str) -> Option<Scope>` in `expand.rs` implements P2. When the result is `Some`, `lookup` reads that scope's store with the view's `at`.
  - `pagination_write`:
    - `Transfer` and `TransferReversal` → `TransferReversal`;
    - `ApplicationFee` and `FeeRefund` → `ApplicationFeeRefund`;
    - `BankAccount` and `Card` → `ExternalAccountCreate`;
    - `Person` → `PersonUpdate`;
    - `Payout` → no write.

    The kinds arrive with their tasks; until then these arms return early.

- [ ] **Step 1: Write the failing routing tests** in `tests/api_connect_money.rs`:
  - `new_lists_are_empty_and_well_formed`: each new top-level list, and each sub-list on a connected Custom account, returns 200 `object: list` and `data: []`, with `url` equal to the bare path.
  - `unknown_ids_404`: `GET /v1/transfers/tr_nope` and `GET /v1/accounts/{acct}/persons/person_nope` give 404 `resource_missing`. So does `GET /v1/accounts/acct_nope/external_accounts`.
  - `account_sub_resources_refuse_account_header`: a `Stripe-Account` header on `/v1/accounts/{acct}/persons` gives the existing 400.
  - `new_endpoints_are_read_only`: a POST to each new path is a 400.
  - Plus `py_and_pyr_ids_resolve_by_prefix` as a unit test in `expand.rs`.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test api_connect_money`. Expected: FAIL.
- [ ] **Step 3: Implement the variants, stores, routes, filters and expand tables.** Fix every exhaustive match the compiler lists.
- [ ] **Step 4: Run the full suite.** Expected: PASS.
- [ ] **Step 5: Commit** with the message `feat(stripe-mock): Connect money object types and read endpoints`.

### Task 3: External accounts and persons

**Files:**
- Modify: `src/model/connect.rs`, `src/model/account.rs` (embedded `external_accounts`, requirements), `src/engine/mod.rs` (`update_account_with`), `src/generate/mux.rs` (setup at connect), `src/generate/mod.rs` (kinds, actions), `src/config.rs` (`[churn.person]`, `[churn.external_account]`), `docs/stripe-mock-spec.md` (sections 4, 5.4 and 11)
- Create: `src/generate/connect.rs` (external-account and person flows; Tasks 4–8 add theirs here too)
- Test: `tests/external_accounts.rs`, `src/config.rs` unit tests

**Interfaces:**
- Produces:
  - Config:
    - `ChurnConfig.person: PersonChurn { update: f64, delete: f64 }`
    - `ChurnConfig.external_account: ExternalAccountChurn { create: f64, delete: f64 }`
  - `ActionKind` appends `ExternalAccountCreate`, `ExternalAccountDelete`, `PersonUpdate` and `PersonDelete`. Streams: `{prefix}generate.external_account.create`, `.external_account.delete`, `.person.update` and `.person.delete`.
  - `Action` gains:
    - `CreateExternalAccount { kind: Option<ExternalAccountKind> }`
    - `DeleteExternalAccount { id: Option<String> }`
    - `UpdatePerson { id: Option<String> }`
    - `DeletePerson { id: Option<String> }`
    - `pub enum ExternalAccountKind { BankAccount, Card }` (serde snake_case)

    Run in the platform scope, or on an account the platform doesn't control, each one is a 409 `invalid_action` naming the scope.
  - `pub(crate) fn update_account_with(&mut self, id: &str, f: impl FnOnce(&mut stripe::Account), events: &[&str]) -> Result<(), MockError>` on `Engine`. It runs in the platform scope, and the cause is `account_update`.
  - `Generator::on_connect(&mut self, engine: &mut Engine, kind: AccountKind)` runs once, in the account's scope, right after the account connects. It draws from `{prefix}generate.account_setup`. For a controlled account (P4), under the cause `account_setup`, it creates:
    1. one `bank_account` (`account_holder_type: company`, `bank_name` uniform from `STRIPE TEST BANK`, `JPMORGAN CHASE`, `WELLS FARGO` and `BANK OF AMERICA`, `last4` four random digits, `routing_number: "110000000"`, `country: US`, `currency: usd`, `status: new`, `default_for_currency: true`, `fingerprint` 16 random alphanumerics) (`account.external_account.created`);
    2. 1–3 persons, with `first_name` and `last_name` from fixed lists and `email` `{first}.{last}@example.com` (lowercase); the first is the representative and executive (P8) (`person.created` each);
    3. the account's embedded list updated (P9).
- External-account churn, on controlled accounts:
  - **Create:** a debit `card` with p = 0.3 (`brand` visa or mastercard, `funding: debit`, `currency: usd`, expiry 1–5 years ahead), otherwise a `bank_account`. A new bank account becomes the default with p = 0.3: the old default first gets `default_for_currency: false` (`account.external_account.updated`), then the new one is created (`account.external_account.created`).
  - **Delete:** a random non-default external account, hard-deleted with its `deleted_bank_account` / `deleted_card` tombstone (`account.external_account.deleted`). With none, nothing happens.
  - **Person update:** a new `email` and `phone` (`person.updated`).
  - **Person delete:** hard delete (`person.deleted`), then the P8 requirements change if no representative is left.

- [ ] **Step 1: Write the failing tests** in `tests/external_accounts.rs`:
  - `controlled_account_starts_with_bank_account_and_persons`: a Custom account connects. Its sub-lists show one default bank account and 1–3 persons. `account.external_accounts.data[0]` equals the bank account. The events are labelled with the account, in order: `account.external_account.created`, then one `person.created` per person.
  - `standard_account_has_none`: a Standard account's sub-lists are empty, and no setup events are emitted.
  - `external_account_list_is_polymorphic` (Review Focus 4): script 8 creates with mixed kinds and 2 deletes. Paginate with `limit=3` by cursor: every live external account appears exactly once, no deleted one appears, and `object=card` returns only cards. The default is never deleted, even with only defaults left (the delete is then a 409 naming the account).
  - `new_default_bank_account_updates_old_default`
  - `deleting_representative_sets_requirements`: an `account.updated` event follows, with `requirements.currently_due` set as P8 says.
  - `external_account_actions_refuse_platform_scope`
  - Plus config unit tests: `person_and_external_account_churn_need_connect` and `churn_keys_default_to_zero`.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement them.** Spec updates:
  - section 4: the new churn keys in the example TOML and rules;
  - section 5.4: external accounts and persons, P4, P8 and P9;
  - section 11: the new actions.
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the replay guard.
- [ ] **Step 5: Commit** with the message `feat(stripe-mock): external accounts and persons`.

### Task 4: Settlement queue with scopes

**Files:**
- Modify: `src/generate/mod.rs` (`Ctx.settlements`, `schedule_settlement`, `take_settlements`), `src/generate/mux.rs` (`orphaned`), `src/generate/payments.rs` (callers)
- Test: unit tests in `src/generate/mux.rs` (`#[cfg(test)]`). No public action can create a foreign-scope settlement until Task 5.

**Interfaces:**
- Produces:
  - `pub(crate) fn schedule_settlement(&mut self, at: SimTime, scope: Option<Scope>, id: String, cause: Cause)`: `scope` `None` means the generator's own scope; `Some(s)` means scope `s`, where `Some(None)` is the platform.
  - The settlement runs `make_available` inside `engine.in_scope` for a foreign scope.
  - `Generators.orphaned` stores the resolved `Scope` instead of the account ID.
  - Every phase 7 caller passes `None`.

- [ ] **Step 1: Write the failing unit test `foreign_scope_settlement_runs_in_its_scope`** in `src/generate/mux.rs`. A platform generator schedules a settlement for a balance transaction created in a connected scope. At `available_on` it is `available` in that scope, with no event. Disconnecting that account first changes nothing.
- [ ] **Step 2: Run it.** Expected: FAIL (compile).
- [ ] **Step 3: Implement it.**
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the replay guard.
- [ ] **Step 5: Commit** with the message `refactor(stripe-mock): settlements name their scope`.

### Task 5: Application fees and fee refunds

**Files:**
- Modify: `src/config.rs` (`AccountsConfig.application_fee`, `ActionsConfig.application_fee_refund`), `src/model/connect.rs`, `src/model/payments.rs` (charge balance-transaction fee details), `src/generate/payments.rs` (`create_payment_intent`, `confirm`), `src/generate/connect.rs`, `src/generate/mod.rs`, `docs/stripe-mock-spec.md` (sections 4, 5.5 and 6.1)
- Test: `tests/application_fees.rs`, `src/config.rs` unit tests

**Interfaces:**
- Produces:
  - `AccountsConfig.application_fee: ApplicationFeeConfig { probability: f64, percent: f64 }`. `percent` must be in `(0, 100]`, and an out-of-range value is a config error.
  - `ActionsConfig.application_fee_refund: f64` (stream `generate.application_fee_refund`). `ActionKind::ApplicationFeeRefund` is appended. Add `Action::ApplicationFeeRefund { fee: Option<String>, amount: Option<i64> }`; a named fee with nothing left is a 409 naming it.
  - **The draw.** When a connected-account generator creates a payment intent and `probability > 0`, it draws from `{prefix}generate.application_fee` whether the payment carries a fee. There is no draw at probability 0.
    - The fee is `round_half_up(amount × percent / 100)`, and none if that is 0. It is stored as the intent's `application_fee_amount`.
- **Mutations of a successful direct charge with a fee**, after the steps of spec 6.1:
  1. The charge's balance transaction gets `fee` += the fee, a second `fee_details` entry (`type: application_fee`, `application: ca_…`), and `net` reduced to match. It is created that way, not updated.
  2. The charge carries `application_fee_amount`, and `application_fee` set to the fee's pre-drawn ID.
  3. After `payment_intent.succeeded`, in the platform scope, under the same cause:
     - the fee's balance transaction: `type: application_fee`, `+fee`, fee 0, `pending` with the charge's `available_on`, its settlement scheduled with scope `Some(None)`;
     - the `application_fee`: `account`, `application`, `charge`, `amount`, `currency`, `refunds` an empty embedded list with URL `/v1/application_fees/{id}/refunds`, `fee_source { charge, type: charge }`, `originating_transaction: null` (`application_fee.created`).
- **Fee refund** (platform scope only; in a connected scope, nothing happens): a random fee with something left. It refunds all of it with p = 0.7, otherwise a uniform amount in `[1, left]`.
  1. platform balance transaction: `type: application_fee_refund`, `-amount`, `available` at once;
  2. the `fee_refund` (no event of its own);
  3. the fee: `amount_refunded`, `refunded`, and the refund first in `refunds` (newest first, truncated to the page size) (`application_fee.refunded`);
  4. in the account's scope: a balance transaction, `type: application_fee_refund`, `+amount`, `available` at once (no event).

  A disconnected account's fees are not refundable.

- [ ] **Step 1: Write the failing tests** in `tests/application_fees.rs`:
  - `direct_charge_carries_application_fee`: `application_fee = { probability = 1.0, percent = 10.0 }` and one connected account. A 1000-unit one-off payment in the account's scope gives `charge.application_fee_amount == 100` and a balance transaction with `fee == card_fee(1000) + 100`. The platform has the fee, which `GET /v1/application_fees?charge=` finds, plus a `pending` balance transaction of +100. `application_fee.created` is unlabelled.
  - `application_fee_charge_expands_across_scopes` (P2): from the platform.
  - `platform_charges_never_carry_fees`
  - `fee_refund_partial_then_full`
  - `fee_balance_transaction_settles_after_disconnect`
  - Plus config unit tests: `application_fee_needs_connect` and `application_fee_percent_bounds`.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement them.** Update spec sections 4, 5.5 and 6.1.
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the replay guard.
- [ ] **Step 5: Commit** with the message `feat(stripe-mock): application fees on direct charges and fee refunds`.

### Task 6: Destination and separate charges with transfers

**Files:**
- Modify: `src/config.rs` (`AccountsConfig.platform_payments`), `src/model/connect.rs` (transfer, `py_` payment), `src/model/payments.rs` (`transfer_data` / `transfer_group` on intents and charges), `src/generate/payments.rs`, `src/generate/connect.rs`, `src/generate/mod.rs` (`Continuation::SeparateTransfer`, the `Action::OneOffPayment` fields), `docs/stripe-mock-spec.md` (sections 4, 5.5, 6.1 and 11)
- Test: `tests/charge_patterns.rs`, `src/config.rs` unit tests

**Interfaces:**
- Produces:
  - Config:
    - `AccountsConfig.platform_payments: PlatformPaymentsMix { own: f64, destination: f64, separate: f64 }`, a mix validated like `account_api`;
    - `pub enum ChargePattern { Own, Destination, Separate }`, serde snake_case.
  - `Action::OneOffPayment` gains `pattern: Option<ChargePattern>` and `recipient: Option<String>`, both serde-defaulted to `None`, so existing JSON scripts still parse.
    - A given pattern skips the draw.
    - A non-`own` pattern with no connected account, or a recipient that isn't connected, is a 409 `invalid_action`.
    - Either field in a connected scope is a 409.
  - `Continuation::SeparateTransfer { charge: String, account: String, group: String }`
  - `pub(crate) fn transfer_to(engine: &mut Engine, ctx: &mut Ctx, account: &str, charge: &str, amount: i64, currency: &str, group: &str, cause: &Cause) -> String`: the shared transfer core, which returns the transfer ID.
- **Mutations of `transfer_to`**, each under the payment's cause, all in the same instant:
  1. platform balance transaction: `type: transfer`, `-amount`, fee 0, `available` at once;
  2. the `transfer`: `destination`, `destination_payment` (pre-drawn `py_`), `source_transaction` (the charge), `source_type: card`, `transfer_group`, `reversals` an empty embedded list with URL `/v1/transfers/{id}/reversals`, `reversed: false`, `amount_reversed: 0` (`transfer.created`);
  3. in the account's scope: the `py_` payment's balance transaction (`type: payment`, `+amount`, fee 0, `pending`, `available_on` + 2 days), its settlement scheduled with scope `Some(Some(account))`;
  4. the `py_` charge:
     - `status: succeeded`, `paid`, `captured`;
     - `source_transfer` the transfer;
     - `payment_method_details { type: stripe_account }`;
     - `customer`, `payment_intent`, `payment_method` and `outcome` all `null`;
     - no event (P1).
- **Destination:** after `payment_intent.succeeded` and before any `invoice_payment.paid`. The charge's `transfer` is set in the same instant, as a silent charge update.

- [ ] **Step 1: Write the failing tests** in `tests/charge_patterns.rs`:
  - `destination_charge_moves_money`: `platform_payments = { destination = 1.0 }` with one connected account.
    - A one-off payment's events in order: `payment_intent.created`, `charge.succeeded`, `payment_intent.succeeded`, `transfer.created`, all unlabelled.
    - The account's `GET /v1/charges` (with `Stripe-Account`) lists the `py_` payment, whose `source_transfer` expands to the transfer.
    - `transfer.destination_payment` expands from the platform (P2).
  - `separate_charge_transfers_later`: the charge has `transfer_group` and no `transfer_data`. No transfer exists until the continuation, 1–72 hours later; then `GET /v1/transfers?transfer_group=` finds it.
  - `separate_transfer_skipped_after_disconnect`
  - `no_connected_account_falls_back_to_own`
  - `invoice_payments_follow_patterns`: a platform subscription's paid invoice under `destination = 1.0` produces a transfer.
  - `connect_objects_land_in_their_scope` (Review Focus 2): with destination, separate and application fees all on, for every new object type, it lives only in its P1 scope, and every event's label matches.
  - Plus config unit tests: `platform_payments_mix_must_sum_to_one` and `platform_payments_need_connect`.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement them.** Update spec sections 4, 5.5, 6.1 and 11.
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the replay guard.
- [ ] **Step 5: Commit** with the message `feat(stripe-mock): destination and separate charges with transfers`.

### Task 7: Transfer reversals

**Files:**
- Modify: `src/config.rs` (`ActionsConfig.transfer_reversal`), `src/model/connect.rs`, `src/generate/connect.rs`, `src/generate/mod.rs`, `docs/stripe-mock-spec.md` (sections 4 and 11)
- Test: `tests/charge_patterns.rs`

**Interfaces:**
- Produces:
  - `ActionsConfig.transfer_reversal: f64` (stream `generate.transfer_reversal`). `ActionKind::TransferReversal` is appended.
  - `Action::TransferReversal { transfer: Option<String>, amount: Option<i64> }`. A named transfer with nothing left, or to a disconnected account, is a 409 naming it.
- **Mutations** (platform scope; in a connected scope, nothing happens). The target is a random transfer with something left whose destination is still connected. It reverses all of it with p = 0.7, otherwise a uniform amount in `[1, left]`:
  1. platform balance transaction: `type: transfer_refund`, `+amount`, `available` at once;
  2. the `transfer_reversal`: `transfer`, `balance_transaction`, `destination_payment_refund` (pre-drawn `pyr_`), `source_refund: null`;
  3. the transfer: `amount_reversed`, `reversed` once fully reversed, the reversal first in `reversals` (`transfer.reversed`);
  4. in the account's scope: the `pyr_` refund's balance transaction (`type: payment_refund`, `-amount`, `available` at once);
  5. the `pyr_` refund (`succeeded`, `charge` the `py_` payment, `source_transfer_reversal` the reversal);
  6. the `py_` payment's `amount_refunded` and `refunded`.

  Steps 4–6 emit no events (P1).

- [ ] **Step 1: Write the failing tests:**
  - `partial_then_full_reversal`: the amounts across transfer, reversals and `py_` payment agree. A third reversal is a 409.
  - `reversal_events_and_scopes`: `transfer.reversed` is unlabelled, and the `pyr_` refund appears in the account's `GET /v1/refunds`.
  - `reversal_refund_expands_across_scopes` (P2)
  - `transfers_to_disconnected_accounts_are_not_reversed`
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement them.** Update spec sections 4 and 11.
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the replay guard.
- [ ] **Step 5: Commit** with the message `feat(stripe-mock): transfer reversals`.

### Task 8: Payouts

**Files:**
- Modify:
  - `src/config.rs` (`ActionsConfig.payout: PayoutMode`, `ActionsConfig.payout_failure_probability`)
  - `src/engine/mod.rs` (payout index)
  - `src/api/resources.rs` (`payout` filter)
  - `src/model/connect.rs`
  - `src/generate/connect.rs`
  - `src/generate/mod.rs` (`Continuation::PayoutDue`, `PayoutInTransit { id }`, `PayoutArrive { id, fail: bool }`, `Action::Payout`)
  - `docs/stripe-mock-spec.md` (sections 4, 5.5, 7.2 and 11)
- Test: `tests/payouts.rs`

**Interfaces:**
- Produces:
  - `pub enum PayoutMode { Off, Daily }` (serde lowercase, default `Off`).
  - `Action::Payout` (control only): pays out every currency of the scope's balance now, as the midnight run does.
  - `Engine` methods:
    - `pub(crate) fn record_payout(&mut self, payout: &str, swept: Vec<String>)`, keyed by the current scope;
    - `pub(crate) fn payout_members(&self, scope: &Scope, payout: &str) -> &[String]`;
    - `pub(crate) fn swept(&self, scope: &Scope, txn: &str) -> bool`.

    The index is derived truth: rebuilt by replay, not exported separately.
  - `Continuation::PayoutDue` reschedules itself at the next midnight. Turning `payout` to `daily` at runtime schedules the next midnight, and `off` drops the pending `PayoutDue`.
- The P5 lifecycle, P4 destinations and P6 balance rules.
  - A scope's available balance in a currency is the sum of `net` over its `available` balance transactions in that currency that aren't swept, in creation order.
  - A payout's own balance transaction (`type: payout`) is the sweep itself, so it never counts toward a balance and is never swept.
  - A failure's `+amount` balance transaction counts like any other, so the amount is paid out again the next day.
  - So, per scope and currency, the sum of `net` over all balance transactions equals the sum over those that are neither swept nor `type: payout`.

- [ ] **Step 1: Write the failing tests** in `tests/payouts.rs`:
  - `daily_payout_lifecycle`: `payout = "daily"`, with one 10000-unit one-off payment on the platform.
    - Step 3 days. One payout appears at the first midnight after its balance transaction became available. Its amount is the balance transaction's `net`.
    - Statuses and events in order: `payout.created`, then `payout.updated` (`in_transit`) an hour later, then `payout.paid`, `payout.updated` and `payout.reconciliation_completed` at `arrival_date`.
  - `failed_payout_returns_funds`: `payout_failure_probability = 1.0`. The payout ends `failed` with a `payout_failure` balance transaction, and the next midnight pays the amount out again.
  - `payout_sweeps_each_transaction_once` (Review Focus 5): many payments over 5 days, with `pagination_writes = { probability = 1.0 }`.
    - For each payout, `GET /v1/balance_transactions?payout=` paginated returns exactly its members.
    - No transaction appears in two payouts.
    - Each payout's amount is the sum of its members' `net`.
  - `connected_account_payouts_use_default_bank_account`: a Custom account's payout `destination` expands to its default bank account. A Standard account's stays an ID.
  - `disconnect_mid_money_flow` (Review Focus 3):
    - Setup: `separate = 1.0`, `application_fee.probability = 1.0`, `payout = "daily"`, with one Custom account mid-flow: a separate transfer queued, a payout in transit, and a fee's balance transaction pending.
    - Disconnect, then step 4 days.
    - Expected: no event labelled with the account after the disconnect; the platform's fee balance transaction settles; the account's in-transit payout stays `in_transit`; no transfer to it is made.
  - `payouts_off_by_default`
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement them.** Update spec sections 4, 5.5, 7.2 (the `payout` filter) and 11.
- [ ] **Step 4: Run the full suite.** Expected: PASS, including the replay guard.
- [ ] **Step 5: Commit** with the message `feat(stripe-mock): daily payouts with sweep index`.

### Task 9: Conformance coverage, money consistency, spec sweep

**Files:**
- Modify: `tests/openapi_conformance.rs`, `docs/stripe-mock-spec.md` (sections 5.1, 5.5, 7.2, 14, 16 and 17), `docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md` (only where the build differs, each change ledgered)
- Test: `tests/openapi_conformance.rs`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Extend the tests in `tests/openapi_conformance.rs`:**
  - `busy_connect_run_conforms`:
    - Turn on:
      - every new rate;
      - `payout = "daily"`;
      - `platform_payments = { own = 0.4, destination = 0.3, separate = 0.3 }`;
      - `application_fee = { probability = 0.5, percent = 10.0 }`;
      - `payout_failure_probability = 0.2`.
    - Walk the account sub-resources through `sub_path`, and every child type through its parent's sub-list.
    - Assert all 25 types are served.
    - Assert these status values appear:
      - payout `pending`, `in_transit`, `paid`, `failed`;
      - transfer `reversed: true` at least once;
      - application fee `refunded: true` at least once;
      - external accounts of both objects.
    - The expand pass treats `payout.destination` as expandable only where the ID is in truth (P4), and expands the P2 paths across scopes.
  - `served_objects_carry_their_documented_fields`: add the 8 new types, each excluded field commented with its reason (R1).
  - `money_moves_consistently` (Review Focus 1): every invariant listed there, over the busy run's truth state.
  - `payment_lists_survive_faults`: add `/v1/transfers` and `/v1/payouts`.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test openapi_conformance`. Expected: PASS if Tasks 2–8 are right. A failure is a bug in the owning task's code: fix it, add a regression test in that task's test file, and ledger it.
- [ ] **Step 3: Bring the spec in line.**
  - Section 5.1: a phase 8 table with prefixes, delete styles and relations.
  - Section 5.5: phase 8 marked built.
  - Section 7.2: every new endpoint, filter and sub-list.
  - Section 14: the coverage run's new assertions.
  - Section 16: phase 8 done.
  - Section 17: P1–P9 as decisions.
- [ ] **Step 4: Run the full suite,** then `grep -c $'\r' docs/stripe-mock-spec.md`. Expected: PASS, and `0`.
- [ ] **Step 5: Commit** with the message `test(stripe-mock): OpenAPI conformance coverage for phase 8`.
