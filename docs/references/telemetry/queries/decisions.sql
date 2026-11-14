-- Note: None. The card shows 0 made when no decision record was added in the 7 days.
-- Waits for: nothing
-- Decisions: the decision records of the 7 days against the 7 days before. made reads the counter harold.decisions.added and evaluated
-- reads harold.decisions.evaluated. good_process_pct and good_outcome_pct are the shares of the evaluated ones with the grade good.
-- due reads the newest export of the gauge harold.decisions.open with status = due, summed over the organisations.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- No row while neither counter has a point in the 14 days: the tile shows HyperDX's empty state, not 0.
-- made_change_pct, evaluated_change_pct and due_change_pct are the percent change against the 7 days before. They are empty when that number was 0.
-- good_process_change_pts and good_outcome_change_pts are the change of the share in percentage points. They are empty when a week had no evaluated record.
-- Columns: made, made_change_pct, evaluated, evaluated_change_pct, good_process_pct, good_process_change_pts, good_outcome_pct, good_outcome_change_pts, due, due_change_pct.
SELECT
    made,
    round((made - made_last) / nullIf(made_last, 0) * 100) AS made_change_pct,
    evaluated,
    round((evaluated - evaluated_last) / nullIf(evaluated_last, 0) * 100) AS evaluated_change_pct,
    round(good_process / nullIf(evaluated, 0) * 100) AS good_process_pct,
    round((good_process / nullIf(evaluated, 0) - good_process_last / nullIf(evaluated_last, 0)) * 100, 1) AS good_process_change_pts,
    round(good_outcome / nullIf(evaluated, 0) * 100) AS good_outcome_pct,
    round((good_outcome / nullIf(evaluated, 0) - good_outcome_last / nullIf(evaluated_last, 0)) * 100, 1) AS good_outcome_change_pts,
    due,
    round((due - due_last) / nullIf(due_last, 0) * 100) AS due_change_pct
FROM (
    SELECT
        sumIf(rise, MetricName = 'harold.decisions.added' AND TimeUnix > now() - INTERVAL 7 DAY) AS made,
        sumIf(rise, MetricName = 'harold.decisions.added' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS made_last,
        sumIf(rise, MetricName = 'harold.decisions.evaluated' AND TimeUnix > now() - INTERVAL 7 DAY) AS evaluated,
        sumIf(rise, MetricName = 'harold.decisions.evaluated' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS evaluated_last,
        sumIf(rise, MetricName = 'harold.decisions.evaluated' AND Attributes['process_grade'] = 'good' AND TimeUnix > now() - INTERVAL 7 DAY) AS good_process,
        sumIf(rise, MetricName = 'harold.decisions.evaluated' AND Attributes['process_grade'] = 'good' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS good_process_last,
        sumIf(rise, MetricName = 'harold.decisions.evaluated' AND Attributes['outcome_grade'] = 'good' AND TimeUnix > now() - INTERVAL 7 DAY) AS good_outcome,
        sumIf(rise, MetricName = 'harold.decisions.evaluated' AND Attributes['outcome_grade'] = 'good' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS good_outcome_last,
        count() AS points
    FROM (
        SELECT
            MetricName,
            Attributes,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                MetricName,
                Attributes,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName IN ('harold.decisions.added', 'harold.decisions.evaluated')
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    )
) AS counts
CROSS JOIN (
    SELECT
        sumIf(v, wk = 'this') AS due,
        sumIf(v, wk = 'last') AS due_last
    FROM (
        SELECT
            if(TimeUnix > now() - INTERVAL 7 DAY, 'this', 'last') AS wk,
            Attributes['organisation'] AS organisation,
            argMax(Value, TimeUnix) AS v
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'harold.decisions.open'
          AND Attributes['status'] = 'due'
          AND TimeUnix > now() - INTERVAL 14 DAY
        GROUP BY wk, organisation
    )
) AS gauges
WHERE points > 0
LIMIT 1
