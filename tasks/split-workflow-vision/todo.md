# Separate the workflow product from the OpenData experiments

The user chose a new repository for multi-user sessions, multi-agent execution, and workflow capture/refinement/reuse. Keep event-stream and agent execution trace examples here, including reliability, S3 Standard, S3 Express One Zone, and performance experiments.

- [x] Initialize an independent local repository and move the workflow discovery into it.
- [x] Preserve the product vision, ActiveGraph evidence, and outstanding design decisions.
- [x] Update this example's roadmap and define concrete reliability/performance experiments.
- [x] Check S3 Express configuration support against pinned source and AWS documentation.
- [x] Verify repository boundaries, links, and the unchanged runnable lesson.
- [x] Complete independent documentation review.

This task establishes repository boundaries and experiment plans. At the split stage only the in-memory lesson was implemented. The later delivery now implements all ten lessons and local experiments; see `tasks/opendata-remaining-lessons/todo.md`. Real cloud verification remains pending.

## Results

The user delegated naming; the new repository is **Iterant**, at
`/Users/kahgeh/Dev/p/iterant`, initialized independently on `main` with no remote.
It contains the product vision, ActiveGraph research, and outstanding platform
design task. Product content formerly in `tasks/multi-user-agent-workflows/`
was migrated and that empty directory removed.

Harold retains `examples/opendata-log/`, now with a ten-lesson roadmap and
`EXPERIMENTS.md`. The plan separates accepted-append, durable-acknowledgment,
reader-visibility, and failover measurements and defines reconciliation for
crashes, lost replies, slow readers, and writer fencing.

Pinned source exposes S3 Express through `AWS_S3_EXPRESS=true`, configured
bucket/region, zonal endpoint derivation, and refreshing session credentials.
AWS requirements and placement/failure-boundary differences are cited in the
experiment document. This has not been integration-tested against AWS.

Verification: seven Markdown files and 19 local links checked; existing lesson
binary matches README output exactly with no stderr; independent Git roots
confirmed; Harold production manifests/runtime files unchanged;
`git diff --check` passed. Independent documentation review approved the split
with no findings. No dependencies,
cloud resources, or performance results were added.
