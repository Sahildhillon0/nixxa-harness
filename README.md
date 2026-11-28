# Nixxa

> A sandboxed, terminal-native AI agent harness built from swappable blackbox modules.

Nixxa runs an AI agent **inside a sandbox** and lets you drive and watch it from a **TUI**.
Every part of the system is a sealed module behind a contract: the brain that thinks, the
controller that decides, the sandbox that acts, the db that remembers, the knowledge layer
that connects to your memory, notes, code index and docs, the accounts module that holds your
logins and API keys, and the harness you look at.
Replace one, and the rest never notice.

> **Status: Stage 3 — Real brain (in progress).** `cargo run -p nixxa-harness` talks to Claude (`claude-sonnet-5-5` by default) with streaming and
> tool use, or to a **local model** (LM Studio / Ollama, through their OpenAI-compatible
> endpoint), or to **OpenRouter** after signing in through the browser, all chosen on the setup
> screen. Keys stay in the OS keychain, never in a file. The live checks against the Anthropic API haven't been run yet.
>
> **Stage 4 — Real sandbox (Linux): done.** Every launch asks where the agent works: an **empty
> sandbox**, **this directory** (your real files, no isolation), or **this directory inside the
> sandbox**, either your real files or a copy. The sandbox is bubblewrap + Landlock + seccomp:
> commands can't reach anything outside the workspace, and only the sandbox's own localhost
> works. Every action asks first; `shift-tab` switches to **Auto**, where the agent works without
> asking. `--fake` runs the scripted Stage 2 agent offline.
>
> **Stage 5 — Real memory: done.** Sessions are saved as you go, in SQLite. `nixxa --continue`
> picks up the latest session for this directory; `ctrl-s` lists every session and searches
> their transcripts. Knowledge is still a fake.
>
> **Stage 6 — Working in the sandbox: done.** The agent can run long builds and test
> suites as **background jobs** (up to 4 at once, 30 min each by default) and check on them while
> it works; each job is its own process tree, so ending one never touches another. The network
> is **off** until you allow it: `ctrl+n` lets you allow crates.io, npm, PyPI or GitHub (or your
> own hosts), or open it; every request is logged, and the status bar always shows `net: …`.
> **`ctrl+t` opens a terminal** in the same place as the agent, for you: the agent can't see it or
> reach it, and it survives everything the agent runs. When a background job the agent started
> finishes, the agent is told, and if it was waiting it carries on by itself; if you're away, a
> desktop notification and the bell say so.
>
> **Stage 7 — Safe edits: done.** The agent edits files with **patches** against the
> version it read: an edit made on a stale read fails instead of overwriting newer changes. Before every
> edit and command, and at the start of every turn, Nixxa takes a **checkpoint** of the
> workspace; **`/undo`** (or `ctrl-z`) shows what going back would change, and restores it, also
> after a command like `rm -rf` changed files. Checkpoints live outside the workspace and are
> removed when Nixxa quits. **Secrets** in anything headed for the model (your message, file
> reads, command and job output, notes) are replaced by placeholders such as
> `‹secret:aws-access-key#3f2a›`, and what's saved is the redacted form; you see each one, and
> can send one of your own as it is, explicitly. Reading `.env` and the like asks even in Auto.
> A repository's **`AGENTS.md`** gives the agent the project's rules in words, and a
> ` ```nixxa-policy ` block of rules Nixxa enforces: `protect = ["migrations/**"]` refuses any
> edit there before you're even asked, and in the sandbox mounts the folder read-only so no
> command (`rm -rf`, `mv`) can change it either. A repository can only make Nixxa stricter: it
> can't turn the network on (the hosts it asks for are a request you see in `ctrl+n`), raise a
> budget, turn on Auto or drop a rule from your own `policy.toml`. If the agent edits `AGENTS.md`,
> nothing changes until you accept it in `ctrl+p`, which shows every rule and where it came from.

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
| `accounts/` | Accounts | Holds every account and API key (any provider, any integration, many per provider) in the OS keychain and hands out credentials on request. | `contracts` |
| `harness/` | Harness | The TUI and the composition root: wires concrete modules together and renders events. | everything (wiring only) |

The rule that makes this work: **modules talk only through `contracts/`**. No module imports
a sibling. Want a new LLM provider? Touch `brain/`. Postgres instead of SQLite? Touch `db/`.
Notion instead of Obsidian? Touch `knowledge/`. New way to sign in? Touch `accounts/`. Details in [ARCHITECTURE.md](ARCHITECTURE.md).

```
nixxa-harness/
├── contracts/     # the language every module speaks
├── brain/         # thinks
├── controller/    # decides
├── sandbox/       # acts (isolated)
├── db/            # remembers (Nixxa's own sessions)
├── knowledge/     # connects (claude-mem, Obsidian, CodeGraph, Context7)
├── accounts/      # holds logins + API keys (OS keychain)
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

## Accounts

Bring any account you have. Every model provider and every integration that needs a credential
goes through one place, and you can keep several accounts per provider (e.g. *work* and
*personal*) and switch between them per session.

| Kind | Examples | How you add it |
|---|---|---|
| API key | Anthropic, OpenAI, Google Gemini, OpenRouter, Mistral, Groq, Context7, any OpenAI-compatible endpoint | paste the key (masked), Nixxa validates it with a cheap test call |
| Cloud account | Anthropic or other models via AWS Bedrock, Google Vertex AI, Azure / Microsoft Foundry | use the cloud's own credentials/profile |
| Sign-in (OAuth) | Providers that officially allow third-party apps to sign in | browser or device-code login, tokens refreshed automatically |
| Local | Ollama, LM Studio, llama.cpp server | base URL, no secret |
| Environment | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, … | picked up automatically as a read-only account |

Secrets live in the OS keychain (Linux Secret Service, macOS Keychain, Windows Credential
Manager), never in plain-text config, never in logs, never in the sandbox, never in the model's
context.

> **Note on subscription logins:** signing in with a consumer subscription (e.g. a Claude
> Pro/Max plan) is only offered where the provider officially permits third-party apps to use
> it. Anthropic currently does not allow third-party products to offer claude.ai login without
> its approval, so for Anthropic models Nixxa uses API keys (Anthropic Console) or cloud
> accounts. Each provider's sign-in options are checked against its terms when its adapter is
> built.

## Roadmap

Nixxa is built in stages. Each stage ends with something that runs, and has explicit exit
criteria (see [ARCHITECTURE.md § Stages](ARCHITECTURE.md#stages)).

| Stage | Name | Delivers | Status |
|---|---|---|---|
| 0 | Foundations | Repo, docs, folder skeleton | done |
| 1 | Contracts | Cargo workspace, shared types/traits/events, CI on Linux/macOS/Windows, boundary checks | done |
| 2 | Walking skeleton | TUI + minimal controller loop with fake brain, sandbox, store and knowledge | done |
| 3 | Real brain | First LLM provider, streaming, context-window management; first API key stored in the OS keychain | **in progress** (Anthropic live checks pending) |
| 4 | Real sandbox | Out-of-process isolated sandbox on Linux (bubblewrap + Landlock + seccomp) | done |
| 5 | Real memory | SQLite store, session resume, transcript search | done |
| 6 | Working in the sandbox | **Network allowlist** through a logging host-side proxy (crates.io, npm, PyPI…, or open), the **`ctrl+t` terminal**, background jobs and notifications | **done** |
| 7 | Safe edits | **Patch** action and **undo** (checkpoints), **secret redaction**, the **`AGENTS.md`** project policy | **done** |
| 8 | Plan mode & verify loop | Approve a whole **plan** (dry run) instead of each step; the **test/lint loop** within a budget | planned |
| 9 | Code intelligence | **LSP diagnostics**, a **repo map**, **CodeGraph** + **Context7** | planned |
| 10 | Knowledge — personal | MCP client, **Obsidian vault** + **claude-mem**, cross-OS discovery, session write-back | planned |
| 11 | Extensibility | User-added **MCP servers**, **skills**, **slash commands** | planned |
| 12 | Parallel & background agents | Tabs, **git worktrees** each in its own sandbox with a merge review, background sessions | planned |
| 13 | Accounts & providers | All major providers, multiple accounts per provider, cloud accounts, sign-in where permitted, account switcher, per-account usage, **local-model profiles** | planned |
| 14 | Policy, replay & audit | LOCKED mode, per-source rules, budgets, **session replay**, **audit export** | planned |
| 15 | Editor & CI | **Agent Client Protocol** for editors, **GitHub Action / CI mode** | planned |
| 16 | Every OS | Sandbox backends for macOS and Windows | planned |
| 17 | Hardening & release | Config UX, packaging, `v0.1.0` | planned |
| 18 | Team *(later)* | Shared policy and budgets, org-wide audit logs, session sharing | later |

## Building

```sh
cargo build --workspace                 # also builds the sandbox daemon, nixxa-sandboxd
cargo run -p nixxa-harness              # the TUI with Claude (asks for an API key the first time)
cargo run -p nixxa-harness -- --continue  # resume the latest session for this directory
cargo run -p nixxa-harness -- --fake    # the scripted Stage 2 agent: no key, no network, no history
cargo test --workspace --all-features   # includes the conformance suites
./scripts/check-deps.sh                 # the dependency rule (needs jq)
```

Requires Rust 1.85+ (edition 2024).

**The sandbox (Linux):** needs `bwrap` (`pacman -S bubblewrap`, `apt install bubblewrap`) and
the `nixxa-sandboxd` daemon, found next to the `nixxa` binary or on `PATH`. `cargo run -p
nixxa-harness` alone doesn't build the daemon; run `cargo build -p nixxa-sandbox --bin
nixxa-sandboxd` (or `cargo build --workspace`) once. Without them, the sandboxed choices are shown
as unavailable, with the reason. On macOS and Windows only **This directory** is available for now.

**Keys worth knowing:** `shift-tab` switches between Ask (every action asks first) and Auto (no
prompts); `ctrl-a` model setup; `ctrl-c` interrupts (twice: quit).

## Setting up a model

With nothing set up yet, Nixxa opens a setup screen with two choices. Press `ctrl-a` to come back to
it later.

1. **Anthropic API key (Claude).** Paste a key from the
   [Anthropic Console](https://console.anthropic.com). It's masked, and only the last four
   characters are ever shown. Nixxa checks it with Anthropic (a free call) and saves it in your OS
   keychain (Secret Service on Linux, Keychain on macOS, Credential Manager on Windows) as
   `anthropic:default`, never in a file. Or set `ANTHROPIC_API_KEY=…`: it's used as a read-only
   `anthropic:env` account and never saved.
2. **Local model.** Start LM Studio's server (`http://localhost:1234/v1`) or Ollama
   (`http://localhost:11434/v1`) with a model loaded, and give Nixxa the URL. It lists the
   server's chat models; pick one that can call tools (e.g. gpt-oss, qwen3, llama3.1+).
   Nixxa checks the server offers it and saves the server as `local:default`. Nothing leaves your
   machine, and it costs nothing.

3. **Sign in with OpenRouter.** First Nixxa shows what leaves your machine, and you confirm it:
   everything in the conversation (workspace files the agent reads, notes, memory) goes to
   OpenRouter and the model's host, and free models may log or train on prompts, so don't use it
   with private code. Then your browser opens OpenRouter's sign-in page; if it can't, Nixxa shows
   the address to copy. Approve it, and OpenRouter gives Nixxa a key of your own (OAuth PKCE,
   through a one-time listener on this machine). Pick a model that can use tools (free ones are
   marked), and the key is saved in the OS keychain as `openrouter:default`. Usage is billed to
   your OpenRouter account. Nixxa can't revoke the key for you: delete it in your OpenRouter key
   settings when you're done.

Nixxa starts with whichever account you set up last, and remembers it across restarts. Setting up
another one switches to it right away, in a new session.

Other settings: `NIXXA_MODEL` (the Claude model; default `claude-sonnet-5-5`), `NIXXA_EFFORT`
(`low`…`max`, Claude), `NIXXA_LOG` (log level), `NIXXA_NOTIFY` (`desktop`, `bell` or `off`: what
happens when a job or turn finishes while you're away), `NO_COLOR`. See `nixxa --help`. Logs go to
`<data dir>/nixxa/logs/` and contain metadata only, never your key or messages.

## The network

In the sandbox the network starts **off**. Press `ctrl+n` to change it for this run:

- `1` Off, `2` Allow, `3` Open (press `3` twice; it opens every public host).
- In Allow, `space` turns a preset on or off (crates.io, npm, PyPI, GitHub), `a` adds your own
  host (`example.com`, `*.example.com`, an IP, with `:port` if not 443 or 80), `d` removes one.
- The panel lists recent requests, allowed and blocked; the status bar shows `net: off`,
  `net: N domains` or `net: OPEN`.

Everything goes through a proxy on your machine: HTTPS is checked against the host it claims,
names are resolved on your machine, and local and private addresses (your router, your own
services) are never reachable, whatever you allow. The agent is told the policy and why a request
was blocked, but can't change it. In **This directory** the network isn't restricted at all.

## The terminal

Press `ctrl+t` for a shell where the agent works: inside the sandbox (with the same files,
limits and network policy as the agent's commands), or your own shell in **This directory**. It
replaces the detail pane; `ctrl+t` again (from outside it) brings the details back, and the
shell keeps running.

- While you type in it, every key goes to the shell (`ctrl+c` too). `ctrl+]` leaves it.
- Then: `i` to type again, arrows or pgup/pgdn to scroll back, `s` to share lines with the agent:
  it picks your last command and its output, the arrows adjust it, and `enter` puts it in your
  message, where you can edit it before sending. Nothing from the terminal reaches the agent
  otherwise.
- The agent's commands can't read or type into it, and its output is drawn through a terminal
  emulator, so nothing it prints can change your real terminal.

## Your history

Every session is saved as it goes (messages, actions and their results; never keys) in one
SQLite file, readable only by you on Linux and macOS:

| OS | File |
|---|---|
| Linux | `$XDG_DATA_HOME/nixxa/nixxa.db`, else `~/.local/share/nixxa/nixxa.db` |
| macOS | `~/Library/Application Support/nixxa/nixxa.db` |
| Windows | `%LOCALAPPDATA%\nixxa\nixxa.db` |

- `nixxa --continue` resumes the latest conversation started in the current directory (sessions
  where nothing was said are skipped).
- `ctrl-s` lists every session (this directory's first); type to search, `enter` to resume.
- A resumed session runs in the place you choose at that launch; the agent is told the
  workspace may have changed.
- `--fake` never reads or writes your history. If the file can't be opened, Nixxa says why,
  runs without history, and leaves the file alone.
- **To delete your history,** quit Nixxa and delete `nixxa.db` and, if present, `nixxa.db-wal`
  and `nixxa.db-shm` next to it. A new, empty history starts on the next launch.

## Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) — modules, contracts, data flow, integrations, process model, stages
- [DESIGN.md](DESIGN.md) — principles, TUI design, interaction model, decision log
- [AGENT.md](AGENT.md) — rules for AI agents (and humans) working in this repo

## Planned stack

Rust (Cargo workspace, one crate per module) · [ratatui](https://ratatui.rs) + crossterm ·
tokio · SQLite · MCP (stdio + HTTP) · OS keychain (`keyring`) · Linux sandbox: bubblewrap / Landlock / seccomp.
Targets Linux, macOS and Windows.

## License

[MIT](LICENSE). Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).
