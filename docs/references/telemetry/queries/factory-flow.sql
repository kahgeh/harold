-- Note: The flow stage with the highest median wait is the first one to ask about.
-- Waits for: nothing
-- Empty: idea, merge and installed: no data yet (has_data is 0 and the numbers are empty)
-- The factory flow, one row for each flow stage, in order: idea, spec, architect review, approval, build, merge, release, installed. All products together.
-- Items: the newest export of pod.inventory.items for the list of the stage, summed over the products. It is the rule of work-waiting.sql
-- (rows "specs: <list>", the first SELECT of "this" week), with one change: it takes the newest export of each list,
-- because the release tool exports merged_not_released on its own clock (docs/references/telemetry/README.md, "Inventory").
-- Median wait: the median of the specs merged in the last 30 days, in seconds. Each spec gives one wait.
-- A wait is the mean of a lead time, the rise of Sum divided by the rise of Count, by the rise rule of delivery-lead-time.sql
-- (the rows "spec: <phase>", the inner SELECT with lagInFrame): a first point, or a point below the previous one, rises by its whole value.
-- The query reads the points of pod.spec.lead_time of the last 30 days. Harold records one lead time for each spec and phase.
-- spec: items = in_spec_writing. The wait is the phase spec_writing of each spec that has it (empty when none has it).
-- architect review: items = in_architect_review. The wait is the phase architect_review of each spec that has it.
-- approval: items = awaiting_approval minus in_spec_writing and in_architect_review (never below 0). The wait is the phase approval_wait of
-- each spec, or the phase open_to_approved when the spec has no approval_wait.
-- build: the phase approved_to_started plus the phase started_to_completed of the same spec, joined by spec.id. A spec with only one of the two phases gives no wait.
-- release: items = merged_not_released. The wait is not recorded yet: median_wait_s is empty and has_data is 1.
-- This query reads no product filter. It stands alone, so bin/logq --named factory-flow and Harold print the same rows.
-- Columns: stage, items, median_wait_s, has_data.
WITH
latest AS (
    SELECT
        Attributes['list'] AS list,
        sum(Value) AS n
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.inventory.items'
      AND Attributes['list'] IN ('awaiting_approval', 'approved_not_started', 'in_progress', 'merged_not_released',
                                   'in_spec_writing', 'in_architect_review')
      AND (Attributes['list'], TimeUnix) IN (
          SELECT Attributes['list'], max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'pod.inventory.items' AND TimeUnix > now() - INTERVAL 7 DAY
          GROUP BY Attributes['list']
      )
    GROUP BY list
),
rises AS (
    SELECT
        phase,
        spec_id,
        rise_sum / rise_count AS wait_s
    FROM (
        SELECT
            Attributes['phase'] AS phase,
            Attributes['spec.id'] AS spec_id,
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
            WHERE MetricName = 'pod.spec.lead_time'
              AND TimeUnix > now() - INTERVAL 30 DAY
        )
        WHERE TimeUnix > now() - INTERVAL 30 DAY
    )
    WHERE rise_count > 0
),
per_spec AS (
    SELECT
        spec_id,
        sumIf(wait_s, phase = 'open_to_approved') AS approval_s,
        countIf(phase = 'open_to_approved') AS has_approval,
        sumIf(wait_s, phase = 'spec_writing') AS writing_s,
        countIf(phase = 'spec_writing') AS has_writing,
        sumIf(wait_s, phase = 'architect_review') AS review_s,
        countIf(phase = 'architect_review') AS has_review,
        sumIf(wait_s, phase = 'approval_wait') AS waiting_s,
        countIf(phase = 'approval_wait') AS has_waiting,
        sumIf(wait_s, phase IN ('approved_to_started', 'started_to_completed')) AS build_s,
        uniqExactIf(phase, phase IN ('approved_to_started', 'started_to_completed')) AS build_phases
    FROM rises
    GROUP BY spec_id
)
SELECT stage, items, median_wait_s, has_data
FROM (
    SELECT 1 AS ord, 'idea' AS stage, CAST(NULL AS Nullable(Float64)) AS items, CAST(NULL AS Nullable(Float64)) AS median_wait_s, 0 AS has_data
    UNION ALL
    SELECT
        2,
        'spec',
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'in_spec_writing'), 0)),
        (SELECT if(countIf(has_writing > 0) = 0, NULL, quantileExactIf(0.5)(writing_s, has_writing > 0)) FROM per_spec),
        1
    UNION ALL
    SELECT
        3,
        'architect review',
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'in_architect_review'), 0)),
        (SELECT if(countIf(has_review > 0) = 0, NULL, quantileExactIf(0.5)(review_s, has_review > 0)) FROM per_spec),
        1
    UNION ALL
    SELECT
        4,
        'approval',
        toNullable(greatest(
            coalesce((SELECT n FROM latest WHERE list = 'awaiting_approval'), 0)
            - coalesce((SELECT n FROM latest WHERE list = 'in_spec_writing'), 0)
            - coalesce((SELECT n FROM latest WHERE list = 'in_architect_review'), 0), 0)),
        (SELECT if(countIf(has_waiting > 0 OR has_approval > 0) = 0, NULL, quantileExactIf(0.5)(if(has_waiting > 0, waiting_s, approval_s), has_waiting > 0 OR has_approval > 0)) FROM per_spec),
        1
    UNION ALL
    SELECT
        5,
        'build',
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'approved_not_started'), 0) + coalesce((SELECT n FROM latest WHERE list = 'in_progress'), 0)),
        (SELECT if(countIf(build_phases = 2) = 0, NULL, quantileExactIf(0.5)(build_s, build_phases = 2)) FROM per_spec),
        1
    UNION ALL
    SELECT 6, 'merge', NULL, NULL, 0
    UNION ALL
    SELECT
        7,
        'release',
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'merged_not_released'), 0)),
        NULL,
        1
    UNION ALL
    SELECT 8, 'installed', NULL, NULL, 0
)
ORDER BY ord
