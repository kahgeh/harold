# Fix WAL Lifecycle

## Status: RESOLVED

## Root Cause

The panic was:
```
thread 'main' panicked at turso_core-0.3.2/schema.rs:613:
all automatic indexes parsed from sqlite_schema should have been consumed, but 1 remain
```

**The bug is in turso's `ALTER TABLE ... ADD COLUMN` implementation.** When a table has `UNIQUE` constraints, `ALTER TABLE` corrupts `sqlite_schema` by leaving an orphan autoindex entry (`sqlite_autoindex_events_2`) that has no matching constraint in the updated `CREATE TABLE` SQL. On reopen, turso finds 2 automatic index entries in `sqlite_schema` but only 1 constraint in the table definition → assertion failure.

Verified: sqlite3 reports `malformed database schema - orphan index` when reading the turso-created WAL database. This confirms the schema is invalid, not turso's parser.

This affects **all turso versions** tested: 0.3.2, 0.4.4, 0.5.0-pre.14.

## Fix

**events crate (`src/migration.rs`)**: Merged partition migrations 001 and 002 into a single `CREATE TABLE` that includes `trace_id TEXT` from the start. Eliminated the `ALTER TABLE events ADD COLUMN trace_id TEXT` that triggered the turso bug.

**events crate (`src/catalog.rs`)**: Changed `Catalog.db` from `Database` to `Option<Database>`. `open_with_pool` previously opened a direct `Database` AND the pool opened another — two simultaneous handles on the same file. Now the pool is the sole owner when present. Checkpoint uses pool-aware `get_connection()`.

**harold (`src/main.rs`)**: Task handles joined before WAL checkpoint. Checkpoint requires no active connections.

**harold (`src/store.rs`)**: Removed `clear_wal_files()` — it was deleting the WAL which contains the entire schema until first checkpoint.

## What was confirmed correct

- `PRAGMA wal_checkpoint(TRUNCATE)` requires its result rows to be consumed → fixed (while loop drains rows)
- Checkpoint covers all partition `.db` files, not just the active one
- Clean shutdown produces 0-byte WAL files on all databases
- Three consecutive restarts confirmed stable after the fix

## Filed as turso bug

The `ALTER TABLE ... ADD COLUMN` corruption of sqlite_schema when UNIQUE constraints exist should be reported upstream. Reproduction:

```rust
conn.execute("CREATE TABLE t (id TEXT, a TEXT, b TEXT, PRIMARY KEY (id), UNIQUE (a, b))", ()).await?;
conn.execute("ALTER TABLE t ADD COLUMN c TEXT", ()).await?;
// Reopen → panic: "all automatic indexes ... but 1 remain"
```
