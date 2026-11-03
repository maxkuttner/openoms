//! The venues a broker connection can point at, with what the cockpit needs to
//! set one up: a plain name, what it trades, what PAPER means there, where to
//! get the keys, and the credential fields.
//!
//! The one place a venue is described: the cockpit's dropdown and credentials
//! form are built from `GET /admin/venues`, and the tests below hold this list
//! to `known_broker_codes` and to each venue's `BrokerCredentials` fields.
//! What each field means and where to get it is on the venue's docs page
//! (`docs_url`), not here: the form looks the same for every venue.

use serde::Serialize;

const DOCS: &str = "https://maxkuttner.github.io/openoms/integrations";

/// The section a venue's FIX session settings go in, after the credentials.
const FIX_SESSION: &str = "FIX session";

#[derive(Serialize)]
pub struct Venue {
    /// The `broker_connection.broker_code`.
    pub code: &'static str,
    pub name: &'static str,
    /// What it trades, in a few words.
    pub description: &'static str,
    pub docs_url: String,
    pub environments: Vec<Environment>,
    /// Credentials first, then any grouped connection settings.
    pub fields: Vec<CredentialField>,
}

#[derive(Serialize)]
pub struct Environment {
    pub code: &'static str,
    /// What this environment is at the venue, e.g. "spot testnet".
    pub label: &'static str,
    /// Where to get the keys for it, when there is one page for that.
    pub keys_url: Option<&'static str>,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    Text,
    Checkbox,
    Textarea,
}

#[derive(Serialize)]
pub struct CredentialField {
    pub name: &'static str,
    pub label: &'static str,
    pub secret: bool,
    pub input: Input,
    /// Connection settings are grouped under a section name; the key and
    /// secret are not.
    pub section: Option<&'static str>,
    /// Pre-filled for a PAPER / LIVE connection: values that are the same for
    /// everyone, such as a testnet host. Never a secret.
    pub paper: Option<&'static str>,
    pub live: Option<&'static str>,
}

fn field(name: &'static str, label: &'static str) -> CredentialField {
    CredentialField { name, label, secret: false, input: Input::Text, section: None, paper: None, live: None }
}

impl CredentialField {
    fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    fn input(mut self, input: Input) -> Self {
        self.input = input;
        self
    }
    fn fix(mut self) -> Self {
        self.section = Some(FIX_SESSION);
        self
    }
    fn prefill(mut self, paper: &'static str, live: &'static str) -> Self {
        self.paper = Some(paper);
        self.live = Some(live);
        self
    }
}

fn environments(paper: &'static str, paper_keys: Option<&'static str>, live_keys: Option<&'static str>) -> Vec<Environment> {
    vec![
        Environment { code: "PAPER", label: paper, keys_url: paper_keys },
        Environment { code: "LIVE", label: "live account", keys_url: live_keys },
    ]
}

pub fn venues() -> Vec<Venue> {
    vec![
        Venue {
            code: "ALPACA",
            name: "Alpaca",
            description: "US stocks, ETFs and options",
            docs_url: format!("{DOCS}/alpaca/"),
            environments: environments(
                "paper trading account",
                Some("https://app.alpaca.markets/paper/dashboard/overview"),
                Some("https://app.alpaca.markets/brokerage/dashboard/overview"),
            ),
            fields: vec![field("key", "API key ID"), field("secret", "Secret key").secret()],
        },
        Venue {
            code: "BINANCE",
            name: "Binance",
            description: "Crypto spot pairs",
            docs_url: format!("{DOCS}/binance/"),
            environments: environments(
                "spot testnet",
                Some("https://testnet.binance.vision/"),
                Some("https://www.binance.com/en/my/settings/api-management"),
            ),
            fields: vec![
                field("api_key", "API key").secret(),
                field("private_key", "Private key (PEM)").secret().input(Input::Textarea),
                field("host", "Host").fix().prefill("fix-oe.testnet.binance.vision", "fix-oe.binance.com"),
                field("port", "Port").fix().prefill("9000", "9000"),
                field("sender_comp_id", "SenderCompID").fix().prefill("OMS", "OMS"),
                field("target_comp_id", "TargetCompID").fix().prefill("SPOT", "SPOT"),
            ],
        },
        Venue {
            code: "IBKR",
            name: "Interactive Brokers",
            description: "Multi-asset over FIX; experimental, no instrument loading",
            docs_url: format!("{DOCS}/interactive-brokers/"),
            environments: environments("paper account", None, None),
            fields: vec![
                field("password", "Password").secret(),
                field("host", "Host").fix(),
                field("port", "Port").fix(),
                field("sender_comp_id", "SenderCompID").fix(),
                field("target_comp_id", "TargetCompID").fix().prefill("IBKR", "IBKR"),
                field("ssl", "TLS").fix().input(Input::Checkbox).prefill("true", "true"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::redacted_fields;
    use crate::credentials_api::{parse_broker, CredentialSubmission};
    use std::collections::{BTreeSet, HashMap};

    #[test]
    fn every_venue_the_server_accepts_is_listed_and_nothing_else() {
        let listed: BTreeSet<_> = venues().iter().map(|v| v.code).collect();
        let accepted: BTreeSet<_> = crate::setup::brokers::known_broker_codes().into_iter().collect();
        assert_eq!(listed, accepted);
    }

    // The cockpit builds the credentials form from this list alone, so its
    // fields must be exactly the ones the credential carries.
    #[test]
    fn each_venues_fields_are_exactly_its_credentials_fields() {
        for venue in venues() {
            let filled: HashMap<String, String> = venue
                .fields
                .iter()
                .map(|f| {
                    let value = f.paper.unwrap_or(match f.input {
                        Input::Checkbox => "true",
                        _ if f.name == "port" => "9000",
                        _ => "x",
                    });
                    (f.name.to_string(), value.to_string())
                })
                .collect();
            let sub = CredentialSubmission { fields: filled };
            let parsed = parse_broker(venue.code, None, &sub)
                .unwrap_or_else(|e| panic!("{}: the listed fields do not make a credential: {e}", venue.code));
            let carried: BTreeSet<_> = redacted_fields(&parsed).into_iter().map(|f| f.name).collect();
            let listed: BTreeSet<_> = venue.fields.iter().map(|f| f.name.to_string()).collect();
            assert_eq!(listed, carried, "{}", venue.code);
        }
    }

    #[test]
    fn every_venue_explains_itself_and_both_environments() {
        for venue in venues() {
            assert!(!venue.name.is_empty() && !venue.description.is_empty(), "{}", venue.code);
            assert!(venue.docs_url.starts_with("https://"), "{}", venue.code);
            let envs: Vec<_> = venue.environments.iter().map(|e| e.code).collect();
            assert_eq!(envs, ["PAPER", "LIVE"], "{}", venue.code);
        }
    }

    // One look in the drawer: the key and secret come first, connection
    // settings after them, in their own section.
    #[test]
    fn credentials_come_before_connection_settings() {
        for venue in venues() {
            let first_grouped = venue.fields.iter().position(|f| f.section.is_some()).unwrap_or(venue.fields.len());
            assert!(
                venue.fields[first_grouped..].iter().all(|f| f.section.is_some()),
                "{}: an ungrouped field after the settings section",
                venue.code
            );
            assert!(first_grouped > 0, "{}: no credential before the settings", venue.code);
            assert!(venue.fields[..first_grouped].iter().any(|f| f.secret), "{}: no secret on top", venue.code);
        }
    }

    #[test]
    fn binance_paper_is_prefilled_with_the_testnet_fix_session() {
        let binance = venues().into_iter().find(|v| v.code == "BINANCE").unwrap();
        let paper = |name: &str| binance.fields.iter().find(|f| f.name == name).unwrap().paper;
        assert_eq!(paper("host"), Some("fix-oe.testnet.binance.vision"));
        assert_eq!(paper("port"), Some("9000"));
        assert_eq!(paper("target_comp_id"), Some("SPOT"));
        assert_eq!(paper("api_key"), None);
    }
}
