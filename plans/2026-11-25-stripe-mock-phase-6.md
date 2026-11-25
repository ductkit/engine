# Stripe Mock Phase 6 (Standalone Server, Control API, Export and Replay) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mock can run as a standalone server (`ductkit-stripe-mock serve --config mock.toml`) and is driven over HTTP:
- an HTTP control API on its own port;
- the latest-state subscription over SSE and WebSocket;
- any run can be exported as a replayable script and replayed to the same truth log.

**Architecture:**
- **One op type for everything that changes a run from outside.** A serde-tagged `ControlOp` enum covers every mutating control call: create customer, run action, connect account, config changes, destinations, step, quiesce and so on.
  - `MockStripe` records each op it applies, with the simulated time and order, in a `RunJournal`. It also records pagination writes fired by API requests, because they mutate truth.
  - Scripts, the HTTP control API and replay all apply ops through one function, `MockStripe::apply(op)`. Nothing can drift between them.
- **Export is seed + config + journal.**
  - `RunExport` holds the effective start config, serialized, plus the journal.
  - Replay starts a virtual-clock mock from that config. For each op it advances to the op's `at` and then applies it. The truth log and event timeline come out identical.
- **The control API is a second axum router on a second listener.** It is only started when asked for (`StartOptions.control_addr`), so in-process tests keep today's single port. The binary always starts both.

**Tech Stack:** The phase 5 stack, plus:
- axum's `ws` feature;
- tokio's `signal` feature;
- reqwest's `stream` feature, for the SSE client in tests;
- `tokio-tungstenite` as a dev-dependency, for the WebSocket client in tests.

**Spec:** [docs/stripe-mock-spec.md](../docs/stripe-mock-spec.md). Phase 6 is item 6 of section 16: "Standalone server binary, SSE/WebSocket subscription, HTTP control API, run export and replay." The main references are:
- sections 3 and 3.1 (two run modes, separate ports, library plus thin binary);
- 6.2 (scripts, and "a failing random run can be exported as a script");
- 10.3 and 10.4 (message format, SSE `GET /control/subscribe?account=…&types=…` and WebSocket on the control port);
- 10.5 (delivery log);
- 11 (control API);
- 12 ("Replay: any run can be reproduced from its exported seed, config and runtime changes. Test failures should print the seed and the replay command.");
- 17, decision 8.

## Global Constraints

- **Separate ports** (spec 3): the Stripe API and the control API never share a listener. The Stripe router has no `/control` routes, and the control router has no Stripe routes.
- **Packaging** (spec 3.1): one crate. The binary `src/main.rs` (command `ductkit-stripe-mock`) is a thin wrapper and holds only argument parsing and process wiring. Everything else is library code (CLAUDE.md: "Keep logic in libraries; binaries should stay thin").
- **The message format is spec 10.3 exactly.** SSE and WebSocket carry the existing `StateUpdate` serialization unchanged, plus `synced` and `lagged` markers.
- **Determinism** (spec 12):
  - Replaying an export of a virtual-clock run gives a byte-identical truth log (`serde_json` of every `StateUpdate`). It also gives an identical event timeline: IDs, types, `created` and `visible_at`.
  - A wall-clock run replays to the same truth log, because ops carry their simulated times.
  - Delivery outcomes depend on the receivers, so the delivery log is not part of the replay guarantee.
  - Journalling adds no RNG draws.
- **Read-only Stripe surface** (spec 17, decision 2): the control API may mutate, and the Stripe API stays read-only.
- **Dependencies:** versions only in the root `[workspace.dependencies]`. Add no CLI-parsing crate; the binary parses its few flags by hand.
- **Process:**
  - `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` are clean before every commit.
  - Run clippy and the suite as separate steps **before** `git commit`.
  - Commits end with the attribution footer.
- **Branching:** branch `stripe-mock-phase-6`, created from `main` in the existing checkout. Never use a worktree.
- **Windows:** keep LF line endings. Python edits use `newline=''`. `docs/stripe-mock-spec.md` is parsed by a config test.

## Review Focus

1. **A wall-clock run replays to the same truth log.** Runtime changes made while time runs on the wall clock land between ticks. Replay applies them at their recorded `at` after running everything due up to that instant, and must reproduce the order. Test `wall_run_replays_to_same_truth` (Task 2).
2. **A request that triggers a pagination write is part of the run.** A run with `api.pagination_writes` firing replays identically without any HTTP requests during replay. Test `pagination_writes_replay` (Task 2).
3. **A slow SSE or WebSocket client never slows the mock.** It gets `lagged` and the stream closes, exactly like the in-process subscription, and generation keeps its pace. Test `slow_sse_client_gets_lagged` (Task 4).
4. **Control API errors are well-formed and never 500.** Bad JSON, an unknown op, a wrong clock mode (`step` in wall mode), time travel, an unknown ID and an invalid config each return a 4xx with the JSON error envelope. Test `control_errors_are_4xx_json` (Task 3).
5. **The server shuts down cleanly on Ctrl-C or SIGTERM.**
   - It closes both ports.
   - It ends SSE and WebSocket streams.
   - When `--export-on-exit <path>` is given, it writes the export.

   Test `binary_exits_cleanly_and_exports` (Task 5). The test sends the shutdown request through the control API (`POST /control/shutdown`), because signals are awkward on Windows.

## Decisions made in this plan (not in the spec)

- **Scripts are op lists.** `MockStripe::run_script(&[ControlOp])` and `POST /control/script` apply a list of `ControlOp`s in order. An exported run *is* a script: its journal is a `Vec<JournalEntry { at, op }>`.
  - Spec 6.2's projection-level script operations (`deliver_in_order`, `make_visible_after`) are **out of scope**. They need delivery-queue hooks this phase does not build. Phase 7 or later can add them as new `ControlOp` variants.
- **What is journalled.** Every op that changes truth, the projection or delivery:
  - customer create, update and delete, with `account`;
  - `run_action`;
  - connect and disconnect account;
  - each config update, recorded as the full resulting section, because closures can't be serialized;
  - destination add, remove, enable, outage start and end, and secret rotation;
  - `quiesce` and `resume_generation`;
  - `step` and `advance_to`, recorded as the time they reached;
  - pagination writes fired by API requests.

  What is not journalled:
  - `pause`, `resume` and `set_speed`, which only shape wall time; ops carry their own `at`;
  - `force_rate_limit` and other API-only faults, which never touch truth;
  - every read.
- **Replay applies each op at its `at`.** Before applying an op, replay advances virtual time to `at`, running every action and delivery due at or before `at`. A live run does the same before a control call. This matches virtual mode exactly and wall mode up to the tick boundary.
  - Wall-mode control calls must call `shared.tick()` before applying, so the live run has also run everything due by `at`. Some already do; make all of them do it.
- **Replay is always virtual.** `MockStripe::replay(export, options)` forces `clock.mode = virtual`. After the last op it advances to `export.ended_at`, the simulated time when the export was taken, and stops there. Destinations from the export are kept, so deliveries are attempted again against the same URLs. `ReplayOptions { drop_destinations: bool }` removes them for offline replays.
- **Export format**, as JSON:
  ```
  {
    "format": "ductkit-stripe-mock/run@1",
    "seed": …,
    "config": <Config as JSON>,
    "ended_at": "<RFC 3339 ms>",
    "journal": [{"at": "<RFC 3339 ms>", "op": {...}}]
  }
  ```
  - `config` is the config the run **started** with.
  - Replay rejects an unknown `format` with `MockError::Config("unsupported run format …")`.
- **Failure helper.** `MockStripe::replay_guard(&self, dir: impl AsRef<Path>) -> ReplayGuard`. On drop during a panic (`std::thread::panicking()`), the guard writes `<dir>/run-<seed>-<ended_at ms>.json` and prints to stderr:
  ```
  stripe-mock: run exported to <path>; replay with: ductkit-stripe-mock replay --run <path>
  ```
  This satisfies spec 12's "test failures should print the seed and the replay command" (the seed is in the file name).
- **Effective config.** `MockStripe::effective_config() -> Config` is the start config with `[api]`, `[events]`, `[webhooks]` and `[actions]` replaced by their current runtime values, and `destinations` by the current list. `GET /control/config` serves it.
- **Control API ports and addresses:**
  - `StartOptions { api_addr: Option<SocketAddr>, control_addr: Option<SocketAddr> }` with `MockStripe::start_with(config, options)`. `start(config)` is `start_with(config, StartOptions::default())`. The default API address is `127.0.0.1:0`. Without a `control_addr`, no control port is opened.
  - The binary defaults are `--api-addr 127.0.0.1:8420` and `--control-addr 127.0.0.1:8421`.
  - The control API has no authentication. It binds to loopback by default, and the binary prints a warning to stderr when `--control-addr` is not a loopback address.
- **Standalone clock default is wall** (spec 3). If the config file has no `clock.mode` key, the binary inserts `mode = "wall"` before parsing. `--clock virtual|wall` overrides both.
- **Ready line.** Once both listeners are bound, the binary prints one JSON line to stdout:
  ```
  {"api_url":"http://…","control_url":"http://…","seed":…}
  ```
  Tests and scripts read it, which makes port `0` usable.
- **Subscribe query parameters** (spec 10.4):
  - `types=customer,subscription_item`: API object names, comma-separated.
  - `account=platform,acct_…`: comma-separated; `platform` means the `None` scope.
  - Either may be absent, which means all.
  - An unknown type, or an account that is unknown or disconnected, returns 400 before the stream starts.
- **SSE framing:**
  - `event: update` with `data:` the `StateUpdate` JSON;
  - `event: synced` with `data: {}`;
  - `event: lagged` with `data: {}`, followed by the end of the stream;
  - a `: keep-alive` comment every 15 s of wall time.
- **WebSocket framing:** one text frame per item: `{"type":"update","update":{…}}`, `{"type":"synced"}` or `{"type":"lagged"}`. After `lagged`, the server sends a close frame. Messages from the client are ignored.
- **Control error envelope:** `{"error": {"code": "<snake_case MockError variant>", "message": "<Display>"}}`. Status by variant:
  - `NoSuchObject`, `NoSuchDestination` and `UnknownAccount` → 404;
  - `ClockMode`, `TimeTravel`, `Deleted`, `InvalidAction` and `ConnectDisabled` → 409;
  - `Config` and body or JSON errors → 400;
  - anything else → 500. The test asserts that the listed cases never fall through to 500.

## Control API routes (Task 3)

All routes are under `/control`. Bodies are JSON. `{d}` is a `DurationStr` string such as `"90s"`, and times are RFC 3339.

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
| `GET /config` | – | `effective_config` |
| `POST /rate_limit` | `{"duration": d}` | `force_rate_limit` |
| `GET /destinations` | – | `destinations` |
| `GET /accounts` | – | `accounts` |
| `GET /objects/{type}/{id}` | `?account=` | `get` / `account(id).get`; `null` → 404 |
| `GET /state` | `?account=&types=` | `dump_state` |
| `GET /truth_log` | `?after_seq=&limit=` (default 1000, max 10000) | slice of `truth_log` |
| `GET /events` | `?after=<evt id>&limit=` | slice of `event_timeline` |
| `GET /deliveries` | `?limit=` | the last `limit` entries of `delivery_log` |
| `GET /export` | – | `export_run` |
| `GET /subscribe`, `GET /subscribe/ws` | `?types=&account=` | SSE / WebSocket (Task 4) |
| `POST /shutdown` | – | 202, then a graceful shutdown of the whole server (Task 5 wires the process side) |

Mutating calls other than `step`, `advance_to`, `quiesce`, pause, resume and speed go through `POST /ops`. That gives one route, one schema, and the same shape as scripts and the journal.

## File Structure

- `src/control/mod.rs` (new): the `ControlOp` and `OpResult` types and `MockStripe::apply` (Task 2).
- `src/control/journal.rs` (new): `RunJournal`, `JournalEntry`, `RunExport`, replay, `ReplayGuard` (Task 2).
- `src/control/http.rs` (new): the control router and error mapping (Task 3).
- `src/control/sse.rs` (new): SSE and WebSocket subscription handlers (Task 4).
- `src/config.rs`: `Serialize` on every config type, plus a round-trip test (Task 1).
- `src/model/customer.rs`, `src/generate/mod.rs`, `src/handle.rs`: `Serialize`/`Deserialize` on `NewCustomer`, `CustomerUpdate`, `Action`, `AccountApi`, `InvoiceOutcome` and `AccountInfo`; `dump_state`, `effective_config`, `set_speed` (Task 1).
- `src/handle.rs`: journal hooks, `start_with`, `StartOptions`, `control_url` (Tasks 2 and 3).
- `src/main.rs` (new), `src/cli.rs` (new, in the library: `run_cli(args) -> ExitCode` and argument parsing) (Task 5).
- Tests (new):
  - `tests/serialize.rs`
  - `tests/replay.rs`
  - `tests/control_http.rs`
  - `tests/control_stream.rs`
  - `tests/binary.rs`
  - `tests/standalone_e2e.rs`

---

### Task 1: Serializable control types, state dump, effective config, speed

**Files:**
- Modify: `src/config.rs`, `src/model/customer.rs`, `src/generate/mod.rs`, `src/handle.rs`, `src/lib.rs`
- Create: `tests/serialize.rs`

**Interfaces:**
- Produces:
  - **`Serialize` on every config type** (`Config` and every nested section and enum). `DurationStr` serializes back to its compact form: the largest unit that divides evenly, so 90 000 ms → `"90s"`, 3 600 000 → `"1h"`, 1 500 → `"1500ms"`.
    - Invariant: `Config::from_toml_str(&toml::to_string(&c)?)` and `serde_json::from_value::<Config>(serde_json::to_value(&c)?)` both equal `c`. Derive `PartialEq` on `Config` if it is missing.
  - **`Serialize` + `Deserialize`** on `NewCustomer`, `CustomerUpdate`, `Action`, `InvoiceOutcome`, `AccountApi`, `AccountKind`, `Configuration` and `AccountInfo`.
    - Enums are internally tagged with `#[serde(tag = "type", rename_all = "snake_case")]` where they carry fields. Unit-only enums use `rename_all = "snake_case"`, and `AccountApi` uses `"v1"` / `"v2"`.
    - Existing `Serialize` types are unchanged.
  - **`MockStripe::dump_state(&self, filter: &SubscribeFilter) -> serde_json::Value`:**
    ```
    {"at": time, "scopes": [{"account": null | "acct_…", "objects": {"<type>": [state, …]}}]}
    ```
    - Latest states only, live objects only (no tombstones). Objects are in creation order.
    - Scopes are in `BTreeMap` order (platform first). Types are in `ObjectType::ALL` order, with empty types omitted.
    - It is spec 11's "snapshot state to JSON".
  - **`MockStripe::effective_config(&self) -> Config`**, as in Decisions.
  - **`MockStripe::set_speed(&self, speed: f64) -> Result<(), MockError>`:** wall mode only (`ClockMode` error otherwise). `speed` must be finite and > 0 (`Config` error otherwise). It re-anchors like `resume`, so simulated time does not jump.

- [ ] **Step 1: Write failing tests** in `tests/serialize.rs`:
  - `config_round_trips`:
    - `Config::default()` round-trips.
    - So does a config with every section set to non-default values. Include `[history]`, `[accounts] connect = true` with each mix, two destinations (snapshot `connect = true` and thin `events_from = ["@self","@accounts"]`), `[api]` faults including `latency` and `rate_limit`, and webhooks retries.
    - Both round-trips must pass: TOML and JSON.
  - `duration_serializes_compactly`: `90s`, `1h`, `1500ms` and `2d` come back as `"90s"`, `"1h"`, `"1500ms"` and `"2d"`.
  - `actions_round_trip`: every `Action` variant, in both its `None` and `Some` forms, plus every `CustomerUpdate` variant and `NewCustomer::default()`, round-trips through `serde_json`. Assert the tag for one value: `serde_json::to_value(Action::NewCustomer)? == json!({"type": "new_customer"})`.
  - `dump_state_lists_live_objects`:
    - Create two customers and delete one.
    - Connect one account (`connect = true`) and create a customer in it.
    - `dump_state(&SubscribeFilter::default())` has 2 scopes. The platform scope has exactly 1 customer plus 1 account object. The account scope has 1 customer.
    - With `accounts: Some(vec![None])`, there is only the platform scope.
  - `effective_config_reflects_updates`:
    - After `update_api_config(|c| c.stale_read.probability = 0.5)` and `add_destination(..)`, `effective_config()` has those values.
    - It also round-trips through TOML.
  - `set_speed_needs_wall`: in virtual mode, `set_speed(2.0)` → `MockError::ClockMode`.
  - `set_speed_changes_rate`: in wall mode, `set_speed(0.0)` → `Config` error. Then, with `speed = 1`, `set_speed(3600.0)`, wait 200 ms of wall time, and `now()` has advanced by 9 to 15 minutes of simulated time.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test serialize`. Expected: compile errors (no `Serialize` on `Config`, missing methods).
- [ ] **Step 3: Implement** the interfaces above.
  - Where a config type uses a custom `Deserialize` (such as `DurationStr`, or the latency distributions), write the matching `Serialize` by hand so the round trip holds.
  - Fields that are `Option` and absent must stay absent: use `skip_serializing_if = "Option::is_none"`, so the `UNIMPLEMENTED_KEYS` checks never see a key the user did not write.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): serializable config and control types, state dump, set_speed`

---

### Task 2: Control ops, run journal, export, replay, scripts

**Files:**
- Create: `src/control/mod.rs`, `src/control/journal.rs`, `tests/replay.rs`
- Modify: `src/handle.rs`, `src/lib.rs`, `src/api/resources.rs` and `src/api/accounts.rs` (journal pagination writes), `src/generate/mux.rs` (if the pagination-write hook lives there)

**Interfaces:**
- Consumes: Task 1's serde types and `effective_config`.
- Produces:
  - **`ControlOp`**, `#[serde(tag = "op", rename_all = "snake_case")]`:
    ```
    pub enum ControlOp {
        CreateCustomer { account: Option<String>, customer: NewCustomer },
        UpdateCustomer { account: Option<String>, id: String, update: CustomerUpdate },
        DeleteCustomer { account: Option<String>, id: String },
        RunAction { account: Option<String>, action: Action },
        ConnectAccount { api: AccountApi },
        DisconnectAccount { id: String },
        SetApiConfig { config: ApiConfig },
        SetEventsConfig { config: EventsConfig },
        SetWebhooksConfig { config: WebhooksConfig },
        SetActionsConfig { config: ActionsConfig },
        AddDestination { destination: DestinationConfig },
        RemoveDestination { id: String },
        SetDestinationEnabled { id: String, enabled: bool },
        StartOutage { id: String, duration: DurationStr },
        EndOutage { id: String },
        RotateSecret { id: String, overlap: DurationStr },
        Step { duration: DurationStr },
        AdvanceTo { time: DateTime<Utc> },
        Quiesce,
        ResumeGeneration,
        PaginationWrite { account: Option<String>, object_type: ObjectType },
    }
    ```
    `PaginationWrite` is journal-only: `apply` rejects it with `InvalidAction("pagination_write is recorded by the mock, not applied by clients")`, while replay runs it. `account: None` means the platform. Missing `account` keys deserialize to `None`.
  - **`OpResult`**, `#[serde(tag = "result", rename_all = "snake_case")]`, one of:
    - `Done`
    - `Customer { id }`
    - `Version { version: Option<u64> }`
    - `Cause { cause: Cause }`
    - `Account { id }`
    - `Destination { destination: DestinationInfo }`
    - `Secret { secret }`
  - **`MockStripe::apply(&self, op: ControlOp) -> Result<OpResult, MockError>`** (async, because `step`/`quiesce` are async).
    - It calls the existing methods.
    - The existing public methods now record their own op into the journal on success. `apply` therefore does not record it a second time.
    - Ops that fail are not recorded.
  - **`MockStripe::run_script(&self, steps: &[ControlOp]) -> Result<Vec<OpResult>, ScriptError>`** with `pub struct ScriptError { pub completed: usize, pub error: MockError }`.
  - **`pub struct JournalEntry { pub at: SimTime, pub op: ControlOp }`.** The journal lives in `Sim` (under the sim lock) as `Vec<JournalEntry>`. Each method records with `at = engine.now()` after its wall `tick()`, under the same lock acquisition as the mutation, so order matches truth.
    - `step` and `advance_to` record `AdvanceTo { time: reached }`.
  - **Pagination writes.** When a pagination write fires, the API handler records `PaginationWrite { account, object_type }` before running it.
  - **`pub struct RunExport { pub format: String, pub seed: u64, pub config: Config, pub ended_at: SimTime, pub journal: Vec<JournalEntry> }`** (`Serialize`, `Deserialize`).
    - `MockStripe::export_run(&self) -> RunExport`.
    - `RunExport::save(&self, path: &Path) -> io::Result<()>` (pretty JSON) and `RunExport::load(path) -> Result<RunExport, MockError>`.
  - **`pub struct ReplayOptions { pub drop_destinations: bool }`** (`Default`).
  - **`MockStripe::replay(export: RunExport, options: ReplayOptions) -> Result<MockStripe, MockError>`:**
    - It starts with the export's config, with the clock forced to virtual and, if `drop_destinations` is set, `destinations` emptied.
    - For each entry it runs `advance_to(at)` and then the op. `PaginationWrite` runs the write in its scope through the generators.
    - It then advances to `ended_at`.
    - With `drop_destinations`, journalled destination ops are skipped. Later ops naming those IDs are then skipped too, so replay succeeds.
  - **`MockStripe::replay_guard(&self, dir: impl AsRef<Path>) -> ReplayGuard<'_>`**, as in Decisions.

- [ ] **Step 1: Write failing tests** in `tests/replay.rs`. Each test compares `serde_json::to_string(&truth_log())` and `(id, type, created, visible_at)` of `event_timeline()` between the original run and the replay.
  - `virtual_run_replays_exactly`:
    - Spec-example rates at one tenth, `connect = true` with `initial = 2` and nonzero rates, `history.duration = "6h"`.
    - Mix control calls with steps:
      - `step` 2 h;
      - `create_customer`;
      - `account(a).update_customer`;
      - `run_action(StartSubscription)`;
      - `update_actions_config` (double `new_customer`);
      - `add_destination` (snapshot, `connect = true`, to an unreachable URL `http://127.0.0.1:9`);
      - `start_outage`;
      - `connect_account(V2)`;
      - `disconnect_account`;
      - `step` 3 h;
      - `quiesce`;
      - `resume_generation`;
      - `step` 1 h.
    - `replay(export_run(), default)` gives an identical truth log and timeline.
  - `wall_run_replays_to_same_truth` (Review Focus 1):
    - Wall mode at `speed = 3600`. Run about 1.5 s of wall time with `create_customer` and `run_action` calls in between, then `pause()` and `export_run()`.
    - The replay's truth log equals the original's.
  - `pagination_writes_replay` (Review Focus 2):
    - `api.pagination_writes = { probability = 1.0 }`.
    - Page `/v1/customers` 3 times with `starting_after` (platform), and `/v1/customers` with a cursor as a connected account.
    - The journal contains 2 or more `PaginationWrite` entries, one with `account: Some`. The replay has an identical truth log, with no HTTP requests made during replay.
  - `export_round_trips_through_file`: `save` then `load` gives an equal `RunExport`. `format == "ductkit-stripe-mock/run@1"`. A file with `"format": "other"` → `MockError::Config` whose message contains `unsupported run format`.
  - `run_script_applies_in_order`:
    - Script `[CreateCustomer{..}, Step{"1h"}, RunAction{StartSubscription{customer: None, prices: None}}, DisconnectAccount{id: "acct_nope"}]`.
    - Result: `ScriptError { completed: 3, error: UnknownAccount }`, with the first 3 ops present in the journal.
  - `pagination_write_op_is_rejected`: `apply(PaginationWrite{..})` → `InvalidAction`.
  - `replay_guard_writes_on_panic`:
    - Inside `std::panic::catch_unwind(AssertUnwindSafe(..))`, create a guard on a temp dir (`std::env::temp_dir()` joined with a unique name) and panic.
    - The dir then holds exactly one `run-*.json` that loads.
    - Without a panic, it writes nothing.
  - `drop_destinations_replays`: replaying `virtual_run_replays_exactly`'s export with `drop_destinations: true` succeeds and has the same truth log. `destinations()` is empty.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test replay`. Expected: compile errors.
- [ ] **Step 3: Implement.**
  - Make every wall-mode control method `tick()` before mutating, if it does not already.
  - Recording must not draw from any RNG stream.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green, with golden and determinism tests unchanged.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): control ops, run journal, export and replay`

---

### Task 3: HTTP control API

**Files:**
- Create: `src/control/http.rs`, `tests/control_http.rs`
- Modify: `src/handle.rs`, `src/lib.rs`, `tests/common/mod.rs` (add a control-client helper)

**Interfaces:**
- Consumes: Task 2's `apply`, `run_script`, `export_run`; Task 1's `dump_state`, `effective_config`, `set_speed`.
- Produces:
  - `pub struct StartOptions { pub api_addr: Option<SocketAddr>, pub control_addr: Option<SocketAddr> }` (`Default`).
  - `MockStripe::start_with(config, options)`.
  - `MockStripe::control_url(&self) -> Option<String>`.
  - `MockStripe::shutdown` also closes the control listener.
  - `MockStripe::shutdown_requested(&self) -> impl Future<Output = ()>`: resolves when `POST /control/shutdown` is called. The binary awaits it (Task 5).
  - The routes in the table above, with the error envelope and status mapping from Decisions.
  - `GET /control/truth_log` returns `{"data": [StateUpdate], "has_more": bool}`.
  - `/events` and `/deliveries` use the same envelope.

- [ ] **Step 1: Write failing tests** in `tests/control_http.rs`, using `start_with` with `control_addr = 127.0.0.1:0`:
  - `ports_are_separate`:
    - `GET {api_url}/control/now` → 404.
    - `GET {control_url}/v1/customers` → 404.
    - `GET {control_url}/control/now` → 200 with the mock's `now`.
    - With `start()`, `control_url()` is `None`.
  - `ops_route_applies_and_journals`:
    - `POST /control/ops {"op":"create_customer","customer":{}}` → 200 `{"result":"customer","id":"cus_…"}`.
    - The customer is then visible on the Stripe API.
    - `GET /control/export` has that op in its journal.
  - `step_and_advance`:
    - `POST /control/step {"duration":"1h"}` moves `now` by 3600 s.
    - `POST /control/advance_to` with an earlier time → 409, `code: "time_travel"`.
  - `control_errors_are_4xx_json` (Review Focus 4). Each case has a JSON `error.code` and `error.message`:
    - malformed JSON → 400;
    - `{"op":"nope"}` → 400;
    - `step` on a wall-mode mock → 409 `clock_mode`;
    - `GET /control/objects/customer/cus_nope` → 404;
    - `POST /control/ops` with `set_api_config` and probability 2.0 → 400 `config`;
    - `disconnect_account` with an unknown ID → 404 `unknown_account`.
  - `script_route_reports_partial`: a script whose third step fails → 404 or 409 with `"completed": 2`. The first two ops applied.
  - `inspection_routes`:
    - `/control/state` equals `dump_state(&default)`.
    - `/control/config` deserializes to `Config` and equals `effective_config()`.
    - `/control/truth_log?after_seq=0&limit=5` has 5 entries and `has_more: true`, and the next page continues at seq 6.
    - `/control/accounts` and `/control/destinations` equal the handle's lists.
  - `quiesce_route`: `POST /control/quiesce` → 200, then `GET /control/generation` → `{"enabled": false}`. After `POST /control/generation/resume` it reports `true`.
  - `shutdown_route`: `POST /control/shutdown` → 202, and `shutdown_requested()` resolves within 1 s.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test control_http`. Expected: compile errors.
- [ ] **Step 3: Implement** the router in `control/http.rs`, sharing `Arc<Shared>`. Handlers are thin: parse, call the `MockStripe`-level logic (move it to `Shared` methods where a handler needs it without a `MockStripe`), and map errors.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): HTTP control API on a separate port`

---

### Task 4: SSE and WebSocket subscription

**Files:**
- Create: `src/control/sse.rs`, `tests/control_stream.rs`
- Modify: root `Cargo.toml` (axum `ws` feature; reqwest `stream` feature; `tokio-tungstenite` in `[workspace.dependencies]`), `crates/stripe-mock/Cargo.toml` (`tokio-tungstenite` dev-dependency), `src/control/http.rs` (routes)

**Interfaces:**
- Consumes: `MockStripe::subscribe_with` / `Engine::subscribe` and `SubscribeOptions` (bounded buffer), Task 3's router.
- Produces: `GET /control/subscribe` (SSE) and `GET /control/subscribe/ws` (WebSocket), with the query and framing rules in Decisions.
  - Both use the same `Subscription` type as in-process, with the default `SubscribeOptions`.
  - They end when the mock shuts down.
- **Dependency version:** use the `tokio-tungstenite` version that axum 0.8.9's `ws` feature already resolves (check with `cargo tree -p ductkit-stripe-mock -e features -i tokio-tungstenite` after enabling `ws`), so only one copy is built. Enable its default `connect` feature for the test client only.

- [ ] **Step 1: Write failing tests** in `tests/control_stream.rs`:
  - `sse_snapshot_then_live`:
    - Create 3 customers, then open `GET /control/subscribe?types=customer` with reqwest `bytes_stream`.
    - Parse SSE frames: 3 `update` events with `kind: "snapshot"`, then `synced`.
    - After `create_customer`, one `update` with `kind: "created"`.
    - Every `data` parses as JSON with the spec 10.3 keys: `seq`, `at`, `account`, `object_type`, `object_id`, `version`, `kind`, `state`, `events`, `cause`.
  - `sse_filters_by_account`:
    - With `connect = true` and one account holding a customer, `?account=platform` never shows an update whose `account` is non-null.
    - `?account=<acct>` shows only that account.
    - `?account=acct_nope` → 400 JSON error.
    - `?types=nope` → 400.
  - `ws_snapshot_then_live`: the same as the first test over `tokio_tungstenite::connect_async`, with text frames `{"type":"update",…}` then `{"type":"synced"}`.
  - `slow_sse_client_gets_lagged` (Review Focus 3):
    - Subscribe with SSE and do not read.
    - Generate more updates than the buffer holds (`run_action(NewCustomer)` in a loop, more than the default buffer size times 2, then `step`).
    - The handle's `step` duration stays under 2 s of wall time while the client is stalled.
    - Reading the stream then yields some updates, then `lagged`, then the end of the stream.
  - `streams_end_on_shutdown`: an open SSE stream and an open WebSocket both end (EOF or close frame) within 1 s of `shutdown()`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test control_stream`. Expected: 404 on the routes, or compile errors.
- [ ] **Step 3: Implement.**
  - SSE: `axum::response::sse::{Sse, Event, KeepAlive}` with a 15 s keep-alive.
  - WebSocket: `axum::extract::ws`.
  - Map `SubscriptionItem` to frames in one function shared by both.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): SSE and WebSocket latest-state subscription`

---

### Task 5: The `ductkit-stripe-mock` binary

**Files:**
- Create: `src/main.rs`, `src/cli.rs`, `tests/binary.rs`
- Modify: root `Cargo.toml` (tokio `signal` feature), `crates/stripe-mock/Cargo.toml` (`[[bin]] name = "ductkit-stripe-mock"`, `path = "src/main.rs"`), `src/lib.rs`

**Interfaces:**
- Consumes: Tasks 2–4.
- Produces:
  - `pub fn cli::run(args: Vec<String>) -> std::process::ExitCode`, called by `main` inside a tokio multi-thread runtime. `main.rs` holds nothing else.
  - **Commands.** `--help` prints usage. Unknown flags exit with code 2 and a usage message.
    ```
    ductkit-stripe-mock serve --config <file> [--api-addr A] [--control-addr A] [--clock virtual|wall] [--seed N] [--export-on-exit <file>]
    ductkit-stripe-mock replay --run <file> [--api-addr A] [--control-addr A] [--drop-destinations] [--export-on-exit <file>]
    ductkit-stripe-mock check --config <file>
    ```
    - `--seed` overrides `seed`.
    - `replay` replays to `ended_at`, then serves the replayed state in virtual mode. It can be driven further through the control API.
    - `check` validates a config and prints `ok`, or the error with exit code 1.
  - **Config loading.** It uses `Config::from_toml_str`, after inserting `clock.mode = "wall"` when the key is absent (Decisions). A config error exits 1 with the message on stderr.
  - **Ready line.** It prints the ready line (Decisions) to stdout, then flushes.
  - **Shutdown.** The server shuts down on Ctrl-C, on SIGTERM (unix), or on `POST /control/shutdown`, whichever comes first. It writes the export if `--export-on-exit` was given, then calls `MockStripe::shutdown` and exits 0.

- [ ] **Step 1: Write failing tests** in `tests/binary.rs`. Spawn `env!("CARGO_BIN_EXE_ductkit-stripe-mock")`, write configs to a unique temp dir, and read the ready line from stdout with a 10 s timeout.
  - `serve_prints_ready_line_and_serves`:
    - Run `serve --config <minimal> --api-addr 127.0.0.1:0 --control-addr 127.0.0.1:0`.
    - The ready line parses. `GET {api_url}/v1/customers` with a test key → 200, and `GET {control_url}/control/now` → 200.
  - `standalone_defaults_to_wall`:
    - With no `[clock]` in the file, `GET /control/now` twice, 300 ms apart, at `speed` 60, shows simulated time moving.
    - `POST /control/step` → 409 `clock_mode`.
    - `--clock virtual` → `step` works and time does not move on its own.
  - `binary_exits_cleanly_and_exports` (Review Focus 5):
    - With `--export-on-exit <file>`, `POST /control/ops` a customer, then `POST /control/shutdown`.
    - The process exits 0 within 5 s, both ports refuse connections, and the export file loads. Its journal has the customer op.
  - `replay_command_serves_replayed_state`:
    - Take the export from the previous test (or make one in-process with `export_run().save`).
    - `replay --run <file> --api-addr 127.0.0.1:0 --control-addr 127.0.0.1:0` starts.
    - `GET /control/truth_log` (all pages) equals the export's original truth log, recorded in-process in the test.
  - `check_reports_errors`: `check` on a valid config → exit 0, stdout `ok`. On a config with `[api] stale_read = { probability = 2.0 }` → exit 1, and stderr contains `stale_read`.
  - `bad_flags_exit_2`: `serve --nope` → exit 2, and stderr contains `usage`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test binary`. Expected: build error (no binary target).
- [ ] **Step 3: Implement** `cli.rs` and `main.rs`. Argument parsing is a small hand-written loop over `args`; no new dependency.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): ductkit-stripe-mock serve, replay and check commands`

---

### Task 6: Standalone end-to-end and spec

**Files:**
- Create: `tests/standalone_e2e.rs`
- Modify: `docs/stripe-mock-spec.md`, `CLAUDE.md` (Commands: how to run the server)

**Interfaces:**
- Consumes: everything above.
- Spec edits:
  - **Section 3:** the binary flags, the ready line, the default ports, the wall default and `check`.
  - **Section 6.2:** scripts are `ControlOp` lists. Projection-level script ops are deferred.
  - **Section 10.4:** the query parameters, the SSE and WebSocket framing, `lagged` then close, and keep-alive.
  - **Section 11:** a route table that points to the plan's routes (copy the table, not the plan), the error envelope, `POST /control/shutdown`, and that the control API has no auth and binds to loopback.
  - **Section 12:**
    - the export format and what is journalled;
    - the replay guarantee (truth log and event timeline; not the delivery log);
    - `ReplayGuard` and the printed replay command.
  - **Section 16:** mark phase 6 as done only if the file already marks earlier phases; otherwise leave it.

- [ ] **Step 1: Write failing tests:**
  - `tests/standalone_e2e.rs::drive_subscribe_export_replay`:
    - Spawn the binary: `serve`, virtual clock, spec-example rates at one tenth, `connect = true` with `initial = 2`, and one snapshot destination pointing at a local axum receiver started in the test (`connect = true`).
    - Open an SSE subscription, then drive with `POST /control/step` for 2 h, `POST /control/ops` (connect a v2 account, create a customer in it), then `step` 2 h and `POST /control/quiesce`.
    - Assertions:
      1. The latest state per `(account, object_type, object_id)` built from SSE updates equals `GET /control/state` once it is flattened.
      2. The receiver got at least one delivery carrying `account`.
      3. `GET /control/export`, saved and then run as `replay --run`, gives a `GET /control/truth_log` identical to the original's.
  - `spec_documents_control_api`: the existing spec-parsing test still passes. A new assertion checks that the spec contains `POST /control/ops` and `ductkit-stripe-mock/run@1`.
- [ ] **Step 2: Run** `cargo test -p ductkit-stripe-mock --test standalone_e2e` and the config tests. Expected: the e2e test passes or fails on real gaps (fix them in the owning code). The spec assertion fails.
- [ ] **Step 3: Edit the spec and CLAUDE.md.** Keep LF line endings, then check `git diff --stat` and that `grep -c $'\r' docs/stripe-mock-spec.md` prints 0.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: green.
- [ ] **Step 5: Clippy, then commit:** `feat(stripe-mock): standalone e2e and spec for control API, streams, replay`
