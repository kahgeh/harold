# Job Tool

The **job tool** shows the long commands that an agent started outside its pane. It tells the agent when each command ends. It is a [tool](README.md) with its own Turso file, `tools/job/jobs.db`, in Harold's data folder ([ADR 0001](../../adr/0001-tools-are-modules-hosted-by-harold.md)). It trusts local callers ([ADR 0002](../../adr/0002-tool-services-trust-local-callers.md)).

The service is `harold.tools.job.v1.JobService`, with the calls `Add`, `List` and `Remove`. The proto is `harold-api/proto/tools/job.proto`.

An **agent job** is a long command that an agent started outside its pane and recorded with Harold. The glossary uses "background job" for Harold's own threads, so this page never uses that word for an agent job.

## The commands

```sh
harold tool job add calibration --pid 4121 --for 20m --agent "$REX_BLOCK" [--what "one line"]
harold tool job list
harold tool job remove calibration
```

| Command | What it does |
| --- | --- |
| `add NAME --pid PID --for TIME --agent PANE [--what TEXT]` | Records the job. `NAME` has 1 to 48 characters of `a-z`, `0-9` and `-`. `TIME` is the expected run time: `90s`, `20m` or `2h`. A bare number is minutes. The most is 7 days. `PANE` is the pane id of the agent. `TEXT` has at most 120 characters. |
| `list` | Prints one line for each job that runs, and for each job that ended in the last 10 minutes. |
| `remove NAME` | Takes away a wrong record. It is not an end: Harold types no line. The tool counts it as `removed`. |

Harold reads the start time of the process when it accepts the job. So a later process with the same pid is not the job. These refusals exist:

| Refusal | When |
| --- | --- |
| `invalid <field>` | A field fails its check, for example `invalid name`. |
| `no_such_process` | No process has this pid now. |
| `name_in_use` | A job of this name runs. |
| `unknown_job` | `remove` names no running job. |

## The check

Every 15 seconds, and once at the start of Harold, the tool checks each running job. The job runs when the pid has a process and the process has the same start time. When it does not, the job has ended. Harold cannot read the exit code of a process that it did not start, so the end line gives no exit code.

A restart of Harold keeps the jobs and checks each one again.

## The end line

When a job ends, Harold types one line into the pane of the agent:

```
JOB calibration ended after 21 m (pid 4121). Check its output.
```

The line is Harold's own fixed text. The agent's job name is cut to 48 characters. A run of less than 1 minute reads `<1 m`.

- The tool types the line through the typing port of the host, as `Writer::Tool("job")`. The typing rule is the one of the [evidence line](evidence.md#the-line-to-the-chief): the port types only into an idle pane with an empty input box. The tool does not change that rule.
- An idle pane gets the line at once (`end_line = typed`). A busy pane gets it at a later check, when the pane is idle (`typed_at_turn_end`).
- When the pane is gone, Harold records `no_pane` and types nothing. When Enter fails after the text is typed, Harold records `failed` and does not type again.
- The tool types the line once, also after a restart. The outcome is in the column `end_line` of `jobs.db`.

## The control-tower

The control-tower draws one **job line** under the row of the pane of the agent, in the agent view only, at the indent of the todos line:

```
    ⏵ job macos-tests · pid 4121 · 12 m of 20 m, ends about 06:55
    ⏵ job macos-tests · pid 4121 · 4 m past its end (expected 06:43)
    ⏵ job release-build · pid 3977 · ended 2 m ago
```

- The line is muted while the job runs, amber after the expected end, and muted with `ended … ago` for 10 minutes after the end. Then it leaves.
- A narrow window cuts the line at its end with `…`.
- A job whose pane has no row shows under the group `jobs with no agent`, at the end of the list.
- A job on another host names the host before the pid. Harold's own machine has an empty host.

The jobs travel in `AgentStateSnapshot.jobs` (field 12, message `AgentJob`). A change of a job raises `through_event_version`, so every control-tower gets it at once. An older control-tower ignores the field. The list is in memory only: the tool publishes it after each change and at each check.

## The file

`tools/job/jobs.db` has one table, `jobs`: `name`, `what`, `pane_id`, `pid`, `process_start_s`, `started_at_ms`, `due_at_ms`, `ended_at_ms` (0 while the job runs), `state` (`running`, `ended` or `removed`) and `end_line`. The tool deletes a finished job 7 days after its end.

## Telemetry

See the [telemetry reference](../telemetry/README.md).

- `harold.feature.used` with `spec.id = agent-jobs`: one for each accepted `add`.
- `harold.job.ends` (counter): `how` (`ended`, `removed`) and `end_line` (`typed`, `typed_at_turn_end`, `no_pane`, `failed`, and `none` for a removed job). Each series starts at 0.
- `harold.job.past_due` (gauge): the running jobs past their expected end. It starts at 0.
- Log lines: `job added`, `job ended` and `job end not told`. The last one is logged once, 10 minutes after the end, only when `end_line` is `no_pane` or `failed`. A busy pane that gets its line later logs nothing.
- Evidence: `evidence/agent-jobs-1.sql` and `evidence/agent-jobs-2.sql`.
