-- This week against last week for one declared agent counter.
-- Parameter: {name:String}, the exported metric name, such as agent.harold.release.attempts.
-- Reads the cumulative Value by TimeUnix. It does not use StartTimeUnix, because a series
-- that Harold forgets restarts its start time and keeps its total.
-- Columns: series, this_week, last_week.
SELECT
    arrayStringConcat(arrayMap(key -> concat(key, '=', Attributes[key]), arraySort(mapKeys(Attributes))), ',') AS series,
    greatest(toInt64(maxIf(Value, TimeUnix <= now()) - maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY)), 0) AS this_week,
    greatest(toInt64(maxIf(Value, TimeUnix <= now() - INTERVAL 7 DAY) - maxIf(Value, TimeUnix <= now() - INTERVAL 14 DAY)), 0) AS last_week
FROM otel.otel_metrics_sum
WHERE MetricName = {name:String}
  AND TimeUnix > now() - INTERVAL 28 DAY
GROUP BY Attributes
ORDER BY series
