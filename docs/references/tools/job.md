# Job Tool

The **job tool** shows the long commands that an agent started outside its pane. It tells the agent when each command ends. It is a [tool](README.md) with its own Turso file, `tools/job/jobs.db`, in Harold's data folder ([ADR 0001](../../adr/0001-tools-are-modules-hosted-by-harold.md)). It trusts local callers ([ADR 0002](../../adr/0002-tool-services-trust-local-callers.md)).

The service is `harold.tools.job.v1.JobService`, with the calls `Add`, `List` and `Remove`. The proto is `harold-api/proto/tools/job.proto`.

An **agent job** is a long command that an agent started outside its pane and recorded with Harold. The glossary uses "background job" for Harold's own threads, so this page never uses that word for an agent job.

## The commands

```sh
harold tool job add calibration --pid 4121 --for 20m --agent "$REX_BLOCK" [--what "one line"]
harold tool job add calibration --unit cal-1 --user --for 20m --agent "$REX_BLOCK"
harold tool job list
harold tool job remove calibration
```

| Command | What it does |
| --- | --- |
| `add NAME --pid PID --for TIME --agent PANE [--what TEXT]` | Records the job. `NAME` has 1 to 48 characters of `a-z`, `0-9` and `-`. `TIME` is the expected run time: `90s`, `20m` or `2h`. A bare number is minutes. The most is 7 days. `PANE` is the pane id of the agent. `TEXT` has at most 120 characters. |
| `add NAME --unit UNIT [--user] --for TIME --agent PANE [--what TEXT]` | Records a systemd unit in place of a pid. `--user` is for a unit of the user's systemd (`systemctl --user`). `--unit` and `--pid` are one or the other. The unit name has at most 128 characters of letters, digits and `: _ . @ -`, and does not start with `-`. |
| `list` | Prints one line for each job that runs, and for each job that ended in the last 10 minutes. |
| `remove NAME` | Takes away a wrong record. It is not an end: Harold types no line. The tool counts it as `removed`. |

Harold reads the start time of the process when it accepts the job. So a later process with the same pid is not the job. These refusals exist:

| Refusal | When |
| --- | --- |
| `invalid <field>` | A field fails its check, for example `invalid name`. |
| `no_such_process` | No process has this pid now. |
| `no_such_unit` | `systemctl` does not find the unit. |
| `no_systemd` | `--unit` on a machine with no systemd. On macOS, `--unit` is always refused. |
| `name_in_use` | A job of this name runs. |
| `unknown_job` | `remove` names no running job. |

## A unit

An agent starts a long command on a Linux machine, such as omarchy, with `systemd-run`. The agent records the unit on the machine that runs it, with one command. Start the unit with `--remain-after-exit`, so that its exit code stays readable after it ends:

```sh
ssh omarchy systemd-run --user --remain-after-exit --unit cal-1 ...
ssh omarchy '~/bin/harold/harold tool job add calibration --unit cal-1 --user --for 20m --agent block:01a1…'
```

A transient unit that ends cleanly is unloaded at once. The exit code of such a unit is unknown. `add` reads `RemainAfterExit` of the unit. When it is `no`, `add` prints `the exit code will be unknown: start the unit with --remain-after-exit` and still adds the job.

`--agent` takes the pane id as the master names it: `block:<id>` for an agent on the Mac, and `rex:<label>/block:<id>` for an agent on a Rex host. The slave keeps it as text and does not check it.

## The check

Every 15 seconds, and once at the start of Harold, the tool checks each running job. The job runs when the pid has a process and the process has the same start time. When it does not, the job has ended. Harold cannot read the exit code of a process that it did not start, so the end line gives no exit code.

For a unit, Harold runs `systemctl [--user] show <unit> -p ActiveState,SubState,Result,ExecMainStatus,LoadState` at each check. The job runs while `ActiveState` is `active` or `activating` and `SubState` is not `exited`. The job has ended at any other value, and when the unit is not found (`LoadState = not-found`). The exit code is `ExecMainStatus` while the unit exists, and unknown when it does not. After Harold reads the exit code of a unit that stays loaded, it runs `systemctl [--user] stop <unit>` once, so that the ended unit does not stay. A `systemctl` that fails or takes more than 5 seconds is not an end: the job stays `running`.

A restart of Harold keeps the jobs and checks each one again.

## The end line

When a job ends, Harold types one line into the pane of the agent:

```
JOB calibration ended after 21 m (pid 4121). Check its output.
```

The line is Harold's own fixed text. The agent's job name is cut to 48 characters. A run of less than 1 minute reads `<1 m`.

- The tool types the line through the typing port of the host, as `Writer::Tool("job")`. The typing rule is the one of the [evidence line](evidence.md#the-line-to-the-chief): the port types only into an idle pane with an empty input box. The tool does not change that rule.
- For a unit, the line reads `JOB calibration on omarchy ended after 21 m: exit 0.`, or `failed (exit 2)` for a non-zero code, or `exit unknown`. A process on a slave reads `JOB calibration on omarchy ended after 21 m (pid 4121). Check its output.`
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
- A job on another host names the host before the pid or the unit: `⏵ job calibration · omarchy · unit cal-1 · 12 m of 20 m, ends about 07:00`. Harold's own machine has an empty host.
- While the master's slave link to the host is closed, the line reads `⏵ job calibration · omarchy · unit cal-1 · omarchy not reached for 3 m`, counted from the `slave link closed` time. Harold does not assume an end. The job state is `unknown`.
- The job sits under the row of the pane that `--agent` names, on whichever host the job runs.

The jobs travel in `AgentStateSnapshot.jobs` (field 12, message `AgentJob`). A change of a job raises `through_event_version`, so every control-tower gets it at once. An older control-tower ignores the field. The list is in memory only: the tool publishes it after each change and at each check.

## Jobs on a slave

A job that runs on a slave (omarchy) is recorded by the slave's Harold, which runs the check. The slave sends its jobs to the master in `AgentStateSnapshot.jobs`, with no change to `slave.proto`. The master adds them to the snapshot that it serves, and sets `host` to the slave's name (also its Rex host label).

- **One count for each end.** For a slave's job, only the master counts `harold.job.ends` and logs `job ended` and `job end not told`. The slave counts nothing, and records `end_line = master`.
- **The end line.** The master types it when a slave's job changes to `ended`, into a pane on the Mac. The master records each step in the table `told` of its own `jobs.db` (key: host, name and start time), so a restart or a repeated snapshot does not type it twice. For an agent pane on a Rex host the master cannot type yet: it records `end_line = remote_pane`, and the job line shows the end.
- **Unknown.** The state of a slave's job is `unknown` only while the master's slave link to that slave is closed. The master keeps the last jobs of the slave. When the link opens, the slave's next snapshot gives their state. A job leaves the snapshot 10 minutes after its end.
- **Not reached.** A job in `unknown` for 30 minutes makes the master log `job host not reached`, once for each job and link outage.

## Keep the machine awake

When `[job] keep_awake_folder` is set, a running job keeps its machine awake. See the [operation reference](../operation/README.md#keep-a-machine-awake-during-an-agent-job) for the contract with the idle-suspend. Harold writes the file `<folder>/harold-job-<name>` at once and again every `keep_awake_every_s` seconds (300). It deletes the file when the job ends or is removed. At start, it deletes each `harold-job-*` file that names no running job, and leaves every other file in the folder alone. `harold tool job list` adds `keeps awake` to a job that has its file.

Harold logs `job keeps machine awake` once when it writes the first file of a job, and `job keep awake failed` once for each job when a write fails. The `reason` is `not_found`, `permission_denied` or `other`. The `folder` field holds the setting's text, never the expanded path.

## The file

`tools/job/jobs.db` has one table, `jobs`: `name`, `what`, `pane_id`, `pid`, `process_start_s`, `started_at_ms`, `due_at_ms`, `ended_at_ms` (0 while the job runs), `state` (`running`, `ended` or `removed`) `end_line` (also `master` on a slave, and `remote_pane` on a master's `told` record), `unit`, `user_unit` and `exit_code` (a unit's exit code, empty when unknown). The table `told` is the master's record of the jobs of slaves: `host`, `name`, `started_at_ms`, `ended_seen_ms`, `end_line` and `unreached_mark_ms`. The tool deletes a finished job 7 days after its end.

## Telemetry

See the [telemetry reference](../telemetry/README.md).

- `harold.feature.used` with `spec.id = agent-jobs`: one for each accepted `add`. With `spec.id = agent-jobs-hosts`: one for each accepted `add --unit`.
- `harold.job.ends` (counter): `how` (`ended`, `removed`) and `end_line` (`typed`, `typed_at_turn_end`, `no_pane`, `failed`, `remote_pane`, and `none` for a removed job) and `host` (empty for a job on this machine). Each series of the empty `host` starts at 0.
- `harold.job.past_due` (gauge): the running jobs past their expected end. It starts at 0.
- Log lines: `job added`, `job ended`, `job end not told`, `job host not reached`, `job keeps machine awake` and `job keep awake failed`. The last one is logged once, 10 minutes after the end, only when `end_line` is `no_pane` or `failed`. A busy pane that gets its line later logs nothing.
- Evidence: `evidence/agent-jobs-1.sql`, `evidence/agent-jobs-2.sql` and `evidence/agent-jobs-hosts-1.sql`.
