//! `symbology` — identify financial instruments to a canonical FIGI identity.
//!
//! Give it whatever identifiers you have (ticker+exchange, ISIN, CUSIP, FIGI) and it
//! resolves to a FIGI-anchored [`InstrumentIdentity`] via the OpenFIGI mapping API.
//! Functional core (pure [`build_job`] / [`disambiguate`]) behind a swappable async
//! [`FigiProvider`], with an [`IdentityCache`] in front.
//!
//! ```no_run
//! # async fn run() -> Result<(), symbology::SymbologyError> {
//! use symbology::{Identifier, InstrumentQuery, OpenFigiClient, InMemoryCache};
//! let id = Identifier::new(OpenFigiClient::new(None), InMemoryCache::new());
//! let res = id.identify(&InstrumentQuery::ticker("AAPL").exch("US")).await?;
//! println!("{res:?}");
//! # Ok(()) }
//! ```

mod cache;
mod error;
mod exchange;
mod identifier;
mod identity;
mod openfigi;
mod provider;
mod query;

pub use cache::{IdentityCache, InMemoryCache, NoCache};
pub use error::SymbologyError;
pub use exchange::openfigi_exch_code;
pub use identifier::{build_job, disambiguate, interpret, DisambiguationPolicy, Identifier};
pub use identity::{InstrumentIdentity, Resolution};
pub use openfigi::OpenFigiClient;
pub use provider::{FigiProvider, MappingJob, MappingResult};
pub use query::InstrumentQuery;
