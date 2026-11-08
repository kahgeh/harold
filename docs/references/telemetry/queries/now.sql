-- Note: None. The row shows the agents that work now, of all agents, and the specs in progress now, from the last 2 minutes.
-- Waits for: nothing
-- Agents now: the newest export of the gauge harold.agents of the last 2 minutes, summed over the providers and the Rex hosts. "all" counts every state.
-- A series that Harold stopped to export does not count. working is the sum of the state working. Harold exports every 30 seconds.
-- Specs in progress now: the newest export of the gauge pod.inventory.items of the last 2 minutes, the attribute list = in_progress, summed over the products.
-- An in-progress spec has a branch and no parked handoff file. Parked and not clear specs are not counted.
-- A source with no export in the last 2 minutes gives NULL, and its card shows HyperDX's empty state. With no source at all there is no row.
-- read_at is the time of the newest export that the row holds, in UTC. The tiles show it in the local time zone.
-- Columns: working, all, in_progress, read_at.
SELECT
    agents.working AS working,
    agents.all_agents AS `all`,
    items.in_progress AS in_progress,
    multiIf(agents.at IS NULL, items.at, items.at IS NULL, agents.at, greatest(agents.at, items.at)) AS read_at
FROM (
    SELECT
        if(count() = 0, NULL, round(sumIf(Value, Attributes['state'] = 'working'))) AS working,
        if(count() = 0, NULL, round(sum(Value))) AS all_agents,
        if(count() = 0, NULL, max(TimeUnix)) AS at
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'harold.agents'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'harold.agents' AND TimeUnix > now() - INTERVAL 2 MINUTE
      )
) AS agents
CROSS JOIN (
    SELECT
        if(count() = 0, NULL, round(sumIf(Value, Attributes['list'] = 'in_progress'))) AS in_progress,
        if(count() = 0, NULL, max(TimeUnix)) AS at
    FROM otel.otel_metrics_gauge
    WHERE MetricName = 'pod.inventory.items'
      AND TimeUnix = (
          SELECT max(TimeUnix) FROM otel.otel_metrics_gauge
          WHERE MetricName = 'pod.inventory.items' AND TimeUnix > now() - INTERVAL 2 MINUTE
      )
) AS items
WHERE working IS NOT NULL OR in_progress IS NOT NULL
LIMIT 1
