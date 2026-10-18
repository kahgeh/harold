# 1. Tools are modules that Harold hosts

Date: 2026-10-18
Status: Accepted (dash-question-queue, merged at 8ab08a3)

## Context

The question queue is Harold's first tool, and more are expected (the next is each product's backlog plus the chief's queue). Each is likely to want a different database. The core store and the Harold gRPC service must stay unchanged by them.

## Decision

- A tool is a module under `harold/src/tools/<name>/` with a `NAME` and `start(ToolContext) -> Result<ToolHandle, ToolError>`.
- The tool opens and migrates its own storage in `ToolContext.data_folder` (`<store folder>/tools/<name>/`). The host knows no database type.
- `ToolContext` holds only what the host gives: the data folder, a `PaneTyping` port, a chief-pane lookup and the shutdown signal. The core is reached only through these.
- `ToolHandle` holds the tonic routes, which the host adds to the gRPC server, and the task handle, which the host joins on shutdown.
- Each tool has its own proto package, `harold.tools.<name>.v1`, and its CLI under `harold tool <name> ...`.
- Typing into a pane goes through `PaneTyping` as the single `Writer::Tool(name)`. A tool that needs it gets its own writer; the host does not share one between tools.
- A tool that cannot start is logged (`tool failed to start`, with `tool` and `reason`) and left out; Harold runs on.
- There is no plugin loader or registry. The host is plain code that gains a `start` call and a `start_tool` line per tool.

## Consequences

- A new tool changes `tools.rs` and touches no other core code.
- A rollback to an older Harold ignores `tools/<name>/`; deleting the folder empties that tool.
- Each tool's database is separate from the core store, so the core's disk-fault handling does not cover it.
- The data standard is still undecided; one database per tool is the current practice, not a standard.
