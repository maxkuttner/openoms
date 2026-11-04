use crate::credentials::Shown::Hidden;
use crate::venues::{environments, field, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Okx;

static MANIFEST: Manifest = Manifest {
    code: "OKX",
    credential_kind: "Okx",
    venue,
    field_types: &[],
    redaction: &[("api_key", Hidden), ("api_secret", Hidden), ("passphrase", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
};

impl Connector for Okx {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }
}

fn venue() -> Venue {
    Venue {
        code: "OKX",
        name: "OKX",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/okx/"),
        universe: Some("About 700 spot pairs"),
        // Demo Trading keys are made in the same account, under Demo Trading.
        environments: environments(
            "demo trading",
            Some("https://www.okx.com/account/my-api"),
            Some("https://www.okx.com/account/my-api"),
        ),
        fields: vec![
            field("api_key", "API key").secret(),
            field("api_secret", "API secret").secret(),
            field("passphrase", "Passphrase").secret(),
        ],
    }
}
