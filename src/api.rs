//! HTTP API: chargers report session events and receive their setpoints, OPS queries the
//! station state.
//!
//! The model works in watts; this layer converts to and from kW and maps errors to HTTP codes.
//! Every event response embeds the full station state: a convenience for demos and tests
//! (a real charger should only see its own setpoints).
//!
//! Channels follow who speaks first. Whatever a charger initiates — session events and
//! acknowledgements — is an HTTP request. Setpoints are initiated by the server (a charger must
//! be told when *another* charger's event changes its share), and plain HTTP cannot speak
//! first: they are pushed over a WebSocket, on `/chargers/{id}/setpoints` (a plain GET on the
//! same URL returns them, as a pull fallback). Each charger has a `watch` channel holding its
//! latest setpoints: a slow client only ever gets the newest value, never a backlog.
//!
//! Every pushed message carries a `version` (the `seq` of the event that last changed it). A
//! charger acknowledges it with `POST /chargers/{id}/ack`; until then, increases are held
//! station-wide ("decrease before increase", see `Station::grant`).
//!
//! Rule: no I/O under the station lock. Responses and log lines are built while holding it,
//! then written after it is released. Publishing to the `watch` channels is in-memory and done
//! under the lock, so concurrent events are published in the order they were applied.

use crate::model::{kw_to_w, EventError, EventKind, Record, Station, Status, Watts, MAX_KW};
use actix_web::{
    dev::Server, error::InternalError, http::header, http::StatusCode, web, App, HttpRequest,
    HttpResponse, HttpServer,
};
use actix_ws::Message;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::TcpListener;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use uuid::Uuid;

/// State shared by all server workers.
pub struct Shared {
    station: Mutex<Station>,
    /// One channel per charger, in config order, holding its latest setpoints.
    push: Vec<watch::Sender<ChargerSetpoints>>,
}

/// Handle on the shared state, as received by handlers.
pub type AppState = web::Data<Shared>;

impl Shared {
    pub fn new(station: Station) -> AppState {
        let push = (0..station.config.chargers.len())
            .map(|c| watch::channel(ChargerSetpoints::of(&station, c)).0)
            .collect();
        web::Data::new(Shared {
            station: Mutex::new(station),
            push,
        })
    }
}

/// Starts the HTTP server on an already bound listener (port 0 in tests).
pub fn serve(station: Station, listener: TcpListener) -> std::io::Result<Server> {
    let state = Shared::new(station);
    let server = HttpServer::new(move || App::new().app_data(state.clone()).configure(configure));
    Ok(server.listen(listener)?.run())
}

/// Registers the routes. The caller provides the [`AppState`] through `App::app_data`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    // Malformed bodies or query strings get the same `{"error": …}` shape as other errors.
    let json = web::JsonConfig::default().error_handler(|err, _| {
        InternalError::from_response("", error(StatusCode::UNPROCESSABLE_ENTITY, err)).into()
    });
    let query = web::QueryConfig::default().error_handler(|err, _| {
        InternalError::from_response("", error(StatusCode::UNPROCESSABLE_ENTITY, err)).into()
    });
    cfg.app_data(json)
        .app_data(query)
        .route("/sessions", web::post().to(start_session))
        .route("/sessions/{id}", web::put().to(change_demand))
        .route("/sessions/{id}", web::delete().to(end_session))
        .route("/status", web::get().to(status))
        .route("/events", web::get().to(events))
        .route("/chargers/{id}/setpoints", web::get().to(setpoints))
        .route("/chargers/{id}/ack", web::post().to(ack));
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
            .map(Some)
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
        st.change_demand(id, demand_w, now).map(|()| Some(id))
    })
}

async fn end_session(state: AppState, id: web::Path<String>) -> HttpResponse {
    let Ok(id) = Uuid::parse_str(&id) else {
        return event_error(EventError::UnknownSession);
    };
    run_event(&state, StatusCode::OK, "session_ended", |st, now| {
        st.disconnect(id, now).map(|()| Some(id))
    })
}

#[derive(Deserialize)]
struct Ack {
    version: u64,
}

/// A charger acknowledges the setpoints it received, up to `version`.
async fn ack(state: AppState, id: web::Path<String>, body: web::Json<Ack>) -> HttpResponse {
    let label = format!("ack charger={id} version={}", body.version);
    run_event(&state, StatusCode::OK, &label, |st, now| {
        st.ack(&id, body.version, now).map(|()| None)
    })
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
    session: Option<String>,
}

/// History: the latest `limit` matching events (default 100), oldest first.
async fn events(state: AppState, query: web::Query<EventsQuery>) -> HttpResponse {
    let session = match query.session.as_deref().map(Uuid::parse_str) {
        Some(Err(_)) => return error(StatusCode::UNPROCESSABLE_ENTITY, "session must be a UUID"),
        parsed => parsed.and_then(Result::ok),
    };
    let limit = query.limit.unwrap_or(100);
    let mut entries: Vec<EventView> = {
        let st = lock(&state);
        st.history
            .iter()
            .rev()
            .filter(|r| session.is_none_or(|id| r.session.is_some_and(|(s, _, _)| s == id)))
            .take(limit)
            .map(|r| EventView::of(&st, r))
            .collect()
    };
    entries.reverse();
    HttpResponse::Ok().json(entries)
}

async fn status(state: AppState) -> HttpResponse {
    let view = StatusView::of(&lock(&state));
    HttpResponse::Ok().json(view)
}

/// A charger's setpoints: JSON snapshot on a plain GET (pull), stream of changes on a
/// WebSocket upgrade (push). Reads the `watch` channel, never the station lock.
async fn setpoints(
    state: AppState,
    id: web::Path<String>,
    req: HttpRequest,
    body: web::Payload,
) -> actix_web::Result<HttpResponse> {
    let Some(c) = state.push.iter().position(|tx| tx.borrow().charger == *id) else {
        return Ok(error(StatusCode::NOT_FOUND, "unknown charger"));
    };
    let tx = &state.push[c];
    let upgrade = req.headers().get(header::UPGRADE);
    if !upgrade.is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket")) {
        return Ok(HttpResponse::Ok().json(&*tx.borrow()));
    }

    let (response, session, messages) = actix_ws::handle(&req, body)?;
    let rx = tx.subscribe();
    let charger = id.into_inner();
    let state = state.clone();
    lock(&state).set_link(c, true, now_ms());
    actix_web::rt::spawn(async move {
        println!("ws_connected charger={charger}");
        push_setpoints(session, messages, rx).await;
        // Offline for OPS, but the sessions keep their power: a charger out of reach may still
        // be delivering (safety first).
        lock(&state).set_link(c, false, now_ms());
        println!("ws_disconnected charger={charger}");
    });
    Ok(response)
}

/// Sends the current setpoints, then one message per change, until the client goes away.
async fn push_setpoints(
    mut session: actix_ws::Session,
    mut messages: actix_ws::MessageStream,
    mut rx: watch::Receiver<ChargerSetpoints>,
) {
    loop {
        let text = serde_json::to_string(&*rx.borrow_and_update()).unwrap_or_default();
        if session.text(text).await.is_err() {
            return;
        }
        // Wait for the next change, answering pings meanwhile. Charger messages are ignored:
        // events go through the REST routes.
        loop {
            tokio::select! {
                changed = rx.changed() => match changed {
                    Ok(()) => break,
                    Err(_) => return,
                },
                incoming = messages.recv() => match incoming {
                    Some(Ok(Message::Ping(bytes))) => {
                        if session.pong(&bytes).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
}

/// Applies an event under the lock and publishes the chargers' new setpoints, then logs it
/// and answers with the session's setpoints (session events only) and the full station state.
fn run_event(
    state: &AppState,
    code: StatusCode,
    label: &str,
    event: impl FnOnce(&mut Station, u64) -> Result<Option<Uuid>, EventError>,
) -> HttpResponse {
    let response = {
        let mut st = lock(state);
        let id = match event(&mut st, now_ms()) {
            Ok(id) => id,
            Err(e) => return event_error(e),
        };
        // Only chargers whose setpoints actually changed are notified.
        for (c, tx) in state.push.iter().enumerate() {
            let new = ChargerSetpoints::of(&st, c);
            tx.send_if_modified(|current| {
                let changed = *current != new;
                if changed {
                    *current = new;
                }
                changed
            });
        }
        // An ended session reports 0 kW.
        let session = id.map(|id| {
            let s = st
                .sessions()
                .find(|(_, _, s)| s.id == id)
                .map(|(_, _, s)| s);
            (
                id,
                s.map_or(0, |s| s.setpoint_w),
                s.map_or(0, |s| s.target_w),
            )
        });
        EventResponse {
            session_id: session.map(|s| s.0),
            setpoint_kw: session.map(|s| kw(s.1)),
            target_kw: session.map(|s| kw(s.2)),
            status: StatusView::of(&st),
        }
    };
    // Logs are for humans: 0.1 kW. The API keeps watt precision.
    let session = match (response.session_id, response.setpoint_kw) {
        (Some(id), Some(kw)) => format!(" session={id} setpoint={kw:.1}kW"),
        _ => String::new(),
    };
    let pending: Vec<&str> = response
        .status
        .pending_acks
        .iter()
        .map(|p| p.charger.as_str())
        .collect();
    println!(
        "{label}{session} station_allocated={:.1}kW pending_acks={pending:?}",
        response.status.allocated_kw
    );
    HttpResponse::build(code).json(response)
}

fn lock(state: &AppState) -> MutexGuard<'_, Station> {
    // A poisoned lock means a panic mid-update: the state may be inconsistent, so fail loudly.
    state.station.lock().expect("station lock poisoned")
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn error(code: StatusCode, message: impl ToString) -> HttpResponse {
    HttpResponse::build(code).json(json!({ "error": message.to_string() }))
}

fn invalid_demand() -> HttpResponse {
    let message = format!("demand_kw must be a number between 0 and {MAX_KW}");
    error(StatusCode::UNPROCESSABLE_ENTITY, message)
}

fn event_error(e: EventError) -> HttpResponse {
    match e {
        EventError::UnknownConnector => {
            error(StatusCode::NOT_FOUND, "unknown charger or connector")
        }
        EventError::ConnectorBusy => error(StatusCode::CONFLICT, "connector already has a session"),
        EventError::UnknownSession => error(StatusCode::NOT_FOUND, "unknown session"),
        EventError::UnknownCharger => error(StatusCode::NOT_FOUND, "unknown charger"),
        EventError::FutureVersion => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "this version was never sent",
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Response bodies (kW)
// ---------------------------------------------------------------------------------------------

fn kw(w: Watts) -> f64 {
    w as f64 / 1000.0
}

/// History entry: the event, then the allocation that resulted from it.
/// Session fields are `null` for an `ack`; `version` only appears for an `ack`.
#[derive(Serialize)]
struct EventView {
    seq: u64,
    at_ms: u64,
    kind: EventKind,
    session_id: Option<Uuid>,
    charger: String,
    connector: Option<u32>,
    demand_kw: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    result: ResultView,
}

#[derive(Serialize)]
struct ResultView {
    allocated_kw: f64,
    setpoints: Vec<SessionSetpoint>,
}

#[derive(Serialize)]
struct SessionSetpoint {
    session_id: Uuid,
    kw: f64,
}

impl EventView {
    fn of(st: &Station, r: &Record) -> Self {
        let charger = &st.config.chargers[r.charger];
        EventView {
            seq: r.seq,
            at_ms: r.at_ms,
            kind: r.kind,
            session_id: r.session.map(|(id, _, _)| id),
            charger: charger.id.clone(),
            connector: r.session.map(|(_, k, _)| charger.connectors[k].id),
            demand_kw: r.session.map(|(_, _, w)| kw(w)),
            version: r.version,
            result: ResultView {
                allocated_kw: kw(r.setpoints.iter().map(|(_, w)| w).sum()),
                setpoints: r
                    .setpoints
                    .iter()
                    .map(|&(session_id, w)| SessionSetpoint {
                        session_id,
                        kw: kw(w),
                    })
                    .collect(),
            },
        }
    }
}

/// What a charger receives: the setpoint of each of its connectors (0 kW when free), and the
/// `version` to acknowledge.
#[derive(Serialize, Clone, PartialEq)]
struct ChargerSetpoints {
    charger: String,
    version: u64,
    setpoints: Vec<ConnectorSetpoint>,
}

#[derive(Serialize, Clone, PartialEq)]
struct ConnectorSetpoint {
    connector: u32,
    session_id: Option<Uuid>,
    kw: f64,
}

impl ChargerSetpoints {
    fn of(st: &Station, charger: usize) -> Self {
        let config = &st.config.chargers[charger];
        let setpoints = config
            .connectors
            .iter()
            .zip(&st.slots[charger])
            .map(|(connector, slot)| ConnectorSetpoint {
                connector: connector.id,
                session_id: slot.session.as_ref().map(|s| s.id),
                kw: kw(slot.session.as_ref().map_or(0, |s| s.setpoint_w)),
            })
            .collect();
        ChargerSetpoints {
            charger: config.id.clone(),
            version: st.links[charger].version,
            setpoints,
        }
    }
}

/// Answer to an event. Session fields are absent for an `ack`.
#[derive(Serialize)]
struct EventResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<Uuid>,
    /// Granted, possibly below the target while an increase waits for acknowledgements.
    #[serde(skip_serializing_if = "Option::is_none")]
    setpoint_kw: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_kw: Option<f64>,
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
    /// Chargers whose acknowledgement is awaited: while not empty, increases are held.
    pending_acks: Vec<PendingAck>,
    chargers: Vec<ChargerView>,
}

#[derive(Serialize)]
struct PendingAck {
    charger: String,
    /// The version to acknowledge; when it was sent is in `/events` (its `seq`).
    version: u64,
}

#[derive(Serialize)]
struct ChargerView {
    id: String,
    max_kw: f64,
    allocated_kw: f64,
    /// Charger headroom; not necessarily usable (the grid may be saturated).
    available_kw: f64,
    unserved_kw: f64,
    /// Version of its latest setpoints.
    version: u64,
    /// At least one setpoint WebSocket is open (a pull-only charger shows as offline).
    online: bool,
    /// When it last went online or offline.
    online_since_ms: u64,
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
    /// Granted and published to the charger.
    setpoint_kw: f64,
    /// What the allocator would give; above `setpoint_kw` while an increase is held.
    target_kw: f64,
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
            .zip(&st.links)
            .map(|((charger, slots), link)| {
                let sessions = || slots.iter().filter_map(|slot| slot.session.as_ref());
                let allocated_w: Watts = sessions().map(|s| s.setpoint_w).sum();
                ChargerView {
                    id: charger.id.clone(),
                    max_kw: kw(charger.max_w),
                    allocated_kw: kw(allocated_w),
                    available_kw: kw(charger.max_w - allocated_w),
                    unserved_kw: kw(sessions().map(|s| s.demand_w - s.setpoint_w).sum()),
                    version: link.version,
                    online: link.online(),
                    online_since_ms: link.online_since_ms,
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
                                target_kw: kw(s.target_w),
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
            pending_acks: st
                .awaited()
                .map(|c| PendingAck {
                    charger: st.config.chargers[c].id.clone(),
                    version: st.links[c].version,
                })
                .collect(),
            chargers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Config;
    use actix_web::test;

    // Error paths not expressible as scenario steps (the runner always sends well-formed requests).
    #[actix_web::test]
    async fn malformed_requests_get_json_errors() {
        let cfg = Config::from_json(
            r#"{ "grid_kw": 10, "chargers": [ { "id": "A", "max_kw": 10, "connectors": [ { "id": 1 } ] } ] }"#,
        );
        let state = Shared::new(Station::new(cfg.unwrap(), 0));
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
            (test::TestRequest::get().uri("/events?session=nope"), 422),
            (test::TestRequest::get().uri("/events?limit=-1"), 422),
        ];
        for (req, code) in cases {
            let res = test::call_service(&app, req.to_request()).await;
            assert_eq!(res.status().as_u16(), code);
            let body: serde_json::Value = test::read_body_json(res).await;
            assert!(body["error"].is_string(), "{body}");
        }
    }

    #[actix_web::test]
    async fn events_limit_and_session_filter() {
        let cfg = Config::from_json(
            r#"{ "grid_kw": 10, "chargers": [ { "id": "A", "max_kw": 10, "connectors": [ { "id": 1 }, { "id": 2 } ] } ] }"#,
        );
        let state = Shared::new(Station::new(cfg.unwrap(), 0));
        let app = test::init_service(App::new().app_data(state).configure(configure)).await;
        let get = |uri: &str| test::TestRequest::get().uri(uri).to_request();
        let post = |connector: u32| {
            test::TestRequest::post()
                .uri("/sessions")
                .set_json(json!({ "charger": "A", "connector": connector, "demand_kw": 4 }))
                .to_request()
        };

        let a: serde_json::Value = test::call_and_read_body_json(&app, post(1)).await;
        test::call_service(&app, post(2)).await;
        let all: Vec<serde_json::Value> = test::call_and_read_body_json(&app, get("/events")).await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["kind"], "session_started");
        assert_eq!(all[1]["result"]["allocated_kw"], 8.0);

        let last: Vec<serde_json::Value> =
            test::call_and_read_body_json(&app, get("/events?limit=1")).await;
        assert_eq!(last.len(), 1);
        assert_eq!(last[0]["seq"], 2, "limit keeps the most recent");

        let uri = format!("/events?session={}", a["session_id"].as_str().unwrap());
        let only_a: Vec<serde_json::Value> = test::call_and_read_body_json(&app, get(&uri)).await;
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0]["session_id"], a["session_id"]);
    }
}
