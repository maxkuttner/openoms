use async_trait::async_trait;

use crate::error::SymbologyError;
use crate::identity::InstrumentIdentity;

/// One OpenFIGI mapping job (`POST /v3/mapping` array element).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingJob {
    pub id_type: String,
    pub id_value: String,
    pub exch_code: Option<String>,
    pub mic_code: Option<String>,
    pub currency: Option<String>,
    pub market_sec_des: Option<String>,
}

/// The result for one job: matches, a warning (no match), or an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MappingResult {
    Data(Vec<InstrumentIdentity>),
    Warning(String),
    Error(String),
}

/// Port for an instrument-mapping backend — OpenFIGI in production, a stub in tests.
/// `map` returns one [`MappingResult`] per input job, in order.
#[async_trait]
pub trait FigiProvider: Send + Sync {
    async fn map(&self, jobs: &[MappingJob]) -> Result<Vec<MappingResult>, SymbologyError>;
}
