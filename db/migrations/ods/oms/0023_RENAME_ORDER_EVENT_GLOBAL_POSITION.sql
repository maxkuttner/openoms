-- 0006 misspelled the event log's ordering column as `gloabl_position`. Every
-- reader had to carry the typo, and `OrderEventStore::load_stream` did not — it
-- queried `global_position` and would have failed the first time anything called
-- it. Rename the column rather than spread the typo further.
ALTER TABLE order_event RENAME COLUMN gloabl_position TO global_position;
