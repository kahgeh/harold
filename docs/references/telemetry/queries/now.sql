-- Note: None. The card shows the agents that work now, of all agents.
-- Waits for: nothing
-- Agents now: the last value of each gauge in the week.
-- The rows "<product>: working" and "<product>: all" read pod.agents, summed over the roles. "all" counts every state.
-- The rows "all Harold agents: working" and "all Harold agents: all" read harold.agents, summed over the providers and the Rex hosts. They have no product.
-- The product filter applies to the pod.agents rows only.
-- Columns: series, this_week, last_week.
SELECT
    concat(product, ': ', kind) AS series,
    if(isNull(max(this_value)), NULL, sum(this_value)) AS this_week,
    if(isNull(max(last_value)), NULL, sum(last_value)) AS last_week
FROM (
    SELECT
        if(Attributes['pod.product'] = '', 'no product', Attributes['pod.product']) AS product,
        Attributes['state'] AS state,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_value,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_value
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.agents'
      AND TimeUnix > now() - INTERVAL 14 DAY
      AND $__filters
    GROUP BY Attributes
    UNION ALL
    SELECT
        'all Harold agents' AS product,
        Attributes['state'] AS state,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 7 DAY AND TimeUnix <= now()) AS this_value,
        argMaxIfOrNull(Value, TimeUnix, TimeUnix > now() - INTERVAL 14 DAY AND TimeUnix <= now() - INTERVAL 7 DAY) AS last_value
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.agents'
      AND TimeUnix > now() - INTERVAL 14 DAY
    GROUP BY Attributes
)
ARRAY JOIN if(state = 'working', ['working', 'all'], ['all']) AS kind
GROUP BY concat(product, ': ', kind)
HAVING isNotNull(this_week) OR isNotNull(last_week)
ORDER BY series
