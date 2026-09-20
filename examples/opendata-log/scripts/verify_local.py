#!/usr/bin/env python3
"""Build and exercise the complete local lesson path; never contacts AWS."""
import hashlib
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def main():
    crate = Path(__file__).resolve().parents[1]
    output = crate / "results" / f"local-{time.time_ns()}"
    output.mkdir(parents=True)
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("OPENDATA_", "BENCH_", "SLATEDB_", "AWS_"))}
    checks = []
    failure = None

    def run(name, command, overrides=None, expected_error=None, limit=180):
        print(f"Checking {name}...", flush=True)
        started = time.monotonic()
        # A fresh process group contains this check and any writer children it owns.
        # If the outer deadline expires, leave no stopped writer or wrapper behind.
        child = subprocess.Popen(command, cwd=crate, env={**environment, **(overrides or {})},
                                 text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        expired = False
        try:
            transcript, _ = child.communicate(timeout=limit)
        except subprocess.TimeoutExpired:
            expired = True
            os.killpg(child.pid, signal.SIGKILL)
            transcript, _ = child.communicate()
        except BaseException:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate()
            raise
        (output / f"{name}.log").write_text(transcript)
        passed = not expired and (child.returncode == 0 if expected_error is None else
                                 child.returncode != 0 and expected_error in transcript)
        checks.append({"name": name, "passed": passed, "exit": child.returncode,
                       "timed_out": expired, "seconds": time.monotonic() - started})
        if not passed:
            raise RuntimeError(f"{name} failed; see {output / (name + '.log')}")

    try:
        run("format", ["cargo", "fmt", "--check"])
        run("tests", ["cargo", "test", "--locked", "--offline", "--all-targets"])
        run("clippy", ["cargo", "clippy", "--locked", "--offline", "--all-targets", "--", "-D", "warnings"])
        run("build", ["cargo", "build", "--locked", "--offline", "--bins", "--examples"])
        metadata = subprocess.run(["cargo", "metadata", "--offline", "--no-deps", "--format-version=1"],
                                  cwd=crate, env=environment, text=True, capture_output=True, check=True)
        binary = Path(json.loads(metadata.stdout)["target_directory"]) / "debug"
        with tempfile.TemporaryDirectory(prefix="opendata-lessons-") as temporary:
            environment.update(OPENDATA_BACKEND="local", OPENDATA_LOCAL_DIR=temporary,
                               OPENDATA_CHECKPOINT=str(Path(temporary) / "monitor.json"))
            run("lesson-1", [str(binary / "opendata-agent-progress")])
            for lesson in [2, 3, 4, 6, 7]:
                run(f"lesson-{lesson}", [str(binary / "examples" / f"lesson_{lesson}")])
            run("lesson-4-resume", [str(binary / "examples/lesson_4")])
            for lesson, backend in [(5, "s3"), (8, "express")]:
                run(f"lesson-{lesson}-requires-cloud", [str(binary / "examples" / f"lesson_{lesson}")],
                    expected_error=f"OPENDATA_BACKEND={backend}")
            run("cloud-endpoint-guard", [str(binary / "examples/lesson_5")],
                {"OPENDATA_BACKEND": "s3", "AWS_ENDPOINT": "https://example.invalid"},
                expected_error="Unset AWS_ENDPOINT")
            run("lesson-9", [sys.executable, "scripts/reliability.py", "--binary", str(binary / "examples/lesson_9"), "--output", str(output / "reliability")])
            run("lesson-10", [sys.executable, "scripts/benchmark.py", "--binary", str(binary / "examples/lesson_10"),
                              "--output", str(output / "resources.json")],
                {"BENCH_OUTPUT": str(output / "benchmark.csv")})
            run("lesson-10-bounded-input", [str(binary / "examples/lesson_10")],
                {"BENCH_EVENTS": "0"}, expected_error="events")
    except Exception as error:
        failure = str(error)
        raise
    finally:
        (output / "summary.json").write_text(json.dumps({
            "cargo_lock_sha256": hashlib.sha256((crate / "Cargo.lock").read_bytes()).hexdigest(),
            "checks": checks, "cloud_executed": False, "failure": failure,
            "source_sha256": {str(path.relative_to(crate)): hashlib.sha256(path.read_bytes()).hexdigest()
                              for pattern in ("src/*.rs", "examples/*.rs", "scripts/*.py", "Cargo.toml")
                              for path in sorted(crate.glob(pattern))},
        }, indent=2))
        print(f"Evidence: {output}", flush=True)
    print(f"Passed {len(checks)} local checks. Real S3/Express trials were not run.")


if __name__ == "__main__":
    main()
