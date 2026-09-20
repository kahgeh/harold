# OpenData Log learning example

## Intent and proposed progression

Build a separate Rust learning crate whose small examples build toward multi-agent event streams and execution traces, then test their reliability and performance. Use deterministic simulated agents so each lesson exposes log behavior without requiring model credentials. Early lessons run locally; later S3 Standard and S3 Express One Zone lessons introduce external storage. Keep the crate independent of Harold's workspace dependencies and runtime. Multi-user application and workflow capture/refinement/reuse design belongs to the new `/Users/kahgeh/Dev/p/iterant` repository.

The user chose monitoring several agents' progress. The agreed progression is:

1. One agent appends `Started`, `Progress`, and `Finished`; read its history.
2. Two agents append interleaved records under distinct keys; show each key's history and the shared sequence numbering.
3. A monitor remembers each stream's next read position and reads only new records. Checkpoints belong to the application.
4. Replace memory storage with a local directory; wait for durability, close, reopen, and replay. Distinguish log persistence from checkpoint persistence.
5. Replace local log storage with S3. Configure the bucket, region, storage prefix, and AWS credentials; append, wait for durability, close, reopen the same log, and replay. Keep the monitor's saved checkpoints local to isolate the storage change and explain that S3 log persistence does not automatically persist checkpoints.
6. Run simulated producers concurrently and let a monitor catch up. Preserve each agent's stream order; do not imply an atomic merged snapshot or coordinated consumer groups.
7. Introduce structured agent execution traces with stable run/event identity and causal links. Use durable history for complete capture and subscriber catch-up; do not equate a droppable live export with the authoritative record.
8. Exercise S3 Express One Zone directory-bucket storage, including credential refresh, conditional writes, and recovery. Compare with S3 Standard under controlled placement and settings.
9. Inject crashes, pauses, lost responses, storage errors, and slow readers; verify durable event survival, duplicates, ordering, fencing, and catch-up.
10. Measure append acceptance, durable acknowledgment, independent-reader visibility, throughput, tail latency, queue growth, resource use, and end-to-end takeover time. Keep exact configuration and raw evidence.

Each step should remain runnable and show expected output with a short explanation. Avoid task claiming, leases, exactly-once processing, or real LLM integration unless the learning goal requires them.

## Checklist

- [x] Settle the target multi-agent behavior and first lesson scope.
- [x] Complete supply-chain audit and verify the selected published release's API.
- [x] Implement the first small runnable lesson in an independent crate.
- [x] Run it and check the observed record order and replay behavior.
- [x] Obtain completion review and resolve findings.

## Evidence and review

## Delivered scope

Lesson 1 is implemented in `examples/opendata-log/`: a 70-line `src/main.rs`, a walkthrough with observed output, and an independent manifest/lockfile. It appends two literal reports for `agent-a`, reads them, remembers the next sequence, appends `Finished`, reads just that report, and demonstrates an empty catch-up read. Both log and checkpoint are in memory.

Lesson 2 is now implemented as `examples/opendata-log/examples/lesson_2.rs`, with its own `LESSON_2.md` walkthrough: interleaved reports from two simulated agents, scanned by key with their original shared sequence positions. See `tasks/opendata-log-lesson-2/todo.md` for verification evidence. Lessons 3-10 are now implemented; see `tasks/opendata-remaining-lessons/todo.md` and `examples/opendata-log/VERIFICATION.md` for current local verification and pending AWS acceptance. The evidence below records the original lesson-1 delivery.

The user requested separate ownership: storage/trace learning and experiments remain here; the broader runtime and workflow vision now lives in Iterant. The roadmap preserves local persistence, S3 Standard, and concurrency lessons and adds traces, Express, fault injection, and performance measurement. The original split did not provision cloud resources. The later lessons now implement persistent storage, without provisioning buckets.

The executable uses published `opendata-log =1.0.0`, `opendata-common =0.1.17`, `bytes =1.11.1`, and `tokio =1.49.0`. The auditor checked published APIs and identified that common 0.1.18 belongs to a different SlateDB generation.

Supply-chain audit approved fetch/build/run/lint for the exact lock SHA-256 `b91478108016d4cb0d1e34ac19e903f24b55b3b9d8299429c49b62e30589ca77`. All 351 registry archive checksums, 63 build scripts, and 28 procedural macro crates were reviewed. Two quick-xml 0.38.4 denial-of-service advisories in cloud dependency paths are unreachable in this explicit in-memory, literal-data lesson; the full dependency graph is not globally RustSec-clean. Reassess before cloud/XML/network inputs. Report: `/Users/kahgeh/Dev/p/crates-audit/opendata-agent-progress-locked-2026-09-20.md`.

Verification so far:

- `cargo run --locked --offline --manifest-path examples/opendata-log/Cargo.toml` compiled and exited successfully. Actual output shows sequences 0/1, then only 2, then no new reports.
- A fresh run of the compiled binary from the crate directory exactly matched the README output and produced no stderr.
- `cargo fmt --manifest-path examples/opendata-log/Cargo.toml --check` and `git diff --check` passed.
- Metadata checks confirm Harold's original four workspace members and one independent tutorial member; root Cargo.toml and Cargo.lock are unchanged.
- `cargo clippy --locked --offline --all-targets --manifest-path examples/opendata-log/Cargo.toml -- -D warnings` passed without warnings.
- Completion reviewer independently ran the binary, confirmed the documented output, found no issues, and gave an explicit thumbs-up for lesson 1.

No separate unit tests were added for this deterministic teaching script; running the real log and checking its output is the acceptance check. No persistent-data or concurrent-agent behavior is claimed.
