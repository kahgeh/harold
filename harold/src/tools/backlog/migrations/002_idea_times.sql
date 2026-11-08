ALTER TABLE items ADD COLUMN first_put_at_ms INTEGER;
ALTER TABLE items ADD COLUMN opened_at_ms INTEGER;
-- The store has no older time than the last put, so an existing row takes that.
UPDATE items SET first_put_at_ms = updated_at_ms;
