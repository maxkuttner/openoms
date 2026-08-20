//! Reference data — the port of `db/scripts/seed.sh` and `seed_venues.py`.
//!
//! The Python seeder reshaped the ISO 10383 registry and loaded it with `\copy`
//! into a temp table before upserting. Here the reshape is `parse_mic_csv` and the
//! load is a multi-row INSERT, which removes the last reason to have Python
//! installed.

use sqlx::PgPool;

use super::assets;

/// One `venue` row, reshaped from the registry's 17 columns down to the six the
/// table keeps.
#[derive(Debug)]
pub struct VenueRow {
    pub code: String,
    pub name: String,
    pub country: String,
    pub city: String,
    pub mic: String,
    pub status: String,
}

const COL_MIC: &str = "MIC";
const COL_OPERATING: &str = "OPERATING MIC";
const COL_NAME: &str = "MARKET NAME-INSTITUTION DESCRIPTION";
const COL_COUNTRY: &str = "ISO COUNTRY CODE (ISO 3166)";
const COL_CITY: &str = "CITY";
const COL_STATUS: &str = "STATUS";

/// Reshape the registry. Returns `Err` naming the missing column when the header
/// is not what we expect — a silent empty result would seed nothing and leave
/// every instrument failing its venue foreign key later.
pub fn parse_mic_csv(text: &str) -> Result<Vec<VenueRow>, String> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|e| format!("unreadable CSV header: {e}"))?.clone();

    let index_of = |name: &str| -> Result<usize, String> {
        headers
            .iter()
            .position(|h| h.trim() == name)
            .ok_or_else(|| format!("column {name:?} not found in MIC registry header"))
    };
    let (i_mic, i_oprt, i_name, i_country, i_city, i_status) = (
        index_of(COL_MIC)?,
        index_of(COL_OPERATING)?,
        index_of(COL_NAME)?,
        index_of(COL_COUNTRY)?,
        index_of(COL_CITY)?,
        index_of(COL_STATUS)?,
    );

    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for record in reader.records() {
        let r = record.map_err(|e| format!("malformed CSV row: {e}"))?;
        let get = |i: usize| r.get(i).unwrap_or("").trim().to_string();

        let code = get(i_mic);
        // venue.code is the primary key; the registry repeats a MIC across
        // segment rows, so the first occurrence wins.
        if code.is_empty() || !seen.insert(code.clone()) {
            continue;
        }
        let name = {
            let n = get(i_name);
            if n.is_empty() { code.clone() } else { n }
        };
        let status = match get(i_status).to_uppercase().as_str() {
            "DELETED" | "EXPIRED" => "INACTIVE",
            _ => "ACTIVE",
        };
        rows.push(VenueRow {
            code,
            name,
            country: get(i_country),
            city: get(i_city),
            mic: get(i_oprt),
            status: status.to_string(),
        });
    }
    Ok(rows)
}

/// Upsert every venue from the embedded registry. Returns the row count.
///
/// Sent as arrays through UNNEST rather than row-by-row: 2,856 individual
/// statements would take seconds where one takes milliseconds.
pub async fn seed_venues(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let rows = parse_mic_csv(assets::MIC_CSV)
        .map_err(|e| sqlx::Error::Protocol(format!("MIC registry: {e}")))?;

    let codes: Vec<String> = rows.iter().map(|r| r.code.clone()).collect();
    let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
    let countries: Vec<String> = rows.iter().map(|r| r.country.clone()).collect();
    let cities: Vec<String> = rows.iter().map(|r| r.city.clone()).collect();
    let mics: Vec<String> = rows.iter().map(|r| r.mic.clone()).collect();
    let statuses: Vec<String> = rows.iter().map(|r| r.status.clone()).collect();

    // SET ROLE, the INSERT and RESET ROLE must run on the same connection: a
    // pooled `&PgPool` checks out a (possibly different) connection per call,
    // which could run the INSERT without the oms role active, or return a
    // connection to the pool with it still set for the next borrower.
    let mut tx = pool.begin().await?;

    sqlx::raw_sql("SET ROLE oms; SET search_path TO public;")
        .execute(&mut *tx)
        .await?;
    let affected = sqlx::query(
        "INSERT INTO venue (code, name, country, city, mic, status) \
         SELECT code, name, NULLIF(country, ''), NULLIF(city, ''), NULLIF(mic, ''), status \
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[]) \
              AS t(code, name, country, city, mic, status) \
         ON CONFLICT (code) DO UPDATE SET \
            name = EXCLUDED.name, country = EXCLUDED.country, city = EXCLUDED.city, \
            mic = EXCLUDED.mic, status = EXCLUDED.status, updated_at = now()",
    )
    .bind(&codes).bind(&names).bind(&countries).bind(&cities).bind(&mics).bind(&statuses)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::raw_sql("RESET ROLE;").execute(&mut *tx).await?;

    tx.commit().await?;

    Ok(affected)
}

/// Currencies, venues, crypto venues, calendars — in dependency order.
///
/// Venues must land before calendars, which join to them by code, and before any
/// instrument seeding, which foreign-keys to them.
pub async fn seed_reference_data(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let seed_sql = assets::seed_sql();
    let (currencies, rest) = seed_sql.split_at(1);
    for (name, sql) in currencies {
        tracing::info!("seeding {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }

    tracing::info!("seeding venues from the ISO 10383 registry");
    let venues = seed_venues(pool).await?;

    for (name, sql) in rest {
        tracing::info!("seeding {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    Ok(venues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::database::assets::MIC_CSV;

    // MIC and OPERATING MIC are deliberately different per row (a segment MIC's
    // operating MIC is its parent market) so a code/mic column swap in
    // `parse_mic_csv` would fail `maps_registry_columns_onto_venue` instead of
    // passing unnoticed.
    const SAMPLE: &str = "\"MIC\",\"OPERATING MIC\",\"OPRT/SGMT\",\"MARKET NAME-INSTITUTION DESCRIPTION\",\"LEGAL ENTITY NAME\",\"LEI\",\"MARKET CATEGORY CODE\",\"ACRONYM\",\"ISO COUNTRY CODE (ISO 3166)\",\"CITY\",\"WEBSITE\",\"STATUS\",\"CREATION DATE\",\"LAST UPDATE DATE\",\"LAST VALIDATION DATE\",\"EXPIRY DATE\",\"COMMENTS\"
\"XBOS\",\"XNAS\",\"SGMT\",\"NASDAQ BOSTON\",\"\",\"\",\"NSPD\",\"\",\"US\",\"NEW YORK\",\"\",\"ACTIVE\",\"\",\"\",\"\",\"\",\"\"
\"XOLD\",\"XOLD\",\"OPRT\",\"DEFUNCT EXCHANGE\",\"\",\"\",\"NSPD\",\"\",\"US\",\"CHICAGO\",\"\",\"DELETED\",\"\",\"\",\"\",\"\",\"\"
\"XBOS\",\"XNAS\",\"SGMT\",\"NASDAQ BOSTON DUPLICATE\",\"\",\"\",\"NSPD\",\"\",\"US\",\"NEW YORK\",\"\",\"ACTIVE\",\"\",\"\",\"\",\"\",\"\"
\"XBLK\",\"XBLK\",\"OPRT\",\"\",\"\",\"\",\"NSPD\",\"\",\"US\",\"NEW YORK\",\"\",\"ACTIVE\",\"\",\"\",\"\",\"\",\"\"";

    #[test]
    fn maps_registry_columns_onto_venue() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        let nasdaq = &rows[0];
        assert_eq!(nasdaq.code, "XBOS");
        assert_eq!(nasdaq.name, "NASDAQ BOSTON");
        assert_eq!(nasdaq.country, "US");
        assert_eq!(nasdaq.city, "NEW YORK");
        assert_eq!(nasdaq.mic, "XNAS", "mic should hold the OPERATING MIC, not a copy of code");
    }

    /// The registry keeps historical entries. A DELETED or EXPIRED MIC is a real
    /// venue that no longer trades, so it is kept but marked INACTIVE rather than
    /// dropped — an instrument may still reference it.
    #[test]
    fn retired_mics_become_inactive() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        let old = rows.iter().find(|r| r.code == "XOLD").expect("XOLD present");
        assert_eq!(old.status, "INACTIVE");
        assert_eq!(rows[0].status, "ACTIVE");
    }

    /// venue.code is a primary key, so a repeated MIC must not produce two rows —
    /// the first wins, matching the Python seeder it replaces.
    #[test]
    fn keeps_only_the_first_of_a_duplicate_mic() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        assert_eq!(rows.iter().filter(|r| r.code == "XBOS").count(), 1);
        assert_eq!(rows[0].name, "NASDAQ BOSTON", "first occurrence should win");
    }

    /// A blank market name falls back to the MIC code, matching the Python
    /// seeder — a venue must never have an empty display name.
    #[test]
    fn falls_back_to_the_code_when_name_is_blank() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        let blank = rows.iter().find(|r| r.code == "XBLK").expect("XBLK present");
        assert_eq!(blank.name, "XBLK");
    }

    /// A header change in a future ISO release must fail loudly at parse time,
    /// not silently seed an empty venue table.
    #[test]
    fn rejects_a_file_without_the_expected_columns() {
        let err = parse_mic_csv("\"FOO\",\"BAR\"\n1,2").expect_err("should reject");
        assert!(err.contains("MIC"), "error should name the missing column: {err}");
    }

    /// The committed registry must parse — this is the file that actually ships.
    #[test]
    fn parses_the_committed_registry() {
        let rows = parse_mic_csv(MIC_CSV).expect("committed CSV must parse");
        assert!(rows.len() > 2000, "expected the full registry, got {}", rows.len());
        assert!(rows.iter().any(|r| r.code == "XNAS"));
        assert!(rows.iter().any(|r| r.code == "OPRA"));
    }
}
