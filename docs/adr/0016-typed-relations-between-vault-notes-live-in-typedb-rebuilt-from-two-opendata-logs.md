# 0016. Typed relations between vault notes live in TypeDB, rebuilt from two OpenData logs

Date: 2026-10-25
Status: Accepted (knowledge-typedb)

## Context

The chief wants to record how vault notes relate, beyond the links in their text. The user asked for TypeDB and made recoverability the rule: every insert and update in an OpenData log, schema changes in a separate one, and TypeDB rebuilt by replaying the schema log, then the data log.

## Decision

One graph writer in Harold checks every graph change against the graph, writes it to the data log and flushes it, then applies it to TypeDB in one transaction with the graph position, one change at a time. Each change is safe to apply twice, and replay applies records with a sequence greater than the position. A record TypeDB still refuses stops the catch-up until a logged voided record passes it. When a vault note goes, the pass keeps it, marked missing, while a typed relation names it. Schema steps live in Harold's code, are appended to the schema log once, and only add. On start Harold catches TypeDB up from the logs, and rebuilds it from them when the database is missing. TypeDB CE runs on this Mac on `127.0.0.1`, started by Harold. The logs use `opendata-log =1.0.0` and `opendata-common =0.1.17`, already in `harold/Cargo.toml` for the event stream, so they add no crate.

## Consequences

- The chief's typed relations survive a lost TypeDB database, and the logs on this Mac are their only copy. Keeping them in the bucket is a later item.
- No schema step may remove or rename a type without a data-log rewrite.
- Harold starts a third kind of child process, after the voice and embedding workers, and it is a binary from outside crates.io, checked by hand.
- A record TypeDB refuses stops the catch-up until someone voids it with a logged voided record, so the graph is never silently short.
