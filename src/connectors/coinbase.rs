use crate::credentials::Shown::Hidden;
use crate::venues::{field, Environment, Input, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Coinbase;

static MANIFEST: Manifest = Manifest {
    code: "COINBASE",
    credential_kind: "Coinbase",
    venue,
    field_types: &[],
    redaction: &[("api_key", Hidden), ("private_key", Hidden)],
    live_only: true,
    universe: true,
    quotes: QuoteScope::OwnVenue,
};

impl Connector for Coinbase {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }
}

fn venue() -> Venue {
    Venue {
        code: "COINBASE",
        name: "Coinbase",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/coinbase/"),
        universe: Some("About 900 spot pairs"),
        // LIVE only: Coinbase's sandbox returns canned responses, not paper trading.
        environments: vec![Environment {
            code: "LIVE",
            label: "live account",
            keys_url: Some("https://portal.cdp.coinbase.com/projects/api-keys"),
        }],
        fields: vec![
            field("api_key", "API key").secret(),
            field("private_key", "Private key (PEM)").secret().input(Input::Textarea),
        ],
    }
}
