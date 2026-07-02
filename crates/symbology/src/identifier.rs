use crate::cache::IdentityCache;
use crate::error::SymbologyError;
use crate::identity::{InstrumentIdentity, Resolution};
use crate::provider::{FigiProvider, MappingJob, MappingResult};
use crate::query::InstrumentQuery;

/// How to collapse multiple FIGI candidates into a [`Resolution`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisambiguationPolicy {
    /// Prefer the candidate matching the query's exchange, else the composite listing.
    #[default]
    PreferRequested,
    /// Never collapse: any multiple is `Ambiguous`.
    Strict,
}

/// Identifies instruments via a [`FigiProvider`], with a cache in front.
pub struct Identifier<P: FigiProvider, C: IdentityCache> {
    provider: P,
    cache: C,
    policy: DisambiguationPolicy,
}

impl<P: FigiProvider, C: IdentityCache> Identifier<P, C> {
    pub fn new(provider: P, cache: C) -> Self {
        Self { provider, cache, policy: DisambiguationPolicy::default() }
    }

    pub fn with_policy(mut self, policy: DisambiguationPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Identify one instrument. Cache hit → no network.
    pub async fn identify(&self, q: &InstrumentQuery) -> Result<Resolution, SymbologyError> {
        if let Some(hit) = self.cache.get(q) {
            return Ok(hit);
        }
        let job = build_job(q)?;
        let results = self.provider.map(&[job]).await?;
        let res = match results.into_iter().next() {
            Some(r) => interpret(r, q, self.policy),
            None => Resolution::NotFound,
        };
        self.cache.put(q, &res);
        Ok(res)
    }

    /// Identify many in one batched call (cache hits and unmappable queries skip the wire).
    pub async fn identify_batch(
        &self,
        queries: &[InstrumentQuery],
    ) -> Result<Vec<Resolution>, SymbologyError> {
        let mut out: Vec<Option<Resolution>> = vec![None; queries.len()];
        let mut jobs = Vec::new();
        let mut job_idx = Vec::new();

        for (i, q) in queries.iter().enumerate() {
            if let Some(hit) = self.cache.get(q) {
                out[i] = Some(hit);
            } else {
                match build_job(q) {
                    Ok(job) => {
                        jobs.push(job);
                        job_idx.push(i);
                    }
                    Err(_) => out[i] = Some(Resolution::NotFound),
                }
            }
        }

        if !jobs.is_empty() {
            let results = self.provider.map(&jobs).await?;
            for (k, r) in results.into_iter().enumerate() {
                let i = job_idx[k];
                let res = interpret(r, &queries[i], self.policy);
                self.cache.put(&queries[i], &res);
                out[i] = Some(res);
            }
        }

        Ok(out.into_iter().map(|o| o.unwrap_or(Resolution::NotFound)).collect())
    }
}

/// Pure: pick the strongest available identifier. Precedence figi → isin → cusip → ticker.
pub fn build_job(q: &InstrumentQuery) -> Result<MappingJob, SymbologyError> {
    let (id_type, id_value) = if let Some(v) = &q.figi {
        ("ID_BB_GLOBAL", v.clone())
    } else if let Some(v) = &q.isin {
        ("ID_ISIN", v.clone())
    } else if let Some(v) = &q.cusip {
        ("ID_CUSIP", v.clone())
    } else if let Some(v) = &q.ticker {
        ("TICKER", v.clone())
    } else {
        return Err(SymbologyError::NoIdentifier);
    };
    // Prefer exch_code for OpenFIGI; only fall back to mic_code when no exch_code is
    // given (sending both can over-constrain and return no match).
    let (exch_code, mic_code) = if q.exch_code.is_some() {
        (q.exch_code.clone(), None)
    } else {
        (None, q.mic.clone())
    };
    Ok(MappingJob {
        id_type: id_type.to_string(),
        id_value,
        exch_code,
        mic_code,
        currency: q.currency.clone(),
        market_sec_des: q.market_sec_des.clone(),
    })
}

/// Pure: one job result + the query → a Resolution.
pub fn interpret(
    result: MappingResult,
    q: &InstrumentQuery,
    policy: DisambiguationPolicy,
) -> Resolution {
    match result {
        MappingResult::Data(candidates) => disambiguate(candidates, q, policy),
        MappingResult::Warning(_) | MappingResult::Error(_) => Resolution::NotFound,
    }
}

/// Pure: collapse candidates per policy.
pub fn disambiguate(
    mut candidates: Vec<InstrumentIdentity>,
    q: &InstrumentQuery,
    policy: DisambiguationPolicy,
) -> Resolution {
    match candidates.len() {
        0 => Resolution::NotFound,
        1 => Resolution::Resolved(candidates.pop().unwrap()),
        _ => {
            if policy == DisambiguationPolicy::PreferRequested {
                // 1) the candidate on the requested exchange
                if let Some(exch) = &q.exch_code {
                    if let Some(found) =
                        candidates.iter().find(|c| c.exch_code.as_deref() == Some(exch.as_str()))
                    {
                        return Resolution::Resolved(found.clone());
                    }
                }
                // 2) else the composite/primary listing (figi == compositeFIGI)
                if let Some(found) =
                    candidates.iter().find(|c| c.composite_figi.as_deref() == Some(c.figi.as_str()))
                {
                    return Resolution::Resolved(found.clone());
                }
            }
            Resolution::Ambiguous(candidates)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubProvider {
        results: Vec<MappingResult>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl FigiProvider for StubProvider {
        async fn map(&self, jobs: &[MappingJob]) -> Result<Vec<MappingResult>, SymbologyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(jobs
                .iter()
                .enumerate()
                .map(|(i, _)| self.results[i % self.results.len()].clone())
                .collect())
        }
    }

    fn ident(figi: &str, exch: &str, composite: &str) -> InstrumentIdentity {
        InstrumentIdentity {
            figi: figi.into(),
            composite_figi: Some(composite.into()),
            share_class_figi: None,
            ticker: Some("AAPL".into()),
            name: Some("APPLE INC".into()),
            exch_code: Some(exch.into()),
            security_type: None,
            security_type2: None,
            market_sector: Some("Equity".into()),
        }
    }

    #[test]
    fn build_job_precedence() {
        let q = InstrumentQuery { figi: Some("BBG1".into()), isin: Some("US..".into()), ..Default::default() };
        assert_eq!(build_job(&q).unwrap().id_type, "ID_BB_GLOBAL");
        assert_eq!(build_job(&InstrumentQuery::isin("US0378331005")).unwrap().id_type, "ID_ISIN");
        assert_eq!(build_job(&InstrumentQuery::cusip("037833100")).unwrap().id_type, "ID_CUSIP");

        let j = build_job(&InstrumentQuery::ticker("AAPL").exch("US")).unwrap();
        assert_eq!(j.id_type, "TICKER");
        assert_eq!(j.exch_code.as_deref(), Some("US"));

        assert!(matches!(build_job(&InstrumentQuery::default()), Err(SymbologyError::NoIdentifier)));
    }

    #[test]
    fn disambiguate_prefers_requested_exch() {
        let cands = vec![ident("BBG_US", "US", "BBG_US"), ident("BBG_LN", "LN", "BBG_LN")];
        let q = InstrumentQuery::ticker("AAPL").exch("LN");
        match disambiguate(cands, &q, DisambiguationPolicy::PreferRequested) {
            Resolution::Resolved(i) => assert_eq!(i.exch_code.as_deref(), Some("LN")),
            other => panic!("expected resolved, got {other:?}"),
        }
    }

    #[test]
    fn disambiguate_falls_back_to_composite() {
        let cands = vec![ident("BBG_X", "XX", "BBG_US"), ident("BBG_US", "US", "BBG_US")];
        let q = InstrumentQuery::ticker("AAPL"); // no exch
        match disambiguate(cands, &q, DisambiguationPolicy::PreferRequested) {
            Resolution::Resolved(i) => assert_eq!(i.figi, "BBG_US"),
            other => panic!("expected composite, got {other:?}"),
        }
    }

    #[test]
    fn disambiguate_strict_is_ambiguous() {
        let cands = vec![ident("A", "US", "A"), ident("B", "LN", "B")];
        let q = InstrumentQuery::ticker("AAPL").exch("US");
        assert!(matches!(
            disambiguate(cands, &q, DisambiguationPolicy::Strict),
            Resolution::Ambiguous(_)
        ));
    }

    #[tokio::test]
    async fn identify_resolves_and_caches() {
        let provider = StubProvider {
            results: vec![MappingResult::Data(vec![ident("BBG1", "US", "BBG1")])],
            calls: AtomicUsize::new(0),
        };
        let id = Identifier::new(provider, InMemoryCache::new());
        let q = InstrumentQuery::ticker("AAPL").exch("US");

        let r1 = id.identify(&q).await.unwrap();
        assert!(matches!(r1, Resolution::Resolved(_)));
        let r2 = id.identify(&q).await.unwrap(); // cache hit
        assert_eq!(r1, r2);
        assert_eq!(id.provider.calls.load(Ordering::SeqCst), 1, "provider hit once");
    }

    #[tokio::test]
    async fn identify_not_found_on_warning() {
        let provider = StubProvider {
            results: vec![MappingResult::Warning("No identifier found.".into())],
            calls: AtomicUsize::new(0),
        };
        let id = Identifier::new(provider, InMemoryCache::new());
        let r = id.identify(&InstrumentQuery::ticker("ZZZZ")).await.unwrap();
        assert_eq!(r, Resolution::NotFound);
    }
}
