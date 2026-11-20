# harness — shows and wires

The `nixxa` binary: the ratatui TUI and the composition root that picks concrete modules and plugs them into the controller.

| | |
|---|---|
| **Owns** | `main`, config loading, TUI layout/widgets, keybindings, approval prompts, event rendering, wiring |
| **Does not own** | Any agent logic |
| **Contract** | Implements `ApprovalPort`, `EventSink` |
| **Depends on** | all modules (wiring only) |
| **Comes alive** | Stage 2 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.

## Stage 5 status

- **History (`wiring::store`):** real runs keep sessions in `SqliteStore` at
  `<data dir>/nixxa/nixxa.db` (`paths::db_path`). `--fake` uses `MemoryStore`, so the demo never
  touches the user's history. If there is no data directory or the database can't be opened
  (damaged, from a newer Nixxa), the conversation says why and that history is off, and Nixxa
  carries on with `MemoryStore`: the one fallback, and always visible (D-058).
- **Session list (`sessions.rs`, `ctrl-s`):** an overlay of every stored session, newest first,
  this directory's first ("This directory", then "Elsewhere"). Each row: how long ago it started,
  the workspace, the account and the first user message (first line, through `safe::line`); the
  open session is marked. Typing filters by those fields and searches transcripts with
  `Store::search`, whose snippets show under the matching rows. `enter` resumes, `esc` closes.
  Resuming ends the running session first (it is saved as it goes) and runs the resumed one in
  the place chosen at this launch (D-059).
- **`--continue`** (`-c`): the first session to start is the latest one for this directory, after
  the start-up question; if there is none, a new one starts and the conversation says so.
- **Late events:** each controller's `ChannelSink` belongs to a `Generation`; starting another
  session advances it, so a replaced session's last events (its interrupt notice) never land in
  the new conversation.
- **Tests:** the list's ordering, grouping, filtering, search and keys (`sessions.rs`, `app.rs`),
  its drawing (`ui.rs`), `--continue` parsing, and end to end (`wiring.rs`): quit mid-turn with
  ctrl-c twice, reopen the same file with a new store, resume, and check the conversation, the
  activity pane and the brain's history (`stage_five_quit_mid_turn_relaunch_and_resume_…`); a
  crash mid-approval closed on resume; an unopenable database turning history off visibly.

## Stage 3 status (in progress)

`cargo run -p nixxa-harness` opens the TUI on the current directory.

- **Brains:**
  - Claude (`AnthropicBrain`) for `anthropic:*` accounts;
  - a local model (`OpenAiCompatBrain`) for `local:*` accounts, with the model saved as the
    account setting `model.<account>`.

  `wiring::Models` picks the brain from the account's provider. `--fake` uses the scripted Stage 2
  brain. Knowledge is still the Stage 2 fake; the store is real since Stage 5.
- **Where the agent works (`place.rs`, `workplace.rs`, `copy.rs`):** asked at every launch: an
  empty sandbox, this directory (`HostSandbox`, not isolated), or this directory in the sandbox,
  as the real files or a copy (`BwrapSandbox`). `workplace::blocked` lists the choices that can't
  be used here and why (not Linux, `bwrap` or `nixxa-sandboxd` missing, the workspace guard);
  they are never swapped for something less isolated. The daemon is started before the session.
  `FakeSandbox` is the empty sandbox only with `--fake`. D-047 to D-051.
- **Approval modes:** `shift-tab` switches Ask ↔ Auto (`ports::AutoApprove`); in Auto,
  `TuiApprovals` approves at once and the action is marked `auto`. D-052.
- **Start account:** the one set up last (the `active_account` setting), else the Anthropic
  default (a stored key or `ANTHROPIC_API_KEY`), else the local default. A local account without a
  model reopens setup on its server.
- **Setup screen (`setup.rs`):** shown when nothing is set up; `ctrl-a` reopens it, and `esc` goes
  back a step.
  1. **Anthropic API key:** masked (bullets plus the last four characters), checked with
     `verify_key`, stored as `anthropic:default`. `ctrl-u` clears; paste uses bracketed paste.
  2. **Local model:** the server URL (LM Studio's filled in) → its models (embedding models, and
     models the server says can't use tools, are hidden; `tools ✓` when it says they can) →
     `verify` → saved as `local:default` with the model.

  3. **Sign in with OpenRouter:** a privacy notice the user confirms (`y`) → `OpenRouterSignIn`
     starts a loopback listener → the browser opens the page (`browser.rs`: `xdg-open` / `open` /
     `rundll32 url.dll,FileProtocolHandler`, the URL as one argument, no shell), or the address
     is shown full width to copy → "Waiting for sign-in… (esc to cancel)" → the key is held in
     memory only → a picker of OpenRouter's tool-capable models (type to filter; free ones marked;
     the privacy reminder repeated) → `verify` (key at `GET /key`, then the model) → saved as
     `openrouter:default`. The note after saving says where to revoke the key.

  Saving an account starts a session with it, replacing the running one (key rotation for the
  account in use excepted).
- **Startup:** if the keychain can't be read, the conversation says so (`check_storage`).
- **Settings:** `settings.rs` reads the arguments and `ANTHROPIC_API_KEY` (trimmed), `NIXXA_MODEL`
  and `NIXXA_EFFORT` (both for Claude). Modules never read the environment (D-007).
- **Errors:** a turn that ends in an error drops its half-streamed text (D-032).
- **Tests:**
  - offline end-to-end runs through the real wiring with recorded Claude and local-model responses
    (`stage_three_end_to_end_with_recorded_claude`, `…_with_a_recorded_local_model`,
    `tests/anthropic_loop.rs`, `tests/local_loop.rs`);
  - the setup steps (`setup.rs`) and their rendering;
  - setup-screen masking;
  - a full setup run whose screens, logs and data directory are scanned for the key
    (`setup_never_writes_the_key_to_the_screen_logs_or_disk`).

## Stage 2 notes

`cargo run -p nixxa-harness` opens the TUI on the current directory with every module wired to
its fake (scripted brain, fake sandbox, in-memory store, fake knowledge).

| File | Job |
|---|---|
| `src/main.rs` | Starts the tokio runtime |
| `src/tui.rs` | Owns the terminal: keys and controller events in, draw, clean shutdown |
| `src/app.rs` | Screen state and key handling, no I/O (unit-tested) |
| `src/ui.rs` | Drawing with ratatui (tested against `TestBackend`) |
| `src/ports.rs` | `EventSink` and `ApprovalPort` over one unbounded channel to the UI loop |
| `src/wiring.rs` | Composition root; holds the Stage 2 end-to-end test |
| `src/place.rs` | The start-up question: the three choices, the second question, blocked choices |
| `src/workplace.rs` | Builds the chosen place's sandbox and instructions; the workspace guard; finding the daemon |
| `src/sessions.rs` | The session list (`ctrl-s`): rows, grouping, filtering, keys |
| `src/copy.rs` | Scratch folders and the copy for "Work on a copy" (Linux) |
| `src/safe.rs` | Makes untrusted text safe to draw (control, C1 and bidi characters made visible) and scrubs each frame |
| `src/logging.rs` | `tracing` to daily files, metadata only |
| `src/paths.rs` | Per-OS data directory (Linux XDG, macOS Application Support, Windows `%LOCALAPPDATA%`) |

On screen: status bar (session, account · model, sandbox, knowledge sources, cost), conversation,
activity + detail, input, hint bar. Under 100 columns the panes collapse to one, with Activity
behind `tab`. Approval prompts take `y` / `n`. `NO_COLOR` is honoured. A message sent while a turn is
running is shown dimmed as "(queued)" until its turn starts.

**Untrusted text:** everything drawn that Nixxa did not write goes through `safe::line` /
`safe::lines`; new UI code must do the same (the frame scrub is only a backstop).

**Logs:** `<data dir>/nixxa/logs/nixxa.YYYY-MM-DD.log`, kept 7 days; level from `NIXXA_LOG`
(default `info`). Metadata only — never message text or action output. If no data directory
can be found, logging is off and the conversation says so.

Keys: `enter` send · `alt-enter` / `shift-enter` newline · `tab` focus · `shift-tab` Ask/Auto ·
`j` `k` / arrows move · `end` back to newest · `ctrl-s` sessions · `ctrl-c` interrupt (twice: quit).

Not yet: LOCKED and `m` (Stage 9), "allow all" / edit in the prompt (Stage 9),
knowledge panel (Stage 6), accounts screen (Stage 3/8), help overlay.
