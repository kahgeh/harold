# 0006. Answers go to the chief early, and "later" keeps meaning after everything else

Status: Accepted (answers-send-early, merged at e981c54)

## Context

The question tool used to type nothing to the chief until no question was waiting, so answering 4 of 5 questions told the chief nothing. The user wanted answers to reach the chief as they are given. Sending at once makes one chief-pane line per answer, and answering "later" used to mean "after the next line", which early lines would change.

## Decision

- A line starts when at least one answer is unsent and 3 seconds (the settle) have passed since the newest unsent answer's own `updated_at_ms`. Questions still waiting do not hold it back. Other writes and the 5-second busy-pane retry do not restart the settle.
- Each answer can be marked "send now" (`send_now`, proto field 6, stored in migration 002). A marked answer starts a line at once, holding every unsent answer.
- A set-aside question returns to waiting after a line is typed with nothing waiting, or at a defer or a dismiss that leaves nothing waiting and no answer unsent. So "later" still means after everything else.
- Each early line repeats, after `later:`, the questions still set aside.

## Consequences

- The chief hears answers in several lines, not one; it must read each `ANSWERS n` line on its own.
- The stored mark survives a busy pane and a restart. A Harold without migration 002 refuses the migrated store, so a rollback means moving `questions.db` aside.
- Dismissing the last waiting question returns the set-aside ones, by the same rule as `later` (dismiss-returns-set-aside)
