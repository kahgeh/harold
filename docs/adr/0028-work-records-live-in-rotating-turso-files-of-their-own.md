# 28. Work records live in rotating Turso files of their own

Date: 2026-11-03
Status: Accepted (work-records). Builds on 0001 and 0002.

## Context

The chief keeps work records with a priority, a status and decisions. The chief's queue has one Now item, and 35 items on 2026-11-03. The records must not make the user or the chief busier.

## Decision

1. The work records live in their own Turso files, in `tools/work/` in Harold's data folder. A file is named `work-<YYYY-MM-DD>.db` by the day it starts. The newest name is the current file. No pointer file exists.
2. Harold rotates by itself. Every `rotation_days` days (14 by default, `0` turns it off), it copies the open work records, with their decisions and links, to a new file.
3. A rotation writes the new file under a temporary name. Harold counts the open work records, decisions and links in both files. Only when the counts match does it rename the new file and use it.
4. A failed rotation leaves the old file current and unchanged. Harold tries again at the next daily check. After a rotation, Harold opens the old file read-only.
5. `list` and `show` read the current file. `history` and `search` read the files newest first. The first file that holds a name gives the answer.
6. The work tool is a module of Harold with a `WorkService` gRPC service on loopback. It trusts local callers (0002). Only the chief's pane writes.
7. The work tool reads the question tool and the queue through Harold's own gRPC address on loopback (`ToolContext::grpc_addr`), as a local caller (ADR 0002), never through their stores.

## Consequences

The views read one small file. A history reads the files newest first. A failed rotation keeps the old file, logs `work rotation failed` once, and fires the alert `work-rotation-failed`. The queue and `backlog.db` do not change. A closed work record stays in the file where it closed.

After migration 002, v0.75.0 or earlier cannot open the new work file: its work tool refuses with `incompatible work schema`. The old file stays readable, because a rotation never changes it.
