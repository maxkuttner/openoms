//! The base provider identity trait.
//!
//! Every data source (universe source, quote source, …) is first a
//! [`DataProvider`] with a stable `code()`. Capability traits such as
//! [`crate::UniverseSource`] extend it, so a provider opts into exactly the
//! capabilities it supports without a single god-trait.

/// A named data provider. The `code()` is stamped into
/// `oms.instrument_xref.source_code` and used in cost tables / logs.
pub trait DataProvider: Send + Sync {
    /// Stable provider code, e.g. `"DATABENTO"`.
    fn code(&self) -> &'static str;
}
