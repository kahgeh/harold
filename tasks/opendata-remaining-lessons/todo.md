# Complete OpenData lessons 3–10

The user authorized the full existing roadmap. Keep the standalone Cargo workspace,
Harold's runtime unchanged, and workflow product development in Iterant.

## Design

Each numbered runnable example adds one observable behavior. Share only storage
configuration and reusable checkpoint/trace utilities where necessary. Local
storage is the default for persistence experiments; cloud runs require explicit
bucket, region and isolated prefix. No implicit bucket creation or deletion.

Use published pinned APIs, durable flush before acknowledging, inclusive per-key
cursors, bounded concurrent producers, stable trace source identities, and a
coordinator outside terminated writers. Collect raw monotonic measurements and
state polling effects. A writer open actively takes ownership; it is not a passive
standby. Cloud compatibility is conditional until tested against supplied buckets.

## Plan and acceptance

- [x] Recover the approved roadmap and existing lesson behavior.
- [x] Audit dependencies for cloud use and any manifest changes.
- [x] Verify pinned storage, reader and fencing APIs.
- [x] Implement lessons 3–6: independent cursors, saved local resume, S3 storage, concurrent monitoring.
- [x] Implement lesson 7: structured causal execution traces and reconnect/deduplication.
- [x] Implement lesson 8: explicit Express configuration and storage/replay exercise.
- [x] Implement lesson 9: controlled process crash/takeover, uncertain retries and reader catch-up evidence.
- [x] Implement lesson 10: bounded workload, distinct timing intervals, raw samples and summary.
- [x] Write numbered lesson walkthroughs and cloud run instructions.
- [x] Run local examples, meaningful persistence/recovery checks, format and clippy.
- [x] Run AWS trials if credentials and bucket targets are supplied; otherwise record that external validation remains pending.
- [x] Obtain completion review, fix findings and record evidence.

## Verification approach

Use fresh temporary storage for verification. Preserve the user's existing data.
Check exact lesson assertions, restart behavior and independent-reader visibility;
kill only child processes created by the reliability harness. Use offline Cargo
where possible after audit approval. No benchmark result establishes a guarantee.

## Results

Implemented every numbered lesson with shared storage/checkpoint helpers and
separate walkthroughs. Exact serde/serde_json additions and cloud scope approved
by rust_supply_chain_auditor; cloud XML parser residual documented.

Initial full local verification: `python3 scripts/verify_local.py`, 17 checks
passed, results `examples/opendata-log/results/local-1789891031365548000/`.
This includes six process-failure scenarios and independent-reader timing.
Detailed evidence and cloud limitations: `examples/opendata-log/VERIFICATION.md`.

No bucket targets were supplied. Cloud code/configuration and run instructions
are delivered; actual S3/Express runs (including renewal) remain external
acceptance, not a local pass. No cloud resources were provisioned.

Final complete rerun including queue/CPU/RSS measurement and configurable reader
refresh: 17 checks passed, evidence
`examples/opendata-log/results/local-1789891295157296000/`. Reviewer additionally
verified 100 producers/1000 events and unsuccessful deadline behavior with partial
CSV retained. Completion reviewer approved with an explicit thumbs-up and no blocking findings.
Reviewed source hashes match the final verification evidence. AWS acceptance
remains pending as documented.

## Deferred cloud configuration — September 21, 2026

- [x] Verify region, bucket and prefix are already runtime environment settings.
- [x] Add a concise configuration table to the README and clarify that Sydney is
  an example, local remains the default, and credentials/bucket details can wait.
- [x] Record the user's deferred-cloud preference; no cloud requests or runtime
  code changes were needed. Existing credential-provider caveat remains explicit.

## Publish the lessons — September 21, 2026

- [x] Inspect worktree, staged scope, submodule and remote divergence.
- [x] Confirm final reviewed executable sources still match the successful
  17-check verification hashes; validate documentation links and whitespace.
Publication scope: one commit containing the OpenData lesson work and related
task/lesson notes, followed by a normal push of `main`. Confirm the remote hash
after pushing; report that result in the delivery response.

Unrelated `tasks/local-scan-intervals/` and `tasks/restore-dashboard-connection/`
remain outside this commit. Iterant is a separate repository and is not part
of this push.
