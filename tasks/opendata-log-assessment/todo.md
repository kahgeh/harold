# OpenData Log replacement assessment

Scope: assess whether OpenData Log can replace Harold's embedded `events` dependency. Research only; no dependency, implementation, or runtime changes.

- [x] Trace the current application-used event-storage contract and checkpoint ownership.
- [x] Inspect upstream OpenData Log source for append, scan, durability, persistence, and operational requirements.
- [x] Compare semantics and identify necessary adapter behavior and risks.
- [x] Record a recommendation and evidence limitations.

## Review

Source assessment: feasible behind a Harold-owned event-log adapter; not a direct dependency swap. Upstream GitHub API resolved `main` to `5f5c28826921c1fe014e033c4506680be149d386`; inspected pinned source as well as upstream README and manifests.

- Harold uses a single ordered `harold/main` stream, atomic append batches, and exclusive-cursor reads. All current append callers use `ExpectedVersion::Any`. Independent explorer traced application usage and found no dependency on crate worker pools, notification stores, or log subscriptions.
- OpenData `LogDb` offers atomic record batches and per-key sequence scans. One fixed key preserves Harold's cross-event order. Local filesystem SlateDB storage is supported; the HTTP server is optional.
- Retain event UUID, type, payload, and timestamp in a serialized envelope. Preserve stable positive `i64` versions; OpenData sequences are zero-based `u64`, requiring explicit checked mapping. Outbox replay deduplication depends on stable event IDs. Current Harold does not offer logical request deduplication.
- OpenData append does not wait for durability. Wait for flush or the batch's durable watermark before acknowledging, use durable read visibility for projection, and drain/close on shutdown. Current events WAL/NORMAL transaction evidence is not a power-loss durability proof.
- Keep Harold's existing state database and its projection/outbox/checkpoint transaction. Retention must not remove history needed for replay. Existing data import versus fresh stores is a separate scope decision.
- Recommendation: a bounded prototype is reasonable if reducing custom storage maintenance or gaining object storage is the objective. A production switch needs restart/replay, batch atomicity, durable acknowledgment, version mapping, and latency/resource acceptance. No performance improvement is established by this assessment.

Evidence: `harold/src/store.rs:105,180,228,433,528,1109`; `harold/src/projector.rs:172`; `harold/src/agent/runtime.rs:1055,1440`; upstream `log/src/log.rs:174,178,288,297,333`, `log/src/model.rs`, and `common/src/storage/config.rs` / `factory.rs`.

Verification boundary: source inspection only. No dependency installation, build, crash test, benchmark, or runtime change performed. Implementation would require a separate concrete proposal.
