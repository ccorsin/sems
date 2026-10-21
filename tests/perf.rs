//! Performance measurements, kept apart from functional tests (slower, machine-dependent).
//!
//! Run in release mode: `cargo test --release --test perf -- --ignored --nocapture`

use sems::allocator::{allocate, Demand};
use sems::model::{Charger, Config, Connector};
use std::hint::black_box;
use std::time::{Duration, Instant};

const ITERATIONS: usize = 1_000;

/// P3 — allocation time vs number of sessions (2 connectors per charger, all busy).
/// Shows that the computation stays in microseconds and grows linearly.
#[test]
#[ignore]
fn p3_allocator_scaling() {
    println!("\nP3 allocator scaling ({ITERATIONS} runs each)");
    println!(
        "{:>8} | {:>10} | {:>10} | {:>10}",
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

        let mut times: Vec<Duration> = (0..ITERATIONS)
            .map(|_| {
                let start = Instant::now();
                black_box(allocate(black_box(&config), black_box(&demands)));
                start.elapsed()
            })
            .collect();
        times.sort();
        let (p50, p99, max) = (
            times[ITERATIONS / 2],
            times[ITERATIONS * 99 / 100],
            times[ITERATIONS - 1],
        );
        println!("{sessions:>8} | {p50:>10.1?} | {p99:>10.1?} | {max:>10.1?}");
        assert!(
            max < Duration::from_secs(1),
            "allocation must stay far below the 1 s budget"
        );
    }
}
