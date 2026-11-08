-- Note: None. The row shows the agents that work now, of all agents, and the specs in progress now.
-- Waits for: nothing
-- Agents now: the newest export of the gauge harold.agents of the last 7 days, summed over the providers and the Rex hosts. "all" counts every state.
-- A series that Harold stopped to export does not count. working is the sum of the state working.
-- Specs in progress now: the newest export of the gauge pod.inventory.items of the last 7 days, the attribute list = in_progress, summed over the products.
-- An in-progress spec has a branch and no parked handoff file. Parked and not clear specs are not counted.
-- A source with no export in the last 7 days gives NULL, and its tile shows HyperDX's empty state. With no source at all there is no row.
-- Columns: working, all, in_progress.
SELECT
    agents.working AS working,
    agents.all_agents AS `all`,
    items.in_progress AS in_progress
FROM (
    SELECT
        if(count() = 0, NULL, round(sumIf(Value, Attributes['state'] = 'working'))) AS working,
        if(count() = 0, NULL, round(sum(Value))) AS all_agents
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.agents'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'harold.agents' AND TimeUnix > now() - INTERVAL 7 DAY
      )
) AS agents
CROSS JOIN (
    SELECT
        if(count() = 0, NULL, round(sumIf(Value, Attributes['list'] = 'in_progress'))) AS in_progress
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.inventory.items'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'pod.inventory.items' AND TimeUnix > now() - INTERVAL 7 DAY
      )
) AS items
WHERE working IS NOT NULL OR in_progress IS NOT NULL
LIMIT 1
