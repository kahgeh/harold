# Evidence queries

An **evidence query** is a named query file `<id>-<n>.sql` in this folder. `<id>` is the id of a spec. `<n>` is 1, 2 and on. The build of the spec adds each file with the code. Harold runs the file after the release of the spec (see the [evidence tool](../../../tools/evidence.md)).

## The form

- The query takes one parameter, `{since:DateTime}`. It is the merge time of the spec, in UTC. So the query reads only signals after the work.
- The query returns one row with two columns:
  - `present`: 1 or 0.
  - `reason`: a fixed word of 1 to 40 characters, from `a` to `z` and `_`. Examples: `use_seen`, `no_use_yet`.
- The file is at most 64 KiB. A bigger file counts as a `bad_query`.
- Start the file with the comment lines of the other named queries: `Note`, `Waits for`, and what the query shows.
- Read only from `otel.*`. The user `hyperdx_ro` cannot write.

## The files

| File | What it shows |
| --- | --- |
| `evidence-check-1.sql` | `pod.evidence.missing` has a point for the spec `evidence-check` after `since`. Reasons `gauge_seen` and `no_gauge` |
| `evidence-check-2.sql` | `harold.evidence.reads` with `outcome = ok` rose after `since`. Reasons `read_ok` and `no_ok_read` |
| `flow-spec-review-1.sql` | `pod.spec.lead_time` with `phase = architect_review` rose after `since`. Reasons `phase_seen` and `no_phase_yet` |
| `flow-spec-review-2.sql` | The median of `open_to_approved` for the specs recorded after `since` is above 60 seconds. Reasons `approval_wait_true` and `approval_wait_zero` |

## Run one

```
bin/logq --named evidence/evidence-check-1 since='2026-11-12 03:05:00'
```

`bin/logq --named` takes a name in this one folder, `evidence/`. You and Harold read the same file text. Harold reads the file from the base branch with `git show`, so a file that is not merged never runs.
