-- Note: Both results are expected to be 0 (info only). A row of "wrong owned reply" means a Mac took a reply that another Mac owns.
-- Reply owner: the log line `reply owner decided` of harold::inbound, with the fields owned, owner_from, latest_notifier and harold.version.
-- A wrong owned reply is a line with owned = true and owner_from = none or latest_marker, while latest_notifier names another Mac than the Mac
-- that logged the line (the resource attribute harold.machine; the names match without regard to case). An empty latest_notifier is never wrong.
-- Only lines whose harold.version is min_version or later count. An old Harold logs no such line, so its wrong claim does not count against the new code.
-- Set min_version below to the version of the release that holds the fix, when the release is known. Versions compare number by number.
-- The `pane label unknown` lines (a Rex block that the agent snapshot lacks) are counted over the same 7 days. An old Harold never logs this line, so no
-- version test is needed. The pane id of each is in the line itself: read it with bin/logq for the error diagnosis.
-- Window: the last 7 days. The lines of the logs hold no reply text.
-- Columns: kind, at, machine, owner_from, latest_notifier, harold_version, lines.
-- Rows: one for each wrong owned reply (kind "wrong owned reply", lines 1), then one row of kind "pane label unknown" with its count in lines (0 when none).
WITH '0.1.0' AS min_version
SELECT
    'wrong owned reply' AS kind,
    toString(Timestamp) AS at,
    ResourceAttributes['harold.machine'] AS machine,
    LogAttributes['owner_from'] AS owner_from,
    LogAttributes['latest_notifier'] AS latest_notifier,
    LogAttributes['harold.version'] AS harold_version,
    toUInt64(1) AS lines
FROM otel.otel_logs
WHERE Body = 'reply owner decided'
  AND Timestamp > now() - INTERVAL 7 DAY
  AND LogAttributes['owned'] = 'true'
  AND LogAttributes['owner_from'] IN ('none', 'latest_marker')
  AND LogAttributes['latest_notifier'] != ''
  AND lower(LogAttributes['latest_notifier']) != lower(ResourceAttributes['harold.machine'])
  AND arrayMap(x -> toUInt32OrZero(x), splitByChar('.', LogAttributes['harold.version']))
      >= arrayMap(x -> toUInt32OrZero(x), splitByChar('.', min_version))
UNION ALL
SELECT
    'pane label unknown' AS kind,
    '' AS at,
    '' AS machine,
    '' AS owner_from,
    '' AS latest_notifier,
    '' AS harold_version,
    countIf(Body = 'pane label unknown' AND Timestamp > now() - INTERVAL 7 DAY) AS lines
FROM otel.otel_logs
WHERE Body = 'pane label unknown'
ORDER BY kind DESC, at DESC
