//! Performance measurements, kept apart from functional tests (slower, machine-dependent).
//!
//! Run in release mode: `cargo test --release --test perf -- --ignored --nocapture`
//!
//! P1 and P2 run client and server on the same machine: figures include the client's own cost
//! and are upper bounds. Every event response carries the full station state, so P2 also
//! measures that serialization.

mod common;
use common::{brief, change_demand, connect, get, kw, next, next_within, settle, start, subscribe};
use futures_util::future::join_all;
use sems::allocator::{allocate, Demand};
use sems::model::{Charger, Config, Connector};
use serde_json::Value;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Sorts the samples and prints p50 / p99 / max; returns max.
fn report(label: &str, mut times: Vec<Duration>) -> Duration {
    times.sort();
    let n = times.len();
    let (p50, p99, max) = (times[n / 2], times[n * 99 / 100], times[n - 1]);
    println!("{label:>16} | {p50:>10.1?} | {p99:>10.1?} | {max:>10.1?}");
    max
}

/// P1 — the brief's requirement: an event on one charger changes another charger's setpoint;
/// time from sending the event to that other charger receiving it over its WebSocket.
#[actix_web::test]
#[ignore]
async fn p1_event_to_push_latency() {
    const RUNS: usize = 200;
    let addr = start(&brief());
    let mut cp1 = subscribe(&addr, "CP1").await;
    next(&mut cp1).await.unwrap();
    connect(&addr, "CP1", 1, 300.0).await;
    next(&mut cp1).await.unwrap();
    let b = connect(&addr, "CP2", 1, 300.0).await;
    assert_eq!(kw(&next(&mut cp1).await.unwrap(), 1), 200.0);

    // Untimed: B drops to 0 kW and the chargers settle, so A is back to 300 kW. Timed: B asks
    // for 300 kW again and A's decrease to 200 kW reaches CP1. A decrease is published at once;
    // an increase waits for acknowledgements (decrease before increase), hence this direction.
    let client = awc::Client::new();
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        settle(&addr).await;
        change_demand(&client, &addr, &b, 0.0).await;
        settle(&addr).await;
        assert_eq!(kw(&next(&mut cp1).await.unwrap(), 1), 300.0);

        let start = Instant::now();
        let (code, _) = change_demand(&client, &addr, &b, 300.0).await;
        let pushed = next_within(&mut cp1, Duration::from_secs(1)).await;
        times.push(start.elapsed());
        assert_eq!(code, 200);
        assert_eq!(kw(&pushed.expect("push within 1 s"), 1), 200.0);
    }

    println!("\nP1 event → push to another charger ({RUNS} runs)");
    println!("{:>16} | {:>10} | {:>10} | {:>10}", "", "p50", "p99", "max");
    let max = report("latency", times);
    assert!(max < Duration::from_secs(1), "brief requirement: < 1 s");
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
                black_box(allocate(black_box(&config), black_box(&demands)));
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
