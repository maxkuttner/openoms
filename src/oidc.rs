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
    CoreProviderMetadata,
};
use openidconnect::{
    ClaimsVerificationError, ClientId, IssuerUrl, JsonWebKeySetUrl, Nonce,
    SignatureVerificationError,
};

use crate::config::OidcSettings;

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
    /// Discovery, a JWKS fetch, or the token-endpoint exchange itself failed —
    /// a transport or configuration problem talking to the IdP, not a
    /// judgment about whether a token is trustworthy. Carries a message for
    /// logs; never populated from response bodies that could hold a token.
    ProviderUnavailable(String),
}

/// The signing algorithms an ID token is allowed to use.
///
/// The standard asymmetric set: RSASSA-PKCS1-v1_5, RSASSA-PSS and ECDSA, each
/// over SHA-256/384/512. RS256 alone was too narrow — an IdP configured for
/// ES256 or PS256, both ordinary choices, failed every login with
/// `UnsupportedAlgorithm`, which reads in the log like an attack rather than a
/// configuration mismatch.
///
/// **The symmetric HMAC algorithms (HS256/384/512) are deliberately absent, and
/// must stay absent.** Their key is the client secret, and an attacker who
/// re-signs a token as HS256 using the RSA *public* key bytes as that secret
/// defeats a verifier that takes the algorithm from the token's own header —
/// the classic RS256→HS256 key-confusion attack, pinned by
/// `an_algorithm_confusion_token_is_refused`. `none` is absent for the same
/// reason. Neither belongs in an allow-list that exists precisely so the
/// token's own `alg` header never gets a vote.
const ALLOWED_ALGS: [CoreJwsSigningAlgorithm; 9] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::RsaSsaPssSha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::EcdsaP521Sha512,
];

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
        // See `ALLOWED_ALGS` for what is in it and what must never be.
        .set_allowed_algs(ALLOWED_ALGS.clone())
        // `now` is a parameter (not `Utc::now()`) so tests control time
        // exactly. Folding `leeway` in here — rather than comparing it after
        // the fact — is what gives an ID token a grace window around `exp`.
        //
        // This shift is only safe because `exp` is the *only* time-based
        // check this verifier performs: the crate's `iat` and `auth_time`
        // verifiers default to no-ops here, and there is no `nbf` claim in
        // OIDC ID tokens. If a future change calls
        // `set_issue_time_verifier_fn`, `set_max_age`, or
        // `set_auth_time_verifier_fn` on this same verifier, shifting `now`
        // would silently loosen those checks too — that coupling would not
        // be visible at the call site making the change.
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

/// The JWKS cache: the keys as last fetched, and when — the latter is what
/// enforces the once-a-minute refetch cap.
struct CachedKeys {
    keys: JsonWebKeySet,
    fetched_at: DateTime<Utc>,
}

/// How long an unrecognised `kid` is allowed to trigger a JWKS refetch. Below
/// this, a token history of unknown key ids just verifies against whatever is
/// cached (and fails) rather than hitting the IdP again — a malformed or
/// adversarial token must not be able to drive a fetch storm.
const JWKS_REFETCH_INTERVAL: chrono::Duration = chrono::Duration::seconds(60);

/// True for hosts reachable only from this machine — the one case
/// `refuse_insecure_endpoint` tolerates an `http://` discovered endpoint for,
/// so a local Keycloak run entirely on loopback still works without TLS.
///
/// `host` is `url::Url::host_str()`'s output, which brackets an IPv6 literal
/// (`"[::1]"`, not `"::1"`) — unlike `setup::database::config::is_loopback_host`,
/// which parses a bare `host:port` pair and so never sees brackets.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Refuses a discovered endpoint that is neither `https://` nor loopback
/// `http://`.
///
/// `authorization_endpoint` and `token_endpoint` come straight off the
/// provider's own discovery document (`CoreProviderMetadata`), which applies
/// no scheme check of its own. A misconfigured — or compromised — IdP
/// advertising `http://` for the token endpoint would put the client secret
/// on the wire in clear text on every code exchange; for the authorization
/// endpoint, it would send the PKCE challenge and the browser redirect over
/// an interceptable channel. Loopback is the one deliberate exception, so a
/// local Keycloak in development still works without standing up TLS for it.
fn refuse_insecure_endpoint(url: &str, label: &str) -> Result<(), OidcError> {
    let parsed = openidconnect::url::Url::parse(url).map_err(|e| {
        OidcError::ProviderUnavailable(format!("{label} endpoint is not a valid URL: {e}"))
    })?;
    let is_loopback = parsed.host_str().is_some_and(is_loopback_host);
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_loopback => Ok(()),
        scheme => Err(OidcError::ProviderUnavailable(format!(
            "{label} endpoint advertised by the provider is not https:// and not loopback \
             (scheme {scheme:?}); refusing to trust it: {url}"
        ))),
    }
}

/// A configured connection to one OIDC identity provider: its discovered (or,
/// in tests, pre-baked) endpoints, plus a cached JWKS refreshed on demand.
///
/// Holds the client secret and talks to the IdP directly — `verify_id_token`
/// remains the one place that decides whether a token is trustworthy; this
/// type's job is only to get a token and the keys to check it with.
pub struct Provider {
    settings: OidcSettings,
    client_secret: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    http_client: openidconnect::reqwest::Client,
    keys: tokio::sync::RwLock<CachedKeys>,
}

impl Provider {
    /// Discovers `settings.issuer`'s metadata (`{issuer}/.well-known/openid-configuration`)
    /// and fetches its JWKS once, up front, so the first login attempt after
    /// startup doesn't pay for a cold cache.
    pub async fn discover(settings: OidcSettings, client_secret: String) -> Result<Provider, OidcError> {
        let http_client = build_http_client()?;

        let issuer = IssuerUrl::new(settings.issuer.clone())
            .map_err(|e| OidcError::ProviderUnavailable(format!("invalid issuer URL: {e}")))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
            .await
            .map_err(|e| OidcError::ProviderUnavailable(format!("OIDC discovery failed: {e}")))?;

        let authorization_endpoint = metadata.authorization_endpoint().to_string();
        refuse_insecure_endpoint(&authorization_endpoint, "authorization")?;
        let token_endpoint = metadata
            .token_endpoint()
            .ok_or_else(|| {
                OidcError::ProviderUnavailable("provider metadata has no token_endpoint".into())
            })?
            .to_string();
        // The client secret rides on this exact request (`request_id_token`'s
        // token-endpoint POST) — an `http://` token endpoint would put it on
        // the wire in clear text. Checked before it is ever used, not just
        // before it is stored.
        refuse_insecure_endpoint(&token_endpoint, "token")?;
        let jwks_uri = metadata.jwks_uri().clone();
        // The worst of the three to leave unguarded: an attacker who can
        // substitute the key set served from here can sign arbitrary ID
        // tokens that then pass `verify_id_token` — every other hardening in
        // the verifier becomes irrelevant once the keys themselves are
        // attacker-controlled.
        refuse_insecure_endpoint(jwks_uri.as_str(), "jwks")?;

        let keys = JsonWebKeySet::fetch_async(&jwks_uri, &http_client)
            .await
            .map_err(|e| OidcError::ProviderUnavailable(format!("failed to fetch JWKS: {e}")))?;

        Ok(Provider {
            settings,
            client_secret,
            authorization_endpoint,
            token_endpoint,
            jwks_uri: jwks_uri.to_string(),
            http_client,
            keys: tokio::sync::RwLock::new(CachedKeys { keys, fetched_at: Utc::now() }),
        })
    }

    /// A `Provider` built from pre-baked endpoints, with no discovery and no
    /// network call — for tests. `issuer` is used verbatim to derive fake
    /// `/authorize`, `/token` and `/jwks` endpoints the way a real IdP would
    /// lay them out under its issuer URL.
    #[cfg(test)]
    pub fn for_test(issuer: &str, client_id: &str, public_base_url: &str) -> Provider {
        Provider {
            settings: OidcSettings {
                issuer: issuer.to_string(),
                client_id: client_id.to_string(),
                public_base_url: public_base_url.to_string(),
                scopes: vec!["openid".to_string()],
                required_claim: None,
                ttl: crate::sessions::SessionTtl::default(),
            },
            client_secret: "test-secret".to_string(),
            authorization_endpoint: format!("{issuer}/authorize"),
            token_endpoint: format!("{issuer}/token"),
            jwks_uri: format!("{issuer}/jwks"),
            http_client: build_http_client().expect("building a client touches no network"),
            keys: tokio::sync::RwLock::new(CachedKeys {
                keys: JsonWebKeySet::new(Vec::new()),
                fetched_at: Utc::now(),
            }),
        }
    }

    /// `{public_base_url}/auth/callback`, tolerating a trailing slash on the
    /// configured base URL — this is the one place that derivation happens,
    /// so the authorization URL and the real callback route can never drift
    /// apart.
    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.settings.public_base_url.trim_end_matches('/'))
    }

    /// The URL to send the browser to in order to start a login. `state` and
    /// `nonce` are minted by the caller (and must be remembered against the
    /// pending login to check on callback); `pkce_challenge` is the S256
    /// challenge derived from a verifier the caller also holds onto. PKCE
    /// method is hard-coded to S256 — `plain` is never offered.
    pub fn authorize_url(&self, state: &str, nonce: &str, pkce_challenge: &str) -> String {
        let mut url = openidconnect::url::Url::parse(&self.authorization_endpoint)
            .expect("authorization_endpoint was validated at discovery or for_test construction");

        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.settings.client_id)
            .append_pair("redirect_uri", &self.redirect_uri())
            .append_pair("scope", &self.settings.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256");

        url.to_string()
    }

    /// Exchanges an authorization code for a verified identity. `pkce_verifier`
    /// must match the challenge given to `authorize_url`; `nonce` must match
    /// the one minted for this login. Only the resulting `VerifiedIdentity` is
    /// returned — the IdP's access and refresh tokens are discarded and never
    /// stored, per the human-identity design constraint that sessions are
    /// ours, not the IdP's.
    pub async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        nonce: &str,
    ) -> Result<VerifiedIdentity, OidcError> {
        let token = self.request_id_token(code, pkce_verifier).await?;

        let expected = Expectations {
            issuer: self.settings.issuer.clone(),
            audience: self.settings.client_id.clone(),
            nonce: nonce.to_string(),
            leeway: chrono::Duration::seconds(60),
        };

        self.verify_with_cache(&token, &expected).await
    }

    /// POSTs the authorization-code grant to the token endpoint and pulls out
    /// just the `id_token`. Any `access_token` / `refresh_token` in the
    /// response is dropped on the floor right here — this function's return
    /// type has no room to carry them further even by accident.
    async fn request_id_token(&self, code: &str, pkce_verifier: &str) -> Result<String, OidcError> {
        let redirect_uri = self.redirect_uri();
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", self.settings.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("code_verifier", pkce_verifier),
        ];

        let response = self
            .http_client
            .post(&self.token_endpoint)
            .form(&params)
            .send()
            .await
            .map_err(|e| OidcError::ProviderUnavailable(format!("token request failed: {e}")))?;

        if !response.status().is_success() {
            return Err(OidcError::ProviderUnavailable(format!(
                "token endpoint returned {}",
                response.status()
            )));
        }

        // `.text()` + `serde_json::from_str` rather than `.json()`: the
        // latter needs `reqwest`'s `json` feature, which `openidconnect`
        // doesn't enable on the `reqwest` 0.12 it pulls in, and there's no
        // reason to widen that dependency just for this.
        let text = response
            .text()
            .await
            .map_err(|e| OidcError::ProviderUnavailable(format!("failed to read token response: {e}")))?;
        let body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| OidcError::ProviderUnavailable(format!("malformed token response: {e}")))?;

        body.get("id_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| OidcError::ProviderUnavailable("token response has no id_token".into()))
    }

    /// Verifies `token` against the cached JWKS, refreshing that cache first
    /// if the token names a `kid` we don't currently hold — rate-limited to
    /// `JWKS_REFETCH_INTERVAL` so a stream of tokens with bogus or unknown
    /// `kid`s can't drive a fetch storm against the IdP.
    async fn verify_with_cache(
        &self,
        token: &str,
        expected: &Expectations,
    ) -> Result<VerifiedIdentity, OidcError> {
        self.refresh_keys_if_unknown_kid(token).await;

        let cache = self.keys.read().await;
        verify_id_token(token, &cache.keys, expected, Utc::now())
    }

    async fn refresh_keys_if_unknown_kid(&self, token: &str) {
        let Some(kid) = token_kid(token) else {
            // No `kid` in the header at all: nothing to look up by, so there
            // is nothing a refetch could fix. Let verification proceed (and
            // fail on its own terms) against whatever is cached.
            return;
        };

        if self.has_key(&kid).await {
            return;
        }

        let mut cache = self.keys.write().await;
        // Re-check under the write lock: another concurrent login may have
        // already refreshed the cache while we were waiting for it.
        if cache.keys.keys().iter().any(|k| key_id_matches(k, &kid)) {
            return;
        }
        if Utc::now() - cache.fetched_at < JWKS_REFETCH_INTERVAL {
            // Rate-limited: a malformed/adversarial token with an unknown
            // `kid` must not be able to force a fetch on every attempt.
            return;
        }

        if let Ok(url) = JsonWebKeySetUrl::new(self.jwks_uri.clone()) {
            if let Ok(fresh) = JsonWebKeySet::fetch_async(&url, &self.http_client).await {
                cache.keys = fresh;
            }
        }
        // The window resets whether the fetch above succeeded or not — a
        // failing IdP shouldn't be hammered every time a token comes in
        // either.
        cache.fetched_at = Utc::now();
    }

    async fn has_key(&self, kid: &str) -> bool {
        let cache = self.keys.read().await;
        cache.keys.keys().iter().any(|k| key_id_matches(k, kid))
    }
}

fn key_id_matches(key: &openidconnect::core::CoreJsonWebKey, kid: &str) -> bool {
    use openidconnect::JsonWebKey as _;
    key.key_id().map(|id| id.as_str()) == Some(kid)
}

/// Builds the `reqwest` client used for discovery, JWKS fetches and the token
/// exchange. This is `openidconnect`'s own re-export of `reqwest` (currently
/// 0.12.x, distinct from this crate's direct `reqwest` 0.13 dependency used
/// elsewhere) — its `AsyncHttpClient` impl is only defined for that exact
/// type, so fighting to reuse our 0.13 client would just mean hand-rolling
/// the trait impl for no benefit.
fn build_http_client() -> Result<openidconnect::reqwest::Client, OidcError> {
    openidconnect::reqwest::ClientBuilder::new()
        // Following redirects here would let a malicious or compromised IdP
        // response redirect these requests anywhere (SSRF).
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| OidcError::ProviderUnavailable(format!("failed to build HTTP client: {e}")))
}

/// Reads the `kid` header field without verifying anything — used only to
/// decide whether the JWKS cache needs a refetch before real verification is
/// attempted. A missing or malformed header just means "don't refetch";
/// `verify_id_token` is the sole authority on whether the token is valid.
fn token_kid(token: &str) -> Option<String> {
    let header_b64 = token.split('.').next()?;
    let header_bytes = URL_SAFE_NO_PAD.decode(header_b64).ok()?;
    let header: serde_json::Value = serde_json::from_slice(&header_bytes).ok()?;
    header.get("kid")?.as_str().map(String::from)
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
    fn a_well_formed_token_yields_its_subject_and_profile_claims() {
        let s = signer();
        let token = s.token_with(|c| c);

        let identity = verify_id_token(&token, &s.keys(), &expectations(), Utc::now())
            .expect("a valid token must verify");

        assert_eq!(identity.subject, "user-42");
        assert_eq!(identity.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn a_token_without_name_or_email_still_verifies_with_none() {
        let s = signer();
        let token = s.token_with(|mut c| {
            c.name = None;
            c.email = None;
            c
        });

        let identity = verify_id_token(&token, &s.keys(), &expectations(), Utc::now())
            .expect("name and email are optional OIDC claims, not required ones");

        assert_eq!(identity.subject, "user-42");
        assert_eq!(identity.display_name, None);
        assert_eq!(identity.email, None);
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

    /// The allow-list by its JWA names, which is how an operator reads it out
    /// of their IdP's configuration. RS256-only turned an ES256 or PS256
    /// provider into a login that fails every time.
    #[test]
    fn the_allow_list_is_the_standard_asymmetric_set() {
        let names: Vec<String> = ALLOWED_ALGS
            .iter()
            .map(|a| serde_json::to_value(a).expect("serialize alg").as_str().unwrap().to_string())
            .collect();

        assert_eq!(
            names,
            ["RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "ES512"]
        );
    }

    /// The exclusion that defeats key confusion. Widening the allow-list must
    /// never widen it to a symmetric algorithm — or to `none`, whose whole
    /// point is having no signature at all.
    #[test]
    fn no_symmetric_algorithm_or_none_is_ever_allowed() {
        for forbidden in [
            CoreJwsSigningAlgorithm::HmacSha256,
            CoreJwsSigningAlgorithm::HmacSha384,
            CoreJwsSigningAlgorithm::HmacSha512,
            CoreJwsSigningAlgorithm::None,
        ] {
            assert!(
                !ALLOWED_ALGS.contains(&forbidden),
                "{forbidden:?} must never be accepted for an ID token"
            );
        }
    }

    #[test]
    fn an_algorithm_confusion_token_is_refused() {
        // The classic RS256→HS256 attack: forge a token with `alg: HS256`,
        // HMAC-signed using the RSA *public* key's bytes as the shared
        // secret. A verifier that blindly looks up "the key for this token"
        // and hands it to whatever primitive `alg` names would accept this,
        // since the RSA public key is, by design, public. Our allow-list
        // (`set_allowed_algs`) refuses it on its own; the crate's own
        // refusal of symmetric algorithms for public clients would refuse it
        // even if that allow-list were absent.
        let s = signer();
        let token = s.confusion_token();

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

    #[test]
    fn the_authorize_url_carries_everything_the_provider_needs() {
        let provider = Provider::for_test("https://id.example.com", "oms", "https://oms.example.com");

        let url = provider.authorize_url("st-1", "n-1", "challenge-1");

        assert!(url.starts_with("https://id.example.com/authorize"));
        assert!(url.contains("client_id=oms"));
        assert!(url.contains("state=st-1"));
        assert!(url.contains("nonce=n-1"));
        assert!(url.contains("code_challenge=challenge-1"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Foms.example.com%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid"));
    }

    #[test]
    fn the_redirect_uri_is_derived_from_one_configured_value() {
        let provider = Provider::for_test("https://id.example.com", "oms", "https://oms.example.com/");

        assert_eq!(provider.redirect_uri(), "https://oms.example.com/auth/callback");
    }

    /// `refuse_insecure_endpoint` is called identically for all three
    /// discovered endpoints (`discover`'s `authorization`/`token`/`jwks`
    /// call sites) — exercised here by label so a future call site that
    /// forgets the check is the only way any of the three could go
    /// unguarded, not a gap in what's tested.
    const ENDPOINT_LABELS: [&str; 3] = ["authorization", "token", "jwks"];

    #[test]
    fn an_https_endpoint_is_always_trusted() {
        for label in ENDPOINT_LABELS {
            assert!(refuse_insecure_endpoint("https://id.example.com/x", label).is_ok());
        }
    }

    #[test]
    fn an_http_endpoint_on_loopback_is_tolerated_for_local_development() {
        for label in ENDPOINT_LABELS {
            assert!(refuse_insecure_endpoint("http://localhost:8080/x", label).is_ok());
            assert!(refuse_insecure_endpoint("http://127.0.0.1:8080/x", label).is_ok());
            assert!(refuse_insecure_endpoint("http://[::1]:8080/x", label).is_ok());
        }
    }

    #[test]
    fn an_http_endpoint_off_loopback_is_refused() {
        for label in ENDPOINT_LABELS {
            let err = refuse_insecure_endpoint("http://id.example.com/x", label).unwrap_err();
            assert!(matches!(err, OidcError::ProviderUnavailable(_)));
        }
    }

    #[test]
    fn a_malformed_endpoint_url_is_refused_not_panicked_on() {
        assert!(refuse_insecure_endpoint("not a url", "token").is_err());
    }

    /// `is_loopback_host` matches on the exact string, so a hostile look-alike
    /// that merely *contains* a loopback name must not be mistaken for it —
    /// otherwise `127.0.0.1.evil.com` or `localhost.evil.com` could downgrade
    /// an endpoint to plain `http://` by embedding the loopback name as a
    /// prefix. Pinned here as a test rather than left to inspection.
    #[test]
    fn loopback_look_alikes_are_not_mistaken_for_loopback() {
        assert!(!is_loopback_host("127.0.0.1.evil.com"));
        assert!(!is_loopback_host("localhost.evil.com"));
        assert!(!is_loopback_host("[::1].evil.com"));
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

    use hmac::{Hmac, Mac};
    use openidconnect::core::{CoreIdTokenClaims, CoreJsonWebKeySet, CoreRsaPrivateSigningKey};
    use openidconnect::{
        Audience, EmptyAdditionalClaims, EndUserEmail, EndUserName, LocalizedClaim,
        PrivateSigningKey, StandardClaims, SubjectIdentifier,
    };

    type HmacSha256 = Hmac<sha2::Sha256>;

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
        name: Option<String>,
        email: Option<String>,
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
                name: Some("Ada Lovelace".into()),
                email: Some("ada@example.com".into()),
            }
        }
    }

    struct TestSigner {
        key_pair: rsa::RsaPrivateKey,
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

            TestSigner {
                key_pair,
                signing_key,
            }
        }

        fn keys(&self) -> CoreJsonWebKeySet {
            CoreJsonWebKeySet::new(vec![self.signing_key.as_verification_key()])
        }

        /// Builds a signed token whose claims `f` may mutate, starting from a
        /// set of claims that verifies cleanly against `expectations()`.
        fn token_with(&self, f: impl FnOnce(Claims) -> Claims) -> String {
            let c = f(Claims::default());

            let mut claims = CoreIdTokenClaims::new(
                IssuerUrl::new(c.iss).expect("valid issuer URL"),
                vec![Audience::new(c.aud)],
                DateTime::from_timestamp(c.exp, 0).expect("valid exp"),
                DateTime::from_timestamp(c.iat, 0).expect("valid iat"),
                StandardClaims::new(SubjectIdentifier::new(c.sub)),
                EmptyAdditionalClaims {},
            )
            .set_nonce(Some(Nonce::new(c.nonce)));

            if let Some(name) = c.name {
                claims = claims.set_name(Some(LocalizedClaim::from(EndUserName::new(name))));
            }
            if let Some(email) = c.email {
                claims = claims.set_email(Some(EndUserEmail::new(email)));
            }

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

        /// DER bytes of this signer's RSA *public* key — the exact bytes an
        /// IdP publishes in its JWKS for signature verification. Never the
        /// private key.
        fn public_key_der(&self) -> Vec<u8> {
            use rsa::pkcs1::EncodeRsaPublicKey;

            self.key_pair
                .to_public_key()
                .to_pkcs1_der()
                .expect("encode RSA public key as DER")
                .as_bytes()
                .to_vec()
        }

        /// The classic RS256→HS256 algorithm-confusion attack: a token
        /// claiming `alg: HS256`, HMAC-SHA256-signed using this signer's RSA
        /// *public* key bytes as the shared secret. As with `unsigned_token`,
        /// `openidconnect`'s public signing API can't produce this (it only
        /// signs with an actual `JwsSigningAlgorithm`, and never treats a
        /// verification key as an HMAC secret), so it's assembled by hand.
        fn confusion_token(&self) -> String {
            let c = Claims::default();
            let header = serde_json::json!({ "alg": "HS256" });
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
            let signing_input = format!("{header_b64}.{payload_b64}");

            let mut mac = HmacSha256::new_from_slice(&self.public_key_der())
                .expect("HMAC accepts a key of any length");
            mac.update(signing_input.as_bytes());
            let signature_b64 = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

            format!("{signing_input}.{signature_b64}")
        }
    }
}
