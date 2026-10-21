//! Scenario runner: replays every `scenarios/*.json` file against the API (in memory).
//!
//! A scenario is a config plus ordered steps. Each step sends at most one event, then checks
//! its optional expectations against `GET /status`. After **every** step two invariants are
//! checked too, without being written in the scenario files:
//! - safety: no setpoint above its demand or connector, no charger or grid overload;
//! - history: an accepted event adds exactly one `/events` entry (right kind, right session,
//!   resulting allocation equal to `/status`); a rejected event adds none.
//!
//! Sessions are named by aliases (`"as": "A"`) since the server generates the UUIDs.
//! An unknown alias maps to a random UUID, to exercise the "unknown session" path.

use actix_web::{test, App};
use sems::{api, model::Config, model::Station};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, fs};
use uuid::Uuid;

/// Tolerance when comparing kW values: 1 W.
const EPS: f64 = 0.001;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    /// Mandatory: what behaviour or edge case this scenario validates.
    #[allow(dead_code)]
    why: String,
    /// Inline config, or a path to a config file (relative to the crate root).
    config: Value,
    steps: Vec<Step>,
}

/// One step: at most one event, then expectations.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Step {
    connect: Option<Connect>,
    change: Option<Change>,
    /// Alias of the session to end.
    disconnect: Option<String>,
    #[serde(default)]
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    #[serde(rename = "as")]
    alias: String,
    charger: String,
    connector: u32,
    demand_kw: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    session: String,
    demand_kw: f64,
}

/// Every field is optional: a scenario only checks what matters to it.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// HTTP code of the event; when absent, any 2xx is required.
    http: Option<u16>,
    /// Setpoint (kW) per session alias.
    setpoints: Option<HashMap<String, f64>>,
    /// Connector status per session alias (the connector the alias was plugged into).
    statuses: Option<HashMap<String, String>>,
    allocated_kw: Option<f64>,
    available_kw: Option<f64>,
    unserved_kw: Option<f64>,
}

#[actix_web::test]
async fn scenarios() {
    let mut paths: Vec<_> = fs::read_dir("scenarios")
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no scenario found");

    let mut failures = Vec::new();
    for path in paths {
        let scenario: Scenario = serde_json::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        // Announced first, so that the server log lines appear under their scenario.
        println!("── {}", scenario.name);
        match run(&scenario).await {
            Ok(()) => println!("   ok"),
            Err(e) => {
                println!("   FAIL: {e}");
                failures.push(format!("{}: {e}", scenario.name));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenario(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

async fn run(scenario: &Scenario) -> Result<(), String> {
    let config_json = match &scenario.config {
        Value::String(path) => fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?,
        inline => inline.to_string(),
    };
    let station = Station::new(Config::from_json(&config_json)?, 0);
    let app = test::init_service(
        App::new()
            .app_data(api::Shared::new(station))
            .configure(api::configure),
    )
    .await;

    // Alias → (session id, charger, connector).
    let mut sessions: HashMap<String, (Uuid, String, u32)> = HashMap::new();
    let id_of = |sessions: &HashMap<_, (Uuid, _, _)>, alias: &str| {
        sessions.get(alias).map_or_else(Uuid::new_v4, |s| s.0)
    };

    let mut last_seq = 0;
    for (i, step) in scenario.steps.iter().enumerate() {
        let at = |e: String| format!("step {}: {e}", i + 1);

        // The request, and the history `kind` it produces if accepted.
        let request = match (&step.connect, &step.change, &step.disconnect) {
            (Some(c), None, None) => Some((
                test::TestRequest::post().uri("/sessions").set_json(json!({
                    "charger": c.charger, "connector": c.connector, "demand_kw": c.demand_kw
                })),
                "session_started",
            )),
            (None, Some(c), None) => Some((
                test::TestRequest::put()
                    .uri(&format!("/sessions/{}", id_of(&sessions, &c.session)))
                    .set_json(json!({ "demand_kw": c.demand_kw })),
                "demand_changed",
            )),
            (None, None, Some(alias)) => Some((
                test::TestRequest::delete().uri(&format!("/sessions/{}", id_of(&sessions, alias))),
                "session_ended",
            )),
            (None, None, None) => None,
            _ => return Err(at("a step holds at most one event".into())),
        };

        // Kind and session id of the accepted event, if any.
        let mut accepted = None;
        if let Some((request, kind)) = request {
            let res = test::call_service(&app, request.to_request()).await;
            let code = res.status();
            let body: Value = test::read_body_json(res).await;
            match step.expect.http {
                Some(expected) if code.as_u16() != expected => {
                    return Err(at(format!("expected HTTP {expected}, got {code}: {body}")))
                }
                None if !code.is_success() => {
                    return Err(at(format!("unexpected HTTP {code}: {body}")))
                }
                _ => {}
            }
            if let (Some(c), true) = (&step.connect, code.is_success()) {
                let id = body["session_id"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or(at("no session_id".into()))?;
                sessions.insert(c.alias.clone(), (id, c.charger.clone(), c.connector));
            }
            if code.is_success() {
                accepted = Some((kind, body["session_id"].clone()));
            }
        }

        let status: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/status").to_request(),
        )
        .await;
        let latest: Vec<Value> = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/events?limit=1").to_request(),
        )
        .await;
        check_invariant(&status).map_err(at)?;
        check_history(&mut last_seq, accepted, &latest, &status).map_err(at)?;
        check_expect(&step.expect, &status, &sessions).map_err(at)?;
    }
    Ok(())
}

/// History invariant: an accepted event adds exactly one entry, of the right kind and session,
/// whose resulting allocation matches `/status`; a rejected event (or none) adds nothing.
fn check_history(
    last_seq: &mut u64,
    accepted: Option<(&str, Value)>,
    latest: &[Value],
    status: &Value,
) -> Result<(), String> {
    let seq = latest.first().map_or(0, |e| e["seq"].as_u64().unwrap_or(0));
    let Some((kind, session_id)) = accepted else {
        return match seq == *last_seq {
            true => Ok(()),
            false => Err(format!(
                "history: no accepted event, yet seq went {last_seq} → {seq}"
            )),
        };
    };
    if seq != *last_seq + 1 {
        return Err(format!(
            "history: expected seq {}, got {seq}",
            *last_seq + 1
        ));
    }
    let entry = &latest[0];
    if entry["kind"] != kind || entry["session_id"] != session_id {
        return Err(format!(
            "history: expected {kind} for {session_id}, got {entry}"
        ));
    }
    let (recorded, live) = (
        num(&entry["result"]["allocated_kw"]),
        num(&status["allocated_kw"]),
    );
    if (recorded - live).abs() > EPS {
        return Err(format!(
            "history: result {recorded} kW differs from /status {live} kW"
        ));
    }
    *last_seq = seq;
    Ok(())
}

/// Safety invariant, checked on the API output after every step.
fn check_invariant(status: &Value) -> Result<(), String> {
    let mut station_total = 0.0;
    for charger in status["chargers"].as_array().unwrap() {
        let mut charger_total = 0.0;
        for connector in charger["connectors"].as_array().unwrap() {
            let session = &connector["session"];
            if session.is_null() {
                continue;
            }
            let (setpoint, demand) = (num(&session["setpoint_kw"]), num(&session["demand_kw"]));
            if setpoint > demand + EPS || setpoint > num(&connector["max_kw"]) + EPS {
                return Err(format!(
                    "invariant: setpoint {setpoint} kW above demand or connector limit"
                ));
            }
            charger_total += setpoint;
        }
        if charger_total > num(&charger["max_kw"]) + EPS {
            return Err(format!(
                "invariant: charger {} overloaded ({charger_total} kW)",
                charger["id"]
            ));
        }
        station_total += charger_total;
    }
    if station_total > num(&status["grid_kw"]) + EPS {
        return Err(format!("invariant: grid overloaded ({station_total} kW)"));
    }
    Ok(())
}

fn check_expect(
    expect: &Expect,
    status: &Value,
    sessions: &HashMap<String, (Uuid, String, u32)>,
) -> Result<(), String> {
    let close = |name: &str, got: f64, want: f64| {
        if (got - want).abs() <= EPS {
            Ok(())
        } else {
            Err(format!("{name}: expected {want}, got {got}"))
        }
    };
    for (field, want) in [
        ("allocated_kw", expect.allocated_kw),
        ("available_kw", expect.available_kw),
        ("unserved_kw", expect.unserved_kw),
    ] {
        if let Some(want) = want {
            close(field, num(&status[field]), want)?;
        }
    }

    let connector_of = |alias: &str| -> Result<&Value, String> {
        let (_, charger, connector) = sessions
            .get(alias)
            .ok_or(format!("unknown alias {alias}"))?;
        status["chargers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == charger.as_str())
            .and_then(|c| {
                c["connectors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|k| k["id"] == *connector)
            })
            .ok_or(format!("connector of {alias} not found"))
    };
    for (alias, want) in expect.setpoints.iter().flatten() {
        let connector = connector_of(alias)?;
        if connector["session"]["id"].as_str() != Some(sessions[alias].0.to_string().as_str()) {
            return Err(format!("session {alias} is not active"));
        }
        close(
            &format!("setpoint of {alias}"),
            num(&connector["session"]["setpoint_kw"]),
            *want,
        )?;
    }
    for (alias, want) in expect.statuses.iter().flatten() {
        let got = &connector_of(alias)?["status"];
        if got != want.as_str() {
            return Err(format!("status of {alias}: expected {want}, got {got}"));
        }
    }
    Ok(())
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}
