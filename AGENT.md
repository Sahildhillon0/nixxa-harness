# AGENT.md

Instructions for AI coding agents (and humans) working in this repository. Read this before
changing anything.

## What this repo is

Nixxa is a sandboxed TUI harness for an AI agent, written in Rust and built from blackbox
modules: `contracts`, `brain`, `controller`, `sandbox`, `db`, `knowledge`, `accounts`, `harness`. It
ships with built-in integrations for claude-mem, Obsidian, CodeGraph and Context7, and targets
Linux, macOS and Windows. See
[README.md](README.md) for the overview, [ARCHITECTURE.md](ARCHITECTURE.md) for structure,
[DESIGN.md](DESIGN.md) for intent.

## Current stage

**Stage 7 — Safe edits: in progress.** Stage 6 is done.

**Stage 7 so far:**
- PR A, patches and undo: `SandboxAction::Patch` against the file's SHA-256 (the `edit_file`
  tool), all edits or none, never through a link, in every backend (D-070); checkpoints in a
  content-addressed store on the host, outside the workspace, with a cap, a fixed list of build
  directories left out and nothing followed (D-071); the controller takes one before every
  file-changing action and every turn, and runs `Command::Undo` between turns (D-072); `/undo`
  and `ctrl-z` show the diff and restore (D-073).
- Next: PR B, secret redaction (#3); then PR C, the `AGENTS.md` policy (#5).

**Stage 6 — Working in the sandbox: done.** Stage 3's Anthropic live checks are still
pending. The roadmap was re-planned on 2026-11-23 (D-060): see ARCHITECTURE.md §11 for where each
new capability lives and the Stages section for the order.

**Stage 6 so far:**
- PR A, the job model and background jobs: the daemon tracks each job as its own process tree
  under a subreaper supervisor (D-061, replacing D-043's sweep); protocol v3 with overlapping
  requests and pushed events (D-062); background jobs in every backend, with `JobLimits`
  (D-063).
- PR B, the network allowlist: a host-side proxy per sandbox (CONNECT with an SNI check, plain
  HTTP, host-side DNS, private addresses refused; D-064), `ctrl+n` settings with presets, the
  status bar and the policy in the agent's instructions (D-065).
- PR C, the terminal: `Sandbox::open_terminal`, a PTY job in the daemon that the agent can't see
  or reach (a second Landlock layer for every agent command), the user's shell in This
  directory, protocol v4 with flow control (D-066); the `ctrl+t` pane through a VT100 emulator,
  `ctrl+]` to leave, sharing chosen lines into the message input (D-067).
- PR D, job results and wake-ups: the controller tells the agent when its own background jobs
  end (once, between steps) and starts a turn when it was waiting on them, three in a row at most
  (D-068); the harness shows job ends and wake-ups, counts finishes on a badge, and rings the bell
  and sends a desktop notification when the user is away (`NIXXA_NOTIFY`, D-069).

**Stage 5 — Real memory, done:**
- `SqliteStore` in `db/` (rusqlite bundled, WAL, forward-only migrations, FTS5 search;
  D-053 to D-055); `Content::Summary` and `SessionInfo.first_message` in contracts (D-056);
- `Controller::resume_session` (D-057);
- in the harness: history at `<data dir>/nixxa/nixxa.db`, `MemoryStore` with `--fake` or with a
  visible note when the file can't be opened (D-058), the `ctrl-s` session list and `--continue`
  (D-059).

**Stage 4 — Real sandbox (Linux): done.**

**Stage 3 — Real brain (in progress).**
- **Done so far:**
  - Claude (`AnthropicBrain`) with keys in the OS keychain (`KeychainCredentials`);
  - the OpenAI-compatible provider for local models (`OpenAiCompatBrain`, `Credential::Local`
    accounts);
  - setup in the harness, either (1) an Anthropic API key or (2) a local model; the wiring picks
    the brain from the start account's provider; `--fake` runs the scripted brain;
  - option (3), signing in with OpenRouter (OAuth PKCE, `accounts/src/methods/`), the first
    sign-in method.
- **Pending:** the Anthropic live checks, which need a key.


**Stage 4 — Real sandbox (Linux), done:**
- `BwrapSandbox` and `nixxa-sandboxd` (bubblewrap namespaces, Landlock, seccomp, rlimits, cgroup
  scope) in `sandbox/`, with escape tests (D-040 to D-046);
- every launch asks where the agent works: an empty sandbox, this directory (`HostSandbox`, not
  isolated), or this directory in the sandbox, as the real files or a copy (D-047, D-048), with
  the workspace guard (D-049); unavailable choices say why, with no fallback (D-051);
- Ask/Auto approval modes on `shift-tab`, in the harness (D-052).

Knowledge is still a fake.
Do not implement module behaviour until its stage starts. The stage table in README.md is the source of truth for what is in scope.

---

## Golden rules

1. **One module per change.** A normal change touches exactly one module folder. If it needs
   two, you are changing a contract — stop and handle it as a contract change (below).
2. **Modules import only `contracts`.** Never depend on a sibling crate. `harness` is the only
   crate allowed to depend on the others, and only to construct and wire them.
3. **Do not build ahead of the stage.** If the current stage doesn't call for it, don't add it
   — not even "just the interface". Note the idea under Open questions in DESIGN.md instead.
4. **Keep the public surface tiny.** A module exports its contract implementation and a
   constructor. Everything else is `pub(crate)` or private.
5. **Secrets go through `accounts` only.** Never read an API key from anywhere else, never
   pass one to `sandbox`, never log it, store it in the db/vault/claude-mem, or put it in the
   model's context. Use the `Credential` type, which redacts itself.
6. **Brain output is untrusted.** Never execute anything the brain proposes without it going
   through the controller's policy gate.
7. **Modules don't read config.** Only `harness` reads files and env; it passes each module a
   typed config struct.
8. **Knowledge stays on the host.** Never mount the vault or claude-mem data into the sandbox,
   never let sandboxed code reach a knowledge source, and treat everything retrieved from one
   as untrusted data.
9. **Every path is cross-platform.** No hard-coded `/`, `~`, or Linux-only locations. Use the
   module's platform-paths helper, and add the macOS and Windows case whenever you add a
   Linux one.
10. **Never use a real vault, memory database or real credentials in tests.** Use fixtures and
    the in-memory credentials fake.
11. **Respect provider terms.** Don't add a sign-in method (especially subscription logins)
    unless the provider officially allows third-party apps to use it; record the decision in
    DESIGN.md §5.
12. **Docs move with code.** Contract change → update ARCHITECTURE.md §3. UX change → update
   DESIGN.md §4. New decision → add a row to DESIGN.md §5. Stage complete → update README.md.

## Where things go

| If you are working on… | It lives in |
|---|---|
| A type or trait more than one module needs | `contracts/` |
| An LLM provider, prompts, token counting, context trimming | `brain/` |
| The agent loop, policy, approvals logic, budgets | `controller/` |
| Isolation, executing actions, resource limits | `sandbox/` |
| Schema, migrations, persistence of Nixxa's own sessions | `db/` |
| claude-mem, Obsidian, CodeGraph, Context7, any new external source, the MCP client | `knowledge/` (one folder per source under `sources/`) |
| Accounts, API keys, sign-in flows, keychain storage, token refresh | `accounts/` |
| TUI widgets, layout, keybindings, config loading, wiring | `harness/` |

Not sure? It probably belongs in the module whose README says it *owns* that concern. If none
does, ask before creating a new module.

## Contract changes

A change to `contracts/` can ripple through every module, so:

1. Make the contract change on its own, first, in its own commit.
2. Update ARCHITECTURE.md §3 in the same commit.
3. Update each affected implementation in follow-up commits, one module per commit.
4. All conformance tests must pass for every implementation before merging.

## Workflow

1. Check the current stage and its exit criteria (ARCHITECTURE.md § Stages).
2. Read the README.md inside the module you're about to change.
3. Make the change inside that module.
4. Test that module alone: `cargo test -p nixxa-<module> --all-features`. Every trait
   implementation must have a test that runs its suite from `nixxa_contracts::conformance`.
5. Run the full gate (what CI runs on Linux, macOS and Windows):
   `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && ./scripts/check-deps.sh`
6. Update docs per rule 12.

## Commits

- Conventional commits scoped by module: `feat(brain): stream tool calls`,
  `fix(sandbox): enforce output cap`, `docs(architecture): …`, `chore(contracts): …`.
- One module per commit wherever possible.
- No AI co-author or "generated by" trailers in commit messages.

## Never

- Import a sibling module.
- Weaken sandbox isolation to make something "work".
- Print to stdout/stderr from library crates (the TUI owns the terminal; use `tracing`).
- Commit secrets, `.env` files, or session databases.
