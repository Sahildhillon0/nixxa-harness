# db — remembers

Persists and retrieves Nixxa's own sessions, transcripts and action logs. External memory (claude-mem, Obsidian) lives in `knowledge`.

| | |
|---|---|
| **Owns** | Schema, migrations, storage engine, search |
| **Does not own** | Deciding what to remember, how it is shown |
| **Contract** | Implements `Store` |
| **Depends on** | `contracts` only |
| **Comes alive** | In-memory in Stage 2, SQLite in Stage 5 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.

## Stage 5 status

- **`SqliteStore`** (`src/sqlite.rs`) implements `Store` on one SQLite file. The harness passes
  the path (`<data dir>/nixxa/nixxa.db`); this module reads no config. `SqliteStore::in_memory()`
  is for tests.
  - `rusqlite` with `bundled`: SQLite is compiled in, so no system library is needed on any OS.
    Pinned to 0.37, the newest that builds on the workspace's MSRV (1.85). D-053.
  - One connection on a dedicated `nixxa-db` thread; every call is a job sent to it, so no
    blocking SQLite call runs on the async runtime. A dropped call still finishes its write.
  - WAL mode, `synchronous = NORMAL` and a 5 s busy timeout, so two Nixxa instances can share
    the file. Writes take the lock up front (`BEGIN IMMEDIATE`).
  - A new file is created `0600` on Unix (SQLite gives the `-wal`/`-shm` files the same mode).
  - Errors are `StoreError`s, never panics. A file that isn't a database (or is damaged) and a
    file from a newer Nixxa are refused with an error naming the path, before anything is
    written; the file is never deleted or overwritten.
- **Schema and migrations (D-054):** forward-only steps in `MIGRATIONS`, embedded in the binary
  and tracked with `PRAGMA user_version`, each in its own transaction (the version is re-read
  inside it, so two instances migrate a new file once). Never edit a released step; add one.
  Messages and action records are stored as their exact JSON (`body`), so every `Content`
  variant round-trips, `Summary` included (D-056). Real columns only for what is queried:
  `sessions.started_at` (ordering), `sessions.first_message` (the session list),
  `messages.session_id`/`seq` (append order) and `messages.text` (search).
- **Search (D-055):** FTS5 (`unicode61`, case- and diacritic-insensitive) over the text of
  `Content::Text` messages. Every typed word must match, the last one as a prefix; typed text is
  quoted, never read as FTS5 syntax. Best match first (bm25), one hit per session, with a
  12-word snippet on one line (control characters become spaces).
- **`MemoryStore`** (`src/memory.rs`) keeps everything in memory: `--fake`, and the fallback
  when the database can't be opened (D-058).

Tests: `cargo test -p nixxa-db` — the conformance suite against an in-memory and a file
database, reopening a file, migrating an empty file, refusing a newer one, a corrupt file left
untouched, two connections writing at once, search ranking and case, and owner-only permissions.
