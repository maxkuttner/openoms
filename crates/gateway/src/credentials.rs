//! A connection's stored credential: its kind tag and its fields. The OMS seals,
//! stores and redacts it; each connector parses its own typed view.

use serde::{Deserialize, Serialize};

/// Credentials for one broker connection: its kind tag and its fields, stored as
/// `{"kind": <tag>, ...fields}` — the same JSON the per-broker enum used to
/// write, so stored blobs need no migration. Each connector owns the typed shape
/// of its fields and reads it with [`BrokerCredentials::parse`].
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct BrokerCredentials {
    pub kind: String,
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl BrokerCredentials {
    pub fn new(kind: &str, fields: serde_json::Map<String, serde_json::Value>) -> Self {
        Self { kind: kind.to_string(), fields }
    }

    /// The connector's own typed view of these fields.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(serde_json::Value::Object(self.fields.clone()))
    }

    pub fn from_typed<T: Serialize>(kind: &str, typed: &T) -> Self {
        match serde_json::to_value(typed) {
            Ok(serde_json::Value::Object(fields)) => Self::new(kind, fields),
            _ => unreachable!("a credential struct always serializes to a JSON object"),
        }
    }

    /// What a field the blob does not carry means: a flag's default (IBKR's
    /// `ssl` predates some stored blobs and defaults to true), else nothing.
    pub fn default_text(&self, name: &str) -> String {
        crate::connectors::by_credential_kind(&self.kind)
            .and_then(|c| c.manifest().field_types.iter().find(|(n, _)| *n == name).map(|(_, t)| *t))
            .and_then(|t| match t {
                crate::connectors::FieldType::Bool { default } => Some(default.to_string()),
                crate::connectors::FieldType::Port => None,
            })
            .unwrap_or_default()
    }

    /// One field's value as text, as a form or a log shows it.
    pub fn text(&self, name: &str) -> Option<String> {
        self.fields.get(name).map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
}

/// How one credential field may be shown.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shown {
    /// In the clear (a host, a port).
    Clear,
    /// Named, value withheld.
    Hidden,
    /// Value withheld, its last four characters given as a hint.
    Tail,
}

// Hand-written: deriving Debug would print every secret the first time one
// appears in a panic message or a log line. Only fields the connector shows in
// the clear carry a value.
impl std::fmt::Debug for BrokerCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields = crate::connectors::by_credential_kind(&self.kind).map_or(&[][..], |c| c.manifest().redaction);
        let mut d = f.debug_struct("BrokerCredentials");
        d.field("kind", &self.kind);
        for &(name, how) in fields {
            match how {
                Shown::Clear => d.field(name, &self.text(name).unwrap_or_else(|| self.default_text(name))),
                Shown::Hidden | Shown::Tail => d.field(name, &"<redacted>"),
            };
        }
        d.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stored blobs, exactly as the tagged enum wrote them before credentials
    /// became generic: each must decode and encode back to the same JSON.
    #[test]
    fn stored_blobs_of_every_kind_round_trip() {
        let blobs = [
            r#"{"kind":"Alpaca","key":"AK1","secret":"S1"}"#,
            r#"{"kind":"IbkrFix","host":"h","port":4001,"sender_comp_id":"S","target_comp_id":"T","password":"p","ssl":false}"#,
            r#"{"kind":"BinanceFix","host":"h","port":9000,"sender_comp_id":"S","target_comp_id":"T","api_key":"k","private_key":"pem"}"#,
            r#"{"kind":"Bybit","api_key":"k","api_secret":"s"}"#,
            r#"{"kind":"Coinbase","api_key":"k","private_key":"pem"}"#,
            r#"{"kind":"Okx","api_key":"k","api_secret":"s","passphrase":"p"}"#,
        ];
        for blob in blobs {
            let creds: BrokerCredentials = serde_json::from_str(blob).expect(blob);
            let back = serde_json::to_value(&creds).unwrap();
            assert_eq!(back, serde_json::from_str::<serde_json::Value>(blob).unwrap(), "{blob}");
        }
    }

    #[cfg(feature = "fix")]
    #[test]
    fn ibkr_without_ssl_defaults_to_true() {
        let json = r#"{"kind":"IbkrFix","host":"h","port":4001,"sender_comp_id":"S","target_comp_id":"T","password":"p"}"#;
        let creds: BrokerCredentials = serde_json::from_str(json).unwrap();
        assert!(creds.parse::<crate::connectors::ibkr::fix::IbkrFixCredentials>().unwrap().ssl);
    }

    #[test]
    fn typed_round_trip_keeps_the_kind_tag() {
        let okx = crate::connectors::okx::adapter::OkxCredentials { api_key: "k".into(), api_secret: "s".into(), passphrase: "p".into() };
        let v = serde_json::to_value(BrokerCredentials::from_typed("Okx", &okx)).unwrap();
        assert_eq!(v["kind"], "Okx");
        assert_eq!(v["passphrase"], "p");
    }

    /// A kind no connector knows is data a newer version wrote: it still loads,
    /// and registration reports it as a failed connection.
    #[test]
    fn unknown_kind_still_decodes_but_no_connector_parses_it() {
        let creds: BrokerCredentials = serde_json::from_slice(br#"{"kind":"Nasdaq"}"#).unwrap();
        assert_eq!(creds.kind, "Nasdaq");
        assert!(creds.parse::<crate::connectors::okx::adapter::OkxCredentials>().is_err());
    }
}
