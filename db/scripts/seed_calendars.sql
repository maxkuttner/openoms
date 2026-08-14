-- Venue trading calendars: the timezone + close time authority.
--
-- These cannot come from the automated venue source. seed_venues.py loads the ISO
-- 10383 MIC registry, and that file carries no timezone column — only country and
-- city (see the note in 0001_CREATE_VENUE_TABLE.sql, which omits timezone for that
-- reason and points here instead). So the mapping is hand-curated, and every venue
-- the OMS can actually book against needs a row.
--
-- What depends on it: an option's `expiry_date` is a bare venue-local DATE (Alpaca
-- sends "YYYY-MM-DD", meaning an ET calendar date). The expiry job derives the real
-- UTC instant as `(expiry_date + close_time) AT TIME ZONE timezone`, so an option
-- whose venue has no calendar row never expires and never stops being subscribed.
-- Preflight names those at boot rather than letting them go quiet.
--
-- Timezone is an IANA name, not an offset: 16:00 in New York is 20:00Z in summer and
-- 21:00Z in winter, and Postgres applies the right one per date.
--
-- close_time is NULL for the crypto venues — they trade 24/7, and spot pairs carry
-- no expiry_date at all, so there is nothing to derive. NULL is the honest value; a
-- placeholder would invent an expiry instant for something that has none.
--
-- The equity/option venues are the MICs alpaca_exchange_to_mic() can emit
-- (src/adapters/alpaca.rs), plus OPRA for the listed options themselves. All US,
-- all 16:00 ET.
--
-- Run as mdm_master (owner of public), matching seed_currencies.sql. Depends on
-- venue being seeded first — a missing venue silently seeds no calendar, which
-- preflight then reports.

SET ROLE mdm_master;
SET search_path TO public;

INSERT INTO calendar (code, venue_id, timezone, close_time, description)
SELECT c.code, v.id, c.timezone, c.close_time, c.description
FROM (VALUES
    ('OPRA',    'America/New_York', TIME '16:00', 'US listed options (OPRA consolidated)'),
    ('XNAS',    'America/New_York', TIME '16:00', 'Nasdaq'),
    ('XNYS',    'America/New_York', TIME '16:00', 'New York Stock Exchange'),
    ('ARCX',    'America/New_York', TIME '16:00', 'NYSE Arca'),
    ('XASE',    'America/New_York', TIME '16:00', 'NYSE American'),
    ('BATS',    'America/New_York', TIME '16:00', 'Cboe BZX'),
    ('IEXG',    'America/New_York', TIME '16:00', 'IEX'),
    ('OTCM',    'America/New_York', TIME '16:00', 'OTC Markets'),
    ('BINANCE', 'UTC',              NULL,         'Binance — 24/7, no close'),
    ('BYBIT',   'UTC',              NULL,         'Bybit — 24/7, no close')
) AS c(code, timezone, close_time, description)
JOIN venue v ON v.code = c.code
ON CONFLICT (code) DO UPDATE SET
    venue_id    = EXCLUDED.venue_id,
    timezone    = EXCLUDED.timezone,
    close_time  = EXCLUDED.close_time,
    description = EXCLUDED.description,
    updated_at  = now();

RESET ROLE;
