# Security Audit Report

**Date:** 2026-02-27
**Scope:** Full codebase review of Harold
**Auditor:** Claude Opus 4.6

## Summary

A comprehensive security review was conducted across the entire Harold codebase — a Rust-based agent notification and reply routing daemon for macOS. **No high-confidence exploitable vulnerabilities were identified.**

## Threat Model Assumption

This audit assumes Harold runs on a **trusted, single-user local workstation**:

- Only trusted software runs under the local user account
- No hostile local processes are executing on the same machine
- Harold remains bound to localhost (default `127.0.0.1`)

Under this assumption, risks like unauthenticated localhost gRPC access and tmux pane spoofing are treated as **lower-likelihood hardening concerns** rather than high-confidence exploitable issues.

## Scope

The review covered all source code including:

- gRPC service handlers and routing (`harold/src/main.rs`)
- iMessage integration and AppleScript execution (`harold/src/outbound/imessage.rs`)
- tmux pane scanning and relay (`harold/src/inbound/tmux.rs`)
- Event store and database interactions (`events/src/`)
- Configuration management and utility functions (`harold/src/util.rs`)
- AI CLI subprocess spawning and prompt construction (`harold/src/inbound/mod.rs`)
- Message listener and polling (`harold/src/listener.rs`)

## Remediated Issues

### 1. Phone number committed to public git history

- **Severity:** Medium
- **Category:** data_exposure
- **File:** `harold/config/local.toml`
- **Description:** The `.gitignore` pattern `config/local.toml` only matched at the repo root, not `harold/config/local.toml`. The file containing a personal phone number, handle IDs, and local paths was tracked in the public repository.
- **Remediation:** Fixed `.gitignore` to `**/config/local.toml`, removed file from tracking, scrubbed from all git history with `git-filter-repo`, and force pushed.

## External iMessage Attack Surface Analysis

A dedicated investigation was conducted to determine whether an external party sending an iMessage could control or execute commands on the machine.

**Finding: Not exploitable.**

### Primary security boundary

In `harold/src/listener.rs:87`, the SQL query that polls `~/Library/Messages/chat.db` includes a `WHERE handle_id IN ({id_list})` clause. Only messages from conversations matching the user's own configured handle IDs are processed. An arbitrary external sender's messages are never read by Harold.

### Defense-in-depth layers

Even if the `handle_ids` filter were bypassed, multiple layers prevent exploitation:

| Layer | Location | Protection |
|---|---|---|
| Terminal escape stripping | `inbound/tmux.rs:78-100` | `strip_control()` removes ANSI escapes and all control characters; `\x1b` byte is always dropped |
| Literal tmux mode | `inbound/tmux.rs:110` | `tmux send-keys -l` treats text literally, key names like `C-c` are not interpreted |
| No shell interpolation | `inbound/tmux.rs:106-115` | `Command::new("tmux").args([...])` uses direct exec, no shell involved |
| Constrained AI routing | `inbound/mod.rs:54-135` | AI classifier runs with `--max-turns 1` and `disableAllHooks: true`; output only matched against existing pane labels |
| AppleScript escaping | `util.rs:17-21`, `outbound/imessage.rs:15-23` | `sanitise_for_applescript` + backslash/quote escaping applied to all text entering AppleScript |
| Parameterized SQL | `events/src/` | All queries use bound parameters; dynamic table names validated by `TableNameValidator` regex |

## Positive Security Observations

1. **Parameterized SQL queries**: All database interactions use parameterized queries (`?1`, `?2`, etc.) with bound parameters. The only dynamic table name usage is protected by `TableNameValidator` enforcing `^[a-zA-Z][a-zA-Z0-9_]*$`.

2. **No shell injection surface**: External commands (`tmux`, `osascript`, `sqlite3`, `uv`, Claude CLI) use `std::process::Command` with `.args()`, bypassing shell interpretation entirely.

3. **AppleScript string injection handled**: The `sanitise_for_applescript` + escape sequence in `imessage.rs` properly escapes `\` and `"` for AppleScript string literals passed via `osascript -e`.

4. **ANSI/control character stripping**: The `strip_control` function removes ANSI escape sequences and control characters before relaying text to tmux panes, preventing terminal escape sequence injection.

5. **AI prompt injection mitigation**: The `semantic_resolve` function strips `</message>` closing tags from the user message body to prevent prompt injection breakout from the XML-structured prompt.

6. **No web-facing HTTP surface**: The application only exposes a gRPC service on localhost. No HTTP handlers, HTML templates, cookies, or sessions — eliminating entire categories of web vulnerabilities.

7. **Environment variable allowlisting**: The `ai_cli_env()` function uses an explicit allowlist when spawning AI CLI subprocesses.

8. **Safe deserialization**: All serialization uses `serde_json` with strongly typed Rust structs.

## Hardening Recommendations

These are not vulnerabilities but would strengthen defense-in-depth:

1. **Broaden escape sequence stripping**: `strip_control` handles CSI sequences (`\x1b[`) but not OSC (`\x1b]`), DCS (`\x1bP`), or APC (`\x1b_`). While `\x1b` is already dropped as a control character (rendering these non-functional), explicit handling would be more intentional.

2. **AI prompt tag stripping**: Consider stripping `<message>` (opening tag) in addition to `</message>` to further harden the prompt boundary.

3. **gRPC authentication**: The localhost-bound gRPC service has no authentication. On a single-user macOS workstation this is low risk, but a shared secret or Unix domain socket would add a layer of protection.
