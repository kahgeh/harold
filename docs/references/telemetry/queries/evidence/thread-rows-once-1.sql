-- Note: Evidence query 1 of the spec thread-rows-once. Parameter {since:DateTime}: the release time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the fix acted, or no page opened a second stream.
-- duplicate_removed: the browser span thread.duplicate_row has a point after {since}. The page removed a copy that the user would have seen.
-- no_second_stream: {since} is more than 7 days ago, at least one app page connected line is in the 7 days after {since}, and none has
-- streams_for_page above 1.
-- too_early: neither is true.
-- Columns: present (1 or 0), reason (duplicate_removed, no_second_stream or too_early). One row.
SELECT
    toUInt8(removed > 0 OR quiet) AS present,
    multiIf(removed > 0, 'duplicate_removed', quiet, 'no_second_stream', 'too_early') AS reason
FROM (
    SELECT
        (SELECT count() FROM otel.otel_traces
         WHERE SpanName = 'thread.duplicate_row' AND Timestamp > {since:DateTime}) AS removed,
        (SELECT countIf(toUInt32OrZero(LogAttributes['streams_for_page']) > 1) = 0 AND count() > 0
                AND now() > {since:DateTime} + INTERVAL 7 DAY
         FROM otel.otel_logs
         WHERE Body = 'app page connected'
           AND Timestamp > {since:DateTime} AND Timestamp <= {since:DateTime} + INTERVAL 7 DAY) AS quiet
)
