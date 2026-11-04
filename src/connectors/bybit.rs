use crate::credentials::Shown::Hidden;
use crate::venues::{environments, field, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Bybit;

static MANIFEST: Manifest = Manifest {
    code: "BYBIT",
    credential_kind: "Bybit",
    venue,
    field_types: &[],
    redaction: &[("api_key", Hidden), ("api_secret", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
};

impl Connector for Bybit {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }
}

fn venue() -> Venue {
    Venue {
        code: "BYBIT",
        name: "Bybit",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/bybit/"),
        universe: Some("About 800 spot pairs"),
        environments: environments(
            "spot testnet",
            Some("https://testnet.bybit.com/app/user/api-management"),
            Some("https://www.bybit.com/app/user/api-management"),
        ),
        fields: vec![field("api_key", "API key").secret(), field("api_secret", "API secret").secret()],
    }
}
