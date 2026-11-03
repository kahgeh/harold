# Revision tags

A pod keeps one revisions file for each spec: `specs/<id>.revisions.md`. Each `## ` heading in the file is one entry. Harold reads the headings in `revision_marks` and `revision_entries` (`harold/src/pod_stats.rs`). The PnEM writes the tags, and Harold reads them. Keep this list and the parser in step.

## Heading format

```
## <YYYY-MM-DD> <HH:MM>, rev <n> (<tags>)
```

- The time is local time. Harold reads it as the time of the revision.
- A heading with a date only counts from that date's local midnight. The old files still parse.
- A heading counts as dated only when it starts with a readable date, or a date and a time, and a comma. A heading with no readable date has no time. It counts as a revision, but it is never redo and it is not counted as revised after approval.
- The tags are the comma-separated words in the last parenthesis.
- A heading with no tags is a revision of unknown class. It is never redo.
- The PnEM must write the time on each `direction` or `defect` revision.

## Tags

- `first`: the first draft. It is not a revision.
- `direction`: the user changed what the spec asks for.
- `defect`: the spec had a fault that a reader found.
- `architect`: the architect reviewed the spec and asked for a change.
- `wording`: the revision changes words only. It counts as a revision. It is never redo.

## Redo

Redo is build work after a `direction` or `defect` revision. The revision must have a time after the spec's started time. A revision between the approval and the start is not redo.

With several such revisions, redo runs from the earliest one to the end of the spec. For a spec not yet complete, it runs to now.

A revision with no readable time, or with other tags only, is never redo.

## Revised after approval

This is the count of dated entries with a time after the approval. Any tag counts, except `first`. A heading with no readable time does not count. A spec with no approval has a count of 0.
