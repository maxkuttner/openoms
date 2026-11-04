use crate::credentials::Shown::{Clear, Hidden};
use crate::venues::{environments, field, Input, Venue, DOCS};

use super::{Connector, FieldType, Manifest, QuoteScope};

pub struct Ibkr;

static MANIFEST: Manifest = Manifest {
    code: "IBKR",
    credential_kind: "IbkrFix",
    venue,
    field_types: &[("port", FieldType::Port), ("ssl", FieldType::Bool { default: true })],
    redaction: &[("host", Clear), ("port", Clear), ("sender_comp_id", Clear), ("target_comp_id", Clear), ("ssl", Clear), ("password", Hidden)],
    live_only: false,
    universe: false,
    quotes: QuoteScope::None,
};

impl Connector for Ibkr {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }
}

fn venue() -> Venue {
    Venue {
        code: "IBKR",
        name: "Interactive Brokers",
        description: "Multi-asset over FIX; experimental, no instrument loading",
        docs_url: format!("{DOCS}/interactive-brokers/"),
        universe: None,
        environments: environments("paper account", None, None),
        fields: vec![
            field("password", "Password").secret(),
            field("host", "Host").fix(),
            field("port", "Port").fix(),
            field("sender_comp_id", "SenderCompID").fix(),
            field("target_comp_id", "TargetCompID").fix().prefill("IBKR", "IBKR"),
            field("ssl", "TLS").fix().input(Input::Checkbox).prefill("true", "true"),
        ],
    }
}
