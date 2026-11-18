# Design

What Nixxa should feel like, the principles behind it, and the decisions made so far.
[ARCHITECTURE.md](ARCHITECTURE.md) covers *how it is built*; this covers *what and why*.

---

## 1. Vision

An AI agent you can hand real work to **without handing it your machine**. It lives in a
sandbox, you watch it through a terminal UI, and you decide how much rope it gets. It is not
amnesiac: out of the box it remembers past work (claude-mem), knows your notes (Obsidian
vault), understands your code (CodeGraph) and reads current docs (Context7). Under the hood
every part is a replaceable blackbox, so Nixxa can grow by swapping parts rather than
rewriting them.

## 2. Principles

1. **Blackbox everything.** Each module has one job and one contract. To change one thing,
   you change one folder.
2. **Sandboxed by default.** The agent never touches the host. Isolation is not a setting you
   turn on; weakening it is the setting.
3. **The human is always in the loop, and can see it.** Every action the agent takes is
   visible, attributable and (depending on mode) approvable.
4. **Terminal-native.** Keyboard-first, fast, works over SSH, respects the terminal's theme.
5. **Staged.** Every stage ships something that runs. Nothing is built ahead of its stage.
6. **Boring technology.** Proven crates, simple protocols, SQLite before anything clever.
7. **Context by default, private by design.** Memory, notes, code index and docs are on by
   default and found automatically on every OS, but they stay on the host, never enter the
   sandbox, and you can always see when their content is sent to the model.
8. **Degrade, don't fail.** A missing integration is a dimmed indicator, not an error screen.
9. **Bring any account.** Any provider, any integration, as many accounts as you have —
   added in the TUI, kept in the OS keychain, switchable per session.

## 3. Non-goals (for now)

- Not an IDE or editor.
- Not a web or desktop GUI (the architecture allows one later — see swap guide).
- Not multi-user or a hosted service.
- Not a model — Nixxa drives models, it does not train or host them.

---

## 4. TUI design

### Layout

```
┌ nixxa ─ 7f3a ─ anthropic:work · <model> ─ sandbox ● ─ mem ● vault ● graph ○ docs ● ─ $0.04 ┐
│ Conversation                              │ Activity                               │
│                                           │ ✓ mem    primer: 8 observations        │
│ you   list the project and run the tests  │ ✓ vault  primer: Projects/nixxa.md     │
│                                           │ ✓ exec   ls -la              14 ms     │
│ nixxa I'll look at the layout first.      │ ● exec   cargo test          running…  │
│       Found a Cargo workspace with 6      │                                        │
│       crates. Running the tests now.      │                                        │
│       ▌                                   │                                        │
│                                           ├────────────────────────────────────────┤
│                                           │ Detail: cargo test                     │
│                                           │   running 14 tests                     │
│                                           │   test db::resume ... ok               │
├───────────────────────────────────────────┴────────────────────────────────────────┤
│ > _                                                                                │
└ enter send · tab focus · ctrl-c interrupt · ? help ──────────────── mode: ASK ─────┘
```

- **Status bar (top):** session, active account and model (`provider:account · model`), sandbox state, knowledge sources
  (`mem` claude-mem, `vault` Obsidian, `graph` CodeGraph, `docs` Context7: `●` ready,
  `○` missing, `✗` error), budget used. Always visible.
- **Conversation (left):** the dialogue, streaming token by token.
- **Activity (right, top):** one line per action — status icon, kind (`exec`, `read`, `mem`,
  `vault`, `graph`, `docs`…), short target, timing. Knowledge calls show up here like any
  other action, so you always see what was looked up and what was written.
- **Detail (right, bottom):** full output of the selected action.
- **Input (bottom):** multi-line prompt.
- **Hint bar (bottom edge):** context-sensitive keys and the current approval mode.

Narrow terminals (< 100 columns) collapse to a single column with Activity behind a tab.

### Approval prompt

When the controller returns `Ask`, a modal appears over the activity pane:

```
┌ Approve action? ────────────────────────────────┐
│ exec  rm -rf build/                              │
│ cwd   /workspace                                 │
│ why   "clean before rebuild"                     │
│                                                  │
│ [y] allow  [n] deny  [a] allow all `rm` this     │
│ session    [e] edit command   [esc] decide later │
└──────────────────────────────────────────────────┘
```

### Approval modes

| Mode | Behaviour |
|---|---|
| **LOCKED** | Agent may only talk and read (files, memory, vault, code graph, docs). Every write/exec is denied. |
| **ASK** *(default)* | Reads are allowed; writes, exec, and agent-proposed writes to memory or the vault ask. |
| **AUTO** | Everything inside the sandbox and all knowledge writes are allowed, within budget. Still logged. |

The mode is always shown in the hint bar and changeable with one key.

### Keybindings (initial)

| Key | Action |
|---|---|
| `enter` / `shift-enter` (or `alt-enter`) | send / newline — many terminals can't tell `shift-enter` from `enter`, so `alt-enter` works too |
| `tab` / `shift-tab` | cycle pane focus |
| `j` `k` / arrows | move within focused pane |
| `ctrl-c` | interrupt the agent (twice: quit) |
| `m` | cycle approval mode |
| `y` / `n` | approve / deny in a prompt |
| `ctrl-s` | session list |
| `ctrl-k` | knowledge panel: source status, paths found, enable/disable per source |
| `ctrl-a` | accounts screen: add / remove / validate / set default; switch the session's account |
| `?` | help overlay |

### Visual language

- **Colour has meaning, never decoration:** green = done/ok, amber = pending/needs you,
  red = denied/error, dim = sandbox internals, accent = the agent's voice.
- Status is never colour-only: every state also has an icon (`✓ ● ? ✗`).
- Uses the terminal's own palette; honours `NO_COLOR`.
- Text Nixxa did not write (agent output, command output, notes, proposed commands) never
  reaches the terminal raw: control characters show as `^[`, `^M`…, C1 controls as `�`, and
  bidi overrides as `<U+202E>`, so output can't drive the terminal or disguise an approval.
- Sandbox state is unmissable: `● isolated` in green, `○ NOT ISOLATED` in red (Stages 2–3).
- When vault or memory content is about to be sent to the model, the activity line carries a
  `↑` marker, so leaving the machine is always visible.

### Accounts screen

```
┌ Accounts ──────────────────────────────────────────────────────────────┐
│ Provider     Account     Method     Status      Used today   Default   │
│ anthropic    work        api key    ● valid     $1.82        ★         │
│ anthropic    personal    api key    ● valid     $0.00                  │
│ openai       env         env var    ● valid     $0.10                  │
│ ollama       local       local      ○ offline   —                      │
│ context7     default     api key    ● valid     —                      │
│                                                                        │
│ [n] new  [enter] use for this session  [d] set default  [v] validate   │
│ [x] remove                                                             │
└────────────────────────────────────────────────────────────────────────┘
```

Adding an account: pick provider → pick method (only the ones that provider supports) →
paste key (masked) / open browser / choose cloud profile → Nixxa validates it → saved. Secrets
are never shown again after entry, only their last four characters.

### Session write-back

At session end (on by default, configurable per source) Nixxa writes:
- a **session note** into the Obsidian vault (configurable folder, default `Sessions/`):
  frontmatter (date, workspace, model, duration, cost), a summary, the actions taken, and
  `[[links]]` to the project note;
- a **session record** into claude-mem, so Claude Code sessions can find Nixxa's work and vice
  versa.

Both appear in the activity pane as the last lines of the session.

---

## 5. Decision log

| ID | Decision | Why | Stage |
|---|---|---|---|
| D-001 | Rust | Crate boundaries enforce the blackbox design at compile time; strong fit for sandboxing and a fast TUI. | 0 |
| D-002 | ratatui + crossterm | Mature, actively maintained, cross-platform terminal UI. | 0 |
| D-003 | Modules talk only through `contracts/` | Makes every module swappable and testable alone. | 0 |
| D-004 | Controller is the sole orchestrator; brain never executes | One place to enforce policy; model output is untrusted. | 0 |
| D-005 | Sandbox runs out of process from Stage 4 | Real isolation needs a separate process; the trait hides the move. | 0 |
| D-006 | SQLite first | Zero-setup, single file, plenty for one user; swappable later. | 0 |
| D-007 | Only the harness reads config | Modules stay pure and testable; one source of truth. | 0 |
| D-008 | Linux-first sandbox (bubblewrap + Landlock + seccomp); macOS and Windows backends in Stage 10 | Strongest unprivileged isolation available; other OS backends via the same trait. Where no backend exists, sandbox actions are disabled rather than run unisolated. | 0 |
| D-009 | New `knowledge` module for external memory/context sources | claude-mem, Obsidian, CodeGraph and Context7 are not Nixxa's own storage (`db`) and not execution (`sandbox`); one module with one adapter per source keeps each swappable. | 0 |
| D-010 | claude-mem, Obsidian, CodeGraph, Context7 on by default, auto-detected on Linux/macOS/Windows | Useful with zero config; missing sources degrade to a dimmed indicator. | 0 |
| D-011 | Knowledge sources run on the host and never in, or reachable from, the sandbox | Private notes and memory must not be exposed to code the agent runs. | 0 |
| D-012 | Obsidian accessed as plain files, vault found via Obsidian's `obsidian.json` | No plugin, works with Obsidian closed, same logic on every OS. | 0 |
| D-013 | CodeGraph used only when `.codegraph/` already exists | Indexing a repo is the user's decision. | 0 |
| D-014 | Supported on Linux, macOS and Windows; CI on all three from Stage 1 | Cross-platform problems are cheapest to catch early. | 0 |
| D-015 | New `accounts` module owns all credentials for every provider and integration, many accounts per provider | One place to add, store, refresh and audit secrets; providers and sources just ask for a credential. | 0 |
| D-016 | Secrets in the OS keychain; encrypted-file fallback only; never plain text | Keys are the most sensitive thing Nixxa holds. | 0 |
| D-017 | Subscription sign-in only where the provider officially permits third-party apps; Anthropic via API key or cloud accounts | Anthropic does not currently allow third-party products to offer claude.ai login without approval; we stay within each provider's terms. | 0 |
| D-018 | Controller runs turns one at a time, queues messages sent mid-turn, and cancels a turn with a signal checked at its cancellation points (brain stream, approval prompt, action) | Keeps the conversation a single ordered history the brain can replay. Cancelling only at those points (dropping just that future, per the contracts' cancellation convention) means store writes and history updates always complete, so an interrupt never leaves a proposal without its observation. An infrastructure error (trait error) ends the turn; action-level failures and denials go back to the brain. | 2 |
| D-019 | ratatui 0.29 + crossterm 0.28 (not ratatui 0.30) | ratatui 0.30 needs Rust 1.88, above the workspace MSRV of 1.85; 0.29 has everything Stage 2 needs. Revisit when the MSRV moves. | 2 |
| D-020 | The harness sanitises every untrusted string it draws, and scrubs the whole frame as a backstop | Escape sequences in model or command output could retitle the window, write the host clipboard (OSC 52) or spoof the approval prompt with `\r` or bidi overrides, which would defeat the human-in-the-loop principle. One choke point (`harness/src/safe.rs`) plus a buffer scrub keeps a forgotten call site from reopening the hole. | 2 |
| D-021 | Logs go to daily files in `<data dir>/nixxa/logs` (kept 7 days), metadata only | The TUI owns stdout/stderr. Logging ids, kinds, verdicts and errors, never message text or action output, keeps file contents and secrets out of logs. `NIXXA_LOG` sets the level. | 2 |

New decisions get the next ID. A reversed decision is struck through, not deleted.

## 6. Open questions

- Which LLM provider ships first in Stage 3? (Leaning Anthropic via API key.)
- Which providers, if any, permit third-party subscription sign-in at the time Stage 8 starts?
- Should an account be pinnable per workspace (e.g. this repo always uses `anthropic:work`)?
- User-added external tools (Stage 11): MCP only, or also a native plugin trait?
- Sandbox network policy: off / allow-list / proxy-with-log?
- Windows sandbox backend: AppContainer, WSL2, or container-only?
- claude-mem write-back: how should Nixxa sessions be identified so they read naturally next to
  Claude Code sessions?
- Vault write-back layout: one note per session, or append to a daily note? (Default planned:
  one note per session.)
- Should the agent be allowed to edit existing vault notes, or only create new ones?
- License.

Resolved: *long-term memory as its own module?* → yes, external memory lives in `knowledge`
(D-009); Nixxa's own transcripts stay in `db`.

---

## 7. What the user sees, stage by stage

| Stage | Experience |
|---|---|
| 0 | Documents and an empty skeleton. |
| 1 | `cargo build` works on Linux, macOS and Windows; nothing to run yet. |
| 2 | `nixxa` opens the full TUI; a scripted fake agent walks through a conversation with approvals and fake knowledge lookups. |
| 3 | Paste an API key once; a real model talks back and proposes actions; execution is still simulated. |
| 4 | Actions really run — inside an isolated sandbox (Linux). The status bar turns green. |
| 5 | Sessions survive restarts; a session picker appears. |
| 6 | `mem ●` and `vault ●` light up on their own. Sessions start knowing your past work and project notes, and end with a note in your vault. |
| 7 | `graph ●` and `docs ●` light up. The agent navigates code by meaning and reads current docs. |
| 8 | Add any number of accounts for any provider from the accounts screen; switch account or model mid-session. |
| 9 | Approval modes, per-source privacy switches, budgets (per session and per account) and an audit view; the agent can work unattended. |
| 10 | The sandbox works on macOS and Windows too. |
| 11 | Installable release on all three OSs, more models and tools. |
