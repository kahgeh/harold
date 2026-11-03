# Watch Tool

A **watch** is a schedule an agent asks Harold for: run one of its skills with a model every few minutes, for a set time or with no end, and report to the chief or to the agent itself. The tool also holds one-time reminders, which run no model. It is not the agent monitor's watching of agents (the glossary's "Watching agents"), and not the `WatchAgentStates` stream.

The watch tool is a [tool](README.md) that holds the watches, runs them and types their reports. The service is `harold.tools.watch.v1.WatchService`. It trusts local callers (ADR 0002): there is no token, and it checks every field itself. The proto is `harold-api/proto/tools/watch.proto`.

## Making, listing, pausing and stopping a watch

```sh
harold tool watch start after-release --skill pod-watch --every 10 --for 120
#   started after-release in /Users/you/code/chief-of-pods until 20:45, every 10 minutes, reporting to chief
harold tool watch start after-release --skill pod-watch --every 5 --for 60 --report-to chief --run-budget 0.03 --every-run
harold tool watch start pods --skill pod-watch --every 10 --report-to chief
#   started pods in /Users/you/code/chief-of-pods with no end, every 10 minutes, reporting to chief
harold tool watch pause pods --for 2h       # paused pods for 120 minutes
harold tool watch resume pods               # resumed pods
harold tool watch pause --all               # paused 2
harold tool watch resume --all              # resumed 1, skipped pods (budget)
harold tool watch remind release-check --in 45m --report-to chief "check the v0.75.0 release"
#   reminder release-check at 15:45
harold tool watch stop after-release        # stopped after-release
harold tool watch list                      # after-release running in .../chief-of-pods until 20:45, limit 120 s, 3 runs, 1 report, $0.03
```

Each command prints one line. `list` prints one line for each watch and one for each waiting reminder, then `spent today $…`. Any agent can run them.

| Flag | Meaning | Valid values |
|---|---|---|
| `<name>` | Names the watch in the commands, the reports and the log | 1 to 64 characters from `A-Z`, `a-z`, `0-9`, `-` and `_` |
| `--skill` | The skill to run, by its folder name under `<folder>/.claude/skills/` | The same characters as a name, so no `/`, `.` or `..`. `<folder>/.claude/skills/<skill>/SKILL.md` must exist, and its real path, with links followed, must lie inside `<folder>/.claude/skills/` or inside a folder in `[watch] skill_folders` |
| `--every` | Minutes from the start of one run to the start of the next | 2 to 240 |
| `--for` | How long the watch runs from now. Left out, the watch has no end (an open-ended watch) | A time, 1 minute to 24 hours (see Time forms) |
| `--report-to` | Who gets the reports | `chief`, or `self` for the watch's own folder. `self` when left out. No other folder |
| `--run-budget` | The most one run may spend, in US dollars | 0.01 or more, and no more than `[watch] max_run_budget_usd` when that is set. Left out, `[watch] run_budget_usd` when set, else no run limit |
| `--run-limit` | The most seconds one run may take before Harold stops it (the run limit) | A whole number above 0, and no more than `[watch] max_run_limit_secs` when that is set. Left out, 120. Runs take the model one at a time, and a run waits at most 1 minute for it, then is skipped: a long run limit on one watch can make the runs of other watches wait, and skip |
| `--every-run` | Type a line after every run, not only when the run answers `REPORT` | Present or left out. Left out, a `QUIET` run types nothing |

- **The folder** is the working directory of the `harold tool watch start` command. The CLI sends it, and the tool does not trust it: it must be an absolute path that exists and is a folder, and Harold stores its canonical form, with links followed and no trailing slash. Otherwise `start` refuses with `invalid folder`.
- **`start`** runs the first check at once, then one every `--every` minutes until the `--for` time has passed. With no `--for`, it runs until `watch stop`. `start` with the name of a running or paused watch replaces it: its settings, its end and its run count start over.
- **`stop`** ends a running or paused watch at once. A run in progress is stopped and its report is not typed. No closing line is typed, since an agent asked for it. `stop` also cancels a waiting reminder of that name.
- **A watch also ends** when its time is up (a watch with an end), when a daily budget is set and spent (a watch with an end), or after 3 failed runs in a row. An open-ended watch never ends by time.
- **`start` refuses** with `unknown skill <skill>`, `too many watches` (5 are running or paused), `daily budget spent`, `daily budget not set`, `name <name> is in use`, and `invalid <flag>` for a value outside the table. At the service these are `InvalidArgument` `invalid <field>` (`invalid name`, `invalid folder`, `invalid skill`, `invalid every_minutes`, `invalid for_minutes`, `invalid report_to`, `invalid run_budget_usd`, and `run_limit_secs is above [watch] max_run_limit_secs (N)` for a run limit above the setting), `NotFound` `unknown_skill`, and `FailedPrecondition` `too_many_watches`, `daily_budget_spent`, `daily_budget_unset` or `name_in_use`. `stop` refuses `NotFound` `unknown_watch` and `FailedPrecondition` `not_running`.
- **`list`** shows every running or paused watch, every watch that ended today with why it ended, every waiting reminder, and the day's spend for every watch together. The forms:

```text
pods running in /Users/you/code/chief-of-pods no end, limit 120 s, 14 runs, 2 reports, $0.06
after-release paused until 16:30 in /Users/you/code/chief-of-pods, 3 runs, 1 report, $0.03
after-release paused in /Users/you/code/chief-of-pods, 3 runs, 1 report, $0.03
pods paused (budget) until 00:00 in /Users/you/code/chief-of-pods, 14 runs, 2 reports, $1.00
after-release ended (time was up) in /Users/you/code/chief-of-pods, 12 runs, 1 report, $0.11
reminder release-check due 15:45, reporting to chief
spent today $1.03
```

The reason in `ended (…)` is `time was up`, `stopped`, `daily budget spent` or `3 runs in a row failed`.
- **State is kept** in the tool's own database, so a Harold restart resumes a running or paused watch with its end unchanged. Its next run comes at its stored time, or at once if that has passed, never twice for one slot of time.
- **After the Mac sleeps, a run can start up to one `--every` late.** The wait for the next run is a timer of at most `--every`, and the timer does not count the time the Mac slept.

## Time forms

A time is a whole number of minutes (`90`), or a number with `m` (`90m`) or `h` (`2h`). The `h` form means hours. The service accepts 1 minute to 24 hours (1 to 1440 minutes) for `start --for`, `pause --for` and `remind --in`, and refuses a value outside that range with `invalid for_minutes` or `invalid in_minutes`. A value that is not a time in these forms is refused at once with the same `invalid <field>` line. `remind --at` takes `HH:MM` on the 24-hour clock.

## Open-ended watches

A watch started with no `--for` is an **open-ended watch**: it runs until `watch stop` or after 3 failed runs in a row. Time never ends it. A Harold restart does not end it.

- **A daily budget is required.** `start` with no `--for` is refused with `daily budget not set` (`FailedPrecondition` `daily_budget_unset`) unless `[watch] daily_budget_usd` is set. So an open-ended watch always has a daily cap.
- **The start line** ends with `with no end`. `list` shows `running … no end`.
- **The facts** hold no `runs_left` for it. The report line has no total (see The lines Harold types).

## Pause and resume

```sh
harold tool watch pause pods                 # paused pods
harold tool watch pause pods --for 2h        # paused pods for 120 minutes
harold tool watch pause --all --for 30m      # paused 3 for 30 minutes
harold tool watch resume pods                # resumed pods
harold tool watch resume --all               # resumed 2
```

- **`pause <name>` or `pause --all`**, with an optional `--for <time>`. Give a name or `--all`, not both. A paused watch runs nothing and spends nothing. A run in progress is stopped and its report is not typed. A waiting report is dropped.
- **`resume <name>` or `resume --all`.** A resume, by the command or at the end of `--for`, runs the first check at once and keeps the run count. A paused watch builds up no missed runs: its resume runs once, and nothing else catches up.
- **A pause does not move the end of a watch.** A watch with an end that passes while the watch is paused ends as before, with its closing line.
- **A pause survives a Harold restart.** A `--for` pause that ended during the restart resumes at once.
- **Pause and resume type no line**, because a person or an agent asked for it.
- **`pause` refuses** `unknown_watch`, `not_running` and `budget_day`. **`resume` refuses** `unknown_watch`, `not_paused` and `budget_day`. At the CLI these print `unknown watch <name>`, `<name> is not running`, `<name> is not paused` and `<name> is capped by the daily budget`.
- **The count line of `--all`** is `paused N` or `resumed N`, and when a capped watch was skipped, `, skipped <names> (budget)`.
- **The CLI prints what the daemon changed.** `paused <name>` and `resumed <name>` come only when the daemon names the watch in its answer. Otherwise the line is `paused 0` or `resumed 0`.
- **The service's `caller`** is `cli` or `control_tower`. It goes to the log only (`how` in `watch paused` and `watch resumed`). It does not change what the call does. A pause or resume from a timer logs `timed`.

### The capped watch

When a daily budget is spent, a watch with an end stops, as before. An open-ended watch becomes a **capped watch** instead: paused, with `how` = `budget_day`, until the start of the next local day, when it resumes by itself with a first check at once.

- **A person never resumes a capped watch by hand**, so nothing overrides the daily budget. `resume <name>` on it refuses `budget_day`. `pause <name>` on it refuses `budget_day` too.
- **`resume --all` skips it and names it**: `resumed 1, skipped pods (budget)`. `pause --all` skips it too.
- **Harold types one line** when the watch is capped (see The lines Harold types). `list` shows `paused (budget) until 00:00`.
- **If the first run of the new day could pass the budget again**, the same check pauses the watch until the day after.
- **`start` is refused** with `daily budget spent`, for any name, while a capped watch holds.
- **Limit: the day of a daylight-saving change.** The tool takes each local day as 24 hours. On the day of a change, the day boundary of the daily budget is one hour off before or after the change. A watch that caps in the first hours of that day can resume one hour early or late. From the next day, the boundary is right.

## Reminders

A **watch reminder** is a one-time line that Harold types at a set time. It runs no model and costs nothing.

```sh
harold tool watch remind release-check --in 45m --report-to chief "check the v0.75.0 release"
#   reminder release-check at 15:45
harold tool watch remind standup --at 09:30 -- "-3 specs wait"
```

| Flag | Meaning | Valid values |
|---|---|---|
| `<name>` | Names the reminder in `list`, the line and the log. A reminder shares the name space of the watches | The same characters as a watch name |
| `--in` | The wait before the line is typed | A time, 1 minute to 24 hours |
| `--at` | The next time the local clock reads this time, today or tomorrow | `HH:MM`, 24 hours |
| `--report-to` | Who gets the line | `chief`, or `self` for the folder where the command ran. `self` when left out |
| `"<text>"` | The text of the reminder | At least one visible character. Harold removes control characters, escape sequences and Unicode format characters (for example a right-to-left override), and cuts it to 300 characters |

- **Give `--in` or `--at`, not both.** A text that starts with `-` goes after `--`.
- **The output** is `reminder <name> at HH:MM`, the due time in local time.
- **Harold types** `REMIND <name>: <text>` once, with the checks of the watch reports (see The lines Harold types).
- **A missed reminder.** A reminder that is not typed within 120 minutes of its time is dropped and logged (`reminder ended`, `reason` = `dropped`). This is the give-up time of the timed prompts. After a sleep or a Harold restart inside those 120 minutes, Harold types it once, late. It never types a reminder twice.
- **`watch stop <name>`** cancels a waiting reminder. `list` shows each waiting reminder as `reminder <name> due HH:MM, reporting to <chief|self>`.
- **A name in use** is refused with `name <name> is in use` (`FailedPrecondition` `name_in_use`), when a running or paused watch or a waiting reminder has the name. `start` with the name of a waiting reminder is refused the same way.
- **A `--at` reminder keeps its local date and time.** It is due when the local clock reaches it. A change of the local offset moves it with the clock. A time in a spring-forward gap is due at the first instant after the gap. A time that occurs twice is due at its first occurrence.

## The clock

The watch tool reads the local offset from `/bin/date` (`+%Y-%m-%d %H:%M:%S %z`) at each use, as the timed prompts do. It does not read it once at the start of Harold. After a daylight-saving change, the day of the daily budget, the clock times in `list` and the due time of a `--at` reminder are right with no restart. When `/bin/date` fails, the tool uses the last offset that it read.

Tokio's clock stops while the Mac sleeps, and the wall clock does not. So a waiting watch wakes at least once a minute and reads the wall clock again. After a sleep, a run time, the end of a pause, the next day of a capped watch or the end of a watch that passed during the sleep takes effect within a minute of the wake. A waiting reminder wakes the same way.

## Settings

`[watch]` in `default.toml` ships the model only. An open-ended watch also needs `daily_budget_usd`. The user keeps the rest in `local.toml`:

```toml
[watch]
model = "haiku"                    # the model every watch runs
# No budget is shipped. Each of these is optional, and enforced only when set:
# daily_budget_usd = 1.00          # every watch together, per local day, first come first served. Must be set to start a watch with no --for
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
WATCH pods 14: harold: 2 of 2 slots free for 25m while watch-jobs is approved
WATCH after-release ended: 12 runs, 1 report, $0.11
WATCH after-release ended: 10 runs, 2 skipped, 1 report, $0.11
WATCH after-release stopped: daily budget of $1.00 spent
WATCH after-release stopped: 3 runs in a row failed (timeout)
WATCH pods paused: daily budget of $1.00 spent, resumes at 00:00
REMIND release-check: check the v0.75.0 release
```

- **The report line of an open-ended watch** is `WATCH <name> <run>: <text>`, with the run number and no total (the third line above).
- **The paused-budget line** is Harold's own fixed text. Harold types it once, when the daily budget caps an open-ended watch. `resumes at` is the start of the next local day.
- **The `REMIND` line** carries the text that an agent gave to `remind`. The fixed prefix `REMIND <name>: ` shows the chief that an agent wrote the `REMIND` line. It is not a request from the user. Harold cuts the text to 300 characters and removes control characters and escape sequences. ADR 0002 covers this: Harold trusts local callers.

The typing makes the same checks as timed prompts: the pane listing works, exactly one agent is in the folder, it is Claude Code, it is idle, and its input box is found and empty. Until those pass, the line waits and Harold tries again every 5 seconds. Only the newest report waits: the next run's report replaces a waiting one, which is then never typed. A report still waiting when the watch ends is dropped, and the closing line takes its place. A closing line is tried for up to an hour. A `REMIND` line is tried until 120 minutes after its time.

## Ending and cost

When a watch with an end ends by time, by a spent daily budget, or after 3 failed runs in a row, Harold types one closing line. An open-ended watch ends only after 3 failed runs in a row, or by `watch stop`. The closing line of a watch that ended by time counts only the runs that ran, and adds `<n> skipped` only when some were skipped (the second example above); a skipped run still holds its run number, so report lines such as `3/12` are unchanged, and the `watch stopped` log line's `runs` stays the number of run slots used. When a daily budget is set and spent, every running watch with an end stops with its own closing line, every open-ended watch becomes a capped watch, and `start` refuses until the next local day, even if Harold restarts meanwhile.

Harold always records what each run cost, and limits spending only where the user or the agent set a budget:

- **Always:** at most 5 watches run at once, one model at a time, and each run is limited to its run limit (120 seconds unless `--run-limit` sets another) and 8 turns.
- **A run budget**, when the run has one (`--run-budget`, else `[watch] run_budget_usd`), goes to Claude Code as `--max-budget-usd`. A run that hits it fails with reason `run_budget_reached`.
- **A daily budget**, when `[watch] daily_budget_usd` is set: before each run Harold checks that the day's total, plus that run's budget if it has one, stays within it. If it would not, every watch with an end stops and every open-ended watch is capped until the next local day. With no run budget the check uses the day's total alone, so the last run of the day can pass the budget by its own cost.
- **What a run counts:** its `total_cost_usd`. A run that timed out, was killed or gave no readable cost counts at its run budget when it has one; otherwise its cost is unknown, stored as empty and left out of the day's total.

As a guide, a Haiku run that reads the skill and the facts and runs two or three commands costs about 1 to 2 US cents.

Three runs in a row that fail stop the watch: `process_failed` (Claude Code missing, logged out or exiting with an error; the `watch run failed` line then gives `exit_code` or `signal`, and `subtype`, `is_error`, `num_turns`, `terminal_reason` and `api_error_status` from Claude Code's output; a model name that does not exist shows as `terminal_reason` `api_error` with `api_error_status` 404), `max_turns_reached` (Claude Code stopped at its 8-turn cap: the skill asks for too much, so trim it; the line gives the same cause fields), `timeout`, `run_budget_reached`, `unreadable_answer` and `skill_missing` (the skill is gone, or its real path left every allowed folder). A skipped run neither fails nor resets the count.

## Risks

- **A skill may let the model change things.** This is the user's choice. Harold does not check `allowed-tools` or `add-dirs`. A skill that lists `Bash`, `Write`, `Edit` or a network tool lets a watch run, every few minutes and without a person watching, any command or edit those allow, in the watch's folder and anywhere those tools reach. Any agent can start a watch for a skill in its own folder, and can change the skill while the watch runs. `add-dirs` widens what a run may read to any folder the skill names, and with `Write` or `Edit` in `allowed-tools` what it may change. What stands between a watch and a harmful change is the skill's author, not Harold.
- **A watch skill folder is shared trust.** Any agent can link a skill from a listed folder into its own `.claude/skills/` and run it, and whoever can change the chief-of-pods repository or an organisation repository changes what every watch that links there does at its next run. List only folders whose authors you trust.
- **Text from other agents can steer a run.** The facts hold every agent's work summary, and a run may read files and, with a command such as `tmux capture-pane -p`, any pane's screen. A steered run can do whatever its skill's `allowed-tools` allows. Its typed output is still one line of at most 300 characters, as a `WATCH` line into the chief's pane or the watch's own folder. The chief reads a `WATCH` line as a report, not as the user's request.
- **What reaches a hosted model.** The facts, the skill's text, the files the run reads and the output of the commands it runs go to Anthropic's hosted model, as activity summaries' text does today.
- **No budget by default.** A watch's spending is bounded only by its time, turns and the number of watches: up to 5 watches, one run at a time, each run up to its run limit (120 seconds unless `--run-limit` sets another), all day. The `watch.cost_usd` counter and `list` show what was spent, and the `watch-cost-high` alert fires when a day passes $1.00.
- **A `REMIND` line is text from an agent in the chief's pane.** A local agent can make Harold type up to 300 characters of its own text with `remind`. ADR 0002 covers this, as for every tool service. The limits are the fixed `REMIND <name>: ` prefix and the cut to 300 characters with control characters removed. The chief reads a `REMIND` line as a reminder from an agent, not as a request from the user.
- **One shared budget, when set.** One agent's watches can spend the whole day's budget and stop the chief's watch.

## What is logged

No line carries the text of a reminder or any of the skill's text, its `allowed-tools`, the paths in its `add-dirs`, the facts, the commands' output, the answer or the report.

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
| A watch is paused | info | `watch paused` | `watch`, `how` (`cli`, `control_tower`, `timed` or `budget_day`), `paused_until` (local time; left out when the pause has no end) |
| A watch resumes from a pause | info | `watch resumed` | `watch`, `how` (as for `watch paused`) |
| The store fails on a pause or a resume of `--all`, at the timed end of a pause, or at a cap | warn | `watch not paused` or `watch not resumed` | `watch`, `reason` (`store_failed`). A timed resume tries again after 5 seconds |
| A watch's task cannot read its row from the store | warn | `watch store read failed` | `watch`, `reason` (`store_failed`). The task tries again after 5 seconds |
| A reminder is set | info | `reminder set` | `reminder`, `due_at` |
| A reminder is typed | info | `reminder typed` | `reminder`, `late_s` |
| A reminder is dropped or cancelled | info | `reminder ended` | `reminder`, `reason` (`dropped` or `cancelled`) |
| A watch ends | info | `watch stopped` | `watch`, `reason` (`ended`, `stopped`, `daily_budget_spent`, `runs_failing`), `runs`, `cost_usd` |

`watch run finished`, `watch run failed` and `watch run skipped` are written inside the run's `harold.watch.run` span, so they carry its trace id. Counted from 0 at a watch's start, for HyperDX:

- `watch.runs`, by `folder` and `outcome` (`quiet`, `report`, `skipped`, or a failure reason). The watch's name is chosen by an agent, so it is never a counter attribute, only a log field and a span attribute.
- `watch.cost_usd`, by `folder`.
- The span `harold.watch.run`, with `watch`, `run`, `outcome`, `reason` and `cost_usd`, and its child `harold.watch.model` with `result` (`exited`, `killed` or `failed_to_start`).

`watch started` for an open-ended watch logs `ends_at="none"`. A reminder writes the lines of the report slot too (`watch report waiting`, `watch report typed`), with its name in `watch` and `run` 0. Counted from 0: `watch.reminders`, by `outcome` (`typed`, `dropped`, `cancelled`). `watch`, `run` and `cost_usd` are on the telemetry reference's allowed fields, and so are `reminder`, `paused_until`, `how` and `late_s`. The folder stays out of spans and log exports. The [telemetry reference](../telemetry/README.md) has the `watch-cost-high` alert (severity warn, rarely) and the panel "Watch runs: cost and duration per run".

To find out why a run is slow, read the two run lines: a large `wait_ms` means another watch held the model, and a large `run_ms` means the Claude Code start or the skill was slow. To find out why runs fail, filter `harold.watch.run` spans by a failure `outcome`: `watch` and `run` name the run, `reason` the failure, and the `harold.watch.model` child's `result` and duration tell a timeout (`killed` at the watch's run limit, 120 seconds unless `--run-limit` sets another) from a process that did not start (`failed_to_start`: Claude Code is missing or logged out). For `unreadable_answer` the answer is not logged, so run the skill by hand with the same facts.

`add_dirs` is the number of `add-dirs` folders the run was given (0 when the skill names none, or could not be read), never the paths. It goes to `harold.log` only, not to the exported fields. Beside a report about a file the run could not read, `add_dirs=0` means the skill names no folder, and above 0 means it names a folder, but not the right one.

## Data

The tool's database is `<store folder>/tools/watch/watch.db` (Turso), with the tables `watches`, `runs` and `reminders` (`001_watches.sql`, `002_run_limit.sql`, `003_controls.sql`). `runs` holds each run's watch, folder, run number, start, outcome and cost (empty when unknown); the day's spend is summed from it. `watches` also keeps when a watch ended, for `list`.

Migration `003_controls.sql` rebuilds `watches`, because a Turso table cannot change a `CHECK` or `NOT NULL` in place. In the new table:

- `ends_at_ms` may be empty (an open-ended watch).
- `state` takes `running`, `paused` and `stopped`.
- `paused_until_ms` holds the end of a pause (empty when the pause has no end).
- `pause_how` holds who paused the watch: `cli`, `control_tower` or `budget_day`.

The migration copies every row of the old table, with the old column names. The new table `reminders` holds `name`, `folder`, `report_to`, `text`, `due_at_ms` (for `--in`), `due_local` (for `--at`, a local date and time), `state` (`waiting`, `typed`, `dropped` or `cancelled`), `created_at_ms` and `ended_at_ms`.

## The gRPC contract

`harold.tools.watch.v1.WatchService` in `harold-api/proto/tools/watch.proto`. All calls are on loopback and trust local callers (ADR 0002).

| RPC | Request | Response |
|---|---|---|
| `StartWatch` | `name`, `folder`, `skill`, `every_minutes`, optional `for_minutes` (absent: no end), `report_to`, `run_budget_usd`, `every_run`, `run_limit_secs` | `name`, optional `ends_at_ms` (absent: no end) |
| `StopWatch` | `name` | empty |
| `ListWatches` | empty | `watches` (`Watch`), `spent_usd`, `reminders` (`Reminder`: `name`, `report_to`, `due_at_ms`) |
| `Pause` | `name`, `all`, optional `for_minutes`, `caller` (`cli`, `control_tower`; empty means `cli`) | `paused`, `skipped` (names) |
| `Resume` | `name`, `all`, `caller` | `resumed`, `skipped` (the capped watches) |
| `Remind` | `name`, `folder`, `in_minutes` or `at_local`, `report_to`, `text` | `due_at_ms` |

`Watch` gains `paused`, optional `paused_until_ms`, `pause_how` and optional `ends_at_ms`. Refusals: `InvalidArgument` `invalid <field>`; `NotFound` `unknown_skill` and `unknown_watch`; `FailedPrecondition` `too_many_watches`, `daily_budget_spent`, `daily_budget_unset`, `not_running`, `not_paused`, `budget_day` and `name_in_use`; `Unavailable` when the database write fails.

## Rollback

An older Harold does not know migration `003_controls`. When it opens a `watch.db` that has this migration, it refuses the database. Its watch tool does not start, and Harold logs `tool failed to start` with `reason` = `store_open_failed`. The other tools work.

To use the watch tool with an older Harold:

1. Stop Harold.
2. Move `<store folder>/tools/watch/watch.db` (and its `-wal` and `-shm` files, if they are there) to a different folder.
3. Start the older Harold. It makes a new, empty `watch.db`.

This loses the watch history, the day's spend and the waiting reminders. To go back to the new Harold, stop Harold and move the old files back.
