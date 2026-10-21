//! Setpoint delivery to chargers (`/chargers/{id}/setpoints`), against a real server on a
//! random local port: WebSocket push, plain GET pull, unknown charger.
//! Station: the brief's (`config/station.json`), grid 400 kW, CP1 and CP2 of 300 kW.
//! Clients use `awc`, actix's own HTTP and WebSocket client.

use awc::ws::Frame;
use futures_util::{Stream, StreamExt};
use sems::{api, model::Config, model::Station};
use serde_json::{json, Value};
use std::{fmt::Debug, fs, net::TcpListener, time::Duration};
use tokio::time::timeout;

/// Starts a server with the brief's station and returns its address.
fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let json = fs::read_to_string("config/station.json").unwrap();
    let station = Station::new(Config::from_json(&json).unwrap(), api::now_ms());
    actix_web::rt::spawn(api::serve(station, listener).unwrap());
    addr
}

/// Opens a charger's WebSocket, as a real charger would.
async fn subscribe(
    addr: &str,
    charger: &str,
) -> impl Stream<Item = Result<Frame, impl Debug>> + Unpin {
    let url = format!("ws://{addr}/chargers/{charger}/setpoints");
    awc::Client::new().ws(url).connect().await.unwrap().1
}

/// Next pushed message, or `None` if nothing arrives within 200 ms.
async fn next(ws: &mut (impl Stream<Item = Result<Frame, impl Debug>> + Unpin)) -> Option<Value> {
    match timeout(Duration::from_millis(200), ws.next()).await.ok()? {
        Some(Ok(Frame::Text(text))) => Some(serde_json::from_slice(&text).unwrap()),
        other => panic!("unexpected frame: {other:?}"),
    }
}

/// Setpoint (kW) of connector `n` in a setpoints message.
fn kw(msg: &Value, n: u32) -> f64 {
    let connector = msg["setpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["connector"] == n);
    connector.unwrap()["kw"].as_f64().unwrap()
}

async fn connect(addr: &str, charger: &str, connector: u32, demand_kw: f64) {
    let body = json!({ "charger": charger, "connector": connector, "demand_kw": demand_kw });
    let res = awc::Client::new()
        .post(format!("http://{addr}/sessions"))
        .send_json(&body)
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
}

async fn get(url: String) -> (u16, Value) {
    let mut res = awc::Client::new().get(url).send().await.unwrap();
    (res.status().as_u16(), res.json().await.unwrap())
}

#[actix_web::test]
async fn other_charger_is_told_without_asking() {
    let addr = start();
    let mut cp1 = subscribe(&addr, "CP1").await;
    let mut cp2 = subscribe(&addr, "CP2").await;

    // Initial snapshot on connection: nothing plugged in.
    assert_eq!(kw(&next(&mut cp1).await.unwrap(), 1), 0.0);
    assert_eq!(kw(&next(&mut cp2).await.unwrap(), 1), 0.0);

    connect(&addr, "CP1", 1, 300.0).await;
    assert_eq!(kw(&next(&mut cp1).await.unwrap(), 1), 300.0);

    // B plugs into CP2: CP1 must lower A to 200 kW although it sent nothing.
    connect(&addr, "CP2", 1, 300.0).await;
    assert_eq!(kw(&next(&mut cp1).await.unwrap(), 1), 200.0);
    assert_eq!(kw(&next(&mut cp2).await.unwrap(), 1), 200.0);

    // Pull fallback: a plain GET on the same URL returns what was last pushed.
    let (code, pulled) = get(format!("http://{addr}/chargers/CP1/setpoints")).await;
    assert_eq!(code, 200);
    assert_eq!(kw(&pulled, 1), 200.0);
}

#[actix_web::test]
async fn no_message_when_setpoints_do_not_change() {
    let addr = start();
    let mut cp1 = subscribe(&addr, "CP1").await;
    next(&mut cp1).await.unwrap();

    // A asks for 100 kW, below its share: B's arrival on CP2 does not change it.
    connect(&addr, "CP1", 1, 100.0).await;
    assert_eq!(kw(&next(&mut cp1).await.unwrap(), 1), 100.0);
    connect(&addr, "CP2", 1, 300.0).await;
    assert_eq!(next(&mut cp1).await, None, "CP1 must not be disturbed");
}

#[actix_web::test]
async fn unknown_charger_is_404() {
    let addr = start();
    let (code, body) = get(format!("http://{addr}/chargers/CP9/setpoints")).await;
    assert_eq!(code, 404);
    assert!(body["error"].is_string());

    let url = format!("ws://{addr}/chargers/CP9/setpoints");
    let ws = awc::Client::new().ws(url).connect().await;
    assert!(ws.is_err(), "WebSocket handshake must be refused");
}
