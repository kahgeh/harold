# 25. Harold keeps transcript text in memory and shows it only in the control-tower

Date: 2026-11-01
Status: Accepted (recap-as-work-summary). Builds on 0024 (rex-state-from-rex).

## Context

Claude Code writes a recap of a session into the session's transcript after the user was away. The line (`away_summary`) is not documented. Harold already reads a transcript for the restart reminder. The standards forbid transcript text in a log line, a span or a metric, and let only agent facts and activity summaries go to a hosted model. They did not state whether Harold may keep transcript text in memory.

## Decision

1. Harold reads the latest recap of an agent from the agent's transcript with a shared tail reader (`harold/src/transcript.rs`), which the restart reminder also uses.
2. Harold reads the recap through a recap adapter. The provider's settings name the adapter with `recap_adapter` in its `[[agents]]` entry. A provider with no `recap_adapter` has no recap and Harold does not read its transcript. Today only Claude Code has an adapter, `claude-transcript-v1`, and it reads an undocumented format.
3. The recap, and the current step from a Rex program status record, stay in memory. A Harold restart forgets them.
4. They reach only the `WatchAgentStates` stream that the control-tower reads. They are not part of the agent projection or of `AgentSnapshot`. They never go to a model, the store, a log line, a span, a metric, the phone app or a phone message.
5. A counter and an alert (`alert-agent-recaps-stopped`) tell the owner when Claude Code stops writing recaps or changes the line's format.
6. The current step (`msg`) is not a work summary. This corrects 0024 point 5.
7. The user said yes on 2026-11-01 (Q142) to Harold keeping transcript text in memory. The standards list it as an exception.

## Consequences

- A change in Claude Code's undocumented line can stop recaps. The alert notices it within 3 days, and the work summary shows meanwhile.
- A Claude Code agent in Rex has a work summary only from the activity summary, after its first turn end.
- An older control-tower skips the new `AgentPaneState` fields and shows the work summary.
- A new provider needs its own recap adapter before Harold can show its recap.
