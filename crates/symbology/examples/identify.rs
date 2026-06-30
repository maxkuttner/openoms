//! Identify an instrument via OpenFIGI.
//!
//!   cargo run -p symbology --example identify -- AAPL US
//!   OPENFIGI_API_KEY=… cargo run -p symbology --example identify -- US0378331005
//!
//! First arg is a ticker (with optional exchange code as the second arg); to look up by
//! ISIN/CUSIP, pass it as the ticker — OpenFIGI tolerates it, or extend this example.

use symbology::{Identifier, InMemoryCache, InstrumentQuery, OpenFigiClient, Resolution};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let value = args.next().expect("usage: identify <TICKER> [EXCH_CODE]");
    let exch = args.next();

    let mut q = InstrumentQuery::ticker(&value);
    if let Some(e) = exch {
        q = q.exch(e);
    }

    let api_key = std::env::var("OPENFIGI_API_KEY").ok();
    let id = Identifier::new(OpenFigiClient::new(api_key), InMemoryCache::new());

    match id.identify(&q).await? {
        Resolution::Resolved(i) => println!(
            "RESOLVED  {}  {} — {}  [{}]",
            i.figi,
            i.ticker.unwrap_or_default(),
            i.name.unwrap_or_default(),
            i.exch_code.unwrap_or_default(),
        ),
        Resolution::Ambiguous(cands) => {
            println!("AMBIGUOUS ({} candidates):", cands.len());
            for i in cands {
                println!(
                    "  {}  {} [{}]",
                    i.figi,
                    i.ticker.unwrap_or_default(),
                    i.exch_code.unwrap_or_default()
                );
            }
        }
        Resolution::NotFound => println!("NOT FOUND"),
    }
    Ok(())
}
