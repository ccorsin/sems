//! Properties checked on the API output after every step, by the scenario runner and the
//! randomized test. They hold whatever the allocation policy: no expected value is hard-coded.

use serde_json::Value;

/// Tolerance when comparing kW values: 1 W.
pub const EPS: f64 = 0.001;

pub fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

/// Safety invariant: no setpoint above its demand or connector, no charger or grid overload.
pub fn check_invariant(status: &Value) -> Result<(), String> {
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

/// History invariant: an accepted event adds exactly one entry, of the right kind and session,
/// whose resulting allocation matches `/status`; a rejected event (or none) adds nothing.
pub fn check_history(
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

/// `/status` is self-consistent: totals match the sessions, statuses match demand and setpoint.
pub fn check_consistency(status: &Value) -> Result<(), String> {
    let close = |what: &str, got: f64, want: f64| match (got - want).abs() <= EPS {
        true => Ok(()),
        false => Err(format!("consistency: {what} is {got}, expected {want}")),
    };
    let (mut allocated, mut unserved) = (0.0, 0.0);
    for charger in status["chargers"].as_array().unwrap() {
        let (mut charger_allocated, mut charger_unserved) = (0.0, 0.0);
        for connector in charger["connectors"].as_array().unwrap() {
            let session = &connector["session"];
            let expected = if session.is_null() {
                "available"
            } else if num(&session["demand_kw"]) == 0.0 {
                "suspended_ev"
            } else if num(&session["setpoint_kw"]) == 0.0 {
                "suspended_evse"
            } else {
                "charging"
            };
            if connector["status"] != expected {
                return Err(format!(
                    "consistency: status {} where {expected} was expected: {connector}",
                    connector["status"]
                ));
            }
            if !session.is_null() {
                charger_allocated += num(&session["setpoint_kw"]);
                charger_unserved += num(&session["demand_kw"]) - num(&session["setpoint_kw"]);
            }
        }
        close(
            "charger allocated_kw",
            num(&charger["allocated_kw"]),
            charger_allocated,
        )?;
        close(
            "charger unserved_kw",
            num(&charger["unserved_kw"]),
            charger_unserved,
        )?;
        allocated += charger_allocated;
        unserved += charger_unserved;
    }
    close("allocated_kw", num(&status["allocated_kw"]), allocated)?;
    close("unserved_kw", num(&status["unserved_kw"]), unserved)?;
    let grid = num(&status["grid_kw"]);
    close(
        "available_kw",
        num(&status["available_kw"]),
        grid - allocated,
    )
}

/// The most power deliverable within every limit (kW):
/// `min(grid, Σ chargers min(charger, Σ sessions min(demand, connector)))`.
pub fn optimum_kw(status: &Value) -> f64 {
    let chargers = status["chargers"].as_array().unwrap();
    let deliverable: f64 = chargers
        .iter()
        .map(|charger| {
            let sessions: f64 = charger["connectors"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|k| !k["session"].is_null())
                .map(|k| num(&k["session"]["demand_kw"]).min(num(&k["max_kw"])))
                .sum();
            sessions.min(num(&charger["max_kw"]))
        })
        .sum();
    deliverable.min(num(&status["grid_kw"]))
}
