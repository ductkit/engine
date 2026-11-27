# Stripe Mock Phase 9 (Billing Extras) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mock gains the billing extras: coupons, promotion codes, discounts, invoice items, credit notes with their line items, tax rates and subscription schedules. Discounts, taxes and credit notes change invoice amounts, and payments charge what is left.

**Architecture:**
- **New object types on the existing engine.** Truth log, scopes, events, faults, history and the generated types work as in phases 7 and 8. Each new type is a generated struct, built and changed only through `create_obj` / `update_obj` / `delete`. All of them live in the scope of the generator that makes them, so platform and connected accounts are covered alike.
- **Invoice amounts become computed.** One pure function turns an invoice's lines, its discount and its tax rate into every amount field: per-line discount and tax amounts, the invoice's subtotal, discount and tax totals, and its total and amount due. Both the line and invoice constructors take its result. With no discount and no tax rate, it returns today's amounts.
- **Subscription schedules hook the renewal.** A scheduled subscription's renewal first checks its schedule: it may swap the items for the next phase, or end the schedule by releasing or canceling the subscription.
- **Default config draws nothing new.** Every new action defaults to rate 0, and every new decision draws from a new stream. The one always-on change: one-off invoices' lines already name an `ii_` invoice item, and those items now exist (ruling Q4).

**Tech Stack:** Unchanged from phase 8. No new dependencies.

**Spec:**
- The design, and the binding authority for this plan: [docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md](../docs/superpowers/specs/2026-11-25-stripe-mock-types-and-openapi-design.md). Phase 9 covers group 3 of section 6.5, the phase-9 parts of 6.6, and sections 7 and 8 as they apply.
- The project spec: [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md), sections 4, 5.1, 5.4, 5.5, 6.1, 7.2, 7.3, 11, 14, 16 and 17.
- Facts checked against Stripe's docs for API version `2026-11-18` while planning (2026-11-28):
  - **ID examples:** coupon `jMT0WJUD` (no prefix), `promo_`, `di_`, `ii_`, `cn_`, `cnli_`, `txr_`, `sub_sched_`.
  - **Discounts:** they "can't be fetched by ID".
  - **Invoice items:** deletable only while pending or on a draft invoice. Events are `invoiceitem.created` and `invoiceitem.deleted` only.
  - **Promotion codes:** there is no delete endpoint.
  - **Credit notes:**
    - A pre-payment note reduces `amount_remaining` and `amount_due`.
    - A post-payment note refunds only when asked (`refund_amount`).
    - Only a note on an open invoice can be voided, which raises the amount due again.
  - **Schedules:** `release` keeps the subscription running, and `cancel` cancels it. There is also `subscription_schedule.aborted`, for a cancellation caused by delinquency.

## Global Constraints

- **Generated types:** every new object comes from the vendored `crates/stripe-mock/openapi/spec3.json`. Regenerate with `DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen`. Never hand-edit `src/stripe/generated/`.
- **Only typed code writes objects.** Every new object is built and changed through the generated structs with `create_obj` / `update_obj` / `delete`. No `json!` for a served object.
- **Field presence (phase 7 ruling R1):** model code decides every field explicitly. No `Default` and no `..` struct update. A non-required field is present (a value or `null`) unless Stripe documents it as includable-only or expandable-only. Each absent field gets a comment saying why.
- **The pinned schema wins** over documented behavior or an existing test. Fix the test and the spec section in the same task, and ledger it as a ruling.
- **Determinism:**
  - Same seed and config give the same run.
  - New draws come from new streams only (ruling Q11 names them).
  - New `ActionKind`, `ObjectType`, `Action` and `Continuation` variants are appended at the end.
  - With every new key at its default, no existing stream draws differently, and the truth log differs from phase 8's only by the invoice items of ruling Q4. Task 1's guard test pins this.
- **Config:** every new key defaults to rate `0.0`:
  - `actions.billing_catalog_change`, `actions.discount_change`, `actions.invoice_item`, `actions.credit_note`, `actions.schedule_subscription`;
  - `churn.coupon.update`, `churn.invoiceitem.delete`.

  Validation uses `check_rate` with the key's full name. The spec's section 4 example TOML must keep parsing; the config test checks it.
- **Events** come from the pinned `enabled_events` enum:
  - `coupon.created`, `coupon.updated`, `coupon.deleted`;
  - `promotion_code.created`, `promotion_code.updated`;
  - `customer.discount.created`, `customer.discount.deleted` (`customer.discount.updated` is not generated);
  - `invoiceitem.created`, `invoiceitem.deleted`;
  - `credit_note.created`, `credit_note.updated`, `credit_note.voided`;
  - `tax_rate.created`, `tax_rate.updated`;
  - `subscription_schedule.created`, `subscription_schedule.updated`, `subscription_schedule.released`, `subscription_schedule.completed`, `subscription_schedule.canceled`, `subscription_schedule.aborted`. `expiring` is not generated.

  Thin `v1.` events follow from the projector, as for every type.
- **Connect rules unchanged** (spec 5.4): every new type exists in the platform and in each connected account. Nothing new happens in a disconnected account's scope except settlement.
- **Process:**
  - Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` as separate steps before each commit, each with `-j 2` on Windows (os error 1455).
  - End each commit with a `Co-Authored-By:` trailer naming the model that wrote it.
  - Work on branch `stripe-mock-phase-9` in the existing checkout. Never use a worktree.
- **Windows:** LF line endings. Python edits use `newline=''`. `grep -c $'\r'` gives 0 for every edited file.

## Rulings made while planning

Section 8 of the design leaves these to the builder. Each is stated once here, and tasks refer to them by number.

- **Q1, types, IDs and removal.** Eight `ObjectType` variants, appended in this order (`ALL` becomes 33):

  | Variant | `object` | Prefix | Retrieve path | Removal |
  |---|---|---|---|---|
  | `Coupon` | `coupon` | none: 8 characters `[A-Za-z0-9]` | `/v1/coupons/{id}` | hard delete (`deleted_coupon`) |
  | `PromotionCode` | `promotion_code` | `promo` | `/v1/promotion_codes/{id}` | `active: false` only |
  | `Discount` | `discount` | `di` | none | delete (`deleted_discount`) |
  | `InvoiceItem` | `invoiceitem` | `ii` | `/v1/invoiceitems/{id}` | delete, while pending or on a draft (`deleted_invoiceitem`) |
  | `CreditNote` | `credit_note` | `cn` | `/v1/credit_notes/{id}` | `status: void` |
  | `CreditNoteLineItem` | `credit_note_line_item` | `cnli` | none | none |
  | `TaxRate` | `tax_rate` | `txr` | `/v1/tax_rates/{id}` | `active: false` |
  | `SubscriptionSchedule` | `subscription_schedule` | `sub_sched` | `/v1/subscription_schedules/{id}` | `released` / `completed` / `canceled` |

  - `id_prefix()` returns `""` for `Coupon`.
  - Coupon IDs come from a new helper, `SimRng::code(len: usize) -> String`, which draws `len` characters from `[A-Za-z0-9]`.
  - `expand::type_of_id` cannot recognize a coupon. Ruling Q10 covers that.
- **Q2, discounts.**
  - **Embedded only.** A discount is a stored object with no endpoint. It shows up as `customer.discount` (the full object), in `subscription.discounts` (IDs, expandable), and in an invoice's and its lines' `discounts` (IDs, expandable; a deleted one expands to `deleted_discount`).
  - **At most one per parent.** A customer or a subscription has at most one discount. A subscription invoice uses the subscription's discount, or else the customer's. A one-off invoice uses the customer's.
  - **Creating one** (all under one cause):
    - the coupon's `times_redeemed` +1, a silent update that sets `valid: false` when `max_redemptions` is reached;
    - the promotion code's `times_redeemed` +1, silent;
    - the discount (`customer.discount.created`);
    - then the parent: the customer's `discount` set silently, or the subscription's `discounts = [di]` (`customer.subscription.updated`).
  - **Removing one:** the parent first (silent for a customer; `customer.subscription.updated` for a subscription), then the discount deleted (`customer.discount.deleted`, tombstone `deleted_discount`, which is the full discount plus `deleted: true`).
  - **When a discount ends:**
    - `once`: removed in the same instant its first invoice is finalized.
    - `repeating`: `end = add_intervals(start, "month", duration_in_months)`, and the continuation `DiscountEnd { id }` removes it at `end`.
    - `forever`: `end` is `null`.
  - **Related object.** A discount's thin events get `related_object { id: di_…, type: "discount", url: <parent's URL> }`, where the parent is the subscription when set, else the customer. The URL field is required and a discount has no URL of its own, and the parent embeds it.
  - **Customer deletion.** Deleting a customer removes its discount first.
  - Cost if wrong: a consumer that expects `customer.updated` alongside a customer discount sees only the discount event.
- **Q3, invoice amounts.** One pure function, `model::amounts::compute(lines: &[LineAmount], discount: Option<&AppliedDiscount>, tax: Option<&AppliedTax>) -> InvoiceAmounts`, does all the arithmetic in integers.
  - **Discount:**
    - The discount total `D` is `min(subtotal, amount_off)` when the coupon's `amount_off` currency matches the invoice. Otherwise it is `percent_off` applied as `(subtotal × percent_bp + 5_000) / 10_000`, which rounds half up. `percent_off` is drawn in whole percents, so `percent_bp = percent × 100` is exact.
    - A coupon whose `amount_off` is in another currency doesn't apply: `D = 0`, and the invoice carries no discount.
    - `D` is shared across lines with `amount > 0`, in line order: each line gets `D × amount / subtotal` rounded down, and the last such line gets the remainder.
  - **Tax** (exclusive only):
    - Each line's tax is `(taxable × bp + 5_000) / 10_000`, where `taxable` is the line's amount minus its discount share.
    - `bp` comes from the rate's percentage, drawn from `[5.0, 7.25, 8.875, 10.0, 20.0]` and stored as basis points (`500, 725, 888, 1000, 2000`). `8.875` is shown as `percentage: 8.875`, but uses 888 bp.
  - **Invoice fields:**
    - `subtotal` and `subtotal_excluding_tax` = the sum of line amounts.
    - `total_discount_amounts = [{amount: D, discount}]`, or `[]`.
    - `total_excluding_tax` = subtotal − D.
    - `total_taxes` = one entry per tax rate, with its amount and taxable amount summed.
    - `total` = total_excluding_tax + tax.
    - `amount_due = amount_remaining = total`.
  - **Line fields:**
    - `discount_amounts = [{amount, discount}]` and `discounts = [di]`;
    - `taxes`: entries of `{ amount, tax_behavior: exclusive, tax_rate_details: { tax_rate }, taxability_reason: standard_rated, taxable_amount, type: tax_rate_details }`.
    - The line's `amount` stays before discount and tax, as in Stripe.
  - **Over the limit.** An invoice whose total with tax would exceed `MAX_CHARGE_AMOUNT` is created without its tax. Cost: very large invoices lose their tax line.
  - **Paying.** `billing::pay` sets `amount_paid` to the `amount_remaining` it charged, not to `total`.
- **Q4, invoice items.**
  - **Always on:** each line of a one-off invoice now gets its real `invoiceitem`, using the `ii_` ID that phase 7 already draws. In one instant, before the lines:
    - the items, with `invoice` set to the draft's ID and `invoiceitem.created`;
    - then the lines;
    - then the invoice.
  - **Pending items** come from the `invoice_item` action: `invoice: null`, a one-time price (falling back like a one-off line does), quantity 1–3.
  - **Joining an invoice.** A customer's pending items, oldest first, join that customer's next invoice of either kind, after its own lines. The item's `invoice` is set in a silent update, and the invoice gets one line per item, whose `parent.invoice_item_details` names the item and the subscription (if any). An item that would push the subtotal over `MAX_CHARGE_AMOUNT` stays pending.
  - **Deleting.** An item can be deleted only while pending or on a draft:
    - `churn.invoiceitem.delete` deletes a random pending one;
    - deleting a draft deletes its items;
    - deleting a customer deletes its pending items;
    - each emits `invoiceitem.deleted`.
  - **No `invoiceitem.updated`** exists, so every update is silent.
  - Cost if wrong: Stripe may return a deleted draft's items to pending instead of deleting them.
- **Q5, tax rates in use.**
  - **Which objects get a rate.** While at least one active tax rate exists, a new subscription takes one with probability 0.5: either `start_subscription`, the subscription of a Checkout session, or one from `schedule_subscription`. So does a new one-off invoice. The rate is drawn uniformly over the active ones, from stream `generate.tax`.
  - **Where it's stored.** A subscription stores it in `default_tax_rates` (`[rate]`), and its invoices tax every line with it. A one-off invoice stores it in its own `default_tax_rates`.
  - **Embedded copies.** These copies are snapshots taken when the rate was attached. Archiving a rate doesn't rewrite them, and an archived rate is never attached again. Cost if wrong: Stripe may render the rate live; the mock shows `active: true` on an old embed.
  - **Other fields.** `tax_rates` on subscription items and invoice items stays `[]`. `inclusive` is always `false`.
- **Q6, billing catalog.** The design says "`catalog_change` extended to coupons, promotion codes and tax rates". Adding branches to `catalog_change` would change its draws, so this ships as a sibling action instead: `billing_catalog_change` (stream `generate.billing_catalog.change`). Weights out of 100, each falling back to a new coupon when its target is missing:

  | Share | Step |
  |---|---|
  | 30 | New coupon |
  | 25 | New promotion code for a random valid coupon |
  | 15 | New tax rate |
  | 10 | Deactivate a random active promotion code (`promotion_code.updated`) |
  | 10 | Archive a random active tax rate (`tax_rate.updated`) |
  | 10 | Delete a random coupon |

  - **Deleting a coupon:** its active promotion codes are deactivated first. Discounts that use the coupon outlive it.
  - **Coupon draws:**
    - 60% `percent_off` in 5–50, step 5; else `amount_off` in 100–2000, step 100, in `random_currency`.
    - Duration: `once` 40%, `repeating` 30% (`duration_in_months` 1–6), `forever` 30%.
    - `max_redemptions`: 1–100 with probability 0.2, else `null`. `redeem_by` is always `null`.
    - `name` is `"{p}% off"` or `"{amount} {CUR} off"`.
  - **Promotion code draws:**
    - `code`: 8 uppercase characters.
    - `active: true`; `customer`, `expires_at` and `max_redemptions` are `null`.
    - `restrictions { first_time_transaction: false, minimum_amount: null, minimum_amount_currency: null }`.
  - **Tax rate draws:** one of three, uniformly, with the percentage from the Q3 list:
    - `Sales Tax`: `US`, `CA`, `sales_tax`;
    - `VAT`: `DE`, `vat`;
    - `GST`: `AU`, `gst`.
  - **Coupon churn:** `churn.coupon.update` changes the `metadata.note` of a random live coupon (`coupon.updated`).
  - Cost if wrong: none for the data; the action's name differs from the design's wording.
- **Q7, the discount action** (`discount_change`, stream `generate.discount_change`).
  - **Add or remove.** It removes a random existing discount with probability 0.3; otherwise, and whenever none exists, it adds one.
  - **Where it adds.** The target is drawn 50/50: a random live customer with no discount, or a random `active`/`past_due` subscription with no discount. If the drawn kind has no candidate, it uses the other kind. If neither has one, nothing happens.
  - **What it uses.** With probability 0.5 it uses a random active promotion code whose coupon is valid. Otherwise it uses a random valid coupon. With neither, it first creates a coupon, drawn as in Q6, under the same cause.
- **Q8, credit notes** (`credit_note`, stream `generate.credit_note`).
  - **Void or create.** With probability 0.25, and when one exists, the action voids a random `issued` pre-payment note on an `open` invoice. Otherwise it credits a random eligible invoice, drawn uniformly from:
    - `open` invoices with `amount_remaining ≥ 2`. They get a **pre-payment** note for `max(1, amount_remaining × p / 100)`, with `p` uniform in 10–50, so an invoice is never fully credited.
    - `paid` invoices whose charge has something left to refund. They get a **post-payment** note for `max(1, left × p / 100)`, with `p` in 10–100. It refunds that amount through `payments::refund` (`refund.created`, `charge.refunded`) and records `refunds = [{amount_refunded, refund, type: refund}]`.
  - **Credit note fields:**
    - `number`: `{invoice number}-CN-{n:02}`, where `n` counts that invoice's notes.
    - `pdf`: `https://pay.stripe.com/credit_notes/mock/{id}/pdf`.
    - `reason`: drawn from the enum.
    - `subtotal = total = amount`, and `discount_amount` is 0.
    - `pre_payment_amount` or `post_payment_amount` = amount, with the other 0.
    - `type`: `pre_payment` or `post_payment`; `mixed` is not generated.
  - **Line items.** The amount is spread over the invoice's lines with `amount > 0` by the Q3 sharing rule, giving one line item per invoice line: `type: invoice_line_item`, `invoice_line_item` the line, `taxes: []`, `tax_rates: []`, `quantity` and `unit_amount` `null`.
  - **Invoice changes** (`invoice.updated`):
    - pre-payment: `amount_due` and `amount_remaining` − amount, and `pre_payment_credit_notes_amount` + amount;
    - post-payment: `post_payment_credit_notes_amount` + amount;
    - voiding reverses the pre-payment changes and sets the note `void` with `voided_at` (`credit_note.voided`).
  - **Order:** note line items (no event), then the note (`credit_note.created`), then the invoice.
  - **How the lines are found.** `credit_note_line_item` has no parent field, so the engine keeps an index, `credit_note → [cnli]`, as it does for payout sweeps. It serves `/v1/credit_notes/{id}/lines`, and the note's embedded `lines`.
  - Cost if wrong: credit notes carry no tax breakdown.
- **Q9, subscription schedules** (`schedule_subscription`, stream `generate.subscription.schedule`).
  - **Phases.** One action draws a customer (`resolve_customer`) and 2–3 phases. Each phase has 1–2 prices (via `pick_prices`, kept to the first phase's currency and interval) and 1–3 iterations of that interval. `end_behavior` is `release` 70% or `cancel` 30%.
  - **Created** in one instant:
    - the subscription, through the `start_subscription` path, with `schedule` set;
    - then the schedule: `active`, `subscription` the sub, `phases` with `start_date`/`end_date` from `add_intervals` on the start, and `current_phase` = phase 1 (`subscription_schedule.created`).

    Everything after that (first invoice, payment, activation) runs as for any subscription. `not_started` is not generated.
  - **At each `Renew` of a scheduled subscription,** when `now ≥ current_phase.end_date`:
    - **A next phase exists:** the old items are deleted and the next phase's items created, both unannounced. Each new item's current period is the one `advance_periods` would give. `current_phase` advances (`subscription_schedule.updated`), and the renewal goes on.
    - **Last phase, `release`:** the schedule is `released`, with `released_at`, `released_subscription`, `subscription: null` and `current_phase: null` (`subscription_schedule.released`). The sub's `schedule` becomes `null` (`customer.subscription.updated`), and the renewal goes on.
    - **Last phase, `cancel`:** the sub is canceled now (`customer.subscription.deleted`), and the schedule is `completed`, with `completed_at` and `current_phase: null` (`subscription_schedule.completed`). There is no renewal invoice.
  - **When the subscription ends early:** the schedule is `canceled`, with `canceled_at` (`subscription_schedule.canceled`). That covers `cancel_subscription`, which cancels a scheduled sub now whatever `at_period_end` says, customer deletion, and `incomplete_expired`. If the dunning retry fails, the event is `subscription_schedule.aborted` instead, and the status is still `canceled`.
  - **Not covered:** with renewals off (`renew_subscription_cycle` not natural), a schedule never advances. Checkout subscriptions are never scheduled.
  - Cost if wrong: a consumer waiting for `subscription_schedule.expiring` never gets one.
- **Q10, API.**
  - **Lists and filters:**

    | Path | Filters |
    |---|---|
    | `/v1/coupons` | none |
    | `/v1/promotion_codes` | `active`, `code`, `coupon` (matches `promotion.coupon`), `customer` |
    | `/v1/invoiceitems` | `customer`, `invoice`, `pending` (`true`: `invoice` is null) |
    | `/v1/credit_notes` | `customer`, `invoice` |
    | `/v1/credit_notes/{id}/lines` | sub-list |
    | `/v1/tax_rates` | `active`, `inclusive` |
    | `/v1/subscription_schedules` | `customer` |

    Discounts and credit note line items have no retrieve endpoint.
  - **New `expand::allowed` entries:**
    - Customer: `discount.promotion_code`, `discount.source.coupon`.
    - Subscription: `schedule`, `discounts`.
    - Invoice: `discounts`.
    - PromotionCode: `customer`, `promotion.coupon`.
    - InvoiceItem: `customer`, `invoice`, `discounts`.
    - CreditNote: `customer`, `invoice`.
    - SubscriptionSchedule: `customer`, `subscription`.
    - Coupon, TaxRate, Discount, CreditNoteLineItem: none.
  - **Coupon lookups.** A path whose last segment is `coupon` resolves its ID as a `Coupon`, since coupons have no prefix. A deleted coupon stays an ID, because the schema has no `deleted_coupon` alternative there.
  - **Store parents** (`parent_fields`):

    | Type | Parent fields |
    |---|---|
    | `PromotionCode` | `customer` |
    | `Discount` | `customer`, `subscription` |
    | `InvoiceItem` | `customer`, `invoice` |
    | `CreditNote` | `invoice`, `customer` |
    | `SubscriptionSchedule` | `customer` |
    | everything else new | none |
  - **Pagination writes** (spec 7.4), one arm per listable new type, drawing from the existing pagination stream:
    - coupons, promotion codes and tax rates get the matching Q6 creation;
    - invoice items get a new pending item;
    - credit notes and schedules get a `metadata.note` update (`credit_note.updated`, `subscription_schedule.updated`).

    Task 2 adds each arm as a stub that does nothing. The task that builds the type fills it in: Task 3 (coupons, codes, tax rates), Task 4 (invoice items), Task 7 (credit notes) and Task 8 (schedules).
- **Q11, streams and control actions.**
  - **New `ActionKind` variants,** appended in this order, each with its stream:

    | Kind | Stream |
    |---|---|
    | `BillingCatalogChange` | `generate.billing_catalog.change` |
    | `CouponUpdate` | `generate.coupon.update` |
    | `InvoiceItem` | `generate.invoice_item` |
    | `InvoiceItemDelete` | `generate.invoice_item.delete` |
    | `DiscountChange` | `generate.discount_change` |
    | `CreditNote` | `generate.credit_note` |
    | `ScheduleSubscription` | `generate.subscription.schedule` |

    `Ctx` gains `tax_rng` (stream `generate.tax`). Each new `[actions]` kind gets its arm in `Generator::set_actions_config`; phase 8's final review found that arm missing for two kinds.
  - **New `Action` variants** (control; draws from `control.actions`):
    - `BillingCatalogChange`;
    - `DeleteCoupon { id }`;
    - `CreateInvoiceItem { customer: Option<String> }`;
    - `DeleteInvoiceItem { id: Option<String> }`;
    - `AddDiscount { customer: Option<String>, subscription: Option<String>, coupon: Option<String>, promotion_code: Option<String> }`;
    - `RemoveDiscount { id: Option<String> }`;
    - `CreditNote { invoice: Option<String>, amount: Option<i64> }`;
    - `VoidCreditNote { id: Option<String> }`;
    - `ScheduleSubscription { customer: Option<String>, end_behavior: Option<ScheduleEndBehavior> }`;
    - `CancelSubscriptionSchedule { id: Option<String> }`, which cancels the schedule and the subscription now.
  - `Archive { id }` also takes `promo_…` (deactivate) and `txr_…` (archive).
  - **Errors.** A named target that doesn't fit is a 409 `invalid_action` naming it. Examples: an invoice item on a finalized invoice, a credit amount above what's left, a note on a paid invoice to void, or a customer that already has a discount.
- **Q12, replay guard.** The exact phase 7 digest can't survive Q4: the new items take truth-log `seq` numbers and event IDs. Task 1 replaces the test `default_config_replays_phase_7_run` with `default_config_replays_phase_8_run`.
  - It hashes the truth log after three changes: entries of the eight new types removed, and `seq` and `events` stripped from every entry.
  - It runs with the same `PHASE_7_FULL` config, stepped two days.
  - The digest is recorded on the phase 9 base commit, before any change.
  - Everything else (order, times, states, causes) stays pinned. Cost: event IDs and sequence numbers are no longer pinned at default config.

## Review Focus

1. **Invoice money doesn't add up.**
   - Per invoice: subtotal − discount + tax = total.
   - The lines' discount and tax amounts add up to the invoice totals.
   - `amount_due` = total − active pre-payment credits.
   - The charge's amount = the `amount_remaining` it paid.
   - A note's amount = the sum of its line items.
   - A post-payment note's refund adds up to its `post_payment_amount`, and the charge's `amount_refunded` includes it.

   Task 9's `billing_money_moves_consistently` checks every item.
2. **References that outlive their target.** Each must still be served:
   - a deleted coupon, still named by a discount;
   - a deleted discount, named by `invoice.discounts`, which expands to `deleted_discount`;
   - an archived tax rate, embedded in a subscription;
   - a deactivated promotion code, named by a discount;
   - a deleted pending item, which is never on any invoice.

   Tasks 3, 5 and 6 each pin their own.
3. **Schedules racing other actions.** Each must leave the schedule in a terminal status with its event, and never advance a phase on an ended subscription:
   - `cancel_subscription`, customer deletion, dunning and `incomplete_expired` on a scheduled subscription;
   - a renewal while `past_due`;
   - renewals turned off mid-schedule.

   Task 8 owns these.
4. **Connected accounts.**
   - Every new type is generated in a connected account's scope with its events labelled.
   - A disconnect drops pending `DiscountEnd` and schedule steps.
   - Nothing appears in a disconnected scope.

   Task 9's scope test extends `connect_objects_land_in_their_scope`.
5. **Discount lifecycle edges:**
   - a `once` discount is removed exactly once, at its first finalization, even when that invoice is later voided;
   - a `repeating` discount ends on time;
   - a customer with a discount is deleted;
   - a subscription with a discount is canceled (its discount stays);
   - an `amount_off` coupon in another currency applies nothing.

   Task 5 owns these.

---

### Task 1: Generated types and the replay guard

**Files:**
- Modify: `tests/codegen/mod.rs` (`ROOTS`), `src/stripe/generated/` (regenerated only), `tests/replay.rs`, `tests/openapi_codegen.rs` (if it lists roots)

**Interfaces:**
- Produces: the generated types `Coupon`, `DeletedCoupon`, `PromotionCode`, `Discount`, `DeletedDiscount`, `Invoiceitem`, `DeletedInvoiceitem`, `CreditNote`, `CreditNoteLineItem`, `TaxRate` and `SubscriptionSchedule`, named the way the generator names them. Later tasks use those names.

- [ ] **Step 1: Write the guard test.** In `tests/replay.rs`:
  - Add `fn phase_8_projection(log: &[StateUpdate]) -> String`. It drops the Q1 types and serializes each remaining entry without `seq` and `events`.
  - Add `default_config_replays_phase_8_run` with `const PHASE_8_TRUTH_SHA256`.
  - Run it once on the unchanged base commit to print the digest, then record it. Replace `default_config_replays_phase_7_run` with it (ruling Q12).
- [ ] **Step 2:** Add these 11 roots after `"deleted_person"`: `"coupon"`, `"deleted_coupon"`, `"promotion_code"`, `"discount"`, `"deleted_discount"`, `"invoiceitem"`, `"deleted_invoiceitem"`, `"credit_note"`, `"credit_note_line_item"`, `"tax_rate"`, `"subscription_schedule"`.
- [ ] **Step 3: Regenerate.** Run `DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen`, then `cargo test -p ductkit-stripe-mock --test openapi_codegen`. Expected: PASS, and the freshness check is clean.
- [ ] **Step 4:** Run the full suite. Expected: PASS, including the new guard test. Record the closure size before and after in the report.
- [ ] **Step 5: Commit:** `feat(stripe-mock): generated types for billing extras; phase 8 replay guard`.

### Task 2: Object types, routes, expansion and events

**Files:**
- Modify:
  - `src/model/mod.rs`: variants, `ALL`, `as_str`, `id_prefix`, `api_path`;
  - `src/rng.rs`: `code`;
  - `src/engine/store.rs`: `parent_fields`;
  - `src/engine/mod.rs`: the credit note line index;
  - `src/engine/events.rs`: discount `related_object`;
  - `src/api/mod.rs`: routes;
  - `src/api/resources.rs`: filters, the `/lines` sub-list;
  - `src/api/expand.rs`: `allowed` and the coupon lookup;
  - `src/generate/mod.rs`: `pagination_write` arms, as stubs that do nothing until their task.
- Test: `tests/api_billing_extras.rs` (new)

**Interfaces:**
- Produces:
  - `ObjectType::{Coupon, PromotionCode, Discount, InvoiceItem, CreditNote, CreditNoteLineItem, TaxRate, SubscriptionSchedule}`;
  - `SimRng::code(&mut self, len: usize) -> String`;
  - `Engine::record_credit_note_lines(&mut self, credit_note: &str, lines: Vec<String>)` and `Engine::credit_note_lines(&self, credit_note: &str) -> &[String]`, kept per scope like the payout index;
  - discount thin events whose `related_object.url` is the parent's URL (Q2).

- [ ] **Step 1: Write the failing tests** in `tests/api_billing_extras.rs`:
  - `new_lists_are_empty_and_well_formed`: each Q10 list path returns `{object: "list", data: [], has_more: false, url}`, and `/v1/credit_notes/cn_x/lines` returns 404 `resource_missing`.
  - `unknown_ids_are_404`: retrieves of `/v1/coupons/ABCDEFGH`, `/v1/promotion_codes/promo_x`, `/v1/invoiceitems/ii_x`, `/v1/credit_notes/cn_x`, `/v1/tax_rates/txr_x` and `/v1/subscription_schedules/sub_sched_x`.
  - `new_expand_paths_validate`: every Q10 path is accepted on its type, and `expand[]=source` on a promotion code is a 400.
  - A unit test in `src/rng.rs`, `code_draws_alphanumerics`: length 8, every character `is_ascii_alphanumeric`, and two draws differ.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test api_billing_extras -j 2`. Expected: FAIL (404 route, unknown type).
- [ ] **Step 3: Implement the Q1 and Q10 plumbing.** The event projector takes a `Discount`'s `related_object.url` from its state's `subscription` (`/v1/subscriptions/{id}`), else `customer`. The coupon lookup lives in `expand::apply`'s ID resolution: when `type_of_id` fails and the segment is `coupon`, it looks up `ObjectType::Coupon`.
- [ ] **Step 4: Run them.** Run `cargo test -p ductkit-stripe-mock --test api_billing_extras -j 2`, then the full suite. Expected: PASS.
- [ ] **Step 5: Commit:** `feat(stripe-mock): billing extras object types, routes and expansion`.

### Task 3: Coupons, promotion codes and tax rates

**Files:**
- Create: `src/model/discounts.rs` (constructors for `Coupon`, `PromotionCode`, `Discount`, `TaxRate` and the tombstones), `src/generate/billing_catalog.rs`
- Modify: `src/config.rs` (`actions.billing_catalog_change`, `churn.coupon`), `src/generate/mod.rs` (kinds, dispatch, `Action` variants, `set_actions_config`), `src/generate/catalog.rs` (`archive_by_id` for `promo_` and `txr_`), `src/model/mod.rs`
- Test: `tests/billing_catalog.rs` (new)

**Interfaces:**
- Consumes: Task 2's types and `SimRng::code`.
- Produces:
  - `billing_catalog::new_coupon(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) -> String`, used by Q7's on-the-spot coupon;
  - `billing_catalog::valid_coupons(engine: &Engine) -> Vec<String>` and `active_promotion_codes(engine: &Engine) -> Vec<String>`, where a code counts only if its coupon is valid and live;
  - `model::discounts::tax_bp(rate: &stripe::TaxRate) -> i64`, the Q3 basis points.

- [ ] **Step 1: Write the failing tests** in `tests/billing_catalog.rs`:
  - `catalog_creates_coupons_codes_and_rates`: with `billing_catalog_change = 50.0` over 2 days, each of coupons, promotion codes and tax rates is listed at least once. Every object passes `check_payload`. Each coupon has exactly one of `percent_off` and `amount_off`, and `amount_off` coupons carry a `currency`.
  - `deleting_a_coupon_deactivates_its_codes`: use `Action::BillingCatalogChange` until a promotion code exists, then `DeleteCoupon` its coupon. The events are `promotion_code.updated` (`active: false`) before `coupon.deleted`. A retrieve of the coupon returns `{id, object: "coupon", deleted: true}`, and the code is still retrievable.
  - `archive_deactivates_codes_and_tax_rates`: `Archive` on `promo_…` gives `active: false`; on `txr_…` it also gives `active: false` (`tax_rate.updated`). A second archive is a 409.
  - `coupon_churn_updates_metadata`: `churn.coupon.update = 50.0` gives `coupon.updated`, with `metadata.note` set.
  - `default_config_creates_no_billing_catalog`: the default config, stepped 2 days, lists no coupons, codes or tax rates.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test billing_catalog -j 2`. Expected: FAIL.
- [ ] **Step 3: Implement Q6, the matching `Action` variants and the pagination-write arms of Q10.** Implement Q6 in `billing_catalog.rs`. The constructors in `model/discounts.rs` follow R1, each with a unit test that runs `check_payload` on it.
- [ ] **Step 4: Run them.** Run `cargo test -p ductkit-stripe-mock --test billing_catalog -j 2`, then the full suite. Expected: PASS, and the replay guard passes.
- [ ] **Step 5: Commit:** `feat(stripe-mock): coupons, promotion codes and tax rates`.

### Task 4: Invoice items

**Files:**
- Modify: `src/model/invoice.rs` (`new_invoice_item`, `deleted_invoice_item`), `src/generate/billing.rs` (one-off items, joining pending items, draft deletion), `src/generate/subscriptions.rs` (`invoice_subscription` joins pending items; customer cascade), `src/config.rs` (`actions.invoice_item`, `churn.invoiceitem`), `src/generate/mod.rs`
- Test: `tests/invoice_items.rs` (new)

**Interfaces:**
- Consumes: `LineSpec`, `create_draft_invoice`.
- Produces: `billing::pending_items(engine: &Engine, customer: &str) -> Vec<String>` (oldest first). Task 9 uses it.

- [ ] **Step 1: Write the failing tests** in `tests/invoice_items.rs`:
  - `one_off_lines_have_real_items`: for each line of a one-off invoice, `parent.invoice_item_details.invoice_item` retrieves as an `invoiceitem` with that `invoice`, the line's amount, currency and period, and `invoiceitem.created` before `invoice.created`.
  - `pending_items_join_the_next_subscription_invoice`:
    - `CreateInvoiceItem` for a customer, then `StartSubscription` for the same customer;
    - the first invoice has the item's line last, and its subtotal includes it;
    - the item now names the invoice;
    - `/v1/invoiceitems?pending=true` is empty.
  - `deleted_draft_takes_its_items`: `OneOffInvoice { outcome: DeleteDraft }` gives `invoiceitem.deleted` for each item before `invoice.deleted`.
  - `only_pending_or_draft_items_delete`: `DeleteInvoiceItem` on a pending item works (`invoiceitem.deleted`, tombstone `{id, object: "invoiceitem", deleted: true}`). On an item of a finalized invoice it is a 409 naming it.
  - `customer_deletion_deletes_pending_items`.
  - `oversized_pending_item_stays_pending`: build a pending item of amount `MAX_CHARGE_AMOUNT` (a price with a large unit amount, quantity 1), then a one-off invoice. The item stays pending, and the invoice's subtotal is at most `MAX_CHARGE_AMOUNT`.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test invoice_items -j 2`. Expected: FAIL.
- [ ] **Step 3: Implement Q4,** including its `Action` variants and the invoice-item pagination-write arm. IDs of pending items come from the `invoice_item` action's own stream, or from `control.actions`. One-off items reuse the `ii_` IDs already drawn.
- [ ] **Step 4: Run them.** Run `cargo test -p ductkit-stripe-mock --test invoice_items -j 2`, then the full suite. Expected: PASS, the replay guard passes, and `default_config_replays_phase_8_run` gives the Task 1 digest.
- [ ] **Step 5: Commit:** `feat(stripe-mock): invoice items, pending items joining the next invoice`.

### Task 5: Discounts and computed invoice amounts

**Files:**
- Create: `src/model/amounts.rs`, `src/generate/discounts.rs`
- Modify: `src/model/invoice.rs` (`new_line` and `new_invoice` take `InvoiceAmounts`), `src/generate/billing.rs` (`create_draft_invoice` computes amounts; `finalize` ends `once` discounts; `pay` per Q3), `src/generate/subscriptions.rs` (customer cascade), `src/config.rs` (`actions.discount_change`), `src/generate/mod.rs` (`DiscountEnd`)
- Test: `tests/discounts.rs` (new), plus unit tests in `src/model/amounts.rs`

**Interfaces:**
- Produces:
  - `model::amounts::{LineAmount { amount: i64 }, AppliedDiscount { id: String, percent_bp: Option<i64>, amount_off: Option<i64> }, AppliedTax { rate: stripe::TaxRate, bp: i64 }, InvoiceAmounts { lines: Vec<LineAmounts>, subtotal, discount, tax, total }, LineAmounts { discount: i64, tax: i64, taxable: i64 }}` and `compute(...) -> InvoiceAmounts` (Q3);
  - `model::amounts::share(total: i64, weights: &[i64]) -> Vec<i64>`, the Q3 sharing rule, which Task 7 reuses;
  - `discounts::applied_discount(engine: &Engine, customer: &str, subscription: Option<&str>, currency: &str) -> Option<AppliedDiscount>`.

- [ ] **Step 1: Write the failing unit tests** in `src/model/amounts.rs`:
  - `no_discount_no_tax_is_unchanged`: lines `[500, 1000]` give subtotal = total = 1500 and every share 0.
  - `percent_rounds_half_up_and_shares_to_the_last`: lines `[333, 333, 334]` at 15% give `D = (1000×1500+5000)/10000 = 150`, with shares `[49, 49, 52]`.
  - `amount_off_caps_at_subtotal`: amount_off 2000 on subtotal 1500 gives `D = 1500` and total 0.
  - `tax_on_the_discounted_amount`: line 1000, 10% off, 7.25% tax gives taxable 900, tax `(900×725+5000)/10000 = 65`, and total 965.
  - `zero_lines_get_no_share`: lines `[0, 1000]` at 50% give shares `[0, 500]`.
- [ ] **Step 2: Write the failing integration tests** in `tests/discounts.rs`:
  - `customer_discount_applies_to_a_one_off_invoice`: `AddDiscount { customer, coupon }` with a 20% coupon, then `OneOffInvoice { outcome: Paid }`.
    - `invoice.discounts` = `[di]`, and `total_discount_amounts[0].amount` = the sum of the lines' `discount_amounts`.
    - The charge's amount equals `amount_due`.
    - `customer.discount.created` comes before the invoice events.
  - `subscription_discount_wins_over_customer`.
  - `once_discount_ends_at_first_finalization`: a `once` coupon on a customer. After one finalized invoice, `customer.discount` is `null`, `customer.discount.deleted` fired once, and a later invoice carries no discount.
  - `repeating_discount_ends_on_time`: `duration_in_months = 1`. The discount is still there just before `end`, and `customer.discount.deleted` fires at `end`.
  - `deleted_discount_stays_expandable_on_old_invoices`: `RemoveDiscount`, then `expand[]=discounts` on the earlier invoice gives `{deleted: true, object: "discount"}`.
  - `customer_deletion_removes_its_discount_first`.
  - `foreign_currency_amount_off_applies_nothing`: a USD `amount_off` coupon on a EUR invoice gives `discounts: []` and total = subtotal.
  - `deleted_coupon_leaves_its_discounts`: `DeleteCoupon` while a discount uses it. The discount still applies to the next invoice, and `expand[]=discount.source.coupon` on the customer leaves the ID.
  - `discount_thin_event_points_at_the_parent`: the thin `v1.customer.discount.created` has `related_object.url` set to the customer's or subscription's URL.
  - `promotion_code_redemptions_count`: `AddDiscount` with a promotion code. The discount names it, and the code's and the coupon's `times_redeemed` are 1.
- [ ] **Step 3: Run them.** Run `cargo test -p ductkit-stripe-mock --lib amounts -j 2` and `--test discounts -j 2`. Expected: FAIL.
- [ ] **Step 4: Implement Q2, Q3 (without tax inputs, which Task 6 wires) and Q7.**
- [ ] **Step 5: Run them.** Run both commands, then the full suite. Expected: PASS, and the replay guard passes: with no discount, the computed amounts are the old ones.
- [ ] **Step 6: Commit:** `feat(stripe-mock): discounts and computed invoice amounts`.

### Task 6: Tax rates on subscriptions and invoices

**Files:**
- Modify: `src/generate/subscriptions.rs` (tax draw on start), `src/generate/checkout.rs` (tax draw for subscription mode), `src/generate/billing.rs` (one-off tax draw; `AppliedTax` into `compute`), `src/model/subscription.rs` and `src/model/invoice.rs` (`default_tax_rates`, line `taxes`, `total_taxes`), `src/generate/mod.rs` (`Ctx::tax_rng`)
- Test: `tests/tax_rates.rs` (new)

**Interfaces:**
- Consumes: `compute`, `tax_bp` and `AppliedTax` from Tasks 3 and 5.
- Produces: `billing::draw_tax_rate(engine: &Engine, ctx: &mut Ctx) -> Option<stripe::TaxRate>`. It draws from `tax_rng` only when an active rate exists.

- [ ] **Step 1: Write the failing tests** in `tests/tax_rates.rs`:
  - `taxed_subscription_invoices`: create a tax rate with `BillingCatalogChange` until one exists, then start subscriptions until one has `default_tax_rates` (bounded at 40 tries; the test fails if none gets one).
    - Every invoice of that subscription has each line's `taxes[0].amount` equal to the Q3 formula on its taxable amount.
    - `total` = `total_excluding_tax` + the sum of `total_taxes`.
    - The charge equals `amount_due`.
  - `archived_rate_stays_embedded_and_is_not_reused`:
    - After `Archive` of the rate, the taxed subscription still embeds it with `active: true`; this is the snapshot rule of Q5.
    - Its next invoice is still taxed.
    - No new subscription or one-off invoice takes it: after archiving, 20 more starts get no `default_tax_rates`.
  - With no active rate, nothing is drawn from `generate.tax`. The replay guard pins this, because its config starts subscriptions and creates no tax rate, so this task adds no separate test for it.
  - `tax_dropped_over_the_charge_limit`: a one-off invoice whose subtotal is `MAX_CHARGE_AMOUNT`, with a rate attached, has `total_taxes: []`.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test tax_rates -j 2`. Expected: FAIL.
- [ ] **Step 3: Implement Q5.**
- [ ] **Step 4: Run them.** Run `cargo test -p ductkit-stripe-mock --test tax_rates -j 2`, then the full suite. Expected: PASS, and the replay guard passes.
- [ ] **Step 5: Commit:** `feat(stripe-mock): tax rates on subscriptions and invoices`.

### Task 7: Credit notes

**Files:**
- Create: `src/model/credit_note.rs`, `src/generate/credit_notes.rs`
- Modify: `src/config.rs` (`actions.credit_note`), `src/generate/mod.rs`, `src/generate/payments.rs` (a `pub(crate) fn invoice_charge(engine: &Engine, invoice: &str) -> Option<String>`: the paid invoice payment's intent's `latest_charge`), `src/api/resources.rs` (the `/lines` sub-list from the engine index)
- Test: `tests/credit_notes.rs` (new)

**Interfaces:**
- Consumes: `share`, `payments::refund`, and `Engine::record_credit_note_lines`.

- [ ] **Step 1: Write the failing tests** in `tests/credit_notes.rs`:
  - `pre_payment_note_reduces_amount_due`: an open invoice, then `CreditNote { invoice, amount: Some(100) }`. To get the open invoice, start a subscription, then set `payment_failure_probability = 1.0` and step to its first renewal. The failed renewal invoice stays `open` until the dunning retry, three days later.
    - The note has `type: pre_payment` and `pre_payment_amount: 100`, and `lines` sums to 100.
    - The invoice's `amount_due` and `amount_remaining` drop by 100, and `pre_payment_credit_notes_amount` is 100.
    - The events are `credit_note.created` then `invoice.updated`.
    - When the invoice is then paid, the charge is `total − 100`.
  - `post_payment_note_refunds`: a paid invoice, then `CreditNote`.
    - A `refund.created` follows for the invoice's charge, with the note's amount.
    - `refunds[0].refund` is that refund.
    - `post_payment_credit_notes_amount` is incremented, and the charge's `amount_refunded` includes it.
  - `void_restores_the_invoice`: `VoidCreditNote` on the pre-payment note gives `status: void` and `voided_at`, `credit_note.voided`, and the invoice's amounts as before. A second void is a 409.
  - `only_open_invoice_notes_void`: voiding a post-payment note is a 409 naming it.
  - `over_credit_is_refused`: `amount` greater than `amount_remaining − 1` on an open invoice, or greater than what's left to refund on a paid one, is a 409.
  - `lines_sub_list_pages`: a note on a 3-line invoice lists 3 line items through `/v1/credit_notes/{id}/lines?limit=2`, over two pages, in creation order. The line items equal the first page of the embedded `lines`.
  - `numbers_count_per_invoice`: two notes on one invoice get numbers ending `-CN-01` and `-CN-02`.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test credit_notes -j 2`. Expected: FAIL.
- [ ] **Step 3: Implement Q8,** including its `Action` variants and the credit-note pagination-write arm.
- [ ] **Step 4: Run them.** Run `cargo test -p ductkit-stripe-mock --test credit_notes -j 2`, then the full suite. Expected: PASS.
- [ ] **Step 5: Commit:** `feat(stripe-mock): credit notes`.

### Task 8: Subscription schedules

**Files:**
- Create: `src/model/schedule.rs`, `src/generate/schedules.rs`
- Modify: `src/model/subscription.rs` (`NewSubscription.schedule`), `src/generate/subscriptions.rs` (`Renew` hook, early-end hooks in `cancel_subscription`, `delete_customer_cascade`, `ExpireIncomplete` and `DunningRetry`), `src/config.rs` (`actions.schedule_subscription`), `src/generate/mod.rs`
- Test: `tests/subscription_schedules.rs` (new)

**Interfaces:**
- Produces:
  - `pub enum ScheduleEndBehavior { Release, Cancel }` (serde `snake_case`);
  - `schedules::on_renew(engine: &mut Engine, ctx: &mut Ctx, sub: &str, cause: &Cause) -> RenewStep`, where `enum RenewStep { Continue, Stop }`;
  - `schedules::end_early(engine: &mut Engine, sub: &str, aborted: bool, cause: &Cause)`.

- [ ] **Step 1: Write the failing tests** in `tests/subscription_schedules.rs`. Use control actions with `payment_failure_probability = 0` unless a test says otherwise.
  - `release_walks_every_phase`: `ScheduleSubscription { end_behavior: Some(Release) }`.
    - At each phase boundary, `subscription_schedule.updated` fires.
    - The subscription's items carry the new phase's prices, and the next invoice bills them.
    - After the last phase, `subscription_schedule.released` fires, and the subscription is still `active` with `schedule: null`.
    - The schedule has `released_subscription`, `subscription: null` and `released_at`.
  - `cancel_completes_and_cancels`: `end_behavior: Cancel`. At the end, `customer.subscription.deleted`, then `subscription_schedule.completed`, and no renewal invoice after it.
  - `phase_dates_follow_the_interval`: every phase's `start_date` equals the previous phase's `end_date`. Each `end_date` is `add_intervals(start, interval, iterations)`.
  - `early_cancel_cancels_the_schedule`: `CancelSubscription { at_period_end: Some(true) }` on a scheduled subscription cancels it now, and the schedule goes `canceled` (`subscription_schedule.canceled`).
  - `failed_dunning_aborts`: `payment_failure_probability = 1.0` after activation. When the subscription is canceled by dunning, `subscription_schedule.aborted` fires and the status is `canceled`.
  - `customer_deletion_cancels_schedules`.
  - `cancel_schedule_action`: `CancelSubscriptionSchedule` gives the schedule `canceled` and the subscription `canceled`, in that order of events.
  - `renewals_off_freezes_phases`: with `renew_subscription_cycle = "off"`, stepping past a phase end changes nothing.
  - `expand_subscription_schedule`: `GET /v1/subscriptions/{id}?expand[]=schedule` embeds the schedule while it is active.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test subscription_schedules -j 2`. Expected: FAIL.
- [ ] **Step 3: Implement Q9,** including its `Action` variants and the schedule pagination-write arm.
- [ ] **Step 4: Run them.** Run `cargo test -p ductkit-stripe-mock --test subscription_schedules -j 2`, then the full suite. Expected: PASS, and the replay guard passes.
- [ ] **Step 5: Commit:** `feat(stripe-mock): subscription schedules`.

### Task 9: Conformance coverage, money consistency, spec sweep

**Files:**
- Modify: `tests/openapi_conformance.rs`, `tests/charge_patterns.rs` (`connect_objects_land_in_their_scope`), `docs/stripe-mock-spec.md` (sections 4, 5.1, 5.5, 6.1, 7.2, 7.3, 11, 14, 16, 17), and the design doc (only where the build differs, each change ledgered)

- [ ] **Step 1: Extend the tests:**
  - **`busy_connect_run_conforms`:**
    - Turn on every new rate, plus `catalog_change` and `start_subscription`, which they need.
    - Assert all 33 types are served, `CreditNoteLineItem` through its sub-list and `Discount` embedded in customers and subscriptions.
    - Assert these values appear: schedule `released`, `completed` and `canceled`; credit note `issued` and `void`; promotion code `active: false`; tax rate `active: false`; invoice item pending and invoiced; and a coupon deleted.
    - The expand pass covers the Q10 paths.
  - **`served_objects_carry_their_documented_fields`:** add the 8 new types, each excluded field commented with its reason (R1).
  - **`billing_money_moves_consistently`:** every Review Focus 1 invariant, over the busy run's truth state, in every scope.
  - **`connect_objects_land_in_their_scope`:** the new types, in a connected account's scope.
  - **`payment_lists_survive_faults`:** add `/v1/invoiceitems` and `/v1/credit_notes`.
- [ ] **Step 2: Run them.** Run `cargo test -p ductkit-stripe-mock --test openapi_conformance -j 2`. Expected: PASS if Tasks 2–8 are right. A failure is a bug in the owning task's code: fix it, add a regression test in that task's test file, and ledger it.
- [ ] **Step 3: Bring the spec in line:**
  - 4: new keys;
  - 5.1: a phase 9 table with prefixes, delete styles and relations;
  - 5.5: phase 9 marked built, with Q2–Q9 in prose;
  - 6.1: the invoice amount sequence;
  - 7.2: the endpoints, filters and sub-list;
  - 7.3: the new expand paths and the coupon lookup;
  - 11: the control actions;
  - 14: the coverage run's new assertions;
  - 16: phase 9 done;
  - 17: Q1–Q12 as decisions.
- [ ] **Step 4: Run the full suite,** then `grep -c $'\r' docs/stripe-mock-spec.md`. Expected: PASS, and `0`.
- [ ] **Step 5: Commit:** `test(stripe-mock): OpenAPI conformance coverage for phase 9`.
