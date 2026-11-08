-- Note: Evidence query 1 of the spec flow-release. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: pod.spec.lead_time with phase = merged_to_released has a rise after {since}, so the first release after the merge reached the spec.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its Count minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole Count.
-- The query reads the points of 14 days before {since} for the previous point.
-- Columns: present (1 or 0), reason (phase_seen or no_release_yet). One row.
SELECT
    toUInt8(countIf(rise_count > 0 AND TimeUnix > {since:DateTime}) > 0) AS present,
    if(countIf(rise_count > 0 AND TimeUnix > {since:DateTime}) > 0, 'phase_seen', 'no_release_yet') AS reason
FROM (
    SELECT
        TimeUnix,
        if(cnt >= before_cnt, toInt64(cnt) - toInt64(before_cnt), toInt64(cnt)) AS rise_count
    FROM (
        SELECT
            TimeUnix,
            Count AS cnt,
            lagInFrame(Count, 1, toUInt64(0)) OVER (
                PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_cnt
        FROM otel.otel_metrics_histogram
        WHERE MetricName = 'pod.spec.lead_time'
          AND Attributes['phase'] = 'merged_to_released'
          AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
    )
)
