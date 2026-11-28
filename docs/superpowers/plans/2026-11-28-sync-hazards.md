# Sync hazard tests: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Six named tests. Each one proves that one sync-breaking hazard happens in what a sync can see, that naive handling gets it wrong, and, where one exists, that a simple correct handling gets it right.

**Architecture:** A new module `hazards.rs` in the existing `tests/reference_consumer` target. It reuses:
- `Client` / `Fetched` (client.rs);
- `HookServer` / `fails_first` (hooks.rs);
- `Truth` (truth.rs);
- `scenario::destinations` and `scenario::last_snapshot_event`.

Each test is a small scripted run on the virtual clock.

**Tech Stack:** Rust 2024, tokio, reqwest, serde_json; the `ductkit_stripe_mock` public API.

**Spec:** `docs/superpowers/specs/2026-11-28-sync-hazards-design.md`. Mock behaviour: `docs/stripe-mock-spec.md`, sections 7.4, 8.1 and 9.

## Global Constraints

- Tests only; no change to `crates/stripe-mock/src/`.
- **Every cargo command uses `-j 4`.** Higher job counts crash rustc 1.97.1 on this machine.
- `cargo fmt --all` and `cargo clippy -p ductkit-stripe-mock --all-targets -j 4 -- -D warnings` must be clean before each commit.
- No assertion may depend on luck in the seed or in real-HTTP arrival order. Where timing is drawn, pick adaptively with a bound and fail loudly, or read the mock's schedule (the delivery log) instead of arrival.
- **These tests pin behaviour the mock already has, so "RED" means a deliberate fault-off run.** Each task runs its test once with the hazard's config turned off (named in the task) and confirms it fails, then restores the config and confirms it passes.
- Commits go on branch `test/sync-hazards`. Messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- The test command is `cargo test -p ductkit-stripe-mock --test reference_consumer -j 4 hazards::`.

## Review Focus

1. **Two updates in the same `created` second.** A newest-`created` rule can't order them. Task 1 steps 2 s between the two updates so that its correct rule is well defined, and says why in a comment.
2. **Webhook arrival order.** Real HTTP doesn't fix the order within a step. Task 2 therefore reads `Scheduled.attempt_at` from the delivery log rather than arrival, and Task 1 relies only on a 30-minute gap.
3. **A deleted stub vs a 404 on a thin fetch.** A customer returns a 200 stub. Task 5 asserts the raw status and body, not only `Fetched::Gone`.
4. **A stale read showing state from before the object existed.** It would be a 404, not an older version. Task 6 creates the customer more than `max_lag` before the update, so a stale read always lands on version 1.
5. **The late tail pushing an event past the final poll.** Quiesce makes every event visible (spec section 11), so Task 3 polls once more after quiesce before comparing.

---

### Task 1: Module scaffold and `stale_retry_carries_older_version`

**Files:**
- Create: `crates/stripe-mock/tests/reference_consumer/hazards.rs`
- Modify: `crates/stripe-mock/tests/reference_consumer/main.rs` (`mod hazards;`)
- Modify: `crates/stripe-mock/tests/reference_consumer/scenario.rs` (make `last_snapshot_event` `pub`)

**Interfaces:**
- Consumes:
  - `scenario::destinations(snapshot_url, thin_url) -> String`
  - `scenario::last_snapshot_event(m, object_id, event_type) -> String` (made `pub` here)
  - `hooks::{HookServer, fails_first}` and `HookServer::take_new(&mut usize) -> Vec<Value>`
  - `Truth`
- Produces, in `hazards.rs`:
  - `async fn quiet_mock(extra: &str, snapshot: &HookServer, thin: &HookServer) -> MockStripe`. It builds `seed = 3` + `extra` + `scenario::destinations(...)` with no generated actions.
  - `fn state_at(m: &MockStripe, id: &str, version: u64) -> Value`: the truth-log state of `id` at `version`.

- [ ] **Step 1: Write `stale_retry_carries_older_version`.**
  - Use `extra = "[webhooks]\nretry_schedule = [\"30m\"]\n"`.
  - Create customer `c` and step 1 s.
  - Update `c` with `SetName(Some(format!("n{i}")))` until the newest `customer.updated` satisfies `fails_first`, at most 30 tries. Call that event `stale`.
  - Step 2 s (so the two updates have different `created` seconds; Review Focus 1).
  - Update with `SetEmail` until the newest `customer.updated` does *not* satisfy `fails_first`, at most 30 tries. Call it `fresh`.
  - Step 1 h, then `quiesce`.
  - Let `bodies` be the snapshot hook's accepted events about `c`, in arrival order.
  - Assertions:
    - The delivery log has an `Attempt { attempt: 2, object_version, latest_version, .. }` for `stale`, with `object_version < latest_version`.
    - `stale`'s accepted body has `data.object == state_at(m, c, object_version)`, which differs from the latest truth state.
    - Naive: `bodies.last()`'s `data.object` is not the latest truth state.
    - Correct: the body with the greatest `(created, arrival index)` has `data.object` equal to the latest truth state.
- [ ] **Step 2: Run it and check that it fails** (compile error: no `hazards` module and private `last_snapshot_event`).
- [ ] **Step 3: Implement the scaffold** as specified in Interfaces.
- [ ] **Step 4: Run it and check that it passes.** Then run it with `retry_schedule = ["1s"]` and confirm it fails: the retry lands before `fresh`. Restore `30m`.
- [ ] **Step 5: Commit.** `test(stripe-mock): a retried webhook carries an older version`

### Task 2: `delete_before_create_under_reorder`

**Files:**
- Modify: `crates/stripe-mock/tests/reference_consumer/hazards.rs`

**Interfaces:**
- Consumes: `quiet_mock`; `m.delivery_log()` (`DeliveryRecordKind::Scheduled { attempt_at, .. }`, `event_id`); `m.event_timeline()` (`id`, `format`, `event_type`, `object_id`).

- [ ] **Step 1: Write `delete_before_create_under_reorder`.**
  - Use `extra = "[webhooks]\nreorder_window = \"10s\"\n"`.
  - 20 times: create a customer, step 1 s, delete it, step 60 s.
  - Then `quiesce`.
  - For each snapshot `customer.created` / `customer.deleted` event, take its first `Scheduled.attempt_at`.
  - Assertions:
    - At least one customer's delete is scheduled before its create.
    - Applying the events in `attempt_at` order:
      - with a naive upsert (create inserts, delete removes), at least 1 ghost customer is left;
      - with a consumer that remembers deleted IDs and ignores later creates of them, the set is empty.
- [ ] **Step 2: Run it** with `reorder_window = "0s"` and confirm it fails (no inversion).
- [ ] **Step 3: Restore `10s`** and confirm it passes.
- [ ] **Step 4: Commit.** `test(stripe-mock): reordering delivers a delete before its create`

### Task 3: `ending_before_poller_misses_late_events`

**Files:**
- Modify: `crates/stripe-mock/tests/reference_consumer/hazards.rs`

**Interfaces:**
- Consumes: `Client::get(path, None) -> Fetched`; `m.event_timeline()`.

- [ ] **Step 1: Write `ending_before_poller_misses_late_events`.**
  - Use `extra`:
    - `[actions] new_customer = 30.0`
    - `[events] late_tail = { probability = 0.5, min = "1m", max = "10m" }`
  - For 120 ticks, step 1 min, then run both pollers.
  - **Naive poller:** `/v1/events?limit=100&ending_before=<newest id>` (the first poll has no cursor). It adds the IDs and moves the cursor to `data[0]`.
  - **Overlap poller:** walks `/v1/events?limit=100&created[gte]=<newest created − 600>` with `starting_after`, adds every ID to a set, and tracks the greatest `created`.
  - Then `quiesce` and poll both once more (Review Focus 5).
  - Let `all` be the timeline's snapshot event IDs.
  - Assert `naive ⊊ all` (print how many were missed) and `overlap == all`.
- [ ] **Step 2: Run it** with `late_tail` probability `0.0` and confirm it fails (naive == all).
- [ ] **Step 3: Restore `0.5`** and confirm it passes.
- [ ] **Step 4: Commit.** `test(stripe-mock): an ending_before poller misses late events`

### Task 4: `same_second_events_list_out_of_truth_order`

**Files:**
- Modify: `crates/stripe-mock/tests/reference_consumer/hazards.rs`

- [ ] **Step 1: Write `same_second_events_list_out_of_truth_order`.**
  - Use `extra = "[events]\nsame_second_shuffle = true\n"`.
  - Create `c`, then step 2 s.
  - Up to 20 bursts:
    - apply 4 `SetName` updates with no step between them;
    - note the burst's event IDs from the timeline, in truth order;
    - GET `/v1/events?limit=100&created[gte]=<burst second>`, filter it to those IDs, and reverse it (oldest first);
    - if the replay's last event is not the burst's last, stop; otherwise step 2 s and repeat.
  - Assertions:
    - An inverted burst was found.
    - The replay's last `data.object` differs from `c`'s latest truth state.
- [ ] **Step 2: Run it** with `same_second_shuffle = false` and confirm it fails (no inversion in 20 bursts).
- [ ] **Step 3: Restore `true`** and confirm it passes.
- [ ] **Step 4: Commit.** `test(stripe-mock): same-second events list out of truth order`

### Task 5: Thin fetch after delete, and stale-read regression

**Files:**
- Modify: `crates/stripe-mock/tests/reference_consumer/hazards.rs`

**Interfaces:**
- Consumes: `common::get(m, path) -> reqwest::Response` (2xx bodies are conformance-checked), `Client::get`, `Truth::from_log`, `Truth::classify`, `state_at`.

- [ ] **Step 1: Write `thin_fetch_after_delete_returns_stub`.**
  - Use `extra = ""`.
  - Create `c`, then step 1 min.
  - Update `c`, step 1 min, delete `c`.
  - From the thin hook, take the `v1.customer.updated` for `c`.
  - Assertions:
    - `common::get(m, related_object.url)` is 200 with `deleted == true`.
    - `Client::get` returns `Fetched::Gone`.
    - A `Synced` holding that body under `c`'s key classifies with `c`'s key in `other`.
- [ ] **Step 2: Write `stale_read_regresses_after_webhook`.**
  - Use `extra = "[api]\nstale_read = { probability = 1.0, max_lag = \"10m\" }\n"`.
  - Create `c`, step 20 min (beyond `max_lag`; Review Focus 4), update `c` (version 2), step 1 s.
  - The snapshot hook has the `customer.updated` with `data.object == state_at(m, c, 2)`.
  - Retrieve `/v1/customers/{c}` up to 10 times until a body equals `state_at(m, c, 1)`.
  - Assert one did.
- [ ] **Step 3: Check that both tests can fail.**
  - Run `thin_fetch_after_delete_returns_stub` without the delete, and confirm it fails.
  - Run the stale-read test with `probability = 0.0`, and confirm it fails.
  - Restore both, and confirm both pass.
- [ ] **Step 4: Run the full checks:**
  - `cargo fmt --all`;
  - `cargo clippy -p ductkit-stripe-mock --all-targets -j 4 -- -D warnings`;
  - `cargo test -p ductkit-stripe-mock -j 4`.

  All pass.
- [ ] **Step 5: Commit.** `test(stripe-mock): a thin fetch after delete gets a stub; a stale read regresses`
