//! Shared error type for the data-provider abstraction.

use thiserror::Error;

/// Error surface common to every provider and enricher. Concrete impls convert
/// their vendor SDK errors into these variants.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider config error: {0}")]
    Config(String),
    #[error("provider request failed: {0}")]
    Request(String),
    #[error("provider returned no data")]
    NoData,
}
