# 7. The web app reaches the todos through the question tool's service

Date: 2026-10-21
Status: Proposed (web-app-todos). Amends 0002.

## Context

The web app is core code. The todos live in the question tool, which ADR 0001 makes a module reached through its own gRPC service, and the dash already reads and answers them that way.

## Decision

The web app is a client of `harold.tools.questions.v1.QuestionService` on Harold's own gRPC address, like the dash. It never imports the tool's module or opens `questions.db`. It uses `ListQuestions`, `AnswerQuestion`, `DeferQuestion` and `TickStep`.

## Consequences

- **This amends ADR 0002.** Under 0002 the tool trusts local callers, and the dash at the desk was the only one that answered. Now a phone on the tailnet can answer too, through the web app, which calls the tool as a local caller. The web app acts for the one Tailscale login the app answers, so an answer from the phone counts as the user's, as one from the dash does.
- **The gate is two checks:**
  - the login check (`allowed_login`, `login::require_login`);
  - then the same-origin check (`origin::require_same_origin`).

  The router applies both as layers to every route, so all five `POST /todos/...` routes pass them before any call reaches the tool. Route tests cover each of the five.
- **When the question tool did not start**, the todo page says it can't read the todos, and the thread works as before.
- **Polling:** the web app polls every 2 seconds while a page is open, as the dash does.
