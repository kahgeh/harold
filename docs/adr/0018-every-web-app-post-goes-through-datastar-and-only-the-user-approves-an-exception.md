# 0018. Every web app post goes through Datastar, and only the user approves an exception

Date: 2026-10-25
Status: Accepted (datastar-posts). Builds on 0007 and 0012.

## Context

The web app's pages posted in two ways. The thread, todo and spec pages used Datastar's `@post`. The pair, call, photo, echo test, speech check, push, dictation and page-report scripts used their own `fetch` calls. The same-origin check accepted a post that carried `Datastar-Request: true`, `application/json` or `application/octet-stream`, on every route. A page therefore needed no one rule, and a new post could use any of the three.

## Decision

- Every post and every delete from a page of the web app goes through Datastar's `@post` or `@delete`. Datastar sends `Datastar-Request: true`. A script that posts sets its signals with Datastar's own `mergePatch` and starts the request with Datastar's `post` action, so the request is the same as one a template makes.
- The same-origin check accepts a post or a delete from the app's own origin only with `Datastar-Request: true`. It logs `app post refused`, with the route and `not_datastar`, when a post from the app's own origin lacks it.
- Two posts are approved exceptions, and the check lets each in by its own content type, on its own route only:
  - **`POST /call/audio`** keeps a raw `fetch` with `application/octet-stream`. Audio sent as signals would be base64, which is a third larger, four times a second.
  - **`POST /telemetry/traces`** keeps the OpenTelemetry library's own exporter with `application/json` (ADR 0012). The library owns its transport, and the route already allows only its own fields.
- Only the user approves another exception. Each exception is recorded in an ADR.
- Every moved post sets `retry: 'never'` and `retryMaxCount: 0`. Datastar's default retries a failed request up to ten times, and several posts are not safe to send twice: `POST /call` would start a second call, `POST /pair` would spend the code again, and a result post would store a second row. Every moved post also sets `openWhenHidden: true`. With the default, Datastar closes a request when the page hides and sends it again when the page shows.
- A handler reads its signals as the JSON body that Datastar sends, and answers with Datastar events: patched signals, or patched elements for a line on the page. Datastar reads no body from a status of 400 or more, so an answer the page shows is a `200` with patched elements or signals. A `401` stays a plain `401`: Datastar announces it as an `error` event with the status, and each page reloads to the pair screen on it.
- The photo is the base64 signal `_photo`. A signal that starts with `_` stays out of every request by default, and only the photo post names it. Harold checks the 5 MB limit on the decoded bytes.
- A test fails when a page script sends a post or a delete without Datastar, other than the call audio post.

## Consequences

- One rule covers every post: a post without the Datastar header, other than the two exceptions, gets `403`.
- A post sent as the page hides has no `keepalive`, so the phone can drop it. Harold's own fallbacks cover this: a page not heard from for 75 s stops counting as on screen, and a call ends after 15 s of silence. The `hidden_reported` field on `app page disconnected` shows how often a hidden report is missing.
- A photo post is a third larger than the photo, because of base64.
- A new page that posts loads the vendored Datastar file, and a script that posts needs its own call of Datastar's `post` action.
