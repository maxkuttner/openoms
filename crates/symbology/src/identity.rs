use serde::{Deserialize, Serialize};

/// A canonical instrument identity anchored on FIGI (from OpenFIGI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentIdentity {
    pub figi: String,
    /// The composite (primary-listing) FIGI; equals `figi` for the composite itself.
    pub composite_figi: Option<String>,
    /// Share-class FIGI — same across all listings of the share class.
    pub share_class_figi: Option<String>,
    pub ticker: Option<String>,
    pub name: Option<String>,
    /// OpenFIGI exchange code.
    pub exch_code: Option<String>,
    pub security_type: Option<String>,
    pub security_type2: Option<String>,
    pub market_sector: Option<String>,
}

/// The outcome of an identification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    /// Exactly one identity (or one chosen by the disambiguation policy).
    Resolved(InstrumentIdentity),
    /// Multiple candidates the policy could not collapse.
    Ambiguous(Vec<InstrumentIdentity>),
    /// No match (OpenFIGI warning) or nothing mappable.
    NotFound,
}
