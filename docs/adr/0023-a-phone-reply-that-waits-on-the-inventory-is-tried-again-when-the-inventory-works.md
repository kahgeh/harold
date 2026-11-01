# 23. A phone reply that waits on the inventory is tried again when the inventory works, and does not count toward the 10 tries

Date: 2026-11-01
Status: Accepted (held-line-retry-wait)

## Context

The delivery handler gives up an outbox row after its 10th failed try, about 4 minutes after the first. A phone reply waits when the inventory fails and the chief's pane is not known. In a test, the inventory failed for longer than 4 minutes, and Harold lost the reply. The outbox keeps its order, so a waiting row also holds up the later rows.

## Decision

- A try that fails with `InventoryFailed` does not add to the stored `attempt_count`. The delivery handler tries the reply again when the agent snapshot shows that the inventory works, or after 5 seconds.
- The reply gives up after `LONGEST_INVENTORY_WAIT`, with `reason=inventory_failed`. The wait starts again after a restart of Harold.
- The user chose option (a) and 15 minutes. `LONGEST_INVENTORY_WAIT` is 15 minutes. While the reply waits, later rows that are not phone replies go ahead. Later phone replies wait behind it, so replies keep their order. The store's next-row query has a form that leaves out phone replies.
- Every other retryable failure keeps its waits: 1 s doubling to 60 s, and 10 tries.

## Consequences

- The delivery retry rule has two rules. `docs/explanations/architecture.md` states both.
- A reply survives an inventory fault shorter than `LONGEST_INVENTORY_WAIT`.
- A turn-end update can reach the phone before an earlier reply is typed.
