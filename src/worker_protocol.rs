//! Host-facing re-export of the neutral Ticket 11 worker protocol.

pub use bunkerbox_worker_protocol::*;

#[cfg(test)]
#[path = "worker_protocol_ut.rs"]
mod worker_protocol_tests;
