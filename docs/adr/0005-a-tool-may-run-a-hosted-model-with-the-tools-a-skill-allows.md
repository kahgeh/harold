# 5. A tool may run a hosted model with the tools a skill allows

Date: 2026-10-20
Status: Accepted (watch-jobs)

## Context

Until now Harold ran Claude Code only for activity summaries, with no tools. The watch tool runs it on a schedule, in a folder and with a skill that a local caller names.

## Decision

A tool may start Claude Code with `--safe-mode`, in a canonical folder the tool checked, with `--allowedTools` set to the skill's `allowed-tools` exactly as written, unchecked. This is the user's choice: the skill's author decides what a run may do, and Harold does not narrow it.

The caller's folder and skill are read at each run, so the caller can change the instructions and the tools while a watch runs. The skill's real path must lie in the caller's `.claude/skills/` or in a watch skill folder the user lists, so the user decides which shared folders of skills any watch may run. Harold bounds a run's time and turns always, and its cost only when the user or the caller sets a budget (optional for now, none shipped). It types the run's output only to the chief or the watch's own folder, in the fixed `WATCH` forms.

## Consequences

- A skill may let the model change things, such as writing files, running commands or reaching the network, every few minutes with no person watching.
- Text in the facts, the files a run reads or command output can steer a run to do whatever its skill allows.
- Agent facts, the skill's text, files the run reads and command output go to Anthropic's hosted model.
- A later change that narrows the tools needs its own decision.
