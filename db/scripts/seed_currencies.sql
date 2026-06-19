-- Seed common ISO 4217 currencies (idempotent). Instruments FK to currency(code),
-- so at minimum USD must exist before the instrument seeder runs.
-- Run as mdm_master (owner of public). Extend as new trading currencies appear.

SET ROLE mdm_master;
SET search_path TO public;

INSERT INTO currency (code, name, numeric_code, minor_units) VALUES
    ('USD', 'US Dollar',            '840', 2),
    ('EUR', 'Euro',                 '978', 2),
    ('GBP', 'Pound Sterling',       '826', 2),
    ('JPY', 'Yen',                  '392', 0),
    ('CHF', 'Swiss Franc',          '756', 2),
    ('CAD', 'Canadian Dollar',      '124', 2),
    ('AUD', 'Australian Dollar',    '036', 2),
    ('NZD', 'New Zealand Dollar',   '554', 2),
    ('HKD', 'Hong Kong Dollar',     '344', 2),
    ('CNY', 'Yuan Renminbi',        '156', 2),
    ('SGD', 'Singapore Dollar',     '702', 2),
    ('SEK', 'Swedish Krona',        '752', 2),
    ('NOK', 'Norwegian Krone',      '578', 2),
    ('DKK', 'Danish Krone',         '208', 2)
ON CONFLICT (code) DO NOTHING;

RESET ROLE;
