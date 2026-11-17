# Architecture

This document describes how Nixxa is put together: the modules, the contracts between them,
how data flows, where the trust boundaries are, and the order in which it gets built.

> No code exists yet (Stage 0). Type and trait names below are design intent, not API.

---

## 1. The core idea: blackbox modules

Nixxa is six folders. Five do work; one (`contracts/`) defines how they talk.

```
                     ┌──────────────────────────┐
                     │         harness          │  TUI + composition root
                     │  (wires everything up)   │
                     └─────────────┬────────────┘
                                   │ events ▲  │ user input / approvals
                                   ▼        │  ▼
                     ┌──────────────────────────┐
                     │        controller        │  the agent loop + policy
                     └──┬──────────┬─────────┬──┘
              Brain     │          │ Sandbox │    Store
              trait     ▼          │  trait  ▼    trait
                 ┌────────────┐    │   ┌────────────┐
                 │   brain    │    │   │     db     │
                 └────────────┘    ▼   └────────────┘
                          ┌──────────────────┐
                          │     sandbox      │  isolated execution
                          └──────────────────┘

          every arrow is a type or trait defined in  contracts/
```

### The dependency rule

1. `contracts` depends on nothing in the workspace.
2. `brain`, `controller`, `sandbox`, `db` depend **only** on `contracts`.
3. `harness` depends on all of them, and is the **only** place concrete implementations are
   chosen and plugged together (the composition root).
4. No module imports a sibling. Ever. If two modules need to share something, it goes into
   `contracts`.

The controller does not know *which* brain, sandbox or store it has — it receives trait
objects from the harness. That is what makes every module swappable.

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
- **Job:** given a `Context` (conversation, available tools, system instructions), produce a
  stream of `BrainEvent`s ending in an `Intent`.
- **Owns:** LLM provider adapters (`brain/providers/*`), prompt assembly, tool-schema
  rendering, token counting, context-window trimming/summarisation, streaming.
- **Does not own:** executing anything, deciding whether an action is allowed, storage.
- **Pure-ish:** the brain has network access to its provider and nothing else. It cannot touch
  the filesystem or the sandbox.

### `controller/` — decides
- **Job:** run the agent loop. It is the only orchestrator.
- **Owns:** the loop state machine, the policy gate (allow / ask / deny), budgets (steps,
  tokens, time, money), interrupts, retries, turning brain intents into sandbox actions.
- **Does not own:** how thinking, execution, storage or rendering work.

### `sandbox/` — acts
- **Job:** execute an `Action` in isolation and return an `Observation`.
- **Owns:** the isolation mechanism, the workspace mount, resource limits, network policy,
  the built-in action set (exec, read file, write file, list dir, …), output capture and
  truncation.
- **Does not own:** deciding what to run. It executes what the controller hands it, within
  its own hard limits — it does not trust the controller to have been strict enough.

### `db/` — remembers
- **Job:** persist and retrieve sessions.
- **Owns:** schema, migrations, the storage engine, transcript and action-log storage,
  search, long-term memory records.
- **Does not own:** deciding what is worth remembering (controller) or how it is shown (harness).

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
| `Context` | Everything the brain needs for one think step: messages, tool specs, instructions, limits |
| `Intent` | What the brain wants: `Say(text)`, `Act(Action)`, `Done(summary)` |
| `Action` | A concrete request to the sandbox: `Exec`, `ReadFile`, `WriteFile`, `ListDir`, … |
| `Observation` | The sandbox's result: stdout/stderr/exit code, file contents, error, timing, truncation flag |
| `Verdict` | The policy gate's answer: `Allow`, `Ask(reason)`, `Deny(reason)` |
| `Event` | Everything the harness can render: tokens streaming, intent proposed, approval needed, action started/finished, budget update, error, turn done |

### Traits

| Trait | Implemented by | Core operations |
|---|---|---|
| `Brain` | `brain` | think(context) → stream of brain events; describe() → model/provider info |
| `Sandbox` | `sandbox` | execute(action) → observation; capabilities(); health(); reset() |
| `Store` | `db` | create/load session; append message; append action record; list/search sessions |
| `ApprovalPort` | `harness` | ask the human about an action → `Verdict` (async) |
| `EventSink` | `harness` | receive `Event`s from the controller |

The controller is the one consumer of `Brain`, `Sandbox`, `Store`, `ApprovalPort` and
`EventSink`. The harness constructs the controller with all five.

### Errors
Each trait has its own error enum in `contracts` (e.g. `BrainError::RateLimited`,
`SandboxError::Timeout`). Modules map their internal errors into these; internal error types
never leak across a boundary.

### Conformance tests
`contracts` ships a test suite per trait. Any implementation (real or fake) must pass it.
A new SQLite store and a new Postgres store prove they are interchangeable by passing the same
suite.

---

## 4. The agent loop

```
 user input ──► controller
                   │ 1. Store.append(user message)
                   │ 2. build Context from Store
                   ▼
                 Brain.think(context) ──► streams tokens ──► Event ──► harness
                   │
                   ▼ Intent
       ┌───────────┼────────────────────────┐
       │ Say       │ Act(action)            │ Done
       ▼           ▼                        ▼
   record +    3. policy gate            record,
   emit        ├─ Deny  → observation    end turn
               │          "denied"
               ├─ Ask   → ApprovalPort ─► harness prompt ─► Allow/Deny
               └─ Allow
                   │
                   ▼
               4. Sandbox.execute(action) ──► Observation
                   │
                   ▼
               5. Store.append(action + observation), emit Event
               6. budgets left? ── yes ──► back to 2
                                 └ no ───► stop, tell the user
```

The user can interrupt at any point; the controller cancels the in-flight brain stream or
sandbox action and records the interruption.

---

## 5. Process model and trust boundaries

| Zone | Contains | Trust |
|---|---|---|
| Host process | harness, controller, brain, db | trusted code, **untrusted brain output** |
| Sandbox | sandbox runtime + everything the agent runs | **untrusted** |

- **Brain output is untrusted.** Model output can be manipulated by prompt injection from
  files or command output. Every `Act` goes through the controller's policy gate.
- **Secrets never enter the sandbox.** API keys live in the host process and are passed only
  to the brain. The sandbox gets a clean environment.
- **The sandbox enforces its own limits** (CPU, memory, wall time, output size, file system
  scope, network) independently of the controller. Defence in depth.
- **Network is off by default** inside the sandbox; allow-listing is a later-stage policy feature.

### How the process model evolves
- **Stages 2–3:** single process. The sandbox is a fake (or a plain subprocess runner in a
  temp dir, clearly labelled *not isolated* in the TUI).
- **Stage 4:** the sandbox moves out of process. The `sandbox` crate's `Sandbox`
  implementation becomes a thin client that launches `nixxa-sandboxd` inside
  bubblewrap + Landlock + seccomp (Linux) and talks to it over a framed protocol on stdio.
  **The trait does not change**, so the controller and harness do not change.
- **Later:** alternative sandbox backends (container, microVM, macOS) are new
  implementations of the same trait, selected in harness config.

---

## 6. Swap guide

The point of all of the above, as a lookup table:

| I want to… | Touch only |
|---|---|
| Add or change an LLM provider / model | `brain/providers/` |
| Change how context is trimmed or summarised | `brain/` |
| Change approval rules, budgets, loop behaviour | `controller/` |
| Switch SQLite → Postgres (or anything) | `db/` + one line in `harness` wiring |
| Switch bubblewrap → containers / microVMs | `sandbox/` + one line in `harness` wiring |
| Add a new built-in action (e.g. `HttpFetch`) | `contracts` (new `Action` variant) → `sandbox` (execute it) → `controller` (policy for it) → `brain` (describe it as a tool) |
| Change the look, layout or keybindings | `harness/` |
| Replace the TUI with a web UI | new frontend crate implementing `ApprovalPort` + `EventSink`; nothing else |

If a change needs more than one row, it is a contract change — see [AGENT.md](AGENT.md).

---

## 7. Cross-cutting concerns

- **Configuration:** only the harness reads config (`~/.config/nixxa/config.toml` + env).
  It hands each module its own typed config section. Modules never read files or env vars
  themselves.
- **Logging/tracing:** `tracing` everywhere; the harness owns the subscriber. Logs go to a file,
  never to the terminal the TUI is drawing on.
- **Async:** tokio. Contract traits are async. Cancellation is explicit.
- **Testing:** each module is tested alone against its contract with fakes for everything
  else. The controller's tests use fake brain / sandbox / store / approval port. End-to-end
  tests run the harness headless.

---

## 8. Planned workspace layout

```
nixxa-harness/
├── Cargo.toml              # [workspace] members = all six
├── contracts/   → crate nixxa-contracts   (lib)
├── brain/       → crate nixxa-brain       (lib)   brain/src/providers/<name>/
├── controller/  → crate nixxa-controller  (lib)   controller/src/policy/
├── sandbox/     → crate nixxa-sandbox     (lib + bin nixxa-sandboxd from Stage 4)
├── db/          → crate nixxa-db          (lib)   db/migrations/
└── harness/     → crate nixxa-harness     (bin `nixxa`)   harness/src/tui/
```

---

## Stages

Every stage ends with something that runs and a checklist that must be true before moving on.

### Stage 0 — Foundations *(current)*
- Repo, README, ARCHITECTURE, DESIGN, AGENT, one folder per module with its own README.
- **Exit:** docs agreed; module boundaries and stage plan accepted.

### Stage 1 — Contracts
- Cargo workspace with six crates (empty bodies except `contracts`).
- All types, traits, events and error enums from §3. Conformance test suites.
- CI: fmt, clippy (`-D warnings`), test, and the dependency-rule check.
- **Exit:** workspace builds; CI fails if any crate imports a sibling.

### Stage 2 — Walking skeleton
- Harness: ratatui app with conversation pane, activity pane, input line, status bar.
- Controller: the loop from §4, policy = "ask for everything".
- Fakes: scripted brain, echo sandbox, in-memory store.
- **Exit:** type a message, watch a fake agent propose an action, approve it, see the fake
  observation — end to end, every module in place.

### Stage 3 — Real brain
- First real provider with streaming and tool calling; API key from config/env.
- Token counting and context-window trimming.
- **Exit:** a real model chats and proposes real actions (still executed by the fake sandbox).

### Stage 4 — Real sandbox
- `nixxa-sandboxd` out of process, bubblewrap + Landlock + seccomp, network off, workspace
  directory mounted read-write, resource limits.
- Built-in actions: exec, read, write, list.
- **Exit:** the agent runs real commands that provably cannot touch the host outside the
  workspace or reach the network (with tests that try).

### Stage 5 — Real memory
- SQLite store, migrations, session list and resume, transcript search.
- **Exit:** quit mid-session, relaunch, resume where you left off.

### Stage 6 — Policy & control
- Approval modes (locked / ask / autonomous), per-action rules, budgets (steps, tokens,
  time, cost), interrupt, audit log view.
- **Exit:** the agent can run unattended within a budget and stops cleanly at the limit.

### Stage 7 — Hardening & reach
- More providers, external tools (e.g. MCP), config UX, packaging, docs site, first release.
- **Exit:** `v0.1.0` tagged and installable.
