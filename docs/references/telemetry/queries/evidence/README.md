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
| `evidence-follow-up-1.sql` | `harold.evidence.lines` with `outcome = typed` rose after `since`, or `pod.evidence.missing` has points after `since` and none is 1. Reasons `line_typed`, `none_missing`, `line_not_typed` and `no_gauge` |
| `thread-rows-once-1.sql` | The span `thread.duplicate_row` has a point after `since`, or 7 days of `app page connected` have no `streams_for_page` above 1. Reasons `duplicate_removed`, `no_second_stream` and `too_early` |
| `review-skip-closes-1.sql` | The three files `reviews/agent-jobs.md`, `reviews/model-stall.md` and `reviews/harder-held-out.md` log no `pod file unreadable` in the 7 days after `since`, and Harold logged lines. Reasons `files_read`, `still_unreadable` and `no_harold_lines` |
| `flow-spec-review-1.sql` | `pod.spec.lead_time` with `phase = architect_review` rose after `since`. Reasons `phase_seen` and `no_phase_yet` |
| `flow-spec-review-2.sql` | The median of `open_to_approved` for the specs recorded after `since` is above 60 seconds. Reasons `approval_wait_true` and `approval_wait_zero` |
| `flow-merge-1.sql` | `pod.spec.lead_time` with `phase = ready_to_completed` rose after `since`. Reasons `phase_seen` and `no_phase_yet` |
| `flow-merge-2.sql` | `pod.inventory.items` with `list = ready_not_merged` has points after `since`. Reasons `list_seen` and `no_list` |
| `flow-idea-1.sql` | `pod.inventory.items` with `list = idea` has points after `since`. Reasons `list_seen` and `no_list` |
| `flow-idea-2.sql` | `pod.spec.lead_time` with `phase = idea_to_open` rose after `since`. Reasons `phase_seen` and `no_phase_yet` |
| `flow-release-1.sql` | `pod.spec.lead_time` with `phase = merged_to_released` rose after `since`. Reasons `phase_seen` and `no_release_yet` |
| `flow-installed-1.sql` | `harold.release.install_lag` rose after `since`. Reasons `install_seen` and `no_install_yet` |
| `flow-installed-2.sql` | `pod.inventory.items` with `list = released_not_installed` has points after `since`. Reasons `list_seen` and `no_list` |
| `agent-jobs-1.sql` | `harold.feature.used` with `spec.id = agent-jobs` rose after `since`. Reasons `job_added` and `no_job_yet` |
| `agent-jobs-2.sql` | `harold.job.ends` with `how = ended` rose after `since`, with no `end_line = failed`. Reasons `end_told`, `no_end_yet` and `end_not_told` |
| `agent-jobs-hosts-1.sql` | `harold.job.ends` with a non-empty `host` rose after `since`. Reasons `host_end_seen` and `no_host_end_yet` |
| `pod-role-record-1.sql` | `pod role recorded` lines for each of `chief`, `pnem` and `pod_coordination`, and a `pod role lookup` line with `outcome = found`, from `since` to `since` + 14 days, and no lookup with `no_record` or `session_file_unreadable`. Reasons `lookups_found`, `lookup_faults` and `no_lookup_yet` |
| `decision-tool-1.sql` | `decision added` and `decision evaluated` log lines from `since` to `since` + 30 days. Reasons `evaluated`, `only_added` and `none_added` |
| `cost-per-value-1.sql` | `pod.spec.value` and `harold.turn.tokens` have points after `since`. Reasons `value_seen`, `no_value_yet` and `no_values` |
| `workflow-cost-1.sql` | `harold.workflow.cost` rose after `since` for 2 or more model workflows. Reasons `workflow_cost_recorded`, `one_workflow_only` and `no_workflow_run_yet` |
| `agent-restart-1.sql` | `restart check result` lines from `since` to `since` + 30 days. Reasons `checked` (all four groups), `checked_identity_communication` (stage 1 alone) and `no_restart_yet` |

## Run one

```
bin/logq --named evidence/evidence-check-1 since='2026-11-12 03:05:00'
```

`bin/logq --named` takes a name in this one folder, `evidence/`. You and Harold read the same file text. Harold reads the file from the base branch with `git show`, so a file that is not merged never runs.
