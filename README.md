# SEMS — Station Energy Management System

The load-management "brain" of an EV charging station: it distributes the available power
across concurrent charging sessions without ever exceeding the grid, charger or connector
limits, and recomputes the distribution on every event (vehicle plugs in, changes its demand,
leaves).

Written in Rust (actix-web). The design process, every decision and the alternatives considered
are traced step by step in [`JOURNAL.md`](JOURNAL.md).

> Current version: **V4** — max-min fair water-filling (no power left unused while a vehicle
> could take it), **safe transitions** (a decrease is applied before any increase, so the grid
> is never exceeded, even while chargers apply their new setpoints), a live **OPS view**
> (connector and charger availability, power availability) and an optional **battery (BESS)**
> (boost, 10 % state-of-charge floor, opportunistic charging). Earlier versions are on branches
> `v1`, `v2` and `v3`. See the [roadmap](#roadmap).

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
  are late, stale, for a future version, for an unknown charger, or never sent; one station in
  two has a BESS whose state of charge is reported at random. A shadow model
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
| S21 | Pending decrease holds an unrelated increase | The barrier is station-wide: an increase elsewhere waits even when it would already fit |
| S22 | OPS view during a pending acknowledgement | The exposed state (statuses, targets, pending chargers, totals) reflects a partial transition |
| S23 | BESS boost | Above the 10 % floor, two vehicles get 300 kW each on a 400 kW grid; the BESS discharges exactly the excess, the grid import stays at 400 kW |
| S24 | BESS state-of-charge floor | Below 10 % the boost disappears: vehicles drop back to the grid's share, the BESS idles; above 10 % again, the boost returns |
| S25 | BESS opportunistic charge | The BESS charges only with what the vehicles leave, idles when full or while its SoC is unknown |

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
A step `{ "soc": 0.42 }` reports the BESS state of charge; BESS expectations are `capacity_kw`,
`grid_import_kw`, `bess_mode` and `bess_kw`.

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

## Battery storage (BESS)

Optional, declared in the station config:

```json
"bess": { "capacity_kwh": 200, "max_kw": 200 }
```

Without it, nothing changes. With it, three rules, recomputed on every event:

| Rule | Definition |
|---|---|
| **Boost** and **anti over-discharge** | Power offered to vehicles = `grid + max_kw` while the state of charge (SoC) is above **10 %**, `grid` otherwise (or while the SoC is unknown). The water-filling runs unchanged on that capacity. |
| **One flow at a time** | With V the vehicles' granted total: **discharging** `V − grid` if V > grid; else **charging** `min(grid − V, max_kw)` while not full (vehicles first: the BESS only takes what they leave); else **idle** (no flow: power 0). |
| **Grid connection** | The import is `V − discharge + charge`: always between 0 and the grid limit, never an overload, never an export. |

The BMS reports the SoC: `POST /bess {"soc": 0.42}` (0 to 1). The BESS speaks first, so it is an
HTTP request, like a charger event; it may change the capacity, hence the allocation, and is
recorded in `/events` (kind `bess_soc`). `GET /status` adds `capacity_kw`, `grid_import_kw` and a
`bess` block: `soc`, `mode` (`charging`, `discharging`, `idle`), `power_kw`, `boost_kw`,
`energy_above_floor_kwh`.

**Why no acknowledgement for the BESS.** Commanding its power from the server would make every
boost transition unsafe one way or the other: vehicles ramping up first overload the grid, the
BESS discharging first exports. Instead, the BESS works as station BESS do (peak shaving): its
inverter regulates the flow at the grid connection locally, discharging only what the site draws
beyond the grid and charging only with the headroom. The server only *plans*: it decides the
capacity offered to vehicles and the mode, and vehicle setpoints keep going through the
acknowledgement barrier. When the SoC falls below 10 %, vehicle decreases are published at once
and the BESS keeps covering them until they are applied: that is why the floor is the
software's own rule, above the BMS hardware cut-off. The barrier stays for the chargers, where a
third-party vehicle and a network link bring uncertainty; the BESS is the station's own asset.

**Assumptions.** The SoC is an input received from the BMS, never computed here (no energy
integration over time). The BESS applies its mode reliably and almost instantly, regulating the
grid connection locally (no export). Its hardware cut-off is below the 10 % floor.

## API

All powers are in kW. Errors are `{"error": "<message>"}`.

| Method | Route | Body | Success | Errors |
|---|---|---|---|---|
| `POST` | `/sessions` | `{"charger", "connector", "demand_kw"}` | `201` | 404 unknown charger/connector, 409 connector busy, 422 invalid body or demand (powers are 0 to 1,000,000 kW) |
| `PUT` | `/sessions/{id}` | `{"demand_kw"}` | `200` | 404 unknown session, 422 |
| `DELETE` | `/sessions/{id}` | — | `200` | 404 unknown session |
| `POST` | `/chargers/{id}/ack` | `{"version"}` | `200` | 404 unknown charger, 422 version never sent |
| `POST` | `/bess` | `{"soc"}` (0 to 1) | `200` | 404 station without BESS, 422 invalid SoC |
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
| `online`, `online_since_ms` | Charger level: a setpoint WebSocket is open, and since when it is online or offline |
| `capacity_kw` | Station level: power offered to vehicles, the grid plus the BESS boost |
| `grid_import_kw` | Station level: drawn from the grid, always between 0 and `grid_kw` |
| `bess` | Station level: `soc`, `mode`, `power_kw`, `boost_kw`, `energy_above_floor_kwh` (`null` without BESS) |
| `available_kw` (charger) | Charger headroom: `max_kw − allocated_kw` (not necessarily usable: the grid may be saturated) |

### OPS view

The brief asks to *"let Electra OPS see the live state of the station (connector and charger
availability, power availability…)"*. All of it is in `GET /status`, read at any time:

| The brief asks for | Where in `GET /status` |
|---|---|
| **Connector availability** | `chargers[].connectors[].status`: `available`, `charging`, `suspended_ev` (plugged in, the vehicle requests nothing), `suspended_evse` (plugged in, the station gives nothing yet), with `status_since_ms` |
| **Charger availability** | `chargers[].online` (its setpoint WebSocket is open) and `online_since_ms` |
| **Power availability** | Station: `grid_kw`, `capacity_kw`, `allocated_kw`, `available_kw`, `unserved_kw`, `grid_import_kw`; BESS: `bess.mode`, `bess.power_kw`, `bess.boost_kw`, `bess.energy_above_floor_kwh`. Charger: `max_kw`, `allocated_kw`, `available_kw`, `unserved_kw`. Session: `demand_kw`, `setpoint_kw`, `target_kw` |
| What is blocking | `pending_acks`: chargers whose acknowledgement holds increases |
| What happened | `GET /events`: every accepted event with the allocation that resulted from it |

A charger going offline **releases nothing**: it may still be drawing power, so its sessions
keep their setpoints and, if it owed an acknowledgement, it stays in `pending_acks`. OPS sees
both facts side by side.
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
- **BESS as a planning input** (V4): the server sizes the capacity and decides the mode; the
  BESS regulates the grid connection locally; no acknowledgement for the station's own asset.
  See [Battery storage](#battery-storage-bess).

## Assumptions and limitations

- Power is modelled in kW (real chargers are often driven in amperes) and is continuously
  divisible (internal charger "stacks" are not modelled).
- One server per station. State is lost on restart (not handled).
- A setpoint received is assumed applied at once (no ramp time modelled); an acknowledgement
  confirms reception, not a measured power.
- **BESS**: the SoC is an input from the BMS (not computed); the BESS regulates the grid
  connection locally and applies its mode almost instantly; no hysteresis around the 10 % floor,
  so a SoC oscillating around it switches the boost on and off (a UX defect; hysteresis is the
  natural improvement).
- **No minimum viable power per session (not covered).** With many vehicles on a small grid,
  shares can become too small to be useful. The sketched rule: a configurable minimum; sessions
  admitted in arrival order while each can get it; the others wait at 0 kW as `suspended_evse`,
  re-evaluated on every event. Not needed for the brief's station (at least 100 kW per vehicle).
- **Silent charger.** Current behaviour: maximum safety, a charger that never acknowledges
  freezes every increase on the station (S20). Limitation: no graceful degradation when a single
  charger fails. Next step: a timeout after which the charger is marked failed, excluded from the
  allocation and reported to OPS, so the rest of the station works normally.
- **Starvation of increases** under heavy activity is the price of the station-wide barrier; the
  delay before an increase is granted is a metric to instrument in production.
- The barrier is **station-wide and all or nothing**: a pending decrease on one charger also
  holds increases on unrelated chargers, and an increase that would already fit is held too.
  A finer barrier would serve faster at the cost of more logic.
- A charger that disappears without ending its session keeps its power reserved (safety first);
  OPS sees it `online: false` and can end its sessions with `DELETE /sessions/{id}`, which does
  not lift a pending acknowledgement.
- `online` means "reachable for setpoint push": a charger that only polls
  `GET /chargers/{id}/setpoints` shows as offline. Connections and disconnections are logged on
  stdout, not recorded in `/events`.

### Known risks (assumed, not solved)

- **No authentication.** A malicious actor could spoof an acknowledgement or falsify a SoC and
  influence the physical safety of the system (lift the barrier, trigger a boost). A real
  deployment needs authentication and validation of charger and BESS inputs.
- **The BESS is not measured.** The server computes `grid_import_kw`, it does not measure it: a
  BESS drifting from its expected behaviour would go undetected, which would be unacceptable in
  production. A meter at the grid connection should be read and checked.
- **Stale SoC.** If the BMS stops reporting, the last SoC is kept: the station may keep planning
  a boost while the battery actually drains, and the 10 % floor then gives a false sense of
  safety. Same principle as the silent charger: a critical input must not be trusted forever.
  Fixing it properly needs a watchdog timer (the system is otherwise purely event-driven: a
  check on the next event only would leave a quiet station unprotected); the same watchdog
  would implement the charger acknowledgement timeout.
- **A panic while holding the station lock stops the service** — a deliberate fail-safe. A panic
  means an invariant was broken; carrying on with a possibly corrupted state could grant power
  beyond the limits. Stopped, the server publishes nothing: chargers keep their last setpoints,
  which were safe, and no increase is granted; supervision detects the outage. Inputs that could
  panic are rejected at the edge (powers above 1 GW, invalid SoC). Possible evolution: apply each
  event to a copy of the state and roll back on panic, so only the faulty request fails.

## Performance

`cargo test --release --test perf -- --ignored --nocapture --test-threads=1`. Release build,
natively on a 2017 MacBook Pro (i7-7700HQ); ranges over 3 runs. Client and server share the
machine, so these are upper bounds; max values vary with OS scheduling.

| Test | What | p50 | p99 | max |
|---|---|---|---|---|
| **P1** | Event on one charger → **decrease** received over WebSocket by **another** charger (200 runs) | 192–199 µs | 316–382 µs | 325–385 µs |
| **P2** | 8 concurrent clients × 500 demand changes on 20 sessions: per-request latency | 1.2 ms | 3.6–4.4 ms | 7.2–23 ms |
| **P3** | Allocation alone, 4 sessions | 1.0 µs | 6–12 µs | ≤ 61 µs |
| | Allocation alone, 100 sessions | 13 µs | 17–37 µs | ≤ 72 µs |
| | Allocation alone, 10,000 sessions | 1.3–1.4 ms | 2.0–2.2 ms | ≤ 14 ms |

**P4 — the brief's requirement, end to end.** What counts is the time from an event until
**every affected charger holds its final setpoint**, the whole cascade included. Simulated
chargers (one WebSocket each) acknowledge every setpoint as soon as they receive it, as real
ones would; in "cascade" cases the measure includes the decrease, its acknowledgement round trip
and the increase it releases. 200 runs per case, ranges over 3 runs:

| Event | Active sessions | Ends when | p50 | p99 | max |
|---|---|---|---|---|---|
| Connect, no cascade | 0 → 1 | A receives 300 kW | 1.1–1.9 ms | 2.0–3.9 ms | 2.1–149 ms |
| Connect, **cascade** | 1 → 2 | B plugs in: A receives 200 kW, acknowledges, then **B receives 200 kW** | 1.7–3.3 ms | 2.8–98 ms | 5.6–125 ms |
| Disconnect, no cascade | 1 → 0 | A leaves: CP1 receives 0 kW | 1.1–1.7 ms | 2.2–8.6 ms | 10–58 ms |
| Disconnect, **cascade** | 2 → 1 | A leaves: **B receives 300 kW** | 1.1–2.1 ms | 2.0–160 ms | 2.0–197 ms |
| Demand down, **cascade** | 2 | A drops to 50 kW, acknowledges, then **B receives 300 kW** | 1.7–2.3 ms | 2.5–167 ms | 2.6–285 ms |
| Demand up, **cascade** | 2 | A asks 300 kW: B receives 200 kW, acknowledges, then **A receives 200 kW** | 1.7–2.1 ms | 5.4–187 ms | 18–289 ms |
| **Full station**, cascade on both chargers | 3 → 4 | D plugs into CP2: A, B (CP1) and C (CP2) drop to 100 kW, **both chargers acknowledge**, then **D receives 100 kW** | 1.8–2.0 ms | 7.1–305 ms | 8.2–377 ms |

Every case, cascade and acknowledgement round trips included, stays far below 1 s (the test
asserts it); the typical cost is about 2 ms. Tails vary from run to run (client, server and
simulated chargers share one 2017 laptop). The full-station case, the heaviest the brief's
station allows (two acknowledgements before the increase), stays under 0.4 s at worst. With real
chargers, network latency would add to these figures.

- **P1, the brief's requirement** (< 1 s): the worst case measured is about 0.4 ms, some
  2,500 times below the budget. P1 measures a **decrease**, the safety-critical direction,
  published at once. Since V3 an increase additionally waits for the decreasing chargers'
  acknowledgement round trip: that delay depends on the chargers, not on the server.
- **P2**: **4,600–4,950 events/s** with the safety invariant checked on every one of the
  4,000 responses, and every event found in `/events`. A real station sees a few events per
  second: this is headroom, not a target. Each response carries the full station state,
  which is part of this cost.
- **Across versions**: the water-filling (V2, a sort, O(n log n)) made the allocation about
  5 times slower than V1's equal share (250 µs → 1.3 ms at 10,000 sessions), and the V3
  barrier adds a pass over the sessions; yet end-to-end figures barely move (P1 max: V1
  444–582 µs, V2 408–513 µs, V3 404–642 µs, V4 325–385 µs; P2: 5,300–5,900, 5,600–5,900,
  5,000–5,500, 4,600–4,950 events/s; V4 responses also carry the BESS block).
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

**V3 and V4, all scenarios** (S01–S17 and S23–S25 measured on the settled station; S18–S22 acknowledge manually,
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
| S21 pending decrease holds an unrelated increase | 150.0 | 50.0 | 50.0 | 0.74 | 0.8 | 0.0 |
| S22 OPS view during a pending acknowledgement | 200.0 | 50.0 | 33.3 | 0.50 | 1.0 | 0.0 |
| S23 BESS boost | 0.0 | 100.0 | 100.0 | 1.00 | 0.7 | 0.0 |
| S24 BESS state-of-charge floor | 0.0 | 100.0 | 66.7 | 1.00 | 1.2 | 0.0 |
| S25 BESS opportunistic charge | 0.0 | 100.0 | 100.0 | 1.00 | 0.5 | 0.0 |

**Reading**

- **Gain**: waste drops to **0 kW in every scenario** (V1: up to 166.7 kW) and efficiency is
  100 % everywhere; the randomized test confirms it on random stations (< 1 W per session).
- **Cost of V2, assumed**: redistributing disturbs more chargers (churn up to +0.6
  setpoints/event) and moved more power between chargers, so the potential transition
  overshoot grew (S12: 0 → 100 kW, S13: 100 → 113.3 kW).
- **V3 removes it**: overshoot is **0 kW on every transition of every scenario**, intermediate
  states included (the runner checks them before acknowledging), and the randomized test checks
  it with acknowledgements arriving late, stale or never.
- **Price of V3's safety**: while a charger has not acknowledged, increases wait. S18–S22 show
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
| **V3.2** ✅ | OPS view: charger availability (`online`, `online_since_ms`) and available power per charger |
| **V4** ✅ | Battery storage (BESS): boost, 10 % SoC floor, opportunistic charging, one flow at a time, never an export |
| Later | Timeout for a charger that never acknowledges; hysteresis around the SoC floor; minimum viable power per session (UX more than safety); finer acknowledgement barrier |
