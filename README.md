# Nixxa

> A sandboxed, terminal-native AI agent harness built from swappable blackbox modules.

Nixxa runs an AI agent **inside a sandbox** and lets you drive and watch it from a **TUI**.
Every part of the system is a sealed module behind a contract: the brain that thinks, the
controller that decides, the sandbox that acts, the db that remembers, and the harness you
look at. Replace one, and the rest never notice.

> **Status: Stage 0 — Foundations.** Design documents and folder skeleton only. No code yet.

---

## Modules

| Folder | Module | One-line job | Knows about |
|---|---|---|---|
| `contracts/` | Contracts | Shared types, traits and events. No logic. | nothing |
| `brain/` | Brain | Turns context into *intents* (talk, act, done) via an LLM. Never executes anything. | `contracts` |
| `controller/` | Controller | Runs the agent loop: asks the brain, checks policy, dispatches to the sandbox, records to the db. | `contracts` |
| `sandbox/` | Sandbox | Executes actions in isolation and returns observations. The only place anything "happens". | `contracts` |
| `db/` | DB | Persists sessions, transcripts, action logs and memory. | `contracts` |
| `harness/` | Harness | The TUI and the composition root: wires concrete modules together and renders events. | everything (wiring only) |

The rule that makes this work: **modules talk only through `contracts/`**. No module imports
a sibling. Want a new LLM provider? Touch `brain/`. Postgres instead of SQLite? Touch `db/`.
Firecracker instead of bubblewrap? Touch `sandbox/`. Details in [ARCHITECTURE.md](ARCHITECTURE.md).

```
nixxa-harness/
├── contracts/     # the language every module speaks
├── brain/         # thinks
├── controller/    # decides
├── sandbox/       # acts (isolated)
├── db/            # remembers
├── harness/       # shows + wires (TUI binary)
├── README.md
├── ARCHITECTURE.md
├── DESIGN.md
└── AGENT.md
```

## Roadmap

Nixxa is built in stages. Each stage ends with something that runs, and has explicit exit
criteria (see [ARCHITECTURE.md § Stages](ARCHITECTURE.md#stages)).

| Stage | Name | Delivers | Status |
|---|---|---|---|
| 0 | Foundations | Repo, docs, folder skeleton | **in progress** |
| 1 | Contracts | Cargo workspace, shared types/traits/events, CI, boundary checks | planned |
| 2 | Walking skeleton | TUI + minimal controller loop with fake brain, fake sandbox, in-memory db | planned |
| 3 | Real brain | First LLM provider, streaming, context-window management | planned |
| 4 | Real sandbox | Out-of-process isolated sandbox (Linux: bubblewrap + Landlock + seccomp) | planned |
| 5 | Real memory | SQLite store, session resume, transcript search | planned |
| 6 | Policy & control | Approval modes, budgets, interrupts, audit log | planned |
| 7 | Hardening & reach | More providers, tool plugins, config, packaging, release | planned |

## Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) — modules, contracts, data flow, process model, stages
- [DESIGN.md](DESIGN.md) — principles, TUI design, interaction model, decision log
- [AGENT.md](AGENT.md) — rules for AI agents (and humans) working in this repo

## Planned stack

Rust (Cargo workspace, one crate per module) · [ratatui](https://ratatui.rs) + crossterm ·
tokio · SQLite · bubblewrap / Landlock / seccomp on Linux.

## License

To be decided.
