-- Note: Evidence query 1 of the spec flow-installed. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: the first release and the first install of it after the merge
-- Shows: harold.release.install_lag has a rise after {since}, so a Harold of a new release started and recorded the time from its tag to its install.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its Count minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole Count.
-- The query reads the points of 14 days before {since} for the previous point.
-- Columns: present (1 or 0), reason (install_seen or no_install_yet). One row.
SELECT
    toUInt8(countIf(rise_count > 0 AND TimeUnix > {since:DateTime}) > 0) AS present,
    if(countIf(rise_count > 0 AND TimeUnix > {since:DateTime}) > 0, 'install_seen', 'no_install_yet') AS reason
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
        WHERE MetricName = 'harold.release.install_lag'
          AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
    )
)
