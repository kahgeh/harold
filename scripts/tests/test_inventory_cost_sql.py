"""Check the query of the card Inventory cost on fake rows.

The query file docs/references/telemetry/queries/inventory-cost.sql reads three tables and the
ClickHouse dictionary otel.harold_settings. The test replaces each table name with a SELECT of
constants and each dictGetOrNull call with a lookup in a literal map (SELECT only, no table, no
write). It sends the text to the read-only ClickHouse through bin/logq. The real dictionary needs
the chief's setup, so the dictionary itself is not under test here.
The tests skip when bin/logq or ClickHouse is not there, as in CI.
"""

from pathlib import Path
import os
import re
import shutil
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
QUERY = ROOT / "docs" / "references" / "telemetry" / "queries" / "inventory-cost.sql"
SUM_TABLE = "otel.otel_metrics_sum"
GAUGE_TABLE = "otel.otel_metrics_gauge"
LOG_TABLE = "otel.otel_logs"
LIST = "harold.list_price.per_mtok"
RATE = "harold.exchange_rate.usd_to_aud"
HOURLY = "harold.attention.hourly_rate"
DICTIONARY = re.compile(r"dictGetOrNull\('otel\.harold_settings', '(value|currency)', tuple\(")

# USD per million tokens: input, output, cache_write, cache_read.
OPUS = {"input": 4.0, "output": 20.0, "cache_write": 5.0, "cache_read": 0.2}
SONNET = {"input": 2.0, "output": 10.0, "cache_write": 2.5, "cache_read": 0.1}


def find_logq():
    """Return the path of bin/logq, or None. LOGQ names it. The default is the chief's tool."""
    named = os.environ.get("LOGQ")
    if named:
        return named if Path(named).is_file() else None
    found = shutil.which("logq")
    if found:
        return found
    default = Path.home() / "Dev" / "p" / "chief-of-pods" / "bin" / "logq"
    return str(default) if default.is_file() else None


def settings(rate=1.5, hourly=100, prices=None):
    """Return the fake dictionary: the row name and its value, and the row name and its currency."""
    prices = {"claude-opus-5-5": OPUS, "claude-sonnet-5-5": SONNET} if prices is None else prices
    values, currencies = {}, {}
    for model, kinds in prices.items():
        for kind, price in kinds.items():
            values[f"{LIST}/{model}/{kind}"] = price
            currencies[f"{LIST}/{model}/{kind}"] = "USD"
    if rate is not None:
        values[RATE], currencies[RATE] = rate, "AUD"
    if hourly is not None:
        values[HOURLY], currencies[HOURLY] = hourly, "AUD"
    return values, currencies


def literal_map(entries, kind):
    if not entries:
        return f"CAST(map(), 'Map(String, {kind})')"
    body = ", ".join(f"'{name}', to{kind}({value!r})" if kind == "Float64" else f"'{name}', '{value}'"
                     for name, value in entries.items())
    return f"map({body})"


def replace_dictionary(text, values, currencies):
    """Replace each dictGetOrNull call with a lookup of its row name in the fake maps."""
    out, position = [], 0
    for match in DICTIONARY.finditer(text):
        if match.start() < position:
            continue
        depth, index = 1, match.end()  # inside tuple(
        while depth:
            depth += {"(": 1, ")": -1}.get(text[index], 0)
            index += 1
        name = text[match.end():index - 1]
        assert text[index] == ")", "the call dictGetOrNull(...) closes after tuple(...)"
        index += 1
        entries, kind = (values, "Float64") if match.group(1) == "value" else (currencies, "String")
        fake = literal_map(entries, kind)
        out.append(text[position:match.start()])
        out.append(f"if(mapContains({fake}, {name}), {fake}[{name}], NULL)")
        position = index
    out.append(text[position:])
    return "".join(out)


def hours(count):
    return f"now() - INTERVAL {count} HOUR"


def series(spec, model, kind, value, hours_ago=1, metric="harold.turn.tokens.by_spec", extra=""):
    """Return one fake point of the by-spec counter. A model of None is an old series."""
    model_part = f", 'model', '{model}'" if model else ""
    return (f"SELECT '{metric}' AS MetricName, CAST(map(), 'Map(String, String)') AS ResourceAttributes, "
            f"map('spec.id', '{spec}', 'kind', '{kind}'{model_part}{extra}) AS Attributes, "
            f"{hours(hours_ago)} AS TimeUnix, toFloat64({value}) AS Value")


def build_turn(model, kind, value, hours_ago=1, role="build_worker"):
    """Return one fake point of the counter harold.turn.tokens."""
    return (f"SELECT 'harold.turn.tokens' AS MetricName, CAST(map(), 'Map(String, String)') AS ResourceAttributes, "
            f"map('role', '{role}', 'model', '{model}', 'kind', '{kind}') AS Attributes, "
            f"{hours(hours_ago)} AS TimeUnix, toFloat64({value}) AS Value")


def overflow_point():
    return ("SELECT 'harold.turn.tokens.by_spec' AS MetricName, CAST(map(), 'Map(String, String)') AS ResourceAttributes, "
            "map('otel.metric.overflow', 'true') AS Attributes, now() - INTERVAL 1 HOUR AS TimeUnix, toFloat64(5) AS Value")


def state(spec, name, hours_ago=1):
    """Return one fake point of pod.spec.words: the spec was in this state hours_ago hours ago."""
    return (f"SELECT 'pod.spec.words' AS MetricName, map('spec.id', '{spec}', 'state', '{name}') AS Attributes, "
            f"{hours(hours_ago)} AS TimeUnix")


def answered(seconds, hours_ago=1):
    return ("SELECT 'agent event' AS Body, map('agent.name', 'chief.attention_answered', "
            f"'agent.attr.seconds', '{seconds}') AS LogAttributes, {hours(hours_ago)} AS Timestamp")


EMPTY_SUM = ("SELECT '' AS MetricName, CAST(map(), 'Map(String, String)') AS ResourceAttributes, "
             "CAST(map(), 'Map(String, String)') AS Attributes, now() AS TimeUnix, toFloat64(0) AS Value WHERE 0")
EMPTY_GAUGE = "SELECT '' AS MetricName, CAST(map(), 'Map(String, String)') AS Attributes, now() AS TimeUnix WHERE 0"
EMPTY_LOG = ("SELECT '' AS Body, CAST(map(), 'Map(String, String)') AS LogAttributes, now() AS Timestamp WHERE 0")


class InventoryCostTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the fixture tests need the read-only ClickHouse")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the fixture tests are skipped")
        cls.text = QUERY.read_text(encoding="utf-8")

    def run_on(self, sums=(), states=(), logs=(), dictionary=None):
        """Run the query on the fake rows. Return the row as a dict of text, or None when there is no row."""
        values, currencies = dictionary or settings()
        sql = self.text
        for table, rows, empty in ((SUM_TABLE, sums, EMPTY_SUM), (GAUGE_TABLE, states, EMPTY_GAUGE),
                                   (LOG_TABLE, logs, EMPTY_LOG)):
            self.assertIn(table, sql)
            sql = sql.replace(table, "(" + " UNION ALL ".join(rows or [empty]) + ")")
        sql = replace_dictionary(sql, values, currencies)
        self.assertNotIn("harold_settings", "\n".join(line.split("--")[0] for line in sql.splitlines()))
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        if len(lines) < 2:
            return None
        return dict(zip(lines[0], lines[1]))

    def test_new_series_cost_their_list_price_times_the_exchange_rate(self):
        # Opus: 1 M input (4) + 0.5 M output (10) + 10 M cache read (2) + 0.2 M cache write (1) = 17 USD = 25.5 AUD.
        row = self.run_on(
            sums=[series("a", "claude-opus-5-5", "input", 1000000), series("a", "claude-opus-5-5", "output", 500000),
                  series("a", "claude-opus-5-5", "cache_read", 10000000),
                  series("a", "claude-opus-5-5", "cache_write", 200000)],
            states=[state("a", "in_progress")])
        self.assertEqual(row["tokens_m"], "11.7")
        self.assertEqual(row["token_cost"], "25.5")
        self.assertEqual(row["total_cost"], "25.5")
        self.assertEqual(row["attention_cost"], "\\N")
        self.assertEqual(row["unpriced_models"], "")
        self.assertEqual(row["old_stock_unpriced"], "0")
        self.assertEqual(row["overflow"], "0")

    def mix_turns(self):
        # Input side: opus 1 M input (4) + 1 M cache read (0.2) + sonnet 2 M input (2): (4 + 0.2 + 4) / 4 M = 2.05 USD per M.
        # Output: opus 1 M (20) + sonnet 1 M (10): 15 USD per M.
        return [build_turn("claude-opus-5-5", "input", 1000000), build_turn("claude-opus-5-5", "cache_read", 1000000),
                build_turn("claude-sonnet-5-5", "input", 2000000), build_turn("claude-opus-5-5", "output", 1000000),
                build_turn("claude-sonnet-5-5", "output", 1000000),
                build_turn("claude-opus-5-5", "input", 90000000, role="chief")]

    def test_old_series_are_priced_at_the_build_worker_mix(self):
        # 2 M input at 2.05 = 4.1 and 0.1 M output at 15 = 1.5: 5.6 USD = 8.4 AUD.
        row = self.run_on(sums=[series("a", None, "input", 2000000), series("a", None, "output", 100000),
                                *self.mix_turns()],
                          states=[state("a", "approved")])
        self.assertEqual(row["token_cost"], "8.4")
        self.assertEqual(row["old_stock_unpriced"], "0")

    def test_a_spec_with_old_and_new_series_adds_both(self):
        # Old: 2 M input at 2.05 = 4.1. New: sonnet 1 M input (2) and 1 M output (10). 16.1 USD = 24.15 AUD.
        row = self.run_on(sums=[series("a", None, "input", 2000000), series("a", "claude-sonnet-5-5", "input", 1000000),
                                series("a", "claude-sonnet-5-5", "output", 1000000), *self.mix_turns()],
                          states=[state("a", "in_progress")])
        self.assertEqual(row["token_cost"], "24.15")

    def test_an_empty_mix_prices_old_stock_at_nothing_and_says_so(self):
        row = self.run_on(sums=[series("a", None, "input", 2000000), series("b", "claude-opus-5-5", "input", 1000000)],
                          states=[state("a", "in_progress"), state("b", "in_progress")])
        self.assertEqual(row["token_cost"], "6")  # only the new series: 4 USD = 6 AUD
        self.assertEqual(row["old_stock_unpriced"], "1")
        self.assertEqual(row["tokens_m"], "3")

    def test_a_model_with_no_list_price_is_named_and_adds_nothing(self):
        row = self.run_on(sums=[series("a", "claude-unknown-9", "input", 1000000),
                                series("a", "claude-opus-5-5", "input", 1000000)],
                          states=[state("a", "in_progress")])
        self.assertEqual(row["unpriced_models"], "claude-unknown-9")
        self.assertEqual(row["token_cost"], "6")

    def test_a_kind_with_no_list_price_names_its_model(self):
        prices = {"claude-opus-5-5": {"input": 4.0}}
        row = self.run_on(sums=[series("a", "claude-opus-5-5", "input", 1000000),
                                series("a", "claude-opus-5-5", "cache_write", 1000000)],
                          states=[state("a", "in_progress")], dictionary=settings(prices=prices))
        self.assertEqual(row["unpriced_models"], "claude-opus-5-5")
        self.assertEqual(row["token_cost"], "6")

    def test_no_exchange_rate_gives_no_token_cost_and_the_total_is_the_human_attention(self):
        row = self.run_on(sums=[series("a", "claude-opus-5-5", "input", 1000000)], states=[state("a", "in_progress")],
                          logs=[answered(7200)], dictionary=settings(rate=None))
        self.assertEqual(row["token_cost"], "\\N")
        self.assertEqual(row["attention_cost"], "200")
        self.assertEqual(row["total_cost"], "200")

    def test_no_hourly_rate_gives_no_attention_cost_and_the_total_is_the_tokens(self):
        row = self.run_on(sums=[series("a", "claude-opus-5-5", "input", 1000000)], states=[state("a", "in_progress")],
                          logs=[answered(7200)], dictionary=settings(hourly=None))
        self.assertEqual(row["attention_cost"], "\\N")
        self.assertEqual(row["currency"], "\\N")
        self.assertEqual(row["token_cost"], "6")
        self.assertEqual(row["total_cost"], "6")

    def test_the_total_adds_tokens_and_human_attention(self):
        row = self.run_on(sums=[series("a", "claude-opus-5-5", "input", 1000000)], states=[state("a", "in_progress")],
                          logs=[answered(7200)])
        self.assertEqual((row["token_cost"], row["attention_cost"], row["total_cost"]), ("6", "200", "206"))

    def test_the_newest_price_prices_the_stock_now_and_the_stock_7_days_ago(self):
        # 1 M input recorded 9 days ago, then the counter rose to 3 M. The one price (4 USD, 1.5) prices both.
        row = self.run_on(
            sums=[series("a", "claude-opus-5-5", "input", 1000000, hours_ago=9 * 24),
                  series("a", "claude-opus-5-5", "input", 3000000, hours_ago=1)],
            states=[state("a", "in_progress", hours_ago=8 * 24), state("a", "in_progress")])
        self.assertEqual(row["token_cost"], "18")
        self.assertEqual(row["total_cost_change_pct"], "200")  # 6 AUD then, 18 AUD now

    def test_a_spec_that_is_not_approved_or_in_progress_has_no_stock(self):
        self.assertIsNone(self.run_on(sums=[series("a", "claude-opus-5-5", "input", 1000000)],
                                      states=[state("a", "completed")]))

    def test_the_overflow_series_sets_overflow(self):
        row = self.run_on(sums=[series("a", "claude-opus-5-5", "input", 1000000), overflow_point()],
                          states=[state("a", "in_progress")])
        self.assertEqual(row["overflow"], "1")


class InventoryCostTextTest(unittest.TestCase):
    """These tests read the file as text. They need no ClickHouse."""

    def test_the_query_only_reads_and_prices_by_the_dictionary(self):
        text = QUERY.read_text(encoding="utf-8")
        code = "\n".join(line.split("--")[0] for line in text.splitlines())
        self.assertNotRegex(code, r"(?i)\b(INSERT|ALTER|CREATE|DROP|TRUNCATE|DELETE)\b")
        for name in (LIST, RATE, HOURLY):
            self.assertIn(name, code)
        for column in ("token_cost", "attention_cost", "total_cost", "total_cost_change_pct", "unpriced_models",
                       "old_stock_unpriced", "overflow"):
            self.assertRegex(code, rf"\bAS {column}\b")


if __name__ == "__main__":
    unittest.main()
