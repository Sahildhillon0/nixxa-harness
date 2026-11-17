# Architecture

This document describes how Nixxa is put together: the modules, the contracts between them,
how data flows, how the built-in integrations plug in, where the trust boundaries are, and the
order in which it gets built.

> No code exists yet (Stage 0). Type and trait names below are design intent, not API.

---

## 1. The core idea: blackbox modules

Nixxa is seven folders. Six do work; one (`contracts/`) defines how they talk.

```
                        ┌──────────────────────────┐
                        │         harness          │  TUI + composition root
                        │  (wires everything up)   │
                        └─────────────┬────────────┘
                                      │ events ▲  │ user input / approvals
                                      ▼        │  ▼
                        ┌──────────────────────────┐
                        │        controller        │  agent loop + policy + routing
                        └──┬───────┬───────┬────┬──┘
                 Brain     │       │       │    │  Knowledge
                 trait     ▼       │       │    ▼  trait
                   ┌────────────┐  │       │  ┌──────────────────────────────┐
                   │   brain    │  │       │  │          knowledge           │
                   └────────────┘  │       │  │ claude-mem · Obsidian vault  │
                         Sandbox   │       │  │ CodeGraph  · Context7        │
                         trait     ▼       ▼  └──────────────────────────────┘
                     ┌──────────────┐  ┌────────────┐
                     │   sandbox    │  │     db     │  Store trait
                     │  (isolated)  │  └────────────┘
                     └──────────────┘

             every arrow is a type or trait defined in  contracts/
```

### The dependency rule

1. `contracts` depends on nothing in the workspace.
2. `brain`, `controller`, `sandbox`, `db`, `knowledge` depend **only** on `contracts`.
3. `harness` depends on all of them, and is the **only** place concrete implementations are
   chosen and plugged together (the composition root).
4. No module imports a sibling. Ever. If two modules need to share something, it goes into
   `contracts`.

The controller does not know *which* brain, sandbox, store or knowledge sources it has — it
receives trait objects from the harness. That is what makes every module swappable.

This rule is enforced mechanically from Stage 1 (a CI check over `cargo metadata` that fails
if any crate depends on a sibling other than `contracts`).

---

## 2. Modules

Each module is one Cargo crate in its own folder. Its public surface is: the contract trait
implementation, a constructor that takes a config struct, and nothing else. Everything
internal is `pub(crate)`.

### `contracts/` — the shared language
- **Owns:** domain types, the module traits, the event enum, the error enums, the
  conformance test suites every implementation must pass.
- **Does not own:** any logic, I/O, or dependencies beyond serde-level basics.
- **Changing it** is a deliberate act: it is the only change that can ripple into other modules.

### `brain/` — thinks
- **Job:** given a `Context` (conversation, available tools, primer, system instructions),
  produce a stream of `BrainEvent`s ending in an `Intent`.
- **Owns:** LLM provider adapters (`brain/providers/*`), prompt assembly, tool-schema
  rendering, token counting, context-window trimming/summarisation, streaming.
- **Does not own:** executing anything, deciding whether an action is allowed, storage.
- **Pure-ish:** the brain has network access to its provider and nothing else.

### `controller/` — decides
- **Job:** run the agent loop. It is the only orchestrator.
- **Owns:** the loop state machine, the policy gate (allow / ask / deny), **routing** (sandbox
  actions → `Sandbox`, knowledge actions → `Knowledge`), session primer and write-back,
  budgets (steps, tokens, time, money), interrupts, retries.
- **Does not own:** how thinking, execution, storage, knowledge lookup or rendering work.

### `sandbox/` — acts
- **Job:** execute a `SandboxAction` in isolation and return an `Observation`.
- **Owns:** the isolation mechanism (one backend per OS), the workspace mount, resource limits,
  network policy, built-in actions (exec, read file, write file, list dir, …), output capture
  and truncation.
- **Does not own:** deciding what to run. It enforces its own hard limits and does not trust
  the controller to have been strict enough.

### `db/` — remembers (Nixxa's own records)
- **Job:** persist and retrieve Nixxa sessions.
- **Owns:** schema, migrations, the storage engine, transcript and action-log storage, search.
- **Does not own:** external memory systems (that is `knowledge`), deciding what is worth
  remembering, how it is shown.

### `knowledge/` — connects
- **Job:** give the agent access to memory and context that lives *outside* Nixxa, and write
  back to it.
- **Owns:** one adapter per source (`knowledge/sources/{claude_mem,obsidian,codegraph,context7}`),
  a private MCP client (stdio + HTTP), cross-OS discovery of each source, health checks,
  result shaping and truncation.
- **Does not own:** deciding when to query or write (controller), Nixxa's own transcripts (db).
- **Runs on the host, never in the sandbox.** Details in §5.

### `harness/` — shows and wires
- **Job:** be the program the user runs.
- **Owns:** the `main` binary, config loading, the ratatui TUI, input handling, approval
  prompts, rendering the event stream, choosing and constructing concrete modules.
- **Does not own:** any agent logic. If the harness contains an `if` about what the agent
  should do, it belongs in the controller.

---

## 3. Contracts (planned)

### Core types

| Type | Meaning |
|---|---|
| `SessionId`, `TurnId`, `ActionId` | Opaque identifiers |
| `Message` | A conversation entry: role (user / agent / system / observation) + content |
| `Context` | Everything the brain needs for one think step: messages, tool specs, primer, instructions, limits |
| `Intent` | What the brain wants: `Say(text)`, `Act(Action)`, `Done(summary)` |
| `Action` | `Sandbox(SandboxAction)` or `Knowledge(KnowledgeAction)` — the controller routes on this |
| `SandboxAction` | `Exec`, `ReadFile`, `WriteFile`, `ListDir`, … |
| `KnowledgeAction` | `Query { source, tool, args }` (read) or `Record { source, entry }` (write) |
| `Primer` | Context gathered at session start from knowledge sources (recent memory, project note) |
| `Observation` | A result: output, error, timing, truncation flag, source |
| `Verdict` | The policy gate's answer: `Allow`, `Ask(reason)`, `Deny(reason)` |
| `Event` | Everything the harness can render: tokens, intent proposed, approval needed, action started/finished, knowledge source status, budget update, error, turn done |

### Traits

| Trait | Implemented by | Core operations |
|---|---|---|
| `Brain` | `brain` | think(context) → stream of brain events; describe() |
| `Sandbox` | `sandbox` | execute(action) → observation; capabilities(); health(); reset() |
| `Store` | `db` | create/load session; append message; append action record; list/search sessions |
| `Knowledge` | `knowledge` | sources() → status of each; tools() → tool specs; primer(workspace) → `Primer`; query(action) → observation; record(entry) → ack |
| `ApprovalPort` | `harness` | ask the human about an action → `Verdict` |
| `EventSink` | `harness` | receive `Event`s from the controller |

The controller is the one consumer of all six. The harness constructs it with all six.

### Errors
Each trait has its own error enum in `contracts` (e.g. `BrainError::RateLimited`,
`SandboxError::Timeout`, `KnowledgeError::SourceUnavailable`). Modules map internal errors
into these; internal error types never leak across a boundary.

### Conformance tests
`contracts` ships a test suite per trait. Any implementation (real or fake) must pass it.

---

## 4. The agent loop

```
 session start ──► controller
                   │ 0. Knowledge.primer(workspace) ─► recent claude-mem context,
                   │    matching vault project note  → stored as the session Primer
 user input ──►    │ 1. Store.append(user message)
                   │ 2. build Context (messages + primer + tool specs from Knowledge + Sandbox)
                   ▼
                 Brain.think(context) ──► streams tokens ──► Event ──► harness
                   │
                   ▼ Intent
       ┌───────────┼──────────────────────────┐
       │ Say       │ Act(action)              │ Done
       ▼           ▼                          ▼
   record +    3. policy gate              record, end turn
   emit        ├─ Deny  → "denied" observation
               ├─ Ask   → ApprovalPort ─► harness prompt ─► Allow/Deny
               └─ Allow
                   │
                   ▼
               4. route:  Sandbox(a)   ──► Sandbox.execute(a)
                          Knowledge(a) ──► Knowledge.query / record(a)
                   │
                   ▼ Observation
               5. Store.append(action + observation), emit Event
               6. budgets left? ── yes ──► back to 2
                                 └ no ───► stop, tell the user

 session end ──► 7. write-back (if enabled): session summary → claude-mem,
                    session note → Obsidian vault  (logged like any action)
```

The user can interrupt at any point; the controller cancels the in-flight brain stream,
sandbox action or knowledge call and records the interruption.

---

## 5. Integrations (the `knowledge` module)

All four are **on by default** and **auto-detected**. Each reports a status
(`ready` / `missing` / `error: <reason>`), shown in the TUI status bar. A missing source
removes its tools from the brain's tool list; it never stops Nixxa from starting.

Each source is an adapter behind the same `Knowledge` trait. Adding a new source (Notion,
Logseq, a team wiki) is a new folder under `knowledge/sources/` plus a config entry.

### claude-mem — shared long-term memory
- **What:** persistent memory shared with Claude Code: observations, session summaries and
  search across past work.
- **Read:** primer at session start (recent context for this project); agent tools for
  search, timeline and fetching full observations.
- **Write:** Nixxa sessions are recorded into claude-mem (prompts, actions, summary) so memory
  flows both ways between Claude Code and Nixxa.
- **Reached via:** claude-mem's local worker service (host/port read from claude-mem's own
  settings file in its data directory) and its MCP search server. The exact API is pinned
  against a specific claude-mem version in Stage 6 and covered by a contract test.
- **Discovery (all OSs):** data directory `~/.claude-mem` (`%USERPROFILE%\.claude-mem` on
  Windows), overridable by `CLAUDE_MEM_DATA_DIR` or Nixxa config.

### Obsidian vault — your notes
- **What:** your vault as searchable, linkable context, plus a written record of each session.
- **Read:** search (file name, frontmatter, tags, full text), read a note, follow `[[wikilinks]]`
  and backlinks; primer = the project note whose name or frontmatter matches the workspace.
- **Write:** at session end, a session note (frontmatter + summary + actions + links) into a
  configurable folder; agent-proposed edits go through the policy gate. Writes are atomic,
  additive (never delete or rename), and never touch `.obsidian/`.
- **Reached via:** plain file access. A vault is a folder of Markdown, so Obsidian does not
  need to be running and no plugin is required.
- **Discovery (all OSs):** read Obsidian's vault registry `obsidian.json` and choose the vault
  marked open, else the most recently used; overridable in config.

  | OS | Registry location |
  |---|---|
  | Linux | `~/.config/obsidian/obsidian.json` (Flatpak: `~/.var/app/md.obsidian.Obsidian/config/obsidian/`, Snap: `~/snap/obsidian/current/.config/obsidian/`) |
  | macOS | `~/Library/Application Support/obsidian/obsidian.json` |
  | Windows | `%APPDATA%\obsidian\obsidian.json` |

### CodeGraph — code intelligence
- **What:** semantic navigation of the workspace's code: symbol source, callers/callees, call
  paths including dynamic dispatch.
- **When:** only if the workspace already has a `.codegraph/` index. **Creating an index is the
  user's decision**; Nixxa offers a command for it but never indexes on its own.
- **Reached via:** CodeGraph's MCP server (falling back to its CLI), run on the host, read-only
  against the workspace directory.
- **Discovery (all OSs):** executable on `PATH` or a path in config; the health check runs it
  once and reports a clear error if it fails (e.g. a version-manager shim pointing at a runtime
  that does not provide it).

### Context7 — library documentation
- **What:** current, version-specific documentation and examples for libraries and frameworks.
- **Reached via:** the Context7 MCP server (remote HTTP by default, local stdio optional);
  optional API key from config, held on the host.
- **Privacy:** only library names and topics are sent — never code, vault or memory content.
  Every call is visible in the activity pane.

### Policy defaults for knowledge actions

| Action | LOCKED | ASK (default) | AUTO |
|---|---|---|---|
| Query (any source) | allow | allow | allow |
| Record / write (claude-mem, vault) by the agent | deny | ask | allow |
| Session-end write-back (configured by the user, not proposed by the agent) | per config | per config | per config |

---

## 6. Process model and trust boundaries

| Zone | Contains | Trust |
|---|---|---|
| Host process | harness, controller, brain, db, knowledge | trusted code, **untrusted brain output**, **untrusted retrieved content** |
| Sandbox | sandbox runtime + everything the agent runs | **untrusted** |
| External services | LLM provider, Context7 | receive only what the policy lets out |

- **Brain output is untrusted.** Model output can be manipulated by prompt injection. Every
  `Act` goes through the controller's policy gate.
- **Retrieved content is untrusted too.** Notes, memories, docs and command output are data,
  not instructions. They are marked as such in the context the brain sees.
- **Private knowledge stays on the host.** The sandbox never mounts the vault or the
  claude-mem data directory, and never talks to knowledge sources directly. The agent reaches
  them only through the controller.
- **Your knowledge does go to the LLM provider** whenever it enters the context. The TUI shows
  when vault or memory content is being sent. A per-source "never send" switch is part of
  Stage 8 policy.
- **Secrets never enter the sandbox.** API keys live in the host process. The sandbox gets a
  clean environment.
- **The sandbox enforces its own limits** (CPU, memory, wall time, output size, file system
  scope, network) independently of the controller.
- **Network is off by default** inside the sandbox.

### How the process model evolves
- **Stages 2–3:** single process. The sandbox is a fake, labelled *not isolated* in the TUI.
- **Stage 4:** the sandbox moves out of process: a thin client launches `nixxa-sandboxd`
  inside bubblewrap + Landlock + seccomp (Linux) and talks to it over a framed protocol on
  stdio. **The trait does not change.**
- **Stage 9:** backends for macOS (Seatbelt profile) and Windows (AppContainer or
  WSL2/container — see open questions). Until an OS has a backend, Nixxa runs there with
  sandbox actions **disabled** (chat and knowledge still work) unless a container runtime
  (Docker/Podman) is available as a fallback backend.

---

## 7. Swap guide

| I want to… | Touch only |
|---|---|
| Add or change an LLM provider / model | `brain/providers/` |
| Change how context is trimmed or summarised | `brain/` |
| Change approval rules, budgets, loop behaviour, routing | `controller/` |
| Switch SQLite → Postgres (or anything) | `db/` + one line in `harness` wiring |
| Switch or add a sandbox backend | `sandbox/` + one line in `harness` wiring |
| Add a knowledge source (Notion, Logseq, a wiki…) | `knowledge/sources/<name>/` + config |
| Change how the Obsidian vault is found or written to | `knowledge/sources/obsidian/` |
| Add a new built-in sandbox action | `contracts` → `sandbox` → `controller` (policy) → `brain` (tool spec) |
| Change the look, layout or keybindings | `harness/` |
| Replace the TUI with a web UI | new frontend crate implementing `ApprovalPort` + `EventSink`; nothing else |

If a change needs more than one row, it is a contract change — see [AGENT.md](AGENT.md).

---

## 8. Cross-cutting concerns

- **Platforms:** Linux, macOS and Windows from Stage 1 (CI matrix on all three). All paths go
  through one platform-paths helper per module (home, config and data dirs); no hard-coded
  `/` separators or `~` expansion.
- **Configuration:** only the harness reads config (`<config dir>/nixxa/config.toml` + env:
  `~/.config` on Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on Windows). It
  hands each module its own typed config section, including each knowledge source's
  enable/disable switch and path overrides. Modules never read files or env vars themselves.
  The one exception is knowledge-source *discovery* (reading `obsidian.json` and claude-mem
  settings), which is part of the `knowledge` module's job.
- **Logging/tracing:** `tracing` everywhere; the harness owns the subscriber. Logs go to a file,
  never to the terminal the TUI is drawing on.
- **Async:** tokio. Contract traits are async. Cancellation is explicit.
- **Testing:** each module is tested alone against its contract with fakes for everything
  else. Knowledge adapters are tested against fixture vaults and recorded MCP sessions, never
  against a real user vault.

---

## 9. Planned workspace layout

```
nixxa-harness/
├── Cargo.toml              # [workspace] members = all seven
├── contracts/   → crate nixxa-contracts   (lib)
├── brain/       → crate nixxa-brain       (lib)   brain/src/providers/<name>/
├── controller/  → crate nixxa-controller  (lib)   controller/src/policy/
├── sandbox/     → crate nixxa-sandbox     (lib + bin nixxa-sandboxd)  sandbox/src/backends/<os>/
├── db/          → crate nixxa-db          (lib)   db/migrations/
├── knowledge/   → crate nixxa-knowledge   (lib)   knowledge/src/{mcp,sources/<name>}/
└── harness/     → crate nixxa-harness     (bin `nixxa`)   harness/src/tui/
```

---

## Stages

Every stage ends with something that runs and a checklist that must be true before moving on.

### Stage 0 — Foundations *(current)*
- Repo, README, ARCHITECTURE, DESIGN, AGENT, one folder per module with its own README.
- **Exit:** docs agreed; module boundaries, integrations and stage plan accepted.

### Stage 1 — Contracts
- Cargo workspace with seven crates (empty bodies except `contracts`).
- All types, traits, events and error enums from §3, including `Knowledge` and the
  `Action` split. Conformance test suites.
- CI on Linux, macOS and Windows: fmt, clippy (`-D warnings`), test, dependency-rule check.
- **Exit:** workspace builds on all three OSs; CI fails if any crate imports a sibling.

### Stage 2 — Walking skeleton
- Harness: ratatui app with conversation pane, activity pane, input line, status bar
  (including knowledge-source indicators).
- Controller: the loop from §4 with routing; policy = "ask for everything".
- Fakes: scripted brain, echo sandbox, in-memory store, fake knowledge source.
- **Exit:** type a message, watch a fake agent query fake knowledge and propose a sandbox
  action, approve it, see the observation — end to end, every module in place.

### Stage 3 — Real brain
- First real provider with streaming and tool calling; API key from config/env.
- Token counting and context-window trimming.
- **Exit:** a real model chats and proposes real actions (still executed by fakes).

### Stage 4 — Real sandbox (Linux)
- `nixxa-sandboxd` out of process, bubblewrap + Landlock + seccomp, network off, workspace
  mounted read-write, resource limits. Built-in actions: exec, read, write, list.
- **Exit:** the agent runs real commands that provably cannot touch the host outside the
  workspace or reach the network (with tests that try).

### Stage 5 — Real memory
- SQLite store, migrations, session list and resume, transcript search.
- **Exit:** quit mid-session, relaunch, resume where you left off.

### Stage 6 — Knowledge I: personal (Obsidian + claude-mem)
- `knowledge` crate with its MCP client (stdio + HTTP) and source-status reporting.
- Obsidian: cross-OS vault discovery, search/read/links, project-note primer, session-note
  write-back.
- claude-mem: discovery, primer, search/timeline/get tools, session write-back.
- **Exit:** on a fresh machine with Obsidian and claude-mem installed, Nixxa finds both with
  zero config on Linux, macOS and Windows; a session starts primed with memory and the
  project note, and ends with a note in the vault and a session in claude-mem that Claude
  Code can then find. With neither installed, Nixxa starts normally and says so.

### Stage 7 — Knowledge II: code & docs (CodeGraph + Context7)
- CodeGraph: detect `.codegraph/`, explore/symbol/call-path tools, user-triggered indexing
  command, clear health errors.
- Context7: resolve-library and query-docs tools, optional API key.
- **Exit:** in an indexed repo the agent answers "who calls X" via CodeGraph instead of grep,
  and looks up current docs for a named library via Context7, with both calls visible in the
  activity pane.

### Stage 8 — Policy & control
- Approval modes (locked / ask / autonomous), per-action and per-source rules (including
  "never send this source to the provider"), budgets (steps, tokens, time, cost), interrupt,
  audit log view.
- **Exit:** the agent can run unattended within a budget and stops cleanly at the limit.

### Stage 9 — Every OS
- Sandbox backends for macOS and Windows; container fallback backend.
- **Exit:** the Stage 4 escape tests pass on all three OSs (or sandbox actions are clearly
  disabled where no backend is available).

### Stage 10 — Hardening & release
- More providers, user-added MCP tools, config UX, packaging (Linux, macOS, Windows), docs
  site, first release.
- **Exit:** `v0.1.0` tagged and installable on all three OSs.
