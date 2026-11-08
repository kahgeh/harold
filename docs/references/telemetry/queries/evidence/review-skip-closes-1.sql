-- Note: Evidence query 1 of the spec review-skip-closes. Parameter {since:DateTime}: the release time of the spec, in UTC.
-- Waits for: nothing
-- Shows: after {since}, Harold read the three reviews files. It logged no pod file unreadable for reviews/agent-jobs.md,
-- reviews/model-stall.md or reviews/harder-held-out.md, and Harold logged at least one line (so Harold ran).
-- files_read: Harold logged lines and the three files logged nothing.
-- still_unreadable: at least one of the three files logged pod file unreadable.
-- no_harold_lines: Harold logged no line in the window, so there is nothing to judge.
-- The window runs from {since} to {since} + INTERVAL 7 DAY.
-- Columns: present (1 or 0), reason (files_read, still_unreadable or no_harold_lines), unreadable (the count of unreadable lines). One row.
SELECT
    toUInt8(harold > 0 AND unreadable = 0) AS present,
    multiIf(unreadable > 0, 'still_unreadable', harold > 0, 'files_read', 'no_harold_lines') AS reason,
    unreadable
FROM (
    SELECT
        countIf(ServiceName = 'harold') AS harold,
        countIf(Body = 'pod file unreadable'
                AND LogAttributes['file'] IN ('reviews/agent-jobs.md', 'reviews/model-stall.md', 'reviews/harder-held-out.md')) AS unreadable
    FROM otel.otel_logs
    WHERE Timestamp > {since:DateTime} AND Timestamp <= {since:DateTime} + INTERVAL 7 DAY
)
LIMIT 1
