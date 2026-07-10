-- Catalog of the main Databento datasets, exposed as selectable instrument
-- universes. Everything is disabled — the user opts in by running the seeder's
-- interactive picker (`make seed-instruments`) and ticking the wanted universes.
--
-- Idempotent: ON CONFLICT DO NOTHING so re-running db-seed never clobbers the
-- user's `enabled` toggles or seed-state. A whole-dataset universe (no rows in
-- instrument_universe_symbol) seeds every symbol (ALL_SYMBOLS).
--
-- NOTE: v1 seeding handles category=EQUITY. OPTION/FUTURE datasets are catalogued
-- here for discoverability but the loader warns + skips them until per-category
-- mapping lands.

INSERT INTO instrument_universe
    (code, description, category, dataset, option_dataset, stype_in, include_options, enabled)
VALUES
    -- US equities — exchange feeds
    ('XNAS_ITCH',      'Nasdaq TotalView-ITCH',                 'EQUITY', 'XNAS.ITCH',      NULL, 'raw_symbol', false, false),
    ('XNAS_BASIC',     'Nasdaq Basic',                          'EQUITY', 'XNAS.BASIC',     NULL, 'raw_symbol', false, false),
    ('XBOS_ITCH',      'Nasdaq BX (TotalView-ITCH)',            'EQUITY', 'XBOS.ITCH',      NULL, 'raw_symbol', false, false),
    ('XPSX_ITCH',      'Nasdaq PSX (TotalView-ITCH)',           'EQUITY', 'XPSX.ITCH',      NULL, 'raw_symbol', false, false),
    ('XNYS_PILLAR',    'NYSE (Pillar)',                         'EQUITY', 'XNYS.PILLAR',    NULL, 'raw_symbol', false, false),
    ('XASE_PILLAR',    'NYSE American (Pillar)',                'EQUITY', 'XASE.PILLAR',    NULL, 'raw_symbol', false, false),
    ('XCHI_PILLAR',    'NYSE Chicago (Pillar)',                 'EQUITY', 'XCHI.PILLAR',    NULL, 'raw_symbol', false, false),
    ('XCIS_TRADESBBO', 'NYSE National (Trades & BBO)',          'EQUITY', 'XCIS.TRADESBBO', NULL, 'raw_symbol', false, false),
    ('ARCX_PILLAR',    'NYSE Arca (Pillar)',                    'EQUITY', 'ARCX.PILLAR',    NULL, 'raw_symbol', false, false),
    ('IEXG_TOPS',      'IEX (TOPS)',                            'EQUITY', 'IEXG.TOPS',      NULL, 'raw_symbol', false, false),
    ('EPRL_DOM',       'MIAX Pearl Equities (Depth)',           'EQUITY', 'EPRL.DOM',       NULL, 'raw_symbol', false, false),
    -- US equities — consolidated
    ('EQUS_SUMMARY',   'Databento US Equities Summary',         'EQUITY', 'EQUS.SUMMARY',   NULL, 'raw_symbol', false, false),
    -- Options
    ('OPRA_PILLAR',    'OPRA — all US options',                 'OPTION', 'OPRA.PILLAR',    NULL, 'raw_symbol', false, false),
    -- Futures
    ('GLBX_MDP3',      'CME Globex (MDP 3.0)',                  'FUTURE', 'GLBX.MDP3',      NULL, 'raw_symbol', false, false),
    ('IFEU_IMPACT',    'ICE Futures Europe (iMpact)',           'FUTURE', 'IFEU.IMPACT',    NULL, 'raw_symbol', false, false),
    ('NDEX_IMPACT',    'ICE Endex (iMpact)',                    'FUTURE', 'NDEX.IMPACT',    NULL, 'raw_symbol', false, false)
ON CONFLICT (code) DO NOTHING;

-- Option universes (e.g. OPRA_PILLAR) get their underlyings from the cockpit
-- picker at seed time, written to instrument_universe_symbol; none are seeded here.
