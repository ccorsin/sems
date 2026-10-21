# Development journal — SEMS (Station Energy Management System)

This journal traces the approach step by step: start with a simple version, then improve it.
Each decision is recorded with the alternatives considered and the reason for the choice.
The summary below gives the whole path at a glance; the steps that follow give the detail.

## Summary

**Goal.** Distribute a station's power across charging sessions without ever exceeding the grid,
charger or connector limits (`grid > charger > connector > vehicle`), react in well under 1 s to
every event, and expose it all through an API in Docker.

**Architecture.** Rust with actix-web. The station lives in memory behind a single lock
(`Mutex<Station>`): every event recomputes the whole allocation, which keeps the global
invariant "Σ ≤ grid" simple, in microseconds. Powers are integer watts rounded down, so no sum
can exceed a limit by rounding. The allocator is a pure function, separate from the API.
Channels follow who speaks first: chargers send events and acknowledgements over HTTP; the
server pushes setpoints over a WebSocket per charger (`/chargers/{id}/setpoints`, also readable
by a plain GET). OPS reads `/status` (connector and charger availability, power availability at
station, charger and session level) and the history `/events`, where each event is stored with
the allocation that resulted from it.

**Versions.**

| Version | What it does | Result |
|---|---|---|
| V1 | Capped equal share: `min(demand, connector, grid / n, charger / n_charger)`; REST API, `/status`, `/events`, WebSocket push, Docker | Safe, but up to 166.7 kW unused in the scenarios |
| V2 | Max-min fair water-filling, first inside each charger, then across the grid | 0 kW unused in every scenario; end-to-end latency unchanged |
| V3.1 | Decrease before increase: decreases are published at once; increases wait until every charger with a pending decrease acknowledges its latest version (`POST /chargers/{id}/ack`, version = event `seq`) | 0 kW of transition overshoot on every transition; a charger that never acknowledges freezes increases (documented limitation) |
| V3.2 | OPS view completed: each charger's `online` status (a setpoint WebSocket is open) with `online_since_ms`, and available power per charger | Every item the brief lists for OPS is in `/status`; going offline releases nothing |
| V4 | Optional BESS: capacity = grid + its power above a 10 % SoC (reported by the BMS); one flow at a time (discharge the excess, else charge the headroom, else idle); the BESS regulates the grid connection locally | Vehicles get up to the BESS power on top of the grid; the grid import stays within 0..=grid, never an export |

**Tests.** Unit tests on configuration, model and allocator; 25 declarative JSON scenarios, each
with its `why`, replayed through the API with four properties checked after every request
(safety, transition, history, consistency) and allocation metrics per scenario; a seeded
randomized test (random stations and events, including invalid requests and late, stale, future
or missing acknowledgements, random BESS states of charge, with a shadow model predicting every
HTTP code); push tests against
a real server; performance tests. Every test family was verified against deliberate bugs.

**Figures** (2017 MacBook Pro, native release build): an event on one charger reaches another
charger's WebSocket in under 0.7 ms (the requirement is 1 s); about 5,000 events/s under a
concurrent burst with the safety invariant checked on every response.

**Assumptions.** Power modelled in kW, continuously divisible (no stacks); one server per
station; a setpoint received is applied at once; state in memory; no authentication. BESS: the
SoC is an input from the BMS; the BESS applies its mode at once and never exports.

**Next.** A timeout for a charger that never acknowledges; hysteresis around the BESS SoC
floor; minimum viable power per session.

---

## Step 0 — Analysis and scoping (2026-10-21)

### The problem

The "brain" of a charging station: distribute the available power across active sessions
without ever exceeding physical limits, reacting to events in under one second. Everything
is exposed through an API running in a Docker container.

Deliverables: a clean, documented repository; a README (architecture, trade-offs, how to run,
how to test); justified test scenarios that the reviewer can replay.

### Power constraints (clarified with the PM)

They cascade, and all of them apply at the same time:

```
grid > charger > connector > vehicle
```

- **Grid**: a hard cap (site safety), fixed value in the config. The sum of setpoints must never
  exceed it, **including during transitions**.
- **Charger**: its power is shared between its connectors. A single vehicle may take the full
  charger power; two vehicles on the same charger share it.
- **Two distinct contention levels**:
  - two vehicles on the *same* charger share that charger's power;
  - two vehicles on *different* chargers only share the grid.
- **Vehicle**: reports an instantaneous limit (kW) that may go **up or down** at any time.
  No charging curve, no SoC.

**kW vs kWh**: kW is power (a rate), kWh is energy (a quantity). Without a BESS, energy is
unlimited and only power is constrained, so V1 deals with kW only. kWh will only appear with
the BESS (tracking SoC over time).

### What the system must cover (summary of the PM discussions)

1. Recomputation triggered by **3 event types**: connect, disconnect, demand change.
2. Compliance with **2 nested constraint levels**: charger and grid.
3. **No waste**: released power must benefit the other sessions (V2).
4. **OPS visibility**: who is charging, at what power, what is still available, how it evolves.
5. **Minimal traceable history**: events and the resulting state.
6. *(Bonus)* BESS: boost, 10% SoC floor, opportunistic charging.

### Technical decisions

| Topic | Decision | Alternatives rejected and why |
|---|---|---|
| Language | Rust | — |
| Dev machine | 2017 MacBook Pro: no `brew` installs, lean dependencies | — |
| HTTP stack | `actix-web` (default features off, `macros` only) + `actix-ws` + `serde` — *revised before increment 2, initially `axum`* | `axum`: equivalent for our needs and lighter to build, but the developer already masters actix-web, and "understand every line" matters more than build time at equal runtime performance. `tiny_http` (less standard); std only (too much hand-written code). **Measured** on the dev machine (first debug build / incremental / release build / release binary): axum + ws 87 crates, 49 s / 1 s / 53 s / 0.8 MB; actix-web default features 187 crates, 96 s / 2.2 s / 140 s / 4.8 MB; **actix-web without default features** (no compression, cookies, HTTP/2) **155 crates, 59 s / 1.5 s / 77 s / 2.2 MB**. Acceptable. |
| Triggers | HTTP REST requests as input | OCPP/WebSocket, MQTT, Modbus, gRPC: realistic but oversized for the assignment. Cited as targets in the README. |
| Setpoints to chargers | **WebSocket push** from V1 (hybrid: REST in, WS out) | Pull (latency depends on the polling interval); SSE (one-way, further from OCPP); full WebSocket à la OCPP (cannot be tested with `curl`); webhook, gRPC, MQTT (too heavy). |
| State | In memory: immutable `Arc<Config>` + `Mutex<State>` | `RwLock` (negligible gain); `tokio::Mutex` (useless without `.await` under the lock); actor (more code, possible migration if the BESS requires it); `ArcSwap`, Redis (oversized). A **single** lock, because the "Σ ≤ grid" invariant is global. |
| Persistence | None in V1, **SQLite** later | JSONL (no queries); PostgreSQL (one more service for the reviewer). Rule: no I/O under the lock. |
| Units | **Integer watts internally, kW in the API**; setpoint granularity: 1 W | `f64`: rounding could exceed the cap by an epsilon. Setpoints are rounded down, so no overshoot by construction. 1 kW granularity (PM's example) rejected: up to 1 kW lost per session, i.e. artificial waste. Real chargers quantize (amperes, stacks); documented simplification. |
| Configuration | JSON file passed **as an argument** at startup | Recompiling; hot-reload configuration API (not required). |
| Port | Fixed 8080; the external port is chosen with `docker run -p` | Environment variable (unnecessary with Docker). |
| Logs | One line per event on stdout (`println!`) + history through the API | Logging crate (extra dependencies). |
| Session ID | UUID v4 generated by the server | Incremental counter. The UUID is the equivalent of OCPP's `transactionId`. |

### V1 algorithm — capped equal share

```
setpoint = min( demand, connector_max, grid / n_station, charger_max / n_charger )
```

`n_station` and `n_charger` count only sessions with a **non-zero demand**: a vehicle that is
still plugged in but no longer requests power (full battery) must not take a share.

- **Safe by construction**: each term is a safe share of its level, so no limit is ever exceeded.
- **Full recomputation** on every event: an increase or a decrease in demand goes through the
  same code.
- **Known (accepted) limitation**: unused power is not redistributed. Example: grid 500 kW,
  A and B on CP1 (300 kW), C on CP2, all requesting 300 kW. V1 gives 150 / 150 / 166, i.e.
  466 kW: 34 kW are wasted, whereas C could have taken 200 kW. This motivates V2 (water-filling).

Alternatives considered for later: max-min water-filling (chosen for V2), first come first
served (unfair), linear programming (oversized).

### Assumptions and accepted limitations

- **Stacks**: not modelled. Charger power is assumed to be continuously divisible.
- **Transitions**: chargers are assumed to apply setpoints instantly. Known risk: if a new
  session ramps up before another one has reduced, the grid is physically exceeded. Planned
  mitigation: "decrease before increase" (increases are held until the affected chargers have
  acknowledged their decrease).
- **Abrupt disconnection** (no `DELETE`): **no automatic release**, safety first. Releasing the
  power of a charger that is still delivering would overshoot the grid. The automatic release
  mechanism is still to be defined; meanwhile OPS can release a stuck session manually with
  `DELETE /sessions/{id}`. Note: in OCPP, a charger that loses communication falls back to a
  conservative setpoint on its own.
- **No authentication**: anyone reaching the API can create or delete sessions. Acceptable for
  the assignment, documented as a limitation.
- **kW instead of amperes** (confirmed by the PM): real chargers, especially AC ones driven
  through OCPP, are often controlled in amperes. The brief reasons in kW only, so the system does
  too; no volts/amperes/three-phase conversion. Deliberate simplification.
- **One server per station** (confirmed by the PM): the system is "the brain of a station".
  Multi-station management (shared grid constraints, central supervision) is out of scope.
- **OPS observes, does not act** (confirmed by the PM): live observation is a must-have. Actions
  (capping a charger, removing it from the allocation, changing the grid limit at runtime) are a
  natural evolution, not a deliverable.
- **Faults** (charger, connector, BESS): out of scope.
- **Minimum power per session**: ignored in V1. It does not arise with the brief's config
  (at least 100 kW per vehicle), but it matters for a smaller station. Planned: wait at 0 kW
  with a `suspended_evse` status visible to OPS.
- **In-memory state**: lost on restart.

### Edge cases considered

| Edge case | Status |
|---|---|
| Demand above connector or charger capacity | V1: capped |
| Local contention (charger saturated) / global contention (grid saturated) | V1 |
| Demand decrease: redistribute the remainder | V2 |
| Demand increase: grant within what is available | V1 (up to the equal share), V2 (at the expense of others if fairness allows) |
| Non-divisible power (stacks) | Documented limitation |
| Minimum viable power | V3 |
| Simultaneous or bursty events | V1: serialized by the `Mutex`, every intermediate state is safe |
| Abrupt disconnection | No automatic release (safety) |
| Consistent state for OPS during a recomputation | V1: reads take the same lock |
| Station with no vehicle / no BESS / heterogeneous chargers | V1 |
| Connector type (CCS / Type 2) | V1: informational |
| Vehicle plugged in but not charging | V1: `suspended_ev` status + `status_since_ms`; zero-demand sessions take no share |
| BESS: 10% floor, full BESS, vehicles vs BESS priority | V4 (vehicles first) |
| Faults | Out of scope |

### Roadmap

| Version | Content |
|---|---|
| **V1** | Config as argument, REST events, `/status`, `/events`, WebSocket push, capped equal share, tests, Docker |
| **V2** | Cascading water-filling (no waste) |
| **V3** | "Decrease before increase" (first: the strongest safety point), minimum power (`suspended_evse`), `offline` status |
| **V4** | BESS: boost, 10% SoC floor, opportunistic charging |
| Later | SQLite persistence (deprioritized: least value for the assignment) |
| Minor bonus | Grid margin / charger efficiency: the grid limits what is *drawn*, not what is *delivered* (charger losses ~5%, auxiliary loads). One config parameter. |
| Mention only | Vehicle priorities (fleets, subscribers); OPS actions (see assumptions) |

### V1 delivery plan

V1 is split into increments, each one working and committable. A usable deliverable exists from
increment 3 onwards, so running out of time never means shipping nothing.

1. Model and allocator (pure) + unit tests.
2. REST API + `/status` + scenario runner with the safety invariant.
3. Dockerfile + minimal README. → **first complete deliverable**
4. WebSocket push + its test.
5. `/events` with `result`.
6. Metrics + performance tests.

### Test strategy

Three levels, plus performance:

| Level | What | How |
|---|---|---|
| Unit | The pure allocation algorithm | `#[test]` in `allocator.rs` |
| Scenarios | End-to-end behaviour through the API: event sequences → expected state | Declarative JSON files in `scenarios/`, replayed by a Rust runner against the in-process router |
| Push | WebSocket clients receive new setpoints | Rust tests with `awc` (dev-dependency, see increment 4) |
| Performance | Latency, burst throughput, algorithm scaling | `tests/perf.rs`, `#[ignore]`, run with `cargo test --release -- --ignored` |

**Scenario format (option B)**, chosen over Rust-only tests (less readable for a non-Rust
reviewer), `curl` scripts (no assertions) and Hurl (an extra tool, no WebSocket support; may be
reconsidered later). Server-generated UUIDs are referenced through aliases (`"as": "A"`). The
`why` field is mandatory.

```json
{
  "name": "two_vehicles_same_charger",
  "why": "Local contention: two sessions on one charger share its 300 kW even though the grid has headroom.",
  "config": { "grid_kw": 400, "chargers": [ { "id": "CP1", "max_kw": 300, "connectors": [ {"id": 1}, {"id": 2} ] },
                                            { "id": "CP2", "max_kw": 300, "connectors": [ {"id": 1}, {"id": 2} ] } ] },
  "steps": [
    { "connect": { "as": "A", "charger": "CP1", "connector": 1, "demand_kw": 300 }, "expect": { "A": 300 } },
    { "connect": { "as": "B", "charger": "CP1", "connector": 2, "demand_kw": 300 }, "expect": { "A": 150, "B": 150, "allocated_kw": 300 } },
    { "disconnect": "A", "expect": { "B": 300 } }
  ]
}
```

In V1, behaviour depends only on the **order** of events, not on their timing. A simulated clock
(`"advance_time"` step) will be added with the BESS (V4), where SoC evolves over time.

**Continuous safety check**: the runner asserts `allocated ≤ grid` and `charger allocated ≤
charger max` after **every step of every scenario**. The same check runs on every response of the
burst test, and a `debug_assert!` enforces it right after each allocation (zero cost in release).

### V1 scenarios

Brief config unless stated otherwise: grid 400 kW, 2 chargers of 300 kW, 2 connectors each.

| # | Scenario | Why |
|---|---|---|
| S1 | Empty station | Consistent `/status`, full capacity available |
| S2 | Single vehicle | Gets `min(demand, connector, charger, grid)` |
| S3 | Two vehicles, same charger | Local contention |
| S4 | Two vehicles, different chargers | Global contention |
| S5 | One vehicle leaves | The remaining one takes the power back |
| S6 | Full station (4 vehicles) | No limit exceeded at any level |
| S7 | Demand decrease then increase | Symmetric recomputation |
| S8 | Demand above connector limit | Capped by the connector |
| S9 | Heterogeneous chargers | The config adapts to the infrastructure |
| S10 | V1 waste (grid 500 kW) | Documents the V1 limitation; expectations change in V2 |
| S11 | Errors: busy connector (409), unknown connector (404), negative demand (422), unknown session (404) | API robustness |
| S12 | Demand increase under contention: EV1 (CP1, 100) → EV2 (CP2, 300) → EV1 up to 300 → EV1 down to 50 | An increase is granted only within what is available; V1 does not redistribute after the decrease |
| S13 | Cascading release: 4 vehicles → EV1 leaves → EV3 drops to 20 → EV4 leaves | Release across several vehicles; V1 wastes 113 kW at step 3, V2 should not |
| S14 | Zero demand: EV1 and EV2 on different chargers, EV1 drops to 0 kW while staying plugged in | A full vehicle must not block power: EV2 gets its full demand (up to its charger limit) |
| S15 | Plugged in, not charging: EV1 drops to 0 kW, then back up | Status goes `charging` → `suspended_ev` → `charging`, `status_since_ms` updated at each change; the other vehicle gets the power meanwhile |

The safety invariant is not a scenario: it is checked automatically at every step.

### Performance tests

| # | Measure | Proves |
|---|---|---|
| P1 | End-to-end latency: `POST` for EV2 → EV1's charger WebSocket receives its new setpoint (p50 / p99 / max) | The actual requirement: adjustment reflected at the charger in < 1 s |
| P2 | Burst: thousands of concurrent demand changes | No degradation under load, no lock contention, invariant holds |
| P3 | Algorithm scaling: 4, 100, 10,000 sessions | Computation in µs, linear growth |

- A real server on a random local port; requests sent with `awc` (no default features,
  dev-dependency only, never in the shipped binary; initially `reqwest`, see increment 4).
- Assertion: `max < 1 s` (the requirement). Actual figures are printed and reported in the README
  (measured natively on a 2017 MacBook Pro; inside Docker on macOS a VM adds latency, so the
  measurement context is always stated). No tight threshold, to avoid flaky tests on slow machines.
- Rejected: `criterion` (heavy dependency), `wrk`/`oha`/`hey` (need `brew`, no WebSocket),
  `k6` (an extra tool and scripting language).

**Performance goal across versions: non-regression, not speed-up.** Versions improve allocation
*quality* (kW served), not latency, which is dominated by network/WebSocket (ms), not computation
(µs). Risks to watch: SQLite writes (must stay outside the lock) and "decrease before increase"
(deliberately slows increases down: reactivity traded for safety). Optimizations (recompute only
affected chargers, coalesce bursts, `RwLock`) will only be done if a measurement justifies them.

A realistic station has 4 to 20 connectors and a few events per second: these tests show the
headroom, not a limit we are likely to reach.

### Metrics (to compare versions)

Computed by the scenario runner from the state returned after each step; no server code involved.
The same scenarios are replayed on each version, and the results are archived in this journal.

| Metric | Definition | Unit | Highlights |
|---|---|---|---|
| Waste | `optimum − allocated` | kW | V2 |
| Efficiency | `allocated / optimum` | % | V2 |
| Satisfaction | `allocated / Σ demand` | % | V2 |
| Fairness (Jain index) | Over `setpoint / demand` ratios | dimensionless, 0–1 | V2 trade-off |
| Churn | Setpoints changed per event | setpoints/event | Cost of V2 |
| Potential transient overshoot | `max(0, Σ max(old, new) − grid)` | kW | V3 |
| Event → WS push latency | p50 / p99 / max | ms | Non-regression, cost of V3 |
| Burst throughput | Events processed per second | events/s | Non-regression |

The optimum is the maximum power physically deliverable under the nested constraints:

```
optimum = min( grid, Σ_chargers min( charger_max, Σ_sessions min(demand, connector_max) ) )
```

---

## V1 specifications

### Running

```
sems <path/config.json>          # listens on 0.0.0.0:8080
docker run -p <port>:8080 -v ./config:/config sems /config/station.json
```

The config is **validated at startup**. On error, the server prints a clear message and exits.
Detected errors: duplicate IDs, power ≤ 0, connector more powerful than its charger, charger
without connectors.

### Configuration

```json
{
  "grid_kw": 400,
  "chargers": [
    { "id": "CP1", "max_kw": 300,
      "connectors": [ { "id": 1, "type": "CCS" }, { "id": 2, "type": "CCS", "max_kw": 150 } ] }
  ]
}
```

- `connectors[].max_kw` is optional; it defaults to the charger's `max_kw`.
- `type` is informational and has no effect on allocation.

### Exposed quantities

| Field | Level | Meaning |
|---|---|---|
| `demand_kw` | session | What the vehicle requests (input) |
| `setpoint_kw` | session | What the system allows (output), always ≤ `demand_kw` |
| `allocated_kw` | charger, station | Sum of setpoints |
| `available_kw` | station | `grid_kw − allocated_kw` (grid headroom, not necessarily usable: a charger may be saturated) |
| `unserved_kw` | charger, station | `Σ (demand − setpoint)`: unmet demand. OPS diagnostics. |

### Events (chargers → server)

| Method | Route | Body | Success | Errors |
|---|---|---|---|---|
| `POST` | `/sessions` | `{"charger": "CP1", "connector": 1, "demand_kw": 150}` | `201` + response below | 404 unknown charger/connector, 409 connector busy, 422 invalid demand (< 0) |
| `PUT` | `/sessions/{id}` | `{"demand_kw": 80}` | `200` + response below | 404, 422 |
| `DELETE` | `/sessions/{id}` | — | `200` + response below | 404 |

Response: `{"session_id": "…", "setpoint_kw": 150, "status": { /* full state, see GET /status */ }}`.
Returning the full station state to a charger is a **convenience for the demo and tests**: in a
real system a charger should only see its own setpoints. Documented as such.

A demand of 0 kW is valid (vehicle full but still plugged in). Errors have the form
`{"error": "human-readable message"}`.

The WebSocket push and the HTTP response travel on two channels, so their arrival order is not
guaranteed; both carry the same values.

### Queries (OPS)

**`GET /status`** (also embedded as `status` in every event response):

```json
{ "grid_kw": 400, "allocated_kw": 300, "available_kw": 100, "unserved_kw": 300,
  "chargers": [
    { "id": "CP1", "max_kw": 300, "allocated_kw": 300, "unserved_kw": 300,
      "connectors": [
        { "id": 1, "type": "CCS", "max_kw": 300,
          "status": "charging", "status_since_ms": 1761040000000,
          "session": { "id": "9f1c…", "started_at_ms": 1761040000000,
                       "demand_kw": 300, "setpoint_kw": 150 } },
        { "id": 2, "type": "CCS", "max_kw": 150,
          "status": "available", "status_since_ms": 1761039000000, "session": null } ] } ] }
```

**Connector status** (names borrowed from OCPP). It tells OPS *why* a plugged-in vehicle is not
charging: the vehicle does not ask for anything, or the station cannot give it anything.

| `status` | Condition | Meaning for OPS | Version |
|---|---|---|---|
| `available` | no session | Free | V1 |
| `charging` | setpoint > 0 | Charging | V1 |
| `suspended_ev` | demand = 0 | Plugged in, the **vehicle** no longer requests power (battery full): candidate for idle fees | V1 |
| `suspended_evse` | demand > 0, setpoint = 0 | Plugged in, the **station** cannot give it power | V3 (minimum power) |

- The status is **derived** from demand and setpoint, never stored, so it cannot be inconsistent.
- `status_since_ms` is the time of the last status change ("plugged in without charging for
  45 min" is what OPS needs). Only that timestamp is stored.
- Ended sessions are available in the history.

**`GET /events?limit=100&session=<uuid>`**: the server keeps the last 10,000 events. `limit`
defaults to 100; `session` is an optional filter. Each entry holds **the event and the resulting
allocation**:

```json
{ "seq": 7, "at_ms": 1761040000000, "kind": "demand_changed",
  "session_id": "9f1c…", "charger": "CP1", "connector": 1, "demand_kw": 80,
  "result": { "allocated_kw": 230, "setpoints": [ { "session_id": "9f1c…", "kw": 80 } ] } }
```

`kind` values: `session_started`, `demand_changed`, `session_ended`.

This format covers the OPS needs: per-session history (`?session=`), station history (the
sequence of `allocated_kw`), and safety audit (`allocated_kw ≤ grid_kw` on every entry).

### Push (server → chargers)

**`GET /chargers/{id}/setpoints`** — *revised in increment 4, initially `/ws/chargers/{id}`*:
with a WebSocket upgrade, the server sends the current setpoints, then one message whenever a
setpoint of that charger changes; as a plain GET, it returns the current setpoints (pull
fallback). Every connector is listed, free ones at 0 kW with `session_id: null`.

```json
{ "charger": "CP1", "setpoints": [ { "connector": 1, "session_id": "9f1c…", "kw": 150 } ] }
```

### Internal architecture

```
src/
├─ main.rs       # startup: read config, start server
├─ model.rs      # config, sessions, state (pure)
├─ allocator.rs  # allocation algorithm (pure, no I/O)
└─ api.rs        # HTTP and WebSocket routes
```

- `web::Data<Mutex<Station>>` (config + sessions, see increment 1) + one `tokio::sync::watch`
  channel per charger for push.
- No I/O under the lock.
- Dependencies: `actix-web` (default features off), `actix-ws`, `serde`, `serde_json`, `uuid`.
  In-memory API tests use `actix_web::test` (no extra dependency).

---

## Step 1 — V1, increment 1: model and allocator (2026-10-21)

**Scope**: pure core, no server yet. `Cargo.toml` (serde, serde_json, uuid), `src/lib.rs`,
`src/model.rs`, `src/allocator.rs`, `src/main.rs` (loads and validates the config, prints a
summary), `config/station.json` (the brief's station).

**Result**: 16 unit tests pass, `clippy` clean, `cargo fmt` applied. First debug build: 22 s.

```
cargo test
cargo run -- config/station.json   # station: grid 400 kW, 2 chargers, 4 connectors
```

**Implementation choices and deviations from the specs**

- **`src/lib.rs` added** (not in the planned file list): the code lives in a library and
  `main.rs` is a thin binary. Integration tests (`tests/`, increments 2 and 6) can only import a
  library crate.
- **`Station` owns its `Config`** instead of a separate `Arc<Config>`: every status read needs
  the lock anyway (it combines config and sessions), so a separate immutable config would bring
  nothing. One lock, one structure.
- **Invalid demand is detected at the edge**: the model takes `Watts` (unsigned, cannot be
  negative); `kw_to_w` rejects negative or non-finite kW values. The 422 error will be produced
  by the API layer.
- **Extra config checks**: a station with no charger is rejected, and unknown JSON fields are
  rejected (`deny_unknown_fields`) to catch typos such as `max_kW`.
- **Deterministic time**: the clock (`now_ms`) is passed in, never read inside the model.
- **Status transitions in one place**: `Station::apply` snapshots statuses, applies the
  mutation, reallocates, and stamps `status_since_ms` only where the derived status changed.
- **Sessions are found by linear scan**: a station has a handful of connectors; a map
  `id → slot` would add a structure to keep in sync for no measurable gain.

---

## Step 2 — V1, increment 2: REST API, scenarios, allocator performance (2026-10-21)

**Scope**: `src/api.rs` (routes, kW ↔ W conversion, error mapping, `/status`), `src/main.rs`
(serves on `0.0.0.0:8080`), `tests/scenarios.rs` (runner), `scenarios/s01…s15.json`,
`tests/perf.rs` (P3, brought forward from increment 6 since the allocator is pure).

**Result**: 17 unit tests + 15 scenarios pass, `clippy` clean. The runner was checked against a
deliberately wrong expectation (S03 expecting 151 kW): it reports
`FAIL S03 …: step 2: setpoint of A: expected 151, got 150`. A `curl` smoke test against the real
server confirmed the routes, the 409 error body, the `suspended_ev` status and the stdout log.

```
cargo test                                               # unit tests + scenarios
cargo test --test scenarios -- --nocapture               # one line per scenario
cargo test --release --test perf -- --ignored --nocapture
```

**P3 — allocator scaling** (release build, 2017 MacBook Pro, 1,000 runs each):

| Sessions | p50 | p99 | max |
|---|---|---|---|
| 4 | 0.36 µs | 0.38 µs | 1.4 µs |
| 100 | 2.7 µs | 5.1 µs | 36.4 µs |
| 10,000 | 236 µs | 365 µs | 432 µs |

Linear growth. Even a 10,000-session station uses under 0.05% of the 1 s budget for the
computation: latency will be dominated by the network, as expected.

**Implementation choices and deviations from the specs**

- **Uniform JSON errors**: malformed bodies, missing fields and wrong types go through a custom
  `JsonConfig` error handler (422); an invalid UUID in the URL is a 404 "unknown session". Both
  are covered by a unit test in `api.rs`, since the scenario runner only sends well-formed requests.
- **Log line simplified**: `session_started session=<id> setpoint=300.0kW station_allocated=300.0kW`
  (no charger/connector, which is gone after a disconnect). Built under the lock, printed after.
  *Revised in increment 4*: log values are rounded to 0.1 kW for readability (`166.7kW` instead
  of `166.666kW`); the API keeps watt precision, and auditing relies on the API, not the logs.
- **Runner checks `GET /status` after every step** (rather than the event response), so error
  steps and observation-only steps are checked the same way.
- **Scenario format details**: `config` is either inline or a path (`"config/station.json"`) to
  avoid repeating the brief's station; a step without event only checks expectations; when
  `http` is not expected, any 2xx is required (a typo in a scenario cannot pass silently); an
  unknown alias maps to a random UUID to exercise "unknown session"; all scenarios run even if
  one fails, and the report lists every failure.
- **`status_since_ms` not checked by scenarios** (real clock), only status changes; exact values
  are covered by the model's unit tests with a controlled clock.

---

## Step 3 — V1, increment 3: Docker and minimal README (2026-10-21)

**Scope**: `Dockerfile` (multi-stage), `docker-compose.yml`, `.dockerignore`, `README.md`.
**First complete deliverable**: a reviewer with only Docker can run the server and the tests.

```
docker compose up --build        # server on :8080
docker compose run --rm test     # unit tests + scenarios
```

**Result** (Docker Desktop on the 2017 MacBook Pro, 4 CPUs / 8 GB VM):

| | |
|---|---|
| First build of both images | 5 min 24 s |
| Rebuild after a source change | 12 s (dependency layers cached) |
| Runtime image | 26.7 MB, runs as non-root (uid 65532) |
| Test image | 1.76 GB (full Rust toolchain, only built for `test`) |
| Tests in Docker | 17 unit tests + 15 scenarios pass |
| Smoke test | `curl` against the container: 201, 300 kW then 200/200 kW, log lines visible with `docker compose logs` |

**Choices**

- **Stages**: `base` (dummy crate) → `builder` (release) and `tester` (debug, so the
  `debug_assert!` invariant is active), → `runtime` (default target). BuildKit only builds the
  stages a target needs: `docker compose up` never builds the test image.
- **Dependency caching**: dependencies are compiled against a dummy `main.rs` / `lib.rs` before
  the sources are copied; `touch` makes cargo rebuild the real crate afterwards.
- **Runtime image `gcr.io/distroless/cc-debian12:nonroot`** (chosen over `debian:bookworm-slim`):
  no shell, non-root user, ~27 MB. Debugging goes through logs and the API.
- **Builder pinned to `rust:1.96-slim-bookworm`**: same compiler as the dev machine, and Debian 12
  like the runtime (glibc compatibility).
- **`ENTRYPOINT ["/sems"]` + `CMD ["/config/station.json"]`**: another config path can be passed
  without redefining the entry point; another station is a mounted file.
- **Test service behind a compose profile**: `docker compose up` starts the server only.
- **README is minimal on purpose**: quick start, tests, scenarios, API, key decisions,
  limitations, performance, roadmap. The full architecture/trade-offs section comes at the end
  of V1; `JOURNAL.md` holds the detailed reasoning meanwhile.

---

## Step 4 — V1, increment 4: setpoint push over WebSocket (2026-10-21)

**Scope**: `src/api.rs` (route `/chargers/{id}/setpoints`, one `watch` channel per charger,
`api::serve`), `src/main.rs` (uses `api::serve`), `tests/push.rs`, README (API section).

**Result**: 17 unit tests + 15 scenarios + 3 push tests pass, `clippy` clean. The push tests were
checked against a deliberate bug (notify every charger on every event): 2 of 3 fail as expected.
Three consecutive runs pass (no flakiness).

| Push test | Checks |
|---|---|
| `other_charger_is_told_without_asking` | Initial snapshot; A on CP1 gets 300 kW; B arrives on CP2 → **CP1 receives A = 200 kW without having sent anything**, CP2 receives B = 200 kW; a plain GET returns what was last pushed |
| `no_message_when_setpoints_do_not_change` | A (100 kW, below its share) is not disturbed by B's arrival: no message within 200 ms |
| `unknown_charger_is_404` | JSON 404 on GET, WebSocket handshake refused |

**Decisions**

- **Route `/chargers/{id}/setpoints`** (resource-oriented) chosen over `/ws/chargers/{id}`
  (transport in the URL) and `/ocpp/{id}` (misleading: we do not speak OCPP). The same URL
  serves a JSON snapshot on a plain GET and a stream on a WebSocket upgrade, which also gives
  chargers the missing **pull fallback** for their own setpoints (`/status` is meant for OPS).
- **`tokio::sync::watch`, one per charger**: it keeps only the latest value, so a slow client
  never receives a backlog of stale setpoints (a `broadcast` channel would queue them).
- **Publishing under the lock** with `send_if_modified`: in-memory only, and it guarantees that
  concurrent events are published in the order they were applied. Only chargers whose
  setpoints changed are woken up.
- **Pull reads the channel, not the station lock**: a charger polling its setpoints never
  contends with event processing.
- **WebSocket closing releases nothing** (safety first, see assumptions); it is logged
  (`ws_connected` / `ws_disconnected`), groundwork for the V3 `offline` status.
- **Test clients: `awc`** (actix's own HTTP + WebSocket client, no default features) and
  `futures-util` (for `StreamExt::next`, already pulled in by actix-web). Testing push needs a
  real server and real clients; `actix-ws` is server-side only. A first version used `reqwest` +
  `tokio-tungstenite`: replaced by `awc`, one client for both protocols, same ecosystem as the
  server (which the developer knows), and lighter since it reuses `actix-http`: the dev build
  went from 199 to **165 crates** (154 for the binary alone). `api::serve(station, listener)`
  was extracted from `main.rs` so tests can start a server on port 0.

---

## Step 5 — V1, increment 5: `/events` history (2026-10-21)

**Scope**: `src/model.rs` (history recorded by `Station::apply`), `src/api.rs` (`GET /events`),
`tests/scenarios.rs` (history invariant), README (history section).

**Result**: 20 unit tests + 15 scenarios + 3 push tests pass, `clippy` clean. The history
invariant was checked against a deliberate bug (`seq` incremented by 2): every scenario fails at
step 1 with `history: expected seq 1, got 2`.

**Decisions**

- **Recorded in the model, inside `Station::apply`**: the single path of every accepted event.
  No accepted event can be missed, the entry is written after reallocation (so its `result` is
  the post-event state), and rejected events never reach `apply`, so they are never recorded.
  Stored in watts with indices; the API converts to kW and ids.
- **`VecDeque`** (std ring buffer) with a capacity of 10,000: appending and dropping the oldest
  entry are both O(1). Rejected: `Vec::remove(0)` (shifts every entry on each event, under the
  lock), an unbounded `Vec` (memory grows forever), a hand-made ring index (re-implements
  `VecDeque`).
- **`seq` never restarts**, even when old entries are dropped: a reader detects gaps.
- **Rejected events are not recorded** (for now): they stay visible in the HTTP response. A
  documented option if OPS needs to trace refused attempts.
- **History invariant in the scenario runner**, checked after every step like the safety one:
  an accepted event adds exactly one entry (expected seq, kind and session) whose
  `result.allocated_kw` equals `/status`; a rejected event adds none. The 15 existing scenarios
  thus test the history without any new scenario file.
- **Query validation**: an invalid `session` (not a UUID) or `limit` returns a JSON 422, like
  other errors (`QueryConfig` error handler). An unknown session returns an empty list.
- **`allocated_kw` is not stored**: it is derived from the recorded setpoints when rendering.

---

## Step 6 — V1, increment 6: allocation metrics and end-to-end performance (2026-10-21)

**Scope**: `tests/scenarios.rs` (metrics), `tests/perf.rs` (P1, P2; P3 kept), `tests/common/mod.rs`
(server/client helpers shared by `push.rs` and `perf.rs`), README (performance and metrics).
**V1 is complete.**

**Result**: 20 unit tests + 15 scenarios + 3 push tests pass, `clippy` clean. Several metric
values were checked by hand (waste of S07, S09, S15; fairness of S09; overshoot of S04).

### V1 allocation metrics (baseline for V2)

Worst case over each scenario's steps (churn: average per accepted event).

| Scenario | Waste max (kW) | Efficiency min (%) | Satisfaction min (%) | Fairness min | Churn avg (setpoints/event) | Overshoot max (kW) |
|---|---|---|---|---|---|---|
| S01 empty station | 0.0 | 100.0 | 100.0 | 1.00 | 0.0 | 0.0 |
| S02 single vehicle | 0.0 | 100.0 | 85.7 | 1.00 | 1.0 | 0.0 |
| S03 two vehicles on the same charger | 0.0 | 100.0 | 50.0 | 1.00 | 1.5 | 50.0 |
| S04 two vehicles on different chargers | 0.0 | 100.0 | 66.7 | 1.00 | 1.5 | 100.0 |
| S05 a vehicle leaves | 0.0 | 100.0 | 66.7 | 1.00 | 1.7 | 100.0 |
| S06 full station | 0.0 | 100.0 | 33.3 | 1.00 | 2.5 | 100.0 |
| S07 demand decreases then increases | 100.0 | 71.4 | 66.7 | 0.96 | 1.2 | 100.0 |
| S08 demand above the connector limit | 0.0 | 100.0 | 50.0 | 1.00 | 1.0 | 0.0 |
| S09 heterogeneous chargers | 166.7 | 55.2 | 55.2 | 0.91 | 1.7 | 0.0 |
| S10 V1 known waste | 33.3 | 93.3 | 50.0 | 1.00 | 1.3 | 0.0 |
| S11 invalid events | 0.0 | 100.0 | 100.0 | 1.00 | 1.0 | 0.0 |
| S12 demand increase under contention | 100.0 | 71.4 | 66.7 | 0.96 | 1.0 | 0.0 |
| S13 cascading release | 113.3 | 68.8 | 33.3 | 0.85 | 2.4 | 100.0 |
| S14 zero demand takes no share | 0.0 | 100.0 | 66.7 | 1.00 | 1.7 | 100.0 |
| S15 plugged in but not charging | 30.0 | 90.0 | 50.0 | 0.90 | 1.8 | 50.0 |

**Reading**

- **Safe, but wasteful**: waste reaches 166.7 kW (S09: the share left unused by a 22 kW AC
  charger is not handed to the 300 kW charger), 113.3 kW in S13, 100 kW in S07/S12. V2 targets
  0 kW everywhere; the cost to watch is churn (redistributing disturbs more chargers).
- **Transition risk quantified**: whenever a share moves from one charger to another, a charger
  ramping up before the other has reduced would overload the grid by up to 100 kW. This is the
  case for "decrease before increase" (V3), which should bring overshoot to 0.
- Satisfaction below 100 % is expected when the station is saturated (S06: 4 × 300 kW requested
  on a 400 kW grid); it is not a defect.

### Performance (release, native, 2017 MacBook Pro, ranges over 3 runs)

| Test | p50 | p99 | max |
|---|---|---|---|
| P1 event → push to another charger (200 runs) | 155–171 µs | 282–464 µs | 444–582 µs |
| P2 request latency, 8 clients × 500 events, 20 sessions | 1.0–1.2 ms | 3.0–3.4 ms | 3.6–6.2 ms |
| P3 allocation, 4 / 100 / 10,000 sessions (p50) | 0.4–0.6 µs | 2.8 µs | 248–255 µs |

P2 throughput: **5,300–5,900 events/s**, safety invariant checked on all 4,000 responses, all
events found in `/events`. P1's worst case (< 0.6 ms) is ~1,700 times below the 1 s requirement.

### Decisions

- **Metrics computed by the test runner from `/status`**, not by the server: measurement stays
  out of the product. Worst case per scenario (like p99/max for latency: for critical
  infrastructure the worst moment matters), except churn (a recurring cost: average).
- **Optimum** = `min(grid, Σ chargers min(charger, Σ sessions min(demand, connector)))`: the
  most power deliverable under the nested limits; V2's water-filling should reach it.
- **Overshoot is measured at grid level only.** Chargers have the same transition risk (S03:
  A going 300 → 150 while B goes 0 → 150 could briefly put 450 kW on a 300 kW charger); not
  measured yet, noted for V3.
- **P1 toggles B between 0 and 300 kW**: with V1's equal share, only a change in the number of
  active sessions moves A's setpoint (300 ↔ 200 kW).
- **Perf tests run with `--test-threads=1`** so that P1, P2 and P3 do not compete for CPU.
- **Shared helpers in `tests/common/mod.rs`** (start a server, subscribe, connect…), used by
  `push.rs` and `perf.rs`, instead of duplicating them.
- **Honesty about the figures**: client and server share the machine (upper bounds), every event
  response carries the full station state (part of P2's cost), max values vary with OS
  scheduling (hence ranges).

---

## Step 7 — Critical review of the tests, and a bug fix (2026-10-21)

**Bug found while reviewing the tests** (none of them caught it): two sessions requesting
`1e20` kW killed the server.

1. `kw_to_w(1e20)` saturated silently to `u64::MAX` (Rust's float → integer cast).
2. Summing two such demands for `unserved_kw` overflowed: a panic in debug, **under the lock**.
3. The lock was poisoned and, by design ("fail loudly"), every later request panicked too.
   In release (the Docker image) the sum wrapped silently instead: no crash, but wrong values.

**Fix**: `kw_to_w` rejects any power above `MAX_KW` = 1,000,000 kW (1 GW) instead of saturating;
the API answers 422. One bound for every power from the outside world (demands and config):
no site or vehicle comes close, and it keeps every sum far from `u64` overflow (18 million
sessions at 1 GW would be needed). Checked against the real server: both requests now get a
422, the 1 GW limit is accepted, `/status` keeps answering, no panic in the log.
Rejected: a 10 MW cap on demands plus saturating additions (two mechanisms where one bound at
the edge suffices). Regression: unit test on `kw_to_w` and scenario S16.

**Review of the test suite — remaining weaknesses** (documented, not all addressed):

| # | Weakness | Decision |
|---|---|---|
| 1 | Only hand-written scenarios: we test what we imagined (this is how the bug slipped through) | Randomized property test (seeded generator, shadow model predicting HTTP codes, invariants at every step): **postponed to V2**, where it also checks "waste = 0" |
| 2 | Expected values computed by the author of the algorithm (shared misunderstanding possible) | Mitigated by algorithm-independent invariants; V2's optimum acts as an oracle |
| 3 | A panic under the lock takes the whole service down | To be discussed |
| 4 | Time-based assertions ("no message within 200 ms", "< 1 s") can pass or fail by timing | Accepted for now (3/3 green runs) |
| 5 | Order of pushed messages under concurrency not tested | Later, with the WebSocket work |
| 6 | WebSocket edge cases (close, many clients, reconnection) not tested | Later, with the WebSocket work |
| 7 | Overshoot measured at grid level only; runtime Docker image only smoke-tested by hand | Noted |
| 8 | Mutation testing done by hand, case by case | Accepted |

Strengths kept: two invariants checked at every step independently of expected values, a
mandatory `why` per scenario, real server and clients for push and performance, tests verified
against deliberate bugs.

---

# V2 — No waste: max-min fair water-filling

V1 is frozen on branch `v1`; V2 replaces its allocator (no double implementation in the code).

## Step 8 — V2 design (2026-10-21)

**Problem**: V1 is safe but wastes power (up to 166.7 kW in our scenarios): a session's unused
share is not handed to the others. Goal: 0 kW of waste, cascade still enforced.

**Fairness objective** — chosen: **max-min fairness** (raise the least served first, until
nobody can get more without taking from someone served less). Rejected:

| Option | Why not |
|---|---|
| Proportional to demand | Favours big requests: a 22 kW AC car rationed like a 300 kW DC one |
| First come, first served | Unfair; the last car can get nothing |
| Proportional fairness (maximise Σ log setpoint) | Needs a convex solver, hard to explain to OPS; close to max-min on our tree |
| Maximise the total only | Same total as max-min (both reach the optimum), without any fairness guarantee |
| Linear programming | Oversized, heavy dependency |
| Stack-aware discrete allocation | Stacks are out of scope (documented assumption) |

Weighted max-min (a weight per session) is the natural extension if priorities appear.

**Computation method** — chosen: **exact water-filling over sorted caps**, O(n log n).
Rejected: simulated progressive filling (slow, approximate), bisection on the level
(approximate), successive linear programs (only needed for non-nested constraints),
incremental recomputation (premature: a full computation takes microseconds).

**Algorithm** (two fillings for the cascade):

```
fill(capacity, caps) = highest level μ with Σ min(cap, μ) ≤ capacity ("unlimited" if all fit)
cap        = min(demand, connector)
μ_charger  = fill(charger max, caps of its sessions)
cap'       = min(cap, μ_charger)
λ_grid     = fill(grid, all cap')
setpoint   = min(cap', λ_grid)
```

**Decisions**: floor rounding kept (at most n − 1 W unused, accepted); V1 kept in git history
only; scenario expectations updated so that the diff between branches shows the gain; the
randomized test (postponed from step 7) is part of V2.

## Step 9 — V2.1: the water-filling allocator (2026-10-21)

**Scope**: `src/allocator.rs` (`fill`, new `allocate`), updated expectations in 2 model tests and
6 scenarios (S10 renamed `s10_released_share.json`), 5 new allocator unit tests.

- **Simpler than V1 on one point**: a zero demand is a zero cap and naturally takes nothing; the
  V1 special case disappears.
- **The V1 expectations failed exactly where V1 wasted power** (S07, S09, S10, S12, S13, S15):
  e.g. S10 466.666 → 500 kW, S13 286.666 → 400 kW.
- **Metrics**: waste 0 kW in every scenario, efficiency 100 %. Costs measured and assumed: churn
  up (S07 1.2 → 1.8, S13 2.4 → 2.7 setpoints/event), potential transition overshoot up (S12
  0 → 100 kW, S13 100 → 113.3 kW): V3's "decrease before increase" becomes more necessary.
- **Jain's index needs care**: S10 drops to 0.98 although the split is max-min optimal (it
  compares service ratios, not what each car *could* get). Documented in the README.
- **Performance**: allocation alone ~5× slower (10,000 sessions: 250 µs → 1.3 ms; sort and small
  allocations), end-to-end unchanged (P1 max 408–513 µs, P2 5,600–5,900 events/s). An
  optimisation (avoid per-charger allocations) exists if ever needed; nothing justifies it now.

## Step 10 — V2.2: randomized property test (2026-10-21)

**Scope**: `tests/random.rs`, `tests/common/checks.rs` (checks shared with the scenario runner),
scenario S17.

- **Seeded SplitMix64 generator** (no dependency): random stations (1–5 chargers of 7–350 kW,
  1–3 connectors, some with a lower limit, grid 10–1500 kW), 100 random events each: connects
  (some on busy or unknown connectors), demand changes and disconnects (some on ended or unknown
  sessions), demands mostly 0–400 kW with decimals, sometimes 0, the 1 GW limit or invalid.
- **Shadow model** (which session is on which connector) predicts every HTTP code: 201, 200,
  404, 409 or 422, never a 5xx.
- **Checked after every event**: safety, history, `/status` consistency (new: totals and
  statuses match the sessions; also added to the scenario runner), agreement with the shadow
  model, waste < 1 W per session.
- **Verified against deliberate bugs**, each caught with its seed and step: water level 1 W too
  high (seed 2, step 3), 1 kW below the level (seed 1, step 2), charger level ignored (seed 4,
  step 18: `CP4 overloaded (466.885 kW)`).
- **Duration**: 200 stations took ~17 s in debug; default set to **50 stations (~4 s)**,
  `SEMS_RUNS` plays more, `SEMS_SEED` replays one.
- **S17 — heterogeneous chargers under contention**: S09's mix on a 100 kW grid. Since V2, S09
  is a happy path (372 kW requested on 400 kW); S17 keeps a case where heterogeneous chargers
  must share: 22 kW AC served in full, 39 kW each for the two DC chargers (V1 would have given
  33.3 / 22 / 33.3 kW, 11.3 kW wasted). Its fairness index (0.74) illustrates the Jain caveat.
- Cosmetic fix: an empty `f64` sum is `-0.0` in Rust, which printed "-0.0" kW of waste for S01;
  `optimum_kw` now folds from `0.0`.

---

# V3 — Safe transitions: decrease before increase

## Step 11 — V3 design (2026-10-21)

**Problem**: V2 moves more power between chargers, and the metrics showed a potential transition
overshoot of up to 113.3 kW: if a charger ramps up before another one has reduced, the grid is
exceeded. Even with instant chargers, two chargers never receive their messages at the same
moment.

**Product input (PM)**: the grid limit is a hard boundary, not to be exceeded even briefly
(breakers tolerate short overloads, but the design must not rely on it); a charger acknowledges
the *reception* of a setpoint, not a measured power; a charger that does not confirm must block
increases rather than risk an overload; priorities: safe transitions, then offline chargers
visible to OPS, then minimum power. Ramp times need not be modelled: assume a setpoint received
is applied at once.

**Options considered**

| Option | Verdict |
|---|---|
| Fixed delay before increases | Rejected: no guarantee (the grid is a hard limit) |
| Permanent safety margin | Rejected: waste, the opposite of V2 |
| Acknowledgement with a time-based fallback | Rejected: the fallback reintroduces the risk |
| **Acknowledgement barrier** | **Chosen** |

**Simplification pass** (the first proposal was reviewed for minimality):

- Acknowledgements over **HTTP only**, not on the WebSocket: an acknowledgement is initiated by
  the charger, like every other charger event. Rule kept everywhere: *who speaks first decides
  the channel* (charger → HTTP, server → WebSocket). In production this would converge on a
  single bidirectional WebSocket, as in OCPP.
- **No per-charger counter**: the version of a charger's setpoints is the `seq` of the event that
  last changed them. While a charger is awaited its setpoints can only go down, so the version
  awaited is always its latest: per charger, only `version` and an `awaiting` flag remain.
- **Station-wide, all or nothing**: a pending decrease holds every increase; an increase that
  would already fit is held too. A finer barrier is documented, not built.
- Acknowledgements are recorded in `/events` (kind `ack`): the answer to "why did B wait".
- A charger whose sessions all ended stays awaited until it acknowledges (conservative).
- Out of scope: ramp times, per-charger timeouts (documented as the next step), P4 (increase
  delay measurement).

**Why a version in the acknowledgement**: two decreases can be sent to the same charger faster
than an acknowledgement travels back; acknowledging the first proves nothing about the second.
Only the charger's latest version releases it; a version never sent is rejected.

## Step 12 — V3.1a: model, API and runner (2026-10-21)

**Scope**: `src/model.rs` (`target_w`, `ChargerLink`, `ack`, `grant`), `src/api.rs`
(`POST /chargers/{id}/ack`, `version` in pushed messages, `target_kw` and `pending_acks` in
`/status`, acknowledgements in `/events`), `tests/scenarios.rs` (auto/manual acknowledgements,
`targets` and `pending` expectations), `tests/common/checks.rs` (transition invariant), scenarios
S18–S20, push and perf tests updated.

- **The rule fits in `Station::grant`**: decreases granted at once and their charger marked
  awaited; increases granted only when no charger is awaited; versions bumped where published
  setpoints changed.
- **Transition invariant**, checked after every request: even if chargers still applied their
  previous setpoints, no charger and not the grid would be exceeded. A first version counted the
  setpoint of an *unplugged* vehicle and failed S05 and S13; the invariant was wrong, not the
  server (a vehicle that left draws nothing), and now compares the same session only.
- **The 17 existing scenarios kept every expected value** (auto mode describes the settled
  station) and their overshoot dropped to 0. Disabling the barrier makes them fail with
  `the grid could see 500 kW` and `charger "CP1" could draw 450 kW` — the charger-level
  transition risk noted in V1 is now checked.
- **P1 now measures a decrease** (published at once); an increase waits for the acknowledgement
  round trip, which depends on the chargers.

## Step 13 — V3.1b: random acknowledgements (2026-10-21)

**Scope**: `tests/random.rs`.

- Acknowledgements become random steps: latest, stale, future (422), unknown charger (404), or
  never. Each run ends by acknowledging everything, which must settle the station with no waste.
- Detected deliberate bugs: barrier off, decrease not awaited (transition check). The stale
  acknowledgement rule (a single version comparison in the server) is tested by scenario S19.

## Step 14 — V3.1c: documentation (2026-10-21)

README: the communication asymmetry ("who speaks first", HTTP in, WebSocket out, and the OCPP
single-socket target), safe transitions (rule, versions, assumption, silent-charger limitation
and timeout idea with its caveat), API (ack route, `version`, `target_kw`, `pending_acks`),
quick start with an acknowledgement, scenarios S18–S20, metrics V2 → V3 (overshoot 0 everywhere,
and the price of safety while acknowledgements are pending), performance (V3 ranges over 3
runs: P1 max 404–642 µs, P2 5,000–5,500 events/s).

---

## Step 15 — V3.2: OPS view completed (2026-10-21)

The brief asks to let OPS see "connector and charger availability, power availability". Connector
availability (statuses) and station power were already in `/status`; charger availability and
per-charger power were missing.

**Scope**: `src/model.rs` (`ChargerLink.connections`, `online_since_ms`, `Station::set_link`),
`src/api.rs` (WebSocket open/close update the link; `online`, `online_since_ms`, `available_kw`
per charger in `/status`), tests, README ("OPS view" section mapping the brief's words to the
fields).

- **Online = at least one setpoint WebSocket open**: a connection count (a charger may open
  several), `online` derived from it, `online_since_ms` stamped on each change.
- **Going offline releases nothing**: the charger may still be drawing power; its sessions keep
  their setpoints and, if it owed an acknowledgement, it stays in `pending_acks`.
- **Kept minimal**: connections are logged on stdout, not recorded in `/events`; a pull-only
  charger shows as offline (documented).
- **Tests**: model unit test (two channels, one closed: still online; both closed: offline,
  allocation unchanged), push test against a real server (offline, online once the WebSocket
  opens, offline once it closes, 300 kW kept), consistency check `available_kw = max_kw −
  allocated_kw` per charger in every scenario and random step. A deliberate bug (never marking
  a charger offline) makes the push test fail.

## Step 16 — Two more scenarios; minimum power documented (2026-10-21)

- **S21 — a pending decrease holds an unrelated increase**: A drops on CP1 (pending), B asks for
  more on CP2; B waits although its increase would already fit. Shows the cost of the
  station-wide, all-or-nothing barrier, by design.
- **S22 — OPS view during a pending acknowledgement**: checks the whole exposed state during a
  partial transition, not only setpoints: statuses (`suspended_evse` for the held vehicle),
  targets, `pending_acks`, and station totals counting only what is granted.
- **Minimum viable power per session: documented as not covered** (README limitations), with
  the sketched rule. It cannot be a scenario since the feature does not exist; not needed for
  the brief's station.


---

# V4 — Battery storage (BESS)

## Step 17 — BESS design (2026-10-21)

**Brief**: boost the station's capacity with the battery, keep its SoC above 10 %, charge it
with spare grid power.

**Product input (PM)**: the SoC is reported by the BESS's BMS, as a vehicle reports its demand;
the station must **never export** to the public grid, not even briefly; the 10 % floor must be
the system's own rule, above the BMS hardware cut-off, so the boost never vanishes without
warning; one flow at a time (charge, discharge or idle), decided on every recomputation; the BESS
is the station's own asset and can be treated as a planning input that applies its mode reliably.

**Decisions**

- **SoC as an input** (`POST /bess`): no timer, no energy integration; the system stays
  event-driven, and the BESS speaks first, so HTTP.
- **Capacity instead of grid**: `capacity = grid + max_kw` above 10 % SoC, `grid` otherwise or
  while unknown. The allocator is unchanged; it only receives a larger capacity.
- **Mode rule**: discharge what vehicles draw beyond the grid; else charge with the headroom up to
  `max_kw` while not full (vehicles first); else idle.
- **No acknowledgement for the BESS.** Commanding its power from the server would make boost
  transitions unsafe either way (vehicles first: overload; BESS first: export). The BESS
  regulates the grid connection locally (peak shaving); the server plans capacity and mode, and
  vehicle setpoints keep going through the barrier. When the SoC falls below the floor, vehicle
  decreases are published at once and the BESS covers them until applied.
- Kept minimal: the 10 % floor is a constant; no hysteresis (documented); SoC updates recorded in
  `/events` (kind `bess_soc`), which makes a history entry's charger optional.

## Step 18 — V4.1: model, API, scenarios (2026-10-21)

**Scope**: `src/model.rs` (`Bess` config, `soc`, `set_soc`, `boost_w`, `capacity_w`,
`bess_flow`), `src/allocator.rs` (takes the capacity), `src/api.rs` (`POST /bess`,
`capacity_kw`, `grid_import_kw`, `bess` block), checks (safety and transition on the capacity;
grid import within 0..=grid; BESS mode rule), runner (`soc` step, BESS expectations), scenarios
S23–S25.

- A station without BESS behaves exactly as before: the 22 existing scenarios pass unchanged.
- `available_kw = grid − allocated` would have overflowed in unsigned integers once the boost is
  used; it is now `capacity − allocated` (saturating while vehicles apply a lower capacity).
- Deliberate bugs caught: floor ignored (S24), charging that ignores the vehicles (grid import
  500 kW on a 400 kW grid).

## Step 19 — V4.2: randomized BESS, documentation (2026-10-21)

- **Randomized test**: one station in two has a BESS (50–400 kWh, 20–300 kW); random SoC reports
  (floor, full, in between, out of range → 422, no BESS → 404). Passes on 500 stations.
- A first failure came from the check, not the server: a 1 W remainder of the floor rounding is
  real headroom, so the BESS charges 1 W, while the check decided the mode with its 1 W
  comparison tolerance. The mode is now decided on exact values; the tolerance only compares
  powers.
- **Floor rule now checked everywhere** (`capacity = grid + boost`, boost only above 10 %): the
  randomized test catches a floor ignored, charging that ignores the vehicles, and charging
  when full.
- README: a "Battery storage (BESS)" section (rules, interface, why no acknowledgement,
  assumptions), API and `/status` fields, OPS view, scenarios S23–S25, metrics, limitations,
  roadmap (V4 ✅).
