-- Note: Medium specs over 8 hours are the first thing to check. A third or more of the medium specs over 8 hours in a week means the sizes are too small.
-- Waits for: work-status-history
-- Empty: slices per work record: no data yet (waits for work-status-history)
-- Slicing and sizing: the share of the specs of each size that are over the limit of the size, in percent.
-- It reads pod.spec.effort at measure build_worker_hours: the last value of each spec of the week. The limit is 4 hours for small and 8 hours for medium.
-- A spec counts in a week only when it changed or merged in that week. The README states that pod.spec.effort sends a value on every poll for each spec
-- that is not completed or withdrawn, and for each spec that entered one of those states in the last 30 days. So the points of a spec repeat
-- until the spec changes. A spec changed in a week when its state, its `revisions` value or its `build_worker_hours` value at the end of the week
-- differs from the value at the end of the week before, or when it has no point before the week. A spec that merged in the week changed its state.
-- The row "specs over 8 build hours, share of specs (%)" counts all specs, whatever their size, with the limit of 8 hours. It checks the split rule
-- (work over about a day is split) from actual build hours. New specs have no size line, so they are unsized.
-- The two share rows by size count only the specs written before 2026-11-14 (the ones that have a size line). They have no number about
-- 30 days after the last older spec merges, because pod.spec.effort stops sending a spec 30 days after its merge. A later change removes the two old rows when they are empty.
-- The sizes large and unsized have no limit, so the card has no share for them. The rows "specs counted" give the number of specs of each size.
-- The row "slices per work record" is the mean of the histogram harold.work.slices: the rise of Sum divided by the rise of Count. It has no product.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- Histogram rule: the same rule on the Count and the Sum. A reset is judged by the Count. Each point gives the rise of the Count and the rise of the Sum.
-- The product filter applies to the pod.spec.effort rows only. A week with no number for a row gives no number for that week.
-- Columns: series, this_week, last_week.
WITH
    touched AS (
        SELECT
            spec,
            max(this_changed) AS this_changed,
            max(last_changed) AS last_changed
        FROM (
            SELECT
                Attributes['spec.id'] AS spec,
                argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY) AS v2,
                argMaxIfOrNull(Attributes['state'], TimeUnix, TimeUnix > now() - INTERVAL 7 DAY) AS s2,
                argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS v1,
                argMaxIfOrNull(Attributes['state'], TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS s1,
                argMaxIfOrNull(Value, TimeUnix, TimeUnix <= now() - INTERVAL 14 DAY) AS v0,
                argMaxIfOrNull(Attributes['state'], TimeUnix, TimeUnix <= now() - INTERVAL 14 DAY) AS s0,
                isNotNull(v2) AND (isNull(v1) AND isNull(v0) OR ifNull(v2 != if(isNull(v1), v0, v1) OR s2 != if(isNull(s1), s0, s1), 1)) AS this_changed,
                isNotNull(v1) AND (isNull(v0) OR ifNull(v1 != v0 OR s1 != s0, 1)) AS last_changed
            FROM otel.otel_metrics_gauge
            WHERE MetricName = 'pod.spec.effort'
              AND Attributes['measure'] IN ('revisions', 'build_worker_hours')
              AND TimeUnix > now() - INTERVAL 28 DAY
              AND $__filters
            GROUP BY Attributes['spec.id'], Attributes['measure']
        )
        GROUP BY spec
    ),
    (SELECT groupArray(spec) FROM touched WHERE this_changed) AS this_specs,
    (SELECT groupArray(spec) FROM touched WHERE last_changed) AS last_specs
SELECT
    series,
    sum(this_value) AS this_week,
    sum(last_value) AS last_week
FROM (
    SELECT
        'specs counted: ' || size AS series,
        if(countIf(isNotNull(this_v)) = 0, NULL, toFloat64(countIf(isNotNull(this_v)))) AS this_value,
        if(countIf(isNotNull(last_v)) = 0, NULL, toFloat64(countIf(isNotNull(last_v)))) AS last_value
    FROM (
        SELECT
            Attributes['spec.size'] AS size,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND has(this_specs, Attributes['spec.id'])) AS this_v,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND has(last_specs, Attributes['spec.id'])) AS last_v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['measure'] = 'build_worker_hours'
          AND Attributes['spec.size'] IN ('small', 'medium')
          AND TimeUnix > now() - INTERVAL 14 DAY
          AND $__filters
        GROUP BY Attributes['spec.size'], Attributes['spec.id']
    )
    GROUP BY size
    UNION ALL
    SELECT
        'over limit, share of specs (%): ' || size AS series,
        100.0 * countIf(this_v > if(size = 'small', 4, 8)) / nullIf(countIf(isNotNull(this_v)), 0) AS this_value,
        100.0 * countIf(last_v > if(size = 'small', 4, 8)) / nullIf(countIf(isNotNull(last_v)), 0) AS last_value
    FROM (
        SELECT
            Attributes['spec.size'] AS size,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND has(this_specs, Attributes['spec.id'])) AS this_v,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND has(last_specs, Attributes['spec.id'])) AS last_v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['measure'] = 'build_worker_hours'
          AND Attributes['spec.size'] IN ('small', 'medium')
          AND TimeUnix > now() - INTERVAL 14 DAY
          AND $__filters
        GROUP BY Attributes['spec.size'], Attributes['spec.id']
    )
    GROUP BY size
    UNION ALL
    SELECT
        'specs over 8 build hours, share of specs (%)' AS series,
        100.0 * countIf(this_v > 8) / nullIf(countIf(isNotNull(this_v)), 0) AS this_value,
        100.0 * countIf(last_v > 8) / nullIf(countIf(isNotNull(last_v)), 0) AS last_value
    FROM (
        SELECT
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND has(this_specs, Attributes['spec.id'])) AS this_v,
            argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND has(last_specs, Attributes['spec.id'])) AS last_v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.effort'
          AND Attributes['measure'] = 'build_worker_hours'
          AND TimeUnix > now() - INTERVAL 14 DAY
          AND $__filters
        GROUP BY Attributes['spec.id']
    )
    UNION ALL
    SELECT
        'slices per work record' AS series,
        if(countIf(TimeUnix > now() - INTERVAL 7 DAY AND rise_count > 0) = 0, NULL, sumIf(rise_sum, TimeUnix > now() - INTERVAL 7 DAY AND rise_count > 0) / sumIf(rise_count, TimeUnix > now() - INTERVAL 7 DAY AND rise_count > 0)) AS this_value,
        if(countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND rise_count > 0) = 0, NULL, sumIf(rise_sum, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND rise_count > 0) / sumIf(rise_count, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND rise_count > 0)) AS last_value
    FROM (
        SELECT
            Attributes,
            TimeUnix,
            if(cnt >= before_cnt, toInt64(cnt) - toInt64(before_cnt), toInt64(cnt)) AS rise_count,
            if(cnt >= before_cnt, total - before_total, total) AS rise_sum
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                Count AS cnt,
                Sum AS total,
                lagInFrame(Count, 1, toUInt64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_cnt,
                lagInFrame(Sum, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_total
            FROM otel.otel_metrics_histogram
            WHERE MetricName = 'harold.work.slices'
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
    GROUP BY series
)
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
