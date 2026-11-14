"""Check scripts/ci_test_time.sh and the way the two workflows use it. No network: the tests
feed fixed cargo output to the script."""

import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts' / 'ci_test_time.sh'
MAIN = '     Running unittests src/main.rs (target/aarch64-apple-darwin/debug/deps/harold-70c78f147651be5b)'
OTHER = '     Running tests/get_command.rs (target/aarch64-apple-darwin/debug/deps/get_command-13710829997d689a)'
LIMIT_LINE = 'cargo test --locked --workspace --target aarch64-apple-darwin 2>&1 | scripts/ci_test_time.sh'


def result(seconds):
    return f'test result: ok. 4530 passed; 0 failed; 48 ignored; 0 measured; 0 filtered out; finished in {seconds}s'


def run(lines):
    text = '\n'.join(lines) + '\n'
    done = subprocess.run(['bash', str(SCRIPT)], input=text, capture_output=True, text=True, check=False)
    return text, done


def warnings(output):
    return [line for line in output.splitlines() if line.startswith('::warning::')]


class CiTestTimeTests(unittest.TestCase):
    def test_the_main_binary_above_900_seconds_gives_one_warning_with_the_time(self):
        _, done = run([MAIN, 'running 4578 tests', result('901.2'), OTHER, result('2.6')])
        self.assertEqual(done.returncode, 0)
        found = warnings(done.stdout)
        self.assertEqual(len(found), 1, done.stdout)
        self.assertIn('901.2', found[0])

    def test_the_main_binary_at_899_seconds_gives_no_warning(self):
        _, done = run([MAIN, result('899')])
        self.assertEqual(warnings(done.stdout), [])

    def test_another_binary_above_900_seconds_gives_no_warning(self):
        _, done = run([MAIN, result('600.0'), OTHER, result('2000')])
        self.assertEqual(warnings(done.stdout), [])

    def test_the_main_binary_must_come_before_the_line_that_is_read(self):
        _, done = run([OTHER, result('2000'), MAIN, result('10.5')])
        self.assertEqual(warnings(done.stdout), [])

    def test_output_with_no_result_line_gives_no_warning_and_exit_0(self):
        _, done = run(['Compiling harold', MAIN, 'running 1 test'])
        self.assertEqual(warnings(done.stdout), [])
        self.assertEqual(done.returncode, 0)

    def test_the_colour_codes_of_cargo_do_not_hide_the_lines(self):
        _, done = run(['\x1b[1m\x1b[32m' + MAIN.strip() + '\x1b[0m', result('950').replace('ok.', '\x1b[32mok\x1b[0m.')])
        self.assertEqual(len(warnings(done.stdout)), 1, done.stdout)

    def test_the_output_equals_the_input_apart_from_the_warning(self):
        text, done = run(['first', MAIN, result('1000.0'), 'last'])
        kept = [line for line in done.stdout.splitlines() if not line.startswith('::warning::')]
        self.assertEqual('\n'.join(kept) + '\n', text)

    def test_the_output_equals_the_input_when_there_is_no_warning(self):
        text, done = run(['first', MAIN, result('5.0'), 'last'])
        self.assertEqual(done.stdout, text)

    def test_a_failing_command_piped_through_the_script_still_fails_with_pipefail(self):
        done = subprocess.run(
            ['bash', '--noprofile', '--norc', '-eo', 'pipefail', '-c', f'echo before; false | {SCRIPT}'],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(done.returncode, 0)


class WorkflowTests(unittest.TestCase):
    def step(self, name):
        text = (ROOT / '.github' / 'workflows' / name).read_text()
        start = text.index('      - name: Test\n')
        end = text.find('\n      - name:', start + 1)
        return text[start:end if end != -1 else len(text)]

    def test_the_macos_test_step_pipes_cargo_test_through_the_script_under_bash(self):
        step = self.step('macos-checks.yml')
        self.assertIn('        shell: bash\n', step)
        self.assertIn('          ' + LIMIT_LINE + '\n', step)
        self.assertIn('        timeout-minutes: 40\n', step)

    def test_the_linux_test_step_pipes_cargo_test_through_the_script_under_bash(self):
        step = self.step('linux-tests.yml')
        self.assertIn('        shell: bash\n', step)
        self.assertIn(
            '          cargo test --locked --workspace --no-default-features --features harold/linux-defaults'
            ' 2>&1 | scripts/ci_test_time.sh\n',
            step,
        )


if __name__ == '__main__':
    unittest.main()
