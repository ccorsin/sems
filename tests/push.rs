//! Setpoint delivery to chargers (`/chargers/{id}/setpoints`), against a real server on a
//! random local port: WebSocket push, plain GET pull, unknown charger.
//! Station: the brief's, grid 400 kW, CP1 and CP2 of 300 kW.
//! Clients use `awc`, actix's own HTTP and WebSocket client.

mod common;
use common::{brief, connect, get, kw, next, start, subscribe};

#[actix_web::test]
async fn other_charger_is_told_without_asking() {
    let addr = start(&brief());
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
    let addr = start(&brief());
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
    let addr = start(&brief());
    let (code, body) = get(format!("http://{addr}/chargers/CP9/setpoints")).await;
    assert_eq!(code, 404);
    assert!(body["error"].is_string());

    let url = format!("ws://{addr}/chargers/CP9/setpoints");
    let ws = awc::Client::new().ws(url).connect().await;
    assert!(ws.is_err(), "WebSocket handshake must be refused");
}
