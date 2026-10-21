//! Scenario runner: replays every `scenarios/*.json` file against the API (in memory).
//!
//! A scenario is a config plus ordered steps. Each step sends at most one request (a session
//! event or an acknowledgement), then checks its optional expectations against `GET /status`.
//! After **every** request, these properties are checked too, without being written in the
//! scenario files:
//! - safety: no setpoint above its demand or connector, no charger or grid overload;
//! - transition: even if chargers still applied their previous setpoints, no overload;
//! - history: an accepted request adds exactly one `/events` entry (right kind, right session,
//!   resulting allocation equal to `/status`); a rejected one adds none;
//! - consistency: `/status` totals and statuses match its sessions.
//!
//! Acknowledgements: by default (`"acks": "auto"`) the runner plays perfect chargers that
//! acknowledge at once, after checking the intermediate state; expectations then describe the
//! settled station. With `"acks": "manual"`, acknowledgements are explicit `ack` steps.
//!
//! Sessions are named by aliases (`"as": "A"`) since the server generates the UUIDs.
//! An unknown alias maps to a random UUID, to exercise the "unknown session" path.
//!
//! The runner also measures allocation-quality metrics (waste, efficiency, satisfaction,
//! fairness, churn, potential transient overshoot), worst case per scenario, and prints a
//! summary table: the baseline to compare versions of the algorithm against.

mod common;
use actix_web::{test, App};
use common::checks::{
    check_consistency, check_history, check_invariant, check_transition, num, optimum_kw,
    setpoints_of, EPS,
};
use sems::{api, model::Config, model::Station};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, fs};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    /// Mandatory: what behaviour or edge case this scenario validates.
    #[allow(dead_code)]
    why: String,
    /// Inline config, or a path to a config file (relative to the crate root).
    config: Value,
    /// `"auto"` (default): chargers acknowledge at once; `"manual"`: explicit `ack` steps.
    acks: Option<String>,
    steps: Vec<Step>,
}

/// One step: at most one request, then expectations.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Step {
    connect: Option<Connect>,
    change: Option<Change>,
    /// Alias of the session to end.
    disconnect: Option<String>,
    ack: Option<Ack>,
    /// BESS state of charge reported by its BMS (0..=1).
    soc: Option<f64>,
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

/// `"CP1"` acknowledges CP1's latest version; `{"charger": "CP1", "version": 2}` a given one.
#[derive(Deserialize)]
#[serde(untagged)]
enum Ack {
    Latest(String),
    Exact { charger: String, version: u64 },
}

/// Every field is optional: a scenario only checks what matters to it.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// HTTP code of the request; when absent, any 2xx is required.
    http: Option<u16>,
    /// Granted setpoint (kW) per session alias.
    setpoints: Option<HashMap<String, f64>>,
    /// Target (kW) per session alias: what the allocator would give.
    targets: Option<HashMap<String, f64>>,
    /// Connector status per session alias (the connector the alias was plugged into).
    statuses: Option<HashMap<String, String>>,
    /// Chargers whose acknowledgement is awaited.
    pending: Option<Vec<String>>,
    allocated_kw: Option<f64>,
    available_kw: Option<f64>,
    unserved_kw: Option<f64>,
    capacity_kw: Option<f64>,
    grid_import_kw: Option<f64>,
    /// `charging`, `discharging` or `idle`.
    bess_mode: Option<String>,
    bess_kw: Option<f64>,
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
    let mut table = Vec::new();
    for path in paths {
        let scenario: Scenario = serde_json::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        // Announced first, so that the server log lines appear under their scenario.
        println!("── {}", scenario.name);
        match run(&scenario).await {
            Ok(m) => {
                println!("   ok");
                println!("   {}", m.line());
                table.push(format!("| {} | {} |", scenario.name, m.row()));
            }
            Err(e) => {
                println!("   FAIL: {e}");
                failures.push(format!("{}: {e}", scenario.name));
            }
        }
    }
    println!("\n{}", Metrics::HEADER);
    println!("|---|---|---|---|---|---|---|");
    println!("{}", table.join("\n"));
    assert!(
        failures.is_empty(),
        "{} scenario(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A request to send, with the history `kind` it produces if accepted.
type Request = (test::TestRequest, &'static str);

fn ack_request(charger: &str, version: u64) -> Request {
    let uri = format!("/chargers/{charger}/ack");
    let request = test::TestRequest::post()
        .uri(&uri)
        .set_json(json!({ "version": version }));
    (request, "ack")
}

async fn run(scenario: &Scenario) -> Result<Metrics, String> {
    let config_json = match &scenario.config {
        Value::String(path) => fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?,
        inline => inline.to_string(),
    };
    let auto_acks = match scenario.acks.as_deref() {
        None | Some("auto") => true,
        Some("manual") => false,
        Some(other) => {
            return Err(format!(
                "acks must be \"auto\" or \"manual\", not {other:?}"
            ))
        }
    };
    let station = Station::new(Config::from_json(&config_json)?, 0);
    let app = test::init_service(
        App::new()
            .app_data(api::Shared::new(station))
            .configure(api::configure),
    )
    .await;
    let get = |uri: &str| test::TestRequest::get().uri(uri).to_request();

    // Alias → (session id, charger, connector).
    let mut sessions: HashMap<String, (Uuid, String, u32)> = HashMap::new();
    let id_of = |sessions: &HashMap<_, (Uuid, _, _)>, alias: &str| {
        sessions.get(alias).map_or_else(Uuid::new_v4, |s| s.0)
    };

    let mut last_seq = 0;
    let mut metrics = Metrics::default();
    let mut status: Value = test::call_and_read_body_json(&app, get("/status")).await;
    let mut previous = setpoints_of(&status); // Setpoints after the previous step.
    for (i, step) in scenario.steps.iter().enumerate() {
        let at = |e: String| format!("step {}: {e}", i + 1);

        let soc = step.soc.map(|soc| -> Request {
            let request = test::TestRequest::post().uri("/bess");
            (request.set_json(json!({ "soc": soc })), "bess_soc")
        });
        let request: Option<Request> = match (
            &step.connect,
            &step.change,
            &step.disconnect,
            &step.ack,
            soc,
        ) {
            (Some(c), None, None, None, None) => Some((
                test::TestRequest::post().uri("/sessions").set_json(json!({
                    "charger": c.charger, "connector": c.connector, "demand_kw": c.demand_kw
                })),
                "session_started",
            )),
            (None, Some(c), None, None, None) => Some((
                test::TestRequest::put()
                    .uri(&format!("/sessions/{}", id_of(&sessions, &c.session)))
                    .set_json(json!({ "demand_kw": c.demand_kw })),
                "demand_changed",
            )),
            (None, None, Some(alias), None, None) => Some((
                test::TestRequest::delete().uri(&format!("/sessions/{}", id_of(&sessions, alias))),
                "session_ended",
            )),
            (None, None, None, Some(Ack::Exact { charger, version }), None) => {
                Some(ack_request(charger, *version))
            }
            (None, None, None, Some(Ack::Latest(charger)), None) => {
                let version = status["chargers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["id"] == charger.as_str())
                    .map_or(0, |c| c["version"].as_u64().unwrap_or(0));
                Some(ack_request(charger, version))
            }
            (None, None, None, None, Some(soc)) => Some(soc),
            (None, None, None, None, None) => None,
            _ => return Err(at("a step holds at most one request".into())),
        };

        // The step's request first, then (auto mode) the acknowledgements it calls for. Every
        // request is followed by the full set of checks, so intermediate states are verified.
        // `own` marks the step's request, as opposed to an acknowledgement added by auto mode.
        let mut queue: Vec<(Option<Request>, bool)> = vec![(request, true)];
        let mut is_event = false;
        let mut rounds = 0;
        while let Some((item, own)) = queue.pop() {
            rounds += 1;
            if rounds > 100 {
                return Err(at("acknowledgements never settle the station".into()));
            }
            let mut accepted = None;
            if let Some((request, kind)) = item {
                let res = test::call_service(&app, request.to_request()).await;
                let code = res.status();
                let body: Value = test::read_body_json(res).await;
                let expected = if own { step.expect.http } else { Some(200) };
                match expected {
                    Some(expected) if code.as_u16() != expected => {
                        return Err(at(format!("expected HTTP {expected}, got {code}: {body}")))
                    }
                    None if !code.is_success() => {
                        return Err(at(format!("unexpected HTTP {code}: {body}")))
                    }
                    _ => {}
                }
                if let (Some(c), true, true) = (&step.connect, own, code.is_success()) {
                    let id = body["session_id"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or(at("no session_id".into()))?;
                    sessions.insert(c.alias.clone(), (id, c.charger.clone(), c.connector));
                }
                if code.is_success() {
                    accepted = Some((kind, body["session_id"].clone()));
                    is_event |= own;
                }
            }

            let before = status;
            status = test::call_and_read_body_json(&app, get("/status")).await;
            let latest: Vec<Value> =
                test::call_and_read_body_json(&app, get("/events?limit=1")).await;
            check_invariant(&status).map_err(at)?;
            check_consistency(&status).map_err(at)?;
            let excess = check_transition(&before, &status).map_err(at)?;
            metrics.overshoot_kw = metrics.overshoot_kw.max(excess);
            let was_accepted = accepted.is_some();
            check_history(&mut last_seq, accepted, &latest, &status).map_err(at)?;

            if auto_acks && was_accepted && queue.is_empty() {
                for pending in status["pending_acks"].as_array().unwrap() {
                    let charger = pending["charger"].as_str().unwrap();
                    let version = pending["version"].as_u64().unwrap();
                    queue.push((Some(ack_request(charger, version)), false));
                }
            }
        }

        check_expect(&step.expect, &status, &sessions).map_err(at)?;
        previous = metrics.observe(&status, &previous, is_event);
    }
    Ok(metrics)
}

// ---------------------------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------------------------

/// Allocation quality of one scenario: worst case over its steps (churn: average per event).
struct Metrics {
    /// Power that could have been delivered within every limit but was not (kW, max).
    waste_kw: f64,
    /// `allocated / optimum` (%, min).
    efficiency: f64,
    /// `allocated / Σ demand` (%, min).
    satisfaction: f64,
    /// Jain's index over `setpoint / demand` ratios, 1 = perfectly even (min).
    fairness: f64,
    /// Setpoints changed, summed over accepted events.
    churn: usize,
    events: usize,
    /// Worst grid excess if every charger applied the larger of its old and new setpoint,
    /// over every transition, intermediate ones included (kW, max). Since V3: always 0.
    overshoot_kw: f64,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            waste_kw: 0.0,
            efficiency: 100.0,
            satisfaction: 100.0,
            fairness: 1.0,
            churn: 0,
            events: 0,
            overshoot_kw: 0.0,
        }
    }
}

impl Metrics {
    const HEADER: &str = "| Scenario | Waste max (kW) | Efficiency min (%) | Satisfaction min (%) \
        | Fairness min | Churn avg (setpoints/event) | Overshoot max (kW) |";

    /// Folds in the state after a step; returns its setpoints (per charger/connector) for the
    /// next step's comparison.
    fn observe(
        &mut self,
        status: &Value,
        previous: &HashMap<String, f64>,
        is_event: bool,
    ) -> HashMap<String, f64> {
        let allocated = num(&status["allocated_kw"]);
        let optimum = optimum_kw(status);
        let (mut demand, mut ratios) = (0.0, Vec::new());
        let mut setpoints = HashMap::new();
        for charger in status["chargers"].as_array().unwrap() {
            for connector in charger["connectors"].as_array().unwrap() {
                let session = &connector["session"];
                let key = format!("{}/{}", charger["id"], connector["id"]);
                setpoints.insert(key, num(&session["setpoint_kw"]).max(0.0));
                if session.is_null() {
                    continue;
                }
                let d = num(&session["demand_kw"]);
                demand += d;
                if d > 0.0 {
                    ratios.push(num(&session["setpoint_kw"]) / d);
                }
            }
        }

        let pct = |part: f64, whole: f64| {
            if whole > 0.0 {
                100.0 * part / whole
            } else {
                100.0
            }
        };
        let (sum, squares): (f64, f64) = (ratios.iter().sum(), ratios.iter().map(|r| r * r).sum());
        let fairness = if squares > 0.0 {
            sum * sum / (ratios.len() as f64 * squares)
        } else {
            1.0
        };

        self.waste_kw = self.waste_kw.max(optimum - allocated);
        self.efficiency = self.efficiency.min(pct(allocated, optimum));
        self.satisfaction = self.satisfaction.min(pct(allocated, demand));
        self.fairness = self.fairness.min(fairness);
        if is_event {
            let old = |key: &String| previous.get(key).copied().unwrap_or(0.0);
            let changed = setpoints
                .iter()
                .filter(|(k, &new)| (new - old(k)).abs() > EPS);
            self.churn += changed.count();
            self.events += 1;
        }
        setpoints
    }

    fn churn_avg(&self) -> f64 {
        if self.events == 0 {
            0.0
        } else {
            self.churn as f64 / self.events as f64
        }
    }

    fn line(&self) -> String {
        format!(
            "waste ≤ {:.1} kW · efficiency ≥ {:.1} % · satisfaction ≥ {:.1} % · fairness ≥ {:.2} \
             · churn {:.1} setpoints/event · overshoot ≤ {:.1} kW",
            self.waste_kw,
            self.efficiency,
            self.satisfaction,
            self.fairness,
            self.churn_avg(),
            self.overshoot_kw
        )
    }

    fn row(&self) -> String {
        format!(
            "{:.1} | {:.1} | {:.1} | {:.2} | {:.1} | {:.1}",
            self.waste_kw,
            self.efficiency,
            self.satisfaction,
            self.fairness,
            self.churn_avg(),
            self.overshoot_kw
        )
    }
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
        ("capacity_kw", expect.capacity_kw),
        ("grid_import_kw", expect.grid_import_kw),
    ] {
        if let Some(want) = want {
            close(field, num(&status[field]), want)?;
        }
    }
    if let Some(want) = expect.bess_kw {
        close("bess power", num(&status["bess"]["power_kw"]), want)?;
    }
    if let Some(want) = &expect.bess_mode {
        let got = &status["bess"]["mode"];
        if got != want.as_str() {
            return Err(format!("bess mode: expected {want}, got {got}"));
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
    for (field, expected) in [("setpoint", &expect.setpoints), ("target", &expect.targets)] {
        for (alias, want) in expected.iter().flatten() {
            let connector = connector_of(alias)?;
            let id = sessions[alias].0.to_string();
            if connector["session"]["id"].as_str() != Some(id.as_str()) {
                return Err(format!("session {alias} is not active"));
            }
            let got = num(&connector["session"][format!("{field}_kw")]);
            close(&format!("{field} of {alias}"), got, *want)?;
        }
    }
    if let Some(want) = &expect.pending {
        let got: Vec<&str> = status["pending_acks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["charger"].as_str().unwrap())
            .collect();
        if got != *want {
            return Err(format!("pending acks: expected {want:?}, got {got:?}"));
        }
    }
    for (alias, want) in expect.statuses.iter().flatten() {
        let got = &connector_of(alias)?["status"];
        if got != want.as_str() {
            return Err(format!("status of {alias}: expected {want}, got {got}"));
        }
    }
    Ok(())
}
