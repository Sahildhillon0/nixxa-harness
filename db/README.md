# db — remembers

Persists and retrieves sessions, transcripts, action logs and memory.

| | |
|---|---|
| **Owns** | Schema, migrations, storage engine, search |
| **Does not own** | Deciding what to remember, how it is shown |
| **Contract** | Implements `Store` |
| **Depends on** | `contracts` only |
| **Comes alive** | In-memory in Stage 2, SQLite in Stage 5 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. No code yet (Stage 0).
