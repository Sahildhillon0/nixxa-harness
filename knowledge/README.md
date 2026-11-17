# knowledge — connects

Gives the agent access to memory and context that lives outside Nixxa, and writes back to it.
Runs on the host; never inside, or reachable from, the sandbox.

| | |
|---|---|
| **Owns** | One adapter per source under `sources/` (claude-mem, Obsidian vault, CodeGraph, Context7), a private MCP client (stdio + HTTP), cross-OS discovery, health checks, result shaping |
| **Does not own** | Deciding when to query or write (controller), Nixxa's own transcripts (db) |
| **Contract** | Implements `Knowledge` |
| **Depends on** | `contracts` only (gets `Credentials` from the harness) |
| **Comes alive** | Fake in Stage 2; Obsidian + claude-mem in Stage 6; CodeGraph + Context7 in Stage 7 |

| Source | Default | Found via (Linux / macOS / Windows) |
|---|---|---|
| claude-mem | on | `~/.claude-mem` / same / `%USERPROFILE%\.claude-mem` (or `CLAUDE_MEM_DATA_DIR`) |
| Obsidian vault | on | Obsidian's `obsidian.json` in `~/.config/obsidian` (+ Flatpak/Snap) / `~/Library/Application Support/obsidian` / `%APPDATA%\obsidian` |
| CodeGraph | on, if the workspace has `.codegraph/` | executable on `PATH` |
| Context7 | on | remote MCP endpoint (optional API key from `accounts`) |

See [ARCHITECTURE.md § 5](../ARCHITECTURE.md#5-integrations-the-knowledge-module). Crate exists but is empty until the stage above.
