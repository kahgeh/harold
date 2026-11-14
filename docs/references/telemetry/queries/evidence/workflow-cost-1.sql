-- Note: Evidence query 1 of the spec workflow-cost. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: harold.workflow.cost rose after {since} for 2 or more model workflows, so the cost of more than one model workflow is recorded. The seeded series (model none) rise by 0 and do not count. The query reads to {since} + 30 days.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 14 days before {since} for the previous point.
-- Columns: present (1 or 0), reason (workflow_cost_recorded, one_workflow_only or no_workflow_run_yet). One row.
SELECT
    toUInt8(workflows >= 2) AS present,
    multiIf(workflows >= 2, 'workflow_cost_recorded', workflows = 1, 'one_workflow_only', 'no_workflow_run_yet') AS reason
FROM (
    SELECT uniqExactIf(workflow, rise > 0 AND TimeUnix > {since:DateTime}) AS workflows
    FROM (
        SELECT
            workflow,
            TimeUnix,
            if(Value >= before, Value - before, Value) AS rise
        FROM (
            SELECT
                Attributes['workflow'] AS workflow,
                TimeUnix,
                Value,
                lagInFrame(Value, 1, toFloat64(0)) OVER (
                    PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                    ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
            FROM otel.otel_metrics_sum
            WHERE MetricName = 'harold.workflow.cost'
              AND TimeUnix > {since:DateTime} - INTERVAL 14 DAY
              AND TimeUnix <= {since:DateTime} + INTERVAL 30 DAY
        )
    )
)
LIMIT 1
