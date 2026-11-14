-- Note: Dictation after the first tap. Read it after a release of the fix dictation-every-attempt. No alert.
-- Dictation taps: the log lines `app dictation check` of the last 7 days, one for each dictation tap. A line with tap = 0 comes from an old page and has no tap number, and a `missing` line is a page with no speech recognition, so the query leaves both out.
-- Rows: `tap 1` and `tap 2 or more` give the taps and the share of them with result `ok`. One row `failed, tap 2 or more: <result>` for each result of a later tap that is not `ok`, `no_speech` or `page_ended`. A count above 0 in these rows shows that a later tap fails.
-- Confirmed when `tap 2 or more` has at least one ok tap and no row `failed, tap 2 or more: start_failed` or `... aborted` shows in 7 days. A `page_ended` result does not count.
-- Columns: series, taps, ok_taps, ok_share_pct. The share is NULL when there is no tap and in the failed rows.
WITH seen AS (
    SELECT
        LogAttributes['result'] AS result,
        toUInt16OrZero(LogAttributes['tap']) AS tap
    FROM otel.otel_logs
    WHERE Body = 'app dictation check'
      AND Timestamp > now() - INTERVAL 7 DAY
      AND LogAttributes['result'] != 'missing'
)
SELECT
    'tap 1' AS series,
    count() AS taps,
    countIf(result = 'ok') AS ok_taps,
    if(taps = 0, CAST(NULL AS Nullable(Float64)), round(100 * ok_taps / taps, 1)) AS ok_share_pct
FROM seen
WHERE tap = 1
UNION ALL
SELECT
    'tap 2 or more' AS series,
    count() AS taps,
    countIf(result = 'ok') AS ok_taps,
    if(taps = 0, CAST(NULL AS Nullable(Float64)), round(100 * ok_taps / taps, 1)) AS ok_share_pct
FROM seen
WHERE tap >= 2
UNION ALL
SELECT
    concat('failed, tap 2 or more: ', result) AS series,
    count() AS taps,
    toUInt64(0) AS ok_taps,
    CAST(NULL AS Nullable(Float64)) AS ok_share_pct
FROM seen
WHERE tap >= 2 AND result NOT IN ('ok', 'no_speech', 'page_ended')
GROUP BY result
ORDER BY series
LIMIT 50
