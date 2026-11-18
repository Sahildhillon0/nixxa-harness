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

## Stage 2 status

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

On screen: status bar (session, account · model, sandbox, knowledge sources, cost), conversation,
activity + detail, input, hint bar. Under 100 columns the panes collapse to one, with Activity
behind `tab`. Approval prompts take `y` / `n`. `NO_COLOR` is honoured.

Keys: `enter` send · `alt-enter` / `shift-enter` newline · `tab` / `shift-tab` focus ·
`j` `k` / arrows move · `end` back to newest · `ctrl-c` interrupt (twice: quit).

Not yet: approval modes and `m` (Stage 9), "allow all" / edit in the prompt (Stage 9),
session list (Stage 5), knowledge panel (Stage 6), accounts screen (Stage 3/8), help overlay.
