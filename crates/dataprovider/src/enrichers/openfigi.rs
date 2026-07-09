//! OpenFIGI enricher — stamps FIGI identifiers via the `symbology` engine.
//!
//! Batches the SPOT rows of a fetch into OpenFIGI (`ticker` + `exch_code`) and
//! fills `Identifiers::figi` / `composite_figi`. Pure: no DB access — the seeder
//! persists whatever this sets.

use async_trait::async_trait;
use symbology::{
    openfigi_exch_code, Identifier, InMemoryCache, InstrumentQuery, OpenFigiClient, Resolution,
};

use crate::enrich::{EnrichReport, Enricher};
use crate::error::ProviderError;
use crate::instrument::InstrumentDef;

pub struct OpenFigiEnricher {
    engine: Identifier<OpenFigiClient, InMemoryCache>,
}

impl OpenFigiEnricher {
    /// Build with an optional OpenFIGI API key (raises batch/rate limits).
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            engine: Identifier::new(OpenFigiClient::new(api_key), InMemoryCache::new()),
        }
    }
}

#[async_trait]
impl Enricher for OpenFigiEnricher {
    fn code(&self) -> &'static str {
        "OPENFIGI"
    }

    async fn enrich(&self, defs: &mut [InstrumentDef]) -> Result<EnrichReport, ProviderError> {
        // Only SPOT rows are worth resolving to a FIGI.
        let idx: Vec<usize> = defs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.instrument_class == "SPOT")
            .map(|(i, _)| i)
            .collect();
        if idx.is_empty() {
            return Ok(EnrichReport::default());
        }

        let queries: Vec<InstrumentQuery> = idx
            .iter()
            .map(|&i| InstrumentQuery {
                ticker: Some(defs[i].symbol.clone()),
                exch_code: openfigi_exch_code(&defs[i].venue).map(str::to_string),
                ..Default::default()
            })
            .collect();

        let results = self
            .engine
            .identify_batch(&queries)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;

        let mut report = EnrichReport::default();
        for (&i, res) in idx.iter().zip(results) {
            if let Resolution::Resolved(identity) = res {
                let d = &mut defs[i];
                d.identifiers.figi = Some(identity.figi.clone());
                d.identifiers.composite_figi = identity.composite_figi.clone();
                report.resolved.push((d.symbol.clone(), d.venue.clone()));
            }
        }
        Ok(report)
    }
}
