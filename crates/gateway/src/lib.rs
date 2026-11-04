//! The gateway: everything that talks to a venue. It holds no database pool and
//! no Kafka client; what it learns it reports to the core over [`contract`].

pub mod adapter;
pub mod contract;
pub mod stream_health;
pub mod stream_supervisor;
