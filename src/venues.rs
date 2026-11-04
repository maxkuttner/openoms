//! The venues a broker connection can point at, with what the cockpit needs to
//! set one up: a plain name, what it trades, what PAPER means there, where to
//! get the keys, and the credential fields.
//!
//! Each venue is described by its connector (`crate::connectors`); this module
//! holds the shapes and assembles the list the cockpit's dropdown and
//! credentials form are built from (`GET /admin/venues`). The tests below hold
//! that list to `known_broker_codes` and to each venue's credential fields.
//! What each field means and where to get it is on the venue's docs page
//! (`docs_url`), not here: the form looks the same for every venue.

use serde::Serialize;

pub(crate) const DOCS: &str = "https://maxkuttner.github.io/openoms/integrations";

/// The section a venue's FIX session settings go in, after the credentials.
pub(crate) const FIX_SESSION: &str = "FIX session";

#[derive(Serialize)]
pub struct Venue {
    /// The `broker_connection.broker_code`.
    pub code: &'static str,
    pub name: &'static str,
    /// What it trades, in a few words.
    pub description: &'static str,
    pub docs_url: String,
    /// What loading its instrument universe brings in, for the cockpit's
    /// "load now?" prompt; `None` when it has no catalog to load.
    pub universe: Option<&'static str>,
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

pub(crate) fn field(name: &'static str, label: &'static str) -> CredentialField {
    CredentialField { name, label, secret: false, input: Input::Text, section: None, paper: None, live: None }
}

impl CredentialField {
    pub(crate) fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    pub(crate) fn input(mut self, input: Input) -> Self {
        self.input = input;
        self
    }
    pub(crate) fn fix(mut self) -> Self {
        self.section = Some(FIX_SESSION);
        self
    }
    pub(crate) fn prefill(mut self, paper: &'static str, live: &'static str) -> Self {
        self.paper = Some(paper);
        self.live = Some(live);
        self
    }
}

pub(crate) fn environments(paper: &'static str, paper_keys: Option<&'static str>, live_keys: Option<&'static str>) -> Vec<Environment> {
    vec![
        Environment { code: "PAPER", label: paper, keys_url: paper_keys },
        Environment { code: "LIVE", label: "live account", keys_url: live_keys },
    ]
}

/// Whether a broker connection for `broker_code` may run in `environment` —
/// only the environments its venue offers (Coinbase: LIVE only, so a "PAPER"
/// Coinbase connection, which would still trade real money, is refused).
pub fn environment_allowed(broker_code: &str, environment: &str) -> bool {
    venues()
        .iter()
        .find(|v| v.code == broker_code)
        .is_some_and(|v| v.environments.iter().any(|e| e.code == environment))
}

pub fn venues() -> Vec<Venue> {
    crate::connectors::all().iter().map(|c| (c.manifest().venue)()).collect()
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
    fn every_venue_explains_itself_and_its_environments() {
        for venue in venues() {
            assert!(!venue.name.is_empty() && !venue.description.is_empty(), "{}", venue.code);
            assert!(venue.docs_url.starts_with("https://"), "{}", venue.code);
            // LIVE always; PAPER first, where the venue offers one.
            let envs: Vec<_> = venue.environments.iter().map(|e| e.code).collect();
            assert!(envs == ["PAPER", "LIVE"] || envs == ["LIVE"], "{}: {envs:?}", venue.code);
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
    fn bybit_is_a_key_and_secret_venue_on_its_testnet() {
        let bybit = venues().into_iter().find(|v| v.code == "BYBIT").expect("BYBIT listed");
        let fields: Vec<_> = bybit.fields.iter().map(|f| (f.name, f.label, f.secret, f.section)).collect();
        assert_eq!(fields, [("api_key", "API key", true, None), ("api_secret", "API secret", true, None)]);
        assert_eq!(bybit.environments[0].label, "spot testnet");
        assert_eq!(bybit.environments[0].keys_url, Some("https://testnet.bybit.com/app/user/api-management"));
    }

    #[test]
    fn okx_is_key_secret_and_passphrase_on_demo_trading() {
        let okx = venues().into_iter().find(|v| v.code == "OKX").expect("OKX listed");
        let fields: Vec<_> = okx.fields.iter().map(|f| (f.name, f.label, f.secret, f.section)).collect();
        assert_eq!(
            fields,
            [("api_key", "API key", true, None), ("api_secret", "API secret", true, None), ("passphrase", "Passphrase", true, None)]
        );
        assert_eq!(okx.environments[0].code, "PAPER");
        assert_eq!(okx.environments[0].label, "demo trading");
        assert!(environment_allowed("OKX", "PAPER") && environment_allowed("OKX", "LIVE"));
    }

    // A Coinbase connection labelled PAPER would still trade real money.
    #[test]
    fn a_connection_may_only_use_an_environment_its_venue_offers() {
        assert!(environment_allowed("COINBASE", "LIVE"));
        assert!(!environment_allowed("COINBASE", "PAPER"));
        assert!(environment_allowed("BINANCE", "PAPER"));
        assert!(environment_allowed("BINANCE", "LIVE"));
        assert!(!environment_allowed("NOSUCHBROKER", "LIVE"));
    }

    #[test]
    fn coinbase_is_live_only_with_key_and_private_key() {
        let cb = venues().into_iter().find(|v| v.code == "COINBASE").expect("COINBASE listed");
        let envs: Vec<_> = cb.environments.iter().map(|e| e.code).collect();
        assert_eq!(envs, ["LIVE"]);
        let fields: Vec<_> = cb.fields.iter().map(|f| (f.name, f.label, f.secret, f.section)).collect();
        assert_eq!(fields, [("api_key", "API key", true, None), ("private_key", "Private key (PEM)", true, None)]);
        assert!(matches!(cb.fields[1].input, Input::Textarea));
    }

    #[test]
    fn the_universe_prompt_copy_lives_with_the_venue() {
        let universe = |code: &str| venues().into_iter().find(|v| v.code == code).unwrap().universe;
        assert_eq!(universe("BINANCE"), Some("About 2,000 spot pairs"));
        assert_eq!(universe("ALPACA"), Some("About 14,400 stocks and ETFs"));
        assert_eq!(universe("IBKR"), None);
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
