//! The control link: one quiet TLS WebSocket a co-located strategy keeps open
//! to the OMS, off its order path. The OMS pushes state, limits, positions and
//! kill; the strategy acknowledges. Both heartbeat every second, and the
//! strategy stops itself when it stops hearing the OMS ([`client::DeadMan`]).

pub mod client;
pub mod proto;
pub mod v2;
