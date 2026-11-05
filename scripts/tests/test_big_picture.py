"""Check that the big picture dashboard and its named queries do not drift.

The test reads files as text, with the standard library only. It needs no
ClickHouse, no port and no server.

The files:
- hyperdx/big-picture.json holds ten cards. Each card has one tile.
- queries/<card>.sql holds the SQL of the card.

The rule that links a tile to its file:
- The id of a tile is the name of its card, "<card>".
- The tile reads the file queries/<card>.sql. The table of the tile has two columns:
  this_week and last_week.
- The sqlTemplate of a tile equals the text of that file, byte for byte.

The rule for notes: each query file has a "-- Note:" line and a "-- Waits for:" line.
A file that waits for a spec has a "-- Empty: [<part>: ]no data yet (waits for <spec>)" line too.
The name of its tile ends with "(empty: no data yet, waits for <spec>)", or with
"(<part>: no data yet, waits for <spec>)" when only the part waits.

The files agent-counter.sql and README.md are not cards. The test ignores them.
"""

import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
TELEMETRY = ROOT / "docs" / "references" / "telemetry"
DASHBOARD = TELEMETRY / "hyperdx" / "big-picture.json"
QUERIES = TELEMETRY / "queries"
NOT_CARDS = {"agent-counter.sql"}
CARDS = [
    "value-created",
    "delivery-lead-time",
    "work-waiting",
    "waste",
    "tokens-and-turns",
    "slicing-and-sizing",
    "test-and-release-time",
    "products-changed",
    "now",
    "clarification-requests",
]
TILE_ID = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
# $__filters is the only HyperDX macro that a query may use.
MACRO = re.compile(r"\$__\w+")


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
    def tiles(self):
        if not DASHBOARD.is_file():
            self.fail(f"the dashboard file does not exist yet: {DASHBOARD.relative_to(ROOT)}")
        return json.loads(DASHBOARD.read_text(encoding="utf-8"))["tiles"]

    def card_of(self, tile):
        if not TILE_ID.match(tile.get("id", "")):
            self.fail(f"tile id is not a card name: {tile.get('id')!r}")
        return tile["id"]

    def query_text(self, card):
        path = QUERIES / f"{card}.sql"
        if not path.is_file():
            self.fail(f"card {card} has no query file: {path.relative_to(ROOT)}")
        return path.read_text(encoding="utf-8")

    def test_every_card_sql_equals_its_file(self):
        for tile in self.tiles():
            card = self.card_of(tile)
            with self.subTest(tile=tile["id"]):
                self.assertEqual(tile["config"]["sqlTemplate"], self.query_text(card))

    def test_every_file_has_a_card(self):
        self.tiles()
        cards = {self.card_of(tile) for tile in self.tiles()}
        files = {path.stem for path in QUERIES.glob("*.sql") if path.name not in NOT_CARDS}
        self.assertEqual(sorted(files - cards), [], "query files with no card")
        self.assertEqual(sorted(cards - files), [], "cards with no query file")

    def test_ten_cards_one_tile_each(self):
        tiles = self.tiles()
        ids = [tile.get("id") for tile in tiles]
        self.assertEqual(len(ids), len(set(ids)), "tile ids repeat")
        self.assertEqual(sorted(ids), sorted(CARDS))

    def test_tile_name_shows_that_the_card_waits_for_a_spec(self):
        # The name names only the part that waits. A card that waits as a whole is "empty".
        for tile in self.tiles():
            card = self.card_of(tile)
            text = self.query_text(card)
            waits = re.search(r"(?m)^-- Waits for: (\S+)\s*$", text)
            with self.subTest(tile=card):
                self.assertTrue(waits, "no '-- Waits for:' line")
                if waits.group(1) == "nothing":
                    self.assertNotIn("no data yet", tile["name"])
                    continue
                empty = re.search(r"(?m)^-- Empty: (?:(.+?): )?no data yet \(waits for (\S+)\)\s*$", text)
                self.assertTrue(empty, "no '-- Empty:' line")
                self.assertEqual(empty.group(2), waits.group(1))
                part = empty.group(1) or "empty"
                self.assertTrue(
                    tile["name"].endswith(f"({part}: no data yet, waits for {waits.group(1)})"),
                    tile["name"],
                )

    def test_empty_window_guard(self):
        # Offline there is no ClickHouse. The test checks the SQL text.
        # An aggregate with no GROUP BY returns one row of 0 for an empty window.
        # A GROUP BY returns no row, and the card then shows "no data yet".
        # The guard must stand in the outermost SELECT: an inner GROUP BY does not help,
        # because the outer SELECT would still return one row.
        for card in CARDS:
            with self.subTest(card=card):
                sql = self.query_text(card)
                final = final_statement(sql)
                self.assertRegex(depth_zero(final), r"(?i)\b(GROUP\s+BY|HAVING)\b",
                                 "the outermost SELECT has no GROUP BY or HAVING")
                self.assertRegex(sql, r"(?i)\bthis_week\b")
                self.assertRegex(sql, r"(?i)\blast_week\b")
                for macro in set(MACRO.findall(sql)):
                    self.assertEqual(macro, "$__filters", "only $__filters is allowed")

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

    def test_tiles_valid_shape(self):
        for tile in self.tiles():
            with self.subTest(tile=tile.get("id")):
                self.card_of(tile)
                config = tile["config"]
                self.assertEqual(config["configType"], "sql")
                self.assertEqual(config["connectionId"], "@CLICKHOUSE_CONNECTION_ID@")
                self.assertRegex(config["sourceId"], r"^@[A-Z_]+_SOURCE_ID@$")
                for key in ("x", "y", "w", "h"):
                    self.assertIsInstance(tile[key], int)
                self.assertTrue(tile.get("name", "").strip())

    def test_every_query_file_has_a_note_and_a_wait_line(self):
        files = [path for path in QUERIES.glob("*.sql") if path.name not in NOT_CARDS]
        self.assertTrue(files, "no query files")
        for path in sorted(files):
            text = path.read_text(encoding="utf-8")
            with self.subTest(file=path.name):
                self.assertRegex(text, r"(?m)^-- Note: \S", "no '-- Note:' line")
                self.assertRegex(text, r"(?m)^-- Waits for: \S", "no '-- Waits for:' line")
                if not re.search(r"(?m)^-- Waits for: nothing\s*$", text):
                    self.assertRegex(text, r"(?m)^-- Empty: (.+?: )?no data yet \(waits for \S", "no '-- Empty:' line")


if __name__ == "__main__":
    unittest.main()
