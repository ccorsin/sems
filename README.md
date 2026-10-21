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

```bash
docker compose run --rm test                     # with Docker
cargo test                                       # natively (stable Rust)
cargo test --test scenarios -- --nocapture       # one block per scenario
cargo test --release --test perf -- --ignored --nocapture   # performance
```

- **Unit tests** (`src/*.rs`): configuration validation, session lifecycle and statuses,
  allocation algorithm, malformed requests.
- **Scenarios** (`scenarios/*.json`): event sequences replayed through the API, with expected
  setpoints, totals, statuses and HTTP codes. After **every step of every scenario** the runner
  checks the safety invariant: no setpoint above its demand or connector, no charger or grid
  overload.
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
| `POST` | `/sessions` | `{"charger", "connector", "demand_kw"}` | `201` | 404 unknown charger/connector, 409 connector busy, 422 invalid body or demand |
| `PUT` | `/sessions/{id}` | `{"demand_kw"}` | `200` | 404 unknown session, 422 |
| `DELETE` | `/sessions/{id}` | — | `200` | 404 unknown session |
| `GET` | `/status` | — | `200` | — |
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

Allocation time vs number of sessions (release build, 2017 MacBook Pro, 1,000 runs each):

| Sessions | p50 | p99 | max |
|---|---|---|---|
| 4 | 0.36 µs | 0.38 µs | 1.4 µs |
| 100 | 2.7 µs | 5.1 µs | 36.4 µs |
| 10,000 | 236 µs | 365 µs | 432 µs |

Linear growth, far below the 1 s budget: end-to-end latency is dominated by the network.

## Roadmap

| Version | Content |
|---|---|
| **V1** (in progress) | REST API ✅, scenarios ✅, Docker ✅, WebSocket push of setpoints ✅; next: `/events` history, metrics and end-to-end performance tests |
| V2 | Water-filling allocation: no waste, released power goes to the other sessions |
| V3 | "Decrease before increase" transitions, minimum viable power per session, offline chargers |
| V4 | Battery storage (BESS): boost, 10% SoC floor, opportunistic charging |
