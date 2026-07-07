//! Backfill FIGI onto the master instrument table.
//!
//! Seeding (`scripts/seed_instruments.py`) mints `public.instrument` from Databento
//! definitions but leaves `figi`/`cusip` NULL — FIGI is otherwise only stamped lazily
//! by the runtime resolver (`src/symbology_resolver.rs`) on the recon path. This binary
//! closes that gap: it batches every FIGI-less master row through the same
//! `crates/symbology` (OpenFIGI) engine the OMS uses, then stamps `instrument.figi`
//! `/cusip` and upserts an `OPENFIGI` xref row — so the master is FIGI-anchored right
//! after seeding, and `broker_sync` / recon can match on a stable global id instead of
//! a bare ticker string.
//!
//! Run after `seed-instruments` (see the `figi-backfill` Makefile target). Idempotent:
//! the `figi IS NULL` filter means re-runs only touch rows still unresolved.
//!
//!   cargo run --bin figi_backfill -- [--dry-run] [--include-derivatives]
//!                                    [--limit N] [--batch N]

use std::env;

use sqlx::{PgPool, Row};
use symbology::{
    openfigi_exch_code, Identifier, InMemoryCache, InstrumentQuery, OpenFigiClient, Resolution,
};

/// One master row awaiting a FIGI.
struct Candidate {
    id: i64,
    symbol: String,
    venue: String,
    isin: Option<String>,
    cusip: Option<String>,
}

struct Args {
    dry_run: bool,
    include_derivatives: bool,
    limit: Option<i64>,
    batch: usize,
}

fn parse_args() -> Args {
    let mut a = Args { dry_run: false, include_derivatives: false, limit: None, batch: 500 };
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => a.dry_run = true,
            "--include-derivatives" => a.include_derivatives = true,
            "--limit" => a.limit = it.next().and_then(|v| v.parse().ok()),
            "--batch" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<usize>().ok()).filter(|v| *v > 0) {
                    a.batch = v;
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "figi_backfill [--dry-run] [--include-derivatives] [--limit N] [--batch N]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

/// Build the DB URL from the same env vars as the service (src/main.rs).
fn database_url() -> String {
    let var = |k: &str| env::var(k).unwrap_or_else(|_| panic!("{k} must be set"));
    format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=disable",
        var("DB_USER"),
        var("DB_PASSWORD"),
        var("DB_HOST"),
        var("DB_PORT"),
        var("DB_NAME"),
    )
}

/// Master row -> the strongest query we can build. Precedence (figi→isin→cusip→ticker)
/// is applied inside the engine's `build_job`; we just supply what we hold plus the
/// venue's OpenFIGI exchange code for disambiguation.
fn query_for(c: &Candidate) -> InstrumentQuery {
    InstrumentQuery {
        isin: c.isin.clone(),
        cusip: c.cusip.clone(),
        ticker: Some(c.symbol.clone()),
        exch_code: openfigi_exch_code(&c.venue).map(str::to_string),
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let args = parse_args();

    let pool = PgPool::connect(&database_url()).await?;
    let engine: Identifier<OpenFigiClient, InMemoryCache> =
        Identifier::new(OpenFigiClient::new(env::var("OPENFIGI_API_KEY").ok()), InMemoryCache::new());

    // Candidate rows: FIGI-less, active, SPOT-only unless --include-derivatives.
    let mut sql = String::from(
        "SELECT id, symbol, venue, isin, cusip FROM instrument \
         WHERE figi IS NULL AND status = 'ACTIVE'",
    );
    if !args.include_derivatives {
        sql.push_str(" AND instrument_class = 'SPOT'");
    }
    sql.push_str(" ORDER BY id");
    if let Some(n) = args.limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }

    let candidates: Vec<Candidate> = sqlx::query(&sql)
        .fetch_all(&pool)
        .await?
        .iter()
        .map(|r| Candidate {
            id: r.get("id"),
            symbol: r.get("symbol"),
            venue: r.get("venue"),
            isin: r.get("isin"),
            cusip: r.get("cusip"),
        })
        .collect();

    println!(
        "figi_backfill: {} candidate(s){}",
        candidates.len(),
        if args.dry_run { " [dry-run]" } else { "" }
    );

    let (mut resolved, mut ambiguous, mut not_found, mut stamped) = (0u64, 0u64, 0u64, 0u64);

    for chunk in candidates.chunks(args.batch) {
        let queries: Vec<InstrumentQuery> = chunk.iter().map(query_for).collect();
        let results = engine.identify_batch(&queries).await?;

        for (c, res) in chunk.iter().zip(results) {
            match res {
                Resolution::Resolved(identity) => {
                    resolved += 1;
                    if args.dry_run {
                        println!("  {} @ {} -> {}", c.symbol, c.venue, identity.figi);
                        continue;
                    }
                    // Stamp the anchor on the master (only if still empty), then cache the
                    // OPENFIGI xref — same shape as src/symbology_resolver.rs.
                    sqlx::query(
                        "UPDATE instrument SET figi = COALESCE(figi, $2), \
                                               cusip = COALESCE(cusip, $3) WHERE id = $1",
                    )
                    .bind(c.id)
                    .bind(&identity.figi)
                    .bind(c.cusip.as_deref())
                    .execute(&pool)
                    .await?;

                    sqlx::query(
                        "INSERT INTO oms.instrument_xref \
                           (instrument_id, source_type, source_code, external_symbol, \
                            external_exchange, figi, method, confidence) \
                         VALUES ($1, 'OPENFIGI', 'OPENFIGI', $2, $3, $4, 'openfigi', 'resolved') \
                         ON CONFLICT (source_type, source_code, \
                             COALESCE(external_native_id, ''), COALESCE(external_symbol, ''), \
                             COALESCE(external_exchange, '')) \
                         DO UPDATE SET instrument_id = EXCLUDED.instrument_id, \
                                       figi = EXCLUDED.figi, updated_at = now()",
                    )
                    .bind(c.id)
                    .bind(&c.symbol)
                    .bind(&c.venue)
                    .bind(&identity.figi)
                    .execute(&pool)
                    .await?;
                    stamped += 1;
                }
                Resolution::Ambiguous(_) => ambiguous += 1,
                Resolution::NotFound => not_found += 1,
            }
        }
        println!(
            "  progress: resolved={resolved} ambiguous={ambiguous} not_found={not_found} stamped={stamped}"
        );
    }

    println!(
        "figi_backfill done: resolved={resolved} ambiguous={ambiguous} not_found={not_found} stamped={stamped}"
    );
    Ok(())
}
