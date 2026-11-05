-- Note: None. The card shows the agents that work now, of all agents.
-- Waits for: nothing
-- Agents now: the newest export of each gauge. A series that Harold stopped to export does not count.
-- This week reads the newest export of the last 7 days. Last week reads the newest export of the 7 days before them.
-- The rows "<product>: working" and "<product>: all" read pod.agents, summed over the roles. "all" counts every state.
-- The rows "all Harold agents: working" and "all Harold agents: all" read harold.agents, summed over the providers and the Rex hosts. They have no product.
-- The product filter applies to the pod.agents rows only. A week with no export gives no number for that week.
-- Columns: series, this_week, last_week.
SELECT
    concat(product, ': ', kind) AS series,
    if(countIf(wk = 'this') = 0, NULL, sumIf(v, wk = 'this')) AS this_week,
    if(countIf(wk = 'last') = 0, NULL, sumIf(v, wk = 'last')) AS last_week
FROM (
    SELECT
        'this' AS wk,
        if(Attributes['pod.product'] = '', 'no product', Attributes['pod.product']) AS product,
        Attributes['state'] AS state,
        sum(Value) AS v
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.agents'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'pod.agents' AND TimeUnix > now() - INTERVAL 7 DAY
      )
      AND $__filters
    GROUP BY product, state
    UNION ALL
    SELECT
        'this' AS wk,
        'all Harold agents' AS product,
        Attributes['state'] AS state,
        sum(Value) AS v
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.agents'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'harold.agents' AND TimeUnix > now() - INTERVAL 7 DAY
      )
    GROUP BY product, state
    UNION ALL
    SELECT
        'last' AS wk,
        if(Attributes['pod.product'] = '', 'no product', Attributes['pod.product']) AS product,
        Attributes['state'] AS state,
        sum(Value) AS v
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.agents'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'pod.agents' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY
      )
      AND $__filters
    GROUP BY product, state
    UNION ALL
    SELECT
        'last' AS wk,
        'all Harold agents' AS product,
        Attributes['state'] AS state,
        sum(Value) AS v
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.agents'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'harold.agents' AND TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY
      )
    GROUP BY product, state
)
ARRAY JOIN if(state = 'working', ['working', 'all'], ['all']) AS kind
GROUP BY series
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
