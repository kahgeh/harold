# harold-lifecycle: Claude Code mod

Tells Harold whether a Claude Code pane is working, by running `harold accept`. A pane is
working while its main session's turn is running, or while one of its subagents is `pending`,
`running` or `waiting`. The mod only watches: each hook passes its event on unchanged.

| When | Condition | Runs `harold accept` with | Harold sets |
|---|---|---|---|
| A main-session turn starts (`turn.start`, no `agentId`) | Always | `turn-started` | Busy |
| Every 10 seconds | The pane is working | `still-working` | Busy |
| A main-session turn ends (`turn.complete`, no `agentId`) | No subagent is working | `turn-completed` | Idle |
| A main-session turn ends | A subagent is working | `still-working` | Busy |
| 3 seconds after a subagent's turn ends (`turn.complete` with an `agentId`), or when a 10-second check runs | No main turn is running, and no subagent is working | `subagents-finished` | Idle |

Every run has the form `harold accept <event> --pane <id> --adapter claude.lifecycle`. The pane
is the `TMUX_PANE` environment variable. Unless it is `%` plus digits, the mod sends nothing.

## Subagents

The mod reads the subagents' states with `$.agent.list()` when a main turn ends, when a
subagent's turn ends, and at each 10-second check. If the call fails, the mod counts no subagent
as working, so a failing call cannot keep a pane busy.

The 10-second check runs from the start of a main turn until the mod reports the pane idle, on
one timer. The repeat tells Harold the mod is still there: Harold lets a busy report stand
against its screen reading for `agent_monitor.report_grace_ms` (30 seconds), so the mod has to
miss three reports in a row before the screen reading takes over. The check also notices a
subagent that ended without an event reaching the mod. A subagent's turn that ends after the
pane was reported idle sends nothing. The mod keeps one 3-second wait: each subagent end restarts it, and a 10-second check that runs
inside it still reports busy but never idle. The wait lets a
main-session turn that Claude Code starts at once for that subagent's result begin first, so the
pane is not reported idle in between.

## Finding the program

`../../harold` from the mod's folder, which is the installed binary once the installer has
copied the mod to `~/bin/harold/hooks/claude-mod/`. Set `HAROLD_BIN` to use another
program, for a mod run from a checkout.

## Waiting and ordering

Each hook starts its report, calls `next(e)`, then waits for the report before returning.
`turn.start` fires before the first model call, so the turn most likely waits for the report:
a few milliseconds normally. One promise chain carries all four events in the order they were raised, and a report that had to wait
behind one that failed or timed out is skipped (while the pane is still working, the next 10-second check sends its state again; an idle report that is skipped is corrected by Harold's screen reading after `agent_monitor.report_grace_ms`). A dead or
hung Harold therefore costs a turn at most the one bounded call: `harold accept` gives up
after 3 seconds and the mod after 4. Every failure is swallowed and nothing is shown to the
person.

## Known gap

Claude Code passes the settings `env` block to child processes, so a `claude -p` run from a
Bash tool inside a pane loads this mod and inherits that pane's `TMUX_PANE`. When the nested
run ends, it reports the parent pane idle while the parent may still be busy. The parent's next
`still-working`, at most 10 seconds later, corrects it.

## Loading it

The installer copies the mod and adds its folder to `CLAUDE_CODE_PLUGIN_DIRS` in the `env`
block of `~/.claude/settings.json`. For one session, or from a checkout:

```
claude --plugin-dir hooks/claude-mod
```

Mods are early access: a Claude Code release can stop this one loading. Harold then reads
the screen: at once for a session that never had the mod, and after
`agent_monitor.report_grace_ms` (30 seconds) for a pane whose mod stopped while it was busy.

## Tests

```
claude plugin validate hooks/claude-mod
claude plugin test hooks/claude-mod
```

CI has no `claude` binary, so it does not run them.
