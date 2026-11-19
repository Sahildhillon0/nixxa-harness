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
> screen. Keys stay in the OS keychain, never in a file. The live checks against the Anthropic API haven't been run yet. Every launch
> asks where the agent works: the **sandbox** (still an in-memory fake) or **this directory**, where
> it reads, edits and runs commands on your real files, each action approved first. The store and
> knowledge are still fakes, and the status bar says the agent isn't isolated (`NOT ISOLATED`). `--fake` runs the
> scripted Stage 2 agent offline.

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
| 1 | Contracts | Cargo workspace, shared types/traits/events, CI on Linux/macOS/Windows, boundary checks | done (macOS/Windows CI pending) |
| 2 | Walking skeleton | TUI + minimal controller loop with fake brain, sandbox, store and knowledge | done |
| 3 | Real brain | First LLM provider, streaming, context-window management; first API key stored in the OS keychain | **in progress** |
| 4 | Real sandbox | Out-of-process isolated sandbox on Linux (bubblewrap + Landlock + seccomp) | **started** (daemon + bubblewrap; Landlock, seccomp and wiring next) |
| 5 | Real memory | SQLite store, session resume, transcript search | planned |
| 6 | Knowledge I — personal | MCP client, **Obsidian vault** + **claude-mem**, cross-OS discovery, session write-back | planned |
| 7 | Knowledge II — code & docs | **CodeGraph** + **Context7** | planned |
| 8 | Accounts & providers | All major providers, multiple accounts per provider, cloud accounts, sign-in where permitted, account switcher, per-account usage | planned |
| 9 | Policy & control | Approval modes, budgets, interrupts, audit log | planned |
| 10 | Every OS | Sandbox backends for macOS and Windows | planned |
| 11 | Hardening & release | External tools, packaging, `v0.1.0` | planned |

## Building

```sh
cargo build --workspace
cargo run -p nixxa-harness              # the TUI with Claude (asks for an API key the first time)
cargo run -p nixxa-harness -- --fake    # the scripted Stage 2 agent: no key, no network
cargo test --workspace --all-features   # includes the conformance suites
./scripts/check-deps.sh                 # the dependency rule (needs jq)
```

Requires Rust 1.85+ (edition 2024).

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
(`low`…`max`, Claude), `NIXXA_LOG` (log level), `NO_COLOR`. See `nixxa --help`. Logs go to
`<data dir>/nixxa/logs/` and contain metadata only, never your key or messages.

## Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) — modules, contracts, data flow, integrations, process model, stages
- [DESIGN.md](DESIGN.md) — principles, TUI design, interaction model, decision log
- [AGENT.md](AGENT.md) — rules for AI agents (and humans) working in this repo

## Planned stack

Rust (Cargo workspace, one crate per module) · [ratatui](https://ratatui.rs) + crossterm ·
tokio · SQLite · MCP (stdio + HTTP) · OS keychain (`keyring`) · Linux sandbox: bubblewrap / Landlock / seccomp.
Targets Linux, macOS and Windows.

## License

To be decided.
