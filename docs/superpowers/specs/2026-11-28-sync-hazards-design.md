# Sync hazard tests: design

Status: approved · Crate: `crates/stripe-mock` (tests only) · Builds on `2026-11-28-reference-consumer-design.md`

## Intent

The reference consumer harness shows that the mock's faults break a naive sync, but only as aggregate counts. These tests pin each sync-breaking hazard the audit listed in its own small, named test. If a mock change removes a hazard, the test that names it fails.

- **Goal:** for each hazard, assert three things:
  1. it really happens in what a sync can observe;
  2. the naive handling gets it wrong;
  3. where a simple correct handling exists, that handling gets it right.
- **Non-goal:** a sync engine, new mock behaviour, or changes to `src/`.
- **Anti-goal:** an assertion that depends on luck in the seed or in real-HTTP timing.

## Approaches

- **(a) A module in the existing `reference_consumer` test target (picked).** It reuses `Client`, `HookServer`, `Truth` and `common`.
- **(b) A separate test target.** It would duplicate or move the shared modules.
- **(c) Extend the big harness run.** Hazards would stay aggregate counts, which is what this work replaces.

(a) was picked because it shares the helpers at no cost and keeps each hazard isolated.

## Design

**Where:** `tests/reference_consumer/hazards.rs`, declared from `main.rs`. Each test is a small scripted run:
- virtual clock;
- no history and no generated actions, except where a test needs them;
- customers driven by `create_customer`, `update_customer` and `delete_customer`.

**The six tests:**
1. **`stale_retry_carries_older_version`.**
   - Update a customer until the update event is one whose first attempt fails (`fails_first`), then update it again.
   - The retry's body is the older state.
   - The delivery log's retry `Attempt` has `object_version < latest_version`.
   - Applying bodies in arrival order (naive) ends on the older state. Keeping the event with the newest `created`, with ties broken by arrival, ends on the latest.
2. **`delete_before_create_under_reorder`.**
   - Create and delete many customers a second apart, with a wide `reorder_window`.
   - In at least one pair, the `customer.deleted` delivery is scheduled before its `customer.created`. This is read from the delivery log's `Scheduled.attempt_at`, not from arrival, which real HTTP doesn't order.
   - A naive upsert, applying in scheduled order, keeps the customer. A consumer that remembers deleted IDs drops it.
3. **`ending_before_poller_misses_late_events`.**
   - Use a high `late_tail` probability.
   - A loop that polls `/v1/events?ending_before=<newest seen>` each step misses some events.
   - A poller that re-reads `created[gte] = newest_created − late_tail.max` and dedupes by ID sees every event once quiesce has made them visible.
4. **`same_second_events_list_out_of_truth_order`.**
   - Turn `same_second_shuffle` on and apply several updates to one customer in one instant.
   - `/v1/events` lists them out of truth order for some burst; retry bursts until one does.
   - Replaying the list oldest-first ends on a state that is not the latest.
5. **`thin_fetch_after_delete_returns_stub`.**
   - A thin `v1.customer.updated` arrives, and the customer is deleted before the fetch.
   - `GET related_object.url` returns 200 with `deleted: true`.
   - Storing the body as an object yields an `other` entry; treating it as gone is correct.
6. **`stale_read_regresses_after_webhook`.**
   - Use `stale_read` probability 1.
   - After a webhook for version N, a retrieve returns an older version.
   - So a refetch-on-event consumer goes backwards mid-run. This pins the finding about the "no regression" rule.

**Left to the builder:** the exact counts, windows and lags; how adaptively to retry a burst or pair, bounded so it fails loudly rather than looping forever.
