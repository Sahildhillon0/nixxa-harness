# Nixxa

> A sandboxed, terminal-native AI agent harness built from swappable blackbox modules.

Nixxa runs an AI agent **inside a sandbox** and lets you drive and watch it from a **TUI**.
Every part of the system is a sealed module behind a contract: the brain that thinks, the
controller that decides, the sandbox that acts, the db that remembers, the knowledge layer
that connects to your memory, notes, code index and docs, and the harness you look at.
Replace one, and the rest never notice.

> **Status: Stage 0 — Foundations.** Design documents and folder skeleton only. No code yet.

---

## Modules

| Folder | Module | One-line job | Knows about |
|---|---|---|---|
| `contracts/` | Contracts | Shared types, traits and events. No logic. | nothing |
| `brain/` | Brain | Turns context into *intents* (talk, act, done) via an LLM. Never executes anything. | `contracts` |
| `controller/` | Controller | Runs the agent loop: asks the brain, checks policy, routes actions to the sandbox or knowledge, records to the db. | `contracts` |
| `sandbox/` | Sandbox | Executes actions in isolation and returns observations. The only place commands run. | `contracts` |
| `db/` | DB | Persists Nixxa's own sessions, transcripts and action logs. | `contracts` |
| `knowledge/` | Knowledge | Connects to external memory and context: claude-mem, Obsidian vault, CodeGraph, Context7. | `contracts` |
| `harness/` | Harness | The TUI and the composition root: wires concrete modules together and renders events. | everything (wiring only) |

The rule that makes this work: **modules talk only through `contracts/`**. No module imports
a sibling. Want a new LLM provider? Touch `brain/`. Postgres instead of SQLite? Touch `db/`.
Notion instead of Obsidian? Touch `knowledge/`. Details in [ARCHITECTURE.md](ARCHITECTURE.md).

```
nixxa-harness/
├── contracts/     # the language every module speaks
├── brain/         # thinks
├── controller/    # decides
├── sandbox/       # acts (isolated)
├── db/            # remembers (Nixxa's own sessions)
├── knowledge/     # connects (claude-mem, Obsidian, CodeGraph, Context7)
├── harness/       # shows + wires (TUI binary)
├── README.md
├── ARCHITECTURE.md
├── DESIGN.md
└── AGENT.md
```

## Built-in integrations

On by default, auto-detected, on Linux, macOS and Windows. If one is missing, Nixxa says so in
the status bar and carries on without it.

| Integration | What the agent gets | How Nixxa reaches it | Stage |
|---|---|---|---|
| **claude-mem** | Long-term memory shared with Claude Code: search past observations, timelines, session summaries; Nixxa sessions are written back | claude-mem's local worker service and MCP search server | 6 |
| **Obsidian vault** | Your notes as context: search, read, follow `[[links]]`; a session note written at the end of each session | Directly on the vault's Markdown files; vault found via Obsidian's own vault registry. Obsidian doesn't need to be running | 6 |
| **CodeGraph** | Semantic code navigation of the workspace: symbols, source, call paths | CodeGraph MCP server / CLI, run against the workspace, only when a `.codegraph/` index exists | 7 |
| **Context7** | Current, version-specific library documentation | Context7 MCP server | 7 |

## Roadmap

Nixxa is built in stages. Each stage ends with something that runs, and has explicit exit
criteria (see [ARCHITECTURE.md § Stages](ARCHITECTURE.md#stages)).

| Stage | Name | Delivers | Status |
|---|---|---|---|
| 0 | Foundations | Repo, docs, folder skeleton | **in progress** |
| 1 | Contracts | Cargo workspace, shared types/traits/events, CI on Linux/macOS/Windows, boundary checks | planned |
| 2 | Walking skeleton | TUI + minimal controller loop with fake brain, sandbox, store and knowledge | planned |
| 3 | Real brain | First LLM provider, streaming, context-window management | planned |
| 4 | Real sandbox | Out-of-process isolated sandbox on Linux (bubblewrap + Landlock + seccomp) | planned |
| 5 | Real memory | SQLite store, session resume, transcript search | planned |
| 6 | Knowledge I — personal | MCP client, **Obsidian vault** + **claude-mem**, cross-OS discovery, session write-back | planned |
| 7 | Knowledge II — code & docs | **CodeGraph** + **Context7** | planned |
| 8 | Policy & control | Approval modes, budgets, interrupts, audit log | planned |
| 9 | Every OS | Sandbox backends for macOS and Windows | planned |
| 10 | Hardening & release | More providers, external tools, packaging, `v0.1.0` | planned |

## Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) — modules, contracts, data flow, integrations, process model, stages
- [DESIGN.md](DESIGN.md) — principles, TUI design, interaction model, decision log
- [AGENT.md](AGENT.md) — rules for AI agents (and humans) working in this repo

## Planned stack

Rust (Cargo workspace, one crate per module) · [ratatui](https://ratatui.rs) + crossterm ·
tokio · SQLite · MCP (stdio + HTTP) · Linux sandbox: bubblewrap / Landlock / seccomp.
Targets Linux, macOS and Windows.

## License

To be decided.
