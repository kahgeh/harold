# 39. Decision records live in a store of their own, and their details live with what they belong to

Date: 2026-11-14
Status: Accepted (decision-tool). Builds on 0001, 0002, 0014, 0021 and 0028.

## Context

The roles make decisions from evidence. The coach must grade each decision weeks later. A work record decision (ADR 0028) rotates with its work record, so it does not stay writable for months. The coach needs a record that stays writable, with a check date.

## Decision

- **One store that never rotates.** The decision tool keeps its decision records in one Turso file, `tools/decision/decisions.db`. The file does not rotate. A decision record stays writable when its work record closes or rotates, or when the spec that it links is released.
- **The details live with the thing that the decision record belongs to.** The details owner writes the entry under the heading `## <id>: <title>`:
  - A spec's `specs/<id>.decisions.md`, or a product's `decisions.md`, in the pod folder. The PnEM writes it.
  - The vault's `decisions.md`. The chief writes it.
  - A file in the personal repository. The chief writes it.
- **Harold reads the details only to check the anchor.** Harold finds the pod folder from the pod registry that `telemetry.pod_registry` names, as ADR 0021 does. It adds no second registry setting. When `telemetry.pod_registry` is not set, a `pod:` place is refused with `no_pod_registry`. Harold finds the vault from `[knowledge] vault`, the path that ADR 0014 gave the knowledge tool. Harold finds the personal repository from `[decision] personal_repository`. Harold writes no file in a pod folder, the vault or the personal repository.
- **The anchor id is the id of the record.** The details owner writes the entry before the chief adds the record. So the id in the anchor, `d-YYYYMMDD-NN`, is the id of the record. `add` accepts it when it is well formed, it is a real date that is not after the add date (UTC), and no record has it. A date before the add date is allowed.
- **Only the chief's pane runs the write commands.** The reads need no pane.

## Consequences

- Work record decisions (ADR 0028) stay as they are. They hold the agreed choices on one piece of work, and they rotate with it.
- A decision record is a decision made from evidence, with a check date, for the coach. It never rotates.
- A choice can be in both. The chief links the decision record to the work record with `work:<name>`. The tool does not check that the work record exists, so a closed or rotated work record never blocks a write.
- The vault's `decisions.md` is a vault note. The knowledge index reads it, so a knowledge search finds past decisions and their evidence.
- A new place for details, or a write by Harold into a place, needs a new ADR.
