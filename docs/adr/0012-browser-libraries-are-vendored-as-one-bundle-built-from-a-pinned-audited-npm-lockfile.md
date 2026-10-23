# 0012. Browser libraries are vendored as one bundle built from a pinned, audited npm lockfile

Date: 2026-10-23
Status: Accepted (web-app-otel). Builds on 0007.

## Context

The web app has no build step: Harold serves files from `harold/static/web_app/`, compiled into the binary, and Datastar is one vendored file. The OpenTelemetry browser library is only published as many npm packages. The collector's OTLP port is reachable only on the Mac, so a phone cannot send to it. The Rust build and the release must not run npm.

## Decision

- A browser library that comes as npm packages is bundled once, by `scripts/build_web_otel.sh`, from `web-otel/package.json` and `package-lock.json` with `npm ci --ignore-scripts`, into one vendored file under `harold/static/web_app/`. The file is named after its library version and the first 8 hex of its sha256, and carries a header naming each package, version, licence and the lockfile's sha256.
- Each package goes through the javascript-supply-chain-auditor before it is added; the audit is recorded in `docs/references/dependency-audits.md`. Its licence text goes in `LICENSES/`, which the release already copies.
- The script runs only to upgrade, after a fresh audit. An asset test fails when the bundle, its name and the lockfile disagree, or when it names a Node-only hook.
- Browser spans reach the collector only through `POST /telemetry/traces`, behind the login and origin checks. Harold keeps an allowlist of fields, cuts URLs to their path, and forwards OTLP JSON to `[telemetry] otlp_http_endpoint`.
- A browser trace continues through Harold's own spans: the `traceparent` of a reply or todo answer is stored with it (the reply's event, `app_messages.trace`, the answer's `trace_parent`), so the typed prompt, the chief's turn, the delivery and the row sent to the phone join one trace. The phone's `browser.message_shown` closes it through the row's `data-trace` id. Ids only, no text. The one exception is a `browser.error` span's `exception.message`, cut to 200 characters, which the user chose knowingly.

## Consequences

- The phone downloads about 71 KB more script, once per bundle name because the versioned files are served as immutable.
- A `browser.error` span carries the error's message (cut to 200 characters), which can quote page data; it is the only free text in the web app's spans, and Harold's route cuts it.
- Upgrading the library is a deliberate, audited act, not a side effect of a Rust build.
- Harold's route is the one place that decides what leaves a phone, so a new browser field needs a change to the allowlist.
- Departs from the telemetry standard's OTLP over gRPC to port 4317 for this one path, because a browser cannot speak gRPC.
