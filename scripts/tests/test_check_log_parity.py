import os
import re
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "check_log_parity.sh"

# The fake logq reads the SQL on stdin. It returns 1 row for the message "was resent", else 0 rows.
FAKE_LOGQ = """#!/usr/bin/env bash
sql=$(cat)
case "$sql" in
*"was resent"*) echo 1 ;;
*) echo 0 ;;
esac
"""

LOG = "\n".join([
    # allowed: a health line
    "2026-11-07T09:30:00.000000Z  WARN harold::telemetry_health: telemetry export failing (collector unreachable at http://h:4317)",
    # allowed: a telemetry crate line, with ANSI codes
    "\x1b[2m2026-11-07T09:31:00.000000Z\x1b[0m \x1b[33m WARN\x1b[0m opentelemetry_sdk: BatchLogProcessor.ExportError secret=alpha",
    # allowed: no timestamp at the start
    "thread 'main' panicked at src/main.rs:1:1: secret=beta",
    # unsent, resent
    '2026-11-07T09:32:00.500000Z  INFO harold::tools::watch: was resent pane_id="%7" secret=gamma unsent=true',
    # unsent, not resent
    "2026-11-07T09:33:00.000000Z  INFO harold::tools::watch: was lost secret=delta unsent=true",
    # gap
    "2026-11-07T09:34:00.000000Z  INFO harold::tools::watch: gap message secret=epsilon",
    # gap too old: outside the window
    "2026-11-06T01:00:00.000000Z  INFO harold::tools::watch: old gap secret=zeta",
    "",
])


def run(log_text, extra_env=None, logq_text=FAKE_LOGQ):
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        log = tmp / "harold.log"
        log.write_text(log_text)
        logq = tmp / "logq"
        logq.write_text(logq_text)
        logq.chmod(0o755)
        env = dict(os.environ, HAROLD_LOG=str(log), LOGQ=str(logq),
                   NOW="2026-11-07T10:00:00Z")
        env.update(extra_env or {})
        return subprocess.run(["bash", str(SCRIPT), "2"], capture_output=True,
                              text=True, env=env)

# The five sample messages for the cut. They are kept here, in one place. Each pair is an
# input and what `cut_message` (harold/src/telemetry.rs) gives for it. A test below runs
# the script on each input, and a test below checks that harold/src/telemetry_tests.rs
# holds the same pair, so the script and `cut_message` cannot drift apart.
CUT_SAMPLES = [
    ("failed to open /Users/me/bin/harold/data/x.db: permission denied",
     "failed to open <cut> permission denied"),
    ("GET https://h.example/a?b failed", "GET <cut> failed"),
    ("mail me@x.com now", "mail <cut> now"),
    ("token=abc12 bad", "<cut> bad"),
    (r"open C:\temp\x", "open <cut>"),
]

# A fake logq that keeps the SQL it reads in the file named by SQL_FILE.
RECORDING_LOGQ = """#!/usr/bin/env bash
cat > "$SQL_FILE"
echo 0
"""


def sql_for(line, target):
    """The SQL that the script sends for one unsent line of `target`."""
    with tempfile.TemporaryDirectory() as tmp:
        sql_file = Path(tmp) / "sql"
        text = f"2026-11-07T09:33:00.000000Z  WARN {target}: {line} unsent=true\n"
        run(text, extra_env={"SQL_FILE": str(sql_file)}, logq_text=RECORDING_LOGQ)
        return sql_file.read_text()


class CheckLogParityTest(unittest.TestCase):
    def test_counts_and_lines(self):
        result = run(LOG)
        out = result.stdout
        self.assertIn("allowed: 3", out)
        self.assertIn("unsent resent: 1", out)
        self.assertIn("unsent not resent: 1", out)
        self.assertIn("gap: 1", out)
        self.assertIn("not resent: 2026-11-07T09:33:00.000000Z was lost", out)
        self.assertIn("gap: 2026-11-07T09:34:00.000000Z gap message", out)
        self.assertNotIn("old gap", out)

    def test_exit_one_on_gap(self):
        self.assertEqual(run(LOG).returncode, 1)

    def test_exit_zero_without_gap(self):
        no_gap = "\n".join(l for l in LOG.split("\n") if "gap message" not in l)
        result = run(no_gap)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("gap: 0", result.stdout)

    def test_no_field_value_printed(self):
        result = run(LOG)
        text = result.stdout + result.stderr
        for value in ["alpha", "beta", "gamma", "delta", "epsilon", "%7", "secret="]:
            self.assertNotIn(value, text)

    def test_quote_in_message_does_not_break_the_query(self):
        line = "2026-11-07T09:33:00.000000Z  INFO harold::x: it's a \\ test unsent=true\n"
        result = run(line)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("unsent not resent: 1", result.stdout)

    def test_telemetry_crate_rule_matches_the_rust_rule(self):
        def line(target):
            return f"2026-11-07T09:34:00.000000Z  WARN {target}: some message k=v\n"
        for target in ["tonic_prost::x", "opentelemetry_sdk", "opentelemetry-otlp", "hyper_util::client", "http"]:
            self.assertIn("gap: 0", run(line(target)).stdout, target)
        for target in ["httpx", "opentelemetryx::a", "tonic2"]:
            result = run(line(target))
            self.assertIn("gap: 1", result.stdout, target)
            self.assertEqual(result.returncode, 1)

    def test_span_prefix_is_accepted(self):
        line = "2026-11-07T09:34:00.000000Z  WARN run{id=1}: harold::x: spanned message k=v unsent=true\n"
        self.assertIn("unsent not resent: 1", run(line).stdout)

    def test_long_message_is_cut(self):
        line = "2026-11-07T09:34:00.000000Z  INFO harold::x: " + "a" * 300 + " k=v\n"
        self.assertIn("gap: 2026-11-07T09:34:00.000000Z " + "a" * 200 + "\n", run(line).stdout)

    def test_long_unsent_message_is_looked_up_in_full(self):
        message = "b" * 300
        line = f"2026-11-07T09:34:00.000000Z  INFO harold::x: {message} k=v unsent=true\n"
        fake = f"#!/usr/bin/env bash\nsql=$(cat)\ncase \"$sql\" in\n*'{message}'*) echo 1 ;;\n*) echo 0 ;;\nesac\n"
        result = run(line, logq_text=fake)
        self.assertIn("unsent resent: 1", result.stdout)
        self.assertNotIn("b" * 201, result.stdout)
        # printed form is capped on a not-resent line too
        other = run(line).stdout
        self.assertIn("not resent: 2026-11-07T09:34:00.000000Z " + "b" * 200 + "\n", other)
        self.assertNotIn("b" * 201, other)

    def test_another_crates_message_is_cut_before_the_lookup(self):
        for message, cut in CUT_SAMPLES:
            sql = sql_for(message, "slatedb::db")
            self.assertIn("Body = '" + cut + "'", sql, message)
        # A resent line of another crate is found when ClickHouse holds the cut message.
        text = "2026-11-07T09:32:00.500000Z  WARN turso::x: opened /a/b file unsent=true\n"
        fake = "#!/usr/bin/env bash\nsql=$(cat)\ncase \"$sql\" in\n*\"'opened <cut> file'\"*) echo 1 ;;\n*) echo 0 ;;\nesac\n"
        result = run(text, logq_text=fake)
        self.assertIn("unsent resent: 1", result.stdout)
        self.assertIn("unsent not resent: 0", result.stdout)

    def test_the_cut_is_at_most_200_characters_and_joins_words_with_one_space(self):
        sql = sql_for("a    b " + "w" * 300, "slatedb")
        self.assertIn("Body = 'a b " + "w" * 196 + "'", sql)
        self.assertNotIn("w" * 197, sql)

    def test_harolds_own_targets_are_looked_up_uncut(self):
        for target in ["harold", "harold::tools::watch", "events", "events::x", "buckwal", "buckwal::wal"]:
            sql = sql_for("open /a/b now", target)
            self.assertIn("Body = 'open /a/b now'", sql, target)
        # A crate whose name only starts with the same letters is another crate's.
        self.assertIn("Body = 'open <cut> now'", sql_for("open /a/b now", "harolder::x"))

    def test_the_sample_pairs_are_the_ones_cut_message_is_tested_with(self):
        source = (REPO / "harold" / "src" / "telemetry_tests.rs").read_text()
        for message, cut in CUT_SAMPLES:
            pattern = r'cut_message\(\s*r?"' + re.escape(message) + r'"\s*\),\s*"' + re.escape(cut) + '"'
            self.assertRegex(source, pattern, message)

    def test_failing_logq_exits_two(self):
        result = run(LOG, logq_text="#!/usr/bin/env bash\ncat >/dev/null\necho boom >&2\nexit 1\n")
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("Traceback", result.stderr)

    def test_junk_logq_exits_two(self):
        for body in ["echo 'Code: 60. DB::Exception'", "echo 'x 1'", "true", "echo -1"]:
            result = run(LOG, logq_text="#!/usr/bin/env bash\ncat >/dev/null\n" + body + "\n")
            self.assertEqual(result.returncode, 2, body)
            self.assertNotIn("Traceback", result.stderr)

    def test_missing_logq_exits_two(self):
        result = run(LOG, extra_env={"LOGQ": "/nonexistent/logq"})
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("Traceback", result.stderr)

    def test_bad_line_exits_two(self):
        line = "2026-13-45T09:34:00.000000Z  INFO harold::x: m k=v\n"
        result = run(line)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("Traceback", result.stderr)


if __name__ == "__main__":
    unittest.main()
