//! The shape of a venue's entry in the cockpit: a plain name, what it trades,
//! what PAPER means there, where to get the keys, and the credential fields.
//! Each connector builds its own; the OMS lists them (`GET /admin/venues`).

use serde::Serialize;

pub const DOCS: &str = "https://openoms.dev/integrations";

/// The section a venue's FIX session settings go in, after the credentials.
pub const FIX_SESSION: &str = "FIX session";

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

pub fn field(name: &'static str, label: &'static str) -> CredentialField {
    CredentialField { name, label, secret: false, input: Input::Text, section: None, paper: None, live: None }
}

impl CredentialField {
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    pub fn input(mut self, input: Input) -> Self {
        self.input = input;
        self
    }
    pub fn fix(mut self) -> Self {
        self.section = Some(FIX_SESSION);
        self
    }
    pub fn prefill(mut self, paper: &'static str, live: &'static str) -> Self {
        self.paper = Some(paper);
        self.live = Some(live);
        self
    }
}

pub fn environments(paper: &'static str, paper_keys: Option<&'static str>, live_keys: Option<&'static str>) -> Vec<Environment> {
    vec![
        Environment { code: "PAPER", label: paper, keys_url: paper_keys },
        Environment { code: "LIVE", label: "live account", keys_url: live_keys },
    ]
}
