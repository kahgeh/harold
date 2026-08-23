import json
import unittest
from unittest.mock import patch

from . import harold_turn_complete


class HaroldTurnCompleteTest(unittest.TestCase):
    def test_clean_text_removes_controls_collapses_whitespace_and_bounds_output(self) -> None:
        self.assertEqual(
            harold_turn_complete.clean_text("  refresh\x1b[31m\n\tevents  ", 14),
            "refresh events",
        )
        self.assertEqual(
            harold_turn_complete.clean_text("  Résumé\u2003task  ", 160),
            "Résumé task",
        )
        self.assertEqual(
            harold_turn_complete.clean_text(
                "before \x1b]0;private title\x07 after", 160
            ),
            "before after",
        )
        self.assertEqual(
            harold_turn_complete.clean_text("before \u009b31mred after", 160),
            "before red after",
        )
        self.assertEqual(
            harold_turn_complete.clean_text("before\u009dprivate\u009c after", 160),
            "before after",
        )
        self.assertEqual(harold_turn_complete.clean_text("🦀🦀🦀", 2), "🦀🦀")

    @patch.object(harold_turn_complete.subprocess, "run")
    def test_call_harold_preserves_the_wire_compatible_payload(self, run) -> None:
        harold_turn_complete.call_harold(
            pane_id="%8",
            pane_label="harold:2.3",
            last_user_prompt="refresh events",
            assistant_message="events refreshed",
            main_context="harold",
        )

        command = run.call_args.args[0]
        payload = json.loads(command[command.index("-d") + 1])
        self.assertEqual(
            payload,
            {
                "pane_id": "%8",
                "pane_label": "harold:2.3",
                "last_user_prompt": "refresh events",
                "assistant_message": "events refreshed",
                "main_context": "harold",
            },
        )
        self.assertNotIn("work_summary", payload)


if __name__ == "__main__":
    unittest.main()
