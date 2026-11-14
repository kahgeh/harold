-- Note: The count of restart check rows that failed, for the chief's weekly digest. Not an alert: each FAIL already goes to the chief or to pod-coordination. Expected: under 2 a week once the roles lists are right.
-- Waits for: nothing
-- Shows: the `restart check result` lines of the last 7 days with `result = fail`, by role, group and check.
-- Columns: role, group, check, fails. One row for each series, the most fails first.
SELECT
    LogAttributes['role'] AS role,
    LogAttributes['group'] AS `group`,
    LogAttributes['check'] AS check,
    count() AS fails
FROM otel.otel_logs
WHERE ServiceName = 'harold'
  AND Body = 'restart check result'
  AND LogAttributes['result'] = 'fail'
  AND Timestamp > now() - INTERVAL 7 DAY
GROUP BY role, `group`, check
ORDER BY fails DESC, role, `group`, check
LIMIT 200
