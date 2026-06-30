/// Whatever identifiers you have about an instrument. Fed to `Identifier::identify`.
/// Used as a cache key, so it derives `Hash`/`Eq`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct InstrumentQuery {
    pub figi: Option<String>,
    pub isin: Option<String>,
    pub cusip: Option<String>,
    pub ticker: Option<String>,
    /// ISO 10383 MIC of the listing venue.
    pub mic: Option<String>,
    /// OpenFIGI exchange code (e.g. "US", "LN"). Alternative to `mic`.
    pub exch_code: Option<String>,
    pub currency: Option<String>,
    /// OpenFIGI market sector description, e.g. "Equity".
    pub market_sec_des: Option<String>,
}

impl InstrumentQuery {
    pub fn ticker(t: impl Into<String>) -> Self {
        Self { ticker: Some(t.into()), ..Default::default() }
    }
    pub fn isin(v: impl Into<String>) -> Self {
        Self { isin: Some(v.into()), ..Default::default() }
    }
    pub fn cusip(v: impl Into<String>) -> Self {
        Self { cusip: Some(v.into()), ..Default::default() }
    }
    pub fn figi(v: impl Into<String>) -> Self {
        Self { figi: Some(v.into()), ..Default::default() }
    }

    pub fn exch(mut self, v: impl Into<String>) -> Self {
        self.exch_code = Some(v.into());
        self
    }
    pub fn mic(mut self, v: impl Into<String>) -> Self {
        self.mic = Some(v.into());
        self
    }
    pub fn currency(mut self, v: impl Into<String>) -> Self {
        self.currency = Some(v.into());
        self
    }
    pub fn market_sec_des(mut self, v: impl Into<String>) -> Self {
        self.market_sec_des = Some(v.into());
        self
    }
}
