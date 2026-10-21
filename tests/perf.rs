//! Performance measurements, kept apart from functional tests (slower, machine-dependent).
//!
//! Run in release mode: `cargo test --release --test perf -- --ignored --nocapture`
//!
//! P4 measures, per event type, the time until **every** affected charger has its final setpoint,
//! acknowledgement round trips included (the brief's < 1 s is about the whole cascade).
//!
//! P2 and P4 run client and server on the same machine: figures include the client's own cost
//! and are upper bounds. Every event response carries the full station state, so P2 also
//! measures that serialization.

mod common;
use awc::ws::Frame;
use common::{ack, brief, change_demand, connect, get, kw, start, subscribe};
use futures_util::future::join_all;
use futures_util::StreamExt;
use sems::allocator::{allocate, Demand};
use sems::model::{Charger, Config, Connector};
use serde_json::Value;
use std::hint::black_box;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Sorts the samples and prints p50 / p99 / max; returns max.
fn report(label: &str, mut times: Vec<Duration>) -> Duration {
    times.sort();
    let n = times.len();
    let (p50, p99, max) = (times[n / 2], times[n * 99 / 100], times[n - 1]);
    println!("{label:>16} | {p50:>10.1?} | {p99:>10.1?} | {max:>10.1?}");
    max
}

/// P2 — burst of concurrent events: throughput, per-request latency, and the safety invariant
/// checked on every response under load.
#[actix_web::test]
#[ignore]
async fn p2_burst_of_concurrent_events() {
    const CHARGERS: usize = 10;
    const CLIENTS: usize = 8;
    const EVENTS_PER_CLIENT: usize = 500;
    let chargers: Vec<String> = (1..=CHARGERS)
        .map(|i| format!(r#"{{ "id": "CP{i}", "max_kw": 300, "connectors": [ {{ "id": 1 }}, {{ "id": 2 }} ] }}"#))
        .collect();
    let config = format!(
        r#"{{ "grid_kw": 2000, "chargers": [ {} ] }}"#,
        chargers.join(",")
    );
    let addr = start(&config);

    let mut sessions = Vec::new();
    for c in 1..=CHARGERS {
        for k in 1..=2 {
            sessions.push(connect(&addr, &format!("CP{c}"), k, 300.0).await);
        }
    }

    // Each client changes the demand of its own sessions as fast as the server answers.
    let started = Instant::now();
    let clients = (0..CLIENTS).map(|client_idx| {
        let (addr, sessions) = (addr.clone(), sessions.clone());
        async move {
            let client = awc::Client::new();
            let mine: Vec<&String> = sessions.iter().skip(client_idx).step_by(CLIENTS).collect();
            let mut times = Vec::with_capacity(EVENTS_PER_CLIENT);
            for i in 0..EVENTS_PER_CLIENT {
                let demand = (50 + (i * 37 + client_idx * 11) % 300) as f64;
                let start = Instant::now();
                let (code, body) =
                    change_demand(&client, &addr, mine[i % mine.len()], demand).await;
                times.push(start.elapsed());
                assert_eq!(code, 200, "{body}");
                assert_safe(&body["status"]);
            }
            times
        }
    });
    let times: Vec<Duration> = join_all(clients).await.into_iter().flatten().collect();
    let elapsed = started.elapsed();

    let total = CLIENTS * EVENTS_PER_CLIENT;
    let (_, events) = get(format!("http://{addr}/events?limit=10000")).await;
    assert_eq!(
        events.as_array().unwrap().len(),
        total + sessions.len(),
        "every event recorded"
    );

    println!(
        "\nP2 burst: {CLIENTS} clients × {EVENTS_PER_CLIENT} events on {} sessions",
        sessions.len()
    );
    println!(
        "throughput: {:.0} events/s ({total} events in {elapsed:.2?})",
        total as f64 / elapsed.as_secs_f64()
    );
    println!("{:>16} | {:>10} | {:>10} | {:>10}", "", "p50", "p99", "max");
    report("request latency", times);
}

/// Safety invariant on an event response: no charger and no grid overload.
fn assert_safe(status: &Value) {
    let eps = 0.001;
    let mut total = 0.0;
    for charger in status["chargers"].as_array().unwrap() {
        let allocated = charger["allocated_kw"].as_f64().unwrap();
        assert!(
            allocated <= charger["max_kw"].as_f64().unwrap() + eps,
            "charger overload"
        );
        total += allocated;
    }
    assert!(
        total <= status["grid_kw"].as_f64().unwrap() + eps,
        "grid overload"
    );
}

/// A simulated charger: it acknowledges every setpoints message as soon as it receives it, as a
/// real one would, and forwards the message to the test.
fn simulated_charger(addr: &str, id: &str) -> mpsc::UnboundedReceiver<Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (addr, id) = (addr.to_string(), id.to_string());
    actix_web::rt::spawn(async move {
        let mut ws = subscribe(&addr, &id).await;
        let client = awc::Client::new();
        while let Some(Ok(Frame::Text(text))) = ws.next().await {
            let msg: Value = serde_json::from_slice(&text).unwrap();
            ack(&client, &addr, &msg).await;
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    rx
}

/// A session to plug in: (charger, connector, demand in kW).
type Plug = (&'static str, u32, f64);

/// Waits until a charger receives `want` kW on `connector` (1 s budget).
async fn until(rx: &mut mpsc::UnboundedReceiver<Value>, connector: u32, want: f64) {
    loop {
        let msg = timeout(Duration::from_secs(1), rx.recv()).await;
        let msg = msg.expect("final setpoint within 1 s").unwrap();
        if kw(&msg, connector) == want {
            return;
        }
    }
}

/// Plugs a vehicle in through a shared client (P4 makes thousands of requests: one connection
/// each would exhaust the machine's TCP ports); returns the session id.
async fn plug(
    client: &awc::Client,
    addr: &str,
    charger: &str,
    connector: u32,
    demand_kw: f64,
) -> String {
    let body =
        serde_json::json!({ "charger": charger, "connector": connector, "demand_kw": demand_kw });
    let url = format!("http://{addr}/sessions");
    let mut res = client.post(url).send_json(&body).await.unwrap();
    assert_eq!(res.status(), 201);
    let body: Value = res.json().await.unwrap();
    body["session_id"].as_str().unwrap().to_string()
}

/// Untimed: waits for every acknowledgement, then drops the messages received so far.
async fn settled(
    client: &awc::Client,
    addr: &str,
    chargers: &mut [&mut mpsc::UnboundedReceiver<Value>],
) {
    loop {
        let url = format!("http://{addr}/status");
        let status: Value = client.get(url).send().await.unwrap().json().await.unwrap();
        if status["pending_acks"].as_array().unwrap().is_empty() {
            break;
        }
        actix_web::rt::time::sleep(Duration::from_millis(1)).await;
    }
    actix_web::rt::time::sleep(Duration::from_millis(5)).await;
    for rx in chargers.iter_mut() {
        while rx.try_recv().is_ok() {}
    }
}

/// P4 — end-to-end latency per event type, from the event until every affected charger holds
/// its final setpoint. Simulated chargers acknowledge at once; "cascade" cases include the
/// decrease, its acknowledgement and the increase it releases. Station: the brief's; A on CP1,
/// B on CP2.
#[actix_web::test]
#[ignore]
async fn p4_end_to_end_cascade_latency() {
    const RUNS: usize = 200;
    let addr = start(&brief());
    let (mut cp1, mut cp2) = (
        simulated_charger(&addr, "CP1"),
        simulated_charger(&addr, "CP2"),
    );
    let client = awc::Client::new();
    let end = |id: String| {
        let url = format!("http://{addr}/sessions/{id}");
        client.delete(url).send()
    };
    let mut table = Vec::new();

    // Sessions already charging before the timed event.
    let a: Plug = ("CP1", 1, 300.0);
    let b: Plug = ("CP2", 1, 300.0);
    let cases: [(&str, &[Plug]); 7] = [
        ("connect", &[]),
        ("connect cascade", &[a]),
        ("disconnect", &[a]),
        ("disconnect casc.", &[a, b]),
        ("decrease casc.", &[a, b]),
        ("increase casc.", &[("CP1", 1, 50.0), b]),
        ("full station", &[a, ("CP1", 2, 300.0), b]),
    ];
    for (case, (label, before)) in cases.iter().enumerate() {
        let mut times = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let mut ids = Vec::new();
            for (charger, connector, kw) in before.iter() {
                ids.push(plug(&client, &addr, charger, *connector, *kw).await);
            }
            settled(&client, &addr, &mut [&mut cp1, &mut cp2]).await;

            let start = Instant::now();
            match case {
                0 => {
                    ids.push(plug(&client, &addr, "CP1", 1, 300.0).await);
                    until(&mut cp1, 1, 300.0).await;
                }
                1 => {
                    ids.push(plug(&client, &addr, "CP2", 1, 300.0).await);
                    until(&mut cp2, 1, 200.0).await;
                }
                2 => {
                    end(ids.remove(0)).await.unwrap();
                    until(&mut cp1, 1, 0.0).await;
                }
                3 => {
                    end(ids.remove(0)).await.unwrap();
                    until(&mut cp2, 1, 300.0).await;
                }
                4 => {
                    change_demand(&client, &addr, &ids[0], 50.0).await;
                    until(&mut cp2, 1, 300.0).await;
                }
                5 => {
                    change_demand(&client, &addr, &ids[0], 300.0).await;
                    until(&mut cp1, 1, 200.0).await;
                }
                // Fourth session: CP1 and CP2 both decrease and must both acknowledge before
                // D gets its 100 kW.
                _ => {
                    ids.push(plug(&client, &addr, "CP2", 2, 300.0).await);
                    until(&mut cp2, 2, 100.0).await;
                }
            }
            times.push(start.elapsed());

            for id in ids {
                end(id).await.unwrap();
            }
            settled(&client, &addr, &mut [&mut cp1, &mut cp2]).await;
        }
        table.push((*label, times));
    }

    println!("\nP4 end-to-end latency until every affected charger is set ({RUNS} runs each)");
    println!(
        "{:>16} | {:>10} | {:>10} | {:>10}",
        "event", "p50", "p99", "max"
    );
    for (label, times) in table {
        let max = report(label, times);
        assert!(
            max < Duration::from_secs(1),
            "{label}: brief requirement < 1 s"
        );
    }
}

/// P3 — allocation time vs number of sessions (2 connectors per charger, all busy).
/// Shows that the computation stays in microseconds and grows linearly.
#[test]
#[ignore]
fn p3_allocator_scaling() {
    const ITERATIONS: usize = 1_000;
    println!("\nP3 allocator scaling ({ITERATIONS} runs each)");
    println!(
        "{:>16} | {:>10} | {:>10} | {:>10}",
        "sessions", "p50", "p99", "max"
    );
    for sessions in [4, 100, 10_000] {
        let chargers = (0..sessions / 2)
            .map(|i| Charger {
                id: format!("CP{i}"),
                max_w: 300_000,
                connectors: (1..=2)
                    .map(|id| Connector {
                        id,
                        kind: None,
                        max_w: 300_000,
                    })
                    .collect(),
            })
            .collect();
        let config = Config {
            grid_w: 400_000 * sessions as u64,
            chargers,
            bess: None,
        };
        // Varied demands, including zero, so that every branch is exercised.
        let demands: Vec<Demand> = (0..sessions)
            .map(|i| Demand {
                charger: i / 2,
                connector: i % 2,
                demand_w: (i as u64 * 37_000) % 350_000,
            })
            .collect();

        let times: Vec<Duration> = (0..ITERATIONS)
            .map(|_| {
                let start = Instant::now();
                black_box(allocate(
                    black_box(&config),
                    config.grid_w,
                    black_box(&demands),
                ));
                start.elapsed()
            })
            .collect();
        let max = report(&sessions.to_string(), times);
        assert!(
            max < Duration::from_secs(1),
            "allocation must stay far below the 1 s budget"
        );
    }
}
