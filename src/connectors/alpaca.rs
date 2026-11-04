use crate::credentials::Shown::{Hidden, Tail};
use crate::venues::{environments, field, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Alpaca;

static MANIFEST: Manifest = Manifest {
    code: "ALPACA",
    credential_kind: "Alpaca",
    venue,
    field_types: &[],
    redaction: &[("key", Tail), ("secret", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::NonCrypto,
};

impl Connector for Alpaca {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }
}

fn venue() -> Venue {
    Venue {
        code: "ALPACA",
        name: "Alpaca",
        description: "US stocks, ETFs and options",
        docs_url: format!("{DOCS}/alpaca/"),
        universe: Some("About 14,400 stocks and ETFs"),
        environments: environments(
            "paper trading account",
            Some("https://app.alpaca.markets/paper/dashboard/overview"),
            Some("https://app.alpaca.markets/brokerage/dashboard/overview"),
        ),
        fields: vec![field("key", "API key ID"), field("secret", "Secret key").secret()],
    }
}
