//! Station model: static configuration, live sessions, the events that change them, and the
//! history of those events.
//!
//! Power is held in integer watts so that rounding can never push a sum above a limit;
//! conversion from kW happens at the edges (config parsing, API).
//! Every event recomputes all setpoints from scratch, so a demand going up or down
//! follows the same path.

use crate::allocator::{allocate, Demand};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use uuid::Uuid;

/// Power in watts.
pub type Watts = u64;

/// Converts a kW value from the outside world to watts. `None` if negative or not finite.
pub fn kw_to_w(kw: f64) -> Option<Watts> {
    (kw.is_finite() && kw >= 0.0).then(|| (kw * 1000.0).round() as Watts)
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// Static description of the station, validated at startup.
#[derive(Debug)]
pub struct Config {
    pub grid_w: Watts,
    pub chargers: Vec<Charger>,
}

/// A charger: its power is shared between its connectors.
#[derive(Debug)]
pub struct Charger {
    pub id: String,
    pub max_w: Watts,
    pub connectors: Vec<Connector>,
}

/// A connector (plug) of a charger.
#[derive(Debug)]
pub struct Connector {
    pub id: u32,
    /// Plug standard (e.g. "CCS"), informational only.
    pub kind: Option<String>,
    pub max_w: Watts,
}

// JSON shape of the config file (kW, optional fields), converted into `Config`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    grid_kw: f64,
    chargers: Vec<RawCharger>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCharger {
    id: String,
    max_kw: f64,
    connectors: Vec<RawConnector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConnector {
    id: u32,
    #[serde(rename = "type")]
    kind: Option<String>,
    max_kw: Option<f64>,
}

impl Config {
    /// Parses and validates a station configuration (JSON, powers in kW).
    /// A connector without `max_kw` inherits its charger's limit.
    pub fn from_json(json: &str) -> Result<Config, String> {
        let raw: RawConfig =
            serde_json::from_str(json).map_err(|e| format!("invalid config: {e}"))?;
        let positive = |kw: f64, name: &str| match kw_to_w(kw) {
            Some(w) if w > 0 => Ok(w),
            _ => Err(format!("{name}: power must be > 0 kW")),
        };

        let grid_w = positive(raw.grid_kw, "grid")?;
        if raw.chargers.is_empty() {
            return Err("station has no charger".into());
        }

        let mut chargers: Vec<Charger> = Vec::with_capacity(raw.chargers.len());
        for c in raw.chargers {
            let name = format!("charger {}", c.id);
            if chargers.iter().any(|x| x.id == c.id) {
                return Err(format!("{name}: duplicate id"));
            }
            let max_w = positive(c.max_kw, &name)?;
            if c.connectors.is_empty() {
                return Err(format!("{name}: no connector"));
            }

            let mut connectors: Vec<Connector> = Vec::with_capacity(c.connectors.len());
            for k in c.connectors {
                let name = format!("{name} connector {}", k.id);
                if connectors.iter().any(|x| x.id == k.id) {
                    return Err(format!("{name}: duplicate id"));
                }
                let conn_max_w = k.max_kw.map_or(Ok(max_w), |kw| positive(kw, &name))?;
                if conn_max_w > max_w {
                    return Err(format!("{name}: more powerful than its charger"));
                }
                connectors.push(Connector {
                    id: k.id,
                    kind: k.kind,
                    max_w: conn_max_w,
                });
            }
            chargers.push(Charger {
                id: c.id,
                max_w,
                connectors,
            });
        }
        Ok(Config { grid_w, chargers })
    }
}

// ---------------------------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------------------------

/// A vehicle charging session on a connector.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    /// Power the vehicle currently accepts (input, may change at any time).
    pub demand_w: Watts,
    /// Power the station allows (allocator output), always <= `demand_w`.
    pub setpoint_w: Watts,
    pub started_at_ms: u64,
}

/// Connector status, derived from its session (names borrowed from OCPP).
/// Tells OPS whether a plugged-in vehicle is idle because of itself or because of the station.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// No vehicle.
    Available,
    /// Receiving power.
    Charging,
    /// Plugged in, the vehicle requests nothing (e.g. battery full).
    SuspendedEv,
    /// Plugged in, the vehicle requests power but the station gives none.
    SuspendedEvse,
}

/// Live state of one connector.
#[derive(Debug)]
pub struct Slot {
    pub session: Option<Session>,
    /// When the derived status last changed.
    pub status_since_ms: u64,
}

impl Slot {
    /// Derived, never stored, so it cannot drift from the session.
    pub fn status(&self) -> Status {
        match &self.session {
            None => Status::Available,
            Some(s) if s.demand_w == 0 => Status::SuspendedEv,
            Some(s) if s.setpoint_w == 0 => Status::SuspendedEvse,
            Some(_) => Status::Charging,
        }
    }
}

/// Why an event was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum EventError {
    UnknownConnector,
    ConnectorBusy,
    UnknownSession,
}

/// Kind of an accepted event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted,
    DemandChanged,
    SessionEnded,
}

/// History entry: an accepted event and the allocation that resulted from it.
/// Lets OPS explain a setpoint afterwards and audit that no limit was ever exceeded.
#[derive(Debug, Clone)]
pub struct Record {
    /// Increases by one per accepted event and never restarts, even when old entries are
    /// dropped: a reader can detect what it missed.
    pub seq: u64,
    pub at_ms: u64,
    pub kind: EventKind,
    pub session_id: Uuid,
    /// Charger and connector indices, in config order.
    pub charger: usize,
    pub connector: usize,
    /// The session's demand after the event (its last demand for `SessionEnded`).
    pub demand_w: Watts,
    /// Setpoint of every active session right after the event.
    pub setpoints: Vec<(Uuid, Watts)>,
}

/// Number of history entries kept: memory stays bounded, the oldest entries are dropped.
pub const HISTORY_CAP: usize = 10_000;

/// What an accepted event is about; `apply` completes it into a [`Record`].
struct Event {
    kind: EventKind,
    session_id: Uuid,
    charger: usize,
    connector: usize,
    demand_w: Watts,
}

/// The whole station: static config, live state of every connector, recent history.
#[derive(Debug)]
pub struct Station {
    pub config: Config,
    /// `slots[c][k]` is connector `k` of charger `c`, in config order.
    pub slots: Vec<Vec<Slot>>,
    /// Latest accepted events, oldest first (at most `history_cap`).
    pub history: VecDeque<Record>,
    history_cap: usize,
    last_seq: u64,
}

impl Station {
    /// Creates an empty station: every connector is available since `now_ms`.
    pub fn new(config: Config, now_ms: u64) -> Self {
        let slots = config
            .chargers
            .iter()
            .map(|c| {
                c.connectors
                    .iter()
                    .map(|_| Slot {
                        session: None,
                        status_since_ms: now_ms,
                    })
                    .collect()
            })
            .collect();
        Station {
            config,
            slots,
            history: VecDeque::new(),
            history_cap: HISTORY_CAP,
            last_seq: 0,
        }
    }

    /// A vehicle plugs in. Returns the new session id.
    pub fn connect(
        &mut self,
        charger: &str,
        connector: u32,
        demand_w: Watts,
        now_ms: u64,
    ) -> Result<Uuid, EventError> {
        let (c, k) = self
            .find_connector(charger, connector)
            .ok_or(EventError::UnknownConnector)?;
        if self.slots[c][k].session.is_some() {
            return Err(EventError::ConnectorBusy);
        }
        let id = Uuid::new_v4();
        let event = Event {
            kind: EventKind::SessionStarted,
            session_id: id,
            charger: c,
            connector: k,
            demand_w,
        };
        self.apply(now_ms, event, |s| {
            s.slots[c][k].session = Some(Session {
                id,
                demand_w,
                setpoint_w: 0,
                started_at_ms: now_ms,
            });
        });
        Ok(id)
    }

    /// The power accepted by the vehicle changes, up or down.
    pub fn change_demand(
        &mut self,
        id: Uuid,
        demand_w: Watts,
        now_ms: u64,
    ) -> Result<(), EventError> {
        let (c, k) = self.find_session(id).ok_or(EventError::UnknownSession)?;
        let event = Event {
            kind: EventKind::DemandChanged,
            session_id: id,
            charger: c,
            connector: k,
            demand_w,
        };
        self.apply(now_ms, event, |s| {
            if let Some(session) = s.slots[c][k].session.as_mut() {
                session.demand_w = demand_w;
            }
        });
        Ok(())
    }

    /// The vehicle unplugs: its connector becomes available.
    pub fn disconnect(&mut self, id: Uuid, now_ms: u64) -> Result<(), EventError> {
        let (c, k) = self.find_session(id).ok_or(EventError::UnknownSession)?;
        let event = Event {
            kind: EventKind::SessionEnded,
            session_id: id,
            charger: c,
            connector: k,
            demand_w: self.slots[c][k].session.as_ref().map_or(0, |s| s.demand_w),
        };
        self.apply(now_ms, event, |s| s.slots[c][k].session = None);
        Ok(())
    }

    /// Active sessions with their `(charger, connector)` indices.
    pub fn sessions(&self) -> impl Iterator<Item = (usize, usize, &Session)> {
        self.slots.iter().enumerate().flat_map(|(c, row)| {
            row.iter()
                .enumerate()
                .filter_map(move |(k, slot)| slot.session.as_ref().map(|s| (c, k, s)))
        })
    }

    fn find_connector(&self, charger: &str, connector: u32) -> Option<(usize, usize)> {
        let c = self.config.chargers.iter().position(|x| x.id == charger)?;
        let k = self.config.chargers[c]
            .connectors
            .iter()
            .position(|x| x.id == connector)?;
        Some((c, k))
    }

    fn find_session(&self, id: Uuid) -> Option<(usize, usize)> {
        self.sessions()
            .find(|(_, _, s)| s.id == id)
            .map(|(c, k, _)| (c, k))
    }

    /// The single path of every accepted event: applies the mutation, recomputes every
    /// setpoint, stamps connectors whose status changed, and records the event with the
    /// resulting allocation. Rejected events never get here, so they are never recorded.
    fn apply(&mut self, now_ms: u64, event: Event, mutate: impl FnOnce(&mut Self)) {
        let before: Vec<Status> = self.slots.iter().flatten().map(Slot::status).collect();
        mutate(self);
        self.reallocate();
        for (slot, old) in self.slots.iter_mut().flatten().zip(before) {
            if slot.status() != old {
                slot.status_since_ms = now_ms;
            }
        }

        if self.history.len() == self.history_cap {
            self.history.pop_front();
        }
        self.last_seq += 1;
        let setpoints = self
            .sessions()
            .map(|(_, _, s)| (s.id, s.setpoint_w))
            .collect();
        self.history.push_back(Record {
            seq: self.last_seq,
            at_ms: now_ms,
            kind: event.kind,
            session_id: event.session_id,
            charger: event.charger,
            connector: event.connector,
            demand_w: event.demand_w,
            setpoints,
        });
    }

    fn reallocate(&mut self) {
        let demands: Vec<Demand> = self
            .sessions()
            .map(|(charger, connector, s)| Demand {
                charger,
                connector,
                demand_w: s.demand_w,
            })
            .collect();
        let setpoints = allocate(&self.config, &demands);
        for (d, w) in demands.iter().zip(setpoints) {
            if let Some(session) = self.slots[d.charger][d.connector].session.as_mut() {
                session.setpoint_w = w;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KW: Watts = 1000;

    /// The brief's station: grid 400 kW, 2 chargers of 300 kW, 2 CCS connectors each.
    const BRIEF: &str = r#"{ "grid_kw": 400, "chargers": [
        { "id": "CP1", "max_kw": 300, "connectors": [ { "id": 1, "type": "CCS" }, { "id": 2, "type": "CCS", "max_kw": 150 } ] },
        { "id": "CP2", "max_kw": 300, "connectors": [ { "id": 1 }, { "id": 2 } ] } ] }"#;

    fn station() -> Station {
        Station::new(Config::from_json(BRIEF).unwrap(), 0)
    }

    fn slot(st: &Station, id: Uuid) -> &Slot {
        let (c, k) = st.find_session(id).unwrap();
        &st.slots[c][k]
    }

    #[test]
    fn kw_conversion() {
        assert_eq!(kw_to_w(7.4), Some(7400));
        assert_eq!(kw_to_w(0.0), Some(0));
        assert_eq!(kw_to_w(-1.0), None);
        assert_eq!(kw_to_w(f64::NAN), None);
    }

    #[test]
    fn parses_config_with_defaults() {
        let cfg = Config::from_json(BRIEF).unwrap();
        assert_eq!(cfg.grid_w, 400 * KW);
        assert_eq!(
            cfg.chargers[0].connectors[0].max_w,
            300 * KW,
            "inherits the charger limit"
        );
        assert_eq!(cfg.chargers[0].connectors[1].max_w, 150 * KW);
        assert_eq!(cfg.chargers[0].connectors[0].kind.as_deref(), Some("CCS"));
        assert_eq!(cfg.chargers[1].connectors[0].kind, None);
    }

    #[test]
    fn rejects_invalid_configs() {
        let cases = [
            ("not json", "invalid config"),
            (
                r#"{ "grid_kw": 400, "chargers": [], "bess": 1 }"#,
                "invalid config",
            ),
            (
                r#"{ "grid_kw": 0, "chargers": [] }"#,
                "grid: power must be > 0",
            ),
            (r#"{ "grid_kw": 400, "chargers": [] }"#, "no charger"),
            (
                r#"{ "grid_kw": 400, "chargers": [ { "id": "A", "max_kw": -1, "connectors": [ { "id": 1 } ] } ] }"#,
                "charger A: power",
            ),
            (
                r#"{ "grid_kw": 400, "chargers": [ { "id": "A", "max_kw": 50, "connectors": [] } ] }"#,
                "charger A: no connector",
            ),
            (
                r#"{ "grid_kw": 400, "chargers": [ { "id": "A", "max_kw": 50, "connectors": [ { "id": 1 } ] },
                                                  { "id": "A", "max_kw": 50, "connectors": [ { "id": 1 } ] } ] }"#,
                "charger A: duplicate id",
            ),
            (
                r#"{ "grid_kw": 400, "chargers": [ { "id": "A", "max_kw": 50, "connectors": [ { "id": 1 }, { "id": 1 } ] } ] }"#,
                "connector 1: duplicate id",
            ),
            (
                r#"{ "grid_kw": 400, "chargers": [ { "id": "A", "max_kw": 50, "connectors": [ { "id": 1, "max_kw": 60 } ] } ] }"#,
                "more powerful than its charger",
            ),
        ];
        for (json, expected) in cases {
            let err = Config::from_json(json).unwrap_err();
            assert!(
                err.contains(expected),
                "{json}\n→ {err}\n(expected: {expected})"
            );
        }
    }

    #[test]
    fn session_lifecycle_updates_setpoints_and_statuses() {
        let mut st = station();

        // A alone on CP2: takes its full demand.
        let a = st.connect("CP2", 1, 300 * KW, 10).unwrap();
        assert_eq!(slot(&st, a).session.as_ref().unwrap().setpoint_w, 300 * KW);
        assert_eq!(slot(&st, a).status(), Status::Charging);
        assert_eq!(slot(&st, a).status_since_ms, 10);
        assert_eq!(slot(&st, a).session.as_ref().unwrap().started_at_ms, 10);

        // B on the same charger: they share its 300 kW.
        let b = st.connect("CP2", 2, 300 * KW, 20).unwrap();
        assert_eq!(slot(&st, a).session.as_ref().unwrap().setpoint_w, 150 * KW);
        assert_eq!(slot(&st, b).session.as_ref().unwrap().setpoint_w, 150 * KW);
        assert_eq!(
            slot(&st, a).status_since_ms,
            10,
            "setpoint changed, status did not"
        );

        // A's battery is full: A stays plugged in but takes no share, B gets everything.
        st.change_demand(a, 0, 30).unwrap();
        assert_eq!(slot(&st, a).status(), Status::SuspendedEv);
        assert_eq!(slot(&st, a).status_since_ms, 30);
        assert_eq!(slot(&st, b).session.as_ref().unwrap().setpoint_w, 300 * KW);

        // A asks for power again (demand going up).
        st.change_demand(a, 100 * KW, 40).unwrap();
        assert_eq!(slot(&st, a).status(), Status::Charging);
        assert_eq!(slot(&st, a).status_since_ms, 40);
        assert_eq!(slot(&st, a).session.as_ref().unwrap().setpoint_w, 100 * KW);
        assert_eq!(slot(&st, b).session.as_ref().unwrap().setpoint_w, 150 * KW);

        // B leaves: its connector is available again.
        st.disconnect(b, 50).unwrap();
        assert_eq!(st.slots[1][1].status(), Status::Available);
        assert_eq!(st.slots[1][1].status_since_ms, 50);
        assert_eq!(st.sessions().count(), 1);
    }

    #[test]
    fn rejects_invalid_events() {
        let mut st = station();
        let a = st.connect("CP1", 1, 50 * KW, 0).unwrap();
        assert_eq!(
            st.connect("CP9", 1, KW, 0),
            Err(EventError::UnknownConnector)
        );
        assert_eq!(
            st.connect("CP1", 9, KW, 0),
            Err(EventError::UnknownConnector)
        );
        assert_eq!(st.connect("CP1", 1, KW, 0), Err(EventError::ConnectorBusy));

        let unknown = Uuid::new_v4();
        assert_eq!(
            st.change_demand(unknown, KW, 0),
            Err(EventError::UnknownSession)
        );
        assert_eq!(st.disconnect(unknown, 0), Err(EventError::UnknownSession));

        st.disconnect(a, 0).unwrap();
        assert_eq!(
            st.disconnect(a, 0),
            Err(EventError::UnknownSession),
            "already ended"
        );
    }

    #[test]
    fn history_records_accepted_events_with_their_result() {
        let mut st = station();
        let a = st.connect("CP2", 1, 300 * KW, 10).unwrap();
        let b = st.connect("CP2", 2, 300 * KW, 20).unwrap();
        st.change_demand(a, 100 * KW, 30).unwrap();
        st.disconnect(b, 40).unwrap();
        assert!(
            st.connect("CP2", 1, KW, 50).is_err(),
            "rejected: not recorded"
        );

        let h: Vec<&Record> = st.history.iter().collect();
        assert_eq!(h.len(), 4);
        assert_eq!(h.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(
            h.iter().map(|r| r.at_ms).collect::<Vec<_>>(),
            [10, 20, 30, 40]
        );

        // B's arrival: both sessions share CP2.
        assert_eq!(h[1].kind, EventKind::SessionStarted);
        assert_eq!(h[1].setpoints, [(a, 150 * KW), (b, 150 * KW)]);

        // A's demand drops: recorded with its new demand and the resulting split.
        assert_eq!(h[2].kind, EventKind::DemandChanged);
        assert_eq!((h[2].session_id, h[2].demand_w), (a, 100 * KW));
        assert_eq!(h[2].setpoints, [(a, 100 * KW), (b, 150 * KW)]);

        // B leaves: where it was and its last demand are kept; only A remains.
        assert_eq!(h[3].kind, EventKind::SessionEnded);
        assert_eq!(
            (h[3].charger, h[3].connector, h[3].demand_w),
            (1, 1, 300 * KW)
        );
        assert_eq!(h[3].setpoints, [(a, 100 * KW)]);
    }

    #[test]
    fn history_keeps_the_latest_entries_when_full() {
        let mut st = station();
        st.history_cap = 2;
        let a = st.connect("CP1", 1, KW, 0).unwrap();
        st.change_demand(a, 2 * KW, 0).unwrap();
        st.change_demand(a, 3 * KW, 0).unwrap();

        let seqs: Vec<u64> = st.history.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [2, 3], "oldest dropped, numbering continues");
    }
}
