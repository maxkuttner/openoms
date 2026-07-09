//! Poke the Databento `UniverseSource` without a DB or the seeder.
//!
//! Needs `DATABENTO_API_KEY` in the env.
//!
//! Usage:
//!   cargo run -p dataprovider --example probe -- [CATEGORY] [DATASET] [SYMBOLS...] [--fetch]
//!
//! Defaults: equity XNAS.ITCH AAPL MSFT (estimate only).
//!   CATEGORY : equity | option | future
//!   SYMBOLS  : space-separated, or `ALL` for the whole dataset
//!   --fetch  : also pull definitions (COSTS MONEY). Without it, estimate only (free).
//!
//! Examples:
//!   cargo run -p dataprovider --example probe -- equity XNAS.ITCH AAPL MSFT
//!   cargo run -p dataprovider --example probe -- equity EQUS.SUMMARY ALL
//!   cargo run -p dataprovider --example probe -- option OPRA.PILLAR SPY --fetch

use dataprovider::{Category, DataProvider, DatabentoClient, SType, UniverseSource, UniverseSpec};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load DATABENTO_API_KEY from the repo-root .env (walks up from cwd).
    dotenvy::dotenv().ok();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let fetch = args.iter().any(|a| a == "--fetch");
    args.retain(|a| a != "--fetch");

    let category = match args.first().map(String::as_str) {
        Some("option") => Category::Option,
        Some("future") => Category::Future,
        _ => Category::Equity,
    };
    let dataset = args.get(1).cloned().unwrap_or_else(|| "XNAS.ITCH".into());
    let symbols: Vec<String> = match args.get(2).map(String::as_str) {
        None => vec!["AAPL".into(), "MSFT".into()],
        Some("ALL") => Vec::new(),
        Some(_) => args[2..].to_vec(),
    };

    // Option universes read parent symbols off `dataset`; equities use raw_symbol.
    let stype_in = match category {
        Category::Option => SType::Parent,
        _ => SType::RawSymbol,
    };

    let spec = UniverseSpec {
        code: "PROBE".into(),
        description: Some("ad-hoc probe".into()),
        category,
        dataset: dataset.clone(),
        option_dataset: None,
        symbols: symbols.clone(),
        stype_in,
        include_options: false,
    };

    let dbc = DatabentoClient::from_env()?;
    dbc.set_catalog(vec![spec.clone()]).await;

    println!("provider    : {}", dbc.code());
    println!("category    : {:?}", category);
    println!("dataset     : {dataset}");
    println!(
        "symbols     : {}",
        if symbols.is_empty() { "ALL".into() } else { symbols.join(",") }
    );

    println!("\n-- discover --");
    for u in dbc.discover().await? {
        println!("  {} [{:?}] {}", u.code, u.category, u.dataset);
    }

    println!("\n-- estimate_cost (free) --");
    let est = dbc.estimate_cost(&spec).await?;
    println!("  ${:.4}  (symbols: {:?})", est.usd, est.symbol_count);

    if !fetch {
        println!("\n(estimate only. pass --fetch to pull definitions — COSTS MONEY.)");
        return Ok(());
    }

    println!("\n-- fetch_definitions (billed) --");
    let defs = dbc.fetch_definitions(&spec).await?;
    println!("  {} instrument(s)\n", defs.len());
    for d in defs.iter().take(20) {
        print!(
            "  {:<24} {:<8} {:<7} {:<4} tick={} contract={}",
            d.symbol, d.venue, d.instrument_class, d.currency, d.price_increment, d.contract_size
        );
        if let Some(dv) = &d.derivative {
            print!(
                "  under={} {:?} strike={:?} exp={:?}",
                dv.underlying_symbol, dv.option_kind, dv.strike_price, dv.expiry_date
            );
        }
        println!();
    }
    if defs.len() > 20 {
        println!("  … {} more", defs.len() - 20);
    }
    Ok(())
}
