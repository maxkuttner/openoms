use thiserror::Error;

#[derive(Debug, Error)]
pub enum SymbologyError {
    #[error("network error: {0}")]
    Network(String),
    #[error("openfigi returned HTTP {0}")]
    Http(u16),
    #[error("failed to decode openfigi response: {0}")]
    Decode(String),
    #[error("query carries no usable identifier")]
    NoIdentifier,
    #[error("rate limited by openfigi")]
    RateLimited,
}
