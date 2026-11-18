# controller — decides

Runs the agent loop and is the only orchestrator: asks the brain, gates every action through policy, dispatches to the sandbox, records to the db, emits events.

| | |
|---|---|
| **Owns** | Loop state machine, policy gate (allow / ask / deny), routing to sandbox or knowledge, session primer and write-back, budgets, interrupts, retries |
| **Does not own** | How thinking, execution, storage or rendering work |
| **Contract** | Consumes `Brain`, `Sandbox`, `Store`, `Knowledge`, `ApprovalPort`, `EventSink` |
| **Depends on** | `contracts` only |
| **Comes alive** | Minimal in Stage 2, full policy in Stage 9 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.

## Stage 2 status

The loop from ARCHITECTURE.md §4 runs end to end against fakes:

- **Policy:** every action is `Ask` → `ApprovalPort`. A port answer of `Ask` counts as a denial.
- **Routing:** `Action::Sandbox` → `Sandbox::execute`; `Action::Knowledge` → `Knowledge::call` / `record`.
- **Recording:** user, agent, proposal (`Content::Action`) and observation messages go to the
  conversation and the `Store`; each action is stored as an `ActionRecord` with its final verdict.
- **Turns:** one at a time; messages sent during a turn are queued. The conversation carries across turns.
- **Ending a turn:** `Done`, a brain error, an infrastructure error from the sandbox or knowledge,
  a step with no intent, the per-turn step cap (`max_steps`, announced as a system message), or an interrupt.
- **Interrupt / shutdown:** drops the in-flight turn (brain stream, approval prompt or action),
  closes any open action with a `Denied`/`Failed` observation and records "interrupted by the user".

Not yet: token/time/cost budgets and approval modes (Stage 9), retries, session write-back (Stage 6).

Tests: `cargo test -p nixxa-controller`. The fakes live in `tests/controller.rs` because the
controller may depend only on `contracts`.
