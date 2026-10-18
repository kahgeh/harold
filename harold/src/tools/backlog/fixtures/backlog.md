# Backlog: demo (refreshed 2026-03-02)

Ranking rule (2026-02-10): rank by what the user sees first, then by size.

Keep each item small enough to finish in one sitting; split anything
that needs more than one review round.

## 1. search-filter: Let the list be filtered by a typed word
- value: a long list is hard to scan; a filter finds an entry in seconds
- size: small
- risk: the filter could hide an entry the user expects to see
- reason: three people asked for it this week
- paths: app/src/list.rs, app/src/filter.rs
- after: nothing before it
- evidence: notes/2026-02-28.md:12

## 2. export-csv: Write the list out as a CSV file
- value: people can take their entries into a spreadsheet
- size: medium
- risk: commas inside an entry must be quoted
- reason: the spreadsheet is where the monthly summary is made;
  without an export it is typed in by hand
- paths: app/src/export.rs
- after: search-filter
- evidence: notes/2026-02-20.md:4

## 3. dark-theme: Offer a dark colour scheme
- value: easier on the eyes at night
- size: small
- risk: low contrast on the muted labels
- reason: the most asked-for change in the feedback form
- paths: app/styles/theme.css
- evidence: feedback/form-2026-02.csv:7

## 4. undo-delete: Bring back an entry deleted by mistake
- value: a slip of the finger no longer loses work
- size: medium
- risk: the undo window must not outlive the session
- reason: two support emails in February
- paths: app/src/store.rs, app/src/undo.rs
- after: export-csv
- evidence: support/mail-0211.txt; support/mail-0219.txt; app/src/store.rs:88

## 5. sync-retry: Retry a failed sync before showing an error
- value: fewer false alarms on a weak connection
- size: large
- risk: a retry loop could drain the battery
- reason: the error banner shows on most train journeys
- paths: app/src/sync.rs
- after: dark-theme
- evidence: logs/sync-2026-02-27.log:301

## Sizing pass (2026-02-27)

- search-filter moved up: the filter code already exists in the settings page.
  - the settings filter matches on prefixes only
  - a whole-word match is a small change
- sync-retry stays last until the battery numbers are in.

## Review of the export idea (2026-02-20)

The export was weighed against a share button and won on effort.

### What was compared
- a share button needs a server
- an export needs only a file

## Cleared items (2026-02-15, morning)

- Removed two items that shipped:
  ## 9. old-item: Show a word count under each entry
  ## 8. older-item: Sort the list by date
- Both were confirmed by the release notes.

## Refreshed (2026-03-02)

- Re-read every item against the latest notes; no rank changed.

## In flight with another session (not ranked)

Another session owns these until it reports back.

- offline-mode: the offline cache, waiting on the sync rewrite
- tags: entry tags, being designed in a separate spec

## Looked at and left out

- voice-notes: recording audio is out of scope for a text app
- calendar-view: looked at twice and set aside
  - the list already sorts by date
  - a calendar adds a second way to see the same thing

  If the list grows past a few hundred entries this is worth a second look.
- emoji-picker: the system keyboard already offers one
