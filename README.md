# SEMS — Station Energy Management System

The load-management "brain" of an EV charging station: it distributes the available power
across concurrent charging sessions without ever exceeding the grid, charger or connector
limits, and recomputes the distribution on every event (vehicle plugs in, changes its demand,
leaves).

Written in Rust (actix-web). The design process, every decision and the alternatives considered
are traced step by step in [`JOURNAL.md`](JOURNAL.md).

> Current version: **V3.1** — max-min fair water-filling (no power left unused while a vehicle
> could take it) and **safe transitions**: a decrease is applied before any increase, so the
> grid is never exceeded, even while chargers apply their new setpoints. Earlier versions are on
> branches `v1` and `v2`. See the [roadmap](#roadmap).

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
# A is lowered to 200 kW at once, but B stays at 0 kW ("target_kw": 200) until CP1
# acknowledges its decrease: "pending_acks": [{"charger": "CP1", "version": 2}].
curl -X POST localhost:8080/sessions -H 'content-type: application/json' \
     -d '{"charger": "CP2", "connector": 1, "demand_kw": 300}'

# CP1 acknowledges version 2 (a real charger does it as soon as it receives the message):
# B gets its 200 kW.
curl -X POST localhost:8080/chargers/CP1/ack -H 'content-type: application/json' -d '{"version": 2}'

# The first vehicle's battery is nearly full: its demand drops to 50 kW. That is a decrease,
# so CP1 must acknowledge again ("pending_acks": CP1, version 4) before B can take more.
curl -X PUT localhost:8080/sessions/<id-A> -H 'content-type: application/json' -d '{"demand_kw": 50}'

# It leaves. B still waits for CP1's acknowledgement (safety first, even though CP1 now has no
# vehicle); once CP1 acknowledges version 4, B gets its full 300 kW.
curl -X DELETE localhost:8080/sessions/<id-A>
curl -X POST localhost:8080/chargers/CP1/ack -H 'content-type: application/json' -d '{"version": 4}'

# OPS view: who is charging, at what power, what is left.
curl localhost:8080/status
```

The server logs one line per accepted event on stdout (`docker compose logs -f sems`).

## Run the tests

With Docker only:

```bash
# All tests: unit, scenarios, randomized, push
docker compose run --rm test
# Scenarios one by one, with their metrics and the summary table
docker compose run --rm test cargo test --test scenarios -- --nocapture
# Randomized test on more stations (default 50), or replay one seed after a failure
docker compose run --rm -e SEMS_RUNS=1000 test cargo test --test random
docker compose run --rm -e SEMS_SEED=42 test cargo test --test random
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
  checks four properties: **safety** (no setpoint above its demand or connector, no charger or
  grid overload), **transition** (even if chargers still applied their previous setpoints, no
  charger or grid overload), **history** (an accepted request adds exactly one `/events` entry
  whose resulting allocation matches `/status`; a rejected one adds none) and **consistency**
  (`/status` totals and statuses match its sessions). By default the runner plays perfect
  chargers that acknowledge at once (the intermediate state is checked first); scenarios with
  `"acks": "manual"` acknowledge explicitly.
- **Randomized** (`tests/random.rs`): random stations (1–5 chargers of 7–350 kW, 10–1500 kW grid)
  and 100 random steps each, including invalid targets, absurd demands and acknowledgements that
  are late, stale, for a future version, for an unknown charger, or never sent. A shadow model
  predicts every HTTP code; the same properties are checked after every step, plus **no waste
  beyond rounding** (< 1 W per session) once settled. Seeded and reproducible: a failure prints
  its seed. It tests what no hand-written scenario imagined.
- **Push** (`tests/push.rs`): a real server on a random port, real WebSocket and HTTP clients.
  A charger is told its new setpoint when *another* charger's event changes it, is not
  disturbed when it does not, and an increase arrives only once the decreasing charger has
  acknowledged.
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
| S09 | Heterogeneous chargers | 300 kW DC, 50 kW DC and 22 kW AC on one site, enough grid for all |
| S10 | Saturated charger's share goes to the other | Nested limits: what a full charger cannot take goes to another charger |
| S11 | Invalid events | Clear error codes, state untouched by rejected events |
| S12 | Increase under contention | An increase is granted only within what is available |
| S13 | Cascading release | Released power across several vehicles and chargers |
| S14 | Zero demand | A full vehicle still plugged in takes no share |
| S15 | Plugged in, not charging | Status `charging` → `suspended_ev` → `charging` for OPS |
| S16 | Absurd demand values | Regression: powers above 1 GW are rejected (422) instead of overflowing |
| S17 | Heterogeneous chargers under contention | Same mix as S09 on a 100 kW grid: the small AC charger is served in full, the others share the rest |
| S18 | Increase waits for the acknowledgement | A decrease is published at once, the matching increase only after the acknowledgement; a departure frees power at once |
| S19 | Stale acknowledgement | Acknowledging an older version releases nothing while a newer decrease is on its way |
| S20 | Silent charger | Known limitation: a charger that never acknowledges freezes every increase (safety first) |

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
(`setpoints`, `targets`, `statuses`, `pending`, `allocated_kw`, `available_kw`, `unserved_kw`,
`http`); without `http`, a 2xx is required. With `"acks": "manual"`, a step can be
`{ "ack": "CP1" }` (its latest version) or `{ "ack": { "charger": "CP1", "version": 2 } }`.

## Communication: who speaks first

The interface is deliberately **asymmetric**, and one question decides each channel: *who
initiates the exchange?*

| Direction | Who speaks first | Channel | Messages |
|---|---|---|---|
| Charger → server | the charger | **HTTP** (REST) | session started, demand changed, session ended, **acknowledgement** |
| Server → charger | **the server** | **WebSocket** | new setpoints |

- Whatever happens *at* a charger (a vehicle plugs in, renegotiates, leaves; a setpoint was
  received) is initiated by the charger: a plain HTTP request, answered by the server. Easy to
  test with `curl`, and to replay in memory in the scenario runner.
- New setpoints are initiated by **the server**: when B plugs into CP2, CP1 must lower A's power
  although CP1 sent nothing. Plain HTTP cannot speak first (a server only answers); without a
  push channel, CP1 would have to poll and latency would depend on the polling interval. The
  WebSocket is that push channel (P1 below: about 0.5 ms from B's event to CP1's new setpoint).
- The acknowledgement goes **from the charger to the server**, so it is an HTTP request like any
  other charger event, even though the setpoint it acknowledges came over the WebSocket.
- In a real deployment this would converge on **a single bidirectional WebSocket per charger**,
  as OCPP does (events, setpoints and their acknowledgements on one connection). HTTP in was
  kept for readability and testability.

## Safe transitions: decrease before increase

When B arrives, A must drop from 300 to 200 kW and B rise from 0 to 200 kW. If B applied its
setpoint before A received its own, the site would briefly draw 500 kW on a 400 kW grid. The
grid limit is treated as a hard boundary, never to be exceeded even briefly. So:

1. **Decreases are published at once.** The charger concerned is then *awaited*.
2. **Increases are held while any charger is awaited.** The session keeps its previous setpoint
   (a new vehicle stays at 0 kW, status `suspended_evse`); `/status` shows its `target_kw`.
3. **When the last awaited charger acknowledges, all held increases are granted.** An event that
   only frees power (a departure, a lower demand on its own) grants its increases at once.

Every session thus stays at or below a setpoint that was safe before the event: whatever order
chargers receive their messages in, no charger and not the grid is ever exceeded (checked after
every request by the scenario runner and the randomized test).

**Versions.** Every pushed message carries a `version` (the `seq` of the event that last changed
that charger's setpoints, see `/events`); the charger acknowledges it with
`POST /chargers/{id}/ack {"version": n}`. Two decreases can be sent to the same charger faster
than an acknowledgement travels back; an acknowledgement of the older one proves nothing about
the newer one, so only the charger's latest version releases it (S19). A version never sent is
rejected (422).

**Assumption.** A setpoint received is applied at once (no ramp-time modelling): the
acknowledgement of a reception therefore proves the charger draws at most its new setpoint.

**Known limitation: a charger that never acknowledges** (failure, lost link) freezes every
increase on the station (S20). This is the safe side, and OPS sees which charger blocks in
`pending_acks`. The natural next step is a timeout after which the charger is reported as
failed and excluded from the allocation; note that excluding it assumes it fell back to a
safe setpoint on its own (as OCPP's fallback profiles do), which the server cannot verify.
Also: OPS ending its sessions with `DELETE` does not release a silent charger's hold.

## API

All powers are in kW. Errors are `{"error": "<message>"}`.

| Method | Route | Body | Success | Errors |
|---|---|---|---|---|
| `POST` | `/sessions` | `{"charger", "connector", "demand_kw"}` | `201` | 404 unknown charger/connector, 409 connector busy, 422 invalid body or demand (powers are 0 to 1,000,000 kW) |
| `PUT` | `/sessions/{id}` | `{"demand_kw"}` | `200` | 404 unknown session, 422 |
| `DELETE` | `/sessions/{id}` | — | `200` | 404 unknown session |
| `POST` | `/chargers/{id}/ack` | `{"version"}` | `200` | 404 unknown charger, 422 version never sent |
| `GET` | `/status` | — | `200` | — |
| `GET` | `/events?limit=100&session=<id>` | — | `200` | 422 invalid `limit` or `session` |
| `GET` | `/chargers/{id}/setpoints` | — | `200`, or WebSocket upgrade | 404 unknown charger |

Session event responses: `{"session_id", "setpoint_kw", "target_kw", "status"}`; an
acknowledgement answers `{"status"}`. `status` is the full station state (a demo convenience: a
real charger should only see its own setpoints). `setpoint_kw` is what is granted, possibly
below `target_kw` while an increase is held.

### Setpoints for chargers (push and pull)

`/chargers/{id}/setpoints` is the charger's channel. Opened as a **WebSocket**, it sends the
current setpoints, then a message **whenever one of them changes** — including when the change
is caused by another charger's event (a vehicle arriving elsewhere lowers this charger's share).
A plain **GET** on the same URL returns the same message: a pull fallback while a WebSocket
reconnects.

```json
{ "charger": "CP1", "version": 7,
  "setpoints": [ { "connector": 1, "session_id": "9f1c…", "kw": 200 },
                 { "connector": 2, "session_id": null, "kw": 0 } ] }
```

Try it from a browser console (nothing to install), then post events with `curl`:

```js
const ws = new WebSocket("ws://localhost:8080/chargers/CP1/setpoints");
ws.onmessage = (m) => console.log(m.data);
```

Messages sent by the charger on the WebSocket are ignored (pings are answered): events and
acknowledgements go over HTTP. Closing the WebSocket releases nothing (see limitations).

`GET /status` exposes, per station, charger and connector:

| Field | Meaning |
|---|---|
| `demand_kw` | What the vehicle accepts right now (input) |
| `setpoint_kw` | What the station grants and publishes (output), always ≤ `demand_kw` |
| `target_kw` | What the allocator would give; above `setpoint_kw` while an increase is held |
| `pending_acks` | Station level: chargers whose acknowledgement is awaited, with the version to acknowledge |
| `version` | Charger level: version of its latest setpoints |
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
`session_started`, `demand_changed`, `session_ended` or `ack` (with `version`, and `null`
session fields): *"why did B wait 2 s at 0 kW?"* is answered by the `ack` entry that released it.
The last 10,000 events are kept; `seq` never restarts, so a gap reveals dropped entries; it is
also the `version` of pushed setpoints. Rejected requests are not recorded.
`allocated_kw ≤ grid_kw` on every entry is the safety audit.

## Key decisions

Full reasoning and rejected alternatives: [`JOURNAL.md`](JOURNAL.md).

- **Cascading limits** grid > charger > connector > vehicle, all enforced at once.
- **Allocation (V2) — max-min fair water-filling.** Picture each session as a glass as tall as
  what it can take; power is poured into all glasses at the same level. A small glass fills up
  first and what it does not take keeps raising the others. Result: no power is left unused
  while a vehicle could take it, and nobody can get more without taking from someone served
  less. The cascade takes two fillings:

  ```
  cap        = min(demand, connector)
  μ_charger  = fill(charger max, caps of its sessions)     ← level inside each charger
  cap'       = min(cap, μ_charger)
  λ_grid     = fill(grid, all cap')                         ← level across the station
  setpoint   = min(cap', λ_grid)
  ```

  `fill` sorts the caps and serves in full every cap below the equal share of what is left;
  the first cap above it sets the level. O(n log n), rounded down (at most n − 1 W unused).
  Example (S10, grid 500 kW): A and B on CP1 (300 kW) are capped at 150 kW each by their
  charger, so C on CP2 takes the remaining 200 kW.
- **V1 allocation — capped equal share** (branch `v1`):
  `min(demand, connector, grid / n, charger / n_charger)`. Simple and safe, but power left by one
  session was not redistributed: up to 166.7 kW wasted in our scenarios.
  Rejected alternatives: proportional to demand (favours big requests), first come first served
  (unfair), linear programming (oversized); weighted max-min is the natural extension for
  priorities.
- **Integer watts internally, rounded down**: sums can never exceed a limit by rounding.
- **Full recomputation on every event**, under a single lock: the "Σ ≤ grid" invariant is
  global, and the computation takes microseconds.
- **In-memory state**, no database: the hot path stays fast; history persistence is a later step.
- **History recorded in the same operation as the event** (the model's single event path): an
  entry can never be missing or out of step with the state; bounded ring buffer (`VecDeque`).
- **Decrease before increase** (V3): a station-wide acknowledgement barrier, all or nothing,
  versions taken from the history's `seq`. Simplest rule that keeps the grid safe during
  transitions; see [Safe transitions](#safe-transitions-decrease-before-increase).
- **HTTP in, WebSocket out**: see [Communication](#communication-who-speaks-first).

## Assumptions and limitations

- Power is modelled in kW (real chargers are often driven in amperes) and is continuously
  divisible (internal charger "stacks" are not modelled).
- One server per station. No authentication. State is lost on restart.
- A setpoint received is assumed applied at once (no ramp time modelled); an acknowledgement
  confirms reception, not a measured power.
- A charger that never acknowledges freezes every increase on the station (S20); timeout and
  exclusion are the documented next step (see [Safe transitions](#safe-transitions-decrease-before-increase)).
- The barrier is **station-wide and all or nothing**: a pending decrease on one charger also
  holds increases on unrelated chargers, and an increase that would already fit is held too.
  A finer barrier would serve faster at the cost of more logic.
- A charger that disappears without ending its session keeps its power reserved (safety first);
  OPS can end its sessions with `DELETE /sessions/{id}`, which does not lift a pending
  acknowledgement.

## Performance

`cargo test --release --test perf -- --ignored --nocapture --test-threads=1`. Release build,
natively on a 2017 MacBook Pro (i7-7700HQ); ranges over 3 runs. Client and server share the
machine, so these are upper bounds; max values vary with OS scheduling.

| Test | What | p50 | p99 | max |
|---|---|---|---|---|
| **P1** | Event on one charger → **decrease** received over WebSocket by **another** charger (200 runs) | 197–220 µs | 352–475 µs | 404–642 µs |
| **P2** | 8 concurrent clients × 500 demand changes on 20 sessions: per-request latency | 1.1–1.2 ms | 3.1–3.5 ms | 7.6–8.5 ms |
| **P3** | Allocation alone, 4 sessions | 1.0–1.5 µs | 2–8 µs | ≤ 27 µs |
| | Allocation alone, 100 sessions | 13 µs | 19–65 µs | ≤ 183 µs |
| | Allocation alone, 10,000 sessions | 1.3 ms | 1.7–2.4 ms | ≤ 9 ms (one 44 ms outlier) |

- **P1, the brief's requirement** (< 1 s): the worst case measured is about 0.6 ms, some
  1,500 times below the budget. P1 measures a **decrease**, the safety-critical direction,
  published at once. Since V3 an increase additionally waits for the decreasing chargers'
  acknowledgement round trip: that delay depends on the chargers, not on the server.
- **P2**: **5,000–5,500 events/s** with the safety invariant checked on every one of the
  4,000 responses, and every event found in `/events`. A real station sees a few events per
  second: this is headroom, not a target. Each response carries the full station state,
  which is part of this cost.
- **Across versions**: the water-filling (V2, a sort, O(n log n)) made the allocation about
  5 times slower than V1's equal share (250 µs → 1.3 ms at 10,000 sessions), and the V3
  barrier adds a pass over the sessions; yet end-to-end figures barely move (P1 max: V1
  444–582 µs, V2 408–513 µs, V3 404–642 µs; P2: 5,300–5,900, 5,600–5,900, 5,000–5,500 events/s).
  Computation is never the bottleneck, the network is.

## Allocation metrics: V1 → V2 → V3

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
| Overshoot (kW) | Grid overload if every charger applied the larger of its old and new setpoint during a transition (since V3: measured on every transition, intermediate states included) |

**V1 → V2: no waste** (scenarios whose metrics moved; the full V1 table is in `JOURNAL.md`, step 6):

| Scenario | Waste V1 → V2 (kW) | Efficiency V1 → V2 (%) | Fairness V1 → V2 | Churn V1 → V2 (setpoints/event) | Overshoot V1 → V2 (kW) |
|---|---|---|---|---|---|
| S07 demand down then up | 100.0 → **0.0** | 71.4 → **100.0** | 0.96 → 1.00 | 1.2 → 1.8 | 100.0 → 100.0 |
| S09 heterogeneous chargers | 166.7 → **0.0** | 55.2 → **100.0** | 0.91 → 1.00 | 1.7 → 1.0 | 0.0 → 0.0 |
| S10 saturated charger's share | 33.3 → **0.0** | 93.3 → **100.0** | 1.00 → 0.98 | 1.3 → 1.3 | 0.0 → 0.0 |
| S12 increase under contention | 100.0 → **0.0** | 71.4 → **100.0** | 0.96 → 1.00 | 1.0 → 1.5 | 0.0 → 100.0 |
| S13 cascading release | 113.3 → **0.0** | 68.8 → **100.0** | 0.85 → 0.95 | 2.4 → 2.7 | 100.0 → 113.3 |
| S15 plugged in, not charging | 30.0 → **0.0** | 90.0 → **100.0** | 0.90 → 0.94 | 1.8 → 1.8 | 50.0 → 50.0 |

**V2 → V3: no transition overshoot** (every scenario that had one):

| Scenario | Overshoot V2 → V3 (kW) |
|---|---|
| S03, S15 | 50.0 → **0.0** |
| S04, S05, S06, S07, S12, S14, S16 | 100.0 → **0.0** |
| S13 cascading release | 113.3 → **0.0** |
| S17 heterogeneous chargers under contention | 39.0 → **0.0** |

**V3, all scenarios** (S01–S17 measured on the settled station; S18–S20 acknowledge manually,
so their figures include the time spent waiting for acknowledgements):

| Scenario | Waste max (kW) | Efficiency min (%) | Satisfaction min (%) | Fairness min | Churn avg (setpoints/event) | Overshoot max (kW) |
|---|---|---|---|---|---|---|
| S01 empty station | 0.0 | 100.0 | 100.0 | 1.00 | 0.0 | 0.0 |
| S02 single vehicle | 0.0 | 100.0 | 85.7 | 1.00 | 1.0 | 0.0 |
| S03 two vehicles on the same charger | 0.0 | 100.0 | 50.0 | 1.00 | 1.5 | 0.0 |
| S04 two vehicles on different chargers | 0.0 | 100.0 | 66.7 | 1.00 | 1.5 | 0.0 |
| S05 a vehicle leaves | 0.0 | 100.0 | 66.7 | 1.00 | 1.7 | 0.0 |
| S06 full station | 0.0 | 100.0 | 33.3 | 1.00 | 2.5 | 0.0 |
| S07 demand decreases then increases | 0.0 | 100.0 | 66.7 | 1.00 | 1.8 | 0.0 |
| S08 demand above the connector limit | 0.0 | 100.0 | 50.0 | 1.00 | 1.0 | 0.0 |
| S09 heterogeneous chargers | 0.0 | 100.0 | 100.0 | 1.00 | 1.0 | 0.0 |
| S10 saturated charger's share goes to the other charger | 0.0 | 100.0 | 50.0 | 0.98 | 1.3 | 0.0 |
| S11 invalid events | 0.0 | 100.0 | 100.0 | 1.00 | 1.0 | 0.0 |
| S12 demand increase under contention | 0.0 | 100.0 | 66.7 | 1.00 | 1.5 | 0.0 |
| S13 cascading release | 0.0 | 100.0 | 33.3 | 0.95 | 2.7 | 0.0 |
| S14 zero demand takes no share | 0.0 | 100.0 | 66.7 | 1.00 | 1.7 | 0.0 |
| S15 plugged in but not charging | 0.0 | 100.0 | 50.0 | 0.94 | 1.8 | 0.0 |
| S16 absurd demand values | 0.0 | 100.0 | 0.0 | 1.00 | 1.5 | 0.0 |
| S17 heterogeneous chargers under contention | 0.0 | 100.0 | 26.9 | 0.74 | 1.7 | 0.0 |
| S18 increase waits for the acknowledgement | 200.0 | 50.0 | 33.3 | 0.50 | 1.2 | 0.0 |
| S19 stale acknowledgement | 266.7 | 33.3 | 14.8 | 0.33 | 1.0 | 0.0 |
| S20 silent charger freezes increases | 200.0 | 50.0 | 33.3 | 0.50 | 0.5 | 0.0 |

**Reading**

- **Gain**: waste drops to **0 kW in every scenario** (V1: up to 166.7 kW) and efficiency is
  100 % everywhere; the randomized test confirms it on random stations (< 1 W per session).
- **Cost of V2, assumed**: redistributing disturbs more chargers (churn up to +0.6
  setpoints/event) and moved more power between chargers, so the potential transition
  overshoot grew (S12: 0 → 100 kW, S13: 100 → 113.3 kW).
- **V3 removes it**: overshoot is **0 kW on every transition of every scenario**, intermediate
  states included (the runner checks them before acknowledging), and the randomized test checks
  it with acknowledgements arriving late, stale or never.
- **Price of V3's safety**: while a charger has not acknowledged, increases wait. S18–S20 show
  it: up to 266.7 kW unused while acknowledgements are pending (S19), and indefinitely if a
  charger stays silent (S20). With real chargers acknowledging within milliseconds, the
  settled figures (S01–S17) are what a station sees.
- **Fairness must be read with care**: Jain's index compares service *ratios*, not what each
  vehicle *could* get. In S17 it is 0.74 although the split is max-min optimal: the 300 kW
  vehicle gets 39 kW (13 %) on a 100 kW grid, the 22 kW one is served in full (100 %). In S10
  (0.98), C is served at 67 % and A and B at 50 % because their charger cannot give more.
  A low index can mean "unequal requests", not "unfair system".

## Roadmap

| Version | Content |
|---|---|
| **V1** ✅ | REST API, scenarios, Docker, WebSocket push of setpoints, `/events` history, allocation metrics, end-to-end performance tests |
| **V2** ✅ | Max-min fair water-filling: no waste, released power goes to the other sessions; randomized property test (random stations and events, properties at every step) |
| **V3.1** ✅ | Safe transitions: decrease before increase, versioned acknowledgements over HTTP, transition invariant checked everywhere |
| V3.2 | Offline chargers visible to OPS (WebSocket lost); timeout for a charger that never acknowledges |
| Later | Minimum viable power per session (UX more than safety); finer acknowledgement barrier |
| V4 | Battery storage (BESS): boost, 10% SoC floor, opportunistic charging |
