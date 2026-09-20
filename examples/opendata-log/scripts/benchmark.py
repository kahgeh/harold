#!/usr/bin/env python3
"""Run one prebuilt lesson-10 child, retaining its output and process resources."""
import argparse
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import time


def main():
    crate = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=crate / "target/release/examples/lesson_10")
    parser.add_argument("--output", type=Path, required=True, help="New resource JSON file")
    parser.add_argument("--timeout", type=float, default=600, help="Child wall-time limit in seconds (0 < seconds <= 3600)")
    args = parser.parse_args()
    if not 0 < args.timeout <= 3600:
        parser.error("--timeout must be positive and at most 3600 seconds")
    if sys.platform not in ("darwin", "linux"):
        parser.error("RSS units are supported only on macOS and Linux")
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("--binary must be an existing executable; build it first")
    output = args.output.resolve()
    log = output.with_suffix(".stdout.txt")
    csv = Path(os.environ.get("BENCH_OUTPUT", output.with_suffix(".csv"))).resolve()
    if len({output, log, csv, csv.with_suffix(".metadata.json")}) != 4:
        parser.error("resource, log, CSV and metadata paths must be distinct")
    if csv.exists() or csv.with_suffix(".metadata.json").exists():
        parser.error("CSV or benchmark metadata already exists; choose fresh output paths")
    output.parent.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["BENCH_OUTPUT"] = str(csv)
    # This wrapper starts exactly one child: RUSAGE_CHILDREN's peak is that process's peak.
    # Files use exclusive creation; the benchmark independently reserves its CSV.
    with output.open("x") as resources, log.open("x") as transcript:
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.monotonic()
        child = None
        failure = None
        try:
            child = subprocess.Popen([str(binary)], cwd=crate, env=env, stdout=transcript, stderr=subprocess.STDOUT)
            exit_code = child.wait(timeout=args.timeout)
        except subprocess.TimeoutExpired:
            failure = "wrapper wall-time limit exceeded; child killed; raw samples may be incomplete"
            exit_code = 124
        except KeyboardInterrupt:
            failure = "wrapper interrupted; child killed; raw samples may be incomplete"
            exit_code = 130
        except OSError as error:
            failure = str(error)
            exit_code = 127
        finally:
            if child is not None and child.poll() is None:
                child.kill()  # Only the Popen-owned child, never a process group or global search.
                child.wait()
        elapsed = time.monotonic() - start
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        report = {
            "binary": str(binary), "csv": str(csv), "transcript": str(log),
            "exit_code": exit_code, "failure": failure, "elapsed_seconds": elapsed,
            "child_cpu_user_seconds": after.ru_utime - before.ru_utime,
            "child_cpu_system_seconds": after.ru_stime - before.ru_stime,
            "child_peak_rss_bytes": after.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
            "rss_source_units": "bytes" if sys.platform == "darwin" else "KiB",
            "scope": "one child, all threads, full benchmark lifetime including open/replay/close; excludes wrapper",
        }
        json.dump(report, resources, indent=2)
        resources.write("\n")
    print(f"Benchmark exit={exit_code}; elapsed={elapsed:.3f}s; resources={output}; output={log}")
    if failure:
        print(failure, file=sys.stderr)
    return exit_code if exit_code >= 0 else 128 - exit_code


if __name__ == "__main__":
    sys.exit(main())
