# Reference consumer harness: design

Status: draft · Crate: `crates/stripe-mock` (tests only)

## Intent

The mock's tests prove that each fault fires, but nothing proves the faults actually break a sync, or that a sync can still converge despite them. This harness closes that gap. It runs two consumers, a naive one and a correct one, against one faulty run, and compares each with the truth.

It is an acceptance test of the mock. The future `crates/sync` engine gets its own end-to-end test later, which can reuse the driver and the check.

- **Goal:** the naive consumer provably ends up wrong. The correct one provably ends up exactly right after quiesce.
- **Non-goal:** a real sync engine, an SQL target, the standalone binary, or a real Stripe SDK.
- **Anti-goal:** a test that passes or fails depending on luck in timing or the seed.

## Approaches

- **(a) In-process acceptance test (picked).** It uses `MockStripe::start`, the virtual clock and `Receiver` webhook servers. It is fast and deterministic.
- **(b) A scaffold over the standalone binary, behind a consumer trait.** It is closer to how `crates/sync` will be tested, but it runs on wall time and is slower. Deferred.

(a) was picked because it gives the mock its acceptance test now. Keeping the driver, the consumers and the final check separate leaves room for (b) later.

## Design

**Files:**
- `tests/reference_consumer.rs` holds the tests.
- `tests/common/consumer.rs` holds the two consumers and the final check (a submodule of the existing `common`).

**The run:**
- Fixed seed.
- Platform plus 2 connected accounts.
- 2 days of history (so backfill sees objects whose events are older than the retention window), then about 6 simulated hours stepped in small increments.
- Actions limited to the billing core: new customer, start/cancel subscription, catalog change, and customer, product, price and subscription churn including deletes.

**Faults on:**
- the Events API: visibility lag with a late tail, same-second shuffle;
- webhooks: duplicates, reorder window, an explicit `["30m"]` retry schedule;
- the receiver: it fails the first attempt of about a third of events, chosen by a deterministic hash of the event ID, so their retries carry outdated snapshots;
- the API: stale reads and pagination writes, plus a little 429/5xx.

**Compared set:** customers, products, prices, subscriptions and invoices, keyed by `(scope, type, id)`. These have plain lists, so backfill is simple. Other types the run creates are left out on both sides.

**Naive consumer:**
- Snapshot destination with `connect = true`. It upserts `data.object` blindly and removes the object on `*.deleted`. No dedupe, no ordering checks.
- A poller of `/v1/events?ending_before=<newest seen>`, applied the same blind way.
- An initial backfill of each list in each scope.

**Correct consumer:**
- Initial backfill of each list in each scope, retrying on `Stripe-Should-Retry` and on resets or timeouts.
- Thin destination with `events_from = ["@self", "@accounts"]`. Each event is only a hint: it GETs `related_object.url`, using `Stripe-Context` when the event has a `context`. It stores the result, or removes the object on a deleted stub or a 404.
- A final reconcile after quiesce. It is needed because refetches can be stale reads, and Stripe objects carry no version, so a consumer can't tell a stale refetch from a fresh one.

**End of run:**
1. `quiesce`.
2. Step past `stale_read.max_lag`. Generation is off, so nothing changes, but reads become fresh.
3. The correct consumer reconciles: a full re-list.
4. The truth is taken from `dump_state` (or the subscription), filtered to the compared set.

**Assertions:**
- Correct consumer: deep-equal to the truth, key for key.
- Naive consumer: differs from the truth. The test counts the divergences by kind (stale version, missing, resurrected after delete, present but deleted in truth) and asserts at least one stale entry and at least one missing entry, so the failure is the expected one, not an accident. A failure message prints these counts and the seed.
- Hold a `ReplayGuard` so a failing run leaves a replayable export.

**Decisions taken while planning and building:**
- **Location.** The harness lives in its own test target, `tests/reference_consumer/` (`main.rs` plus modules), not in `tests/common/consumer.rs`. Putting it in `common` would compile it into all 52 test targets.
- **The naive consumer's asserted failures are *stale* and *resurrected*.** A failed first attempt is always retried, so the naive consumer eventually receives every event.
  - Its *missing* entries come only from treating `customer.subscription.deleted` (a cancel) as a delete.
  - Random churn almost never updates a customer just before deleting it, so the run scripts this halfway through: 6 platform customers are updated and deleted a minute later. That makes resurrection deterministic for the seed.
- **The `timeout` API fault stays off.** It holds a response for 80 s of real time.
- **Subscriptions are backfilled with `status=all`.** The default list hides canceled ones.
- **History is 1 day, not 2,** to fit the time budget. History events are never delivered, so backfill still sees objects that no event announced.
- **The fault sections are separate from the base scenario (`SCENARIO_BASE` + `FAULTS`),** because TOML forbids a table twice.
- **Some changes announce no event.** A customer's `next_invoice_sequence` changes silently when an invoice is created (`src/generate/billing.rs:372`), as on real Stripe. So refetching on events alone never converges, even with stale reads off, and the final reconcile is needed for silent changes as well as stale reads.
  - Because the reconcile replaces the copy, the main test can't see the consumer's event handling. A second test therefore runs with stale reads off and no reconcile (`thin_refetches_lag_only_behind_silent_changes`).
  - It asserts that every object is at its last announced state or later, with nothing missing, resurrected or unrecognized.
  - Dropping `Stripe-Context` makes it fail (208 missing), and so does keeping a deleted stub as an object (`other`).
- **The scripted update-then-delete is adaptive.** It re-updates each customer until the update event is one whose first attempt fails, and checks that at least one delete gets through first time. So resurrection doesn't depend on the seed.
- **No normalization was needed.** REST objects equal truth states for all five compared types.

**Left to the builder:**
- The exact rates and seed. Pick a seed that triggers each asserted divergence kind, and keep the test under about 20 s.
- Whether REST objects and truth states compare equal for every compared type. They do for customers (`history.rs:166`). If a type differs, for example in embedded list truncation, normalize both sides the same way and document why.
- The conformance check on responses stays on: use `common::get`.

Approved design → implementation plan (superpowers:writing-plans).
