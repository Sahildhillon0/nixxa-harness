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

**Stage 3 — Real brain (in progress).**
- **Done so far:**
  - Claude (`AnthropicBrain`) with keys in the OS keychain (`KeychainCredentials`);
  - the OpenAI-compatible provider for local models (`OpenAiCompatBrain`, `Credential::Local`
    accounts);
  - setup in the harness, either (1) an Anthropic API key or (2) a local model; the wiring picks
    the brain from the start account's provider; `--fake` runs the scripted brain.
- **Still to come:**
  - OpenRouter sign-in.
- **Pending:** the Anthropic live checks, which need a key.

The sandbox, store and knowledge are still fakes.
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
