# db — remembers

Persists and retrieves Nixxa's own sessions, transcripts and action logs. External memory (claude-mem, Obsidian) lives in `knowledge`.

| | |
|---|---|
| **Owns** | Schema, migrations, storage engine, search |
| **Does not own** | Deciding what to remember, how it is shown |
| **Contract** | Implements `Store` |
| **Depends on** | `contracts` only |
| **Comes alive** | In-memory in Stage 2, SQLite in Stage 5 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. Crate exists but is empty until the stage above.
