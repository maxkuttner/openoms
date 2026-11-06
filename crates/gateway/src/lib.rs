//! The gateway: everything that talks to a venue. It holds no database pool and
//! no Kafka client; what it learns it reports to the core over [`contract`].
//!
//! Each connector is one directory under [`connectors`]; the rest is shared.

pub mod adapter;
pub mod connectors;
pub mod contract;
pub mod credentials;
#[cfg(feature = "fix")]
pub mod fix;
#[cfg(feature = "fix")]
pub mod fix_acceptor;
pub mod md;
pub mod stream_health;
pub mod stream_supervisor;
pub mod venue;
pub mod verify;
