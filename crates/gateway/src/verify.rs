//! How checking a credential against its venue before it is saved turns out.

use std::time::Duration;

/// Outcome of attempting to verify a credential before it is saved.
///
/// A plain `Result<(), String>` can only say "it worked" or "it didn't" — it
/// cannot also say "we didn't try", so a FIX credential would have to fake one
/// of the other two. Faking `Ok(())` is the dishonest "passed" the brief warns
/// against; faking `Err(_)` would show a real credential as broken before it
/// is even saved. A third variant is the only way to keep both meanings
/// distinct for the caller (task 3's `tested` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    /// The credential authenticated against the broker.
    Passed,
    /// The credential was tested and the broker rejected it (or the attempt
    /// failed for a network reason). `String` is a message for operators and
    /// logs — built from the failure, never from the submitted credential.
    Failed(String),
    /// Nothing was attempted. Reserved for credentials with no cheap,
    /// side-effect-free way to test.
    NotTestable(&'static str),
}

/// How long a credential test may spend talking to a provider before it is
/// reported as a failure.
///
/// Neither provider client sets a timeout of its own (Alpaca's is a bare
/// `reqwest::Client::new()`, which has none by default), so without this a
/// provider that completes the TCP handshake and then goes silent would hold
/// the admin request open indefinitely — and with it the cockpit's "Test"
/// button, which has no way to cancel. Ten seconds is far longer than either
/// check needs against a healthy provider.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The failure reported when a provider stops responding mid-test. Says which
/// provider and how long was waited, because "test failed" alone would be
/// indistinguishable from a rejected credential — the operator needs to know
/// their key was never actually judged.
pub fn timed_out(provider: &str) -> TestOutcome {
    TestOutcome::Failed(format!(
        "{provider} did not respond within {}s — credential not verified",
        TEST_TIMEOUT.as_secs()
    ))
}
