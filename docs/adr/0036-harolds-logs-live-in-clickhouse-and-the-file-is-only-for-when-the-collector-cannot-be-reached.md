# 36. Harold's logs live in ClickHouse, and the file is only for when the collector cannot be reached

Date: 2026-11-07
Status: Accepted.

## Context

Harold wrote each log line to `harold.log` and sent a reduced copy to the collector. The export dropped fields, left out other crates' lines, and missed panics, bad configs and ports in use. The user reads logs in HyperDX, so the file was a second, partial source.

## Decision

- Harold sends all its logs, traces and metrics to the collector. `harold.log` holds a line only when Harold cannot send it.
- The file holds three kinds of line: telemetry's own failures, unsent log lines (marked `unsent=true`), and a panic that could not be sent.
- Harold keeps up to 5,000 unsent records in the resend queue and sends them again, with their first timestamps, when the export works again.
- Telemetry starts before the config loads, so a bad config and a port in use reach the collector.
- A panic hook logs `harold panicked` and flushes for at most 2 seconds.
- Exception to the standard "no message text" (approved by the user on 2026-11-06, choice A): warn and error messages of other crates, and the panic message, go to the collector with each word that holds `/`, `\`, `@`, `?` or `=` cut to `<cut>`, and at most 200 characters. The cut runs in `allowed_record`. The file keeps the line uncut.
- The trim keeps its 24-hour rule and adds a 10 MB limit.

## Consequences

- ClickHouse holds every line that Harold can send. A reader looks in HyperDX first.
- A secret with none of the cut characters, such as a bare token in another crate's message, can reach the collector.
- Readers of `harold.log` (the release watch, the CLI hint, the docs) change to ClickHouse first.
- Lines made while the collector is down are lost from ClickHouse if Harold restarts before recovery or the queue overflows. The file still holds them.
