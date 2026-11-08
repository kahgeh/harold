-- Note: A day with 0 attention_requests and more than 0 chief_messages means the chief stopped recording. A run of 3 days with attention_requests above 0 and 0 answers means the answer records stopped (info only).
-- Waits for: nothing
-- Attention requests against chief messages: one row for each of the last 14 days, the newest day first.
-- attention_requests and answers are the counts of the lines "agent event" with agent.name chief.attention_request and chief.attention_answered.
-- chief_messages is the rise of harold.notifications with an agent that ends in chief-of-pods (the chief's folder) and outcome sent, by the counter rule in docs/references/telemetry/README.md.
-- Each point rises by its value minus the previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 60 days of the counter. A day is a calendar day of the ClickHouse server. A day with no line or no point gives 0.
-- Columns: day, attention_requests, answers, chief_messages.
WITH
    messages AS (
        SELECT
            toDate(TimeUnix) AS day,
            sum(rise) AS chief_messages
        FROM (
            SELECT
                TimeUnix,
                if(Value >= before, Value - before, Value) AS rise
            FROM (
                SELECT
                    TimeUnix,
                    Value,
                    lagInFrame(Value, 1, toFloat64(0)) OVER (
                        PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                        ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
                FROM otel.otel_metrics_sum
                WHERE MetricName = 'harold.notifications'
                  AND endsWith(Attributes['agent'], '/chief-of-pods')
                  AND Attributes['outcome'] = 'sent'
                  AND TimeUnix > now() - INTERVAL 60 DAY
            )
        )
        GROUP BY day
    ),
    recorded AS (
        SELECT
            toDate(Timestamp) AS day,
            countIf(LogAttributes['agent.name'] = 'chief.attention_request') AS attention_requests,
            countIf(LogAttributes['agent.name'] = 'chief.attention_answered') AS answers
        FROM otel.otel_logs
        WHERE Body = 'agent event'
          AND LogAttributes['agent.name'] IN ('chief.attention_request', 'chief.attention_answered')
          AND Timestamp > now() - INTERVAL 15 DAY
        GROUP BY day
    )
SELECT
    spine.day AS day,
    recorded.attention_requests AS attention_requests,
    recorded.answers AS answers,
    toUInt64(round(messages.chief_messages)) AS chief_messages
FROM (SELECT toDate(now() - toIntervalDay(n)) AS day FROM (SELECT arrayJoin(range(14)) AS n)) AS spine
LEFT JOIN recorded ON recorded.day = spine.day
LEFT JOIN messages ON messages.day = spine.day
ORDER BY day DESC
