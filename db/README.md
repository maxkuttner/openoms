# db

Everything that provisions the database: `migrations/` (the versioned schema, applied by `oms database migrate`), `access/` (role and grant definitions), `scripts/` (one-off seed SQL for calendars/venues/currencies), and `data/` (the ISO 10383 MIC registry loaded at seed time).
