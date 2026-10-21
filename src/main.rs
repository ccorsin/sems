//! Entry point: `sems <config.json>`.
//!
//! Increment 1 only loads and validates the station configuration; the HTTP server comes next.

use sems::model::Config;
use std::{env, fs, process::ExitCode};

fn main() -> ExitCode {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: sems <config.json>");
        return ExitCode::FAILURE;
    };
    let config = match fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|json| Config::from_json(&json))
    {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let connectors: usize = config.chargers.iter().map(|c| c.connectors.len()).sum();
    println!(
        "station: grid {} kW, {} chargers, {} connectors",
        config.grid_w as f64 / 1000.0,
        config.chargers.len(),
        connectors
    );
    ExitCode::SUCCESS
}
