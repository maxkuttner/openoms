//! Coinbase Advanced Trade spot adapter (LIVE only — Coinbase has no paper
//! trading; its "sandbox" returns canned responses).
//!
//! Auth: every private REST call carries `Authorization: Bearer <jwt>`, a fresh
//! ES256 (P-256) token per request signed with the CDP key's private key:
//! header `{alg: ES256, typ: JWT, kid: <key name>, nonce}`, claims
//! `{iss: "cdp", sub: <key name>, nbf, exp: nbf + 120, uri: "<METHOD> host<path>"}`.
//! The user WebSocket takes the same token without `uri` in its subscribe message.
//!
//! Symbol mapping: Coinbase products are their own instruments, venue `COINBASE`,
//! symbol = `product_id` (`BTC-USD`); the base currency is the native id.

use base64::Engine;
use dataprovider::{Identifiers, InstrumentDef};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use rand::RngCore;
use reqwest::Client;

use super::{BrokerError, BrokerInstrument, InstrumentProvider};

const HOST: &str = "api.coinbase.com";
const BASE: &str = "/api/v3/brokerage";
/// Coinbase rejects a token older than two minutes.
const JWT_TTL_SECS: i64 = 120;

#[derive(Clone)]
pub struct CoinbaseAdapter {
    client: Client,
    key_name: String,
    signing_key: SigningKey,
}

/// `"<METHOD> api.coinbase.com<path>"`, the `uri` claim; the query string is not
/// part of it.
pub(crate) fn rest_uri(method: &str, path: &str) -> String {
    let path = path.split_once('?').map_or(path, |(p, _)| p);
    format!("{method} {HOST}{path}")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl CoinbaseAdapter {
    /// `private_key_pem` is the CDP key's EC private key — SEC1
    /// (`BEGIN EC PRIVATE KEY`, how Coinbase hands it out) or PKCS#8. A key copied
    /// out of Coinbase's JSON download keeps its `\n` escapes; those are accepted.
    pub fn new(key_name: String, private_key_pem: &str) -> Result<Self, String> {
        let pem = private_key_pem.trim().replace("\\n", "\n");
        let secret = p256::SecretKey::from_sec1_pem(&pem)
            .or_else(|_| {
                use p256::pkcs8::DecodePrivateKey;
                p256::SecretKey::from_pkcs8_pem(&pem)
            })
            .map_err(|_| "invalid Coinbase private key: expected an EC (P-256) PEM; Ed25519 keys are not supported".to_string())?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Ok(Self { client, key_name, signing_key: SigningKey::from(&secret) })
    }

    pub fn key_name(&self) -> &str {
        &self.key_name
    }

    /// A fresh ES256 token; `uri` for REST calls, `None` for the WebSocket.
    pub fn jwt(&self, uri: Option<&str>) -> String {
        let mut nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce);
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let header = serde_json::json!({ "alg": "ES256", "typ": "JWT", "kid": self.key_name, "nonce": nonce });
        let now = chrono::Utc::now().timestamp();
        let mut claims = serde_json::json!({ "iss": "cdp", "sub": self.key_name, "nbf": now, "exp": now + JWT_TTL_SECS });
        if let Some(uri) = uri {
            claims["uri"] = uri.into();
        }
        let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let sig: Signature = self.signing_key.sign(signed.as_bytes());
        format!("{signed}.{}", b64(&sig.to_bytes()))
    }

    /// GET `BASE + path` (+ `?query`); signed for private endpoints.
    async fn get(&self, path: &str, query: &str, signed: bool) -> Result<serde_json::Value, BrokerError> {
        let full = format!("{BASE}{path}");
        let url = if query.is_empty() { format!("https://{HOST}{full}") } else { format!("https://{HOST}{full}?{query}") };
        let mut req = self.client.get(&url);
        if signed {
            req = req.bearer_auth(self.jwt(Some(&rest_uri("GET", &full))));
        }
        read(req.send().await).await
    }

    /// Signed POST of a JSON body to `BASE + path`.
    async fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, BrokerError> {
        let full = format!("{BASE}{path}");
        let req = self
            .client
            .post(format!("https://{HOST}{full}"))
            .bearer_auth(self.jwt(Some(&rest_uri("POST", &full))))
            .json(body);
        read(req.send().await).await
    }

    /// GET /market/products?product_type=SPOT — public, every spot product in one call.
    pub async fn list_spot_products(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let body = self.get("/market/products", "product_type=SPOT", false).await?;
        Ok(parse_spot_products(&body))
    }

    /// GET /key_permissions — what the credentials Test judges (trade, no transfer).
    pub async fn key_permissions(&self) -> Result<serde_json::Value, BrokerError> {
        self.get("/key_permissions", "", true).await
    }
}

/// The JSON body of a response, or a rejection carrying Coinbase's message for a
/// non-2xx status.
async fn read(resp: Result<reqwest::Response, reqwest::Error>) -> Result<serde_json::Value, BrokerError> {
    let resp = resp.map_err(|e| BrokerError::Network(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| BrokerError::Network(e.to_string()))?;
    let body: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    if status.is_success() {
        return body.ok_or_else(|| BrokerError::BrokerRejected(format!("HTTP {status}: not JSON: {text}")));
    }
    let message = body
        .as_ref()
        .and_then(|b| b["message"].as_str().or(b["error"].as_str()).map(String::from))
        .unwrap_or(text);
    Err(BrokerError::BrokerRejected(format!("HTTP {status}: {message}")))
}

/// A decimal string field as f64.
fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// Digits after the point of a step like "0.01" (2) or "1" (0).
fn decimals(step: &str) -> i32 {
    step.split_once('.').map_or(0, |(_, frac)| frac.trim_end_matches('0').len() as i32)
}

/// Spot products only; every one listed, tradeable only when online and not
/// disabled, cancel-only or view-only.
pub(crate) fn parse_spot_products(body: &serde_json::Value) -> Vec<BrokerInstrument> {
    let list = body["products"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter(|p| p["product_type"].as_str() == Some("SPOT"))
        .filter_map(|p| {
            let symbol = p["product_id"].as_str()?.to_string();
            let tick = p["price_increment"].as_str().or(p["quote_increment"].as_str()).unwrap_or("0");
            let flag = |k: &str| p[k].as_bool().unwrap_or(false);
            let tradeable = p["status"].as_str() == Some("online")
                && !flag("trading_disabled")
                && !flag("is_disabled")
                && !flag("cancel_only")
                && !flag("view_only");
            let definition = InstrumentDef {
                symbol: symbol.clone(),
                venue: "COINBASE".to_string(),
                currency: p["quote_currency_id"].as_str()?.to_string(),
                asset_class: "CRYPTO".to_string(),
                instrument_class: "SPOT".to_string(),
                name: p["base_name"].as_str().map(String::from),
                price_precision: decimals(tick),
                price_increment: tick.parse().unwrap_or(0.0),
                size_increment: num(&p["base_increment"]).unwrap_or(0.0),
                lot_size: None,
                contract_size: 1.0,
                native_id: Some(p["base_currency_id"].as_str()?.to_string()),
                provider_exchange: Some("COINBASE".to_string()),
                derivative: None,
                identifiers: Identifiers::default(),
            };
            Some(BrokerInstrument {
                broker_symbol: symbol,
                broker_exchange: Some("COINBASE".to_string()),
                is_tradeable: tradeable,
                min_quantity: num(&p["base_min_size"]),
                max_quantity: num(&p["base_max_size"]),
                min_notional: num(&p["quote_min_size"]),
                max_notional: num(&p["quote_max_size"]),
                definition,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl InstrumentProvider for CoinbaseAdapter {
    /// Spot only: `option_underlyings` is ignored.
    async fn list_instruments(&self, _option_underlyings: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
        self.list_spot_products().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
    use p256::pkcs8::EncodePrivateKey;

    const KEY_NAME: &str = "organizations/org-1/apiKeys/key-1";

    fn key_pair() -> (String, VerifyingKey) {
        let secret = p256::SecretKey::random(&mut rand::thread_rng());
        let pem = secret.to_sec1_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string();
        let verifying = VerifyingKey::from(&p256::ecdsa::SigningKey::from(&secret));
        (pem, verifying)
    }

    fn part(token: &str, i: usize) -> serde_json::Value {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(i).unwrap()).unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    #[test]
    fn jwt_header_and_claims_follow_coinbase() {
        let (pem, _) = key_pair();
        let a = CoinbaseAdapter::new(KEY_NAME.into(), &pem).unwrap();
        let token = a.jwt(Some("GET api.coinbase.com/api/v3/brokerage/accounts"));
        let header = part(&token, 0);
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], KEY_NAME);
        assert_eq!(header["nonce"].as_str().unwrap().len(), 32);
        let claims = part(&token, 1);
        assert_eq!(claims["iss"], "cdp");
        assert_eq!(claims["sub"], KEY_NAME);
        assert_eq!(claims["exp"].as_i64().unwrap() - claims["nbf"].as_i64().unwrap(), 120);
        assert_eq!(claims["uri"], "GET api.coinbase.com/api/v3/brokerage/accounts");
    }

    #[test]
    fn jwt_signature_verifies_with_the_public_key() {
        let (pem, verifying) = key_pair();
        let token = CoinbaseAdapter::new(KEY_NAME.into(), &pem).unwrap().jwt(None);
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig).unwrap();
        assert_eq!(sig.len(), 64, "ES256 is raw r||s");
        verifying.verify(signed.as_bytes(), &Signature::from_slice(&sig).unwrap()).unwrap();
    }

    #[test]
    fn websocket_jwt_has_no_uri() {
        let (pem, _) = key_pair();
        let token = CoinbaseAdapter::new(KEY_NAME.into(), &pem).unwrap().jwt(None);
        assert!(part(&token, 1).get("uri").is_none());
    }

    #[test]
    fn rest_uri_excludes_the_query() {
        assert_eq!(
            rest_uri("GET", "/api/v3/brokerage/orders/historical/batch?order_status=OPEN"),
            "GET api.coinbase.com/api/v3/brokerage/orders/historical/batch"
        );
    }

    #[test]
    fn a_pem_pasted_with_literal_newlines_is_accepted() {
        let (pem, _) = key_pair();
        let escaped = pem.trim_end().replace('\n', "\\n");
        assert!(CoinbaseAdapter::new(KEY_NAME.into(), &escaped).is_ok());
        // PKCS#8 works too.
        let pkcs8 = p256::SecretKey::random(&mut rand::thread_rng())
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        assert!(CoinbaseAdapter::new(KEY_NAME.into(), &pkcs8).is_ok());
    }

    #[test]
    fn a_bad_pem_is_refused() {
        assert!(CoinbaseAdapter::new(KEY_NAME.into(), "not a key").is_err());
    }

    #[test]
    fn spot_products_are_coinbase_venue_rows() {
        let body: serde_json::Value =
            serde_json::from_str(include_str!("testdata/coinbase/products_spot.json")).unwrap();
        let list = parse_spot_products(&body);
        let btc = list.iter().find(|i| i.broker_symbol == "BTC-USD").unwrap();
        assert_eq!(btc.definition.venue, "COINBASE");
        assert_eq!(btc.definition.currency, "USD");
        assert_eq!(btc.definition.native_id.as_deref(), Some("BTC"));
        assert_eq!(btc.definition.price_increment, 0.01);
        assert_eq!(btc.definition.price_precision, 2);
        assert_eq!(btc.definition.size_increment, 0.00000001);
        assert_eq!(btc.min_quantity, Some(0.00000001));
        assert_eq!(btc.max_quantity, Some(3400.0));
        assert_eq!(btc.min_notional, Some(1.0));
        assert!(btc.is_tradeable);
        assert!(!list.iter().find(|i| i.broker_symbol == "HALT-USD").unwrap().is_tradeable);
        assert!(list.iter().all(|i| i.broker_symbol != "BIT-28NOV26-CDE"), "futures are not loaded");
    }
}
