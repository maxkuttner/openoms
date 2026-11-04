use futures_util::future::BoxFuture;

use crate::credentials_api::TestOutcome;
use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::Connected;
use crate::credentials::Shown::{Clear, Hidden};
use crate::venues::{environments, field, Input, Venue, DOCS};

use super::{FIX_NOT_TESTABLE, Connector, FieldType, Manifest, QuoteScope};

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
    exec_stream: false,
    session_bound: true,
};

impl Connector for Ibkr {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    /// FIX-only: the session both routes orders and delivers execution reports.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, deps: &RegistrationDeps) -> Result<Connected, String> {
        let adapter = crate::fix::start_ibkr(env, creds, &deps.stream_health, deps.engine.clone())
            .ok_or_else(|| "IBKR FIX session could not start".to_string())?;
        Ok(Connected { adapter, exec_stream: None })
    }

    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { FIX_NOT_TESTABLE })
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
