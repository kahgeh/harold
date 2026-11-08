"""Check that the dashboard "Harold now" and its named query do not drift.

The test reads files as text, with the standard library only. It needs no
ClickHouse, no port and no server.

- hyperdx/now.json holds the cards Agents working and Specs in progress.
- queries/now.sql returns one row with the columns working, all and in_progress.
- The sqlTemplate of a number tile is "SELECT <column> FROM (" + the text of the file + ")".
- There is no change tile: a reading of this moment has no week.
"""

import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
TELEMETRY = ROOT / "docs" / "references" / "telemetry"
DASHBOARD = TELEMETRY / "hyperdx" / "now.json"
QUERY = TELEMETRY / "queries" / "now.sql"
DESCRIPTION = "the agents working and the specs in progress, at this moment"
# (tile id, column, unit)
TILES = [
    ("now-working", "working", "working"),
    ("now-all", "all", "of all"),
    ("now-in-progress", "in_progress", "specs"),
]


def strip_comments(sql):
    return re.sub(r"(?m)--.*$", "", sql)


def pick(column, sql):
    return f"SELECT {column} FROM (\n{sql.rstrip()}\n)"


class NowTest(unittest.TestCase):
    def dashboard(self):
        if not DASHBOARD.is_file():
            self.fail(f"the dashboard file does not exist yet: {DASHBOARD.relative_to(ROOT)}")
        return json.loads(DASHBOARD.read_text(encoding="utf-8"))

    def tiles(self):
        return {tile["id"]: tile for tile in self.dashboard()["tiles"]}

    def test_the_dashboard_has_its_name_and_no_filter(self):
        dashboard = self.dashboard()
        self.assertEqual(dashboard["name"], "Harold now")
        self.assertEqual(dashboard.get("filters", []), [])

    def test_the_cards_and_tiles(self):
        tiles = self.tiles()
        expected = {"now-working-about", "now-in-progress-about"} | {tile[0] for tile in TILES}
        self.assertEqual(sorted(tiles), sorted(expected))
        for tile_id in ("now-working-about", "now-in-progress-about"):
            self.assertEqual(tiles[tile_id]["config"]["displayType"], "markdown")
        self.assertTrue(tiles["now-working-about"]["config"]["markdown"].startswith("**Agents working**"))
        self.assertTrue(tiles["now-in-progress-about"]["config"]["markdown"].startswith("**Specs in progress**"))
        self.assertIn(DESCRIPTION, self.dashboard()["description"])

    def test_each_tile_picks_one_column_of_now_sql(self):
        sql = QUERY.read_text(encoding="utf-8")
        tiles = self.tiles()
        for tile_id, column, unit in TILES:
            with self.subTest(tile=tile_id):
                config = tiles[tile_id]["config"]
                self.assertEqual(config["sqlTemplate"], pick(column, sql))
                self.assertEqual(config["displayType"], "number")
                self.assertEqual(config["configType"], "sql")
                self.assertEqual(config["connectionId"], "@CLICKHOUSE_CONNECTION_ID@")
                self.assertRegex(config["sourceId"], r"^@[A-Z_]+_SOURCE_ID@$")
                self.assertEqual(config["numberFormat"]["unit"], unit)
                self.assertEqual(config["numberFormat"]["output"], "number")
                self.assertEqual(config["numberFormat"]["mantissa"], 0)

    def test_there_is_no_change_tile(self):
        for tile_id, tile in self.tiles().items():
            with self.subTest(tile=tile_id):
                self.assertNotIn("change", tile_id)
                self.assertNotIn("_change_pct", tile["config"].get("sqlTemplate", ""))
                self.assertNotEqual(tile["config"].get("numberFormat", {}).get("output"), "percent")

    def test_now_sql_returns_one_row_of_three_columns(self):
        sql = QUERY.read_text(encoding="utf-8")
        code = strip_comments(sql)
        for column in ("working", "all", "in_progress"):
            self.assertRegex(code, rf"(?i)\bAS\s+`?{column}`?\b")
        self.assertNotRegex(code, r"(?i)\bUNION\b|\bGROUP\s+BY\b|\bARRAY\s+JOIN\b|\bthis_week\b|\blast_week\b")
        self.assertRegex(code, r"(?i)\b(HAVING|LIMIT\s+1)\b", "an empty source must give no row")
        for metric in ("harold.agents", "pod.inventory.items"):
            self.assertIn(f"'{metric}'", code)
        self.assertRegex(code, r"(?i)list\s*=\s*'in_progress'")
        for macro in set(re.findall(r"\$__\w+", sql)):
            self.assertEqual(macro, "$__filters")

    def test_every_tile_has_a_valid_shape(self):
        placed = []
        for tile in self.dashboard()["tiles"]:
            with self.subTest(tile=tile["id"]):
                for key in ("x", "y", "w", "h"):
                    self.assertIsInstance(tile[key], int)
                self.assertLessEqual(tile["x"] + tile["w"], 24)
                self.assertTrue(tile.get("name", "").strip())
                for other in placed:
                    apart = (tile["x"] >= other["x"] + other["w"] or other["x"] >= tile["x"] + tile["w"]
                             or tile["y"] >= other["y"] + other["h"] or other["y"] >= tile["y"] + tile["h"])
                    self.assertTrue(apart, f"{tile['id']} overlaps {other['id']}")
            placed.append(tile)

    def test_no_provider_name(self):
        names = re.compile(r"(?i)claude|anthropic|codex|openai|gemini")
        self.assertIsNone(names.search(QUERY.read_text(encoding="utf-8")))
        if DASHBOARD.is_file():
            self.assertIsNone(names.search(DASHBOARD.read_text(encoding="utf-8")))


if __name__ == "__main__":
    unittest.main()
