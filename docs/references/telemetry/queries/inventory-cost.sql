-- Note: The stock is the tokens already spent on specs that have not reached you, at any time. Tokens and turns is the flow of the 7 days.
-- Waits for: nothing
-- Inventory cost, the stock: the tokens spent so far (harold.turn.tokens.by_spec, all kinds) on the specs that are approved or in progress now,
-- in M with one decimal. The spec.id of a series joins the newest state of the spec in pod.spec.words (the points of the last 37 days).
-- The change compares the stock now with the stock 7 days ago: the tokens spent up to then on the specs that were approved or in progress then.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 60 days of the counter. The overflow series has no spec.id and does not join.
-- overflow is 1 when harold.turn.tokens.by_spec has its overflow series (otel.metric.overflow = true) with a point in the 7 days, and 0 otherwise.
-- Inventory cost, your attention: the answer hours (the sum of agent.attr.seconds of the chief.attention_answered lines, divided by 3600),
-- the cost (the hours times the rate of the ClickHouse dictionary otel.harold_settings, read with dictGetOrNull), the count of chief.attention_request
-- lines and their median agent.attr.words. The chief records the events (the README section "The attention part's producer"). The attention columns are NULL until the first recorded answer, and the rate shows after the release that brings the gauge.
-- With no rate in the dictionary, attention_cost and currency are NULL: the cost tile shows HyperDX's empty state, and the card says "no rate set".
-- A column with no number is NULL, and its tile shows HyperDX's empty state. It is never 0. A change column is empty when the 7 days before have no number.
-- Columns: tokens_m, tokens_m_change_pct, attention_hours, attention_cost, attention_cost_change_pct, currency, attention_requests, words_per_request, overflow.
WITH
    dictGetOrNull('otel.harold_settings', 'value', tuple('harold.attention.hourly_rate')) AS rate,
    dictGetOrNull('otel.harold_settings', 'currency', tuple('harold.attention.hourly_rate')) AS rate_currency,
    states AS (
        SELECT
            Attributes['spec.id'] AS spec,
            argMax(Attributes['state'], TimeUnix) AS state_now,
            argMaxIf(Attributes['state'], TimeUnix, TimeUnix <= now() - INTERVAL 7 DAY) AS state_before,
            countIf(TimeUnix <= now() - INTERVAL 7 DAY) AS points_before
        FROM otel.otel_metrics_gauge
        WHERE MetricName = 'pod.spec.words'
          AND TimeUnix > now() - INTERVAL 37 DAY
        GROUP BY spec
    ),
    spent AS (
        SELECT
            Attributes['spec.id'] AS spec,
            sum(rise) AS spent_now,
            sumIf(rise, TimeUnix <= now() - INTERVAL 7 DAY) AS spent_before
        FROM (
            SELECT
                Attributes,
                TimeUnix,
                if(Value >= before, Value - before, Value) AS rise
            FROM (
                SELECT
                    Attributes,
                    TimeUnix,
                    Value,
                    lagInFrame(Value, 1, toFloat64(0)) OVER (
                        PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
                        ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
                FROM otel.otel_metrics_sum
                WHERE MetricName = 'harold.turn.tokens.by_spec'
                  AND TimeUnix > now() - INTERVAL 60 DAY
            )
        )
        WHERE spec != ''
        GROUP BY spec
    ),
    stock AS (
        SELECT
            countIf(state_now IN ('approved', 'in_progress')) AS now_n,
            sumIf(spent_now, state_now IN ('approved', 'in_progress')) AS now_tokens,
            countIf(points_before > 0 AND state_before IN ('approved', 'in_progress')) AS before_n,
            sumIf(spent_before, points_before > 0 AND state_before IN ('approved', 'in_progress')) AS before_tokens
        FROM states
        INNER JOIN spent USING (spec)
    ),
    overflow AS (
        SELECT count() AS overflow_n
        FROM otel.otel_metrics_sum
        WHERE MetricName = 'harold.turn.tokens.by_spec'
          AND Attributes['otel.metric.overflow'] = 'true'
          AND TimeUnix > now() - INTERVAL 7 DAY
    ),
    attention AS (
        SELECT
            countIf(LogAttributes['agent.name'] = 'chief.attention_answered' AND Timestamp > now() - INTERVAL 7 DAY) AS answers_now,
            sumIf(toFloat64OrZero(LogAttributes['agent.attr.seconds']), LogAttributes['agent.name'] = 'chief.attention_answered' AND Timestamp > now() - INTERVAL 7 DAY) / 3600 AS hours_now,
            countIf(LogAttributes['agent.name'] = 'chief.attention_answered' AND Timestamp > now() - INTERVAL 14 DAY AND Timestamp <= now() - INTERVAL 7 DAY) AS answers_before,
            sumIf(toFloat64OrZero(LogAttributes['agent.attr.seconds']), LogAttributes['agent.name'] = 'chief.attention_answered' AND Timestamp > now() - INTERVAL 14 DAY AND Timestamp <= now() - INTERVAL 7 DAY) / 3600 AS hours_before,
            countIf(LogAttributes['agent.name'] = 'chief.attention_request' AND Timestamp > now() - INTERVAL 7 DAY) AS requests_now,
            quantileIf(0.5)(toFloat64OrZero(LogAttributes['agent.attr.words']), LogAttributes['agent.name'] = 'chief.attention_request' AND Timestamp > now() - INTERVAL 7 DAY) AS words_now
        FROM otel.otel_logs
        WHERE Body = 'agent event'
          AND LogAttributes['agent.name'] IN ('chief.attention_answered', 'chief.attention_request')
          AND Timestamp > now() - INTERVAL 14 DAY
    )
SELECT
    if(now_n = 0, NULL, round(now_tokens / 1000000, 1)) AS tokens_m,
    if(now_n = 0 OR before_n = 0, NULL, round((now_tokens - before_tokens) / nullIf(before_tokens, 0) * 100)) AS tokens_m_change_pct,
    if(answers_now = 0, NULL, round(hours_now, 1)) AS attention_hours,
    if(answers_now = 0 OR rate IS NULL, NULL, round(hours_now * rate)) AS attention_cost,
    if(answers_now = 0 OR answers_before = 0 OR rate IS NULL, NULL, round((hours_now - hours_before) / nullIf(hours_before, 0) * 100)) AS attention_cost_change_pct,
    if(answers_now = 0 OR rate IS NULL, NULL, rate_currency) AS currency,
    if(requests_now = 0, NULL, requests_now) AS attention_requests,
    if(requests_now = 0, NULL, round(words_now)) AS words_per_request,
    toUInt8(overflow_n > 0) AS overflow
FROM stock
CROSS JOIN overflow
CROSS JOIN attention
WHERE tokens_m IS NOT NULL OR attention_hours IS NOT NULL
LIMIT 1
