-- Note: Evidence query 1 of the spec agent-restart. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the check "a restart check ran". From {since} to {since} + 30 days, `restart check result` lines exist. With the groups identity, communication, setup and memory all present the reason is checked. After stage 1 alone, with the groups identity and communication only, it is checked_identity_communication.
-- Columns: present (1 for checked or checked_identity_communication, else 0), reason (checked, checked_identity_communication or no_restart_yet), checks (the `restart check result` lines), groups (the groups with at least one line). One row.
SELECT
    toUInt8(reason IN ('checked', 'checked_identity_communication')) AS present,
    reason,
    checks,
    groups
FROM (
    SELECT
        checks,
        groups,
        multiIf(
            groups_of_four = 4, 'checked',
            has_identity AND has_communication, 'checked_identity_communication',
            'no_restart_yet'
        ) AS reason
    FROM (
        SELECT
            count() AS checks,
            uniqExact(LogAttributes['group']) AS groups,
            uniqExactIf(LogAttributes['group'],
                LogAttributes['group'] IN ('identity', 'communication', 'setup', 'memory')) AS groups_of_four,
            countIf(LogAttributes['group'] = 'identity') > 0 AS has_identity,
            countIf(LogAttributes['group'] = 'communication') > 0 AS has_communication
        FROM otel.otel_logs
        WHERE ServiceName = 'harold'
          AND Body = 'restart check result'
          AND Timestamp > {since:DateTime}
          AND Timestamp <= {since:DateTime} + INTERVAL 30 DAY
    )
)
LIMIT 1
