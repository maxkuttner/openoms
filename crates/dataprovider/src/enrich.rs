//! The enrichment capability — the extensibility seam for metadata / identifiers.
//!
//! An [`Enricher`] takes a batch of freshly-fetched [`InstrumentDef`]s and fills
//! their [`crate::Identifiers`] bag from some external endpoint (OpenFIGI → FIGI,
//! and later ISIN/CUSIP/issuer/fund sources). Enrichers are pure: they mutate the
//! defs in place and report what they resolved; the seeder owns persistence.
//!
//! The seeder holds a `Vec<Box<dyn Enricher>>` and runs each in turn, so a new
//! metadata source is a new impl pushed into that vector — nothing else changes.

use async_trait::async_trait;

use crate::error::ProviderError;
use crate::instrument::InstrumentDef;

/// What an enricher resolved on one batch. `resolved` lists the `(symbol, venue)`
/// pairs it stamped, so the seeder can write one xref row per enricher.
#[derive(Debug, Clone, Default)]
pub struct EnrichReport {
    pub resolved: Vec<(String, String)>,
}

impl EnrichReport {
    pub fn count(&self) -> usize {
        self.resolved.len()
    }
}

#[async_trait]
pub trait Enricher: Send + Sync {
    /// Stable code stamped into `oms.instrument_xref.source_code` (e.g. `OPENFIGI`).
    fn code(&self) -> &'static str;

    /// Fill the `identifiers` bag on the defs this enricher can resolve.
    async fn enrich(&self, defs: &mut [InstrumentDef]) -> Result<EnrichReport, ProviderError>;
}
