# 0015. The question tool reads the spec files the chief names

Date: 2026-10-24
Status: Accepted (todos-stay-current)

## Context

[ADR 0001](0001-tools-are-modules-hosted-by-harold.md) gives a tool only its data folder and the ports in `ToolContext`. To mark a todo whose spec has changed since it was asked, the question tool must read that spec. Specs live outside its data folder, in each product's `.superpowers/pod/specs/`.

## Decision

A question may name its spec's file, `spec_file`. The question tool opens that file only after resolving symlinks and finding it inside the `.superpowers/pod/specs/` folder of a product the `Products` port lists. It refuses any other path at `put`, and checks again at every list. It only hashes the file, and compares the checksum with the question's `sha`. It never logs, shows or sends the path or the file's content.

## Consequences

- Under [ADR 0002](0002-tool-services-trust-local-callers.md), any local caller can name a `spec_file`. The limit keeps the files Harold may open to the registered specs folders.
- Any local caller can also set the checked time with `MarkChecked`, so a red checked line can be cleared without a real check. This is accepted, as for the other chief RPCs.
- A registry that can't be read makes every spec state `unknown`, never `current`.
- The core is still reached only through `ToolContext`. The tool uses the existing `Products` port, and no new port is added.
