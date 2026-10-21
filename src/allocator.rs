//! Power allocation: turns vehicle demands into setpoints that respect every limit
//! (grid > charger > connector > vehicle).
//!
//! V1 policy — capped equal share: each session gets the smallest of its demand, its connector
//! limit, an equal share of the grid and an equal share of its charger. Safe by construction
//! (each term is a safe share of its level), but power left unused by one session is not
//! redistributed to the others: that is the V2 improvement.

use crate::model::{Config, Watts};

/// One session's request, located by indices into the config.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    pub charger: usize,
    pub connector: usize,
    pub demand_w: Watts,
}

/// Computes one setpoint per demand, in the same order.
/// Integer division rounds down, so no sum can exceed its limit.
pub fn allocate(config: &Config, demands: &[Demand]) -> Vec<Watts> {
    // Sessions requesting nothing (e.g. battery full) take no share.
    let mut active_per_charger = vec![0; config.chargers.len()];
    for d in demands.iter().filter(|d| d.demand_w > 0) {
        active_per_charger[d.charger] += 1;
    }
    let active: Watts = active_per_charger.iter().sum();

    let setpoints: Vec<Watts> = demands
        .iter()
        .map(|d| {
            if d.demand_w == 0 {
                return 0;
            }
            let charger = &config.chargers[d.charger];
            d.demand_w
                .min(charger.connectors[d.connector].max_w)
                .min(config.grid_w / active)
                .min(charger.max_w / active_per_charger[d.charger])
        })
        .collect();

    debug_assert!(
        within_limits(config, demands, &setpoints),
        "allocation exceeds a limit"
    );
    setpoints
}

/// Safety invariant: no setpoint above its demand or connector, no charger or grid overload.
pub fn within_limits(config: &Config, demands: &[Demand], setpoints: &[Watts]) -> bool {
    let mut per_charger = vec![0; config.chargers.len()];
    for (d, &w) in demands.iter().zip(setpoints) {
        if w > d.demand_w || w > config.chargers[d.charger].connectors[d.connector].max_w {
            return false;
        }
        per_charger[d.charger] += w;
    }
    per_charger
        .iter()
        .zip(&config.chargers)
        .all(|(&w, c)| w <= c.max_w)
        && per_charger.iter().sum::<Watts>() <= config.grid_w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Charger, Connector};

    const KW: Watts = 1000;

    /// A station with the given grid and chargers (kW), two connectors per charger.
    fn config(grid_kw: Watts, chargers_kw: &[Watts]) -> Config {
        let chargers = chargers_kw
            .iter()
            .enumerate()
            .map(|(i, &kw)| Charger {
                id: format!("CP{}", i + 1),
                max_w: kw * KW,
                connectors: (1..=2)
                    .map(|id| Connector {
                        id,
                        kind: None,
                        max_w: kw * KW,
                    })
                    .collect(),
            })
            .collect();
        Config {
            grid_w: grid_kw * KW,
            chargers,
        }
    }

    /// Demand of `kw` on `(charger, connector)` indices.
    fn d(charger: usize, connector: usize, kw: Watts) -> Demand {
        Demand {
            charger,
            connector,
            demand_w: kw * KW,
        }
    }

    fn kw(setpoints: Vec<Watts>) -> Vec<Watts> {
        setpoints.into_iter().map(|w| w / KW).collect()
    }

    #[test]
    fn empty_station() {
        assert!(allocate(&config(400, &[300, 300]), &[]).is_empty());
    }

    #[test]
    fn single_vehicle_capped_by_its_charger() {
        assert_eq!(
            kw(allocate(&config(400, &[300, 300]), &[d(0, 0, 350)])),
            [300]
        );
    }

    #[test]
    fn single_vehicle_gets_its_demand() {
        assert_eq!(
            kw(allocate(&config(400, &[300, 300]), &[d(0, 0, 120)])),
            [120]
        );
    }

    #[test]
    fn local_contention_same_charger() {
        let cfg = config(400, &[300, 300]);
        assert_eq!(
            kw(allocate(&cfg, &[d(0, 0, 300), d(0, 1, 300)])),
            [150, 150]
        );
    }

    #[test]
    fn global_contention_different_chargers() {
        let cfg = config(400, &[300, 300]);
        assert_eq!(
            kw(allocate(&cfg, &[d(0, 0, 300), d(1, 0, 300)])),
            [200, 200]
        );
    }

    #[test]
    fn full_station() {
        let cfg = config(400, &[300, 300]);
        let all = [d(0, 0, 300), d(0, 1, 300), d(1, 0, 300), d(1, 1, 300)];
        assert_eq!(kw(allocate(&cfg, &all)), [100, 100, 100, 100]);
    }

    #[test]
    fn capped_by_connector() {
        let mut cfg = config(400, &[300, 300]);
        cfg.chargers[0].connectors[0].max_w = 150 * KW;
        assert_eq!(kw(allocate(&cfg, &[d(0, 0, 300)])), [150]);
    }

    #[test]
    fn zero_demand_takes_no_share() {
        let cfg = config(400, &[300, 300]);
        assert_eq!(kw(allocate(&cfg, &[d(0, 0, 0), d(0, 1, 300)])), [0, 300]);
    }

    #[test]
    fn rounds_down_never_above_grid() {
        // 400 kW / 3 sessions does not divide evenly: each gets 133 333 W, 1 W stays unallocated.
        let cfg = config(400, &[300, 300]);
        let demands = [d(0, 0, 300), d(0, 1, 300), d(1, 0, 300)];
        let setpoints = allocate(&cfg, &demands);
        assert_eq!(setpoints, [133_333, 133_333, 133_333]);
        assert!(within_limits(&cfg, &demands, &setpoints));
    }

    #[test]
    fn v1_known_waste() {
        // Grid 500 kW: CP1 caps A and B at 150 kW each, yet C only gets 500/3 instead of the
        // 200 kW left. 34 kW wasted; V2 (water-filling) is expected to fix this.
        let cfg = config(500, &[300, 300]);
        let setpoints = allocate(&cfg, &[d(0, 0, 300), d(0, 1, 300), d(1, 0, 300)]);
        assert_eq!(setpoints, [150_000, 150_000, 166_666]);
    }

    #[test]
    fn invariant_detects_overload() {
        let cfg = config(400, &[300, 300]);
        let demands = [d(0, 0, 300), d(1, 0, 300)];
        assert!(
            !within_limits(&cfg, &demands, &[300 * KW, 300 * KW]),
            "grid overload"
        );
        assert!(
            !within_limits(&cfg, &[d(0, 0, 300), d(0, 1, 300)], &[200 * KW, 200 * KW]),
            "charger overload"
        );
        assert!(
            !within_limits(&cfg, &[d(0, 0, 100)], &[150 * KW]),
            "above demand"
        );
    }
}
