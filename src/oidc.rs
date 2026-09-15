//! ID-token verification.
//!
//! This is the security-critical half of OIDC login: given an ID token, a key
//! set, and the expectations for this login attempt, decide whether the token
//! proves who it claims to. Discovery and JWKS fetching/caching are a later
//! task's concern and live outside this module — `verify_id_token` takes its
//! keys as a plain argument and makes no network call, so it can be tested
//! against forged and malformed tokens with no IdP in the loop.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use openidconnect::core::{
    CoreIdToken, CoreIdTokenVerifier, CoreJsonWebKeySet as JsonWebKeySet, CoreJwsSigningAlgorithm,
};
use openidconnect::{
    ClaimsVerificationError, ClientId, IssuerUrl, Nonce, SignatureVerificationError,
};

/// What we require of a token before we'll trust it: the IdP that must have
/// issued it, the client (`aud`) it must have been minted for, the nonce this
/// login attempt handed out, and how much clock skew to tolerate against the
/// IdP's clock.
pub struct Expectations {
    pub issuer: String,
    pub audience: String,
    pub nonce: String,
    pub leeway: chrono::Duration,
}

/// What we trust about the human once `verify_id_token` succeeds.
///
/// Deliberately does not carry an access or refresh token — this crate never
/// stores or returns IdP tokens (see the human-identity design constraints).
/// `claims` is the raw decoded payload, kept around for a later task's
/// group/role gate.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedIdentity {
    pub subject: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub claims: serde_json::Value,
}

/// Why an ID token was refused. Each variant corresponds to a distinct,
/// real-world OIDC failure mode — see the task brief's test suite.
#[derive(Debug, PartialEq, Eq)]
pub enum OidcError {
    /// The signature does not check out against any key in the supplied set.
    Signature,
    /// `exp` (plus leeway) is in the past.
    Expired,
    /// `iss` does not match the expected issuer.
    WrongIssuer,
    /// `aud` does not contain the expected client id.
    WrongAudience,
    /// `nonce` does not match the value minted for this login.
    WrongNonce,
    /// The token is not a well-formed JWT (bad base64, bad JSON, wrong shape).
    Malformed,
    /// The token's signing algorithm is not one we allow — including `none`.
    /// This is decided from OUR allow-list, never from the token's own header.
    UnsupportedAlgorithm,
}

/// Verifies an OIDC ID token's signature and standard claims against
/// `expected`, as of `now`.
///
/// `keys` is the JWKS to verify the signature against — fetching and caching
/// that set is a different task's job. This function makes no network call.
pub fn verify_id_token(
    token: &str,
    keys: &JsonWebKeySet,
    expected: &Expectations,
    now: DateTime<Utc>,
) -> Result<VerifiedIdentity, OidcError> {
    let id_token: CoreIdToken = token.parse().map_err(|_| OidcError::Malformed)?;

    let issuer = IssuerUrl::new(expected.issuer.clone()).map_err(|_| OidcError::Malformed)?;
    let client_id = ClientId::new(expected.audience.clone());
    let leeway = expected.leeway;

    let verifier = CoreIdTokenVerifier::new_public_client(client_id, issuer, keys.clone())
        // Our allow-list, deliberately not derived from the token's own `alg`
        // header: an attacker controls that header, so trusting it would let
        // a forged `alg: none` token walk straight past signature checking.
        .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256])
        // `now` is a parameter (not `Utc::now()`) so tests control time
        // exactly. Folding `leeway` in here — rather than comparing it after
        // the fact — is what gives an ID token a grace window around `exp`.
        .set_time_fn(move || now - leeway);

    // `&Nonce` compares its digest against the claimed nonce's digest
    // (constant-time) rather than merely checking presence.
    let expected_nonce = Nonce::new(expected.nonce.clone());
    let claims = id_token
        .claims(&verifier, &expected_nonce)
        .map_err(map_claims_error)?;

    let subject = claims.subject().as_str().to_string();

    // The typed `claims` above only exposes the fields openidconnect models.
    // A later task's group/role gate needs the full payload, so decode it
    // independently here — safe to do only now that the signature and
    // standard claims above are verified.
    let raw_claims = decode_payload(token)?;
    let display_name = raw_claims
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let email = raw_claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(VerifiedIdentity {
        subject,
        display_name,
        email,
        claims: raw_claims,
    })
}

/// Decodes a JWT's payload segment as JSON, independent of any typed claims
/// struct. Does not touch the signature — callers must only use this on a
/// token that already verified.
fn decode_payload(token: &str) -> Result<serde_json::Value, OidcError> {
    let payload_b64 = token.split('.').nth(1).ok_or(OidcError::Malformed)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| OidcError::Malformed)?;
    serde_json::from_slice(&payload).map_err(|_| OidcError::Malformed)
}

fn map_claims_error(err: ClaimsVerificationError) -> OidcError {
    match err {
        ClaimsVerificationError::Expired(_) => OidcError::Expired,
        ClaimsVerificationError::InvalidIssuer(_) => OidcError::WrongIssuer,
        ClaimsVerificationError::InvalidAudience(_) => OidcError::WrongAudience,
        ClaimsVerificationError::InvalidNonce(_) => OidcError::WrongNonce,
        ClaimsVerificationError::SignatureVerification(sig_err) => map_signature_error(sig_err),
        // InvalidAuthContext, InvalidAuthTime, InvalidSubject, Other, Unsupported:
        // none of these are reachable for an ID token verified the way we
        // configure the verifier above (we set no acr/auth_time/subject
        // checks), but if the crate ever surfaces one, treat it as a
        // structurally-bad token rather than pretend it's one of the above.
        _ => OidcError::Malformed,
    }
}

fn map_signature_error(err: SignatureVerificationError) -> OidcError {
    match err {
        // `alg: none`, or an `alg` outside our allow-list: an algorithm
        // problem, not a bad-signature problem.
        SignatureVerificationError::NoSignature
        | SignatureVerificationError::DisallowedAlg(_)
        | SignatureVerificationError::UnsupportedAlg(_) => OidcError::UnsupportedAlgorithm,
        // No matching key, ambiguous key, wrong key type, or the signature
        // itself doesn't check out — plus whatever this `#[non_exhaustive]`
        // enum grows later: all mean "we don't trust this signature".
        _ => OidcError::Signature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mint a signing key and tokens in-process. No IdP, no network: the whole
    /// point of taking `keys` as an argument.
    fn signer() -> TestSigner {
        TestSigner::new()
    }

    fn expectations() -> Expectations {
        Expectations {
            issuer: "https://id.example.com".into(),
            audience: "oms".into(),
            nonce: "n-123".into(),
            leeway: chrono::Duration::seconds(60),
        }
    }

    #[test]
    fn a_well_formed_token_yields_its_subject() {
        let s = signer();
        let token = s.token_with(|c| c);

        let identity = verify_id_token(&token, &s.keys(), &expectations(), Utc::now())
            .expect("a valid token must verify");

        assert_eq!(identity.subject, "user-42");
    }

    #[test]
    fn a_token_signed_by_someone_else_is_refused() {
        let s = signer();
        let impostor = signer();
        let token = impostor.token_with(|c| c);

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::Signature)
        );
    }

    #[test]
    fn an_expired_token_is_refused_even_though_it_is_otherwise_perfect() {
        let s = signer();
        let token = s.token_with(|mut c| {
            c.exp = (Utc::now() - chrono::Duration::hours(1)).timestamp();
            c
        });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::Expired)
        );
    }

    #[test]
    fn a_token_minted_for_another_application_is_refused() {
        let s = signer();
        let token = s.token_with(|mut c| {
            c.aud = "some-other-app".into();
            c
        });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::WrongAudience)
        );
    }

    #[test]
    fn a_token_from_another_issuer_is_refused() {
        let s = signer();
        let token = s.token_with(|mut c| {
            c.iss = "https://evil.example.com".into();
            c
        });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::WrongIssuer)
        );
    }

    #[test]
    fn a_replayed_token_from_a_different_login_is_refused() {
        let s = signer();
        let token = s.token_with(|mut c| {
            c.nonce = "some-other-nonce".into();
            c
        });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::WrongNonce)
        );
    }

    #[test]
    fn an_unsigned_token_is_refused_however_convincing_its_claims() {
        let s = signer();
        let token = s.unsigned_token();

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn a_token_within_clock_leeway_still_verifies() {
        let s = signer();
        let token = s.token_with(|mut c| {
            c.exp = (Utc::now() - chrono::Duration::seconds(30)).timestamp();
            c
        });

        assert!(verify_id_token(&token, &s.keys(), &expectations(), Utc::now()).is_ok());
    }

    // --- TestSigner -----------------------------------------------------
    //
    // Mints RSA-signed ID tokens in-process, using the same key and JWT types
    // (`openidconnect::core::CoreIdToken` / `CoreIdTokenClaims`) that
    // `verify_id_token` verifies, so the test signs what production checks.
    // `openidconnect`'s own JWT machinery is private to that crate, so the
    // one thing we can't build through its public API is a token with no
    // signature at all (`IdToken::new` always signs) — `unsigned_token`
    // therefore assembles that one adversarial case by hand.

    use openidconnect::core::{CoreIdTokenClaims, CoreJsonWebKeySet, CoreRsaPrivateSigningKey};
    use openidconnect::{
        Audience, EmptyAdditionalClaims, PrivateSigningKey, StandardClaims, SubjectIdentifier,
    };

    /// The claims of a token under test, in plain field form so `token_with`
    /// closures can mutate individual claims directly (mirroring how an
    /// attacker would tamper with one field of an otherwise-legitimate
    /// token).
    #[derive(Clone)]
    struct Claims {
        iss: String,
        aud: String,
        sub: String,
        exp: i64,
        iat: i64,
        nonce: String,
    }

    impl Default for Claims {
        fn default() -> Self {
            let now = Utc::now();
            Claims {
                iss: "https://id.example.com".into(),
                aud: "oms".into(),
                sub: "user-42".into(),
                exp: (now + chrono::Duration::minutes(5)).timestamp(),
                iat: now.timestamp(),
                nonce: "n-123".into(),
            }
        }
    }

    struct TestSigner {
        signing_key: CoreRsaPrivateSigningKey,
    }

    impl TestSigner {
        fn new() -> Self {
            use rsa::pkcs1::EncodeRsaPrivateKey;

            let mut rng = rand::rngs::OsRng;
            let key_pair = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("generate RSA key");
            let pem = key_pair
                .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
                .expect("encode RSA key as PKCS#1 PEM");

            let signing_key =
                CoreRsaPrivateSigningKey::from_pem(&pem, None).expect("build signing key from PEM");

            TestSigner { signing_key }
        }

        fn keys(&self) -> CoreJsonWebKeySet {
            CoreJsonWebKeySet::new(vec![self.signing_key.as_verification_key()])
        }

        /// Builds a signed token whose claims `f` may mutate, starting from a
        /// set of claims that verifies cleanly against `expectations()`.
        fn token_with(&self, f: impl FnOnce(Claims) -> Claims) -> String {
            let c = f(Claims::default());

            let claims = CoreIdTokenClaims::new(
                IssuerUrl::new(c.iss).expect("valid issuer URL"),
                vec![Audience::new(c.aud)],
                DateTime::from_timestamp(c.exp, 0).expect("valid exp"),
                DateTime::from_timestamp(c.iat, 0).expect("valid iat"),
                StandardClaims::new(SubjectIdentifier::new(c.sub)),
                EmptyAdditionalClaims {},
            )
            .set_nonce(Some(Nonce::new(c.nonce)));

            CoreIdToken::new(
                claims,
                &self.signing_key,
                CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
                None,
                None,
            )
            .expect("sign token")
            .to_string()
        }

        /// A token with header `{"alg":"none"}` and an empty signature —
        /// `openidconnect`'s public API has no way to produce this (signing
        /// is mandatory through `IdToken::new`), so it's assembled by hand.
        fn unsigned_token(&self) -> String {
            let c = Claims::default();
            let header = serde_json::json!({ "alg": "none" });
            let payload = serde_json::json!({
                "iss": c.iss,
                "aud": c.aud,
                "sub": c.sub,
                "exp": c.exp,
                "iat": c.iat,
                "nonce": c.nonce,
            });

            let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string());
            let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string());

            format!("{header_b64}.{payload_b64}.")
        }
    }
}
