//! Randomized property test: random stations, random event sequences, properties checked after
//! every step. It tests what no hand-written scenario imagined (a crash on huge demands slipped
//! through all of them).
//!
//! A seeded generator makes every run reproducible: a failure prints its seed, replayed with
//! `SEMS_SEED=<seed> cargo test --test random`. 50 stations by default (a few seconds);
//! `SEMS_RUNS=1000 cargo test --test random` plays more. A shadow model (which session is on which
//! connector) predicts the HTTP code of every request.
//!
//! Acknowledgements are random steps too: late, twice, stale, for a future version, for an
//! unknown charger, or never. One station in two has a BESS whose state of charge is reported at
//! random (floor, full, anything in between, out of range; 404 without BESS). Checked after each step: expected code (never a 5xx), safety,
//! transition, history, `/status` consistency, shadow agreement, and no waste beyond rounding
//! (< 1 W per session) whenever the station is settled. Each run ends with every charger
//! acknowledging, which must settle the station with no waste. A stale acknowledgement must not
//! release increases: that rule is the server's version comparison, tested by scenario S19.

mod common;
use actix_web::{dev::ServiceResponse, test, App};
use common::checks::{
    check_consistency, check_history, check_invariant, check_transition, num, optimum_kw, EPS,
};
use sems::{api, model::Config, model::Station, model::MAX_KW};
use serde_json::{json, Value};
use std::env;
use uuid::Uuid;

/// Stations played by default; override with `SEMS_RUNS`.
const RUNS: u64 = 50;
const STEPS: usize = 100;

/// SplitMix64: tiny, fast, good enough to pick test inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// True with probability `percent` %.
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// Mostly 0–400 kW with decimals; sometimes 0, the 1 GW limit, or an invalid value.
    fn demand_kw(&mut self) -> f64 {
        match self.below(100) {
            0..8 => 0.0,
            8..12 => MAX_KW,
            12..16 => [-1.0, 1e20, MAX_KW + 1.0][self.below(3) as usize],
            _ => self.below(400_000) as f64 / 1000.0,
        }
    }
}

#[actix_web::test]
async fn random_stations_and_events() {
    let var = |name: &str| env::var(name).ok().map(|v| v.parse::<u64>().expect(name));
    let seeds: Vec<u64> = match (var("SEMS_SEED"), var("SEMS_RUNS")) {
        (Some(seed), _) => vec![seed],
        (None, runs) => (1..=runs.unwrap_or(RUNS)).collect(),
    };
    for seed in seeds {
        if let Err(e) = run(seed).await {
            panic!("seed {seed}: {e}\nreplay: SEMS_SEED={seed} cargo test --test random");
        }
    }
}

/// Random station: 1–5 chargers of 7–350 kW, 1–3 connectors (some with a lower limit),
/// grid of 10–1500 kW, and one station in two with a BESS of 50–400 kWh and 20–300 kW.
fn random_config(rng: &mut Rng) -> Value {
    let chargers: Vec<Value> = (1..=1 + rng.below(5))
        .map(|c| {
            let max_kw = 7 + rng.below(344);
            let connectors: Vec<Value> = (1..=1 + rng.below(3))
                .map(|k| match rng.chance(50) {
                    true => json!({ "id": k, "max_kw": 1 + rng.below(max_kw) }),
                    false => json!({ "id": k }),
                })
                .collect();
            json!({ "id": format!("CP{c}"), "max_kw": max_kw, "connectors": connectors })
        })
        .collect();
    let mut config = json!({ "grid_kw": 10 + rng.below(1491), "chargers": chargers });
    if rng.chance(50) {
        let bess = json!({ "capacity_kwh": 50 + rng.below(351), "max_kw": 20 + rng.below(281) });
        config["bess"] = bess;
    }
    config
}

async fn run(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let config = random_config(&mut rng);
    let station = Station::new(Config::from_json(&config.to_string())?, 0);
    let app = test::init_service(
        App::new()
            .app_data(api::Shared::new(station))
            .configure(api::configure),
    )
    .await;

    // Shadow model: session id on each connector (by config index), and ended sessions.
    let mut slots: Vec<Vec<Option<String>>> = config["chargers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| vec![None; c["connectors"].as_array().unwrap().len()])
        .collect();
    let mut ended: Vec<String> = Vec::new();
    let mut last_seq = 0;
    let get = |uri: &str| test::TestRequest::get().uri(uri).to_request();
    let mut before: Value = test::call_and_read_body_json(&app, get("/status")).await;

    for step in 1..=STEPS {
        let active: Vec<String> = slots.iter().flatten().flatten().cloned().collect();
        // A known session most of the time, else an ended or never-seen one.
        let pick_session = |rng: &mut Rng| match (rng.chance(80), active.is_empty()) {
            (true, false) => (
                active[rng.below(active.len() as u64) as usize].clone(),
                true,
            ),
            _ if !ended.is_empty() && rng.chance(50) => {
                (ended[rng.below(ended.len() as u64) as usize].clone(), false)
            }
            _ => (Uuid::new_v4().to_string(), false),
        };

        // The request, what the shadow model expects, and the history kind if accepted.
        let (description, request, expected, kind, target) = match rng.below(100) {
            0..30 => {
                let demand = rng.demand_kw();
                let c = rng.below(slots.len() as u64) as usize;
                let k = rng.below(slots[c].len() as u64) as usize;
                let unknown = rng.chance(10);
                let (charger, connector) = match (unknown, rng.chance(50)) {
                    (true, true) => ("CPX".to_string(), 1),
                    (true, false) => (format!("CP{}", c + 1), 99),
                    _ => (format!("CP{}", c + 1), k as u64 + 1),
                };
                let expected = if !(0.0..=MAX_KW).contains(&demand) {
                    422
                } else if unknown {
                    404
                } else if slots[c][k].is_some() {
                    409
                } else {
                    201
                };
                let body =
                    json!({ "charger": charger, "connector": connector, "demand_kw": demand });
                let request = test::TestRequest::post().uri("/sessions").set_json(&body);
                (
                    format!("connect {body}"),
                    request,
                    expected,
                    "session_started",
                    Some((c, k)),
                )
            }
            30..55 => {
                let (id, known) = pick_session(&mut rng);
                let demand = rng.demand_kw();
                let expected = match ((0.0..=MAX_KW).contains(&demand), known) {
                    (false, _) => 422,
                    (true, false) => 404,
                    (true, true) => 200,
                };
                let request = test::TestRequest::put()
                    .uri(&format!("/sessions/{id}"))
                    .set_json(json!({ "demand_kw": demand }));
                (
                    format!("change {id} to {demand} kW"),
                    request,
                    expected,
                    "demand_changed",
                    None,
                )
            }
            55..70 => {
                let (id, known) = pick_session(&mut rng);
                let request = test::TestRequest::delete().uri(&format!("/sessions/{id}"));
                let expected = if known { 200 } else { 404 };
                (
                    format!("disconnect {id}"),
                    request,
                    expected,
                    "session_ended",
                    None,
                )
            }
            // Acknowledgements arrive late, in any order, twice, stale, or never: mostly the
            // charger's latest version, sometimes an older one, a future one, or an unknown
            // charger.
            70..90 => {
                let c = rng.below(slots.len() as u64) as usize;
                let unknown = rng.chance(5);
                let charger = if unknown {
                    "CPX".to_string()
                } else {
                    format!("CP{}", c + 1)
                };
                let latest = before["chargers"][c]["version"].as_u64().unwrap();
                let version = match rng.below(100) {
                    0..70 => latest,
                    70..90 => rng.below(latest + 1),
                    _ => last_seq + 1 + rng.below(5),
                };
                let expected = if unknown {
                    404
                } else if version > last_seq {
                    422
                } else {
                    200
                };
                let request = test::TestRequest::post()
                    .uri(&format!("/chargers/{charger}/ack"))
                    .set_json(json!({ "version": version }));
                (
                    format!("ack {charger} v{version}"),
                    request,
                    expected,
                    "ack",
                    None,
                )
            }
            // The BMS reports a state of charge: mostly valid (floor and full included),
            // sometimes out of range; a station without BESS answers 404.
            _ => {
                let soc = match rng.below(100) {
                    0..10 => 0.10,
                    10..20 => 1.0,
                    20..90 => rng.below(1001) as f64 / 1000.0,
                    _ => [-0.1, 1.5][rng.below(2) as usize],
                };
                let expected = if config["bess"].is_null() {
                    404
                } else if (0.0..=1.0).contains(&soc) {
                    200
                } else {
                    422
                };
                let request = test::TestRequest::post()
                    .uri("/bess")
                    .set_json(json!({ "soc": soc }));
                (
                    format!("bess soc {soc}"),
                    request,
                    expected,
                    "bess_soc",
                    None,
                )
            }
        };
        let at = |e: String| format!("step {step} ({description}): {e}\nconfig: {config}");

        let res: ServiceResponse = test::call_service(&app, request.to_request()).await;
        let code = res.status().as_u16();
        let body: Value = test::read_body_json(res).await;
        if code != expected {
            return Err(at(format!("expected HTTP {expected}, got {code}: {body}")));
        }

        // Keep the shadow model in step with accepted events.
        let accepted = (code < 300).then(|| (kind, body["session_id"].clone()));
        if let Some((_, Value::String(id))) = &accepted {
            let id = id.clone();
            match kind {
                "session_started" => {
                    let (c, k) = target.unwrap();
                    slots[c][k] = Some(id);
                }
                "session_ended" => {
                    for slot in slots.iter_mut().flatten() {
                        if slot.as_deref() == Some(id.as_str()) {
                            *slot = None;
                        }
                    }
                    ended.push(id);
                }
                _ => {}
            }
        }

        let status: Value = test::call_and_read_body_json(&app, get("/status")).await;
        let latest: Vec<Value> = test::call_and_read_body_json(&app, get("/events?limit=1")).await;
        check_invariant(&status).map_err(at)?;
        check_consistency(&status).map_err(at)?;
        check_transition(&before, &status).map_err(at)?;
        check_history(&mut last_seq, accepted, &latest, &status).map_err(at)?;
        check_shadow(&status, &slots).map_err(at)?;
        if status["pending_acks"].as_array().unwrap().is_empty() {
            check_waste(&status, &slots).map_err(at)?;
        }
        before = status;
    }

    // In the end, every charger acknowledges: the station must settle with no waste.
    let at = |e: String| format!("final settling: {e}\nconfig: {config}");
    for pending in before["pending_acks"].as_array().unwrap().clone() {
        let request = test::TestRequest::post()
            .uri(&format!(
                "/chargers/{}/ack",
                pending["charger"].as_str().unwrap()
            ))
            .set_json(json!({ "version": pending["version"] }));
        let res = test::call_service(&app, request.to_request()).await;
        if res.status() != 200 {
            return Err(at(format!("ack {pending}: HTTP {}", res.status())));
        }
        let status: Value = test::call_and_read_body_json(&app, get("/status")).await;
        let latest: Vec<Value> = test::call_and_read_body_json(&app, get("/events?limit=1")).await;
        check_invariant(&status).map_err(at)?;
        check_transition(&before, &status).map_err(at)?;
        check_history(&mut last_seq, Some(("ack", Value::Null)), &latest, &status).map_err(at)?;
        before = status;
    }
    if !before["pending_acks"].as_array().unwrap().is_empty() {
        return Err(at(
            "acknowledging every pending version does not settle the station".into(),
        ));
    }
    check_waste(&before, &slots).map_err(at)?;
    Ok(())
}

/// No waste beyond rounding on a settled station: at most 1 W per session stays unallocated.
fn check_waste(status: &Value, slots: &[Vec<Option<String>>]) -> Result<(), String> {
    let sessions = slots.iter().flatten().flatten().count() as f64;
    let waste = optimum_kw(status) - num(&status["allocated_kw"]);
    match waste > sessions * 0.001 + EPS {
        true => Err(format!(
            "waste: {waste} kW left unused with {sessions} sessions"
        )),
        false => Ok(()),
    }
}

/// The server's sessions are exactly the ones the shadow model expects, on the same connectors.
fn check_shadow(status: &Value, slots: &[Vec<Option<String>>]) -> Result<(), String> {
    for (charger, expected) in status["chargers"].as_array().unwrap().iter().zip(slots) {
        for (connector, id) in charger["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .zip(expected)
        {
            if connector["session"]["id"].as_str() != id.as_deref() {
                return Err(format!(
                    "shadow: {}/{} holds {}, expected {id:?}",
                    charger["id"], connector["id"], connector["session"]["id"]
                ));
            }
        }
    }
    Ok(())
}
