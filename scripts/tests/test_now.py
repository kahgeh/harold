"""Check that the dashboard "Harold now" and its named query do not drift.

The structure tests read files as text, with the standard library only. They need no
ClickHouse, no port and no server.

- hyperdx/now.json holds an intro text and two cards, Agents working and Specs in progress.
  Each card is ONE table tile with ONE text line that ends with the time of the reading.
- queries/now.sql returns one row with the columns working, all, in_progress and read_at.
  It reads the last 2 minutes. With no export in 2 minutes it returns no row.
- The sqlTemplate of a card tile is wrap(card, the text of the file).
- There is no change: a reading of this moment has no week.

The line tests send the wrap with fake rows (constants only, SELECT only) to the read-only
ClickHouse through bin/logq. They skip when bin/logq or ClickHouse is not there, as in CI.
"""

import json
from pathlib import Path
import re
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_big_picture import find_logq  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
TELEMETRY = ROOT / "docs" / "references" / "telemetry"
DASHBOARD = TELEMETRY / "hyperdx" / "now.json"
QUERY = TELEMETRY / "queries" / "now.sql"
ZONE = "Australia/Melbourne"
INTRO = (
    "The agents working and the specs in progress, at this moment. All products. "
    "Each line ends with the time of the reading in the local time zone. "
    "A reading of this moment has no change figure. "
    "\"No data found within time range.\" means no reading in the last 2 minutes."
)
# card -> (title, the column that must not be NULL, the line)
CARDS = {
    "now-working": ("Agents working", "working",
                    "toString(working), ' working of ', toString(`all`), if(`all` = 1, ' agent', ' agents')"),
    "now-in-progress": ("Specs in progress", "in_progress",
                        "toString(in_progress), if(in_progress = 1, ' spec in progress', ' specs in progress')"),
}


def strip_comments(sql):
    return re.sub(r"(?m)--.*$", "", sql)


def reading_time():
    """The end of the line: the time of the reading in the local zone, with the zone name."""
    local = f"toTimeZone(read_at, '{ZONE}')"
    return (f"'  at ', formatDateTime({local}, '%H:%i'), ' ', "
            f"multiIf(timeZoneOffset({local}) = 39600, 'AEDT', timeZoneOffset({local}) = 36000, 'AEST', 'UTC')")


def wrap(card, source):
    title, column, line = CARDS[card]
    return (f"SELECT concat({line}, {reading_time()}) AS \"{title}\"\n"
            f"FROM (\n{source.rstrip()}\n)\nWHERE {column} IS NOT NULL")


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
        self.assertEqual(list(tiles), ["now-intro"] + list(CARDS))
        self.assertEqual(tiles["now-intro"]["config"]["displayType"], "markdown")
        self.assertEqual(tiles["now-intro"]["config"]["markdown"], INTRO)
        self.assertEqual([t["config"]["displayType"] for t in tiles.values() if t["id"] != "now-intro"], ["table"] * 2)

    def test_each_card_is_one_table_tile_that_wraps_now_sql(self):
        sql = QUERY.read_text(encoding="utf-8")
        tiles = self.tiles()
        for card, (title, column, _) in CARDS.items():
            with self.subTest(card=card):
                config = tiles[card]["config"]
                self.assertEqual(tiles[card]["name"], title)
                self.assertEqual(config["sqlTemplate"], wrap(card, sql))
                self.assertIn(f'AS "{title}"', config["sqlTemplate"])
                self.assertTrue(config["sqlTemplate"].endswith(f"WHERE {column} IS NOT NULL"))
                self.assertEqual(config["configType"], "sql")
                self.assertEqual(config["connectionId"], "@CLICKHOUSE_CONNECTION_ID@")
                self.assertRegex(config["sourceId"], r"^@[A-Z_]+_SOURCE_ID@$")
                self.assertNotIn("numberFormat", config)

    def test_there_is_no_change(self):
        for tile_id, tile in self.tiles().items():
            with self.subTest(tile=tile_id):
                self.assertNotIn("change", tile_id)
                self.assertNotIn("_change_pct", tile["config"].get("sqlTemplate", ""))

    def test_now_sql_returns_one_row_of_four_columns_from_the_last_2_minutes(self):
        sql = QUERY.read_text(encoding="utf-8")
        code = strip_comments(sql)
        for column in ("working", "all", "in_progress", "read_at"):
            self.assertRegex(code, rf"(?i)\bAS\s+`?{column}`?\b")
        self.assertNotRegex(code, r"(?i)\bUNION\b|\bGROUP\s+BY\b|\bARRAY\s+JOIN\b|\bthis_week\b|\blast_week\b")
        self.assertRegex(code, r"(?i)\b(HAVING|LIMIT\s+1)\b", "an empty source must give no row")
        for metric in ("harold.agents", "pod.inventory.items"):
            self.assertIn(f"'{metric}'", code)
        self.assertRegex(code, r"(?i)list'\]\s*=\s*'in_progress'")
        self.assertEqual(code.count("INTERVAL 2 MINUTE"), 2, "each source reads the last 2 minutes")
        self.assertNotRegex(code, r"(?i)INTERVAL\s+\d+\s+(DAY|HOUR)", "a reading older than 2 minutes is not shown")
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


class LineTest(unittest.TestCase):
    """The wrap on fake rows. It runs the SQL (SELECT of constants only) through bin/logq."""

    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the line tests need the read-only ClickHouse")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the line tests are skipped")

    def line(self, card, working, all_agents, in_progress, read_at="2026-11-08 07:05:00"):
        def number(value):
            return f"CAST({'NULL' if value is None else value} AS Nullable(Float64))"
        source = (f"SELECT {number(working)} AS working, {number(all_agents)} AS `all`, "
                  f"{number(in_progress)} AS in_progress, toDateTime64('{read_at}', 3, 'UTC') AS read_at")
        done = subprocess.run([self.logq], input=wrap(card, source), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return done.stdout.rstrip("\n")

    def test_the_lines_end_with_the_time_and_the_zone_name(self):
        self.assertEqual(self.line("now-working", 2, 11, 4), "2 working of 11 agents  at 18:05 AEDT")
        self.assertEqual(self.line("now-in-progress", 2, 11, 4), "4 specs in progress  at 18:05 AEDT")
        self.assertEqual(self.line("now-in-progress", 2, 11, 4, "2026-07-01 07:05:00"),
                         "4 specs in progress  at 17:05 AEST")

    def test_a_count_of_1_takes_the_singular(self):
        self.assertEqual(self.line("now-working", 1, 1, 1), "1 working of 1 agent  at 18:05 AEDT")
        self.assertEqual(self.line("now-in-progress", 1, 1, 1), "1 spec in progress  at 18:05 AEDT")
        self.assertEqual(self.line("now-in-progress", 0, 3, 0), "0 specs in progress  at 18:05 AEDT")

    def test_a_null_count_gives_no_row(self):
        self.assertEqual(self.line("now-working", None, None, 4), "")
        self.assertEqual(self.line("now-in-progress", 2, 11, None), "")


if __name__ == "__main__":
    unittest.main()
