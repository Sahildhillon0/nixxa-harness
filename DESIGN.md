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

### Start-up question

Every launch opens a **Start** panel before anything else (above the first-run setup screen):
*Where should the agent work?*, with the directory Nixxa was started in and three choices
(↑/↓ + enter, or the digit):

1. **Sandbox (empty)**: the real, isolated sandbox on an empty scratch folder. Your files aren't
   visible to the agent.
2. **This directory**: your real files through `HostSandbox`, with no isolation.
3. **Sandbox with this directory**: opens a second question (`esc` goes back):
   1. **Work on my files**: the directory is mounted at `/workspace` inside the sandbox. Changes
      are real, but nothing else on the machine is reachable.
   2. **Work on a copy**: a copy of the directory is mounted instead. The agent can build and test
      freely; your real directory never changes, and Nixxa says where the copy is when it quits.

A choice that can't be used here is shown with the reason instead of its description and can't be
picked: not Linux yet, `bwrap` or `nixxa-sandboxd` missing, the workspace guard, or the sandbox
failing to start. Nixxa never falls back to something less isolated (D-008, D-051). No session
starts until the question is answered; ctrl-c twice still quits. The answer is noted in the
conversation, and the status bar shows the backend, e.g. `bubblewrap ● isolated` or
`host ○ NOT ISOLATED` (D-039, D-047).

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
- **Detail (right, bottom):** full output of the selected action, or, after `ctrl+t`, the
  user's terminal (D-067).
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

**Since Stage 4 (D-052):** **Ask** and **Auto** exist, switched with `shift-tab` at any time
(including on an open prompt, which it approves). Every launch starts in Ask. Auto is shown as
`mode: AUTO` and each action it approves is marked `auto` in the activity pane. In **This
directory** (no isolation), switching to Auto takes a second `shift-tab` within 5 seconds. Today
Ask prompts for every action; in Auto, reading a file that usually holds secrets still asks
(D-076). Plan mode comes with Stage 8 (`shift-tab` will cycle Ask → Plan → Auto); the finer Ask rules, LOCKED and budgets come with Stage 14 (D-060).

### Keybindings (initial)

| Key | Action |
|---|---|
| `enter` / `shift-enter` (or `alt-enter`) | send / newline — many terminals can't tell `shift-enter` from `enter`, so `alt-enter` works too |
| `tab` | cycle pane focus |
| `shift-tab` | switch approval mode: Ask ↔ Auto (D-052) |
| `j` `k` / arrows | move within focused pane |
| `ctrl-c` | interrupt the agent (twice: quit) |
| `m` | cycle approval mode, LOCKED included (Stage 14) |
| `y` / `n` | approve / deny in a prompt |
| `ctrl-s` | session list: type to search, `enter` resume, `esc` close (Stage 5, D-059) |
| `ctrl-n` | network settings: off / allow (presets, hosts) / open, and the recent requests (Stage 6, D-065) |
| `ctrl-t` | the terminal in place of the details pane, typing into it; again (from outside it) the details (Stage 6, D-067) |
| `ctrl-]` | in the terminal: leave it (every other key goes to the shell); twice sends `ctrl-]`. Then `i` type, arrows / pgup / pgdn scroll, `s` share lines with the agent (D-067) |
| `alt-1` … `alt-9` | in a message holding secrets: send that one as it is, or not after all (Stage 7, D-077) |
| `ctrl-z` | undo, as `/undo` in the message input does: between turns, the view of what going back to a checkpoint changes; `←`/`→` further back / forward, `↑`/`↓` scroll, `enter` twice restores, `esc` closes (Stage 7, D-073) |
| `ctrl-k` | knowledge panel: source status, paths found, enable/disable per source |
| `ctrl-a` | accounts screen: add / remove / validate / set default; switch the session's account (Stage 3: the setup screen: Anthropic key, local model, or OpenRouter sign-in) |
| `?` | help overlay |

### Session list

`ctrl-s` opens the **Sessions** overlay over the panes (Stage 5, D-059):

```
┌ Sessions ──────────────────────────────────────────────────────────────────────────┐
│Search  pelican▌                                                                    │
│                                                                                    │
│This directory                                                                      │
│› 1 min ago  project  openrouter:default  (open) Please read notes.txt with a tool… │
│             The file **notes.txt** contains: hello from a file pelican             │
│  2 days ago project  anthropic:default   fix the flaky test in db                  │
│Elsewhere                                                                           │
│  3 h ago    /home/me/site  anthropic:default  deploy the preview                   │
└────────────────────────────────────────────────────────────────────────────────────┘
```

- Every stored session, newest first; the ones started in this directory come first. Each row:
  how long ago it started (relative, so no time zone is needed), the workspace (the folder name
  here, the full path elsewhere), the account it was recorded with, and the first line of its
  first message. The open session is marked `(open)`.
- Typing filters by those fields and searches the transcripts; a transcript match shows its
  snippet under the row. `↑`/`↓` choose, `enter` resumes, `ctrl-u` clears, `esc` closes.
- Resuming ends the running session first (it is saved as it goes, so nothing is lost) and
  replays the chosen one: conversation, activity with each action's result, then a note that it
  was resumed. It runs in the place chosen at this launch, with the current account.
- `nixxa --continue` does the same at start-up for the latest session in this directory that
  has a message, after the start-up question; with none, a new session starts and the
  conversation says so. Sessions nothing was said in aren't listed (except the open one).
- Text from the history (messages, snippets, paths) goes through `safe::line` like all
  untrusted text.

### Undo

`/undo` in the message input, or `ctrl-z`, opens the **Undo** view over the panes (Stage 7,
D-073). It works between turns only: during a turn or an open approval it says why and waits.

```
┌ Undo ─────────────────────────────────────────────────────────────────────────┐
│Go back to the workspace as it was before `exec rm -rf src && echo x > new.txt`│
│1 of 4 · ← further back, → forward                                             │
│                                                                               │
│Undoing changes 3 paths:                                                       │
│  remove  new.txt  (made since)                                                │
│  restore src  (deleted since)                                                 │
│  restore src/main.rs  (deleted since)                                         │
│                                                                               │
│What an undo can't bring back:                                                 │
│  · network requests, and what was sent or downloaded                          │
│  · anything outside the workspace: /tmp, other folders, other machines        │
│  · the agent's background jobs: they are stopped first; your terminal (ctrl+t)│
│    keeps running                                                              │
│  · left out of checkpoints, left as they are: target/                         │
└───────────────────────────────────────────────────────────────────────────────┘
```

- It opens on the checkpoint before the agent's last action that could change files. `←` steps
  back to earlier ones: before each action, and the start of each turn ("undo the last turn").
- What going back changes comes from the sandbox (`diff` to now): each path with what happens to
  it (`revert`, `restore`, `remove`), then the text diffs.
- `enter` asks once more (*Restore these files?*); a second `enter` restores. A checkpoint or
  turn that starts while the view is open closes it, since what it showed is out of date.
- After the restore the conversation says how many paths went back, the activity pane shows
  `↶ undo`, and the agent is told which files went back and that what it read before is stale.
- An action that ran without a checkpoint (the store was full) shows `✗ undo … can't undo` in
  the activity pane and is listed in the view.
- Paths, diffs, link targets and labels go through `safe::line` like all untrusted text.

### Secrets

Secrets are replaced by placeholders before anything reaches the agent (Stage 7, D-074 to
D-077). The user sees each one:

```
┌ Message ──────────────────────────────────────────────────────────────────────┐
│> From my terminal:                                                            │
│  AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE                                       │
│⚠ a aws-access-key becomes ‹secret:aws-access-key#2fa0› (alt+1 sends it as it is)│
│⚠ a aws-secret-key becomes ‹secret:aws-secret-key#2eb2› (alt+2 sends it as it is)│
└───────────────────────────────────────────────────────────────────────────────┘
```

- **Before sending:** the message pane lists every secret in the message and what will replace
  it. `alt+1`…`alt+9` lets one go as it is, for this message only; pressed again, it doesn't.
- **After:** each redaction is a note in the conversation (`Redacted a aws-secret-key in
  `.env`: the agent sees ‹secret:aws-secret-key#85c5› instead.`) and a `⚠ secret` line in the
  activity pane. A value sent as it is says so, in red.
- **Files that hold secrets** (`.env`, keys, `.npmrc`, …): reading one asks even in Auto, and
  the prompt says why.
- The placeholder is the same for the same value all session, so the agent can still tell two
  keys apart, and it can't be turned back into the value.

### Visual language

- **Colour has meaning, never decoration:** green = done/ok, amber = pending/needs you,
  red = denied/error, dim = sandbox internals, accent = the agent's voice.
- Status is never colour-only: every state also has an icon (`✓ ● ? ✗`).
- Uses the terminal's own palette; honours `NO_COLOR`.
- Text Nixxa did not write (agent output, command output, notes, proposed commands) never
  reaches the terminal raw: control characters show as `^[`, `^M`…, C1 controls as `�`, and
  bidi overrides as `<U+202E>`, so output can't drive the terminal or disguise an approval.
- Finishes the user hasn't seen (background jobs, turns) show as a `● N new` badge in the status
  bar until the next key (D-069).
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
| D-022 | Account metadata lives in a keychain entry (`nixxa/_registry`, JSON), not a config file | Keeps accounts fileless, so no plain-text file is ever written and D-007 holds (only the harness reads config). The registry holds no secret, just provider, label, method, default and the last four characters. | 3 |
| D-023 | `keyring` 3.6 with native backends per OS; on Linux the pure-Rust Secret Service client (zbus + RustCrypto, `async-io`) | keyring 4 needs Rust 1.88 (MSRV is 1.85). The pure-Rust Linux client avoids libdbus and OpenSSL at build time and needs no tokio reactor. No Secret Service (headless Linux) is a clear storage error; env accounts still work. | 3 |
| D-024 | Contracts carry opaque provider reasoning (`Content::Reasoning`, `BrainEvent::Reasoning`) | Claude returns signed thinking blocks that must go back unchanged. Dropping or editing them breaks the turn, and on newer accounts an edited history is a 400. Stripping them all is allowed but loses reasoning and cache hits. The controller stores them unannounced; only the producing provider reads them. | 3 |
| D-025 | Anthropic over raw HTTP with reqwest 0.12 (rustls on `ring`, OS trust store); API version `2026-10-21` | There's no official Rust SDK. reqwest 0.13's default TLS uses aws-lc, which needs a C toolchain on Windows. `2026-10-21` is the latest version, and its Messages API changes don't touch what we send. | 3 |
| D-026 | The Anthropic brain holds every intent until `message_stop`; only text deltas stream live | A server-side refusal fallback can replace a partial answer mid-stream, and `max_tokens` can cut a tool call short. Holding the batch means no call from a declined or truncated partial is ever proposed. | 3 |
| D-027 | Server-side refusal fallback (`fallbacks: "default"`) on by default, configurable | Recommended for current Claude models; Claude API only, so it's a config switch for other endpoints. Sent only for models that support it. | 3 |
| D-028 | The controller records a step's whole output (reasoning, text, every proposal) in stream order before it runs any action; the observations follow together. Unrun proposals are closed with a denial when a step stops early. | Refines D-018. The model's message must reach the provider again unchanged: splitting parallel calls into separate turns edits it (a 400 with signed thinking blocks) and teaches the model to stop calling tools in parallel. | 3 |
| D-029 | `Done` summaries are stored in the db but kept out of the conversation the brain sees | The summary isn't part of the model's message; appending it to the model's last turn is a history edit. | 3 |
| D-030 | The brain's HTTP client honours `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` (reqwest `system-proxy`) | Corporate networks need it to reach the API at all. It's the one place a module reads the environment, an exception to D-007, and limited to the standard proxy variables. | 3 |
| D-031 | First-run API-key setup in the TUI: paste (masked, last four only) → free check with Anthropic → saved to the keychain as `anthropic:default`; `ctrl-a` reopens it; `ANTHROPIC_API_KEY` works as a read-only account; `--fake` keeps the scripted brain | The key never touches a file or a log, a bad key is caught before it's saved, and offline demos still work. A storage problem is reported at startup instead of hidden. | 3 |
| D-032 | A turn that ends in an error drops its half-streamed answer from the screen | A refused or failed answer isn't an answer; the docs say to discard a refused partial. An interrupted turn still keeps what streamed, as before. | 3 |
| D-033 | Local models go through one OpenAI-compatible brain (`OpenAiCompatBrain`) with `Credential::Local` accounts; the two providers share transport, SSE, tools, trimming and history code | LM Studio, Ollama and llama.cpp all serve the same Chat Completions API, so one provider covers them and later hosted OpenAI-compatible ones. The re-plan needs a Stage 3 exit that doesn't depend on an Anthropic key. | 3 |
| D-034 | A local-server account stores no secret: its base URL is the registry hint; a key is never sent over plain http to another machine | A base URL isn't secret, and local servers need no auth. Sending a bearer key in clear text across a network would leak it. | 3 |
| D-035 | Small non-secret settings (`active_account`, a local account's `model.<account>`) live in the keychain account registry (`KeychainCredentials::setting` / `set_setting`) | It keeps the harness file-free like the accounts (D-022) without a contract change: `AccountInfo` has no model field, and which account to start with is per installation. Never secrets. | 3 |
| D-036 | Nixxa starts with the account set up last; setting up an account switches to it at once in a new session (except rotating the key of the account in use) | Setting up a local model and then still talking to Claude would surprise; the conversation isn't persisted yet (Stage 5), so a new session loses nothing that could be kept. | 3 |
| D-037 | OpenRouter OAuth (PKCE) moved forward from Stage 8 as the first `SignIn` method; claude.ai or any other subscription login stays excluded, and no other app's tokens are reused | OpenRouter's docs offer the PKCE flow to third-party apps and issue a key the user controls and can revoke, so it's within D-017. It gives a hosted option without pasting keys. A privacy notice is confirmed first, because the conversation (workspace files, notes, memory) leaves the machine and free models may log or train on it. | 3 |
| D-038 | The harness opens sign-in pages through the OS with the URL as one argument and no shell (`xdg-open`, `open`, `rundll32 url.dll,FileProtocolHandler`), and shows the address to copy if that fails | A shell would read the URL as commands. `start` is a `cmd` built-in, so on Windows it would need a shell. | 3 |
| D-039 | Every launch asks where the agent works: **Sandbox** (the default choice) or **This directory**, the real files of the directory Nixxa started in, through `HostSandbox` (not isolated, shown as `host ○ NOT ISOLATED`). Paths stay inside that directory (no `..`, absolute paths or symlinks out); commands run in the user's shell there without `ANTHROPIC_API_KEY`; every action still goes through the approval prompt | A change of plan: working on a real project is the main use, like other coding harnesses, and it can't wait for Stage 4. It weakens principle 2 only when the user picks it, on every launch, and the TUI never hides it. The sandbox choice stays the in-memory fake until Stage 4 makes it a real isolated one. *Amended by D-047: three choices, and the sandbox choices use the real sandbox.* | 3 |
| D-040 | The Linux sandbox is `nixxa-sandboxd`, a daemon inside bubblewrap that the `BwrapSandbox` client drives over length-prefixed JSON frames on stdio (versioned handshake, 8 MiB frame cap, request ids, cancel frames). The daemon does reads, writes and listings itself with `openat2(RESOLVE_BENEATH \| RESOLVE_NO_MAGICLINKS)` from a descriptor for `/workspace`, so the kernel refuses any path that leaves it | Commands and file actions both happen inside the namespaces, never on the host. Doing file actions in the daemon (not with `cat`/`tee`) keeps them exact and lets the kernel, not string checks, enforce "beneath the workspace". A tiny framed protocol is easy to fuzz and can't desync on a bad frame. | 4 |
| D-041 | Bubblewrap gets `--unshare-all --unshare-user --disable-userns --die-with-parent --new-session --cap-drop ALL --clearenv` with only PATH, HOME=/tmp, LANG and TERM=dumb set; read-only `/usr` (plus the `/bin`, `/lib`… symlinks), a few `/etc` files (`passwd`, `group`, `nsswitch.conf`, `ld.so.*`, `localtime`, `alternatives`), private `/proc`, `/dev` and `/tmp`, and the workspace at `/workspace`. Nothing else: no home, `/run/user`, D-Bus, keyring, X11/Wayland sockets, `/sys` or host `/tmp`. The client also clears the environment of the `bwrap` process itself | Least privilege: everything a command needs and nothing it doesn't. `--new-session` blocks TIOCSTI injection; `--disable-userns` stops nested user namespaces. `passwd`/`group` reveal host user names but make `id`, `ls -l` and `git` behave; nothing secret is in them. | 4 |
| D-042 | Limits: when the user's systemd can create scopes (cgroup v2 delegation), the whole sandbox runs under `systemd-run --user --scope` with `MemoryMax` (2 GiB), `MemorySwapMax=0`, `TasksMax` (512) and `OOMPolicy=continue`; every command also gets rlimits (NPROC, FSIZE, NOFILE, CPU, DATA). Without a scope the rlimits alone apply and the backend is named `bubblewrap (rlimits only, no cgroup)`. `RLIMIT_DATA` is used instead of `RLIMIT_AS` | `systemd-run` needs no cgroupfs write access of our own and is cleaned up by systemd; the scope covers the daemon and everything it starts. `OOMPolicy=continue` makes the OOM killer take the hog, not the whole sandbox. NPROC is counted per user namespace, so it caps the sandbox alone. AS breaks runtimes that reserve large address ranges (V8, Go); DATA doesn't count those reservations. | 4 |
| D-043 | ~~The daemon runs one command at a time and owns everything in its PID namespace: on timeout, cancel, output overflow, `reset`, or 250 ms after a command's shell exits, it kills every process except bubblewrap's init and itself. Dropping an `execute` future sends a cancel; a concurrent `reset` cancels a running command first; a dead daemon makes the call fail `Unavailable` and the next call starts a fresh one~~ Superseded by D-061 (Stage 6). | Killing the process group alone misses processes that call `setsid`; a PID-namespace sweep doesn't. One command at a time matches the controller (D-018). Background processes don't outlive their command, which keeps leftovers from running unseen. | 4 |
| D-044 | Landlock is applied by the daemon to itself at start-up, after opening `/workspace` and before the Hello reply, using best-effort ABI negotiation (ABI 9). Allow: read + execute on `/usr`, `/bin`, `/sbin`, `/lib*`, `/etc`, `/proc`, `/dev`, `/.nixxa`; read + write on `/dev/null`, `/dev/zero`, `/dev/urandom`, `/dev/tty`; full access to `/workspace`, `/tmp` and `/dev/shm` (POSIX shared memory); everything else denied. TCP is not restricted by Landlock (see D-045). | Landlock is the filesystem layer of D-008. Best-effort compat keeps it working across kernel versions; the deny-by-default posture means a missing rule can only remove access, never grant it. `/proc` stays readable because `kill_all` walks it. | 4 |
| D-045 | seccomp is a deny-list returning EPERM (not kill), applied after Landlock: ptrace and process_vm_readv/writev; keyctl/add_key/request_key; bpf, perf_event_open, userfaultfd; mount, umount2, pivot_root, move_mount, open_tree, fsopen, fsmount, fsconfig; unshare, setns; clone/clone3 with namespace flags (clone3 always ENOSYS so libc falls back to clone); kexec_*, init_module, finit_module, delete_module; ioctl TIOCSTI and TIOCLINUX; io_uring_setup/enter/register; personality with anything but the default; socket for families other than AF_UNIX, AF_INET and AF_INET6. `no_new_privs` is set before either layer. | A deny-list keeps ordinary tools working (an allow-list would break shells, git, python); EPERM lets programs detect and degrade instead of dying. clone3 → ENOSYS forces the libc fallback to clone, which the namespace-flag rule then filters. x86_64 and aarch64 are both supported; x32 syscalls (`nr & 0x40000000`) are refused by a small first filter. IP sockets stay allowed so commands can use the sandbox's own loopback (local dev servers, test suites): the network namespace has only `lo`, so neither the host nor the internet is reachable, which `the_network_is_unreachable` checks. | 4 |
| D-046 | When Landlock is unavailable (old kernel or LSM disabled) the daemon runs degraded rather than refusing to start: namespaces and seccomp still apply, the Hello reply reports `landlock: false`, the backend name says so, and `isolated` stays false. | Refusing would disable all sandbox actions on such kernels even though partial isolation remains; reporting the missing layer (and keeping `isolated` false) preserves D-008's rule that an incompletely isolated backend is never labelled isolated. | 4 |
| D-047 | The start-up question has three choices: **Sandbox (empty)** (`BwrapSandbox` on a fresh scratch folder), **This directory** (`HostSandbox`, unchanged from D-039) and **Sandbox with this directory**, which asks **Work on my files** (the launch directory mounted read-write at `/workspace`) or **Work on a copy** (D-048). Each sandboxed place gives the agent its own instructions: paths under `/workspace`, no internet, only localhost, and whether changes are real. `FakeSandbox` is used only with `--fake` (as the demo's empty sandbox) and in tests | The user's request: real, isolated commands on a real project is the main use, and "isolated but on my files" and "isolated on a throwaway copy" are different promises. Telling the agent which one it has keeps it from claiming changes it can't make, or making ones the user didn't expect. | 4 |
| D-048 | "Work on a copy" and "Sandbox (empty)" use scratch folders under `<data dir>/nixxa/scratch`, one per session; ones older than 3 days are removed at start-up. The copy keeps symlinks as symlinks (never followed), keeps `.git`, skips sockets, FIFOs and device nodes, and is refused up front past 2 GiB of regular files. When Nixxa quits it prints where the copy is | Symlinks copied as links can't pull in files from outside, and the sandbox refuses to follow them out anyway. Opening a FIFO would hang the copy. The cap keeps a stray `node_modules` or build tree from filling the disk silently. The copy outlives the session so the user can look at it; turning it into a patch is an open question. | 4 |
| D-049 | The workspace guard refuses **This directory**, **Work on my files** and **Work on a copy** when the launch directory is the filesystem root, the home directory or a parent of it, or contains Nixxa's config or data folder, `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config`, `~/.claude-mem`, `~/.kube` or `~/.docker` (paths canonicalised). The refused choices show why; **Sandbox (empty)** is always allowed | Rules 5 and 8: keys, other tools' memory and Nixxa's own data must never be reachable by the agent, isolated or not, and starting Nixxa in `~` by accident is easy. A refusal with a reason is clearer than a mount that quietly hides parts of the tree. Obsidian vaults join the list when Stage 6 knows where they are. | 4 |
| D-050 | The harness looks for `nixxa-sandboxd` next to its own executable, then on `PATH`. `cargo run -p nixxa-harness` doesn't build it (it belongs to `nixxa-sandbox`), so the disabled reason says to run `cargo build -p nixxa-sandbox --bin nixxa-sandboxd`. The harness fills `BwrapConfig` with `PATH` and `XDG_RUNTIME_DIR` | Beside the executable is where packaging will put it (Stage 11) and where cargo builds it; `PATH` covers installs. Cargo's artifact dependencies are unstable, so no build trick. | 4 |
| D-051 | After a sandboxed choice the harness sets the place up on a blocking thread (the copy, the systemd probe), then starts the daemon and waits for its answer (30 s) before the session starts, so the status bar shows the real backend from the first frame. If that fails, the question is asked again with that choice disabled and the reason shown | D-008: never quietly run less isolated than chosen. Starting up front turns a broken sandbox into a clear message at the start instead of a failed first action, and makes `isolated` reflect the daemon's handshake (D-046). | 4 |
| D-052 | Approval modes arrive early, in the harness only: **Ask** (the default at every launch, every action prompts) and **Auto** (the TUI's `ApprovalPort` approves at once and reports it, so the action is still shown, marked `auto`). `shift-tab` switches at any time; on an open prompt it approves that prompt. In **This directory** a second `shift-tab` within 5 s confirms. The mode is never saved; the controller's limits and the interrupt key still apply in Auto. `shift-tab` no longer moves focus backwards | The user asked for a quick way to let the agent work without prompts. Keeping it in the approval port needs no contract or controller change, and Stage 9 can still move modes into policy. Auto in an isolated sandbox can only change the workspace; in This directory it can change anything the user can, hence the confirmation. | 4 |
| D-053 | Nixxa's history is one SQLite file at `<data dir>/nixxa/nixxa.db` (created `0600` on Unix), through `rusqlite` with `bundled`, pinned to 0.37 (the newest that builds on the MSRV, 1.85). The store owns one connection on a dedicated thread and every call is a job sent to it; WAL mode, `synchronous = NORMAL`, a 5 s busy timeout and `BEGIN IMMEDIATE` for writes | Bundled SQLite needs no system library on any OS and always has FTS5. A dedicated thread keeps blocking I/O off the async runtime without making `db` depend on tokio, and a dropped call still finishes its write. WAL plus a busy timeout lets two Nixxa instances share the file; taking the write lock up front avoids WAL's busy-snapshot errors. | 5 |
| D-054 | Schema: forward-only migrations embedded in the binary, tracked with `PRAGMA user_version`, each in its own transaction, the version re-read inside it; never edit a released step. A file with a newer version, or one that isn't a database, is refused with an error naming the path before anything is written, and never deleted or overwritten. Messages and action records are stored as their exact serde JSON; real columns only for what is queried (`started_at`, `first_message`, session and append order, message text for search) | Exact JSON round-trips every `Content` variant (reasoning blocks must come back unchanged, D-028) and needs no migration when a type gains a field. Re-reading the version inside an immediate transaction makes two instances opening a new file migrate it once. Refusing early protects history written by a newer Nixxa or damaged by something else: the user decides what to do with it. | 5 |
| D-055 | Transcript search is FTS5 (`unicode61`, case- and diacritic-insensitive) over the text of `Content::Text` messages only. Typed words are quoted (never FTS5 syntax), all must match, the last as a prefix; results are bm25-ranked, one per session, with a 12-word snippet on one line | Word search is what people type; the prefix makes results follow typing. Quoting means a stray `"` or `AND (` can't error. Summaries, reasoning and observations aren't searched: the user never saw them as conversation. | 5 |
| D-056 | Resolves the D-029 question: a `Done` summary is stored as its own `Content::Summary` variant (a marker on the stored message), which brains never send and the UI never shows. `SessionInfo` also carries `first_message`, for the session list | The smallest change that lets any store keep the distinction: a filter in the controller couldn't tell a summary from agent text once stored. The conformance suite checks the marker survives a round trip and fails against a store that drops it. Without `first_message`, drawing the list would load every session in full. | 5 |
| D-057 | `Controller::resume_session` continues a stored session: it replays the history to the UI with the existing events (`Message`, `ActionProposed`, `ActionFinished`; no turn starts), gives the brain the stored conversation minus summaries, closes any proposal left without a result with a `Failed` observation and a `Deny` record ("not finished: Nixxa quit before this action finished, so it may not have run, or may have run only in part"), and adds a stored, shown system message saying the session was resumed and the workspace may have changed, and which account it continues with when that differs | No new event was needed. Providers reject a tool call without a result; saying it may or may not have run is the truth after a crash mid-action. The place isn't stored (it and any copy belong to one run), so the agent must re-check rather than trust old observations. A gone or different account never fails a resume: the user's current account carries on. | 5 |
| D-058 | `--fake` keeps sessions in `MemoryStore`. When there is no data directory or the database can't be opened, Nixxa says why in the conversation ("History is off: …") and continues with `MemoryStore` | The demo must never write the user's history. Losing history shouldn't stop the user from working, so this is the one fallback in Nixxa, and it is always visible; the file is left untouched. | 5 |
| D-059 | `ctrl-s` opens the session list (§4): newest first, this directory's sessions first, filter by text and transcript search; `enter` resumes, ending the running session first. `nixxa --continue` resumes the latest session for this directory that has a user message (every launch starts a session, so an empty one would hide the real last conversation; the list hides empty sessions too, except the open one), or starts a new one and says so. Each controller's event sink belongs to a generation that starting another session advances, so a replaced session's last events are dropped | Most returns are to the same project, so it comes first. Ending the old session is safe because everything is saved as it goes. Without generations, the old session's interrupt notice would land in the resumed conversation. | 5 |

| D-060 | Roadmap re-planned (2026-11-23). Sixteen new capabilities are placed in the existing modules (ARCHITECTURE.md §11; no new crate, dependency rule unchanged): network allowlist through a logging host-side proxy, a `ctrl+t` terminal, background jobs, patch/undo, secret redaction, an `AGENTS.md` project policy, plan mode, a test/lint loop, LSP and a repo map, user MCP servers/skills/slash commands, parallel agents in worktrees, background sessions, local-model profiles, replay and audit export, ACP and CI frontends, and team features later. The order puts daily-coding features first: Stages 6 (network, terminal, jobs), 7 (safe edits), 8 (plan + verify) and 9 (code intelligence) come before Knowledge, which moves from Stage 6 to 10. Old Stages 7–11 become 9, 13, 14, 16 and 17, with 11 (extensibility), 12 (parallel agents), 15 (editor & CI) and 18 (team) added | The user changed plans: the agent should install dependencies and run tests without opening the network, edits should be reviewable and undoable, and a whole plan approvable at once. Placing each feature inside an existing module keeps the blackbox rule. Project files (`AGENTS.md`, project skills and commands) are untrusted input and can only make Nixxa stricter. The terminal and background jobs require the daemon to track jobs rather than kill everything after each command (D-043 is superseded in Stage 6). Earlier decision rows keep their original stage numbers. | 5 |
| D-061 | The daemon tracks jobs instead of sweeping its PID namespace. Every command (the agent's foreground command, each background job; later the terminal and services) runs under its own **supervisor**: the daemon re-executes itself as `--limited`, which applies the rlimits, makes itself a child subreaper, starts `/bin/sh -c`, points its own stdout/stderr at `/dev/null`, reaps, and reports the shell's exit status on its stdin (one end of a socket pair). A job is the supervisor's descendants, found through parent links in `/proc`. Ending a job kills those descendants until none is left, then the supervisor; nothing else. The daemon is a child subreaper too: processes of a job whose supervisor died first (a command can `kill -9 $PPID`) are re-parented to it, and a sweep every 200 ms kills and reaps every child of the daemon that isn't a live supervisor. It waits on specific pids only, never `-1`, so it never steals a supervisor's status from tokio. Foreground commands keep D-043's behaviour (one at a time, time limit, output cap, killed on timeout or cancel, leftovers killed 250 ms after the shell exits), now scoped to their own tree. Per-job cgroups are not used | A subreaper per job makes attribution exact: an orphan (double fork, `setsid` daemon) can only be re-parented to its own job's supervisor while that lives, so there's no race to lose, unlike scanning `/proc` on a timer. Process groups and sessions can be left; parenthood can't. Per-job cgroups would need write access to the delegated scope from inside the sandbox (a writable cgroupfs mount), which loosens isolation; the scope's memory and task caps still bound the whole sandbox. Commands share one uid, so a command can still signal another job's processes, or kill the daemon itself (`kill -9 2`), as before; the daemon only ever kills the job it ends. Killing the daemon now takes every background job with it: the client reports each as `JobFinished` with no exit code (D-062) and starts a new daemon on the next call. Break-to-fail: replacing the tracker with the old sweep, skipping the descendant kill, dropping either subreaper or the sweep each fails a test | 6 |
| D-062 | Protocol VERSION 3. Requests carry ids and may overlap: the daemon answers job, file and ping requests while a foreground command runs (a second foreground `Exec` is refused as busy, except that a command being cancelled is first allowed to end), and pushes unsolicited `Event` frames (`JobFinished` now; network requests and terminal output later) between replies. One task in the daemon writes frames, so replies and events never interleave. In the client, a writer task sends whole frames and a reader task routes replies by id and events to subscribers (a broadcast; a subscriber that falls behind skips events with a warning instead of losing the stream). The connection lock is held only to start a daemon; foreground `Exec`s still take turns. A dropped `execute` queues a `Cancel` synchronously (no runtime needed). If the daemon dies, waiting calls fail `Unavailable` and every job it was running is reported as `JobFinished` with no exit code | `subscribe()` must work while a request is in flight, and `reset`/`JobStop` must not wait behind a running command. Writing whole frames from one task replaces D-043's `sending` flag: a caller dropped mid-send can no longer leave half a frame. Reporting lost jobs means a controller waiting for a job never waits forever | 6 |
| D-063 | Background jobs (`Exec { background: true }`, `JobStatus`, `JobOutput`, `JobStop`, `SandboxEvent::JobFinished`, `SandboxCapabilities.jobs`). At most 4 run at once; each gets a time limit (30 min default, 2 h max, whatever it asks) and keeps the last 1 MiB of each stream; all three come from `JobLimits`, which the harness passes to every backend. Ids are `j1`, `j2`… and never repeat within a sandbox: each daemon the client starts gets a generation prefix in its `Hello` (`j1`, then `2-j1` after a restart), so an id the agent holds can't come to name a different job. A job survives foreground commands but not `reset` or the session ending; jobs `reset` stopped stay known ("was stopped"), and up to 32 ended jobs are kept for status and output. One `JobOutput` shows at most 64 KiB of each stream, like a foreground command's output, whatever the table keeps. A job's CPU rlimit is raised to its maximum time. The words the agent reads (`job j1 exited with code 0 after 3.1 s`, the last N bytes of each stream) are shared by every backend. `HostSandbox` runs jobs as host processes in their own process group (killing the group ends a job; `setsid` escapes, which is one more way This directory isn't isolated), and kills every job's group synchronously when it is dropped, because quitting drops the runtime and the jobs' tasks never run again; `FakeSandbox` ends a job at once with the command as its output and the code of a final `exit N`. The brain offers `background` on `exec` only when the sandbox has jobs, and `job_output`'s `tail_bytes` defaults to 4096 | The agent needs long builds, test suites and servers without blocking its turn, and the user's terminal (Stage 6) needs the same machinery. Fixed ids and plain-text answers keep the tool surface small. Exit code `None` means "didn't exit by itself" (stopped, timed out, killed, lost). Job results waking the agent come in Stage 6 PR D | 6 |
| D-064 | The network allowlist is a host-side proxy per `BwrapSandbox`, the only way out of the sandbox's network namespace (which has only `lo`). It listens on a Unix socket in a private directory (0700, under `XDG_RUNTIME_DIR`) bind-mounted read-only at `/run/nixxa`; the daemon binds `127.0.0.1:3128` at start-up (so no command can take it) and forwards each connection with a first line naming the job that made it (found through `/proc`: the socket's inode, the process holding it, its nearest supervisor). Landlock grants exactly `ResolveUnix` (connect) beneath `/run/nixxa` and nothing else there; the public CA store (`/etc/ssl`, `/etc/ca-certificates`, `/etc/pki`) is bound read-only. **HTTPS** is `CONNECT host:port` only: the host is checked, resolved on the host, connected, and the tunnel opens only if the TLS ClientHello's server name is that host (no interception, no CA). **Plain HTTP** is absolute-form, sent on as exactly one request: origin form, `Host` written from the checked URL (the client's own `Host` is dropped), `Connection: close`, and one body (by `Content-Length`, or chunked parsed to its end; both, or a bad length, is refused with 400); nothing the client sends after it is read. It is logged with its URL. **Only public addresses are dialled**: a name resolving to loopback, RFC 1918, ULA, link-local, CGNAT, multicast, broadcast, reserved, documentation or unspecified addresses (IPv4 inside IPv6, NAT64 and 6to4 judged as that IPv4; Teredo refused) is refused even when allowed; IP literals need a rule naming that exact address. Ports 443 and 80 unless a rule names others. Commands get `HTTP(S)_PROXY`, `ALL_PROXY`, npm's `npm_config_(https_)proxy` (both cases) and `NO_PROXY=localhost,127.0.0.1,::1` while the network is on. Under **Off** commands get no proxy variables, the port closes every connection, the proxy refuses everything and nothing is logged. Every request is a `NetRequest` (in the foreground command's `Observation.network`, which waits up to 1 s for its last connections to be logged, and as a `SandboxEvent`). Blocked reasons are written for the agent ("host X isn't allowed in this session; ask the user to allow it"). Once upstream finishes, a client gets 5 s to finish too | One proxy per sandbox keeps the policy and the log together, and a Unix socket needs no host port. Checking the SNI against the CONNECT host stops a tunnel opened for an allowed name from carrying another site's traffic; checking resolved addresses (not names) stops an allowed name from reaching the user's own services or LAN. Attribution comes from the daemon, so a command connecting to the socket directly could claim another job: it is for the log, never for the policy. SSH and other non-TLS protocols can't go through (tunnels must carry TLS). **The allowlist limits which servers are reached, not which sites those servers serve:** the `Host` inside a TLS tunnel is encrypted, so a command can ask an allowed server for another site it also serves (domain fronting); allowing a name on a CDN lets commands reach whatever else that server hands out. Only TLS interception could close that, which this design rules out. Break-to-fail: without the private-address check, 4 tests fail; without the SNI check, the proxy test and the live `curl --connect-to` test fail | 6 |
| D-065 | Network settings: `ctrl+n` opens an overlay with the three modes (`1` Off, `2` Allow, `3` Open, which needs a second `3`), the presets (crates.io, npm, PyPI, GitHub) and the user's own hosts (`a` adds a host, `*.domain` or IP, with `:port`; `d` removes), and the recent requests (when, which job, host, size, or why blocked), every string through `safe::line`. Changes apply at once (`Sandbox::set_network`, for the next connection). The status bar always shows `net: off`, `net: N domains` or `net: OPEN` (`net: not restricted` in This directory, whose overlay says so and offers nothing to change). The policy lives as long as the sandbox, starts Off, and is never saved. Each change is recorded in the session as a system note (between turns, never between an action and its result), and every think step restates the current policy in the agent's instructions; there is no tool to change it. Presets were checked live: cargo uses `index.crates.io` and `static.crates.io`, git `github.com` (`codeload.github.com` for archives), PyPI `pypi.org` and `files.pythonhosted.org`, npm `registry.npmjs.org`; GitHub release assets now redirect to `release-assets.githubusercontent.com` (added alongside `objects.` and `raw.`) | The user decides what the agent can reach, sees the decision at all times, and sees what was attempted. Telling the agent the policy and the reason for each block lets it ask for a specific host instead of failing blindly. A resumed session reads the note, not a stored policy, so a new run never starts with the network open | 6 |
| D-066 | The user's terminal (`ctrl+t`) is a PTY job in the same place as the agent's commands, and it is the user's, not the agent's. Contract: `Sandbox::open_terminal(size)` → a `Terminal` handle (`write`, `resize`, `close`; dropping it closes) and a stream of `Output` bytes then one `Closed { reason }`; `SandboxCapabilities.terminal`. **In the sandbox** the daemon opens a PTY in the sandbox's own `devpts` and runs bash (else sh) as a login shell in `/workspace` under its own supervisor, which starts a new session and makes the PTY its controlling terminal: same rlimits and cgroup scope as jobs (its CPU limit raised like a background job's), its tree killed exactly on close (before `close` returns, not by the orphan sweep), but no time limit or output cap, not in the job table, no `JobFinished`, never killed by a foreground command, a cancel, the 250 ms leftover kill or `JobStop`. One at a time. **`reset` and the daemon dying end it** ("the sandbox was reset", "the sandbox stopped"), as they end everything else running; `exit` reports the shell's code. Environment: the commands' scrubbed one plus `TERM=xterm-256color`, a `PS1` that says `sandbox:` and **a home of its own**, `/nixxa-home` (a tmpfs only the daemon's Landlock layer grants), because a login shell sources start-up files from its home and writes its history there, and the commands' home (`/tmp`) is the agent's to write and read; and the proxy variables as commands get them when it opens (a later policy change reaches it only when reopened); its requests are logged as job `terminal`, never in a foreground observation. Protocol version 4: terminal requests and frames (bytes in base64) with credit-based flow control: the daemon sends at most 64 KiB of output ahead of the client's acknowledgements, sent as the stream is consumed, and stops reading the PTY until more is acknowledged, so a slow UI makes the shell wait and nothing is dropped or piles up; output never goes through the shared event broadcast. **Isolation:** the daemon's Landlock layer gains `/dev/ptmx` and `/dev/pts` (read, write, ioctl); the supervisor of every agent command adds a second layer without them, with `/dev` listable but not readable as a tree, before its shell starts (a job that can't be confined doesn't run). So no agent command can open the user's terminal to read what is typed or write to it, or make a terminal of its own, and being in a nested Landlock domain it is denied ptrace-level access (`/proc/<pid>/fd`, `environ`) to the user's shell. TIOCSTI and TIOCLINUX stay refused by seccomp for everyone, the user's shell included; `--new-session` stays, and the daemon's stdio are pipes, so no host terminal reaches the sandbox. **This directory:** the user's `$SHELL` (else `/bin/sh`) as a login shell in the directory on a PTY (`portable-pty`, which does the session setup, so no `unsafe` here), without the harness's secret variables; ending it sends `SIGHUP` (the shell passes it on to its jobs), then kills its process group, synchronously in `Drop`. Unix only: on Windows `open_terminal` says it needs ConPTY, which comes with Stage 16. **Fake:** a scripted terminal that echoes, answers `stty size` and exits | The user needs a shell where the agent works, to look, build, and fix things by hand, and it must not become a way around the agent's isolation or a way for the agent to see what the user types. A second Landlock layer keeps the terminal devices away from the agent without loosening anything the agent had (it never had a terminal). Flow control per terminal keeps a flood from blocking replies or making job and network events skip. **What the second layer doesn't cover:** it protects the terminal from the agent's *commands*, not from the agent's *files*. Anything the user runs in the terminal from the workspace (`cargo build` and its `build.rs`, `npm test`, `make`, a `.envrc`) is code the agent may have written, and it runs with the terminal's reach, keystrokes included; the private home only removes the case where the user runs nothing at all. A command of the agent's can still *signal* the user's shell (same uid, as D-061 says of jobs), and can read the command lines of the terminal's processes (`/proc/<pid>/cmdline` isn't ptrace-checked, and `hidepid` doesn't separate one uid): don't pass secrets as arguments there. **Without Landlock** (a kernel that doesn't enforce it) the agent's commands get no second layer, so there is no terminal: `capabilities().terminal` turns false once the daemon has said so, and `open_terminal` and the daemon both refuse with the reason (open questions). Break-to-fail: the terminal in the job table, on the foreground path, without its descendant kill, without the agent's layer, with TIOCSTI allowed, or without resizes each fails a test | 6 |
| D-067 | The terminal pane: `ctrl+t` shows the terminal in place of the details pane (opening one if none runs) and types into it; `ctrl+t` from outside it puts the details back, and the shell keeps running. While typing, every key is the shell's, `ctrl+c` and `ctrl+t` included (sent as an xterm sends them), and pastes too (bracketed when the program asks, with the end marker removed). **`ctrl+]` leaves** (twice sends it), then the pane's keys: arrows, pgup/pgdn, g/G scroll its 5000 lines of scrollback, `i` types, `s` shares, `tab` moves on. Output goes through `vt100` 0.15 (the version that shares `unicode-width` with ratatui 0.29) into a grid of cells and only cells are drawn, so OSC 52, titles, hyperlinks, the bell and mode switches never reach the real terminal; a cell holding anything `safe::line` would escape is drawn as `�`, and the frame is still scrubbed. The pane is sized to its area on every layout change (the whole body in the one-column layout) and the shell is told once per change. An approval prompt never takes keys typed in the terminal: it goes over the conversation, or in one column only the hint bar says an action waits. **Sharing (the open question):** `s` picks the last command and its output (from the line starting with the prompt the cursor sits after; else the last 10 lines), the arrows adjust it, and `enter` puts it in the message input as a fenced block, every line through `safe::line`; nothing is sent until the user sends it, so they see exactly what goes (secret redaction comes in Stage 7). Opening and closing are notes in the session (never the content); the status bar shows `terminal ●` | `ctrl+t` is the roadmap's key and the exit criterion's; inside a shell it is transpose-chars, which is why the leave key is another: `ctrl+]` is telnet's escape and rarely bound in shells (vim's tag jump still works by pressing it twice). The user, not a heuristic, decides what the agent sees. One key to leave, and nothing else intercepted, keeps every program usable in the pane | 6 |
| D-068 | The controller hears the sandbox (`Sandbox::subscribe`) and tells the agent when **its own** background jobs end. A job is watched from the `Started` observation of the agent's background `exec`; network requests, the user's terminal (which sends no job events) and jobs of a replaced session (whose controller is gone, D-059) are never watched. An end that arrives before its job's `Started` (a command that fails at once: the reply and the event come together) is kept, up to the 32 newest, and taken when the job is known; ids are unique per sandbox (D-063). A watched job's end becomes `Event::JobFinished` (job, command, exit code, duration, and the sandbox's own words, now in `SandboxEvent::JobFinished.status`) and a note for the agent: `Background job: job j1 exited with code 0 after 3.1 s (\`cargo build\`).`, given **once**, at the next think step of a running turn (between steps, never between an action and its result, D-065). **Wake-ups:** if the agent is idle and *waiting on* the job, the controller starts a turn itself: `Event::WakeUp`, then a recorded system message saying the turn started without the user, then the notes. Waiting means the agent started the job and the user hasn't written since; a job the agent stops itself never wakes it. Jobs ending within `wake_delay` (1 s) share one turn. At most `max_wakes` (3) such turns in a row without a user message; past that, the notes are recorded between turns and say no turn was started. Never during a turn or an open approval (the end waits for the next step, or the turn's end). A user message cancels a pending wake and its turn gets the notes after the user's message. Wake-up turns go through the same policy gate (Ask still asks) and the per-turn step cap; there is no session budget yet (Stage 14). A lost job (its sandbox died) wakes with the sandbox's `was lost` words. On resume, jobs the stored session started and never saw end are said lost once (`didn't survive`), pairing starts and ends in order (each launch's sandbox starts again at `j1`); that note marks them for the next resume. A background `exec` interrupted while it starts may leave a job running that the agent never hears of: the dropped call's cancel stops only a foreground command; `job_status` still finds it, and reset and the session's end stop it. No `notify: false` flag on `exec`: the user writing, or the agent stopping the job, already opts out, and the tool surface stays small | The agent can start a long build, end its turn, and carry on when it finishes, without the user prodding it, but can't loop on its own: the cap and the user's message bound it, and Ask mode still holds every action. Notes in the sandbox's words keep one vocabulary across `job_status` and the wake-up. Watching only Started jobs makes "whose job is it" exact. Break-to-fail: delivering the note twice, waking on a network request, removing the cap, and waking during an open approval each fail a test | 6 |
| D-069 | Notifications: when a background job or a turn finishes (a wake-up turn too), the status bar's badge counts it (`● 2 new`), cleared at the next key with the window in focus. If the user is **away**, the channels `NIXXA_NOTIFY` chooses fire: `desktop` (the default: a desktop notification and the terminal bell), `bell`, or `off` (badge only). Away means: the terminal window lost focus (crossterm focus reporting, enabled with the TUI and disabled on exit; terminals without it never say they lost focus), the user is typing in the `ctrl+t` terminal, or no key for 60 s. The text is one line, never output or model text: `job j1 failed (exit 2): cargo test…` (the command cut to 48 characters) or `the agent finished its turn`, every part through `safe::line`. Desktop notifications go through `notify-send` (Linux, BSDs; with `&`, `<`, `>` escaped, because notification servers read the body as markup) or `osascript` (macOS) with the text as separate arguments, never a shell, reaped on a thread; Windows gets the bell only, behind the same `Notifier` trait the tests fake. The activity pane shows each job end (id, command, exit, or lost/stopped/timed out) and each wake-up among the actions. The test loop (Stage 8) will add its own kind of finish | Being told when long work finishes is the point of starting it in the background; a badge alone is missed when the user is in another window, and a notification while the user is watching is noise. No crate: the two platform commands are enough, add no dependency, and an environment variable fits the harness's settings, which have no config file yet. Break-to-fail: notifying while present, and passing unsanitised text, each fail a test | 6 |
| D-070 | **Patches.** `SandboxAction::Patch { path, edits: [{search, replace}], expected_hash }`, the agent's `edit_file` tool. The hash is the lowercase hex SHA-256 of the file's raw bytes, which every `ReadFile` now returns (`Outcome::File { content, hash }`). A hash that isn't the file's fails with the current one and changes nothing. The edits apply in order, each to the result of the ones before, and each `search` must occur exactly once (overlapping occurrences count; zero or several fail, naming the edit). All of them apply, or none do. The result is written as a new file beside the old (`O_CREAT \| O_EXCL \| O_NOFOLLOW`, the same mode) and renamed over it, after checking the file wasn't changed or swapped meanwhile, so a hard link to the old file keeps the old contents. The observation is a unified diff and the new hash. A patch never follows a symbolic link, its own name included: in the daemon through `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS)` (protocol version 5), in `HostSandbox` with `symlink_metadata` first. `WriteFile` stays, and its tool says it is for new files | A stale edit can't clobber work the agent didn't see (a build, the user's terminal, another edit). Exactly-once matching keeps an edit from landing in the wrong place, and all-or-nothing keeps a file from being half-changed. Renaming a new file into place means no reader ever sees a half-written file. Diffs are what plan mode will show (Stage 8). | 7 |
| D-071 | **Checkpoints: a content-addressed store of our own**, not a shadow git store. Blobs (file contents) are files named by their SHA-256 under `<data dir>/nixxa/checkpoints/<run>/objects`, written once. Trees (each directory's entries: files with hash and mode, directories with mode, symbolic links with their target) are kept in memory, and unchanged directories share them with the checkpoint before. A file whose device, inode, size, mtime and ctime are the last snapshot's isn't read again; one changed within the last 2 s always is. Measured (release): 10,000 small files (tmpfs), the first checkpoint 90 ms, then 12 ms unchanged and 11 ms with one file changed; this repository (1,849 files, `.git` included) 150 ms first, 4 ms unchanged; the cargo registry's sources (59,384 files, 1.3 GB) 7.6 s first (1,057 MiB stored), 0.11 s unchanged. **The store** is created 0700 on the host, never mounted into the sandbox, and removed when Nixxa quits. A crash leaves it, and so copies of the workspace's files (`.env` and keys included, readable only by the user), until a later start removes it: when the store and its `alive` file (rewritten at every checkpoint) are older than 3 days and, on Linux, the Nixxa whose pid ends its name is gone. Checkpoints are not kept for resume. **Cap:** 2 GiB of contents per run, checked before a file is read (from its size) and again while it is copied, so a sparse file (`truncate -s 1T`) is refused at once instead of being written out; a file already stored that comes back after a change may be refused even though its contents would deduplicate. A checkpoint that would pass the cap is refused (`LimitExceeded`) and nothing of it is kept; the action still runs and can't be undone (D-072). **Depth:** a snapshot goes 256 directories deep at most; a workspace nested deeper can't be checkpointed (refused with the path, so not undoable) and is marked, not entered, when showing the undo view. Removing a tree of any depth (one made since a checkpoint) moves what is below 64 levels up beside it and removes it in a later round, so no recursion or descriptor count grows with depth (a command can nest 100,000 levels with relative names; before, the recursion overflowed the stack and aborted Nixxa). On macOS each directory made or removed costs more the deeper it is (5,000 levels took 48 s on CI, against 0.1 s on Linux), so a restore that removes a tree that deep is slow there and shows no reason; it took the command as long to make. To revisit with the macOS backend (Stage 16). **Left out:** directories named `target`, `node_modules`, `.venv`, `venv`, `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`, `.gradle`, `.next`, `.nuxt`, `.turbo` or `.parcel-cache` (at any depth), and FIFOs, sockets and devices. A restore leaves them as they are, and the undo view lists them. `.gitignore` is never read, and `.git` is covered. **Hostile entries:** snapshot and restore run on the host over the workspace directory, relative to held directory descriptors, one name at a time. Nothing follows a link: `O_NOFOLLOW \| O_DIRECTORY` for directories; for files, `fstatat(AT_SYMLINK_NOFOLLOW)`, then `O_NOFOLLOW \| O_NONBLOCK`, then `fstat`. A link is stored as a link and its target never read. Special files are never opened. Entries that vanish or change mid-read are skipped or taken as read. A restore removes whatever isn't in the checkpoint or is the wrong kind: a link where a directory was goes, without being followed, and the directory is made again. Files are written as new files renamed into place. Directories the agent locked are unlocked through an `O_PATH` descriptor (Linux) or `fchmodat(AT_SYMLINK_NOFOLLOW)`. A directory swapped for a link during the restore stops it with an error. On Windows (This directory only) the same checks are made by path, a step apart from the operation. **In the bwrap places, host-side, not through the daemon** | A shadow git store would need `libgit2` (C, a big dependency, MSRV risk) or a `git` binary, which may be missing and brings its own configuration, hooks and attribute filters. Git also follows `.gitignore`, which the agent controls: it could ignore a file, then delete it, and no checkpoint would hold it. Blobs and trees are all undo needs, and `sha2` is pure Rust. A fixed list leaves out what is rebuilt, like `target/` and `node_modules/`; files inside them are lost to an undo, and the view says so. Run-scoped stores keep a resumed session from restoring a snapshot older than the user's own later changes outside Nixxa. Through the daemon, every blob of a restore would have to enter the sandbox, and undo would fail with a dead or restarted daemon. On the host, the daemon's Landlock layer doesn't bound a bug in restore, so no-follow is enforced everywhere and each attack has a test (`checkpoint/tests.rs`, `tests/undo.rs`). | 7 |
| D-072 | **The controller takes the checkpoints.** One comes before every patch, write and command (foreground and background) that passed the gate, and one at the start of every turn. Each is announced (`Event::Checkpoint`) and stored on the action's `ActionRecord.checkpoint`. When none can be taken, the action runs anyway: `Event::NotUndoable` for the user, and a note for the agent at its next step. **Undo** is `Command::Undo { to, seen }`, run between turns only (kept while a turn or an approval is open). It is refused, with nothing restored, if a checkpoint newer than `seen` exists. Otherwise the agent's running background jobs are stopped first (they would go on changing files) and a checkpoint is taken, so the undo can be undone. Then the restore runs, `Event::Restored` is emitted, and one note between turns tells the agent which paths went back (40 at most), that its reads are stale, which jobs were stopped, and what isn't covered. An undo also counts as the user leading, so ended jobs don't wake the agent (D-068). The user's terminal keeps running | Taking the checkpoint before the action, not after, is what lets it be undone, and the tests check the order. Per turn too, so "undo the last turn" is one step. The `seen` check stops an undo the user chose from replacing changes they never looked at, such as a wake-up turn starting in between. Stopping jobs first beats refusing or warning: otherwise a running build would rewrite files right after the restore. The terminal is the user's, so the view says it keeps running. | 7 |
| D-073 | **`/undo` in the harness**, the only slash command until Stage 11, and `ctrl-z`. The view opens on the checkpoint before the last action; `←`/`→` move through the earlier ones and the start of each turn. It shows the sandbox's diff to now (each path as `revert`, `restore` or `remove`, then the text diffs) and what an undo can't bring back: network requests, anything outside the workspace, what checkpoints leave out, actions without a checkpoint, and jobs stopped first. The first `enter` asks and the second restores. It isn't offered during a turn or an approval, and says why. A new checkpoint or turn closes an open view. A patch shows `+added -removed` in the activity pane and its diff in the details pane, and a read shows its hash. Every path, diff and label goes through `safe::line` | One small command, easy to replace when Stage 11 brings slash commands. Two `enter`s, because a restore removes files made since. Showing the diff from the sandbox itself, not the activity history, also covers what commands, jobs and the terminal changed. | 7 |
| D-074 | **The secret scrubber, in the controller.** Everything that enters the agent's context passes it first, and the session's record keeps the scrubbed form: the user's message (terminal shares included), every observation (file reads, patch diffs, command and job output and status, listings, error words, knowledge results, the URLs in `Observation.network`), Nixxa's notes (job ends, wake-ups, undo, the network policy), the primer, `Event::Error` words and `NotUndoable` reasons, and a resumed session's history (all of it, every role: sessions stored before this PR were never scrubbed, and a session's placeholders were made with another session's key). **Detectors:** the exact secrets Nixxa holds (D-075, they win an overlap); key formats: Anthropic, OpenRouter, OpenAI, GitHub, AWS access key ids and, within 300 bytes of one, a 40-character secret, GCP service-account `private_key_id`, Slack tokens and webhooks, Stripe, Google API keys, JWTs; PEM private keys, raw or with JSON's `\n`, to `END` or the end of the text, or, with no `BEGIN`, the key lines before `END`; the password in `scheme://user:password@host` (not `${…}` references, not `pass`, `password`, `foo` and similar stand-ins); and high-entropy values assigned to secret-ish names (`*password*`, `*token*`, `*secret*`, `*api_key*`, `*credential*`…, not `*checksum*`, `*integrity*`, `*hash*`): 12 characters or more, two kinds of character, at least 3.3 bits each, quoted, or unquoted and ending its line (a comment may follow), so `token: Type,` and `key = call();` in code are left alone, as are digitless identifiers, `CONSTANT_NAMES` and `a.b.c` paths. **Placeholders:** `‹secret:<kind>#<tag>›`, the tag the first 16 bits of HMAC-SHA256 under a random key per session: the same value gets the same placeholder all session, and a short password can't be brute-forced from its tag. Placeholders already in a text are never scrubbed again. **Cuts:** the sandbox cuts output to fit its caps (64 KiB, job tails, the diff cap) before the controller sees it, so the cut moves off any token: back to the last whitespace, quote, bracket, `,`, `;` or `|` at a head's end, forward past the first token of a tail (`sandbox/src/cut.rs`). A key is kept whole or not at all, and a cut private key is still taken to the end. **The agent's own output** (its text, proposals, summaries) isn't scrubbed for the agent, which knows only what it was given, but its stored form is, in case it repeats a value the user sent as it is (D-077). **A placeholder written into a file** (`write_file` or `edit_file` content) adds a warning to the observation: the file holds the placeholder, not the secret, and the user can put the value in. **Measured** (release, one thread): 785 MiB of Rust crate sources (50,870 files) in 8.7 s; 85 findings, of which 47 private keys and 28 URL passwords are test fixtures shipped in the crates and 10 assignments, mostly test vectors. This repository: only its own planted test keys. Survey: `NIXXA_SCRUB_DIR=<dir> cargo test -p nixxa-controller --release -- --ignored --nocapture what_a_real_tree`. **Limits:** defence in depth, not a guarantee. A secret encoded (base64, URL-encoded), split across lines or strings, computed, or in a format nothing knows gets through, as does a short or low-entropy password in an assignment. Provider reasoning blocks are kept byte for byte (some are signed) and are not scrubbed. Rows stored before this PR stay as they were in the database; what is sent from them is scrubbed. The `ctrl+t` terminal shows the user's own output as it is, until they share it | One place for every way in, so a new source can't forget it, and the stored form equals what the agent saw. Hand-written detectors instead of a regex engine: each rule is a prefix and a character class, and a scan stays at a few milliseconds per 64 KiB. False positives cost a coding agent a wrong edit, so code shapes were measured against real sources and left alone. Moving the cut, rather than scrubbing before it, keeps the scrubber in one module and the sandbox ignorant of secrets. | 7 |
| D-075 | **Fingerprints of the secrets Nixxa holds, from `accounts`.** `Credentials::fingerprints()` → `SecretFingerprints`: for each API key and token (stored and environment accounts; not base URLs or cloud profile names), a salted SHA-256 and a rolling hash keyed by the same salt, labelled `<provider>-key` or `<provider>-token`. A fresh random salt per call. `find(text)` is a Rabin–Karp scan: per distinct secret length, the rolling hash over every window, confirmed by the salted hash. Secrets shorter than 8 bytes aren't kept. The type, and its matching, live in `contracts` with the conformance scenario (every key held is found exactly, nothing shows in `Debug`, a replaced or removed key isn't found). The harness gives each session's scrubber the fingerprints when it starts, and again after an account is saved. If they can't be read, the session still starts, the user is told, and known formats are still found | The controller never holds a plaintext key, yet finds one anywhere, of any format. Keeping the algorithm with the type means the side that builds fingerprints and the side that matches them can't hash differently. | 7 |
| D-076 | **Reading a file that usually holds secrets asks, even in Auto.** `ApprovalPort::ask` takes `Asking`: `ByMode` lets Auto approve, `Always` doesn't. The gate asks `Always` for `read_file` of a path matching `controller/src/sensitive.rs` `PATTERNS`, matched case-insensitively on the last path component (or the end of the path for those with a `/`): `.env`, `.env.*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.ppk`, `*.kdbx`, `*.tfvars`, `*.tfstate`, `id_*`, `.npmrc`, `.pypirc`, `.netrc`, `_netrc`, `.git-credentials`, `.pgpass`, `.htpasswd`, `credentials`, `*credentials.json`, `secrets.*`, `.secrets`, `kubeconfig`, `.kube/config`, `.docker/config.json`; not when the name ends in `.pub`, `.example`, `.sample`, `.template` or `.dist`. The prompt says why. Writing such a file is an ordinary write | Reading sends the file to the provider; a user in Auto should still decide that, with the reason in front of them. A command like `cat .env` can't be told apart by name: the scrubber is the backstop there (D-074). | 7 |
| D-077 | **The harness: warnings, the message preview and "send anyway".** Each `Event::SecretRedacted { kind, source, placeholder, sent }` is a line in the conversation (`Redacted a <kind> in <where>: the agent sees <placeholder> instead.`) and a `⚠ secret` line in the activity pane. The harness holds the session's scrubber (the controller's own, same key): while the user writes, the message pane lists each secret in the message and the placeholder that will replace it, before `enter`. Lines shared from the terminal go through the same preview and are marked as a terminal share. **Send anyway:** `alt+1`…`alt+9` toggles one listed secret to go as it is, for this message only, never a setting. Only the user's own message: a secret in an observation can't be let through, since the user can paste it if they mean to. The agent gets that value, in this session's conversation; the session's record, the UI and the logs keep the placeholder, and the warning says it was sent. **Logs** hold the kind and where from, never the value or the placeholder; the free text they do hold (error words, `NotUndoable` reasons) is scrubbed again with the log's own scrubber. **No scrubber** (no random source): no session starts | The user sees what goes before it goes. One item at a time, explicitly, so letting one value through never turns into turning redaction off. The record stays scrubbed, so a resumed session or an export never holds a value the user let through once. | 7 |
New decisions get the next ID. A reversed decision is struck through, not deleted.

## 6. Open questions

- Which LLM provider ships first in Stage 3? (Leaning Anthropic via API key.)
- Which providers, if any, permit third-party subscription sign-in at the time Stage 13 starts?
- Should an account be pinnable per workspace (e.g. this repo always uses `anthropic:work`)?
- Stage 13, account registry (from the PR #5 review): Windows caps a Credential Manager blob at
  about 2.5 KB, so a single `nixxa/_registry` entry fits only a dozen or two accounts. Shard it
  (one entry per provider) or use a more compact format? And the registry lock is per process:
  two Nixxa instances adding accounts at once can lose an update. Re-read and retry, or an
  advisory lock?
- Two Nixxa instances can resume the same session at once (both then append to it, interleaved).
  Warn when a session is open elsewhere (a lock row with a heartbeat), or refuse?
- History retention: delete a session from the list, or expire old ones? Today the only way is
  deleting the whole file (README).
- User-added external tools (Stage 11): MCP servers and skills are planned (D-060). Also a native
  plugin trait?
- More network presets (Go proxy, Maven Central, RubyGems)? Git over SSH stays unsupported (D-064:
  tunnels must carry TLS); rules narrowed to URL paths would only work for plain HTTP.
- ~~Checkpoints (Stage 7): a shadow git object store per session, or a content-addressed copy?
  What size cap, and are ignored build directories excluded?~~ Resolved by D-071.
- `AGENTS.md` policy block (Stage 7): TOML in a fenced `nixxa-policy` block, or a separate
  `.nixxa/policy.toml`? Should other tools' files (`CLAUDE.md`, `.cursorrules`) be read as
  prose?
- Plan mode (Stage 8): can the user edit a step's command or diff in the TUI, or only remove
  steps and send feedback?
- ~~The `ctrl+t` terminal (Stage 6): how is "share with the agent" chosen (a selection, the last
  command's output)?~~ Resolved by D-067.
- The agent's commands can still signal the user's terminal and read its processes' command
  lines (same uid; D-061, D-066). Scope signals with Landlock (ABI 6), which also stops the
  agent's commands signalling each other and the daemon, or give the terminal its own uid (which
  would also hide its command lines)?
- Background sessions (Stage 12): one `nixxa` service per user, or per workspace? Where does it
  listen (a Unix socket in the runtime dir, a named pipe on Windows)?
- ACP (Stage 15): should Nixxa ever let the editor perform file writes, or always write in its
  own sandbox?
- Team features (Stage 18): self-hosted only, or also hosted? What can be shared without
  sharing the content of sessions?
- "Work on a copy" (D-048): offer the agent's changes back as a patch or a diff to apply, and
  clean the copy up then?
- Windows sandbox backend: AppContainer, WSL2, or container-only?
- claude-mem write-back: how should Nixxa sessions be identified so they read naturally next to
  Claude Code sessions?
- Vault write-back layout: one note per session, or append to a daily note? (Default planned:
  one note per session.)
- Should the agent be allowed to edit existing vault notes, or only create new ones?
- License.

Resolved: *Stage 5 resume: `Done` summaries can't be told from agent text in the db; mark them
or skip them on resume?* → marked, as `Content::Summary` (D-056).

Resolved: *an unreadable registry hides stored accounts behind the env account?* → the harness
calls `KeychainCredentials::check_storage` at startup and says why keys can't be saved (D-031).

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
