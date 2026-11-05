"""Check that the big picture dashboard and its named queries do not drift.

The test reads files as text, with the standard library only. It needs no
ClickHouse, no port and no server.

The files:
- hyperdx/big-picture.json holds ten cards. Each card has two tiles.
- queries/<card>.sql holds the SQL of the card.

The rule that links a tile to its file:
- The id of a tile is "<card>-this-week" or "<card>-last-week".
- The two tiles of one card read the same file, queries/<card>.sql.
  This week is the column this_week. Last week is the column last_week.
- The sqlTemplate of a tile equals the text of that file, byte for byte.

The rule for notes: the first tile of each card has a non-empty "note" field.
A card that waits for a spec names that spec in its note and keeps the
text "no data yet".

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
SUFFIXES = ("this-week", "last-week")
TILE_ID = re.compile(r"^(?P<card>[a-z0-9]+(?:-[a-z0-9]+)*)-(?P<week>this-week|last-week)$")
# $__filters is the only HyperDX macro that a query may use.
MACRO = re.compile(r"\$__\w+")


class BigPictureTest(unittest.TestCase):
    def tiles(self):
        if not DASHBOARD.is_file():
            self.fail(f"the dashboard file does not exist yet: {DASHBOARD.relative_to(ROOT)}")
        return json.loads(DASHBOARD.read_text(encoding="utf-8"))["tiles"]

    def card_of(self, tile):
        match = TILE_ID.match(tile.get("id", ""))
        if not match:
            self.fail(f"tile id is not <card>-this-week or <card>-last-week: {tile.get('id')!r}")
        return match.group("card")

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

    def test_ten_cards_two_tiles_each(self):
        tiles = self.tiles()
        ids = [tile.get("id") for tile in tiles]
        self.assertEqual(len(ids), len(set(ids)), "tile ids repeat")
        expected = sorted(f"{card}-{week}" for card in CARDS for week in SUFFIXES)
        self.assertEqual(sorted(ids), expected)

    def test_empty_window_guard(self):
        # Offline there is no ClickHouse. The test checks the SQL text.
        # An aggregate with no GROUP BY returns one row of 0 for an empty window.
        # A GROUP BY returns no row, and the card then shows "no data yet".
        self.tiles()
        for card in CARDS:
            with self.subTest(card=card):
                sql = self.query_text(card)
                self.assertRegex(sql, r"(?i)\bGROUP\s+BY\b", "no GROUP BY: an empty window returns a row of 0")
                self.assertRegex(sql, r"(?i)\bthis_week\b")
                self.assertRegex(sql, r"(?i)\blast_week\b")
                for macro in set(MACRO.findall(sql)):
                    self.assertEqual(macro, "$__filters", "only $__filters is allowed")

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

    def test_first_tile_of_each_card_has_a_note(self):
        firsts = {}
        for tile in self.tiles():
            firsts.setdefault(self.card_of(tile), tile)
        for card in CARDS:
            with self.subTest(card=card):
                tile = firsts.get(card)
                self.assertIsNotNone(tile, "card has no tile")
                self.assertTrue(str(tile.get("note", "")).strip(), "first tile has no note")


if __name__ == "__main__":
    unittest.main()
