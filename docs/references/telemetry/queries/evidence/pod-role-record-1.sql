-- Note: Evidence query 1 of the spec pod-role-record. Parameter {since:DateTime}: the merge time of the spec, in UTC.
-- Waits for: nothing
-- Shows: the check "pod role lookups found". From {since} to {since} + 14 days, each of the roles chief, pnem and pod_coordination has a `pod role recorded` line, at least one `pod role lookup` line has the outcome found, and no lookup line has the outcome no_record or session_file_unreadable. An agent_gone outcome is no fault.
-- Columns: present (1 for lookups_found, else 0), reason (lookups_found, lookup_faults or no_lookup_yet), recorded (the `pod role recorded` lines), found (the lookup lines with the outcome found), faults (the lookup lines with the outcome no_record or session_file_unreadable). One row.
SELECT
    toUInt8(reason = 'lookups_found') AS present,
    reason,
    recorded,
    found,
    faults
FROM (
    SELECT
        recorded,
        found,
        faults,
        multiIf(
            faults > 0, 'lookup_faults',
            roles_recorded = 3 AND found > 0, 'lookups_found',
            'no_lookup_yet'
        ) AS reason
    FROM (
        SELECT
            countIf(Body = 'pod role recorded') AS recorded,
            uniqExactIf(LogAttributes['role'], Body = 'pod role recorded'
                AND LogAttributes['role'] IN ('chief', 'pnem', 'pod_coordination')) AS roles_recorded,
            countIf(Body = 'pod role lookup' AND LogAttributes['outcome'] = 'found') AS found,
            countIf(Body = 'pod role lookup'
                AND LogAttributes['outcome'] IN ('no_record', 'session_file_unreadable')) AS faults
        FROM otel.otel_logs
        WHERE ServiceName = 'harold'
          AND Body IN ('pod role recorded', 'pod role lookup')
          AND Timestamp > {since:DateTime}
          AND Timestamp <= {since:DateTime} + INTERVAL 14 DAY
    )
)
LIMIT 1
