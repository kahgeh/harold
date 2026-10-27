# Watch Tool

A **watch** is a schedule an agent asks Harold for: run one of its skills with a model every few minutes for a set time, and report to the chief or to the agent itself. It is not the agent monitor's watching of agents (the glossary's "Watching agents"), and not the `WatchAgentStates` stream.

The watch tool is a [tool](README.md) that holds the watches, runs them and types their reports. The service is `harold.tools.watch.v1.WatchService`. It trusts local callers (ADR 0002): there is no token, and it checks every field itself. The proto is `harold-api/proto/tools/watch.proto`.

## Making, listing and stopping a watch

```sh
harold tool watch start after-release --skill pod-watch --every 10 --for 120
#   started after-release in /Users/you/code/chief-of-pods until 20:45, every 10 minutes, reporting to chief
harold tool watch start after-release --skill pod-watch --every 5 --for 60 --report-to chief --run-budget 0.03 --every-run
harold tool watch stop after-release        # stopped after-release
harold tool watch list                      # after-release running in .../chief-of-pods until 20:45, limit 120 s, 3 runs, 1 report, $0.03
```

Each command prints one line (`list` prints one per watch, then `spent today $…`). Any agent can run them.

| Flag | Meaning | Valid values |
|---|---|---|
| `<name>` | Names the watch in the commands, the reports and the log | 1 to 64 characters from `A-Z`, `a-z`, `0-9`, `-` and `_` |
| `--skill` | The skill to run, by its folder name under `<folder>/.claude/skills/` | The same characters as a name, so no `/`, `.` or `..`. `<folder>/.claude/skills/<skill>/SKILL.md` must exist, and its real path, with links followed, must lie inside `<folder>/.claude/skills/` or inside a folder in `[watch] skill_folders` |
| `--every` | Minutes from the start of one run to the start of the next | 2 to 240 |
| `--for` | Minutes the watch runs from now | 1 to 1440 |
| `--report-to` | Who gets the reports | `chief`, or `self` for the watch's own folder. `self` when left out. No other folder |
| `--run-budget` | The most one run may spend, in US dollars | 0.01 or more, and no more than `[watch] max_run_budget_usd` when that is set. Left out, `[watch] run_budget_usd` when set, else no run limit |
| `--run-limit` | The most seconds one run may take before Harold stops it (the run limit) | A whole number above 0, and no more than `[watch] max_run_limit_secs` when that is set. Left out, 120. Runs take the model one at a time, and a run waits at most 1 minute for it, then is skipped: a long run limit on one watch can make the runs of other watches wait, and skip |
| `--every-run` | Type a line after every run, not only when the run answers `REPORT` | Present or left out. Left out, a `QUIET` run types nothing |

- **The folder** is the working directory of the `harold tool watch start` command. The CLI sends it, and the tool does not trust it: it must be an absolute path that exists and is a folder, and Harold stores its canonical form, with links followed and no trailing slash. Otherwise `start` refuses with `invalid folder`.
- **`start`** runs the first check at once, then one every `--every` minutes until `--for` minutes have passed. `start` with the name of a running watch replaces it: its settings, its end and its run count start over.
- **`stop`** ends a running watch at once. A run in progress is stopped and its report is not typed. No closing line is typed, since an agent asked for it.
- **A watch also ends** when its time is up, when a daily budget is set and spent, or after 3 failed runs in a row.
- **`start` refuses** with `unknown skill <skill>`, `too many watches` (5 are running), `daily budget spent`, and `invalid <flag>` for a value outside the table. At the service these are `InvalidArgument` `invalid <field>` (`invalid name`, `invalid folder`, `invalid skill`, `invalid every_minutes`, `invalid for_minutes`, `invalid report_to`, `invalid run_budget_usd`, and `run_limit_secs is above [watch] max_run_limit_secs (N)` for a run limit above the setting), `NotFound` `unknown_skill`, and `FailedPrecondition` `too_many_watches` or `daily_budget_spent`. `stop` refuses `NotFound` `unknown_watch` and `FailedPrecondition` `not_running`.
- **`list`** shows every running watch, and every watch that ended today with why it ended, and the day's spend for every watch together.
- **State is kept** in the tool's own database, so a Harold restart resumes a running watch with its end unchanged. Its next run comes at its stored time, or at once if that has passed, never twice for one slot of time.
- **After the Mac sleeps, a run can start up to one `--every` late.** The wait for the next run is a timer of at most `--every`, and the timer does not count the time the Mac slept.

## Settings

`[watch]` in `default.toml` ships the model only. The user keeps the rest in `local.toml`:

```toml
[watch]
model = "haiku"                    # the model every watch runs
# No budget is shipped. Each of these is optional, and enforced only when set:
# daily_budget_usd = 1.00          # every watch together, per local day, first come first served
# run_budget_usd = 0.05            # a run's limit when start gives no --run-budget
# max_run_budget_usd = 0.10        # the most --run-budget may ask
# max_run_limit_secs = 600         # the most --run-limit may ask, in seconds
# skill_folders = ["/Users/you/code/chief-of-pods/skills/watches", "/Users/you/code/your-organisation/skills/watches"]
```

Agents choose a watch's settings within these limits, and only the user changes the limits. Harold refuses to start on an invalid value and names the key, as for every other setting: each budget, when set, is above 0; `max_run_limit_secs`, when set, is above 0; `run_budget_usd` is no more than `max_run_budget_usd` when both are set; each `skill_folders` entry starts with `/` (`~/` is not expanded). `max_run_limit_secs` caps only a run limit that `--run-limit` asks for: a watch started with no `--run-limit` runs with 120 seconds, also when `max_run_limit_secs` is below 120. An absent budget is never treated as 0. With none set, nothing limits a watch's spending but the timeouts, the turn limit and the number of watches.

## Where a watch skill may live

A watch always names its skill by its folder name under `<folder>/.claude/skills/`. The skill's files may live there, or that entry may be a link to a skill kept elsewhere: in a **watch skill folder**, one of the folders the user lists in `[watch] skill_folders`. The chief keeps its standard watch skills in the chief-of-pods repository (`skills/watches/<name>`) and an organisation keeps its own in its organisation repository (`<organisation repository>/skills/watches/<name>`); each is linked into `.claude/skills/`:

```sh
ln -s /Users/you/code/chief-of-pods/skills/watches/pod-watch .claude/skills/pod-watch
```

- **`skill_folders`** is optional and the user's alone, like the budgets: agents cannot change it. With it unset, only `.claude/skills/` counts. Harold does not check at its start that an entry exists, so one Mac can list an organisation repository it has not cloned: a missing folder matches nothing.
- **The check**, at `start` and again at each run: Harold takes the real path of `<folder>/.claude/skills/<skill>/SKILL.md`, with links followed, and the real path of `<folder>/.claude/skills/` and of each listed folder. The skill passes when its real path lies inside one of them, compared whole path part by whole path part, so `/x/watches-old/a` is not inside `/x/watches`. Outside all of them, `start` refuses with `unknown skill <skill>`, and a run fails with `skill_missing`. A listed folder that is itself a link counts as the folder it points to.
- Nothing else about a skill changes: its `allowed-tools`, its text and the run read the same way wherever its files live.

## What a skill lets the model do

A watch skill is an ordinary Claude Code skill. Its frontmatter's `allowed-tools` lists what the model may use, and its text says what to check and what is worth reporting. Harold reads it afresh at each run, so an edit takes effect at the next run.

`allowed-tools` is one line, comma-separated. A YAML list is not read: the run gets no listed tools, and `watch skill tools not read` is logged.

Harold passes `allowed-tools` to Claude Code as `--allowedTools`, exactly as written: no matching, narrowing or dropping. The skill's author decides what a run may do ([ADR 0005](../../adr/0005-a-tool-may-run-a-hosted-model-with-the-tools-a-skill-allows.md)), and a skill may let the model change things: write files, run any command it lists, or reach the network. A skill with no `allowed-tools` leaves the model only the tools Claude Code runs without asking. Under `--print` nothing can ask a person, so any other tool use is refused.

A run may read only its own folder. The frontmatter's `add-dirs` names other folders it may reach: a comma list, each entry trimmed, empty entries dropped, a leading `~` or `~/` replaced by Harold's `HOME`, every other entry kept as written (Claude Code resolves a relative one from the watch's folder). Harold passes one `--add-dir <folder>` per entry and checks nothing else: it does not check that a folder exists and it refuses no entry. A skill with no `add-dirs` runs as before. This widens only what a run may read; where a skill may live is unchanged.

A starting point for a skill:

```markdown
---
name: pod-watch
description: Checks the pods after a release and reports what the chief should know in one line
allowed-tools: Bash(bin/pod slots:*), Bash(git worktree list:*), Bash(tmux capture-pane -p:*)
add-dirs: ~/bin/harold
---
Check every product in Harold's facts. Answer REPORT with the first problem found, when:
- the chief's pane is missing, or any agent has been unknown for over 5 minutes;
- a product has a free slot (bin/pod slots <product>) while one of its specs is approved and not building;
- a build worker has been idle for over 20 minutes, or its input box is not found (a prompt or menu is waiting for a person);
- a spec has been building for over 3 hours.
With no problem, answer REPORT when a spec has merged in the last 10 minutes, naming it.
Otherwise answer QUIET.
```

## One run

At each run Harold does four things in order.

1. **It gathers the facts**, as one JSON document: `now` (local time); `chief` (whether Harold finds the chief's pane); `agents` (for each agent pane its `folder`, `pane` as `session:window.pane`, `provider`, `state`, `seconds_in_state`, `work_summary` cut to 200 characters, and for a Claude Code pane its `input_box`: `empty`, `holds_text` or `not_found`, which is what a permission prompt or a menu looks like); `pods` (for each product its `slots` with `busy`, `free` and `total`, and each spec's `id`, `state` and `seconds_in_state`, from pod stats); and `watch` (`run`, `runs_left`, `previous_outcome`).
2. **It runs the model.** Harold starts Claude Code in the watch's folder, in its own process group, with a cleared environment as activity summaries do (`HOME`, `PATH` and the other few variables the activity summaries keep), and these flags: `--print --safe-mode --no-session-persistence --model <[watch] model> --output-format json`; `--max-budget-usd <run budget>` when the run has a run budget; `--max-turns 8`; one `--add-dir <folder>` for each entry of the skill's `add-dirs`, none when it has none; `--allowedTools` with the skill's `allowed-tools`, left out when it has none; and `--system-prompt` with a fixed text holding the answer rules, with the skill's text after its frontmatter appended. The facts go on stdin. `--safe-mode` keeps the folder's hooks, `CLAUDE.md`, MCP servers and plugins out, so the run fires no hook. A run that has not finished after its run limit, 120 seconds unless `--run-limit` sets another, is stopped: Harold kills its whole process group, so no command it started keeps running.
3. **It reads the answer.** The model must answer with one line: `QUIET`, or `REPORT ` followed by the line to pass on. Harold takes the `result` text and `total_cost_usd` from Claude Code's JSON, keeps the first line, removes control characters and escape sequences, and cuts it to 300 characters. An answer that starts with neither word, or a `REPORT` with nothing after it, is a failed run with reason `unreadable_answer`. The fixed system prompt states this format and nothing about what is worth reporting.
4. **It reports.** A `REPORT` answer's line is typed to the report-to pane. A `QUIET` answer types nothing, unless the watch was started with `--every-run`, which types `quiet` in its place. Either way the outcome, `quiet` or `report`, is stored.

One model runs at a time across all watches. A run that cannot start within 60 seconds of its time, because another watch's run is still going, is skipped (`watch run skipped`, outcome `skipped`).

### `--max-turns`

Claude Code 2.1.304 does not list `--max-turns` in `claude --help`. One run in an empty temporary folder with `--print --max-turns 8 --max-budget-usd 0.01` accepted the flag and ended normally, so Harold passes `--max-turns 8`. (The finding is a constant in `harold/src/tools/watch/runner.rs`, `MAX_TURNS_ACCEPTED`.) Without the flag, the run limit (120 seconds unless `--run-limit` sets another) and a run budget, when there is one, bound the run.

## The lines Harold types

Harold types through `PaneTyping` as `Writer::Tool("watch")`, one line, and only these forms:

```text
WATCH after-release 3/12: harold: 2 of 2 slots free for 25m while watch-jobs is approved
WATCH after-release 5/12: quiet
WATCH after-release ended: 12 runs, 1 report, $0.11
WATCH after-release ended: 10 runs, 2 skipped, 1 report, $0.11
WATCH after-release stopped: daily budget of $1.00 spent
WATCH after-release stopped: 3 runs in a row failed (timeout)
```

The typing makes the same checks as timed prompts: the pane listing works, exactly one agent is in the folder, it is Claude Code, it is idle, and its input box is found and empty. Until those pass, the line waits and Harold tries again every 5 seconds. Only the newest report waits: the next run's report replaces a waiting one, which is then never typed. A report still waiting when the watch ends is dropped, and the closing line takes its place. A closing line is tried for up to an hour.

## Ending and cost

When a watch ends by time, by a spent daily budget, or after 3 failed runs in a row, Harold types one closing line. The closing line of a watch that ended by time counts only the runs that ran, and adds `<n> skipped` only when some were skipped (the second example above); a skipped run still holds its run number, so report lines such as `3/12` are unchanged, and the `watch stopped` log line's `runs` stays the number of run slots used. When a daily budget is set and spent, every running watch stops with its own closing line, and `start` refuses until the next local day, even if Harold restarts meanwhile.

Harold always records what each run cost, and limits spending only where the user or the agent set a budget:

- **Always:** at most 5 watches run at once, one model at a time, and each run is limited to its run limit (120 seconds unless `--run-limit` sets another) and 8 turns.
- **A run budget**, when the run has one (`--run-budget`, else `[watch] run_budget_usd`), goes to Claude Code as `--max-budget-usd`. A run that hits it fails with reason `run_budget_reached`.
- **A daily budget**, when `[watch] daily_budget_usd` is set: before each run Harold checks that the day's total, plus that run's budget if it has one, stays within it. If it would not, every watch stops. With no run budget the check uses the day's total alone, so the last run of the day can pass the budget by its own cost.
- **What a run counts:** its `total_cost_usd`. A run that timed out, was killed or gave no readable cost counts at its run budget when it has one; otherwise its cost is unknown, stored as empty and left out of the day's total.

As a guide, a Haiku run that reads the skill and the facts and runs two or three commands costs about 1 to 2 US cents.

Three runs in a row that fail stop the watch: `process_failed` (Claude Code missing, logged out or exiting with an error; the `watch run failed` line then gives `exit_code` or `signal`, and `subtype`, `is_error`, `num_turns`, `terminal_reason` and `api_error_status` from Claude Code's output; a model name that does not exist shows as `terminal_reason` `api_error` with `api_error_status` 404), `max_turns_reached` (Claude Code stopped at its 8-turn cap: the skill asks for too much, so trim it; the line gives the same cause fields), `timeout`, `run_budget_reached`, `unreadable_answer` and `skill_missing` (the skill is gone, or its real path left every allowed folder). A skipped run neither fails nor resets the count.

## Risks

- **A skill may let the model change things.** This is the user's choice. Harold does not check `allowed-tools` or `add-dirs`. A skill that lists `Bash`, `Write`, `Edit` or a network tool lets a watch run, every few minutes and without a person watching, any command or edit those allow, in the watch's folder and anywhere those tools reach. Any agent can start a watch for a skill in its own folder, and can change the skill while the watch runs. `add-dirs` widens what a run may read to any folder the skill names, and with `Write` or `Edit` in `allowed-tools` what it may change. What stands between a watch and a harmful change is the skill's author, not Harold.
- **A watch skill folder is shared trust.** Any agent can link a skill from a listed folder into its own `.claude/skills/` and run it, and whoever can change the chief-of-pods repository or an organisation repository changes what every watch that links there does at its next run. List only folders whose authors you trust.
- **Text from other agents can steer a run.** The facts hold every agent's work summary, and a run may read files and, with a command such as `tmux capture-pane -p`, any pane's screen. A steered run can do whatever its skill's `allowed-tools` allows. Its typed output is still one line of at most 300 characters, as a `WATCH` line into the chief's pane or the watch's own folder. The chief reads a `WATCH` line as a report, not as the user's request.
- **What reaches a hosted model.** The facts, the skill's text, the files the run reads and the output of the commands it runs go to Anthropic's hosted model, as activity summaries' text does today.
- **No budget by default.** A watch's spending is bounded only by its time, turns and the number of watches: up to 5 watches, one run at a time, each run up to its run limit (120 seconds unless `--run-limit` sets another), all day. The `watch.cost_usd` counter and `list` show what was spent, and the `watch-cost-high` alert fires when a day passes $1.00.
- **One shared budget, when set.** One agent's watches can spend the whole day's budget and stop the chief's watch.

## What is logged

No line carries any of the skill's text, its `allowed-tools`, the paths in its `add-dirs`, the facts, the commands' output, the answer or the report.

| Moment | Level | Line | Fields |
|---|---|---|---|
| A watch starts, is replaced or resumes | info | `watch started` | `watch`, `folder`, `skill`, `ends_at`, `every_minutes`, `every_run`, `how` (`started`, `replaced` or `resumed`), `model` (the `[watch] model` every run passes) |
| A run ends | info | `watch run finished` | `watch`, `run`, `outcome` (`quiet` or `report`), `cost_usd`, `duration_ms`, `wait_ms` (the wait for the model), `run_ms` (the Claude Code run), `add_dirs` |
| A run fails | warn | `watch run failed` | `watch`, `run`, `reason` (`timeout`, `process_failed`, `run_budget_reached`, `unreadable_answer`, `skill_missing`, `max_turns_reached`), `cost_usd`, `wait_ms`, `run_ms`, `add_dirs`; for a `process_failed` or `max_turns_reached` run that exited, also those of `exit_code`, `signal`, `subtype`, `is_error`, `num_turns`, `terminal_reason` and `api_error_status` that are known (`subtype` is `unreadable_json` when the output was not JSON, and `other` when it or `terminal_reason` was not a short name; `api_error_status` is a whole number from 100 to 599) |
| A run is skipped | warn | `watch run skipped` | `watch`, `run`, `reason` (`model_busy`) |
| A skill's `allowed-tools` is a YAML list, at each run | warn | `watch skill tools not read` | `watch`, `reason` (`yaml_list`) |
| A report waits, logged when its reason changes | info | `watch report waiting` | `watch`, `run`, `reason` (the timed-prompt reasons, such as `agent_busy`) |
| A waiting report is replaced or dropped | info | `watch report replaced` | `watch`, `run`, `why` (`newer_report` or `watch_ended`) |
| A report or closing line is typed | info | `watch report typed` | `watch`, `run`, `pane`, `insert_pressed` |
| A watch ends | info | `watch stopped` | `watch`, `reason` (`ended`, `stopped`, `daily_budget_spent`, `runs_failing`), `runs`, `cost_usd` |

`watch run finished`, `watch run failed` and `watch run skipped` are written inside the run's `harold.watch.run` span, so they carry its trace id. Counted from 0 at a watch's start, for HyperDX:

- `watch.runs`, by `folder` and `outcome` (`quiet`, `report`, `skipped`, or a failure reason). The watch's name is chosen by an agent, so it is never a counter attribute, only a log field and a span attribute.
- `watch.cost_usd`, by `folder`.
- The span `harold.watch.run`, with `watch`, `run`, `outcome`, `reason` and `cost_usd`, and its child `harold.watch.model` with `result` (`exited`, `killed` or `failed_to_start`).

`watch`, `run` and `cost_usd` are on the telemetry reference's allowed fields. The folder stays out of spans and log exports. The [telemetry reference](../telemetry/README.md) has the `watch-cost-high` alert (severity warn, rarely) and the panel "Watch runs: cost and duration per run".

To find out why a run is slow, read the two run lines: a large `wait_ms` means another watch held the model, and a large `run_ms` means the Claude Code start or the skill was slow. To find out why runs fail, filter `harold.watch.run` spans by a failure `outcome`: `watch` and `run` name the run, `reason` the failure, and the `harold.watch.model` child's `result` and duration tell a timeout (`killed` at the watch's run limit, 120 seconds unless `--run-limit` sets another) from a process that did not start (`failed_to_start`: Claude Code is missing or logged out). For `unreadable_answer` the answer is not logged, so run the skill by hand with the same facts.

`add_dirs` is the number of `add-dirs` folders the run was given (0 when the skill names none, or could not be read), never the paths. It goes to `harold.log` only, not to the exported fields. Beside a report about a file the run could not read, `add_dirs=0` means the skill names no folder, and above 0 means it names a folder, but not the right one.

## Data

The tool's database is `<store folder>/tools/watch/watch.db` (Turso), with the tables `watches` and `runs` (`001_watches.sql`). `runs` holds each run's watch, folder, run number, start, outcome and cost (empty when unknown); the day's spend is summed from it. `watches` also keeps when a watch ended, for `list`.

## Rollback

An older Harold ignores `tools/watch/`. Deleting the folder empties the tool.
