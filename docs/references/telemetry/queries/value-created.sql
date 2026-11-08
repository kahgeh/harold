-- Note: A stub. It returns no row, so the card shows HyperDX's empty state ("no data yet") until value-created-signals fills it.
-- Waits for: value-created-signals
-- Value created: the work records that the user checked in the 7 days, and the change against the 7 days before.
-- The spec value-created-signals writes the real query. Until then no signal says that the user checked a work record.
-- No row, not 0: the tile shows HyperDX's empty state.
-- Columns: value, value_change_pct.
SELECT
    CAST(NULL AS Nullable(Float64)) AS value,
    CAST(NULL AS Nullable(Float64)) AS value_change_pct
WHERE 0
LIMIT 1
