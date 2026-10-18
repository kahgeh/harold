# 2. Tool services trust local callers

Date: 2026-10-18
Status: Accepted (dash-question-queue, merged at 8ab08a3)

## Context

Harold's gRPC server is a plain TCP listener with no transport authentication: only the two peer calls, `ReportNotification` and `ForwardReply`, carry a secret in the request, and the tool services have none. The question tool's `AnswerQuestion` is meant to be called only by the dash, on the user's keys.

## Decision

The tool services trust local callers, with no token or secret. The dash is the only built client that answers, and the CLI has no answer command, but nothing stops a local process calling `AnswerQuestion`. The spec and docs say so plainly.

## Consequences

- A local process or agent session can forge an answer. The question tool never approves anything, and the chief records approvals itself, so a forged answer cannot approve on its own.
- If the user wants this stronger, the option is a token file (mode 0600) that the dash sends and the service checks on the answer and defer calls.
- The same rule applies to later tools; a guard that only prevents mix-ups (such as the planned per-product write check) is not security and must say so.
