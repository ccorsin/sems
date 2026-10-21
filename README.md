# SEMS — Station Energy Management System

The load-management "brain" of an EV charging station: it distributes the available power
across concurrent charging sessions without ever exceeding the grid, charger or connector
limits, and recomputes the distribution on every event (a vehicle plugs in, changes its demand,
leaves).

Written in Rust (actix-web). Every decision and the alternatives considered are traced step by
step in [`JOURNAL.md`](JOURNAL.md).

> Current version: **V4** — max-min fair water-filling (no power left unused while a vehicle
> could take it), **safe transitions** (a decrease is applied before any increase, so the grid
> is never exceeded, even while chargers apply their new setpoints), a live **OPS view** and an
> optional **battery (BESS)**. Earlier versions are on branches `v1`, `v2` and `v3`; see the
> [roadmap](#roadmap).

**Contents** — [Quick start](#quick-start) · [Station configuration](#station-configuration) ·
[API](#api) · [OPS view](#ops-view) · [Communication](#communication-who-speaks-first) ·
[Safe transitions](#safe-transitions-decrease-before-increase) ·
[Battery storage](#battery-storage-bess) · [Run the tests](#run-the-tests) ·
[Key decisions](#key-decisions) · [Assumptions and limitations](#assumptions-and-limitations) ·
[Known risks](#known-risks-assumed-not-solved) · [Performance](#performance) ·
[Allocation metrics](#allocation-metrics) · [Roadmap](#roadmap)

## Quick start

Requires Docker only.

```bash
docker compose up --build        # server on http://localhost:8080, brief's station
```

The default station (`config/station.json`) is the brief's: a 400 kW grid and 2 chargers (CP1,
CP2) of 300 kW with 2 CCS connectors each.

```bash
# A vehicle plugs into CP1 and accepts 300 kW: it gets all of it.
curl -X POST localhost:8080/sessions -H 'content-type: application/json' \
     -d '{"charger": "CP1", "connector": 1, "demand_kw": 300}'
# → 201 {"session_id": "<id-A>", "setpoint_kw": 300.0, "status": {…}}

# A second vehicle on CP2: both are capped by the 400 kW grid (200 kW each).
# A is lowered to 200 kW at once; B stays at 0 kW ("target_kw": 200) until CP1
# acknowledges A's decrease: "pending_acks": [{"charger": "CP1", "version": 2}].
curl -X POST localhost:8080/sessions -H 'content-type: application/json' \
     -d '{"charger": "CP2", "connector": 1, "demand_kw": 300}'

# CP1 acknowledges version 2 (a real charger does it on reception): B gets its 200 kW.
curl -X POST localhost:8080/chargers/CP1/ack -H 'content-type: application/json' -d '{"version": 2}'

# A's battery is nearly full: its demand drops to 50 kW. A decrease again, so CP1 must
# acknowledge version 4 before B can take more.
curl -X PUT localhost:8080/sessions/<id-A> -H 'content-type: application/json' -d '{"demand_kw": 50}'

# A leaves; B still waits for CP1's acknowledgement, then gets its full 300 kW.
curl -X DELETE localhost:8080/sessions/<id-A>
curl -X POST localhost:8080/chargers/CP1/ack -H 'content-type: application/json' -d '{"version": 4}'

# OPS view: who is charging, at what power, what is left.
curl localhost:8080/status
```

Replace `<id-A>` with the `session_id` returned by the first request. The server logs one line
per accepted event (`docker compose logs -f sems`); this sequence reads:

```
session_started session=8c45… setpoint=300.0kW station_allocated=300.0kW pending_acks=[]
session_started session=878b… setpoint=0.0kW station_allocated=200.0kW pending_acks=["CP1"]
ack charger=CP1 version=2 station_allocated=400.0kW pending_acks=[]
demand_changed session=8c45… setpoint=50.0kW station_allocated=250.0kW pending_acks=["CP1"]
session_ended session=8c45… setpoint=0.0kW station_allocated=200.0kW pending_acks=["CP1"]
ack charger=CP1 version=4 station_allocated=300.0kW pending_acks=[]
```

Why versions 2 and 4: see [Versions](#versions). To run the whole test suite instead, see
[Run the tests](#run-the-tests).

## Station configuration

The station is described by a JSON file given at startup; nothing about a station is in the code.

```json
{
  "grid_kw": 250,
  "chargers": [
    { "id": "HPC1", "max_kw": 300,
      "connectors": [ { "id": 1, "type": "CCS" }, { "id": 2, "type": "CCS", "max_kw": 150 } ] },
    { "id": "DC50", "max_kw": 50, "connectors": [ { "id": 1, "type": "CCS" } ] },
    { "id": "AC22", "max_kw": 22,
      "connectors": [ { "id": 1, "type": "Type2" }, { "id": 2, "type": "Type2" } ] }
  ],
  "bess": { "capacity_kwh": 200, "max_kw": 150 }
}
```

| Field | Meaning |
|---|---|
| `grid_kw` | Power of the grid connection: a hard limit |
| `chargers[].id`, `max_kw` | Each charger and the power its connectors share |
| `connectors[].id` | Numbered per charger, as in OCPP |
| `connectors[].max_kw` | Optional: defaults to the charger's |
| `connectors[].type` | Optional, informational (`CCS`, `Type2`…) |
| `bess` | Optional battery: energy (`capacity_kwh`) and charge/discharge power (`max_kw`) |

**Validated at startup**: powers above 0 and at most 1 GW, at least one charger and one
connector per charger, unique ids, no connector more powerful than its charger, no unknown field
(a typo such as `max_kW` is reported). An invalid file stops the server with a clear message,
e.g. `charger A connector 1: more powerful than its charger`.

Two examples: `config/station.json` (the brief's) and `config/station_bess.json` (the one above:
heterogeneous chargers and a BESS).

```bash
SEMS_CONFIG=./config/station_bess.json docker compose up --build   # with Docker
cargo run --release -- config/station_bess.json                     # natively
```

## API

What each request means:

| Request | Sent by | In words |
|---|---|---|
| `POST /sessions` | a charger | "A vehicle just plugged into my connector N and accepts up to X kW." The server opens a session, recomputes the allocation and answers with the power this vehicle may draw (possibly 0 kW for now, see [Safe transitions](#safe-transitions-decrease-before-increase)). |
| `PUT /sessions/{id}` | a charger | "This vehicle now accepts X kW" (its battery fills up, it pauses with 0 kW, or it asks for more). The server recomputes the allocation. |
| `DELETE /sessions/{id}` | a charger (or OPS, for a stuck session) | "This vehicle left." Its power is released to the others. |
| `POST /chargers/{id}/ack` | a charger | "I have applied my setpoints of version N." Confirms a decrease, so that held increases can be granted. |
| `POST /bess` | the battery's BMS | "My state of charge is now 42 %." May switch the boost on or off, hence change the allocation. |
| `GET /chargers/{id}/setpoints` | a charger | "What may my vehicles draw?" As a WebSocket, it receives every new setpoint as it happens; as a plain GET, it reads the current ones. |
| `GET /status` | OPS | "What is the station doing right now?" Every charger, connector and session, with powers and statuses. |
| `GET /events` | OPS | "What happened, and why?" The history of accepted events, each with the allocation it produced. |

Every request that changes something (`POST`, `PUT`, `DELETE`) is an event: it is applied
atomically, recorded in `/events` and logged; the `GET` requests only read.

All powers are in kW. Errors are `{"error": "<message>"}`.

| Method | Route | Body | Success | Errors |
|---|---|---|---|---|
| `POST` | `/sessions` | `{"charger", "connector", "demand_kw"}` | `201` | 404 unknown charger/connector, 409 connector busy, 422 invalid body or demand (0 to 1,000,000 kW) |
| `PUT` | `/sessions/{id}` | `{"demand_kw"}` | `200` | 404 unknown session, 422 |
| `DELETE` | `/sessions/{id}` | — | `200` | 404 unknown session |
| `POST` | `/chargers/{id}/ack` | `{"version"}` | `200` | 404 unknown charger, 422 version never sent |
| `POST` | `/bess` | `{"soc"}` (0 to 1) | `200` | 404 station without BESS, 422 invalid SoC |
| `GET` | `/status` | — | `200` | — |
| `GET` | `/events?limit=100&session=<id>` | — | `200` | 422 invalid `limit` or `session` |
| `GET` | `/chargers/{id}/setpoints` | — | `200`, or WebSocket upgrade | 404 unknown charger |

Session events answer `{"session_id", "setpoint_kw", "target_kw", "status"}`, an acknowledgement
or a SoC report `{"status"}`. `status` is the full station state (a demo convenience: a real
charger would only see its own setpoints).

### Setpoints for chargers (push and pull)

`/chargers/{id}/setpoints` is the charger's channel. Opened as a **WebSocket**, it sends the
current setpoints, then a message **whenever one of them changes**, including when another
charger's event caused it. A plain **GET** returns the same message: a pull fallback while a
WebSocket reconnects. Messages sent by the charger on the WebSocket are ignored; closing it
releases nothing.

```json
{ "charger": "CP1", "version": 7,
  "setpoints": [ { "connector": 1, "session_id": "9f1c…", "kw": 200 },
                 { "connector": 2, "session_id": null, "kw": 0 } ] }
```

Try it from a browser console, then post events with `curl`:

```js
const ws = new WebSocket("ws://localhost:8080/chargers/CP1/setpoints");
ws.onmessage = (m) => console.log(m.data);
```

### `GET /status`

At the end of the quick start (abridged: CP1 and the free connectors left out):

```json
{ "grid_kw": 400.0, "capacity_kw": 400.0, "allocated_kw": 300.0, "available_kw": 100.0,
  "unserved_kw": 0.0, "grid_import_kw": 300.0, "pending_acks": [], "bess": null,
  "chargers": [
    { "id": "CP2", "max_kw": 300.0, "allocated_kw": 300.0, "available_kw": 0.0, "unserved_kw": 0.0,
      "version": 6, "online": false, "online_since_ms": 1792592812463,
      "connectors": [
        { "id": 1, "type": "CCS", "max_kw": 300.0, "status": "charging", "status_since_ms": 1792592812709,
          "session": { "id": "878b…", "started_at_ms": 1792592812566,
                       "demand_kw": 300.0, "setpoint_kw": 300.0, "target_kw": 300.0 } } ] } ] }
```

With a BESS, `bess` is an object:

```json
"bess": { "capacity_kwh": 200.0, "max_kw": 150.0, "soc": 0.6, "mode": "charging",
          "power_kw": 150.0, "boost_kw": 150.0, "energy_above_floor_kwh": 100.0 }
```

| Field | Level | Meaning |
|---|---|---|
| `grid_kw`, `max_kw` | station, charger, connector | Configured limits |
| `capacity_kw` | station | Power offered to vehicles: the grid plus the BESS boost |
| `allocated_kw` | station, charger | Sum of setpoints |
| `available_kw` | station, charger | Headroom: `capacity − allocated` (station), `max − allocated` (charger); not necessarily usable, another level may be saturated |
| `unserved_kw` | station, charger | Unmet demand: Σ (demand − setpoint) |
| `grid_import_kw` | station | Drawn from the grid, always between 0 and `grid_kw` |
| `pending_acks` | station | Chargers whose acknowledgement holds increases, with the version awaited |
| `bess` | station | `soc` (`null` until reported), `mode` (`charging`, `discharging`, `idle`), `power_kw`, `boost_kw`, `energy_above_floor_kwh`; `null` without BESS |
| `version` | charger | Version of its latest setpoints |
| `online`, `online_since_ms` | charger | A setpoint WebSocket is open; since when online or offline |
| `status`, `status_since_ms` | connector | See below; time of the last status change |
| `demand_kw` | session | What the vehicle accepts right now (input) |
| `setpoint_kw` | session | What the station grants and publishes (output), always ≤ `demand_kw` |
| `target_kw` | session | What the allocator would give; above `setpoint_kw` while an increase is held |

**Connector status** (names from OCPP; EV = the vehicle, EVSE = the station), derived from the
session, never stored:

| `status` | Condition | Meaning | Who holds it |
|---|---|---|---|
| `available` | no session | Free | — |
| `charging` | demand > 0, setpoint > 0 | Charging | — |
| `suspended_ev` | demand = 0 | Plugged in, the **vehicle** requests nothing (battery full, paused) | the vehicle |
| `suspended_evse` | demand > 0, setpoint = 0 | Plugged in, the **station** gives nothing yet (increase held, or no power left) | the station |

A lasting `suspended_ev` is normal (a candidate for idle fees); a lasting `suspended_evse` is
worth a look (saturated grid, or a charger blocking in `pending_acks`).

These are the only states a connector has. The other state words in `/status` describe other
things: `bess.mode` (`charging`, `discharging`, `idle`) is the battery's flow, see
[Battery storage](#battery-storage-bess); `online` is the charger's link.

### `GET /events`: history

The latest accepted events of the whole station, oldest first; `limit` (default 100) and
`session` (one session's events only) are optional. Each entry holds the event **and the
allocation that resulted from it**:

```json
{ "seq": 2, "at_ms": 1761040000000, "kind": "session_started",
  "session_id": "4b2e…", "charger": "CP1", "connector": 2, "demand_kw": 300.0,
  "result": { "allocated_kw": 300.0,
              "setpoints": [ { "session_id": "9f1c…", "kw": 150.0 }, { "session_id": "4b2e…", "kw": 150.0 } ] } }
```

`kind` is `session_started`, `demand_changed`, `session_ended`, `ack` (with `version`) or
`bess_soc` (with `soc`). *"Why did my car charge at 150 kW?"*: its entries show a second vehicle
plugged into the same charger. *"Why did B wait at 0 kW?"*: the `ack` entry that released it.

The last 10,000 events are kept, in memory (empty after a restart). `seq` never restarts, so a
gap reveals dropped entries. Rejected requests are not recorded.

## OPS view

The brief asks to let OPS see *"connector and charger availability, power availability"*. All of
it is in `GET /status`:

| The brief asks for | Where |
|---|---|
| **Connector availability** | `connectors[].status`, `status_since_ms` |
| **Charger availability** | `chargers[].online`, `online_since_ms` |
| **Power availability** | Station and charger `allocated_kw`, `available_kw`, `unserved_kw`; station `capacity_kw`, `grid_import_kw`, `bess`; session `demand_kw`, `setpoint_kw`, `target_kw` |
| What is blocking | `pending_acks` |
| What happened | `GET /events` |

A charger going offline **releases nothing**: it may still be drawing power, so its sessions
keep their setpoints and, if it owed an acknowledgement, it stays in `pending_acks`. OPS sees
both facts side by side.

## Communication: who speaks first

One question decides each channel: *who initiates the exchange?*

| Direction | Who speaks first | Channel | Messages |
|---|---|---|---|
| Charger → server | the charger | **HTTP** (REST) | session started, demand changed, session ended, **acknowledgement** |
| BMS → server | the battery | **HTTP** | state of charge |
| Server → charger | **the server** | **WebSocket** | new setpoints |

- What happens *at* a charger is initiated by the charger: a plain HTTP request, easy to test
  with `curl` and to replay in memory in the scenario runner. An acknowledgement goes from the
  charger to the server, so it is HTTP too, even though the setpoint it acknowledges came over
  the WebSocket.
- New setpoints are initiated by **the server**: when B plugs into CP2, CP1 must lower A's power
  although CP1 sent nothing. A server cannot speak first over plain HTTP; without push, CP1 would
  poll and latency would depend on the polling interval.
- In production this would converge on **a single bidirectional WebSocket per charger**, as OCPP
  does. HTTP in was kept for readability and testability.

## Safe transitions: decrease before increase

When B arrives, A must drop from 300 to 200 kW and B rise from 0 to 200 kW. If B applied its
setpoint before A applied its own, the site would briefly draw 500 kW on a 400 kW grid. The grid
limit is a hard boundary, never to be exceeded even briefly. So:

1. **Decreases are published at once.** The charger concerned is then *awaited*
   (`pending_acks`).
2. **Increases are held while any charger is awaited.** The session keeps its previous setpoint
   and `/status` shows its `target_kw`. A new vehicle stays at 0 kW (`suspended_evse`).
3. **When the last awaited charger acknowledges, all held increases are granted.** An event that
   only frees power (a departure, a lower demand on its own) grants its increases at once.

Every session thus stays at or below a setpoint that was safe before the event: whatever order
chargers receive their messages in, no charger and not the grid is ever exceeded (checked after
every request by the scenarios and the randomized test).

### Versions

Every accepted event gets a number, `seq`, that only grows (it is the event's number in
`/events`). A charger's **version** is the `seq` of the event that last changed its setpoints;
it travels with them, and the charger acknowledges it: `POST /chargers/{id}/ack {"version": n}`.

The version says *which* setpoints are confirmed. Two decreases can reach the same charger faster
than an acknowledgement travels back, and confirming the older one proves nothing about the
newer one: only the charger's **latest** version releases it (S19). A version never sent is
rejected (422).

A charger's versions are therefore not consecutive. In the quick start:

| seq | Event | Effect | CP1 | CP2 |
|---|---|---|---|---|
| 1 | A plugs into CP1 | A 0 → 300: an increase, nothing to acknowledge | **1** | — |
| 2 | B plugs into CP2 | A 300 → 200: a decrease, CP1 awaited; B appears at 0 (target 200) | **2** | **2** |
| 3 | CP1 acknowledges 2 | Nobody awaited: B 0 → 200 | 2 | **3** |
| 4 | A drops to 50 | A 200 → 50: a decrease, CP1 awaited; B's increase to 300 held | **4** | 3 |
| 5 | A leaves | Nothing new for CP1; it still owes version 4 | 4 | 3 |
| 6 | CP1 acknowledges 4 | B 200 → 300 | 4 | **6** |

A single global counter ties each version to the `/events` entry that caused it.

**Assumptions.** A charger never lets a vehicle draw above its latest setpoint: a new session
draws nothing until its first setpoint arrives (the response to `POST /sessions`, then pushes).
A setpoint received is applied at once (no ramp time modelled): an acknowledgement of reception
therefore proves the charger draws at most its new setpoint.

## Battery storage (BESS)

Optional: without a `bess` block in the configuration, nothing changes. With it, three rules,
recomputed on every event:

| Rule | Definition |
|---|---|
| **Boost** and **floor** | Power offered to vehicles = `grid + max_kw` while the state of charge (SoC) is above **10 %**, `grid` otherwise or while the SoC is unknown. The water-filling runs unchanged on that capacity. |
| **One flow at a time** | With V the vehicles' granted total: **discharging** `V − grid` if V > grid; else **charging** `min(grid − V, max_kw)` while not full (vehicles first); else **idle**. |
| **Grid connection** | The import is `V − discharge + charge`: always between 0 and the grid limit, never an overload, never an export. |

The resulting `bess.mode` in `/status` (the battery's state, not a connector's):

| `mode` | When | `power_kw` |
|---|---|---|
| `discharging` | Vehicles draw more than the grid (only possible with the boost, SoC > 10 %) | `V − grid`: the excess the battery covers |
| `charging` | Vehicles leave grid headroom and the battery is not full | `min(grid − V, max_kw)` |
| `idle` | No flow: vehicles use exactly the grid, or the battery is full, or its SoC is unknown | 0 |

The **BMS** (Battery Management System, the battery's own controller) reports the SoC:
`POST /bess {"soc": 0.42}` (0 to 1). The configuration gives the battery's size, never its
charge: until the first report, `soc` is `null` and there is no boost. A report may change the
capacity, hence the allocation, and is recorded in `/events` (kind `bess_soc`).

**Why no acknowledgement for the BESS.** Commanding its power from the server would make every
boost transition unsafe one way or the other: vehicles ramping up first overload the grid, the
BESS discharging first exports. Instead, the BESS works as station batteries do (peak shaving):
its inverter regulates the flow at the grid connection locally, discharging only what the site
draws beyond the grid and charging only with the headroom. The server only *plans* the capacity
and the mode, and vehicle setpoints keep going through the barrier. When the SoC falls below
10 %, vehicle decreases are published at once and the BESS covers them until they are applied:
that is why the floor is the software's own rule, above the BMS hardware cut-off.

**Assumptions.** The SoC is an input, never computed here (no energy integration over time). The
BESS applies its mode reliably and almost instantly. Its hardware cut-off is below 10 %.

## Run the tests

```bash
# Build (or rebuild after a code change: `run` reuses the last image)
docker compose build test
# All tests: unit, scenarios, randomized, push
docker compose run --rm test
# Scenarios one by one, with their metrics
docker compose run --rm test cargo test --test scenarios -- --nocapture
# Replay one randomized run after a failure (its seed is printed)
docker compose run --rm -e SEMS_SEED=42 test cargo test --test random
# Performance (release build: the first run takes a few minutes)
docker compose run --rm test cargo test --release --test perf -- --ignored --nocapture --test-threads=1
```

Natively (stable Rust): the same commands without `docker compose run --rm test`.

| Family | What it proves |
|---|---|
| **Unit** (`src/*.rs`) | Configuration validation, session lifecycle and statuses, allocation algorithm, malformed requests |
| **Scenarios** (`scenarios/*.json`) | 25 event sequences replayed through the API, with expected setpoints, totals, statuses and HTTP codes |
| **Randomized** (`tests/random.rs`) | Random stations (1–5 chargers, half with a BESS) and 100 random steps each, including invalid requests and acknowledgements that are late, stale, for a future version or never sent. A shadow model predicts every HTTP code. Seeded: a failure prints its seed |
| **Push** (`tests/push.rs`) | Real server, real WebSocket clients: a charger is told its new setpoint when *another* charger's event changes it, and an increase arrives only after the decreasing charger acknowledged |
| **Performance** (`tests/perf.rs`) | See [Performance](#performance); ignored by default |

After **every step** of every scenario and randomized run, four properties are checked, whatever
the expected values:

- **Safety**: no setpoint above its demand or connector, no charger or grid overload.
- **Transition**: even if chargers still applied their previous setpoints, no overload.
- **History**: an accepted request adds exactly one `/events` entry matching `/status`; a
  rejected one adds none.
- **Consistency**: `/status` totals and statuses match its sessions; the BESS follows its rules.

The randomized test also checks **no waste** (< 1 W per session) once the station has settled.
Every test family was verified against deliberate bugs.

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
| S10 | Saturated charger's share goes to the other | What a full charger cannot take goes to another charger |
| S11 | Invalid events | Clear error codes, state untouched by rejected events |
| S12 | Increase under contention | An increase is granted only within what is available |
| S13 | Cascading release | Released power across several vehicles and chargers |
| S14 | Zero demand | A full vehicle still plugged in takes no share |
| S15 | Plugged in, not charging | Status `charging` → `suspended_ev` → `charging` |
| S16 | Absurd demand values | Regression: powers above 1 GW are rejected (422) instead of overflowing |
| S17 | Heterogeneous chargers under contention | S09's mix on a 100 kW grid: the AC charger is served in full, the others share the rest |
| S18 | Increase waits for the acknowledgement | A decrease is published at once, the increase only after the acknowledgement |
| S19 | Stale acknowledgement | Acknowledging an older version releases nothing |
| S20 | Silent charger | A charger that never acknowledges freezes every increase (known limitation) |
| S21 | Pending decrease holds an unrelated increase | The barrier is station-wide |
| S22 | OPS view during a pending acknowledgement | Statuses, targets, pending chargers and totals during a partial transition |
| S23 | BESS boost | Above 10 % SoC, two vehicles get 300 kW each on a 400 kW grid; the BESS discharges the excess |
| S24 | BESS state-of-charge floor | Below 10 % the boost disappears; above 10 % again, it returns |
| S25 | BESS opportunistic charge | The BESS charges only with what vehicles leave, idles when full or while its SoC is unknown |

To add a scenario, drop a JSON file in `scenarios/`: it is picked up automatically.

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

- `config`: a path or an inline config. `why` is mandatory.
- `expect` fields, all optional: `setpoints`, `targets`, `statuses`, `pending`, `allocated_kw`,
  `available_kw`, `unserved_kw`, `http` (without it, a 2xx is required); for the BESS:
  `capacity_kw`, `grid_import_kw`, `bess_mode`, `bess_kw`.
- Acknowledgements: by default the runner plays perfect chargers that acknowledge at once (the
  intermediate state is checked first). With `"acks": "manual"`, a step can be `{ "ack": "CP1" }`
  (its latest version) or `{ "ack": { "charger": "CP1", "version": 2 } }`.
- `{ "soc": 0.42 }` reports the BESS state of charge.

## Key decisions

Full reasoning and rejected alternatives: [`JOURNAL.md`](JOURNAL.md).

- **Cascading limits** grid > charger > connector > vehicle, all enforced at once.
- **Allocation: max-min fair water-filling** (V2). Picture each session as a glass as tall as
  what it can take; power is poured into all glasses at the same level. A small glass fills up
  first and what it does not take keeps raising the others. No power is left unused while a
  vehicle could take it, and nobody can get more without taking from someone served less. The
  cascade takes two fillings:

  ```
  cap        = min(demand, connector)
  μ_charger  = fill(charger max, caps of its sessions)     ← level inside each charger
  cap'       = min(cap, μ_charger)
  λ_grid     = fill(capacity, all cap')                     ← level across the station
  setpoint   = min(cap', λ_grid)
  ```

  `fill` sorts the caps and serves in full every cap below the equal share of what is left; the
  first cap above it sets the level. O(n log n). Example (S10, grid 500 kW): A and B on CP1
  (300 kW) are capped at 150 kW each by their charger, so C on CP2 takes the remaining 200 kW.
  V1 used a capped equal share, `min(demand, connector, grid / n, charger / n_charger)`: safe,
  but up to 166.7 kW unused in our scenarios. Rejected: proportional to demand (favours big
  requests), first come first served (unfair), linear programming (oversized). Weighted max-min
  is the natural extension for priorities.
- **Integer watts internally, rounded down**: no sum can exceed a limit by rounding (at most
  n − 1 W unused).
- **Full recomputation on every event**: the "Σ ≤ grid" invariant is global, and the
  computation takes microseconds.
- **Atomicity by construction.** actix-web processes requests in parallel; the whole station
  sits behind a single `Mutex`, and each event (validation, application, reallocation, setpoint
  publication, history record) runs in one locked section; readers take the same lock. Every
  check happens before any change, so a rejected request leaves no trace. Covered by S11, the
  history property, the randomized test and the burst test P2.
- **In-memory state**, no database; the history is recorded in the same operation as the event,
  in a bounded ring buffer (`VecDeque`).
- **Decrease before increase** (V3): a station-wide barrier, all or nothing, versions taken from
  the history's `seq`: the simplest rule that keeps the grid safe during transitions.
- **HTTP in, WebSocket out**: see [Communication](#communication-who-speaks-first).
- **BESS as a planning input** (V4): the server sizes the capacity and decides the mode; the
  BESS regulates the grid connection locally.

## Assumptions and limitations

- Power is modelled in kW (real chargers are often driven in amperes) and is continuously
  divisible (internal charger "stacks" are not modelled).
- One server per station. State is lost on restart (not handled).
- Charger and BESS behaviour: see the assumptions under
  [Safe transitions](#safe-transitions-decrease-before-increase) and
  [Battery storage](#battery-storage-bess). An acknowledgement confirms reception, not a
  measured power.
- **Silent charger.** A charger that never acknowledges (failure, lost link) freezes every
  increase on the station (S20): the safe side, and OPS sees which charger blocks in
  `pending_acks`. Ending its sessions with `DELETE` does not lift the hold. Next step: a timeout
  after which the charger is reported as failed and excluded from the allocation; excluding it
  assumes it fell back to a safe setpoint on its own (as OCPP's fallback profiles do), which the
  server cannot verify.
- **The barrier is station-wide and all or nothing**: a pending decrease on one charger also
  holds unrelated increases, even one that would already fit (S21). Under heavy activity,
  increases can starve; a finer barrier would serve faster at the cost of more logic.
- A charger that disappears without ending its session keeps its power reserved (safety first);
  OPS sees it `online: false` and can end its sessions with `DELETE /sessions/{id}`.
- `online` means "reachable for setpoint push": a charger that only polls shows as offline.
  Connections are logged on stdout, not recorded in `/events`.
- **No hysteresis around the 10 % SoC floor**: a SoC oscillating around it switches the boost on
  and off.
- **No minimum viable power per session.** With many vehicles on a small grid, shares can become
  too small to be useful. Sketched rule: a configurable minimum; sessions admitted in arrival
  order while each can get it, the others wait at 0 kW as `suspended_evse`. Not needed for the
  brief's station (at least 100 kW per vehicle).

## Known risks (assumed, not solved)

- **No authentication.** A spoofed acknowledgement or SoC could affect physical safety (lift the
  barrier, trigger a boost). A real deployment needs authentication and input validation.
- **The BESS is not measured.** `grid_import_kw` is computed, not measured: a BESS drifting from
  its expected behaviour would go undetected. A meter at the grid connection should be read.
- **Stale SoC.** If the BMS stops reporting, the last SoC is kept: the station may plan a boost
  while the battery drains, and the floor gives a false sense of safety. A proper fix needs a
  watchdog timer (the system is otherwise purely event-driven); the same watchdog would
  implement the charger acknowledgement timeout.
- **A panic while holding the station lock stops the service**, deliberately. A panic means an
  invariant was broken; carrying on could grant power beyond the limits. Stopped, the server
  publishes nothing: chargers keep their last setpoints, which were safe. Inputs that could
  panic are rejected at the edge (powers above 1 GW, invalid SoC). Possible evolution: apply each
  event to a copy of the state and roll back on panic.

## Performance

Release build, measured **inside the Docker container** with the command above (Docker Desktop
VM with 4 CPUs), ranges over 3 runs. Client, server and simulated chargers share the container,
so these are upper bounds.

**P4: the brief's requirement, end to end.** What counts is the time from an event until **every
affected charger holds its final setpoint**, the whole cascade included. Simulated chargers (one
WebSocket each) acknowledge every setpoint on reception; in "cascade" cases the measure includes
the decrease, its acknowledgement round trip and the increase it releases. 200 runs per case:

| Event | Active sessions | Ends when | p50 | p99 | max |
|---|---|---|---|---|---|
| Connect, no cascade | 0 → 1 | A receives 300 kW | 2.0–2.1 ms | 3.0–5.9 ms | 3.1–16 ms |
| Connect, **cascade** | 1 → 2 | A receives 200 kW, acknowledges, then **B receives 200 kW** | 2.7–2.8 ms | 4.3–8.5 ms | 4.7–12 ms |
| Disconnect, no cascade | 1 → 0 | CP1 receives 0 kW | 1.6–1.8 ms | 2.6–3.3 ms | 2.6–24 ms |
| Disconnect, **cascade** | 2 → 1 | A leaves: **B receives 300 kW** | 1.9–2.0 ms | 3.0–8.8 ms | 3.0–13 ms |
| Demand down, **cascade** | 2 | A drops to 50 kW, acknowledges, then **B receives 300 kW** | 2.4 ms | 3.5–4.7 ms | 4.2–5.0 ms |
| Demand up, **cascade** | 2 | A asks 300 kW: B receives 200 kW, acknowledges, then **A receives 200 kW** | 2.4 ms | 3.3–4.3 ms | 3.4–31 ms |
| **Full station** | 3 → 4 | A, B (CP1) and C (CP2) drop to 100 kW, **both chargers acknowledge**, then **D receives 100 kW** | 2.7 ms | 4.4–5.2 ms | 7.1–18 ms |

Every case stays far below 1 s (the test asserts it): about 2–3 ms typically, 31 ms at worst.
Network latency to real chargers would add to these figures.

| Test | What | p50 | p99 | max |
|---|---|---|---|---|
| **P2** | 8 concurrent clients × 500 demand changes on 20 sessions: per-request latency | 1.5–2.2 ms | 6.0–7.0 ms | 9.1–20 ms |
| **P3** | Allocation alone, 4 sessions | 0.4 µs | 0.7–1.1 µs | ≤ 238 µs |
| | Allocation alone, 100 sessions | 6.8–6.9 µs | 18–31 µs | ≤ 160 µs |
| | Allocation alone, 10,000 sessions | 0.71–0.73 ms | 0.97–1.6 ms | ≤ 13 ms |

- **P2**: **2,850–3,600 events/s**, the safety invariant checked on all 4,000 responses and
  every event found in `/events`. A real station sees a few events per second: this is headroom,
  not a target.
- **Across versions**: the water-filling (a sort) is slower than V1's equal share, and the
  barrier adds a pass over the sessions; end-to-end figures barely moved. Computation is never
  the bottleneck, the network is.

## Allocation metrics

Computed by the scenario runner (`cargo test --test scenarios -- --nocapture`), worst case over
each scenario's steps. Optimum = the most power deliverable within every limit:
`min(capacity, Σ chargers min(charger, Σ sessions min(demand, connector)))`.

| Metric | Meaning |
|---|---|
| Waste (kW) | `optimum − allocated`: deliverable within every limit, but not given |
| Efficiency (%) | `allocated / optimum` |
| Fairness | Jain's index over `setpoint / demand` ratios; 1 = perfectly even |
| Churn (setpoints/event) | Setpoints changed per accepted event: how many chargers are disturbed |
| Overshoot (kW) | Grid overload if every charger applied the larger of its old and new setpoint during a transition |

**V1 → V2: no waste.** Waste drops to 0 kW and efficiency to 100 % in every scenario; the
randomized test confirms it on random stations (< 1 W per session). Scenarios that moved (full
V1 table in `JOURNAL.md`, step 6):

| Scenario | Waste (kW) | Efficiency (%) | Churn | Overshoot (kW) |
|---|---|---|---|---|
| S07 demand down then up | 100.0 → **0.0** | 71.4 → **100.0** | 1.2 → 1.8 | 100.0 → 100.0 |
| S09 heterogeneous chargers | 166.7 → **0.0** | 55.2 → **100.0** | 1.7 → 1.0 | 0.0 → 0.0 |
| S10 saturated charger's share | 33.3 → **0.0** | 93.3 → **100.0** | 1.3 → 1.3 | 0.0 → 0.0 |
| S12 increase under contention | 100.0 → **0.0** | 71.4 → **100.0** | 1.0 → 1.5 | 0.0 → 100.0 |
| S13 cascading release | 113.3 → **0.0** | 68.8 → **100.0** | 2.4 → 2.7 | 100.0 → 113.3 |
| S15 plugged in, not charging | 30.0 → **0.0** | 90.0 → **100.0** | 1.8 → 1.8 | 50.0 → 50.0 |

The cost: redistributing disturbs more chargers (churn) and moves more power between them, so
the potential transition overshoot grew.

**V2 → V3: no transition overshoot.** Overshoot is 0 kW on every transition of every scenario,
intermediate states included (it was up to 113.3 kW: S03 and S15 50 kW; S04–S07, S12, S14, S16
100 kW; S13 113.3 kW; S17 39 kW).

**The price of V3's safety**: while a charger has not acknowledged, increases wait. Scenarios
that acknowledge manually show it (S18, S20, S22: 200 kW unused while waiting; S19: 266.7 kW; S21:
150 kW), indefinitely if a charger stays silent (S20). With chargers acknowledging within
milliseconds, the settled figures (0 kW waste) are what a station sees.

**Fairness must be read with care**: Jain's index compares service *ratios*, not what each
vehicle *could* get. In S17 it is 0.74 although the split is max-min optimal: the 300 kW vehicle
gets 39 kW (13 %) on a 100 kW grid, the 22 kW one is served in full. A low index can mean
"unequal requests", not "unfair system".

## Roadmap

| Version | Content |
|---|---|
| **V1** ✅ | REST API, scenarios, Docker, WebSocket push of setpoints, `/events` history, allocation metrics, performance tests |
| **V2** ✅ | Max-min fair water-filling: no waste; randomized property test |
| **V3.1** ✅ | Safe transitions: decrease before increase, versioned acknowledgements |
| **V3.2** ✅ | OPS view: charger availability and available power per charger |
| **V4** ✅ | Battery storage (BESS): boost, 10 % SoC floor, opportunistic charging, never an export |
| Later | Timeout for a silent charger (watchdog, also for a stale SoC); hysteresis around the SoC floor; minimum viable power per session; finer acknowledgement barrier |
