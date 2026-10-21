//! Entry point: `sems <config.json>` loads and validates the station configuration,
//! then serves the HTTP API on port 8080.

use sems::{api, model::Config, model::Station};
use std::{env, fs, net::TcpListener, process::ExitCode};

const ADDR: &str = "0.0.0.0:8080";

#[actix_web::main]
async fn main() -> ExitCode {
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
        "station: grid {} kW, {} chargers, {} connectors — listening on {ADDR}",
        config.grid_w as f64 / 1000.0,
        config.chargers.len(),
        connectors,
    );

    let station = Station::new(config, api::now_ms());
    let result = async { api::serve(station, TcpListener::bind(ADDR)?)?.await }.await;
    if let Err(e) = result {
        eprintln!("server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
