-- Note: The flow stage with the highest median wait is the first one to ask about.
-- Waits for: nothing
-- Empty: installed: no data yet (has_data is 0 and the median is empty); its items is the count of specs installed without evidence
-- The factory flow, one row for each flow stage, in order: idea, spec, architect review, approval, build, merge, release, installed. All products together.
-- Items: the newest export of pod.inventory.items for the list of the stage, summed over the products. It is the rule of work-waiting.sql
-- (rows "specs: <list>", the first SELECT of "this" week), with one change: it takes the newest export of each list,
-- because the release tool exports merged_not_released on its own clock (docs/references/telemetry/README.md, "Inventory").
-- Median wait: the median of the specs merged in the last 30 days, in seconds. Each spec gives one wait.
-- A wait is the mean of a lead time, the rise of Sum divided by the rise of Count, by the rise rule of delivery-lead-time.sql
-- (the rows "spec: <phase>", the inner SELECT with lagInFrame): a first point, or a point below the previous one, rises by its whole value.
-- The query reads the points of pod.spec.lead_time of the last 30 days. Harold records one lead time for each spec and phase.
-- idea: items = idea (the backlog items with no spec file). The wait is the phase idea_to_open of each spec that has it (empty when none has it).
-- spec: items = in_spec_writing. The wait is the phase spec_writing of each spec that has it (empty when none has it).
-- architect review: items = in_architect_review. The wait is the phase architect_review of each spec that has it.
-- approval: items = awaiting_approval minus in_spec_writing and in_architect_review (never below 0). The wait is the phase approval_wait of
-- each spec, or the phase open_to_approved when the spec has no approval_wait.
-- build: items = approved_not_started plus in_progress minus ready_not_merged (never below 0). The wait is the phase approved_to_started plus the phase
-- started_to_ready of the same spec, joined by spec.id, when the spec has a ready file; else approved_to_started plus started_to_completed, as before.
-- A spec with no approved_to_started, or with neither started_to_ready nor started_to_completed, gives no wait.
-- merge: items = ready_not_merged. The wait is the phase ready_to_completed of each spec that has it (empty when none has it).
-- release: items = merged_not_released. The wait is the phase merged_to_released of each spec that has it (empty when none has it).
-- installed: items = the specs whose newest pod.evidence.missing point of the last 10 minutes is 1 (the evidence file says missing). It is a count of
-- specs, with no spec id. The median is empty and has_data stays 0: the stage has no wait. It adds no new reader of ClickHouse (ADR 0037).
-- A spec that restarts its series gives more than one rise: its wait is the mean of them, so it counts once.
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
                                   'in_spec_writing', 'in_architect_review', 'ready_not_merged', 'idea')
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
        sumIf(wait_s, phase = 'idea_to_open') AS idea_s,
        countIf(phase = 'idea_to_open') AS has_idea,
        sumIf(wait_s, phase = 'open_to_approved') AS approval_s,
        countIf(phase = 'open_to_approved') AS has_approval,
        sumIf(wait_s, phase = 'spec_writing') AS writing_s,
        countIf(phase = 'spec_writing') AS has_writing,
        sumIf(wait_s, phase = 'architect_review') AS review_s,
        countIf(phase = 'architect_review') AS has_review,
        sumIf(wait_s, phase = 'approval_wait') AS waiting_s,
        countIf(phase = 'approval_wait') AS has_waiting,
        sumIf(wait_s, phase = 'approved_to_started') AS started_s,
        countIf(phase = 'approved_to_started') AS has_started,
        sumIf(wait_s, phase = 'started_to_ready') AS ready_s,
        countIf(phase = 'started_to_ready') AS has_ready,
        sumIf(wait_s, phase = 'started_to_completed') AS completed_s,
        countIf(phase = 'started_to_completed') AS has_completed,
        sumIf(wait_s, phase = 'ready_to_completed') AS merge_s,
        countIf(phase = 'ready_to_completed') AS has_merge,
        avgIf(wait_s, phase = 'merged_to_released') AS release_s,
        countIf(phase = 'merged_to_released') AS has_release
    FROM rises
    GROUP BY spec_id
)
SELECT stage, items, median_wait_s, has_data
FROM (
    SELECT
        1 AS ord,
        'idea' AS stage,
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'idea'), 0)) AS items,
        (SELECT if(countIf(has_idea > 0) = 0, NULL, quantileExactIf(0.5)(idea_s, has_idea > 0)) FROM per_spec) AS median_wait_s,
        1 AS has_data
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
        toNullable(greatest(
            coalesce((SELECT n FROM latest WHERE list = 'approved_not_started'), 0)
            + coalesce((SELECT n FROM latest WHERE list = 'in_progress'), 0)
            - coalesce((SELECT n FROM latest WHERE list = 'ready_not_merged'), 0), 0)),
        (SELECT if(countIf(has_started > 0 AND (has_ready > 0 OR has_completed > 0)) = 0, NULL,
                   quantileExactIf(0.5)(started_s + if(has_ready > 0, ready_s, completed_s), has_started > 0 AND (has_ready > 0 OR has_completed > 0))) FROM per_spec),
        1
    UNION ALL
    SELECT
        6,
        'merge',
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'ready_not_merged'), 0)),
        (SELECT if(countIf(has_merge > 0) = 0, NULL, quantileExactIf(0.5)(merge_s, has_merge > 0)) FROM per_spec),
        1
    UNION ALL
    SELECT
        7,
        'release',
        toNullable(coalesce((SELECT n FROM latest WHERE list = 'merged_not_released'), 0)),
        (SELECT if(countIf(has_release > 0) = 0, NULL, quantileExactIf(0.5)(release_s, has_release > 0)) FROM per_spec),
        1
    UNION ALL
    SELECT
        8,
        'installed',
        toNullable(toFloat64((
            SELECT countIf(newest = 1)
            FROM (
                SELECT argMax(Value, TimeUnix) AS newest
                FROM otel.otel_metrics_gauge
                WHERE MetricName = 'pod.evidence.missing' AND TimeUnix > now() - INTERVAL 10 MINUTE
                GROUP BY Attributes['pod.product'], Attributes['spec.id']
            )
        ))),
        NULL,
        0
)
ORDER BY ord
