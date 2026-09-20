# Lesson 2: two agents, separate streams

The user requested the next agreed lesson: interleave two simulated agents'
reports in one OpenData log, then scan each agent's key and observe the shared
sequence counter. Preserve the runnable lesson 1 and existing dependencies.

- [x] Add a small independently runnable Cargo example with deterministic interleaving.
- [x] Explain stream selection and sequence gaps with observed output.
- [x] Run both lessons and check their documented output, formatting, and Clippy.
- [x] Obtain completion review and resolve findings.

Use Cargo's existing example discovery (`examples/lesson_2.rs`) so the manifest,
lockfile, and default lesson-1 command need no changes. Keep storage in memory.
Thread concurrency, multi-stream checkpoints, and cloud storage remain later
lessons. Verify actual output rather than adding implementation-mirroring unit tests.

## Results

Implemented `examples/opendata-log/examples/lesson_2.rs`, run with
`cargo run --locked --example lesson_2` from the crate. The program appends
six reports one at a time, prints returned sequence positions, then scans A
and B separately. Actual output gives A positions 0/2/4 and B positions 1/3/5.
`LESSON_2.md` explains the shared counter, key selection, lower-bound scans,
and why record count is not a checkpoint. README and experiment status now
identify lessons 1 and 2 as implemented.

Verification:

- `cargo run --locked --offline --example lesson_2` compiled and exited 0.
- Fresh runs of both compiled lesson binaries exactly matched their documented
  output, with empty stderr.
- `cargo fmt --check` and
  `cargo clippy --locked --offline --all-targets -- -D warnings` passed.
- `git diff --check` passed; local Markdown links resolve.
- No manifest, lockfile, dependency, lesson-1 source, or production runtime changes.

Independent completion review approved the lesson with no findings. No cloud storage, concurrency, or
multi-stream checkpoint implementation is claimed.
