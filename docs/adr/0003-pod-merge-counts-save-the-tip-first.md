# 3. Pod merge counts save the counted tip before recording

Date: 2026-10-18
Status: Accepted (pod-telemetry-counts, merged at 36a7c94)

## Context

`pod.merges` is counted from main's reflog after the last counted tip, kept in `<bundle>/data/pod/tips`. The counter is in memory and exported about every 30 s.

## Decision

On each poll, Harold saves the new tip first and records the counts after. A failed write changes nothing and the merges are counted by a later poll. The file is written through a temp file and a rename, readable by the user only. A missing or garbled file, or a tip the reflog does not hold, counts nothing and starts from the current tip.

## Consequences

- A crash between the two steps can lose a poll's counts but never double them. Recording first could double-count after an export and a crash.
- A merge made while Harold is stopped is counted once at the next start.
- A rollback to a Harold built before 36a7c94 leaves a stale tip; the operation README says to delete `data/pod/tips`.
