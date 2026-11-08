-- Note: The slowest spec of the week is the first one to check when the delivery lead time grows. Read pod.spec.lead_time by spec.id and phase.
-- Waits for: nothing
-- Delivery lead time: the median days from the first draft of a spec to the release that holds it, for the specs released in the 7 days,
-- and the change against the specs released in the 7 days before.
-- The days of a spec are the sum of its four phases: open_to_approved, approved_to_started, started_to_completed and merged_to_released.
-- Each phase of a spec is recorded once, so the value of a phase is the Sum of the first point of its series. The join key is spec.id.
-- The release time of a spec is the time of its first merged_to_released point. A spec without all four phases does not count.
-- The first phases of a spec are recorded before its release, so the query reads all points of the metric, not only the last 14 days.
-- The median of an even number of sums is the mean of the two middle sums.
-- No spec released in the 7 days, or no merged_to_released point at all: no row, not 0. The tile shows HyperDX's empty state.
-- days_change_pct is the percent change against the 7 days before. It is empty when no spec was released in the 7 days before.
-- Columns: days, days_change_pct.
WITH
    phases AS (
        SELECT
            Attributes['spec.id'] AS spec,
            Attributes['phase'] AS phase,
            min(TimeUnix) AS recorded,
            argMin(Sum, TimeUnix) AS seconds
        FROM otel.otel_metrics_histogram
        WHERE MetricName = 'pod.spec.lead_time'
          AND Attributes['phase'] IN ('open_to_approved', 'approved_to_started', 'started_to_completed', 'merged_to_released')
        GROUP BY spec, phase
    ),
    released AS (
        SELECT
            spec,
            maxIf(recorded, phase = 'merged_to_released') AS released_at,
            sum(seconds) / 86400 AS spec_days
        FROM phases
        GROUP BY spec
        HAVING count() = 4
    )
SELECT
    this_days AS days,
    round((this_days - last_days) / nullIf(last_days, 0) * 100) AS days_change_pct
FROM (
    SELECT
        if(countIf(released_at > now() - INTERVAL 7 DAY) = 0, NULL, quantileExactInclusiveIf(0.5)(spec_days, released_at > now() - INTERVAL 7 DAY)) AS this_days,
        if(countIf(released_at > now() - INTERVAL 14 DAY AND released_at <= now() - INTERVAL 7 DAY) = 0, NULL,
           quantileExactInclusiveIf(0.5)(spec_days, released_at > now() - INTERVAL 14 DAY AND released_at <= now() - INTERVAL 7 DAY)) AS last_days
    FROM released
)
WHERE this_days IS NOT NULL
LIMIT 1
