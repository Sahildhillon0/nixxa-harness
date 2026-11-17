# Design

What Nixxa should feel like, the principles behind it, and the decisions made so far.
[ARCHITECTURE.md](ARCHITECTURE.md) covers *how it is built*; this covers *what and why*.

---

## 1. Vision

An AI agent you can hand real work to **without handing it your machine**. It lives in a
sandbox, you watch it through a terminal UI, and you decide how much rope it gets. Under the
hood every part is a replaceable blackbox, so Nixxa can grow by swapping parts rather than
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

## 3. Non-goals (for now)

- Not an IDE or editor.
- Not a web or desktop GUI (the architecture allows one later — see swap guide).
- Not multi-user or a hosted service.
- Not a model — Nixxa drives models, it does not train or host them.

---

## 4. TUI design

### Layout

```
┌ nixxa ── session 7f3a ── brain: <model> ── sandbox: ● isolated ── 12 steps · $0.04 ┐
│ Conversation                              │ Activity                               │
│                                           │                                        │
│ you   list the project and run the tests  │ ✓ exec   ls -la              14 ms     │
│                                           │ ✓ read   Cargo.toml           2 ms     │
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

- **Status bar (top):** session, brain/model, sandbox state, budget used. Always visible.
- **Conversation (left):** the dialogue, streaming token by token.
- **Activity (right, top):** one line per action — status icon, kind, short target, timing.
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
| **LOCKED** | Agent may only talk and read. Every write/exec is denied. |
| **ASK** *(default)* | Reads are allowed; writes and exec ask. |
| **AUTO** | Everything inside the sandbox is allowed, within budget. Still logged. |

The mode is always shown in the hint bar and changeable with one key.

### Keybindings (initial)

| Key | Action |
|---|---|
| `enter` / `shift-enter` | send / newline |
| `tab` / `shift-tab` | cycle pane focus |
| `j` `k` / arrows | move within focused pane |
| `ctrl-c` | interrupt the agent (twice: quit) |
| `m` | cycle approval mode |
| `y` / `n` | approve / deny in a prompt |
| `ctrl-s` | session list |
| `?` | help overlay |

### Visual language

- **Colour has meaning, never decoration:** green = done/ok, amber = pending/needs you,
  red = denied/error, dim = sandbox internals, accent = the agent's voice.
- Status is never colour-only: every state also has an icon (`✓ ● ? ✗`).
- Uses the terminal's own palette; honours `NO_COLOR`.
- Sandbox state is unmissable: `● isolated` in green, `○ NOT ISOLATED` in red (Stages 2–3).

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
| D-008 | Linux-first sandbox (bubblewrap + Landlock + seccomp) | Strongest unprivileged isolation available; other OS backends later via the same trait. | 0 |

New decisions get the next ID. A reversed decision is struck through, not deleted.

## 6. Open questions

- Which LLM provider ships first in Stage 3, and how soon do local models matter?
- External tool format: MCP, a native plugin trait, or both?
- Sandbox network policy: off / allow-list / proxy-with-log?
- macOS support: which isolation backend, and in which stage?
- Should long-term memory (beyond transcripts) be a `db` concern or its own module?
- License.

---

## 7. What the user sees, stage by stage

| Stage | Experience |
|---|---|
| 0 | Documents and an empty skeleton. |
| 1 | `cargo build` works; nothing to run yet. |
| 2 | `nixxa` opens the full TUI; a scripted fake agent walks through a conversation with approvals. |
| 3 | A real model talks back and proposes actions; execution is still simulated. |
| 4 | Actions really run — inside an isolated sandbox. The status bar turns green. |
| 5 | Sessions survive restarts; a session picker appears. |
| 6 | Approval modes, budgets and an audit view; the agent can work unattended. |
| 7 | Installable release, more models and tools. |
