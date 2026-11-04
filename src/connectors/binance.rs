use crate::credentials::Shown::{Clear, Hidden};
use crate::venues::{environments, field, Input, Venue, DOCS};

use super::{Connector, FieldType, Manifest, QuoteScope};

pub struct Binance;

static MANIFEST: Manifest = Manifest {
    code: "BINANCE",
    credential_kind: "BinanceFix",
    venue,
    field_types: &[("port", FieldType::Port)],
    redaction: &[("host", Clear), ("port", Clear), ("sender_comp_id", Clear), ("target_comp_id", Clear), ("api_key", Hidden), ("private_key", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
};

impl Connector for Binance {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }
}

fn venue() -> Venue {
    Venue {
        code: "BINANCE",
        name: "Binance",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/binance/"),
        universe: Some("About 2,000 spot pairs"),
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
    }
}
