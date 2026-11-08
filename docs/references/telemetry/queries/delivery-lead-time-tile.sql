-- Note: A stub. It returns no row, so the card shows HyperDX's empty state ("no data yet") until delivery-lead-time-tile fills it.
-- Waits for: delivery-lead-time-tile
-- Delivery lead time: the days from the idea to the user's check, in the 7 days, and the change against the 7 days before.
-- The spec delivery-lead-time-tile writes the real query. The table delivery-lead-time.sql on "Harold trends" shows the p50 by phase.
-- No row, not 0: the tile shows HyperDX's empty state.
-- Columns: days, days_change_pct.
SELECT
    CAST(NULL AS Nullable(Float64)) AS days,
    CAST(NULL AS Nullable(Float64)) AS days_change_pct
WHERE 0
LIMIT 1
