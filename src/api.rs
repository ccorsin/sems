//! HTTP API: chargers report session events, OPS queries the station state.
//!
//! The model works in watts; this layer converts to and from kW and maps errors to HTTP codes.
//! Every event response embeds the full station state: a convenience for demos and tests
//! (a real charger should only see its own setpoints).
//!
//! Rule: no I/O under the station lock. Responses and log lines are built while holding it,
//! then written after it is released.

use crate::model::{kw_to_w, EventError, Station, Status, Watts};
use actix_web::{error::InternalError, http::StatusCode, web, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Station shared by all server workers.
pub type AppState = web::Data<Mutex<Station>>;

/// Registers the routes. The caller provides the [`AppState`] through `App::app_data`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    // Malformed or incomplete JSON bodies get the same `{"error": …}` shape as other errors.
    let json = web::JsonConfig::default().error_handler(|err, _| {
        InternalError::from_response("", error(StatusCode::UNPROCESSABLE_ENTITY, err)).into()
    });
    cfg.app_data(json)
        .route("/sessions", web::post().to(start_session))
        .route("/sessions/{id}", web::put().to(change_demand))
        .route("/sessions/{id}", web::delete().to(end_session))
        .route("/status", web::get().to(status));
}

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct StartSession {
    charger: String,
    connector: u32,
    demand_kw: f64,
}

#[derive(Deserialize)]
struct ChangeDemand {
    demand_kw: f64,
}

async fn start_session(state: AppState, body: web::Json<StartSession>) -> HttpResponse {
    let Some(demand_w) = kw_to_w(body.demand_kw) else {
        return invalid_demand();
    };
    run_event(&state, StatusCode::CREATED, "session_started", |st, now| {
        st.connect(&body.charger, body.connector, demand_w, now)
    })
}

async fn change_demand(
    state: AppState,
    id: web::Path<String>,
    body: web::Json<ChangeDemand>,
) -> HttpResponse {
    let Ok(id) = Uuid::parse_str(&id) else {
        return event_error(EventError::UnknownSession);
    };
    let Some(demand_w) = kw_to_w(body.demand_kw) else {
        return invalid_demand();
    };
    run_event(&state, StatusCode::OK, "demand_changed", |st, now| {
        st.change_demand(id, demand_w, now).map(|()| id)
    })
}

async fn end_session(state: AppState, id: web::Path<String>) -> HttpResponse {
    let Ok(id) = Uuid::parse_str(&id) else {
        return event_error(EventError::UnknownSession);
    };
    run_event(&state, StatusCode::OK, "session_ended", |st, now| {
        st.disconnect(id, now).map(|()| id)
    })
}

async fn status(state: AppState) -> HttpResponse {
    let view = StatusView::of(&lock(&state));
    HttpResponse::Ok().json(view)
}

/// Applies an event under the lock, then logs it and answers with the session's setpoint
/// and the full station state.
fn run_event(
    state: &AppState,
    code: StatusCode,
    kind: &str,
    event: impl FnOnce(&mut Station, u64) -> Result<Uuid, EventError>,
) -> HttpResponse {
    let response = {
        let mut st = lock(state);
        let id = match event(&mut st, now_ms()) {
            Ok(id) => id,
            Err(e) => return event_error(e),
        };
        let setpoint_w = st
            .sessions()
            .find(|(_, _, s)| s.id == id)
            .map_or(0, |(_, _, s)| s.setpoint_w);
        EventResponse {
            session_id: id,
            setpoint_kw: kw(setpoint_w),
            status: StatusView::of(&st),
        }
    };
    println!(
        "{kind} session={} setpoint={}kW station_allocated={}kW",
        response.session_id, response.setpoint_kw, response.status.allocated_kw
    );
    HttpResponse::build(code).json(response)
}

fn lock(state: &AppState) -> std::sync::MutexGuard<'_, Station> {
    // A poisoned lock means a panic mid-update: the state may be inconsistent, so fail loudly.
    state.lock().expect("station lock poisoned")
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn error(code: StatusCode, message: impl ToString) -> HttpResponse {
    HttpResponse::build(code).json(json!({ "error": message.to_string() }))
}

fn invalid_demand() -> HttpResponse {
    error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "demand_kw must be a finite number >= 0",
    )
}

fn event_error(e: EventError) -> HttpResponse {
    match e {
        EventError::UnknownConnector => {
            error(StatusCode::NOT_FOUND, "unknown charger or connector")
        }
        EventError::ConnectorBusy => error(StatusCode::CONFLICT, "connector already has a session"),
        EventError::UnknownSession => error(StatusCode::NOT_FOUND, "unknown session"),
    }
}

// ---------------------------------------------------------------------------------------------
// Response bodies (kW)
// ---------------------------------------------------------------------------------------------

fn kw(w: Watts) -> f64 {
    w as f64 / 1000.0
}

#[derive(Serialize)]
struct EventResponse {
    session_id: Uuid,
    setpoint_kw: f64,
    status: StatusView,
}

/// Full station state, as seen by OPS.
#[derive(Serialize)]
struct StatusView {
    grid_kw: f64,
    /// Sum of setpoints.
    allocated_kw: f64,
    /// Grid headroom; not necessarily usable (a charger may be saturated).
    available_kw: f64,
    /// Demand not served: Σ (demand − setpoint).
    unserved_kw: f64,
    chargers: Vec<ChargerView>,
}

#[derive(Serialize)]
struct ChargerView {
    id: String,
    max_kw: f64,
    allocated_kw: f64,
    unserved_kw: f64,
    connectors: Vec<ConnectorView>,
}

#[derive(Serialize)]
struct ConnectorView {
    id: u32,
    #[serde(rename = "type")]
    kind: Option<String>,
    max_kw: f64,
    status: Status,
    status_since_ms: u64,
    session: Option<SessionView>,
}

#[derive(Serialize)]
struct SessionView {
    id: Uuid,
    started_at_ms: u64,
    demand_kw: f64,
    setpoint_kw: f64,
}

impl StatusView {
    fn of(st: &Station) -> Self {
        let allocated_w: Watts = st.sessions().map(|(_, _, s)| s.setpoint_w).sum();
        let unserved_w: Watts = st
            .sessions()
            .map(|(_, _, s)| s.demand_w - s.setpoint_w)
            .sum();
        let chargers = st
            .config
            .chargers
            .iter()
            .zip(&st.slots)
            .map(|(charger, slots)| {
                let sessions = || slots.iter().filter_map(|slot| slot.session.as_ref());
                ChargerView {
                    id: charger.id.clone(),
                    max_kw: kw(charger.max_w),
                    allocated_kw: kw(sessions().map(|s| s.setpoint_w).sum()),
                    unserved_kw: kw(sessions().map(|s| s.demand_w - s.setpoint_w).sum()),
                    connectors: charger
                        .connectors
                        .iter()
                        .zip(slots)
                        .map(|(connector, slot)| ConnectorView {
                            id: connector.id,
                            kind: connector.kind.clone(),
                            max_kw: kw(connector.max_w),
                            status: slot.status(),
                            status_since_ms: slot.status_since_ms,
                            session: slot.session.as_ref().map(|s| SessionView {
                                id: s.id,
                                started_at_ms: s.started_at_ms,
                                demand_kw: kw(s.demand_w),
                                setpoint_kw: kw(s.setpoint_w),
                            }),
                        })
                        .collect(),
                }
            })
            .collect();
        StatusView {
            grid_kw: kw(st.config.grid_w),
            allocated_kw: kw(allocated_w),
            available_kw: kw(st.config.grid_w - allocated_w),
            unserved_kw: kw(unserved_w),
            chargers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Config;
    use actix_web::{test, App};

    // Error paths not expressible as scenario steps (the runner always sends well-formed requests).
    #[actix_web::test]
    async fn malformed_requests_get_json_errors() {
        let cfg = Config::from_json(
            r#"{ "grid_kw": 10, "chargers": [ { "id": "A", "max_kw": 10, "connectors": [ { "id": 1 } ] } ] }"#,
        );
        let state = web::Data::new(Mutex::new(Station::new(cfg.unwrap(), 0)));
        let app = test::init_service(App::new().app_data(state).configure(configure)).await;

        let cases = [
            (
                test::TestRequest::post()
                    .uri("/sessions")
                    .set_payload("{ not json")
                    .insert_header(("content-type", "application/json")),
                422,
            ),
            (
                test::TestRequest::post()
                    .uri("/sessions")
                    .set_json(json!({ "charger": "A" })),
                422,
            ),
            (
                test::TestRequest::post()
                    .uri("/sessions")
                    .set_json(json!({ "charger": "A", "connector": 1, "demand_kw": "x" })),
                422,
            ),
            (
                test::TestRequest::put()
                    .uri("/sessions/not-a-uuid")
                    .set_json(json!({ "demand_kw": 1 })),
                404,
            ),
            (test::TestRequest::delete().uri("/sessions/not-a-uuid"), 404),
        ];
        for (req, code) in cases {
            let res = test::call_service(&app, req.to_request()).await;
            assert_eq!(res.status().as_u16(), code);
            let body: serde_json::Value = test::read_body_json(res).await;
            assert!(body["error"].is_string(), "{body}");
        }
    }
}
