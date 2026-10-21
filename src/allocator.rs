//! Power allocation: turns vehicle demands into setpoints that respect every limit
//! (grid > charger > connector > vehicle).
//!
//! V2 policy — max-min fair water-filling. Picture each session as a glass as tall as what it
//! can take; power is poured into every glass at the same level. A small glass fills up first
//! and what it does not take keeps raising the others. No power is left unused while a session
//! could take it, and nobody can get more without taking from someone served less.
//!
//! The cascade is handled with two fillings: first inside each charger (its sessions share its
//! power), then across the grid (every session, already lowered to its charger's level).
//! Integer division rounds down, so no sum can exceed its limit (at most n − 1 W stay unused).
//! The V1 capped equal share lives in the git history (branch `v1`).

use crate::model::{Config, Watts};

/// One session's request, located by indices into the config.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    pub charger: usize,
    pub connector: usize,
    pub demand_w: Watts,
}

/// Water level: the highest `level` such that `Σ min(cap, level)` stays within `capacity`,
/// or `Watts::MAX` when every cap fits (nobody is limited by this capacity).
///
/// Caps are taken in ascending order: one below the equal share of what is left is served in
/// full; the first one above it sets the level, and every remaining cap stops there.
pub fn fill(capacity: Watts, mut caps: Vec<Watts>) -> Watts {
    caps.sort_unstable();
    let mut remaining = capacity;
    for (i, &cap) in caps.iter().enumerate() {
        let share = remaining / (caps.len() - i) as Watts;
        if cap > share {
            return share;
        }
        remaining -= cap;
    }
    Watts::MAX
}

/// Computes one setpoint per demand, in the same order.
pub fn allocate(config: &Config, demands: &[Demand]) -> Vec<Watts> {
    // 1. Each session's own cap: what the vehicle accepts, within its connector.
    //    A zero demand is a zero cap: it naturally takes nothing.
    let caps: Vec<Watts> = demands
        .iter()
        .map(|d| {
            d.demand_w
                .min(config.chargers[d.charger].connectors[d.connector].max_w)
        })
        .collect();

    // 2. Charger level: the sessions of each charger share its power.
    let mut per_charger = vec![Vec::new(); config.chargers.len()];
    for (d, &cap) in demands.iter().zip(&caps) {
        per_charger[d.charger].push(cap);
    }
    let charger_level: Vec<Watts> = per_charger
        .into_iter()
        .zip(&config.chargers)
        .map(|(caps, charger)| fill(charger.max_w, caps))
        .collect();

    // 3. Caps lowered to their charger's level.
    let caps: Vec<Watts> = demands
        .iter()
        .zip(caps)
        .map(|(d, cap)| cap.min(charger_level[d.charger]))
        .collect();

    // 4–5. Grid level: every session shares the grid, each within its lowered cap.
    let grid_level = fill(config.grid_w, caps.clone());
    let setpoints: Vec<Watts> = caps.into_iter().map(|cap| cap.min(grid_level)).collect();

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
    fn fill_serves_small_caps_in_full_and_shares_the_rest() {
        // 100 to share: 10 fits, then 90 / 2 = 45 each for the two big ones.
        assert_eq!(fill(100, vec![80, 10, 80]), 45);
        // Equal caps: plain equal share, rounded down.
        assert_eq!(fill(400_000, vec![300_000; 3]), 133_333);
        // Everything fits: no level.
        assert_eq!(fill(400, vec![22, 50, 300]), Watts::MAX);
        assert_eq!(fill(10, vec![]), Watts::MAX);
        // Zero caps take nothing.
        assert_eq!(fill(100, vec![0, 0, 300]), 100);
    }

    #[test]
    fn saturated_charger_leaves_the_rest_to_others() {
        // V1 gave 150 / 150 / 166.7 kW (33.3 kW wasted). CP1 cannot give A and B more than
        // 150 kW each, so C takes the 200 kW left of the grid.
        let cfg = config(500, &[300, 300]);
        let setpoints = allocate(&cfg, &[d(0, 0, 300), d(0, 1, 300), d(1, 0, 300)]);
        assert_eq!(kw(setpoints), [150, 150, 200]);
    }

    #[test]
    fn small_chargers_unused_share_goes_to_the_big_one() {
        // V1 gave 133.3 / 22 / 50 kW. The 22 and 50 kW chargers are served in full and the
        // 300 kW one takes everything it can.
        let cfg = config(400, &[300, 22, 50]);
        let setpoints = allocate(&cfg, &[d(0, 0, 300), d(1, 0, 22), d(2, 0, 50)]);
        assert_eq!(kw(setpoints), [300, 22, 50]);
    }

    #[test]
    fn demand_drop_is_redistributed() {
        // V1 gave 50 / 200 kW: B now gets what A no longer takes, up to its charger.
        let cfg = config(400, &[300, 300]);
        assert_eq!(kw(allocate(&cfg, &[d(0, 0, 50), d(1, 0, 300)])), [50, 300]);
    }

    #[test]
    fn smallest_demand_served_first() {
        // Grid 100 kW, three chargers: 10 kW is served in full, the others share 90 kW.
        let cfg = config(100, &[300, 300, 300]);
        let setpoints = allocate(&cfg, &[d(0, 0, 80), d(1, 0, 10), d(2, 0, 80)]);
        assert_eq!(kw(setpoints), [45, 10, 45]);
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
