# harold-lifecycle: Claude Code mod

Tells Harold when a Claude Code pane starts and stops working, by running
`harold accept` at the start and end of each main-session turn. It only watches: each hook
passes its event on unchanged.

| Mod event | Condition | Runs |
|---|---|---|
| `turn.start` | no `agentId` | `harold accept turn-started --pane <id> --adapter claude.lifecycle` |
| `turn.complete` | no `agentId` | `harold accept turn-completed --pane <id> --adapter claude.lifecycle` |
| `turn.complete` | has an `agentId` (a subagent) | nothing |

The pane is the `TMUX_PANE` environment variable. Unless it is `%` plus digits, the mod
sends nothing.

## Finding the program

`../../harold` from the mod's folder, which is the installed binary once the installer has
copied the mod to `~/bin/harold/hooks/claude-mod/`. Set `HAROLD_BIN` to use another
program, for a mod run from a checkout.

## Waiting and ordering

Each hook starts its report, calls `next(e)`, then waits for the report before returning.
The report therefore overlaps the turn's own start, one promise chain keeps reports in the
order the events fired, and a dead or hung Harold costs a turn at most the one bounded call:
`harold accept` gives up after 3 seconds and the mod after 4. Every failure is swallowed and
nothing is shown to the person.

## Loading it

The installer copies the mod and adds its folder to `CLAUDE_CODE_PLUGIN_DIRS` in the `env`
block of `~/.claude/settings.json`. For one session, or from a checkout:

```
claude --plugin-dir hooks/claude-mod
```

Mods are early access: a Claude Code release can stop this one loading. Harold then falls
back to reading the screen, as it does for any session without the mod.

## Tests

```
claude plugin validate hooks/claude-mod
claude plugin test hooks/claude-mod
```

CI has no `claude` binary, so it does not run them.
