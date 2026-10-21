//! SEMS — Station Energy Management System: the load-management "brain" of a charging station.
//!
//! - [`model`]: station configuration, live sessions, and the events that change them.
//! - [`allocator`]: the pure algorithm turning vehicle demands into power setpoints.
//! - [`api`]: the HTTP layer (kW ↔ W conversion, routes, error codes).

pub mod allocator;
pub mod api;
pub mod model;
