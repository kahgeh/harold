#!/usr/bin/env python3
"""Controlled child-process failures; standard library only. Never signals other PIDs."""
import argparse
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import tempfile
import threading
import time
import uuid


class Evidence:
    def __init__(self, directory):
        directory.mkdir(parents=True, exist_ok=False)
        self.directory = directory
        self.file = (directory / "evidence.jsonl").open("x")

    def record(self, kind, **values):
        entry = {"kind": kind, "monotonic_ns": time.monotonic_ns(), **values}
        self.file.write(json.dumps(entry, sort_keys=True) + "\n")
        self.file.flush()
        os.fsync(self.file.fileno())

    def verify_acknowledged(self, case, recovered):
        # Re-read the persisted observer ledger, not memory held by the writer.
        entries = map(json.loads, (self.directory / "evidence.jsonl").read_text().splitlines())
        acknowledged = [entry for entry in entries
                        if entry["kind"] == "durable_ack" and entry["case"] == case]
        by_sequence = {record["sequence"]: record["event"] for record in recovered}
        for entry in acknowledged:
            require(by_sequence.get(entry["sequence"]) == entry["event"],
                    f"durable ack missing or changed at sequence {entry['sequence']}")
        self.record("ack_ledger_reconciled", case=case, count=len(acknowledged))


class Child:
    def __init__(self, suite, mode, case):
        self.suite = suite
        self.case = case
        self.name = f"{case}-{mode}-{len(suite.children)}"
        self.messages = queue.Queue()
        environment = dict(suite.environment)
        environment["OPENDATA_PREFIX"] = f"{suite.prefix}/{case}"
        stderr = (suite.evidence.directory / f"{self.name}.stderr").open("x")
        self.process = subprocess.Popen(
            [str(suite.binary), mode, case], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=stderr, text=True, env=environment,
        )
        stderr.close()
        suite.children.append(self)
        threading.Thread(target=self._read, daemon=True).start()
        self.ready = self.receive()
        require(self.ready.get("ok"), f"{self.name}: open failed")

    def _read(self):
        for line in self.process.stdout:
            self.messages.put(line)
        self.messages.put(None)

    def receive(self):
        try:
            line = self.messages.get(timeout=self.suite.timeout)
        except queue.Empty as error:
            raise TimeoutError(f"{self.name}: reply exceeded {self.suite.timeout}s") from error
        require(line is not None, f"{self.name}: exited; inspect stderr evidence")
        result = json.loads(line)
        self.suite.evidence.record("reply", child=self.name, result=result)
        return result

    def send(self, **command):
        self.suite.evidence.record("command", child=self.name, command=command)
        self.process.stdin.write(json.dumps(command) + "\n")
        self.process.stdin.flush()
        return self.receive()

    def append(self, event_id, durable=True, body=None):
        body = body if body is not None else f"payload:{event_id}"
        response = self.send(op="append", id=event_id, body=body, durable=durable)
        require(response.get("ok"), f"{self.name}: append failed: {response}")
        event = {"id": event_id, "body": body}
        if durable:
            # The controller persists the acknowledged identity outside the child.
            self.suite.evidence.record("durable_ack", case=self.case, event=event,
                                       sequence=response["sequence"])
        return event

    def records(self, start=0):
        response = self.send(op="scan", **{"from": start})
        require(response.get("ok"), f"scan failed: {response}")
        return response["records"]

    def stop(self, crash=False):
        if self.process.poll() is None:
            # SIGCONT is needed if an assertion failed while the child was paused.
            self.process.send_signal(signal.SIGCONT)
            if crash:
                self.process.kill()
            else:
                try:
                    self.process.stdin.write('{"op":"close"}\n')
                    self.process.stdin.flush()
                except BrokenPipeError:
                    pass
            try:
                self.process.wait(timeout=self.suite.timeout)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.process.stdin.close()
        self.process.stdout.close()


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def verify_records(records, expected):
    actual = [record["event"] for record in records]
    require(actual == expected, f"recovered events differ: expected={expected}, actual={actual}")
    positions = [record["sequence"] for record in records]
    require(all(a < b for a, b in zip(positions, positions[1:])), "sequences not increasing")


class Suite:
    def __init__(self, args, evidence, local_root):
        self.binary = args.binary.resolve()
        self.timeout = args.timeout
        self.evidence = evidence
        self.children = []
        self.environment = dict(os.environ)
        self.prefix = f"{os.environ.get('OPENDATA_PREFIX', 'lesson-9')}/{uuid.uuid4().hex}"
        self.local_root = local_root
        if local_root:
            self.environment.update(OPENDATA_BACKEND="local", OPENDATA_LOCAL_DIR=str(local_root))

    def child(self, mode, case):
        return Child(self, mode, case)

    def passed(self, case, **details):
        self.evidence.record("pass", case=case, **details)
        print(f"PASS {case}", flush=True)

    def crash_accepted(self):
        writer = self.child("writer", "crash-accepted")
        event = writer.append("accepted-only", durable=False)
        writer.stop(crash=True)
        recovered = self.child("writer", "crash-accepted")
        records = recovered.records()
        require([r["event"] for r in records] in ([], [event]), "unexpected recovery contents")
        recovered.stop()
        self.passed("crash-accepted", survived=bool(records), guaranteed=False)

    def crash_durable(self):
        writer = self.child("writer", "crash-durable")
        expected = [writer.append(f"durable-{i}") for i in range(5)]
        writer.stop(crash=True)
        reader = self.child("reader", "crash-durable")
        records = reader.records()
        verify_records(records, expected)
        self.evidence.verify_acknowledged("crash-durable", records)
        reader.stop()
        self.passed("crash-durable", acknowledged=len(expected))

    def retry_uncertain_reply(self):
        writer = self.child("writer", "uncertain-reply")
        # Simulate the application losing a successful response: do not add it to
        # its ack ledger. The transport observer still retains the raw reply.
        event = {"id": "run-1:event-1", "body": "same retry payload"}
        response = writer.send(op="append", durable=True, **event)
        require(response.get("ok"), "first request was not durable")
        writer.stop(crash=True)
        replacement = self.child("writer", "uncertain-reply")
        replacement.append(event["id"], body=event["body"])
        records = replacement.records()
        verify_records(records, [event, event])
        seen = {}
        for record in records:
            candidate = record["event"]
            require(candidate["id"] not in seen or seen[candidate["id"]] == candidate,
                    "duplicate identity with conflicting content")
            seen[candidate["id"]] = candidate
        require(list(seen.values()) == [event], "consumer dedup did not produce one event")
        replacement.stop()
        self.passed("uncertain-reply", stored_records=2, consumer_events=len(seen))

    def disconnected_reader(self):
        writer = self.child("writer", "reader-catchup")
        expected = [writer.append("before-disconnect")]
        reader = self.child("reader", "reader-catchup")
        first = reader.records()
        verify_records(first, expected)
        checkpoint = first[-1]["sequence"] + 1
        self.evidence.record("reader_checkpoint", next_sequence=checkpoint)
        reader.stop(crash=True)
        expected.extend(writer.append(f"offline-{i}") for i in range(8))
        reader = self.child("reader", "reader-catchup")
        verify_records(first + reader.records(checkpoint), expected)
        # Keep the reader open while more data arrives; its first poll may lag.
        current = reader.records()
        next_sequence = current[-1]["sequence"] + 1
        latest = writer.append("slow-reader")
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            new_records = reader.records(next_sequence)
            if new_records:
                verify_records(new_records, [latest])
                break
            time.sleep(0.02)
        else:
            raise TimeoutError("live reader never observed durable record")
        reader.stop()
        writer.stop()
        self.passed("reader-catchup", caught_up=len(expected), live_refresh=True)

    def fenced_takeover(self):
        old = self.child("writer", "takeover")
        expected = [old.append("before-takeover")]
        injected = time.monotonic_ns()
        old.process.send_signal(signal.SIGSTOP)
        replacement = self.child("writer", "takeover")
        expected.append(replacement.append("after-takeover"))
        recovered = time.monotonic_ns()
        old.process.send_signal(signal.SIGCONT)
        stale = old.send(op="append", id="stale-writer", body="must never persist", durable=True)
        require(not stale.get("ok"), "old writer acknowledged durability after takeover")
        require("detected newer DB client" in stale.get("error", ""),
                f"old writer failed for an unexpected reason: {stale}")
        old.stop(crash=True)
        replacement.stop()
        reader = self.child("reader", "takeover")
        records = reader.records()
        verify_records(records, expected)
        self.evidence.verify_acknowledged("takeover", records)
        reader.stop()
        self.passed("takeover", old_writer_error=stale["error"],
                    writer_open_us=replacement.ready["open_us"],
                    injection_to_durable_us=(recovered - injected) / 1000,
                    detection="immediate controller trigger, no election or router")

    def storage_error(self):
        if self.local_root is None:
            self.evidence.record("skip", case="storage-error", reason="local owned-directory injection only")
            return
        writer = self.child("writer", "storage-error")
        expected = [writer.append("before-storage-error")]
        moved = self.local_root.with_name(self.local_root.name + "-saved")
        self.local_root.rename(moved)
        try:
            self.local_root.write_text("temporary failure injected by lesson 9\n")
            failed = writer.send(op="append", id="during-error", body="uncertain", durable=True,
                                 deadline_ms=1000)
            require(not failed.get("ok"), "storage failure incorrectly acknowledged durability")
            writer.stop(crash=True)
        finally:
            # These paths are strictly inside this suite's fresh TemporaryDirectory.
            if self.local_root.exists():
                self.local_root.unlink()
            moved.rename(self.local_root)
        replacement = self.child("writer", "storage-error")
        recovered = replacement.records()
        self.evidence.verify_acknowledged("storage-error", recovered)
        # An unacknowledged request has unknown outcome, never a guaranteed loss.
        uncertain = {"id": "during-error", "body": "uncertain"}
        actual = [record["event"] for record in recovered]
        require(actual in (expected, expected + [uncertain]), "unexpected storage-error recovery")
        expected = actual
        expected.append(replacement.append("after-storage-restored"))
        verify_records(replacement.records(), expected)
        replacement.stop()
        self.passed("storage-error", surfaced_error=failed["error"], acknowledged_recovered=True)

    def run(self):
        try:
            for scenario in (self.crash_accepted, self.crash_durable, self.retry_uncertain_reply,
                             self.disconnected_reader, self.fenced_takeover, self.storage_error):
                scenario()
        finally:
            for child in self.children:
                child.stop(crash=True)


def main():
    crate = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=crate / "target/debug/examples/lesson_9")
    parser.add_argument("--output", type=Path,
                        default=crate / ".data/reliability" / uuid.uuid4().hex)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--cloud", action="store_true", help="use explicit s3/express environment")
    args = parser.parse_args()
    require(args.timeout > 0, "timeout must be positive")
    require(args.binary.is_file(), "build first: cargo build --locked --offline --example lesson_9")
    if args.cloud:
        require(os.environ.get("OPENDATA_BACKEND") in ("s3", "express"), "--cloud needs s3/express backend")
        require(os.environ.get("OPENDATA_PREFIX"), "--cloud needs a dedicated OPENDATA_PREFIX")
    evidence = Evidence(args.output.resolve())
    evidence.record("start", backend=os.environ.get("OPENDATA_BACKEND") if args.cloud else "local",
                    binary=str(args.binary.resolve()), timeout_seconds=args.timeout)
    try:
        if args.cloud:
            Suite(args, evidence, None).run()
        else:
            with tempfile.TemporaryDirectory(prefix="opendata-lesson9-") as temporary:
                Suite(args, evidence, Path(temporary) / "objects").run()
        evidence.record("complete", passed=True)
        print(f"Evidence: {evidence.directory}")
    except Exception as error:
        evidence.record("failed", error=str(error))
        raise
    finally:
        evidence.file.close()


if __name__ == "__main__":
    main()
