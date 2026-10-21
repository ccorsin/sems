//! Helpers shared by the integration tests: real server and clients (`push.rs`, `perf.rs`),
//! and the properties checked after every step ([`checks`]: `scenarios.rs`, `random.rs`).
#![allow(dead_code)] // Each test crate uses a subset.

pub mod checks;

use awc::ws::Frame;
use futures_util::{Stream, StreamExt};
use sems::{api, model::Config, model::Station};
use serde_json::{json, Value};
use std::{fmt::Debug, fs, net::TcpListener, time::Duration};
use tokio::time::timeout;

/// The brief's station: grid 400 kW, CP1 and CP2 of 300 kW, 2 connectors each.
pub fn brief() -> String {
    fs::read_to_string("config/station.json").unwrap()
}

/// Starts a server for this station config on a random local port; returns its address.
pub fn start(config_json: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let station = Station::new(Config::from_json(config_json).unwrap(), api::now_ms());
    actix_web::rt::spawn(api::serve(station, listener).unwrap());
    addr
}

/// Opens a charger's WebSocket, as a real charger would.
pub async fn subscribe(
    addr: &str,
    charger: &str,
) -> impl Stream<Item = Result<Frame, impl Debug>> + Unpin {
    let url = format!("ws://{addr}/chargers/{charger}/setpoints");
    awc::Client::new().ws(url).connect().await.unwrap().1
}

/// Next pushed message, or `None` if nothing arrives within `wait`.
pub async fn next_within(
    ws: &mut (impl Stream<Item = Result<Frame, impl Debug>> + Unpin),
    wait: Duration,
) -> Option<Value> {
    match timeout(wait, ws.next()).await.ok()? {
        Some(Ok(Frame::Text(text))) => Some(serde_json::from_slice(&text).unwrap()),
        other => panic!("unexpected frame: {other:?}"),
    }
}

/// Next pushed message, or `None` if nothing arrives within 200 ms.
pub async fn next(
    ws: &mut (impl Stream<Item = Result<Frame, impl Debug>> + Unpin),
) -> Option<Value> {
    next_within(ws, Duration::from_millis(200)).await
}

/// Setpoint (kW) of connector `n` in a setpoints message.
pub fn kw(msg: &Value, n: u32) -> f64 {
    let connector = msg["setpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["connector"] == n);
    connector.unwrap()["kw"].as_f64().unwrap()
}

/// Plugs a vehicle in (expects 201); returns the session id.
pub async fn connect(addr: &str, charger: &str, connector: u32, demand_kw: f64) -> String {
    let body = json!({ "charger": charger, "connector": connector, "demand_kw": demand_kw });
    let mut res = awc::Client::new()
        .post(format!("http://{addr}/sessions"))
        .send_json(&body)
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
    let body: Value = res.json().await.unwrap();
    body["session_id"].as_str().unwrap().to_string()
}

/// Changes a session's demand; returns the HTTP code and body.
pub async fn change_demand(
    client: &awc::Client,
    addr: &str,
    id: &str,
    demand_kw: f64,
) -> (u16, Value) {
    let mut res = client
        .put(format!("http://{addr}/sessions/{id}"))
        .send_json(&json!({ "demand_kw": demand_kw }))
        .await
        .unwrap();
    (
        res.status().as_u16(),
        res.json().limit(1 << 20).await.unwrap(),
    )
}

/// A charger acknowledges the version of a setpoints message it received (expects 200).
pub async fn ack(client: &awc::Client, addr: &str, msg: &Value) {
    let charger = msg["charger"].as_str().unwrap();
    let res = client
        .post(format!("http://{addr}/chargers/{charger}/ack"))
        .send_json(&json!({ "version": msg["version"] }))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

/// Every awaited charger acknowledges its pending version, as instant chargers would.
pub async fn settle(addr: &str) {
    let client = awc::Client::new();
    let (_, status) = get(format!("http://{addr}/status")).await;
    for pending in status["pending_acks"].as_array().unwrap() {
        ack(&client, addr, pending).await;
    }
}

pub async fn get(url: String) -> (u16, Value) {
    let mut res = awc::Client::new().get(url).send().await.unwrap();
    (
        res.status().as_u16(),
        res.json().limit(16 << 20).await.unwrap(),
    )
}
