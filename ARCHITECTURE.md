# Architecture

This document describes how Nixxa is put together: the modules, the contracts between them,
how data flows, how the built-in integrations plug in, where the trust boundaries are, and the
order in which it gets built.

> The contracts in §3 are implemented in `contracts/src/` (Stage 1). The Rust doc comments there
> are the authoritative API; this document is the overview.

---

## 1. The core idea: blackbox modules

Nixxa is eight folders. Seven do work; one (`contracts/`) defines how they talk.

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

          ┌────────────────────────────────────────────┐
          │ accounts  (Credentials trait)               │  OS keychain; handed by the
          │ consumed by brain + knowledge, managed      │  harness to brain and knowledge,
          │ through the harness UI. Never the sandbox.  │  never to the sandbox
          └────────────────────────────────────────────┘

             every arrow is a type or trait defined in  contracts/
```

### The dependency rule

1. `contracts` depends on nothing in the workspace.
2. `brain`, `controller`, `sandbox`, `db`, `knowledge`, `accounts` depend **only** on `contracts`.
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
- **Planned (§11):** local-model profiles (context size, sampling, tool-call mode per model,
  #13); a plan step in the loop (`Intent::Plan`, #2).

### `controller/` — decides
- **Job:** run the agent loop. It is the only orchestrator.
- **Owns:** the loop state machine, the policy gate (allow / ask / deny), **routing** (sandbox
  actions → `Sandbox`, knowledge actions → `Knowledge`), session primer and write-back,
  budgets (steps, tokens, time, money), interrupts, retries.
- **Does not own:** how thinking, execution, storage, knowledge lookup or rendering work.
- **Planned (§11):**
  - plan mode (#2);
  - secret redaction of everything entering the brain's context (#3);
  - enforcing the project policy file (#5);
  - the test/lint loop (#7);
  - waking on background-task results (#9);
  - several controllers side by side for parallel agents (#6).

### `sandbox/` — acts
- **Job:** execute a `SandboxAction` in isolation and return an `Observation`.
- **Owns:** the isolation mechanism (one backend per OS), the workspace mount, resource limits,
  network policy, built-in actions (exec, read file, write file, list dir, …), output capture
  and truncation.
- **Does not own:** deciding what to run. It enforces its own hard limits and does not trust
  the controller to have been strict enough.
- **Planned (§11):**
  - the host-side egress proxy behind the network allowlist (#1);
  - PTY sessions for the user's terminal (#16);
  - background jobs (#9) and long-running services (language servers, sandboxed MCP servers);
  - workspace checkpoints and the `Patch` action behind undo (#15);
  - LSP queries and the repo map as built-in actions (#8);
  - read-only mounts for paths the project policy protects (#5).

### `db/` — remembers (Nixxa's own records)
- **Job:** persist and retrieve Nixxa sessions.
- **Owns:** schema, migrations, the storage engine, transcript and action-log storage, search.
- **Does not own:** external memory systems (that is `knowledge`), deciding what is worth
  remembering, how it is shown.
- **Planned (§11):** audit records (who or what approved each action, network requests,
  redactions, checkpoints), replay, and a hash-chained JSONL export (#4).

### `knowledge/` — connects
- **Job:** give the agent access to memory and context that lives *outside* Nixxa, and write
  back to it.
- **Owns:** one adapter per source (`knowledge/sources/{claude_mem,obsidian,codegraph,context7}`),
  a private MCP client (stdio + HTTP), cross-OS discovery of each source, health checks,
  result shaping and truncation.
- **Does not own:** deciding when to query or write (controller), Nixxa's own transcripts (db).
- **Runs on the host, never in the sandbox.** Details in §5.
- **Planned (§11):** user-added MCP servers and skills as sources (#10).

### `accounts/` — holds the keys
- **Job:** store every account and credential the user adds, for any model provider or
  integration, and hand out a valid credential when asked.
- **Owns:** the account registry (provider, label, auth method, default flag), secret storage in
  the OS keychain, sign-in flows (browser PKCE / device code) where a provider permits them,
  token refresh, key validation, env-var accounts, redaction helpers.
- **Does not own:** calling the provider's model API (brain), deciding which account a
  session uses (controller/harness), any UI (harness).
- **Never** gives a credential to the sandbox, and never returns one in a form that could land
  in a log or the model's context. Details in §6.

### `harness/` — shows and wires
- **Job:** be the program the user runs.
- **Owns:** the `main` binary, config loading, the ratatui TUI, input handling, approval
  prompts, rendering the event stream, choosing and constructing concrete modules.
- **Does not own:** any agent logic. If the harness contains an `if` about what the agent
  should do, it belongs in the controller.
- **Planned (§11):**
  - the ctrl+t terminal pane (#16);
  - slash commands (#10);
  - network settings (#1);
  - several sessions in tabs, git worktrees and the merge review (#6);
  - notifications (#9);
  - reading `AGENTS.md` and handing its rules to the controller (#5);
  - two more frontends, `nixxa acp` (#11) and `nixxa run --ci` (#12), each implementing
    `ApprovalPort` + `EventSink`.

---

## 3. Contracts

### Core types

| Type | Meaning |
|---|---|
| `SessionId`, `TurnId`, `ActionId` | Opaque identifiers |
| `Message` | A conversation entry: role (user / agent / system / observation) + content |
| `Context` | Everything the brain needs for one think step: account, instructions, primer, messages, available sandbox action kinds, knowledge tool specs, limits |
| `Content` | What a `Message` carries: text, a proposed `ActionRequest`, an `Observation`, `Reasoning` (opaque provider data such as a signed thinking block) — so the brain can rebuild tool-call history exactly — or `Summary` (a turn's `Done` summary: stored, never sent back to the brain or shown, D-056) |
| `Intent` | What the brain wants: `Say(text)`, `Act(Action)`, `Done(summary)` |
| `Action` | `Sandbox(SandboxAction)` or `Knowledge(KnowledgeAction)` — the controller routes on this |
| `SandboxAction` | `Exec`, `ReadFile`, `WriteFile`, `ListDir`, … |
| `KnowledgeAction` | `Call { source, tool, args }` (a source's tool; its `ToolSpec.mutating` says whether it writes) or `Record { source, entry }` (structured write-back) |
| `ToolSpec` | A knowledge tool: source, name, description, JSON-Schema input, `mutating` flag |
| `Primer` | Context gathered at session start from knowledge sources (recent memory, project note) |
| `AccountRef` | Which account to use: provider id + account label (e.g. `anthropic:work`). Not a secret |
| `Credential` | A secret wrapper (`ApiKey`, `BearerToken`, `CloudProfile`, `None`) whose `Debug`/`Display` print `***`; zeroised on drop |
| `Observation` | A result: output, error, timing, truncation flag, source |
| `Verdict` | The policy gate's answer: `Allow`, `Ask(reason)`, `Deny(reason)` |
| `Event` | Everything the harness can render: tokens, intent proposed, approval needed, action started/finished, knowledge source status, budget update, error, turn done |

### Traits

| Trait | Implemented by | Core operations |
|---|---|---|
| `Brain` | `brain` | think(context) → stream of brain events; describe() |
| `Sandbox` | `sandbox` | execute(action) → observation; capabilities(); health(); reset() |
| `Store` | `db` | create/load session; append message; append action record; list sessions (each with its first user message); search transcripts |
| `Credentials` | `accounts` | list(provider) → accounts; get(account_ref) → `Credential` (refreshed if needed); add / remove / validate / sign_in(provider, method) |
| `Knowledge` | `knowledge` | sources() → status of each; tools() → tool specs of ready sources; primer(workspace) → `Primer`; call(id, source, tool, args) → observation; record(id, source, entry) → observation |
| `ApprovalPort` | `harness` | ask the human about an action → `Verdict` |
| `EventSink` | `harness` | receive `Event`s from the controller |

The controller consumes `Brain`, `Sandbox`, `Store`, `Knowledge`, `ApprovalPort` and
`EventSink`. `Credentials` is consumed by `brain` and `knowledge` (each gets it from the
harness at construction and asks for a credential per request, so refresh and account
switching just work) and managed by the harness's accounts screen. The controller only ever
sees `AccountRef`s, never secrets.

### Conventions (pinned in Stage 1)
- **Errors vs. outcomes:** a trait error means the infrastructure failed. An action that ran but
  did not succeed (non-zero exit, missing file, path outside the workspace, denied) is an
  `Observation` with `Outcome::Failed` / `Outcome::Denied`, because the agent must see it.
- **Cancellation:** dropping a returned future or stream cancels the operation.
- **Async traits** use `async-trait` so every module can be held as `Arc<dyn Trait>`.
- **Secrets:** `Secret` and `Credential` are not serialisable and redact themselves in `Debug`,
  so a credential cannot reach the db, an event or a log by accident.
- **Wire format:** all other types are serde-serialisable with stable, tagged, snake_case JSON.
- **Reasoning:** a brain may emit `BrainEvent::Reasoning { provider, data }`. The controller
  records it, in stream order and unannounced, as `Content::Reasoning`, and the brain gets it back
  in later contexts. Only the brain of that provider reads it; nothing shows it. It exists
  because some providers require their signed reasoning blocks to be sent back unchanged.
- **Ids:** `ActionId` is assigned by the brain (usually the provider's tool-call id);
  `SessionId` by the store; `TurnId` by the controller.

### Errors
Each trait has its own error enum in `contracts` (e.g. `BrainError::RateLimited`,
`SandboxError::Timeout`, `KnowledgeError::SourceUnavailable`, `CredentialsError::Expired`). Modules map internal errors
into these; internal error types never leak across a boundary.

### Conformance tests
`contracts` ships a test suite per trait behind the `conformance` feature
(`nixxa_contracts::conformance::{store,sandbox,knowledge,credentials,brain}::run`). Any
implementation (real or fake) must pass it. The contracts crate's own tests prove each suite
is satisfiable (in-memory reference implementations pass) and has teeth (deliberately broken
ones fail).

The sandbox suite checks, besides the basics: unsupported kinds error, ids are echoed, paths
that escape the workspace never succeed, `reset` keeps the workspace, an isolated backend that
supports `Exec` really runs `echo`, and dropping an in-flight `execute` doesn't wedge the
backend (the next call works).

### Planned contract changes (§11)
These aren't in `contracts` yet. Each lands with the stage that needs it, as its own
`feat(contracts)` commit with conformance tests.

| Change | For |
|---|---|
| `SandboxAction::Patch { path, edits, expected_hash }` (search/replace hunks against a known file state) | precise edits, plan diffs, undo (#15, #2) |
| `Sandbox::checkpoint()` → `CheckpointId`, `Sandbox::restore(CheckpointId)`, `Sandbox::diff(from, to)` | undo (#15), worktree review (#6) |
| `NetworkPolicy { Off, Allow(domains), Open }`, `Sandbox::set_network(policy)`; `Observation.network: Vec<NetRequest>` (each request made while an action ran, allowed or blocked) | network allowlist (#1), audit (#4) |
| Background jobs: `Exec { background: true }` → `JobId`; `JobStatus`, `JobOutput`, `JobStop` actions; `Event::JobFinished` | background tasks (#9) |
| Terminal: `Sandbox::open_terminal(size)` → a PTY stream (input, output, resize) | ctrl+t (#16) |
| `SandboxAction::{Diagnostics, Definition, References, RepoMap}` | LSP and repo map (#8) |
| `Intent::Plan(Plan)`, `Content::Plan`, `Event::PlanProposed`, `ApprovalPort::ask_plan(plan)` → `PlanVerdict` | plan mode (#2) |
| `Verdict` provenance (`user`, `auto`, `plan`, `policy:<rule>`, `ci`) on every `ActionRecord` | audit (#4), CI (#12) |
| `Event::SecretRedacted { kind, source }`, `Event::CheckResult`, `Event::NetworkRequest` | redaction (#3), test loop (#7), network log (#1) |
| `ProjectPolicy` (typed rules from `AGENTS.md`) in `ControllerConfig` | policy file (#5) |

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

### Planned changes to the loop (§11)
- **Before step 2:** everything entering the context (user text, observations, knowledge
  results) passes the **secret scrubber** (#3). What's stored is the scrubbed form too.
- **Step 3** gains two checks that run *before* any approval:
  - the **project policy** (#5): protected paths, denied commands;
  - in plan mode, **"is this in the approved plan?"** (#2). Anything outside the plan falls
    back to Ask.
- **Before step 4:** a **checkpoint** is taken before any action that can change files (#15).
- **After a turn that changed files:** the **verify loop** runs the project's checks (#7) and
  LSP diagnostics (#8). Failures go back to step 2, until they pass or the loop budget runs out.
- **Plan mode** inserts a phase before execution. The agent investigates read-only (reads,
  lists and LSP queries need no approval), then returns `Intent::Plan`: ordered edits (as
  diffs) and commands. The user approves, edits or rejects the plan as a whole; the approved
  steps then run without per-step prompts.

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
  against a specific claude-mem version in Stage 10 and covered by a contract test.
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
| Call a non-mutating tool (any source) | allow | allow | allow |
| Call a mutating tool | deny | ask | allow |
| Record / write (claude-mem, vault) by the agent | deny | ask | allow |
| Session-end write-back (configured by the user, not proposed by the agent) | per config | per config | per config |

---

## 6. Accounts and credentials

Any account, any provider, any integration — and as many per provider as the user wants.

### Auth methods

| Method | Used for | Stored |
|---|---|---|
| `ApiKey` | Anthropic Console, OpenAI, Gemini, OpenRouter, Mistral, Groq, Context7, OpenAI-compatible endpoints | key in OS keychain |
| `Cloud` | Bedrock, Vertex AI, Azure / Microsoft Foundry | a reference to the cloud's own profile/credentials (Nixxa does not copy them) |
| `SignIn` | providers that officially allow third-party OAuth sign-in. First: OpenRouter's OAuth PKCE flow (Stage 3), which issues a user-controlled key | the key or token in the OS keychain; refresh where a provider issues expiring tokens (Stage 13) |
| `Local` | Ollama, LM Studio, llama.cpp | base URL only |
| `Env` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, … | not stored; read at startup, shown as a read-only account |

Each brain provider adapter and each knowledge source declares which methods it accepts. The
accounts module stays generic; a provider-specific sign-in flow lives in
`accounts/src/methods/` and is enabled per provider.

**Subscription logins.** A provider's consumer-subscription login is only implemented when the
provider officially permits third-party apps to use it. Anthropic currently does not allow
third-party products to offer claude.ai login without approval, so Anthropic models are
reached through API keys or cloud accounts. Every new sign-in flow gets its terms checked and
recorded in the DESIGN.md decision log before it is built.

### Storage
- **Secrets:** OS keychain via one cross-platform layer — Secret Service (Linux), Keychain
  (macOS), Credential Manager (Windows). Entry name `nixxa/<provider>/<account>`.
- **Headless Linux without a Secret Service:** fall back to an encrypted file in the data dir
  (passphrase asked once per session), or env-var accounts. Never a plain-text file.
- **Metadata** (provider, label, method, default, hint): a JSON registry in the keychain entry
  `nixxa/_registry`, no secrets, so accounts need no file and only the harness reads config
  (D-022).
- **Sign-in methods** live in `accounts/src/methods/`: a provider-neutral PKCE helper and a
  loopback callback listener (127.0.0.1 only, one request, timeout, cancellable), plus one
  concrete flow per provider (`OpenRouterSignIn`). The harness drives them and opens the browser;
  accounts never does.
- **Env accounts:** the harness reads `ANTHROPIC_API_KEY` (and later others) and hands them to
  `accounts`, which lists them as read-only `<provider>:env` accounts and never stores them.
  An env account is its provider's default whenever no stored account is.

### Rules
- Multiple accounts per provider; one marked default. A session records the `AccountRef` it
  used; the user can switch account mid-session (takes effect on the next brain call).
- Keys are validated on add with the cheapest call the provider offers.
- Usage and cost are tracked **per account** (shown in the status bar and the accounts screen,
  and used by Stage 14 budgets).
- Credentials never reach: the sandbox, the model's context, the db, claude-mem, the vault,
  logs, or crash reports. `Credential` redacts itself and is zeroised on drop.
- Removing an account deletes its keychain entry and, for sign-in methods, revokes the token
  where the provider supports it.

---

## 7. Process model and trust boundaries

| Zone | Contains | Trust |
|---|---|---|
| Host process | harness, controller, brain, db, knowledge, accounts | trusted code, **untrusted brain output**, **untrusted retrieved content** |
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
  Stage 14 policy.
- **Secrets never enter the sandbox.** API keys live in the host process. The sandbox gets a
  clean environment.
- **The sandbox enforces its own limits** (CPU, memory, wall time, output size, file system
  scope, network) independently of the controller.
- **Network is off by default** inside the sandbox. The only planned way out is the host-side
  **egress proxy** (#1):
  - it allows only the domains the user allowed for this session, and logs every request;
  - "open" mode still goes through the proxy and is still logged;
  - raw sockets (anything not using the proxy, including UDP) stay blocked.

  An allowed domain is still a channel out. For example, a package registry or GitHub can
  receive data. That's why the status bar always shows the network state.
- **A repository's own files are untrusted input** (#5, #10). `AGENTS.md`, project skills and
  project slash commands can only make Nixxa **stricter**. They can never turn on the network,
  Auto mode, or a looser rule than the user's own settings. Project skills and commands are
  shown, and approved, the first time they're used.
- **Secret redaction is defence in depth, not a guarantee** (#3). The scrubber catches known
  key formats, high-entropy assignments and the exact secrets Nixxa itself holds (as
  fingerprints from `accounts`). Reading files such as `.env` or `*.pem` asks first.
- **The ctrl+t terminal is the user, not the agent** (#16). It runs in the same place as the
  agent (inside the same sandbox for sandboxed places), so it has the same reach. What's typed
  there isn't approved or sent to the provider; it's recorded in the session log and shared
  with the agent only when the user chooses.
- **User-added MCP servers** run where the user says: on the host (trusted like any program the
  user installs), or inside the sandbox. Their tools are treated as mutating unless the user
  marks them read-only (#10).
- **CI mode never runs on untrusted code with secrets** (#12): no `pull_request_target`
  checkout of a fork's head. The provider key comes from CI secrets and stays outside the
  sandbox.

### How the process model evolves
- **Stages 2–3:** single process. The sandbox is a fake, labelled *not isolated* in the TUI. If
  the user chooses to work in the current directory at start-up, `HostSandbox` acts on the real
  files there instead (confined to that directory, not isolated, D-039).
- **Stage 4:** the sandbox moves out of process: a thin client launches `nixxa-sandboxd`
  inside bubblewrap + Landlock + seccomp (Linux) and talks to it over a framed protocol on
  stdio. **The trait does not change.**
- **Stage 6:** the daemon stops being "one command at a time, then kill everything" (D-043).
  It tracks each job (the agent's command, the user's terminal, background jobs, services) as
  its own process tree with its own limits. Kills target one job, never the user's shell. The
  host side gains the egress proxy, reached from inside through a Unix socket the daemon
  forwards to `127.0.0.1` in the sandbox.
- **Stage 12:** background sessions. A long-running `nixxa` service hosts the controllers, and
  the TUI becomes a client that can detach and re-attach. Until then, background work lives
  inside a running TUI.
- **Stage 16:** backends for macOS (Seatbelt profile) and Windows (AppContainer or
  WSL2/container — see open questions). Until an OS has a backend, Nixxa runs there with
  sandbox actions **disabled** (chat and knowledge still work) unless a container runtime
  (Docker/Podman) is available as a fallback backend.

---

## 8. Swap guide

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
| Add a provider's sign-in method, or a new credential store | `accounts/` |
| Add a model provider that needs a new kind of credential | `accounts/` (method) + `brain/providers/` (adapter) |
| Change the look, layout or keybindings | `harness/` |
| Replace the TUI with a web UI | new frontend crate implementing `ApprovalPort` + `EventSink`; nothing else |
| Add an editor or CI frontend (ACP, GitHub Action) | a frontend in `harness/` implementing `ApprovalPort` + `EventSink` (§11 #11, #12) |
| Change the network allowlist presets or the proxy | `sandbox/` (proxy), `harness/` (settings UI) |
| Add a secret detector | `controller/` (scrubber) |
| Add a project-policy rule | `contracts` (`ProjectPolicy`) → `harness` (parsing `AGENTS.md`) → `controller` (enforcing) → `sandbox` (if it needs a mount) |
| Add a slash command | `harness/` (built-in) or a Markdown template in `<config>/nixxa/commands` (user) |

If a change needs more than one row, it is a contract change — see [AGENT.md](AGENT.md).

---

## 9. Cross-cutting concerns

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

## 10. Planned workspace layout

```
nixxa-harness/
├── Cargo.toml              # [workspace] members = all eight
├── contracts/   → crate nixxa-contracts   (lib)
├── brain/       → crate nixxa-brain       (lib)   brain/src/providers/<name>/
├── controller/  → crate nixxa-controller  (lib)   controller/src/policy/
├── sandbox/     → crate nixxa-sandbox     (lib + bin nixxa-sandboxd)  sandbox/src/backends/<os>/
├── db/          → crate nixxa-db          (lib)   db/migrations/
├── knowledge/   → crate nixxa-knowledge   (lib)   knowledge/src/{mcp,sources/<name>}/
├── accounts/    → crate nixxa-accounts    (lib)   accounts/src/{store,methods/<name>}/
└── harness/     → crate nixxa-harness     (bin `nixxa`)   harness/src/tui/
```

---

## 11. Planned capabilities

Re-planned on 2026-11-23 (D-060). Each capability fits inside the existing modules: no new
crate, and the dependency rule doesn't change. Contract changes are listed in §3 and land with
their stage.

| # | Capability | Lives in | Stage |
|---|---|---|---|
| 1 | Network allowlist per session, via a logging host-side proxy; network settings in the harness | sandbox, harness | 6 |
| 16 | `ctrl+t` terminal in the harness, in place of the details pane | sandbox, harness | 6 |
| 9 | Background tasks with notifications (in-session jobs) | sandbox, controller, harness | 6 |
| 15 | Patch/edit action and undo (checkpoints) | contracts, sandbox, controller, harness | 7 |
| 3 | Secret redaction before anything reaches the provider | controller, accounts | 7 |
| 5 | Project policy file (`AGENTS.md`) | harness, controller, sandbox | 7 |
| 2 | Plan / dry-run mode: approve the whole plan at once | contracts, brain, controller, harness | 8 |
| 7 | Auto test/lint loop within a budget | controller | 8 |
| 8 | LSP diagnostics and a repo map | sandbox | 9 |
| 10 | User-added MCP servers, skills and slash commands | knowledge, harness | 11 |
| 6 | Parallel agents: git worktrees, each in its own sandbox, with a merge review; parallel sessions on different tasks | controller, harness | 12 |
| 9 | Background sessions (detach and re-attach) | harness | 12 |
| 13 | Local-model profiles | brain, harness | 13 |
| 4 | Session replay and audit export | db, harness | 14 |
| 11 | Editor integration through the Agent Client Protocol | harness | 15 |
| 12 | GitHub Action / CI mode | harness | 15 |
| 14 | Team features: shared policy and budgets, org-wide audit logs, session sharing | a separate server, later | 18 |

### #1 Network allowlist (sandbox + harness)
- **Off by default**, as today. The user allows domains per session. There are presets:
  - **crates.io:** `crates.io`, `index.crates.io`, `static.crates.io`;
  - **npm:** `registry.npmjs.org`;
  - **PyPI:** `pypi.org`, `files.pythonhosted.org`;
  - **GitHub;**
  - plus any domain the user types.
- **Open** gives full internet for the session. It still goes through the proxy and is still
  logged. The user turns it on and off in the harness's network settings, which a slash command
  (`/net`) or a key opens; the status bar always shows `net: off`, `net: 3 domains` or
  `net: OPEN`.
- **How the proxy works:** it runs on the host, in the sandbox crate, started by the backend.
  - The sandbox still has only `lo`. The daemon forwards `127.0.0.1:<port>` inside the sandbox
    to the proxy's Unix socket, which is bind-mounted in.
  - Commands get `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` (cargo, npm, pip and git honour them).
    A tool that ignores them fails closed.
  - HTTPS uses `CONNECT`: the proxy allows or blocks by host name and checks the TLS SNI
    matches. There's no TLS interception and no injected CA. DNS is resolved by the proxy on the
    host.
  - Every request (host, port, method/URL for plain HTTP, bytes, allowed or blocked, time) is
    logged. It's attached to the action's observation (`Observation.network`), shown in the
    activity pane, and stored for the audit (#4).
- **A project can ask for domains** in `AGENTS.md` (#5). The harness shows the request; only the
  user can grant it.
- **Exit test:** a fresh Rust project with a dependency builds and runs `cargo test` inside the
  sandbox with only crates.io allowed; a request to any other host is blocked and logged.

### #16 Terminal on `ctrl+t` (sandbox + harness)
- **Opening it:** `ctrl+t` replaces the details pane with an interactive terminal (a PTY
  rendered with a VT100 emulator widget); pressing it again goes back. The shell runs **in the
  same place as the agent**:
  - for sandboxed places, inside the same sandbox (the daemon opens a PTY job);
  - in This directory, the user's own shell in that directory.
- **It's you, not the agent:** nothing typed there is approved, scrubbed or sent to the
  provider. The session log records that a terminal was used, and a "share with the agent"
  action sends a chosen part of its output as a message.
- **Daemon change:** it can't kill the user's shell when the agent's command ends, which is why
  Stage 6 moves the daemon to per-job tracking (§7).

### #9 Background tasks and notifications (sandbox + controller + harness)
- **In-session jobs (Stage 6):** the agent can start a long command in the background
  (`Exec { background: true }` → `JobId`), check on it, read its output and stop it. Each job
  has its own limits and output files. When a job ends, the controller tells the agent at its
  next step. If the agent is idle and waiting on that job, the controller starts a turn (within
  budget).
- **Notifications** (desktop notification where available, the terminal bell, a status-bar
  badge) fire when a job, a turn or a test loop finishes while the user is away from the pane.
- **Background sessions (Stage 12)** keep whole agent runs going without the TUI open. This
  needs the long-running `nixxa` service (§7).

### #15 Patch action and undo (contracts + sandbox + controller + harness)
- **`SandboxAction::Patch`:** search/replace hunks against the file's hash at proposal time, so
  a stale edit fails instead of clobbering. It's what the agent uses for edits, what plan mode
  shows as diffs, and what makes edits exactly reversible. `WriteFile` stays for new files.
- **Checkpoints:** before any action that can change files (patch, write, or exec), the sandbox
  backend snapshots the workspace into a shadow store outside it
  (`<data dir>/nixxa/checkpoints/<session>`: a separate git object store that honours
  `.gitignore`, with a size cap).
- **Undo:** `/undo` (or a key) shows the diff back to the checkpoint before the last action or
  turn, and restores it on confirmation. This works in every place, including the real files in
  This directory and Work on my files. Effects outside the workspace (network calls, `/tmp`)
  can't be undone, and the confirmation says so.

### #3 Secret redaction (controller + accounts)
- **What's scrubbed:** everything entering the brain's context: user messages, file reads,
  command output, knowledge results.
- **Detectors:**
  - known key formats: Anthropic, OpenAI, GitHub, AWS, GCP, Slack, Stripe, JWTs, PEM private
    keys, connection strings with passwords;
  - high-entropy values in `KEY=…`-style assignments;
  - exact matches of secrets Nixxa holds. `accounts` gives the controller salted fingerprints,
    never the secrets.
- **Replacement:** a match becomes a stable placeholder such as `‹secret:aws-access-key#3f2a›`
  (the same value gets the same placeholder within a session), and an `Event::SecretRedacted`
  warning is shown. The user can send one item through anyway.
- **Storage:** the store keeps the scrubbed form, so the db, the vault and claude-mem never get
  the secret either.
- **Sensitive files:** reading `.env`, `*.pem`, `id_*` and similar asks first, even in Auto.
- **If the agent writes a placeholder into a file,** it's warned that the file contains a
  placeholder, not the secret.

### #5 Project policy file: `AGENTS.md` (harness + controller + sandbox)
- **Where it's read:** the harness reads `AGENTS.md` at the workspace root (the cross-tool
  convention) and in subdirectories; for files under a subdirectory, the nearest one wins.
- **Two parts:**
  - **Prose:** goes into the agent's instructions, labelled as project rules.
  - **A fenced ` ```nixxa-policy ` TOML block:** enforced by the controller at the policy gate,
    before any approval:
    - `protect` (globs never written, e.g. `migrations/**`);
    - `ask_read` (globs that ask before reading);
    - `deny_commands` / `ask_commands` (patterns);
    - `checks` (the test/lint commands for #7);
    - `network` (domains the project asks for);
    - `budgets`.
- **Enforcement:** a patch or write to a protected path is denied, with the rule shown. As
  defence in depth, the sandbox mounts protected paths read-only, so a command can't change them
  either.
- **Layering:** user policy (`<config dir>/nixxa/policy.toml`) and, later, org policy (#14)
  layer on top; the strictest rule wins. A project file can never loosen anything (§7).

### #2 Plan / dry-run mode (contracts + brain + controller + harness)
- **The mode:** a third approval mode. `shift-tab` cycles Ask → Plan → Auto.
- **Investigating:** in Plan, the agent first investigates read-only. Reads, lists, LSP queries
  and the repo map need no approval; nothing writes or runs.
- **The plan:** it then proposes `Intent::Plan`: ordered steps, each a patch (shown as a diff)
  or a command, with what it expects and the checks it will run.
- **Approval:** the harness shows the whole plan. The user approves it, removes or edits steps,
  rejects it, or replies with feedback for a new plan.
- **Execution:**
  - Approved steps run without per-step prompts.
  - A step whose file changed since the plan was made (hash mismatch) stops for re-planning.
  - Any action outside the plan falls back to Ask.
  - The verify loop (#7) runs at the end.

### #7 Test/lint loop (controller)
- **The checks** come from `AGENTS.md`, or are detected (`Cargo.toml` → `cargo test` and
  `cargo clippy`; `package.json` scripts; `pyproject.toml` → `pytest`/`ruff`) and confirmed once
  per project.
- **When they run:** after a turn or plan that changed files, inside the sandbox (with the
  network allowlist for dependencies).
- **The loop:** failures, trimmed, plus LSP diagnostics (#8) go back to the agent, until the
  checks pass or the loop budget runs out (iterations, default 5; wall time; tokens/cost). The
  last result is reported either way.
- **Approval:** the checks run without per-run approval because the user configured them, but
  each run is shown.

### #8 LSP diagnostics and a repo map (sandbox)
- **Language servers run inside the sandbox** as long-running services. rust-analyzer, for
  example, runs build scripts and proc macros, which is project code, so it never runs on the
  host for sandboxed places.
- **Querying them:** the sandbox crate runs the LSP client on its side of the protocol. It
  exposes `Diagnostics`, `Definition`, `References` and `Hover` as built-in actions, all
  read-only and free in every mode.
- **The repo map** is a compact, ranked symbol outline of the workspace (tree-sitter tags,
  ranked by references) within a token budget. It's built by the sandbox (`RepoMap`), included
  in the context at session start, and refreshed after large changes.
- **CodeGraph and Context7** (knowledge, §5) join this stage.

### #10 User MCP servers, skills and slash commands (knowledge + harness)
- **MCP servers:** configured in `config.toml` (`command` or `url`, credentials from
  `accounts`, `run = "host" | "sandbox"`) and served by the knowledge module's MCP client.
  - Their tools appear like any knowledge tool and go through the policy gate.
  - They're treated as mutating unless the user marks them read-only.
  - Sandboxed stdio servers run as sandbox services (#8).
- **Skills:** folders with a `SKILL.md` (name, description, instructions) in
  `<config dir>/nixxa/skills` and the project's `.nixxa/skills` (Claude Code's `.claude/skills`
  layout is read too).
  - They're listed by name and description in the agent's instructions, and loaded on demand
    through a `skills` knowledge source.
  - Project skills are untrusted, like `AGENTS.md`.
- **Slash commands:**
  - built-ins in the harness: `/plan`, `/ask`, `/auto`, `/net`, `/undo`, `/terminal`,
    `/agents`, `/replay`, `/export`, `/help`;
  - user commands: Markdown prompt templates with `$ARGUMENTS`, in
    `<config dir>/nixxa/commands` and `.nixxa/commands`.

### #6 Parallel agents (controller + harness)
- **Tabs:** the harness hosts several sessions as tabs. Each has its own controller, sandbox,
  place and history; budgets and notifications are shared.
- **Worktrees:** for parallel work on one repo, the harness creates a git worktree per agent
  (`<data dir>/nixxa/worktrees/<repo>/<branch>`, from the current `HEAD`). Each agent works in
  its own worktree, mounted in its own sandbox.
- **Merge review:** when they're done, a review screen shows each branch's diff and test
  result. The user picks what to merge, and the harness merges or cherry-picks on the host.
  Conflicts go to the user, or to an agent the user points at them.
- **Different tasks:** parallel sessions on different tasks work the same way. Two agents never
  write to the same directory at once: a directory has one writer, and others get a worktree or
  read-only access.

### #13 Local-model profiles (brain + harness)
- **What a profile holds,** per server and model:
  - context window (read from LM Studio's or Ollama's API where possible) and max output;
  - temperature and top-p;
  - tool-call mode: native, or a prompted-JSON fallback for models without tool calling;
  - reasoning on/off and effort;
  - parallel tool calls;
  - extra system-prompt text and timeouts.
- **Presets** for common models (gpt-oss, Qwen, Llama).
- **Choosing one:** per session, on the setup or accounts screen. The harness hands it to the
  OpenAI-compatible provider. Context trimming uses the real context size (LM Studio often
  loads models at 4k).

### #4 Session replay and audit export (db + harness)
- **What's stored per action:** who or what approved it (user, Auto, plan, a policy rule, CI),
  its network requests, redactions, checkpoints, and when a terminal was used.
- **Replay** (from the `ctrl-s` list) is a read-only timeline: step through messages, actions,
  diffs and outputs with timestamps.
- **Export** (`nixxa export <session>` or `/export`):
  - **JSONL:** the audit format. Each record carries the hash of the one before, so edits show.
  - **Markdown or HTML:** for people.
  - Exports keep secrets redacted.

### #11 Editor integration: Agent Client Protocol (harness)
- **The frontend:** `nixxa acp` runs Nixxa as an ACP agent over stdio JSON-RPC, for editors
  that speak ACP. It's a second frontend in the harness implementing `ApprovalPort` and
  `EventSink`. The start-up choice comes from the editor's session settings instead of the TUI
  question.
- **Mapping:**
  - prompts → turns;
  - events → session updates (messages, tool calls with diffs, plans);
  - approvals → the editor's permission requests;
  - plan mode → ACP plans.
- **Where work happens:** execution stays in Nixxa's sandbox. The editor sees the changes on
  disk and as diffs. Reading the editor's unsaved buffers through ACP is optional and
  read-only.

### #12 GitHub Action / CI mode (harness)
- **The command:** `nixxa run --ci --task <text|file>` runs headless: JSONL events to a file or
  stdout, no human.
- **Approvals** come only from policy (`AGENTS.md` plus the CI config). Anything that would ask
  is denied. Alternatively, plan mode posts the plan as a PR comment and waits for a
  maintainer's `/approve`.
- **Budgets** are mandatory.
- **Output:** a branch or PR, a patch, the audit JSONL as an artifact, and a summary comment.
- **Runs** on Linux runners with bubblewrap, like our own CI.
- **Packaging:** shipped as a GitHub Action that wraps the binary.
- **Security** is in §7: no fork code with secrets, and the key stays outside the sandbox.

### #14 Team features (later)
- **What:** shared policy (org, then user, then project; the strictest wins), org-wide budgets
  and per-user usage, central audit-log collection (the hash-chained JSONL from #4), read-only
  session sharing (redacted replays), and SSO.
- **Needs** a separate, self-hostable server.
- **Seams in place before then:** policy layering (#5), the audit export (#4), and a remote
  `EventSink`.

---

## Stages

Every stage ends with something that runs and a checklist that must be true before moving on.

### Stage 0 — Foundations *(done)*
- Repo, README, ARCHITECTURE, DESIGN, AGENT, one folder per module with its own README.
- **Exit:** docs agreed; module boundaries, integrations and stage plan accepted.

### Stage 1 — Contracts *(done — macOS/Windows CI pending)*
- Cargo workspace with eight crates (empty bodies except `contracts`).
- All types, traits, events and error enums from §3, including `Knowledge` and the
  `Action` split, `Credentials`, `Credential` and `AccountRef`. Conformance test suites.
- CI on Linux, macOS and Windows: fmt, clippy (`-D warnings`), test, dependency-rule check.
- **Exit:** workspace builds on all three OSs; CI fails if any crate imports a sibling.

### Stage 2 — Walking skeleton *(done)*
- Harness: ratatui app with conversation pane, activity pane, input line, status bar
  (including knowledge-source indicators).
- Controller: the loop from §4 with routing; policy = "ask for everything".
- Fakes: scripted brain, echo sandbox, in-memory store, fake knowledge source, in-memory
  credentials.
- **Exit:** type a message, watch a fake agent query fake knowledge and propose a sandbox
  action, approve it, see the observation — end to end, every module in place.

### Stage 3 — Real brain *(in progress)*
- First real provider with streaming and tool calling.
- `accounts` comes alive: add an API key from the TUI (stored in the OS keychain) or use an env
  var; one account per provider for now.
- Re-planned (no Anthropic key available): an OpenAI-compatible provider for local models (LM
  Studio / Ollama) as a second setup option, and OpenRouter OAuth sign-in moved forward from
  Stage 8 as the first sign-in method (D-037).
- Token counting and context-window trimming.
- **Exit:** a real model chats and proposes real actions (still executed by fakes); the key
  survives a restart without ever being written to a plain-text file.

### Stage 4 — Real sandbox (Linux) *(done)*
- **PR A (done):** the daemon, its framed protocol and the `BwrapSandbox` client with
  namespace isolation, rlimits and a cgroup scope (D-040–D-043), plus escape tests that try to
  get out. Not wired into the harness. `isolated: false` until Landlock and seccomp landed.
- **PR B (done):** Landlock and seccomp are applied by the daemon at start-up; the protocol
  reports the active layers (VERSION 2) and `BwrapSandbox::capabilities().isolated` is true only
  when namespaces, Landlock and seccomp are all active (D-044–D-046). Escape tests now cover
  ptrace, keyctl, unshare, mount, TIOCSTI, io_uring, AF_NETLINK and a Landlock read-only path.
- **PR C (done):** the start-up question offers an empty sandbox, this directory (not
  isolated), or this directory in the sandbox (the real files, or a copy), backed by
  `BwrapSandbox` (D-047, D-048); the workspace guard (D-049); the daemon is found beside the
  harness or on `PATH` (D-050) and started before the session, with no fallback (D-051); Ask/Auto
  approval modes on `shift-tab` (D-052). Escape tests now also cover `open_tree` and `clone` with
  namespace flags, each shown to fail without seccomp.
- `nixxa-sandboxd` out of process, bubblewrap + Landlock + seccomp, no network but the
  sandbox's own loopback, workspace mounted read-write, resource limits. Built-in actions: exec, read, write, list.
- **Exit:** the agent runs real commands that provably cannot touch the host outside the
  workspace or reach the network (with tests that try).

### Stage 5 — Real memory *(done)*
- SQLite store, migrations, session list and resume, transcript search.
- **Done:** `SqliteStore` (rusqlite bundled, WAL, forward-only migrations, FTS5; D-053–D-055);
  `Done` summaries marked as `Content::Summary` and `SessionInfo.first_message` (contract
  changes, D-056); `Controller::resume_session`, which replays the history to the UI, closes tool
  calls a quit left open and tells the agent the workspace may have changed (D-057); the harness
  keeps history at `<data dir>/nixxa/nixxa.db` (`MemoryStore` with `--fake`, or with a visible
  note when the file can't be opened, D-058), lists sessions on `ctrl-s` and resumes the latest
  one with `--continue` (D-059).
- **Exit:** quit mid-session, relaunch, resume where you left off. Met: the harness test
  `stage_five_quit_mid_turn_relaunch_and_resume_where_you_left_off` quits with ctrl-c twice
  mid-turn, opens a new store on the same file and resumes, and checks the conversation, the
  activity pane and the brain's history; the same was done by hand with a real model.

### Stage 6 — Working in the sandbox: network, terminal, background jobs
- **The daemon moves to per-job tracking:** each job is its own process tree with its own
  limits (the agent's command, the user's terminal, background jobs, services). This replaces
  the "kill everything after each command" sweep (D-043) without loosening isolation.
- **Network allowlist (#1):** the host-side egress proxy with per-request logging; presets and
  "Open"; network settings in the harness; the network state always shown.
- **Terminal (#16):** `ctrl+t` replaces the details pane with a terminal in the same place as
  the agent.
- **In-session background jobs and notifications (#9).**
- **Exit:**
  - `cargo test` on a fresh project with a crates.io dependency passes inside the sandbox with
    only crates.io allowed, and a request to another host is blocked and logged;
  - `ctrl+t` opens a shell in the same sandbox, and it survives the agent's commands;
  - a long background build notifies when it finishes;
  - the Stage 4 escape tests still pass.

### Stage 7 — Safe edits: patches, undo, secrets, project policy
- **Patches and undo (#15):** `SandboxAction::Patch`, checkpoints and `/undo` in every place.
- **Secret scrubbing and warnings (#3).**
- **`AGENTS.md` policy (#5):** the prose goes into the agent's instructions, and the
  `nixxa-policy` block is enforced, including read-only mounts for protected paths.
- **Exit:**
  - edits arrive as patches, and `/undo` restores the files, including after a command changed
    them;
  - a fake key planted in a file never reaches the provider (checked with a recording brain) and
    the user is warned;
  - with `protect = ["migrations/**"]`, both a patch and a `rm` command fail to change
    `migrations/`.

### Stage 8 — Plan mode and the verify loop
- **Plan mode (#2)** and the Ask → Plan → Auto cycle.
- **The test/lint loop (#7)** with a budget.
- **Exit:** in Plan, the agent investigates read-only, proposes edits and commands as one plan,
  which is approved once and executed. The checks then run, and failures are fixed within the
  budget. It ends green, or says why not.

### Stage 9 — Code intelligence
- **LSP (#8):** language servers as sandbox services; diagnostics, definition, references and
  hover as actions.
- **The repo map.**
- **CodeGraph** (detect `.codegraph/`, its tools, a user-triggered indexing command) and
  **Context7** (resolve-library and query-docs, optional API key from `accounts`).
- **Exit:**
  - after an edit, the agent sees and fixes a compiler diagnostic without running a build;
  - a session starts with a repo map;
  - in an indexed repo, "who calls X" goes through CodeGraph;
  - a named library's docs come from Context7.

### Stage 10 — Knowledge: personal (Obsidian + claude-mem)
- `knowledge` crate with its MCP client (stdio + HTTP) and source-status reporting.
- Obsidian: cross-OS vault discovery, search/read/links, project-note primer, session-note
  write-back.
- claude-mem: discovery, primer, search/timeline/get tools, session write-back.
- **Exit:** on a fresh machine with Obsidian and claude-mem installed, Nixxa finds both with
  zero config on Linux, macOS and Windows; a session starts primed with memory and the
  project note, and ends with a note in the vault and a session in claude-mem that Claude
  Code can then find. With neither installed, Nixxa starts normally and says so.

### Stage 11 — Extensibility: MCP servers, skills, slash commands
- User-added MCP servers (#10), on the host or in the sandbox.
- Skills (user and project).
- Built-in and user slash commands.
- **Exit:**
  - a user-added stdio MCP server's tool is called through the policy gate;
  - a skill is loaded on demand;
  - a user command expands its template;
  - a project skill asks before its first use.

### Stage 12 — Parallel and background agents
- **Tabs:** several sessions at once.
- **Worktrees:** one per agent, each in its own sandbox, with the merge review screen (#6).
- **Background sessions:** the long-running `nixxa` service, a detachable TUI, and
  notifications (#9).
- **Exit:**
  - two agents work on two branches of one repo at once, each in its own sandbox;
  - the user reviews both diffs and merges one;
  - a session keeps running after the TUI quits, and re-attaches.

### Stage 13 — Accounts, providers and local-model profiles
- Provider adapters: Anthropic, OpenAI, Google Gemini, OpenRouter, Mistral, Groq, any
  OpenAI-compatible endpoint, local (Ollama / LM Studio), and cloud routes (Bedrock, Vertex
  AI, Azure / Microsoft Foundry).
- Multiple accounts per provider, default account, per-session and mid-session switching.
- Sign-in (OAuth) flows for providers whose terms allow third-party apps; each recorded in the
  decision log. (OpenRouter's arrived early, in Stage 3.)
- Encrypted-file fallback for headless Linux; token refresh and revocation.
- Accounts screen in the TUI; per-account usage and cost.
- Local-model profiles and presets (#13).
- **Exit:** add two accounts for one provider and one for another, switch between them in a
  single session, restart, and everything is still there — on Linux, macOS and Windows — with
  a test proving no secret appears in logs, the db, or the model's context. A local model runs
  with its profile's real context size.

### Stage 14 — Policy, replay and audit
- Approval modes (adds LOCKED), per-action and per-source rules (including "never send this
  source to the provider"), budgets (steps, tokens, time, cost — per session and per account),
  interrupt, user and project policy layering.
- Session replay and the audit export (JSONL hash chain, Markdown/HTML) (#4).
- **Exit:** the agent runs unattended within a budget and stops cleanly at the limit, and the
  session's exported audit log shows every action, who approved it, and every network request.

### Stage 15 — Editor and CI
- `nixxa acp` (#11) and `nixxa run --ci` plus the GitHub Action (#12).
- **Exit:**
  - an ACP-capable editor drives a session, with approvals and diffs shown in the editor;
  - a GitHub Action run takes a task, works in the sandbox under policy and a budget, and opens
    a PR with the audit log attached.

### Stage 16 — Every OS
- Sandbox backends for macOS and Windows, and a container fallback backend. Each carries the
  Stage 6 capabilities: proxy, PTY, jobs.
- **Exit:** the Stage 4 and 6 tests pass on all three OSs (or sandbox actions are clearly
  disabled where no backend is available).

### Stage 17 — Hardening & release
- Config UX, packaging (Linux, macOS, Windows), docs site, first release.
- **Exit:** `v0.1.0` tagged and installable on all three OSs.

### Stage 18 — Team (later)
- Shared policy and budgets, org-wide audit logs and session sharing (#14), with a
  self-hostable server.
- **Exit:** an org policy applies on every member's machine and can't be loosened locally; the
  org's audit view shows every member's sessions.
