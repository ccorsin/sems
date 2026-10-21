# SEMS — Station Energy Management System

The load-management "brain" of an EV charging station: it distributes the available power
across concurrent charging sessions without ever exceeding the grid, charger or connector
limits, and recomputes the distribution on every event (vehicle plugs in, changes its demand,
leaves).

Written in Rust (actix-web). The design process, every decision and the alternatives considered
are traced step by step in [`JOURNAL.md`](JOURNAL.md).

> Current version: **V1** — capped equal-share allocation, REST API. See the [roadmap](#roadmap).

## Quick start

Requires Docker only.

```bash
docker compose up --build        # server on http://localhost:8080, brief's station
```

The default station (`config/station.json`) is the brief's: a 400 kW grid and 2 chargers of
300 kW with 2 CCS connectors each. Another station is a JSON file mounted in place of it
(see `docker-compose.yml`).

Try it with `curl`:

```bash
# A vehicle plugs into CP1 connector 1 and accepts 300 kW: it gets all of it.
curl -X POST localhost:8080/sessions -H 'content-type: application/json' \
     -d '{"charger": "CP1", "connector": 1, "demand_kw": 300}'
# → 201 {"session_id": "<id-A>", "setpoint_kw": 300.0, "status": {…}}

# A second vehicle on the other charger: both are capped by the 400 kW grid (200 kW each).
curl -X POST localhost:8080/sessions -H 'content-type: application/json' \
     -d '{"charger": "CP2", "connector": 1, "demand_kw": 300}'

# The first vehicle's battery is nearly full: its demand drops.
curl -X PUT localhost:8080/sessions/<id-A> -H 'content-type: application/json' -d '{"demand_kw": 50}'

# It leaves.
curl -X DELETE localhost:8080/sessions/<id-A>

# OPS view: who is charging, at what power, what is left.
curl localhost:8080/status
```

The server logs one line per accepted event on stdout (`docker compose logs -f sems`).

## Run the tests

With Docker only:

```bash
# All tests: unit, scenarios, push
docker compose run --rm test
# Scenarios one by one, with their metrics and the summary table
docker compose run --rm test cargo test --test scenarios -- --nocapture
# Performance (release build inside the container: first run takes a few minutes)
docker compose run --rm test cargo test --release --test perf -- --ignored --nocapture --test-threads=1
```

Natively (stable Rust): the same commands without `docker compose run --rm test`, e.g.
`cargo test` or `cargo test --test scenarios -- --nocapture`.

Performance figures measured inside Docker on macOS include the Docker VM's overhead (on the
dev machine: P1 max ≈ 2 ms, P2 ≈ 2,500 events/s); the figures below were measured natively.

- **Unit tests** (`src/*.rs`): configuration validation, session lifecycle and statuses,
  allocation algorithm, malformed requests.
- **Scenarios** (`scenarios/*.json`): event sequences replayed through the API, with expected
  setpoints, totals, statuses and HTTP codes. After **every step of every scenario** the runner
  checks two invariants: **safety** (no setpoint above its demand or connector, no charger or
  grid overload) and **history** (an accepted event adds exactly one `/events` entry whose
  resulting allocation matches `/status`; a rejected event adds none).
- **Push** (`tests/push.rs`): a real server on a random port, real WebSocket and HTTP clients.
  A charger is told its new setpoint when *another* charger's event changes it, and is not
  disturbed when it does not.
- **Performance** (`tests/perf.rs`): ignored by default, run in release mode.

### Scenarios

| # | Scenario | Why |
|---|---|---|
| S01 | Empty station | Nothing allocated, whole grid available |
| S02 | Single vehicle | Takes all it wants, within its charger limit |
| S03 | Two vehicles, same charger | Local contention: they share the charger's 300 kW |
| S04 | Two vehicles, different chargers | Global contention: together capped by the 400 kW grid |
| S05 | A vehicle leaves | The remaining one takes the power back |
| S06 | Full station | No limit exceeded at any level; rounding down stays below the grid |
| S07 | Demand down then up | Both directions go through the same recomputation |
| S08 | Demand above connector limit | The connector level of the cascade |
| S09 | Heterogeneous chargers | 300 kW DC, 50 kW DC and 22 kW AC on one site |
| S10 | V1 known waste | Documents the V1 limitation (fixed in V2) |
| S11 | Invalid events | Clear error codes, state untouched by rejected events |
| S12 | Increase under contention | An increase is granted only within what is available |
| S13 | Cascading release | Released power across several vehicles and chargers |
| S14 | Zero demand | A full vehicle still plugged in takes no share |
| S15 | Plugged in, not charging | Status `charging` → `suspended_ev` → `charging` for OPS |
| S16 | Absurd demand values | Regression: powers above 1 GW are rejected (422) instead of overflowing |

Each file carries its own `why`. To add a scenario, drop a JSON file in `scenarios/`: it is
picked up automatically. Format:

```json
{
  "name": "S03 two vehicles on the same charger",
  "why": "Local contention: both share their charger's 300 kW.",
  "config": "config/station.json",
  "steps": [
    { "connect": { "as": "A", "charger": "CP1", "connector": 1, "demand_kw": 300 } },
    { "connect": { "as": "B", "charger": "CP1", "connector": 2, "demand_kw": 300 },
      "expect": { "setpoints": { "A": 150, "B": 150 }, "allocated_kw": 300 } },
    { "change": { "session": "A", "demand_kw": 0 }, "expect": { "statuses": { "A": "suspended_ev" } } },
    { "disconnect": "B", "expect": { "http": 200 } }
  ]
}
```

`config` is a path or an inline config. Every `expect` field is optional
(`setpoints`, `statuses`, `allocated_kw`, `available_kw`, `unserved_kw`, `http`); without
`http`, a 2xx is required.

## API

All powers are in kW. Errors are `{"error": "<message>"}`.

| Method | Route | Body | Success | Errors |
|---|---|---|---|---|
| `POST` | `/sessions` | `{"charger", "connector", "demand_kw"}` | `201` | 404 unknown charger/connector, 409 connector busy, 422 invalid body or demand (powers are 0 to 1,000,000 kW) |
| `PUT` | `/sessions/{id}` | `{"demand_kw"}` | `200` | 404 unknown session, 422 |
| `DELETE` | `/sessions/{id}` | — | `200` | 404 unknown session |
| `GET` | `/status` | — | `200` | — |
| `GET` | `/events?limit=100&session=<id>` | — | `200` | 422 invalid `limit` or `session` |
| `GET` | `/chargers/{id}/setpoints` | — | `200`, or WebSocket upgrade | 404 unknown charger |

Event responses: `{"session_id", "setpoint_kw", "status"}`, where `status` is the full
station state (a demo convenience: a real charger should only see its own setpoints).

### Setpoints for chargers (push and pull)

`/chargers/{id}/setpoints` is the charger's channel. Opened as a **WebSocket**, it sends the
current setpoints, then a message **whenever one of them changes** — including when the change
is caused by another charger's event (a vehicle arriving elsewhere lowers this charger's share).
A plain **GET** on the same URL returns the same message: a pull fallback while a WebSocket
reconnects.

```json
{ "charger": "CP1", "setpoints": [ { "connector": 1, "session_id": "9f1c…", "kw": 200 },
                                   { "connector": 2, "session_id": null, "kw": 0 } ] }
```

Try it from a browser console (nothing to install), then post events with `curl`:

```js
const ws = new WebSocket("ws://localhost:8080/chargers/CP1/setpoints");
ws.onmessage = (m) => console.log(m.data);
```

Events still go through the REST routes; messages sent by the charger on the WebSocket are
ignored (pings are answered). Closing the WebSocket releases nothing (see limitations).

`GET /status` exposes, per station, charger and connector:

| Field | Meaning |
|---|---|
| `demand_kw` | What the vehicle accepts right now (input) |
| `setpoint_kw` | What the station allows (output), always ≤ `demand_kw` |
| `allocated_kw` | Sum of setpoints |
| `available_kw` | Grid headroom (not necessarily usable: a charger may be saturated) |
| `unserved_kw` | Unmet demand: Σ (demand − setpoint) |
| `status` | `available`, `charging`, `suspended_ev` (plugged in, vehicle requests nothing), `suspended_evse` (plugged in, station gives nothing) — names from OCPP |
| `status_since_ms` | Time of the last status change |

### History (OPS diagnostics)

`GET /events` returns the latest accepted events, oldest first: `limit` (default 100) and an
optional `session` filter. Each entry holds the event **and the allocation that resulted from
it**, so that a setpoint can be explained afterwards:

```json
{ "seq": 2, "at_ms": 1761040000000, "kind": "session_started",
  "session_id": "4b2e…", "charger": "CP1", "connector": 2, "demand_kw": 300.0,
  "result": { "allocated_kw": 300.0,
              "setpoints": [ { "session_id": "9f1c…", "kw": 150.0 }, { "session_id": "4b2e…", "kw": 150.0 } ] } }
```

*"Why did my car charge at 150 kW?"* — `GET /events?session=<its id>` shows each of its events
with the resulting split: here a second vehicle plugged into the same charger. `kind` is
`session_started`, `demand_changed` or `session_ended`. The last 10,000 events are kept; `seq`
never restarts, so a gap reveals dropped entries. Rejected events are not recorded.
`allocated_kw ≤ grid_kw` on every entry is the safety audit.

## Key decisions

Full reasoning and rejected alternatives: [`JOURNAL.md`](JOURNAL.md).

- **Cascading limits** grid > charger > connector > vehicle, all enforced at once.
- **V1 allocation — capped equal share**:
  `setpoint = min(demand, connector, grid / n, charger / n_charger)`, where only sessions with a
  non-zero demand count. Safe by construction; known limitation: power unused by one session is
  not redistributed (see S10, fixed in V2).
- **Integer watts internally, rounded down**: sums can never exceed a limit by rounding.
- **Full recomputation on every event**, under a single lock: the "Σ ≤ grid" invariant is
  global, and the computation takes microseconds.
- **In-memory state**, no database: the hot path stays fast; history persistence is a later step.
- **History recorded in the same operation as the event** (the model's single event path): an
  entry can never be missing or out of step with the state; bounded ring buffer (`VecDeque`).

## Assumptions and limitations

- Power is modelled in kW (real chargers are often driven in amperes) and is continuously
  divisible (internal charger "stacks" are not modelled).
- One server per station. No authentication. State is lost on restart.
- Chargers are assumed to apply setpoints instantly; the transition risk (a vehicle ramping up
  before another one has reduced) is addressed in V3 ("decrease before increase").
- A charger that disappears without ending its session keeps its power reserved (safety first);
  OPS can release it with `DELETE /sessions/{id}`.
- Setpoint push is fire-and-forget: there is no acknowledgement that a charger applied its new
  setpoint yet (needed for "decrease before increase", V3).

## Performance

`cargo test --release --test perf -- --ignored --nocapture --test-threads=1`. Release build,
natively on a 2017 MacBook Pro (i7-7700HQ); ranges over 3 runs. Client and server share the
machine, so these are upper bounds; max values vary with OS scheduling.

| Test | What | p50 | p99 | max |
|---|---|---|---|---|
| **P1** | Event on one charger → new setpoint received over WebSocket by **another** charger (200 runs) | 155–171 µs | 282–464 µs | 444–582 µs |
| **P2** | 8 concurrent clients × 500 demand changes on 20 sessions: per-request latency | 1.0–1.2 ms | 3.0–3.4 ms | 3.6–6.2 ms |
| **P3** | Allocation alone, 4 sessions | 0.4–0.6 µs | 1.7–3.5 µs | ≤ 0.4 ms |
| | Allocation alone, 100 sessions | 2.8 µs | 6–19 µs | ≤ 0.7 ms |
| | Allocation alone, 10,000 sessions | 248–255 µs | 0.56–0.63 ms | ≤ 1.4 ms |

- **P1, the brief's requirement** (< 1 s): the worst case measured is under 0.6 ms, about
  1,700 times below the budget.
- **P2**: **5,300–5,900 events/s** with the safety invariant checked on every one of the
  4,000 responses, and every event found in `/events`. A real station sees a few events per
  second: this is headroom, not a target. Each response carries the full station state,
  which is part of this cost.
- **P3**: linear growth; computation is never the bottleneck.

## Allocation metrics (V1 baseline)

Computed by the scenario runner (`cargo test --test scenarios -- --nocapture`), worst case
over each scenario's steps. Optimum = the most power deliverable within every limit:
`min(grid, Σ chargers min(charger, Σ sessions min(demand, connector)))`.

| Metric | Meaning |
|---|---|
| Waste (kW) | `optimum − allocated`: power deliverable within every limit, but not given |
| Efficiency (%) | `allocated / optimum` |
| Satisfaction (%) | `allocated / Σ demand` (below 100 % is normal when the station is saturated) |
| Fairness | Jain's index over `setpoint / demand` ratios; 1 = perfectly even |
| Churn (setpoints/event) | Setpoints changed per accepted event: how many chargers are disturbed |
| Overshoot (kW) | Grid overload if every charger applied the larger of its old and new setpoint during a transition |

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
| S16 absurd demand values | 0.0 | 100.0 | 0.0 | 1.00 | 1.5 | 100.0 |

**Reading**: V1 is safe (the invariant holds at every step) but **wastes up to 166.7 kW**
(S09: a small charger's unused share is not handed to the big one): V2 (water-filling) targets
0 kW of waste everywhere. **Potential overshoot reaches 100 kW** whenever a share moves from
one charger to another: this is the transition risk addressed by "decrease before increase" (V3).

## Roadmap

| Version | Content |
|---|---|
| **V1** ✅ | REST API, scenarios, Docker, WebSocket push of setpoints, `/events` history, allocation metrics, end-to-end performance tests |
| V2 | Water-filling allocation: no waste, released power goes to the other sessions; randomized property test (random stations and events, invariants at every step) |
| V3 | "Decrease before increase" transitions, minimum viable power per session, offline chargers |
| V4 | Battery storage (BESS): boost, 10% SoC floor, opportunistic charging |
