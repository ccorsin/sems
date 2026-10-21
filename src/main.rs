//! Entry point: `sems <config.json>` loads and validates the station configuration,
//! then serves the HTTP API on port 8080.

use actix_web::{web, App, HttpServer};
use sems::{api, model::Config, model::Station};
use std::{env, fs, process::ExitCode, sync::Mutex};

const ADDR: (&str, u16) = ("0.0.0.0", 8080);

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
        "station: grid {} kW, {} chargers, {} connectors — listening on {}:{}",
        config.grid_w as f64 / 1000.0,
        config.chargers.len(),
        connectors,
        ADDR.0,
        ADDR.1
    );

    let state = web::Data::new(Mutex::new(Station::new(config, api::now_ms())));
    let server =
        HttpServer::new(move || App::new().app_data(state.clone()).configure(api::configure));
    if let Err(e) = async { server.bind(ADDR)?.run().await }.await {
        eprintln!("server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
