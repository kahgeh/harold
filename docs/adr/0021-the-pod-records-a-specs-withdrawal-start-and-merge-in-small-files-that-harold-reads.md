# 21. The pod records a spec's withdrawal, start and merge in small files that Harold reads

Date: 2026-10-26
Status: Accepted (spec-states-and-flow). Builds on 0003.

## Context

Harold guessed a spec's later states from git and worktrees. It could not see a withdrawal: a spec that was replaced, set aside or merged into another stayed in its old state. On a machine that merges on a code host, a merge leaves no `merge <id>:` reflog entry, so Harold saw no merged spec at all.

## Decision

- The pod's shared tools write three files once, in a fixed `key: value` form: `withdrawn/<id>.md`, `started/<id>.md` and `merged/<id>.md`. Harold reads them first, with the older sources as a fallback.
- Harold still never writes in the pod folder.
- `pod_stats`, a core module, reads Harold's agent snapshot directly, pane ids and folders only, to count pods and agents by role. The tool ports of ADR 0004 stay for tools.

## Consequences

- The states are the same on every machine, including an organisation that merges on a code host.
- A tool that does not write a file leaves the old guess in place, with one exception: a spec with a started file and no merged file stays in progress until its merged file appears.
- Only when the last merge of such a spec carries no stage, is over 7 days old and no build worker runs for it does Harold complete it from the old sources. It then logs `spec completed without merged file`.
- A staged spec is never completed without its merged file. After 7 days Harold logs `spec stage merged without merged file`, and the spec can stay in progress until the chief acts.
- A file that does not parse logs `pod file unreadable` once, and the spec falls back to the older sources.
