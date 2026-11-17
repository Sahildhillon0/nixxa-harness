# harness — shows and wires

The `nixxa` binary: the ratatui TUI and the composition root that picks concrete modules and plugs them into the controller.

| | |
|---|---|
| **Owns** | `main`, config loading, TUI layout/widgets, keybindings, approval prompts, event rendering, wiring |
| **Does not own** | Any agent logic |
| **Contract** | Implements `ApprovalPort`, `EventSink` |
| **Depends on** | all modules (wiring only) |
| **Comes alive** | Stage 2 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. Crate exists but is empty until the stage above.
