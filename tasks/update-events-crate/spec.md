# Latest `events` Crate Integration Specification

## Goal

Advance Harold's `events` submodule from `d50236e` to `a23c70c` and preserve the current observable behavior for turn-completion notifications and inbound-message routing.

## Observable behavior

- `TurnComplete` returns `accepted: true` only after the `TurnCompleted` event is durably appended.
- iMessage and Telegram listeners append `InboundMessageReceived` events to the same ordered Harold stream.
- `TurnCompleted` events invoke the existing `outbound::notify` path.
- `InboundMessageReceived` events invoke the existing `inbound::route_inbound_message` path.
- Events are handled in `EventStreamVersion` order.
- Restarting Harold resumes from durable application-owned state and does not create duplicate pending delivery records.
- SIGINT or SIGTERM stops ingress, the listener, and the handler without relying on the removed `EventStore::checkpoint` API.

## Architecture

Open one `events::EventStream` through `EventNamespaces` using namespace `harold` and partition key `main`. A `HaroldStore` owns that stream and a separate Turso application database at `<store-root>/harold-state.db`.

The handler stages newly read events into an application-owned delivery outbox and advances `last_processed_event` in one Turso transaction. A delivery loop then performs the existing notification or routing effect and marks the outbox row delivered. A crash after the external effect but before the delivered marker can still cause an at-least-once duplicate; exact-once delivery is not claimed.

## State schema

Use the refreshed crate's public `LAST_PROCESSED_EVENT_SQL` application schema plus Harold-owned checksum tracking for both application-state migrations.

- `last_processed_event(namespace, partition_key, last_processed_event_version, updated_at_ms)` stores the handler cursor.
- `delivery_outbox(event_id, event_version, event_type, payload, trace_id, attempt_count, last_error, delivered_at_ms)` stores staged work.
- `event_id` is the idempotency key for staging.
- Pending rows have `delivered_at_ms IS NULL` and are read in `event_version` order.

## Storage compatibility

The existing old-format files directly under the configured store root are not modified or deleted. The new event stream lives under `<store-root>/harold/main/`. Historical migration and retention remain explicitly deferred; new messages received after the upgrade use the new stream.

## Dependency constraints

- Pin the submodule to `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`.
- Use Turso `0.5.1`, matching the refreshed `events` crate.
- Before building, resolve the root lockfile to at least `anyhow 1.0.103`, `crossbeam-epoch 0.9.20`, `rand 0.8.6`, and `rand 0.9.3`.
- Temporarily accept pre-existing `opentelemetry_sdk 0.30.0` only because Harold does not configure inbound baggage extraction; upgrade before enabling baggage propagation.
- Temporarily accept transitive `paste 1.0.15` as an existing unmaintained dependency.

## Non-goals

- Do not recreate the removed crate-owned `Projector`, global cursor, lease, or checkpoint APIs.
- Do not migrate or delete historical old-format event files.
- Do not decide completed-work retention in this change.
- Do not alter notification selection, message summarisation, pane routing, or channel behavior.

## Verification

- Observe new store and handler tests fail before implementation and pass afterward.
- Run Harold tests, workspace tests, formatting, Clippy with warnings denied, documentation checks, and an offline advisory scan.
- Confirm the submodule pointer and resolved dependency versions.
- Obtain an independent completion review and resolve every finding before completion.
