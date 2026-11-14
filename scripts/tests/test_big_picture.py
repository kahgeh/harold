"""Check that the big picture dashboard, the trends dashboard and their named queries do not drift.

The structure tests read files as text, with the standard library only. They need no
ClickHouse, no port and no server.

The files:
- hyperdx/big-picture.json holds an intro text and eight cards. A card is ONE table tile with ONE
  text line: the value with its unit, then its change against the 7 days before. The card Decisions is
  the one exception: its tile has three rows, one text line each (fmt "decisions", built with arrayJoin).
- hyperdx/trends.json holds the cards that left the big picture, as tables.
- queries/<card>.sql holds the SQL of the card. It returns at most one row of numbers.

The rule that links a tile to its file: the sqlTemplate of a card tile is
"SELECT concat(...) AS ` ` FROM (" + the text of the file + ") WHERE <value> IS NOT NULL".
The wrap() function below builds it. A column of a change ends with _change_pct. It is already a percent.

The line tests send the wrap with fake rows (constants only, SELECT only) to the read-only
ClickHouse through bin/logq. They skip when bin/logq or ClickHouse is not there, as in CI.

The files agent-counter.sql and factory-flow.sql are not cards. The test ignores them.
The web app page /flow reads factory-flow.sql.
"""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
TELEMETRY = ROOT / "docs" / "references" / "telemetry"
HYPERDX = TELEMETRY / "hyperdx"
DASHBOARD = HYPERDX / "big-picture.json"
TRENDS = HYPERDX / "trends.json"
QUERIES = TELEMETRY / "queries"
# agent-counter.sql is a helper that other queries copy. factory-flow.sql feeds the web app page /flow. No tile reads them.
NOT_CARDS = {"agent-counter.sql", "factory-flow.sql", "unused-features.sql", "unconfirmed-fixes.sql", "unkinded-specs.sql",
             "fix-cost-by-cause.sql", "restart-checks.sql", "reply-owner.sql", "dictation-taps.sql", "agent-use.sql",
             "workflow-cost.sql"}
# These dashboards may hold a tile that reads a query file.
DASHBOARD_FILES = ["big-picture.json", "trends.json", "dashboard.json", "now.json"]

# card file -> the card. "fmt" is count (whole number with thousands separators) or d1 (one decimal).
# "extra" names the part after the change. "better" is the direction that is better.
CARDS = {
    "value-created": dict(title="Value created", value="value", unit="features and fixes", fmt="count",
                          change="value_change_pct", extra=None, better="higher"),
    "delivery-lead-time-tile": dict(title="Delivery lead time", value="days", unit="days", fmt="d1",
                                    change="days_change_pct", extra=None, better="lower"),
    "waste": dict(title="Waste", value="hours", unit="hours", fmt="count",
                  change="hours_change_pct", extra=None, better="lower"),
    "tokens-and-turns": dict(title="Tokens and turns", value="tokens_m", unit="M tokens", fmt="d1",
                             change="tokens_m_change_pct", extra="turns", better="lower"),
    "products-changed": dict(title="Products changed", value="products", unit="products", fmt="count",
                             change="products_change_pct", extra=None, better="higher"),
    "inventory-cost": dict(title="Inventory cost", value="total_cost", unit="", fmt="aud",
                           change="total_cost_change_pct", extra="notes", better="lower"),
    # One line: the cost of each unit of value. Down is better.
    "cost-per-value": dict(title="Cost per value", value="total_cost", unit="", fmt="cpv",
                           change="cost_per_value_change_pct", extra="cpv", better="lower"),
    # Three lines. Made, evaluated and the two good shares are better up, and due is better down.
    "decisions": dict(title="Decisions", value="made", unit="made", fmt="decisions",
                      change="made_change_pct", extra=None, better="higher"),
}
# The columns that the card Decisions reads, besides made and made_change_pct.
DECISION_COLUMNS = ["evaluated", "evaluated_change_pct", "good_process_pct", "good_process_change_pts",
                    "good_outcome_pct", "good_outcome_change_pts", "due", "due_change_pct"]
# The columns that the card Cost per value reads, besides total_cost and cost_per_value_change_pct.
CPV_COLUMNS = ["cost_per_value", "value", "fix_cost", "unpriced_models"]
# The stubs return no row until the spec that fills them is built.
STUBS = {}
INTRO = (
    "The last 7 days, against the 7 days before. All products. Each card is one line: the value, "
    "then its change on last week, with an arrow (\u25b2 up, \u25bc down) and a dot: "
    "\U0001f7e2 better, \U0001f534 worse, \u26aa no change. "
    "\"No data found within time range.\" means \"no data yet\". "
    "A line that ends in \"no change figure\" has no number in the 7 days before to compare with. "
    "Higher is better: Value created, Products changed, Decisions (made, evaluated, good process and good outcome). "
    "Lower is better: Delivery lead time, Waste, Tokens and turns, Inventory cost, Decisions (due), Cost per value. "
    "Decisions has three lines: made and evaluated, the share graded good (its change is in percentage points, pts), and due. "
    "Cost per value is the cost of the 7 days in Australian dollars (tokens, human attention and cloud spend) divided by the value of the 7 days. "
    "\"no value\" means a cost and no value in the 7 days. \"fixes A$<n> (waste)\" is the part of the cost spent on fixes. "
    "Delivery lead time has data after the first release that follows spec-release-phase. "
    "Inventory cost is in Australian dollars (A$): the tokens at the list prices, plus human attention. "
    "\"human attention: no data yet\" means no answer in the last 7 days, "
    "\"human attention: no rate set\" means answers but no hourly rate, "
    "and \"tokens: no exchange rate set\" means no exchange rate. "
    "A note after the change, \"not priced: <model>\", \"old stock not priced\" or \"past the series limit\", is a defect to fix."
)
# The queries that the cards of Harold trends read, as they are now.
TRENDS_CARDS = [
    "delivery-lead-time",
    "slicing-and-sizing",
    "test-and-release-time",
    "clarification-requests",
    "work-waiting",
    "attention-recording",
]
CAVEAT = re.compile(r"(?i)\b(empty|no data yet|waits? for)\b")
MACRO = re.compile(r"\$__\w+")


def grp(x):
    """ClickHouse text of the whole number x with thousands separators (up to 999,999,999)."""
    n = f"toUInt64(round({x}))"
    three = lambda e: f"leftPad(toString({e}), 3, '0')"
    return (f"multiIf({n} >= 1000000, concat(toString(intDiv({n}, 1000000)), ',', {three(f'intDiv({n}, 1000) % 1000')}, ',', {three(f'{n} % 1000')}), "
            f"{n} >= 1000, concat(toString(intDiv({n}, 1000)), ',', {three(f'{n} % 1000')}), toString({n}))")


def d1(x):
    """ClickHouse text of x with one decimal."""
    r = f"toInt64(round({x} * 10))"
    return f"concat(toString(intDiv({r}, 10)), '.', toString({r} % 10))"


GREEN, RED, WHITE = "\U0001f7e2", "\U0001f534", "\u26aa"


def change_text(column, better):
    """Text of the change. The dot is green when the change goes the better way (higher or lower)."""
    rise, fall = (GREEN, RED) if better == "higher" else (RED, GREEN)
    r = f"round({column})"
    return (f"multiIf({column} IS NULL, 'no change figure', "
            f"{r} > 0, concat('{rise} \u25b2 ', toString(toInt64({r})), '% on last week'), "
            f"{r} < 0, concat('{fall} \u25bc ', toString(abs(toInt64({r}))), '% on last week'), "
            f"'{WHITE} 0% on last week')")


def extra_text(kind):
    if kind is None:
        return None
    if kind == "turns":
        return f"if(turns IS NULL, '', concat(' \u00b7 ', {grp('turns')}, ' turns'))"
    return ("concat("
            "arrayStringConcat(arrayMap(m -> concat(' \u00b7 not priced: ', m), "
            "arrayFilter(m -> m != '', splitByChar(',', ifNull(unpriced_models, '')))), ''), "
            "if(ifNull(old_stock_unpriced, 0) = 1, ' \u00b7 old stock not priced', ''), "
            "if(ifNull(overflow, 0) = 1, ' \u00b7 past the series limit', ''))")


def cost_text():
    """The Inventory cost line before the change: the total, then the two parts in parentheses."""
    tokens = ("multiIf(token_cost IS NOT NULL, concat('tokens A$', " + grp("token_cost") + "), "
              "tokens_m IS NOT NULL, 'tokens: no exchange rate set', 'tokens A$0')")
    human = ("multiIf(attention_hours IS NULL, 'human attention: no data yet', "
             "attention_cost IS NULL, 'human attention: no rate set', "
             "concat('human attention A$', " + grp("attention_cost") + "))")
    return f"concat('A$', {grp('total_cost')}, '  (', {tokens}, ' + ', {human}, ')  ')"


def cpv_text():
    """The Cost per value line: the cost of each unit of value, the change, the fix cost and the unpriced models."""
    with_value = (f"concat('A$', {grp('cost_per_value')}, ' per value  (A$', {grp('total_cost')}, ' / ', "
                  f"{grp('value')}, ' value)  ', {change_text('cost_per_value_change_pct', 'lower')})")
    no_value = f"concat('no value  (cost A$', {grp('total_cost')}, ')')"
    fixes = f"if(ifNull(fix_cost, 0) > 0, concat(' \u00b7 fixes A$', {grp('fix_cost')}, ' (waste)'), '')"
    unpriced = ("arrayStringConcat(arrayMap(m -> concat(' \u00b7 not priced: ', m), "
                "arrayFilter(m -> m != '', splitByChar(',', ifNull(unpriced_models, '')))), '')")
    return f"concat(if(cost_per_value IS NULL, {no_value}, {with_value}), {fixes}, {unpriced})"


def pct_change_text(column, better):
    """The change of a figure in percent, with the dot and the arrow, and no "on last week"."""
    rise, fall = (GREEN, RED) if better == "higher" else (RED, GREEN)
    r = f"round({column})"
    return (f"multiIf({column} IS NULL, 'no change figure', "
            f"{r} > 0, concat('{rise} \u25b2 ', toString(toInt64({r})), '%'), "
            f"{r} < 0, concat('{fall} \u25bc ', toString(abs(toInt64({r}))), '%'), "
            f"'{WHITE} 0%')")


def pts_change_text(column, better):
    """The change of a share in percentage points, with one decimal."""
    rise, fall = (GREEN, RED) if better == "higher" else (RED, GREEN)
    r = f"round({column}, 1)"
    return (f"multiIf({column} IS NULL, 'no change figure', "
            f"{r} > 0, concat('{rise} \u25b2 ', {d1(column)}, ' pts'), "
            f"{r} < 0, concat('{fall} \u25bc ', {d1(f'abs({column})')}, ' pts'), "
            f"'{WHITE} 0.0 pts')")


def share_text(column):
    return f"if({column} IS NULL, 'no data yet', concat(toString(toInt64({column})), '%'))"


def decision_lines():
    """The three lines of the card Decisions: made and evaluated, the good shares, and due."""
    return [
        f"concat({grp('made')}, ' made ', {pct_change_text('made_change_pct', 'higher')}, ' \u00b7 ', "
        f"{grp('evaluated')}, ' evaluated ', {pct_change_text('evaluated_change_pct', 'higher')})",
        f"concat('good process ', {share_text('good_process_pct')}, ' ', "
        f"{pts_change_text('good_process_change_pts', 'higher')}, ' \u00b7 good outcome ', "
        f"{share_text('good_outcome_pct')}, ' ', {pts_change_text('good_outcome_change_pts', 'higher')})",
        f"concat({grp('due')}, ' due ', {change_text('due_change_pct', 'lower')})",
    ]


def wrap(card, source):
    """Return the sqlTemplate of the tile of the card. The source is the text of the file or a fake SELECT."""
    spec = CARDS[card]
    if spec["fmt"] == "decisions":
        return (f"SELECT arrayJoin([{', '.join(decision_lines())}]) AS ` `\n"
                f"FROM (\n{source.rstrip()}\n)\nWHERE {spec['value']} IS NOT NULL")
    if spec["fmt"] == "cpv":
        return (f"SELECT {cpv_text()} AS ` `\nFROM (\n{source.rstrip()}\n)\nWHERE {spec['value']} IS NOT NULL")
    if spec["fmt"] == "aud":
        parts = [cost_text(), change_text(spec["change"], spec["better"])]
    else:
        number = (grp if spec["fmt"] == "count" else d1)(spec["value"])
        parts = [number, f"' {spec['unit']}  '", change_text(spec["change"], spec["better"])]
    extra = extra_text(spec["extra"])
    if extra:
        parts.append(extra)
    return (f"SELECT concat({', '.join(parts)}) AS ` `\n"
            f"FROM (\n{source.rstrip()}\n)\nWHERE {spec['value']} IS NOT NULL")


def strip_comments(sql):
    return re.sub(r"(?m)--.*$", "", sql)


def restart_unsafe_metrics(code):
    """Return the harold.* and pod.* metrics read by a SELECT that uses maxIf(.

    The metric of a maxIf( is the next MetricName literal after it: the WHERE of its SELECT.
    """
    found = []
    for match in re.finditer(r"(?i)\bmaxIf\s*\(", code):
        name = re.search(r"MetricName\s*=\s*'([^']+)'", code[match.end():])
        if name and name.group(1).startswith(("harold.", "pod.")):
            found.append(name.group(1))
    return sorted(set(found))


def depth_zero(sql):
    """Return the text of the SQL with the contents of all parentheses blanked out."""
    out, depth = [], 0
    for char in sql:
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif depth == 0:
            out.append(char)
    return "".join(out)


def final_statement(sql):
    """Return the part of the SQL after the last depth-0 UNION ALL and its SELECT.

    This is the outermost SELECT of a query with no UNION ALL at depth 0.
    A WITH clause comes before it, and its parentheses do not count.
    """
    code = strip_comments(sql)
    depth, start, index = 0, 0, 0
    pattern = re.compile(r"(?i)\bUNION\s+ALL\b")
    while index < len(code):
        char = code[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif depth == 0:
            match = pattern.match(code, index)
            if match:
                start = match.end()
                index = match.end()
                continue
        index += 1
    tail = code[start:]
    select = re.search(r"(?i)\bSELECT\b", depth_zero(tail))
    if not select:
        return tail
    # Position of the depth-0 SELECT in the original tail.
    depth, position = 0, 0
    for index, char in enumerate(tail):
        if depth == 0 and re.match(r"(?i)SELECT\b", tail[index:]):
            position = index
            break
        depth += (char == "(") - (char == ")")
    return tail[position:]


class BigPictureTest(unittest.TestCase):
    def dashboard(self, path=DASHBOARD):
        if not path.is_file():
            self.fail(f"the dashboard file does not exist yet: {path.relative_to(ROOT)}")
        return json.loads(path.read_text(encoding="utf-8"))

    def tiles(self, path=DASHBOARD):
        return self.dashboard(path)["tiles"]

    def by_id(self, path=DASHBOARD):
        return {tile["id"]: tile for tile in self.tiles(path)}

    def query_text(self, card):
        path = QUERIES / f"{card}.sql"
        if not path.is_file():
            self.fail(f"card {card} has no query file: {path.relative_to(ROOT)}")
        return path.read_text(encoding="utf-8")

    def test_eight_cards_in_order_and_no_product_filter(self):
        dashboard = self.dashboard()
        self.assertEqual(dashboard["name"], "Harold: the big picture")
        self.assertEqual(dashboard.get("filters", []), [], "the big picture has no product filter")
        ids = [tile["id"] for tile in dashboard["tiles"]]
        self.assertEqual(ids, ["big-picture-intro"] + list(CARDS))
        titles = [tile["name"] for tile in dashboard["tiles"][1:]]
        self.assertEqual(titles, ["Value created", "Delivery lead time", "Waste", "Tokens and turns",
                                  "Products changed", "Inventory cost", "Cost per value", "Decisions"])

    def test_each_card_is_one_table_tile_that_wraps_its_file(self):
        tiles = self.by_id()
        for card, spec in CARDS.items():
            with self.subTest(card=card):
                config = tiles[card]["config"]
                self.assertEqual(config["displayType"], "table")
                self.assertEqual(config["configType"], "sql")
                self.assertEqual(config["connectionId"], "@CLICKHOUSE_CONNECTION_ID@")
                self.assertRegex(config["sourceId"], r"^@[A-Z_]+_SOURCE_ID@$")
                self.assertEqual(config["sqlTemplate"], wrap(card, self.query_text(card)))
                start = "SELECT arrayJoin([" if spec["fmt"] == "decisions" else "SELECT concat("
                self.assertTrue(config["sqlTemplate"].startswith(start))
                self.assertIn("AS ` `\nFROM (", config["sqlTemplate"], "the column name is one space")
                self.assertNotIn(f'AS "{spec["title"]}"', config["sqlTemplate"])
                self.assertTrue(config["sqlTemplate"].endswith(f"WHERE {spec['value']} IS NOT NULL"))

    def test_the_change_is_rounded_and_not_multiplied(self):
        for card, spec in CARDS.items():
            sql = self.by_id()[card]["config"]["sqlTemplate"]
            with self.subTest(card=card):
                self.assertIn(f"round({spec['change']})", sql)
                self.assertNotIn("* 100", sql.split("FROM (", 1)[0], "the wrap must not multiply the percent")
                self.assertNotIn("factor", json.dumps(self.by_id()[card]))

    def test_each_column_is_in_its_file(self):
        for card, spec in CARDS.items():
            sql = strip_comments(self.query_text(card))
            columns = [spec["value"], spec["change"]]
            if spec["extra"] == "turns":
                columns += ["turns"]
            if spec["fmt"] == "decisions":
                columns += DECISION_COLUMNS
            if spec["extra"] == "cpv":
                columns += CPV_COLUMNS
            if spec["extra"] == "notes":
                columns += ["tokens_m", "token_cost", "attention_hours", "attention_cost", "unpriced_models",
                            "old_stock_unpriced", "overflow"]
            for column in columns:
                with self.subTest(card=card, column=column):
                    self.assertRegex(sql, rf"(?i)\bAS\s+{column}\b")

    def test_no_card_tile_has_a_number_format_or_a_markdown_tile(self):
        for tile in self.tiles():
            with self.subTest(tile=tile["id"]):
                self.assertNotIn("numberFormat", tile["config"])
                self.assertNotEqual(tile["config"]["displayType"], "number")
        self.assertEqual([t["id"] for t in self.tiles() if t["config"]["displayType"] == "markdown"],
                         ["big-picture-intro"])

    def test_no_attention_currency_fill(self):
        self.assertNotIn("@ATTENTION_CURRENCY@", DASHBOARD.read_text(encoding="utf-8"))

    def test_the_stubs_return_no_row_and_name_their_spec(self):
        for card, spec_name in STUBS.items():
            with self.subTest(card=card):
                text = self.query_text(card)
                self.assertIn(spec_name, text.split("SELECT", 1)[0])
                self.assertRegex(strip_comments(text), r"(?i)\bWHERE\s+0\b")

    def test_no_title_carries_a_caveat(self):
        for tile in self.tiles():
            with self.subTest(tile=tile["id"]):
                self.assertTrue(tile.get("name", "").strip())
                self.assertIsNone(CAVEAT.search(tile["name"]), tile["name"])

    def test_the_intro_text(self):
        config = self.by_id()["big-picture-intro"]["config"]
        self.assertEqual(config["displayType"], "markdown")
        self.assertEqual(config["markdown"], INTRO)
        for phrase in ("last 7 days", "7 days before", "All products", "No data found within time range.",
                       "no data yet", "no change figure", "no rate set"):
            self.assertIn(phrase, INTRO)

    def test_the_intro_says_which_way_is_better(self):
        higher, lower = INTRO.split("Higher is better: ")[1].split(" Lower is better: ")
        for card, spec in CARDS.items():
            side = higher if spec["better"] == "higher" else lower
            with self.subTest(card=card):
                self.assertIn(spec["title"], side)

    def test_the_intro_explains_the_dots(self):
        for phrase in ("\U0001f7e2 better", "\U0001f534 worse", "\u26aa no change"):
            self.assertIn(phrase, INTRO)
        self.assertNotIn("\u25b2 is up and \u25bc is down", INTRO)

    def test_the_direction_of_each_card(self):
        self.assertEqual({card: spec["better"] for card, spec in CARDS.items()},
                         {"value-created": "higher", "delivery-lead-time-tile": "lower", "waste": "lower",
                          "tokens-and-turns": "lower", "products-changed": "higher", "inventory-cost": "lower",
                          "cost-per-value": "lower", "decisions": "higher"})

    def test_the_change_text_puts_the_dot_by_direction(self):
        up = change_text("c", "higher")
        down = change_text("c", "lower")
        self.assertIn("'\U0001f7e2 \u25b2 '", up)
        self.assertIn("'\U0001f534 \u25bc '", up)
        self.assertIn("'\U0001f534 \u25b2 '", down)
        self.assertIn("'\U0001f7e2 \u25bc '", down)
        for text in (up, down):
            self.assertIn("'\u26aa 0% on last week'", text)
            self.assertIn("'no change figure'", text)

    def test_the_now_card_left(self):
        self.assertNotIn("now", self.by_id())

    def test_tiles_do_not_overlap_and_fit_the_grid(self):
        placed = []
        for tile in self.tiles():
            with self.subTest(tile=tile["id"]):
                for key in ("x", "y", "w", "h"):
                    self.assertIsInstance(tile[key], int)
                self.assertGreaterEqual(tile["x"], 0)
                self.assertLessEqual(tile["x"] + tile["w"], 24)
                for other in placed:
                    apart = (tile["x"] >= other["x"] + other["w"] or other["x"] >= tile["x"] + tile["w"]
                             or tile["y"] >= other["y"] + other["h"] or other["y"] >= tile["y"] + tile["h"])
                    self.assertTrue(apart, f"{tile['id']} overlaps {other['id']}")
            placed.append(tile)

    def test_trends_tiles_read_their_files_as_tables(self):
        tiles = self.by_id(TRENDS)
        for card in TRENDS_CARDS:
            with self.subTest(card=card):
                self.assertIn(card, tiles)
                self.assertEqual(tiles[card]["config"]["sqlTemplate"], self.query_text(card))
                self.assertEqual(tiles[card]["config"]["displayType"], "table")

    def test_trends_has_no_big_picture_card(self):
        tiles = self.by_id(TRENDS)
        for card in CARDS:
            self.assertNotIn(card, tiles)

    def test_every_query_file_is_read_by_a_card(self):
        sqls = []
        for name in DASHBOARD_FILES:
            path = HYPERDX / name
            if not path.is_file():
                self.fail(f"the dashboard file does not exist yet: {path.relative_to(ROOT)}")
            for tile in self.tiles(path):
                sql = tile.get("config", {}).get("sqlTemplate")
                if sql:
                    sqls.append(sql)
        for path in sorted(QUERIES.glob("*.sql")):
            if path.name in NOT_CARDS:
                continue
            with self.subTest(file=path.name):
                text = path.read_text(encoding="utf-8").rstrip()
                self.assertTrue(any(text in sql for sql in sqls), "no tile reads this file")

    def test_every_card_query_returns_at_most_one_row(self):
        # Offline there is no ClickHouse. The test checks the SQL text.
        # An aggregate with no GROUP BY returns one row of 0 for an empty window.
        # HAVING (or LIMIT 1) gives no row instead, and the tile shows HyperDX's empty state.
        for card in list(CARDS) + ["now"]:
            with self.subTest(card=card):
                sql = self.query_text(card)
                final = depth_zero(final_statement(sql))
                self.assertNotRegex(final, r"(?i)\bGROUP\s+BY\b", "the outermost SELECT groups")
                self.assertRegex(final, r"(?i)\b(HAVING|LIMIT\s+1)\b", "the outermost SELECT can return no row")
                self.assertNotRegex(depth_zero(strip_comments(sql)), r"(?i)\bUNION\b")
                for macro in set(MACRO.findall(sql)):
                    self.assertEqual(macro, "$__filters", "only $__filters is allowed")

    def test_the_overflow_columns_exist(self):
        for card in ("tokens-and-turns", "inventory-cost"):
            with self.subTest(card=card):
                code = strip_comments(self.query_text(card))
                self.assertRegex(code, r"(?i)\bAS\s+overflow\b")
                self.assertIn("otel.metric.overflow", code)

    def test_the_attention_rate_comes_from_the_dictionary(self):
        code = strip_comments(self.query_text("inventory-cost"))
        self.assertIn("dictGetOrNull('otel.harold_settings'", code)
        self.assertIn("harold.attention.hourly_rate", code)

    def test_a_later_rate_changes_the_cost_but_not_the_hours(self):
        code = strip_comments(self.query_text("inventory-cost"))
        hours = re.search(r"(?m)^\s*if\(([^\n]*?)\) AS attention_hours,", code).group(1)
        cost = re.search(r"(?m)^\s*if\(([^\n]*?)\) AS attention_cost,", code).group(1)
        self.assertNotRegex(hours, r"\brate\b")
        self.assertIn("hours_now * rate", cost)

    def test_change_percent_is_whole_and_safe_for_zero(self):
        for card in CARDS:
            if card in STUBS:
                continue
            code = strip_comments(self.query_text(card))
            with self.subTest(card=card):
                self.assertRegex(code, r"(?i)round\(")
                self.assertRegex(code, r"(?i)nullIf\([^()]*(?:\([^()]*\)[^()]*)*,\s*0\)", "change divides by nullIf(before, 0)")

    def test_no_provider_name_in_the_cards(self):
        names = re.compile(r"(?i)claude|anthropic|codex|openai|gemini")
        for card in list(CARDS) + ["now"]:
            with self.subTest(card=card):
                self.assertIsNone(names.search(self.query_text(card)))
        for path in (DASHBOARD, HYPERDX / "now.json"):
            with self.subTest(file=path.name):
                self.assertIsNone(names.search(path.read_text(encoding="utf-8")))

    def test_the_guard_check_sees_only_the_outermost_select(self):
        inner_only = "SELECT a FROM (SELECT a, count() FROM t GROUP BY a) ORDER BY a"
        self.assertNotRegex(depth_zero(final_statement(inner_only)), r"(?i)\bGROUP\s+BY\b")
        outer = "WITH c AS (SELECT 1) SELECT a FROM t GROUP BY a"
        self.assertRegex(depth_zero(final_statement(outer)), r"(?i)\bGROUP\s+BY\b")
        union = "SELECT a FROM t GROUP BY a UNION ALL SELECT b FROM u"
        self.assertNotRegex(depth_zero(final_statement(union)), r"(?i)\bGROUP\s+BY\b")

    def test_harold_and_pod_counters_use_the_reset_rule(self):
        # The README counter rule counts a drop by its whole value. A maxIf( total, or the
        # difference of two maxIf( totals, loses the counts before each restart of Harold.
        # Only declared agent.* counters keep their totals, so only they may read maxIf(.
        for path in sorted(QUERIES.glob("*.sql")):
            code = strip_comments(path.read_text(encoding="utf-8"))
            with self.subTest(file=path.name):
                self.assertEqual(restart_unsafe_metrics(code), [])

    def test_the_reset_rule_check_finds_a_plain_maxif_difference(self):
        bad = ("SELECT maxIf(Value, a) - maxIf(Value, b) AS d FROM t "
               "WHERE MetricName = 'harold.turns'")
        self.assertEqual(restart_unsafe_metrics(bad), ["harold.turns"])
        greatest = ("SELECT greatest(maxIf(Value, a) - maxIf(Value, b), 0) FROM t "
                    "WHERE MetricName = 'pod.merges'")
        self.assertEqual(restart_unsafe_metrics(greatest), ["pod.merges"])
        agent = ("SELECT maxIf(Value, a) - maxIf(Value, b) FROM t "
                 "WHERE MetricName = 'agent.harold.release.seconds'")
        self.assertEqual(restart_unsafe_metrics(agent), [])

    def test_every_query_file_has_a_note_and_a_wait_line(self):
        files = [path for path in QUERIES.glob("*.sql") if path.name not in NOT_CARDS]
        self.assertTrue(files, "no query files")
        for path in sorted(files):
            text = path.read_text(encoding="utf-8")
            with self.subTest(file=path.name):
                self.assertRegex(text, r"(?m)^-- Note: \S", "no '-- Note:' line")
                self.assertRegex(text, r"(?m)^-- Waits for: \S", "no '-- Waits for:' line")


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


def fake_source(**columns):
    """Return a SELECT of constants: one fake row. None is NULL. The SQL reads no table."""
    parts = []
    for name, value in columns.items():
        if isinstance(value, str):
            parts.append(f"CAST('{value}' AS Nullable(String)) AS {name}")
        elif value is None:
            parts.append(f"CAST(NULL AS Nullable(Float64)) AS {name}")
        else:
            parts.append(f"CAST({value} AS Nullable(Float64)) AS {name}")
    return "SELECT " + ", ".join(parts)


class LineWrapTest(unittest.TestCase):
    """The wrap on fake rows. It runs the SQL (SELECT of constants only) through bin/logq."""

    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the line tests need the read-only ClickHouse")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the line tests are skipped")

    def line(self, card, **columns):
        sql = wrap(card, fake_source(**columns))
        self.assertRegex(sql, r"(?i)^SELECT\b")
        done = subprocess.run([self.logq], input=sql, capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return done.stdout.rstrip("\n")

    def test_a_rise_a_fall_and_no_change(self):
        self.assertEqual(self.line("value-created", value=4717, value_change_pct=57),
                         "4,717 features and fixes  \U0001f7e2 \u25b2 57% on last week")
        self.assertEqual(self.line("value-created", value=4717, value_change_pct=-12),
                         "4,717 features and fixes  \U0001f534 \u25bc 12% on last week")
        self.assertEqual(self.line("waste", hours=9, hours_change_pct=9),
                         "9 hours  \U0001f534 \u25b2 9% on last week")
        self.assertEqual(self.line("waste", hours=9, hours_change_pct=-12),
                         "9 hours  \U0001f7e2 \u25bc 12% on last week")
        self.assertEqual(self.line("waste", hours=9, hours_change_pct=0), "9 hours  \u26aa 0% on last week")
        self.assertEqual(self.line("value-created", value=4717, value_change_pct=0),
                         "4,717 features and fixes  \u26aa 0% on last week")

    def test_a_null_change_gives_no_change_figure(self):
        self.assertEqual(self.line("waste", hours=9, hours_change_pct=None), "9 hours  no change figure")

    def test_a_null_value_gives_no_row(self):
        self.assertEqual(self.line("waste", hours=None, hours_change_pct=5), "")
        self.assertEqual(self.line("value-created", value=None, value_change_pct=None), "")

    def test_numbers_have_thousands_separators_and_one_decimal(self):
        self.assertEqual(self.line("products-changed", products=999, products_change_pct=1), "999 products  \U0001f7e2 \u25b2 1% on last week")
        self.assertEqual(self.line("products-changed", products=1000, products_change_pct=1), "1,000 products  \U0001f7e2 \u25b2 1% on last week")
        self.assertEqual(self.line("products-changed", products=1234567, products_change_pct=1), "1,234,567 products  \U0001f7e2 \u25b2 1% on last week")
        self.assertEqual(self.line("delivery-lead-time-tile", days=5, days_change_pct=None), "5.0 days  no change figure")
        self.assertEqual(self.line("delivery-lead-time-tile", days=12.34, days_change_pct=2), "12.3 days  \U0001f534 \u25b2 2% on last week")

    def test_tokens_and_turns(self):
        self.assertEqual(
            self.line("tokens-and-turns", tokens_m=4.7, tokens_m_change_pct=-12, turns=4717, turns_change_pct=56),
            "4.7 M tokens  \U0001f7e2 \u25bc 12% on last week \u00b7 4,717 turns")

    def cost(self, **columns):
        row = dict(total_cost=1240, total_cost_change_pct=12, token_cost=940, tokens_m=11.7, attention_hours=3,
                   attention_cost=300, unpriced_models="", old_stock_unpriced=0, overflow=0)
        row.update(columns)
        return self.line("inventory-cost", **row)

    def test_inventory_cost_full_form(self):
        self.assertEqual(self.cost(),
                         "A$1,240  (tokens A$940 + human attention A$300)  \U0001f534 \u25b2 12% on last week")
        self.assertEqual(self.cost(total_cost=1234567, token_cost=1234267, total_cost_change_pct=-3),
                         "A$1,234,567  (tokens A$1,234,267 + human attention A$300)  \U0001f7e2 \u25bc 3% on last week")
        self.assertEqual(self.cost(total_cost=940, total_cost_change_pct=0, attention_hours=None, attention_cost=None),
                         "A$940  (tokens A$940 + human attention: no data yet)  \u26aa 0% on last week")

    def test_inventory_cost_dot_for_a_rise_and_a_fall(self):
        self.assertIn("\U0001f534 \u25b2 12%", self.cost(total_cost_change_pct=12))
        self.assertIn("\U0001f7e2 \u25bc 12%", self.cost(total_cost_change_pct=-12))
        self.assertTrue(self.cost(total_cost_change_pct=None).rstrip().endswith("no change figure"))

    def test_inventory_cost_missing_parts(self):
        self.assertEqual(self.cost(attention_cost=None, total_cost=940),
                         "A$940  (tokens A$940 + human attention: no rate set)  \U0001f534 \u25b2 12% on last week")
        self.assertEqual(self.cost(token_cost=None, total_cost=300),
                         "A$300  (tokens: no exchange rate set + human attention A$300)  \U0001f534 \u25b2 12% on last week")

    def test_inventory_cost_notes(self):
        base = "A$1,240  (tokens A$940 + human attention A$300)  \U0001f534 \u25b2 12% on last week"
        self.assertEqual(self.cost(unpriced_models="claude-a"), base + " \u00b7 not priced: claude-a")
        self.assertEqual(self.cost(unpriced_models="claude-a,claude-b"),
                         base + " \u00b7 not priced: claude-a \u00b7 not priced: claude-b")
        self.assertEqual(self.cost(old_stock_unpriced=1), base + " \u00b7 old stock not priced")
        self.assertEqual(self.cost(overflow=1), base + " \u00b7 past the series limit")
        self.assertEqual(self.cost(unpriced_models="claude-a", old_stock_unpriced=1, overflow=1),
                         base + " \u00b7 not priced: claude-a \u00b7 old stock not priced \u00b7 past the series limit")

    def test_inventory_cost_words(self):
        for text in (self.cost(attention_hours=None, attention_cost=None), self.cost(attention_cost=None)):
            self.assertNotRegex(text, r"(?<!human )attention")


    def cpv(self, **columns):
        row = dict(cost_per_value=357, cost_per_value_change_pct=-12, value=6, total_cost=2140, fix_cost=310,
                   unpriced_models="")
        row.update(columns)
        return self.line("cost-per-value", **row)

    def test_cost_per_value_full_form(self):
        self.assertEqual(self.cpv(), "A$357 per value  (A$2,140 / 6 value)  \U0001f7e2 \u25bc 12% on last week"
                                     " \u00b7 fixes A$310 (waste)")

    def test_cost_per_value_a_rise_a_fall_and_no_change_figure(self):
        self.assertIn("\U0001f534 \u25b2 12% on last week", self.cpv(cost_per_value_change_pct=12))
        self.assertIn("\U0001f7e2 \u25bc 12% on last week", self.cpv(cost_per_value_change_pct=-12))
        self.assertIn("\u26aa 0% on last week", self.cpv(cost_per_value_change_pct=0))
        self.assertIn("  no change figure", self.cpv(cost_per_value_change_pct=None))

    def test_cost_per_value_with_no_fix_cost_has_no_fix_note(self):
        self.assertNotIn("fixes", self.cpv(fix_cost=None))
        self.assertNotIn("fixes", self.cpv(fix_cost=0))

    def test_cost_per_value_with_cost_and_no_value(self):
        self.assertEqual(self.cpv(cost_per_value=None, cost_per_value_change_pct=None, value=0, fix_cost=None),
                         "no value  (cost A$2,140)")
        self.assertEqual(self.cpv(cost_per_value=None, cost_per_value_change_pct=5, value=0, fix_cost=310),
                         "no value  (cost A$2,140) \u00b7 fixes A$310 (waste)")

    def test_cost_per_value_with_no_cost_gives_no_row(self):
        self.assertEqual(self.cpv(total_cost=None, cost_per_value=None, cost_per_value_change_pct=None), "")

    def test_cost_per_value_names_each_unpriced_model(self):
        self.assertTrue(self.cpv(fix_cost=None, unpriced_models="model-a,model-b").endswith(
            "on last week \u00b7 not priced: model-a \u00b7 not priced: model-b"))


class AttentionRecordingCard(unittest.TestCase):
    """The trends card that shows the attention requests against the chief's messages."""

    def test_the_query_has_the_columns_and_the_14_days(self):
        text = (QUERIES / "attention-recording.sql").read_text(encoding="utf-8")
        code = strip_comments(text)
        for column in ("day", "attention_requests", "answers", "chief_messages"):
            self.assertRegex(code, rf"\bAS {column}\b")
        self.assertIn("range(14)", code)
        for name in ("chief.attention_request", "chief.attention_answered", "harold.notifications"):
            self.assertIn(name, code)
        self.assertIn("endsWith(Attributes['agent'], '/chief-of-pods')", code)
        self.assertIn("'sent'", code)


if __name__ == "__main__":
    unittest.main()
