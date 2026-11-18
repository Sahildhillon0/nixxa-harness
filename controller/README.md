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
- **Recording:** user, agent, reasoning, proposal (`Content::Action`) and observation messages go to
  the conversation and the `Store`; each action is stored as an `ActionRecord` with its final verdict.
- **One step, one message:** a step's whole output (reasoning, text, every proposed action) is
  recorded in stream order before any action runs. The observations then follow together. If the
  step stops early (interrupt, infrastructure error), every proposal that never ran is closed with a
  `Denied` observation, so each proposal has a result (D-028).
- **Turns:** one at a time; messages sent during a turn are queued. The conversation carries across turns.
- **Ending a turn:** `Done`, a brain error, an infrastructure error from the sandbox or knowledge,
  a step with no intent, the per-turn step cap (`max_steps`, announced as a system message), or an interrupt.
- **Interrupt / shutdown:** signals the turn, which stops at its next cancellation point (brain
  stream, approval prompt or action), closes an open action with a `Denied`/`Failed` observation
  and records "interrupted by the user". Store writes are never cancelled.
- **Shown vs recorded:** `Done` summaries are stored in the db only. They are not announced, and
  they're kept out of the conversation the brain sees, because they aren't part of the model's
  message (D-029). A `Say` streams `TextDelta`s and then announces the full text, so the
  harness should replace the streamed text with the final message, not append both.

Not yet: token/time/cost budgets and approval modes (Stage 9), retries, session write-back (Stage 6).

Tests: `cargo test -p nixxa-controller`. The fakes live in `tests/controller.rs` because the
controller may depend only on `contracts`.
