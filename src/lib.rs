//! SEMS — Station Energy Management System: the load-management "brain" of a charging station.
//!
//! - [`model`]: station configuration, live sessions, and the events that change them.
//! - [`allocator`]: the pure algorithm turning vehicle demands into power setpoints.

pub mod allocator;
pub mod model;
