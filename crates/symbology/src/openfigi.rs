use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use crate::error::SymbologyError;
use crate::identity::InstrumentIdentity;
use crate::provider::{FigiProvider, MappingJob, MappingResult};

const DEFAULT_BASE: &str = "https://api.openfigi.com";
const MAX_RETRIES: u32 = 3;

/// OpenFIGI-backed [`FigiProvider`]. Batches jobs and backs off on 429.
pub struct OpenFigiClient {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    max_batch: usize,
}

impl OpenFigiClient {
    /// `api_key = None` works (lower limits: ≤10 jobs/request); with a key, ≤100.
    pub fn new(api_key: Option<String>) -> Self {
        let max_batch = if api_key.is_some() { 100 } else { 10 };
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: DEFAULT_BASE.to_string(),
            max_batch,
        }
    }

    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    async fn map_chunk(&self, jobs: &[MappingJob]) -> Result<Vec<MappingResult>, SymbologyError> {
        let body: Vec<Value> = jobs.iter().map(job_to_json).collect();
        let url = format!("{}/v3/mapping", self.base_url);

        for attempt in 0..=MAX_RETRIES {
            let mut req = self.client.post(&url).json(&body);
            if let Some(key) = &self.api_key {
                req = req.header("X-OPENFIGI-APIKEY", key);
            }
            let resp = req.send().await.map_err(|e| SymbologyError::Network(e.to_string()))?;

            if resp.status().as_u16() == 429 {
                if attempt == MAX_RETRIES {
                    return Err(SymbologyError::RateLimited);
                }
                let wait = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(6);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            if !resp.status().is_success() {
                return Err(SymbologyError::Http(resp.status().as_u16()));
            }
            let parsed: Vec<Value> = resp
                .json()
                .await
                .map_err(|e| SymbologyError::Decode(e.to_string()))?;
            return Ok(parsed.iter().map(parse_result).collect());
        }
        Err(SymbologyError::RateLimited)
    }
}

#[async_trait]
impl FigiProvider for OpenFigiClient {
    async fn map(&self, jobs: &[MappingJob]) -> Result<Vec<MappingResult>, SymbologyError> {
        let mut out = Vec::with_capacity(jobs.len());
        for chunk in jobs.chunks(self.max_batch) {
            out.extend(self.map_chunk(chunk).await?);
        }
        Ok(out)
    }
}

fn job_to_json(j: &MappingJob) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("idType".into(), json!(j.id_type));
    m.insert("idValue".into(), json!(j.id_value));
    if let Some(v) = &j.exch_code {
        m.insert("exchCode".into(), json!(v));
    }
    if let Some(v) = &j.mic_code {
        m.insert("micCode".into(), json!(v));
    }
    if let Some(v) = &j.currency {
        m.insert("currency".into(), json!(v));
    }
    if let Some(v) = &j.market_sec_des {
        m.insert("marketSecDes".into(), json!(v));
    }
    Value::Object(m)
}

fn parse_result(v: &Value) -> MappingResult {
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        MappingResult::Data(arr.iter().map(parse_identity).collect())
    } else if let Some(w) = v.get("warning").and_then(|w| w.as_str()) {
        MappingResult::Warning(w.to_string())
    } else if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        MappingResult::Error(e.to_string())
    } else {
        MappingResult::Warning("unrecognized openfigi result".to_string())
    }
}

fn parse_identity(v: &Value) -> InstrumentIdentity {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    InstrumentIdentity {
        figi: s("figi").unwrap_or_default(),
        composite_figi: s("compositeFIGI"),
        share_class_figi: s("shareClassFIGI"),
        ticker: s("ticker"),
        name: s("name"),
        exch_code: s("exchCode"),
        security_type: s("securityType"),
        security_type2: s("securityType2"),
        market_sector: s("marketSector"),
    }
}
