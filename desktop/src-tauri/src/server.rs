//! Validate a user-typed server address, probe it, and classify the result.
//!
//! This module is the whole testable surface behind the connection page:
//! turning a typed address into a normalised URL or a `BadScheme` refusal,
//! and turning a `/health` probe's outcome into one of five distinct,
//! user-facing failures (or success). The `Probe` trait is the seam that
//! lets `classify`/`probe` be exercised without a network — the real
//! implementation, `ReqwestProbe`, wraps `reqwest` with a 5 second timeout.

use std::time::Duration;

/// Why a candidate server address could not be turned into a usable
/// connection. Each variant has its own user-facing message — a trader who
/// cannot tell a typo from a dead server wastes an afternoon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The address was empty, unparseable, or did not start with `http://`
    /// or `https://`.
    BadScheme,
    /// The TCP connection could not be established (refused, DNS failure,
    /// host down).
    Unreachable,
    /// The TLS handshake failed.
    Tls,
    /// Something answered, but not with a 200 from `/health`.
    NotAnOms,
    /// No response was received within the probe's timeout.
    Timeout,
}

impl ConnectError {
    /// The exact string shown to the trader for this failure.
    pub fn message(&self) -> &'static str {
        match self {
            ConnectError::BadScheme => "Enter a full address starting with https://",
            ConnectError::Unreachable => "Can't reach that address",
            ConnectError::Tls => "Secure connection failed",
            ConnectError::NotAnOms => "Reachable, but that doesn't look like an OMS",
            ConnectError::Timeout => "No response — the server may be starting up",
        }
    }
}

/// The way a `Probe` implementation failed to get a status code at all.
/// Distinct from `ConnectError`: this is the transport-level outcome
/// reported by whatever performs the HTTP call, before `classify` turns it
/// into a user-facing `ConnectError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFailure {
    Timeout,
    Tls,
    Connect,
}

/// Trim whitespace, strip trailing slashes, and require an explicit
/// `http://` or `https://` scheme.
///
/// This never guesses a scheme: silently prefixing `https://` onto a bare
/// hostname would point a session cookie at a server the user never named.
pub fn normalise(raw: &str) -> Result<String, ConnectError> {
    let trimmed = raw.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(ConnectError::BadScheme);
    }
    let without_trailing_slashes = trimmed.trim_end_matches('/');
    if without_trailing_slashes.is_empty() {
        return Err(ConnectError::BadScheme);
    }
    Ok(without_trailing_slashes.to_string())
}

/// Turn a probe's raw outcome (an HTTP status, or a `ProbeFailure`) into a
/// user-facing `ConnectError`, or success. A 200 is the only success — a
/// real OMS answers `/health` unauthenticated, so even a 401 means
/// something else is there.
pub fn classify(result: Result<u16, ProbeFailure>) -> Result<(), ConnectError> {
    match result {
        Ok(200) => Ok(()),
        Ok(_) => Err(ConnectError::NotAnOms),
        Err(ProbeFailure::Timeout) => Err(ConnectError::Timeout),
        Err(ProbeFailure::Tls) => Err(ConnectError::Tls),
        Err(ProbeFailure::Connect) => Err(ConnectError::Unreachable),
    }
}

/// Performs the actual HTTP call behind a probe. Implemented by
/// `ReqwestProbe` for real use, and by test doubles to exercise
/// `classify`/`probe` without a network.
#[async_trait::async_trait]
pub trait Probe: Sync {
    async fn get_status(&self, url: &str) -> Result<u16, ProbeFailure>;
}

/// Ask `{url}/health` via the injected `Probe`, and classify what comes
/// back.
pub async fn probe(url: &str, http: &dyn Probe) -> Result<(), ConnectError> {
    let health_url = format!("{url}/health");
    classify(http.get_status(&health_url).await)
}

/// The real `Probe`, backed by `reqwest` with a 5 second timeout.
pub struct ReqwestProbe {
    client: reqwest::Client,
}

impl ReqwestProbe {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client with rustls-tls builds"),
        }
    }
}

impl Default for ReqwestProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Probe for ReqwestProbe {
    async fn get_status(&self, url: &str) -> Result<u16, ProbeFailure> {
        match self.client.get(url).send().await {
            Ok(response) => Ok(response.status().as_u16()),
            Err(err) if err.is_timeout() => Err(ProbeFailure::Timeout),
            Err(err) if err.is_connect() => {
                // reqwest folds TLS handshake failures into "connect" errors;
                // the underlying source chain still says "tls" for those, so
                // distinguish on that rather than misreporting every TLS
                // failure as a plain connection refusal.
                if source_chain_mentions_tls(&err) {
                    Err(ProbeFailure::Tls)
                } else {
                    Err(ProbeFailure::Connect)
                }
            }
            Err(_) => Err(ProbeFailure::Connect),
        }
    }
}

fn source_chain_mentions_tls(err: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let text = cause.to_string().to_lowercase();
        if text.contains("tls") || text.contains("certificate") || text.contains("ssl") {
            return true;
        }
        source = cause.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_host_and_port_is_accepted_and_left_alone() {
        assert_eq!(normalise("https://oms.example.com").unwrap(), "https://oms.example.com");
        assert_eq!(normalise("http://localhost:3001").unwrap(), "http://localhost:3001");
    }

    #[test]
    fn surrounding_whitespace_and_a_trailing_slash_are_removed() {
        // Both are what a human produces when pasting from a browser bar.
        assert_eq!(normalise("  https://oms.example.com/  ").unwrap(), "https://oms.example.com");
        assert_eq!(normalise("https://oms.example.com///").unwrap(), "https://oms.example.com");
    }

    #[test]
    fn a_missing_or_wrong_scheme_is_refused() {
        // Refused rather than guessed: silently prefixing https:// would send a
        // session cookie somewhere the user did not name.
        assert!(matches!(normalise("oms.example.com"), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise("ftp://oms.example.com"), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise("file:///etc/passwd"), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise(""), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise("   "), Err(ConnectError::BadScheme)));
    }

    #[test]
    fn every_failure_says_something_different() {
        // The point of the enum: five distinct causes, five distinct messages.
        // A trader who cannot tell a typo from a dead server wastes an afternoon.
        let all = [
            ConnectError::BadScheme,
            ConnectError::Unreachable,
            ConnectError::Tls,
            ConnectError::NotAnOms,
            ConnectError::Timeout,
        ];
        let mut seen = std::collections::HashSet::new();
        for e in &all {
            assert!(!e.message().is_empty(), "{e:?} has no message");
            assert!(seen.insert(e.message()), "{e:?} reuses another variant's message");
        }
    }

    #[test]
    fn a_200_is_the_only_success() {
        assert!(classify(Ok(200)).is_ok());
        assert!(matches!(classify(Ok(404)), Err(ConnectError::NotAnOms)));
        assert!(matches!(classify(Ok(500)), Err(ConnectError::NotAnOms)));
        // A 401 means something is there and answering, but /health is
        // unauthenticated on a real OMS — so this is not one.
        assert!(matches!(classify(Ok(401)), Err(ConnectError::NotAnOms)));
    }

    #[test]
    fn transport_failures_keep_their_identity() {
        assert!(matches!(classify(Err(ProbeFailure::Timeout)), Err(ConnectError::Timeout)));
        assert!(matches!(classify(Err(ProbeFailure::Tls)), Err(ConnectError::Tls)));
        assert!(matches!(classify(Err(ProbeFailure::Connect)), Err(ConnectError::Unreachable)));
    }

    #[tokio::test]
    async fn probe_reports_what_the_injected_client_saw() {
        struct Always(Result<u16, ProbeFailure>);
        #[async_trait::async_trait]
        impl Probe for Always {
            async fn get_status(&self, _url: &str) -> Result<u16, ProbeFailure> {
                self.0
            }
        }

        assert!(probe("https://oms.example.com", &Always(Ok(200))).await.is_ok());
        assert!(matches!(
            probe("https://oms.example.com", &Always(Err(ProbeFailure::Timeout))).await,
            Err(ConnectError::Timeout)
        ));
    }

    #[tokio::test]
    async fn the_probe_asks_for_health_on_the_given_base() {
        // The URL the probe builds is the contract with the server; a typo here
        // turns every connection attempt into "that doesn't look like an OMS".
        struct Recorder(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Probe for Recorder {
            async fn get_status(&self, url: &str) -> Result<u16, ProbeFailure> {
                self.0.lock().unwrap().push(url.to_string());
                Ok(200)
            }
        }

        let rec = Recorder(Default::default());
        probe("https://oms.example.com", &rec).await.unwrap();
        assert_eq!(rec.0.lock().unwrap().as_slice(), ["https://oms.example.com/health"]);
    }
}
