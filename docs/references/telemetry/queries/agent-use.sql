-- Note: The share of the agents' CPU that no agent owns, and the alert lines of the last 7 days with no agent named, for the chief's weekly digest. Not an alert. A none_share above 0.5 while agents run shows that the parent walk misses processes. Expected alerts_empty: 0.
-- Waits for: nothing
-- Shows: the gauge `harold.agent.cpu.utilization` of the last 7 days, and the lines `machine use past normal` of the last 7 days.
-- Columns: none_share (the CPU with role none over all the CPU, 0 to 1), alerts (the lines), alerts_empty (the lines whose top_agents is empty). One row.
SELECT
    (
        SELECT round(sumIf(Value, Attributes['role'] = 'none') / nullIf(sum(Value), 0), 3)
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'harold.agent.cpu.utilization'
          AND TimeUnix > now() - INTERVAL 7 DAY
    ) AS none_share,
    (
        SELECT count()
        FROM otel.otel_logs
        WHERE ServiceName = 'harold'
          AND Body = 'machine use past normal'
          AND Timestamp > now() - INTERVAL 7 DAY
    ) AS alerts,
    (
        SELECT count()
        FROM otel.otel_logs
        WHERE ServiceName = 'harold'
          AND Body = 'machine use past normal'
          AND LogAttributes['top_agents'] = ''
          AND Timestamp > now() - INTERVAL 7 DAY
    ) AS alerts_empty
