-- Note: The stock is the tokens already spent on specs that have not reached you, at any time. Tokens and turns is the flow of the 7 days.
-- Waits for: the release that brings the gauges harold.list_price.per_mtok and harold.exchange_rate.usd_to_aud (the notes show until then).
-- Inventory cost, the stock: the tokens spent so far (harold.turn.tokens.by_spec, the four kinds) on the specs that are approved or in progress now,
-- in M with one decimal (tokens_m), and their cost in AUD (token_cost). The spec.id of a series joins the newest state of the spec in pod.spec.words (the points of the last 37 days).
-- The change compares the stock now with the stock 7 days ago: the tokens spent up to then on the specs that were approved or in progress then.
-- Token cost: each series (spec, model, kind) is priced at its list price, USD per million tokens, read with dictGetOrNull from the ClickHouse dictionary
-- otel.harold_settings (row name harold.list_price.per_mtok/<model>/<kind>). The sum in USD is multiplied by the exchange rate (row harold.exchange_rate.usd_to_aud).
-- The dictionary holds only the newest list price and exchange rate. A change of either prices the whole stock again, the stock 7 days ago too.
-- Old stock: a series recorded before the model was added has model '' and its input holds all three input kinds. It is priced at the mix of the build workers of the
-- 7 days: the average list price of their input side (input, cache_read, cache_write) and of their output (harold.turn.tokens, role build_worker). A model of the mix with no list price
-- leaves the mix. With no build-worker turn in the 7 days the mix is empty: the old stock adds nothing and old_stock_unpriced is 1.
-- unpriced_models is the models with stock and no list price (one text, names with a comma between them, empty when none). They add nothing.
-- Counter rule: the README rule "The counter rule" in docs/references/telemetry/README.md. Each point rises by its value minus the
-- previous point of its series. A first point, or a point below the previous one (Harold restarted), rises by its whole value.
-- The query reads the points of 60 days of the counter. The overflow series has no spec.id and does not join.
-- overflow is 1 when harold.turn.tokens.by_spec has its overflow series (otel.metric.overflow = true) with a point in the 7 days, and 0 otherwise.
-- Inventory cost, your human attention: the answer hours (the sum of agent.attr.seconds of the chief.attention_answered lines, divided by 3600),
-- the cost (the hours times the rate of the ClickHouse dictionary otel.harold_settings, read with dictGetOrNull), the count of chief.attention_request
-- lines and their median agent.attr.words. The chief records the events (the README section "The attention part's producer"). The attention columns are NULL until the first recorded answer.
-- With no rate in the dictionary, attention_cost and currency are NULL: the card says "human attention: no rate set". The column keeps the name attention_cost.
-- With no exchange rate in the dictionary, token_cost is NULL: the card says "tokens: no exchange rate set".
-- total_cost is token_cost plus attention_cost (the part that is not NULL), in AUD. total_cost_change_pct compares it with the same total 7 days before.
-- A column with no number is NULL, and its tile shows HyperDX's empty state. It is never 0. A change column is empty when the 7 days before have no number.
-- Columns: tokens_m, tokens_m_change_pct, token_cost, attention_hours, attention_cost, attention_cost_change_pct, currency, attention_requests, words_per_request,
-- total_cost, total_cost_change_pct, unpriced_models, old_stock_unpriced, overflow.
WITH
    dictGetOrNull('otel.harold_settings', 'value', tuple('harold.attention.hourly_rate')) AS rate,
    dictGetOrNull('otel.harold_settings', 'currency', tuple('harold.attention.hourly_rate')) AS rate_currency,
    dictGetOrNull('otel.harold_settings', 'value', tuple('harold.exchange_rate.usd_to_aud')) AS usd_to_aud,
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
    series AS (
        SELECT
            Attributes['spec.id'] AS spec,
            Attributes['model'] AS model,
            Attributes['kind'] AS kind,
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
        GROUP BY spec, model, kind
    ),
    spent AS (
        SELECT spec, sum(spent_now) AS spent_now, sum(spent_before) AS spent_before
        FROM series
        GROUP BY spec
    ),
    build_turns AS (
        SELECT
            Attributes['model'] AS model,
            Attributes['kind'] AS kind,
            sum(rise) AS tokens
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
                WHERE MetricName = 'harold.turn.tokens'
                  AND Attributes['role'] = 'build_worker'
                  AND TimeUnix > now() - INTERVAL 14 DAY
            )
            WHERE TimeUnix > now() - INTERVAL 7 DAY
        )
        GROUP BY model, kind
    ),
    mix AS (
        SELECT
            sumIf(tokens * price, kind != 'output' AND price IS NOT NULL) / nullIf(sumIf(tokens, kind != 'output' AND price IS NOT NULL), 0) AS input_price,
            sumIf(tokens * price, kind = 'output' AND price IS NOT NULL) / nullIf(sumIf(tokens, kind = 'output' AND price IS NOT NULL), 0) AS output_price
        FROM (
            SELECT
                kind,
                tokens,
                dictGetOrNull('otel.harold_settings', 'value', tuple(concat('harold.list_price.per_mtok/', model, '/', kind))) AS price
            FROM build_turns
        )
    ),
    priced AS (
        SELECT
            spec,
            model,
            kind,
            spent_now,
            spent_before,
            if(model = '',
               if(kind = 'output', output_price, input_price),
               dictGetOrNull('otel.harold_settings', 'value', tuple(concat('harold.list_price.per_mtok/', model, '/', kind)))) AS price
        FROM series
        CROSS JOIN mix
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
    cost AS (
        SELECT
            sumIf(spent_now * price / 1000000, state_now IN ('approved', 'in_progress') AND price IS NOT NULL) AS usd_now,
            sumIf(spent_before * price / 1000000, points_before > 0 AND state_before IN ('approved', 'in_progress') AND price IS NOT NULL) AS usd_before,
            arrayStringConcat(arraySort(groupUniqArrayIf(model,
                model != '' AND price IS NULL AND (
                    (state_now IN ('approved', 'in_progress') AND spent_now > 0)
                    OR (points_before > 0 AND state_before IN ('approved', 'in_progress') AND spent_before > 0)))), ',') AS unpriced,
            toUInt8(countIf(model = '' AND price IS NULL AND (
                    (state_now IN ('approved', 'in_progress') AND spent_now > 0)
                    OR (points_before > 0 AND state_before IN ('approved', 'in_progress') AND spent_before > 0))) > 0) AS old_unpriced
        FROM states
        INNER JOIN priced USING (spec)
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
    ),
    totals AS (
        SELECT
            if(now_n = 0 OR usd_to_aud IS NULL, NULL, usd_now * usd_to_aud) AS token_now,
            if(before_n = 0 OR usd_to_aud IS NULL, NULL, usd_before * usd_to_aud) AS token_before,
            if(answers_now = 0 OR rate IS NULL, NULL, round(hours_now * rate)) AS human_now,
            if(answers_before = 0 OR rate IS NULL, NULL, round(hours_before * rate)) AS human_before
        FROM stock
        CROSS JOIN cost
        CROSS JOIN attention
    )
SELECT
    if(now_n = 0, NULL, round(now_tokens / 1000000, 1)) AS tokens_m,
    if(now_n = 0 OR before_n = 0, NULL, round((now_tokens - before_tokens) / nullIf(before_tokens, 0) * 100)) AS tokens_m_change_pct,
    if(token_now IS NULL, NULL, round(token_now, 2)) AS token_cost,
    if(answers_now = 0, NULL, round(hours_now, 1)) AS attention_hours,
    if(answers_now = 0 OR rate IS NULL, NULL, round(hours_now * rate)) AS attention_cost,
    if(answers_now = 0 OR answers_before = 0 OR rate IS NULL, NULL, round((hours_now - hours_before) / nullIf(hours_before, 0) * 100)) AS attention_cost_change_pct,
    if(answers_now = 0 OR rate IS NULL, NULL, rate_currency) AS currency,
    if(requests_now = 0, NULL, requests_now) AS attention_requests,
    if(requests_now = 0, NULL, round(words_now)) AS words_per_request,
    if(token_now IS NULL AND human_now IS NULL, NULL, round(ifNull(token_now, 0) + ifNull(human_now, 0), 2)) AS total_cost,
    if((token_now IS NULL AND human_now IS NULL) OR (token_before IS NULL AND human_before IS NULL)
           OR ifNull(token_before, 0) + ifNull(human_before, 0) = 0, NULL,
       round((ifNull(token_now, 0) + ifNull(human_now, 0) - ifNull(token_before, 0) - ifNull(human_before, 0))
             / (ifNull(token_before, 0) + ifNull(human_before, 0)) * 100)) AS total_cost_change_pct,
    unpriced AS unpriced_models,
    old_unpriced AS old_stock_unpriced,
    toUInt8(overflow_n > 0) AS overflow
FROM stock
CROSS JOIN cost
CROSS JOIN overflow
CROSS JOIN attention
CROSS JOIN totals
WHERE tokens_m IS NOT NULL OR attention_hours IS NOT NULL
LIMIT 1
