# Fix WAL Lifecycle

## Status: WAL hypothesis disproved

The original hypothesis (WAL files causing the panic) was wrong. Testing shows the panic occurs even with 0-byte WAL files after a clean checkpoint.

```
thread 'main' panicked at core/schema.rs:910:
all automatic indexes parsed from sqlite_schema should have been consumed, but 1 remain
```

This fires at `Database::open_with_flags_bypass_registry_async` — during the database open itself. **Turso 0.5.0-pre.14 cannot reopen a database that it previously created**, regardless of WAL state.

## Confirmed root cause: turso schema parser bug on reopen

After a clean shutdown with full WAL checkpoint:

- `catalog.db-wal` → 0 bytes ✓
- `events_20260226.db-wal` → 0 bytes ✓
- Schema in `catalog.db` is valid (sqlite3 reads it fine)
- Reopen with turso → panic in `populate_indices`

The schema contains `sqlite_autoindex_*` entries (turso's own auto-indexes for PRIMARY KEY / UNIQUE constraints). Turso's schema parser fails to match one of these to its table's `unique_sets` when reading back a database it created in a previous process.

This is a turso pre-release bug. The WAL is not involved.

## What was fixed (still correct to keep)

### 1. Shutdown ordering in `main.rs` ✓
Task handles are now joined before checkpoint. This is correct regardless — checkpoint requires no active connections, so waiting for tasks to stop first is the right thing to do.

### 2. `EventStore::checkpoint()` drains result rows ✓
`PRAGMA wal_checkpoint(TRUNCATE)` must have its result rows consumed to complete execution. Fixed.

### 3. `EventStore::checkpoint()` covers all partitions ✓
Previously only checkpointed the active partition. Now walks the root directory and checkpoints every `.db` file.

### 4. WAL deletion removed from `store.rs` ✓
The `clear_wal_files` workaround was wrong — it deleted the schema itself (which lived in the WAL before first checkpoint). Removed.

## Remaining problem

Turso 0.5.0-pre.14 cannot reopen a database across process restarts. This needs to be resolved before harold is usable.

**Options:**

1. **Check if a newer turso pre-release fixes it** — the bug may have been fixed in a later commit on main. The repository is at `https://github.com/tursodatabase/turso`. Check commits after the 0.5.0-pre.14 tag.

2. **File a bug** — report the `populate_indices` assertion failure with a minimal reproduction. The schema is valid SQLite; sqlite3 reads it correctly.

3. **Reproduce minimally** — write a small Rust test that creates a turso db with a `PRIMARY KEY` column, drops the `Database`, reopens it, and checks if the panic occurs. This will confirm whether it's the `PRIMARY KEY` auto-index specifically causing the mismatch.
