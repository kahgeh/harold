-- Note: Evidence query 1 of the spec chief-line-own-words. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the count of `summary question dropped` lines and of `turn complete received` lines from {since} to {since} + 30 days.
-- Columns: present (1 when a turn ended after {since}, else 0), reason (checked_no_drop, dropped_question or no_turns), dropped, turns. One row.
SELECT
    toUInt8(turns > 0) AS present,
    multiIf(dropped > 0, 'dropped_question', turns > 0, 'checked_no_drop', 'no_turns') AS reason,
    dropped,
    turns
FROM (
    SELECT
        countIf(Body = 'summary question dropped') AS dropped,
        countIf(Body = 'turn complete received') AS turns
    FROM otel.otel_logs
    WHERE Body IN ('summary question dropped', 'turn complete received')
      AND Timestamp > {since:DateTime}
      AND Timestamp <= {since:DateTime} + INTERVAL 30 DAY
)
LIMIT 1
