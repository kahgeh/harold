"""Check that the big picture dashboard, the trends dashboard and their named queries do not drift.

The test reads files as text, with the standard library only. It needs no
ClickHouse, no port and no server.

The files:
- hyperdx/big-picture.json holds five cards. A card is a markdown tile (its name and its
  description), a value tile and a change tile, and the detail tiles that the card needs.
- hyperdx/trends.json holds the cards that left the big picture, as tables.
- queries/<card>.sql holds the SQL of the card. It returns at most one row.

The rule that links a tile to its file: the sqlTemplate of a number tile is
"SELECT <column> FROM (" + the text of the file + ")" with an optional WHERE tail.
A column of a change tile ends with _change_pct.

The files agent-counter.sql, factory-flow.sql and README.md are not cards. The test ignores them.
The web app page /flow reads factory-flow.sql.
"""

import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
TELEMETRY = ROOT / "docs" / "references" / "telemetry"
HYPERDX = TELEMETRY / "hyperdx"
DASHBOARD = HYPERDX / "big-picture.json"
TRENDS = HYPERDX / "trends.json"
QUERIES = TELEMETRY / "queries"
# agent-counter.sql is a helper that other queries copy. factory-flow.sql feeds the web app page /flow. No tile reads them.
NOT_CARDS = {"agent-counter.sql", "factory-flow.sql"}
# These dashboards may hold a tile that reads a query file.
DASHBOARD_FILES = ["big-picture.json", "trends.json", "dashboard.json", "now.json"]

# card -> (title, the tiles). A tile is (id, column, where tail, unit).
# The unit is the numberFormat unit of the tile. "%" marks a change tile.
CARDS = {
    "value-created": ("Value created", [
        ("value-created-value", "value", None, "work records"),
        ("value-created-change", "value_change_pct", None, "%"),
    ]),
    "waste": ("Waste", [
        ("waste-value", "hours", None, "hours"),
        ("waste-change", "hours_change_pct", None, "%"),
    ]),
    "tokens-and-turns": ("Tokens and turns", [
        ("tokens-and-turns-tokens-value", "tokens_m", None, "M tokens"),
        ("tokens-and-turns-tokens-change", "tokens_m_change_pct", None, "%"),
        ("tokens-and-turns-turns-value", "turns", None, "turns"),
        ("tokens-and-turns-turns-change", "turns_change_pct", None, "%"),
        ("tokens-and-turns-overflow", "overflow", "overflow = 1", "specs past the limit"),
    ]),
    "products-changed": ("Products changed", [
        ("products-changed-value", "products", None, "products"),
        ("products-changed-change", "products_change_pct", None, "%"),
    ]),
    "inventory-cost": ("Inventory cost", [
        ("inventory-cost-tokens-value", "tokens_m", None, "M tokens"),
        ("inventory-cost-tokens-change", "tokens_m_change_pct", None, "%"),
        ("inventory-cost-attention-value", "attention_cost", "attention_cost IS NOT NULL", "@ATTENTION_CURRENCY@"),
        ("inventory-cost-attention-change", "attention_cost_change_pct", None, "%"),
        ("inventory-cost-attention-hours", "attention_hours", None, "hours"),
        ("inventory-cost-attention-requests", "attention_requests", None, "requests"),
        ("inventory-cost-words-per-request", "words_per_request", None, "words"),
    ]),
}
# The queries that the cards of Harold trends read, as they are now.
TRENDS_CARDS = [
    "delivery-lead-time",
    "slicing-and-sizing",
    "test-and-release-time",
    "clarification-requests",
    "work-waiting",
]
# What an empty tile of a card means. The markdown tile of the card says it.
EMPTY_MEANING = {
    "value-created": ["no data yet", "no change figure"],
    "waste": ["no data yet", "no change figure"],
    "tokens-and-turns": ["no data yet", "no change figure", "flow", "harold.turn.tokens.by_spec", "otel.metric.overflow"],
    "products-changed": ["no data yet", "no change figure"],
    "inventory-cost": ["no data yet", "no change figure", "no rate set", "stock", "harold.turn.tokens.by_spec", "otel.metric.overflow"],
}
CAVEAT = re.compile(r"(?i)\b(empty|no data yet|waits? for)\b")
MACRO = re.compile(r"\$__\w+")


def pick(column, sql, where=None):
    """Return the sqlTemplate of a tile that picks one column of the query file."""
    tail = f" WHERE {where}" if where else ""
    return f"SELECT {column} FROM (\n{sql.rstrip()}\n){tail}"


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

    def test_five_cards_and_no_product_filter(self):
        dashboard = self.dashboard()
        self.assertEqual(dashboard["name"], "Harold: the big picture")
        self.assertEqual(dashboard.get("filters", []), [], "the big picture has no product filter")
        ids = [tile["id"] for tile in dashboard["tiles"]]
        self.assertEqual(len(ids), len(set(ids)), "tile ids repeat")
        expected = []
        for card, (_, tiles) in CARDS.items():
            expected += [f"{card}-about"] + [tile[0] for tile in tiles]
        self.assertEqual(sorted(ids), sorted(expected))

    def test_each_tile_picks_one_column_of_its_file(self):
        tiles = self.by_id()
        for card, (_, specs) in CARDS.items():
            for tile_id, column, where, _ in specs:
                with self.subTest(tile=tile_id):
                    config = tiles[tile_id]["config"]
                    self.assertEqual(config["sqlTemplate"], pick(column, self.query_text(card), where))
                    self.assertEqual(config["displayType"], "number")
                    self.assertEqual(config["configType"], "sql")
                    self.assertEqual(config["connectionId"], "@CLICKHOUSE_CONNECTION_ID@")
                    self.assertRegex(config["sourceId"], r"^@[A-Z_]+_SOURCE_ID@$")

    def test_each_column_is_in_its_file(self):
        for card, (_, specs) in CARDS.items():
            sql = strip_comments(self.query_text(card))
            for tile_id, column, _, _ in specs:
                with self.subTest(tile=tile_id):
                    self.assertRegex(sql, rf"(?i)\bAS\s+{column}\b")

    def test_number_format_has_the_unit(self):
        tiles = self.by_id()
        for _, (_, specs) in CARDS.items():
            for tile_id, column, _, unit in specs:
                with self.subTest(tile=tile_id):
                    number = tiles[tile_id]["config"]["numberFormat"]
                    if column.endswith("_change_pct"):
                        # The query returns the percent as a whole number: 100 for +100%.
                        self.assertEqual(number["output"], "percent")
                        self.assertEqual(number["factor"], 0.01)
                        self.assertEqual(number["mantissa"], 0)
                        self.assertNotIn("unit", number)
                    else:
                        self.assertEqual(number["output"], "number")
                        self.assertEqual(number["unit"], unit)
                        self.assertIsInstance(number["mantissa"], int)

    def test_tokens_in_m_have_one_decimal(self):
        tiles = self.by_id()
        for tile_id in ("tokens-and-turns-tokens-value", "inventory-cost-tokens-value"):
            with self.subTest(tile=tile_id):
                self.assertEqual(tiles[tile_id]["config"]["numberFormat"]["mantissa"], 1)

    def test_no_title_carries_a_caveat(self):
        for tile in self.tiles():
            with self.subTest(tile=tile["id"]):
                self.assertTrue(tile.get("name", "").strip())
                self.assertIsNone(CAVEAT.search(tile["name"]), tile["name"])

    def test_each_card_has_its_title_and_what_an_empty_tile_means(self):
        tiles = self.by_id()
        for card, (title, _) in CARDS.items():
            with self.subTest(card=card):
                about = tiles[f"{card}-about"]
                self.assertEqual(about["config"]["displayType"], "markdown")
                text = about["config"]["markdown"]
                self.assertTrue(text.startswith(f"**{title}**"), text[:40])
                for phrase in EMPTY_MEANING[card]:
                    self.assertIn(phrase, text)

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

    def test_change_tile_is_beside_its_value_tile(self):
        tiles = self.by_id()
        for _, (_, specs) in CARDS.items():
            ids = [spec[0] for spec in specs]
            for tile_id, column, _, _ in specs:
                if not column.endswith("_change_pct"):
                    continue
                value_id = tile_id.replace("-change", "-value")
                with self.subTest(tile=tile_id):
                    self.assertIn(value_id, ids)
                    self.assertEqual(tiles[tile_id]["y"], tiles[value_id]["y"])
                    self.assertEqual(tiles[tile_id]["x"], tiles[value_id]["x"] + tiles[value_id]["w"])

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

    def test_change_percent_is_whole_and_safe_for_zero(self):
        for card in CARDS:
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


if __name__ == "__main__":
    unittest.main()
