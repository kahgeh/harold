-- Note: A day with over 2 slot hours of free slots with work waiting needs a look. So does a spec with over 2 redo hours.
-- Waits for: nothing
-- Waste: the hours of the 7 days, rounded to whole hours: the slot hours in which a slot was free while work waited, plus the redo hours.
-- The slot hours read pod.slots.idle_with_work: the sum of the points times 30, divided by 3600.
-- The redo hours read pod.spec.effort at measure redo_hours: the last value of each spec of the week, summed.
-- A spec counts in a week only when it changed or merged in that week. The README states that pod.spec.effort sends a value on every poll for each spec
-- that is not completed or withdrawn, and for each spec that entered one of those states in the last 30 days. So the points of a spec repeat
-- until the spec changes. A spec changed in a week when its state, its `revisions` value or its `build_worker_hours` value at the end of the week
-- differs from the value at the end of the week before, or when it has no point before the week. A spec that merged in the week changed its state.
-- A source with no point in the 7 days gives no hours. With no source at all there is no row: the tile shows HyperDX's empty state, not 0.
-- hours_change_pct is the percent change against the 7 days before. It is empty when the 7 days before have no hours.
-- Columns: hours, hours_change_pct.
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
            GROUP BY Attributes['spec.id'], Attributes['measure']
        )
        GROUP BY spec
    ),
    (SELECT groupArray(spec) FROM touched WHERE this_changed) AS this_specs,
    (SELECT groupArray(spec) FROM touched WHERE last_changed) AS last_specs,
    slots AS (
        SELECT
            countIf(TimeUnix > now() - INTERVAL 7 DAY) AS this_n,
            sumIf(Value, TimeUnix > now() - INTERVAL 7 DAY) * 30 / 3600 AS this_hours,
            countIf(TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_n,
            sumIf(Value, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) * 30 / 3600 AS last_hours
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.slots.idle_with_work'
          AND TimeUnix > now() - INTERVAL 14 DAY
    ),
    redo AS (
        SELECT
            count(this_v) AS this_n,
            sum(this_v) AS this_hours,
            count(last_v) AS last_n,
            sum(last_v) AS last_hours
        FROM (
            SELECT
                argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND has(this_specs, Attributes['spec.id'])) AS this_v,
                argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY AND has(last_specs, Attributes['spec.id'])) AS last_v
            FROM otel.otel_metrics_gauge
            WHERE MetricName = 'pod.spec.effort'
              AND Attributes['measure'] = 'redo_hours'
              AND TimeUnix > now() - INTERVAL 14 DAY
            GROUP BY Attributes['spec.id']
        )
    )
SELECT
    round(this_total) AS hours,
    round((this_total - last_total) / nullIf(last_total, 0) * 100) AS hours_change_pct
FROM (
    SELECT
        if(slots.this_n = 0 AND redo.this_n = 0, NULL, ifNull(slots.this_hours, 0) + ifNull(redo.this_hours, 0)) AS this_total,
        if(slots.last_n = 0 AND redo.last_n = 0, NULL, ifNull(slots.last_hours, 0) + ifNull(redo.last_hours, 0)) AS last_total
    FROM slots
    CROSS JOIN redo
)
WHERE this_total IS NOT NULL
LIMIT 1
