-- Note: Evidence query 1 of the spec decision-tool. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the count of `decision added` lines and of `decision evaluated` lines from {since} to {since} + 30 days. One use of the feature is one evaluated record.
-- Columns: present (1 when a record was evaluated after {since}, else 0), reason (evaluated, only_added or none_added), added, evaluated. One row.
SELECT
    toUInt8(evaluated > 0) AS present,
    multiIf(evaluated > 0, 'evaluated', added > 0, 'only_added', 'none_added') AS reason,
    added,
    evaluated
FROM (
    SELECT
        countIf(Body = 'decision added') AS added,
        countIf(Body = 'decision evaluated') AS evaluated
    FROM otel.otel_logs
    WHERE ServiceName = 'harold'
      AND Body IN ('decision added', 'decision evaluated')
      AND Timestamp > {since:DateTime}
      AND Timestamp <= {since:DateTime} + INTERVAL 30 DAY
)
LIMIT 1
