-- Note: What each model workflow of Harold's own code costs. A diagnosis query, not a card. A row that is doubled comes first: it is not an alert.
-- Waits for: the release that brings the counters harold.workflow.tokens and harold.workflow.cost (until then the query has no row).
-- Shows: the tokens and the AUD cost of the last 7 days (the sum of the rise of each counter), the cost of the 7 days before, the change, and a flag, for each
--   model workflow (workflow), workflow step (workflow.step) and spec (spec.id, or none). Each seeded series (model none) adds 0, so a workflow step
--   that has not run still has a row with zeros. Agent sessions are not model workflows: they stay in harold.turn.tokens.
-- Flag: doubled when cost_aud is more than twice prior_cost_aud and prior_cost_aud is above 0. new when prior_cost_aud is 0 and cost_aud is above 0. Empty otherwise.
-- Order: the rows with doubled first, then new, then the others by change_pct, highest first. A row with no change_pct goes last of its group.
-- change_pct is NULL when prior_cost_aud is 0.
-- Read windows: the counters read 21 days: 14 days of two weeks and 7 days before them for the previous point.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the previous point of its series.
-- A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- Columns: workflow, workflow.step, spec.id, tokens, cost_aud, prior_cost_aud, change_pct, flag.
WITH
    rises AS (
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
            WHERE MetricName IN ('harold.workflow.tokens', 'harold.workflow.cost')
              AND TimeUnix > now() - INTERVAL 21 DAY
        )
    ),
    by_step AS (
        SELECT
            Attributes['workflow'] AS workflow,
            Attributes['workflow.step'] AS step,
            Attributes['spec.id'] AS spec,
            toUInt64(sumIf(rise, MetricName = 'harold.workflow.tokens' AND TimeUnix > now() - INTERVAL 7 DAY)) AS tokens,
            sumIf(rise, MetricName = 'harold.workflow.cost' AND TimeUnix > now() - INTERVAL 7 DAY) AS cost_aud,
            sumIf(rise, MetricName = 'harold.workflow.cost'
                        AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS prior_cost_aud
        FROM rises
        GROUP BY workflow, step, spec
    ),
    flagged AS (
        SELECT
            workflow,
            step,
            spec,
            tokens,
            cost_aud,
            prior_cost_aud,
            if(prior_cost_aud > 0, round((cost_aud - prior_cost_aud) / prior_cost_aud * 100), NULL) AS change_pct,
            multiIf(prior_cost_aud > 0 AND cost_aud > 2 * prior_cost_aud, 'doubled',
                    prior_cost_aud = 0 AND cost_aud > 0, 'new',
                    '') AS flag
        FROM by_step
    )
SELECT
    workflow,
    step AS `workflow.step`,
    spec AS `spec.id`,
    tokens,
    round(cost_aud, 4) AS cost_aud,
    round(prior_cost_aud, 4) AS prior_cost_aud,
    change_pct,
    flag
FROM flagged
ORDER BY multiIf(flag = 'doubled', 0, flag = 'new', 1, 2), change_pct DESC NULLS LAST, workflow, step, spec
LIMIT 200
