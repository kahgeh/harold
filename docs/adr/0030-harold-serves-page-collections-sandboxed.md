# 30. Harold serves page collections sandboxed

Date: 2026-11-03
Status: Accepted. Widens ADR 0017 (the specs listener serves `/c` as well as its two spec routes and three static files) and relies on ADR 0002 (local callers are trusted).

## Context

The chief writes explainers and other pages that the user reads on the phone. Each kind of page needs a place to be served, and a new kind must not need new code.

## Decision

- Harold serves files from folders that the user lists in `[[web_app.collections]]` in `local.toml`, at `/c`, `/c/<name>` and `/c/<name>/<file>`. A collection with `listed = false` is left off the Pages screen and is served at its URL.
- The web app serves them behind its login, device and origin checks. The specs listener (`127.0.0.1:4331`) serves them too, GET only, with its Host check.
- An HTML page is framed in the app's shell and served at `/c/<name>/<file>/raw` with a content security policy that allows no script and nothing from outside, and with `sandbox` and no `allow-*` flag. Only the shell may frame it.
- A Markdown page is rendered with the spec pages' renderer and rules (ADR 0017).
- Harold resolves each file's real path at each request and serves only a plain file of at most 2 MiB inside the collection's real folder. Every bad request answers `404`.
- Nothing is shared: a page leaves the machines only when the user shares it, as for specs (ADR 0017).

## Consequences

- A new kind of page is a config entry and a Harold restart.
- A page that needs a script must be drawn to SVG first.
- A local process that can write into a listed folder can show the user a page, and any local process can read every listed folder through the specs listener, as ADR 0002 trusts local callers.
- The specs listener also starts when a collection is configured and `[telemetry] pod_registry` is not set. On such a machine its `/specs` shows no specs.
- The counter `harold.web_app.collection_pages` and the `app page shown` log line show the refusals. The dashboard has no card for the counter yet.
