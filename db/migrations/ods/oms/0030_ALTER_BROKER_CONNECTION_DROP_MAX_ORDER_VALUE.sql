-- Undoes 0029: the per-connection max order value was withdrawn before any
-- release shipped it. 0029 stays in the history because databases that
-- tracked main had already applied it; this brings every database back to the
-- same schema. Dropping the column drops its two CHECK constraints with it.
ALTER TABLE broker_connection DROP COLUMN max_order_value;
