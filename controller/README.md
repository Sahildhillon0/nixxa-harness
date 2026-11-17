# controller — decides

Runs the agent loop and is the only orchestrator: asks the brain, gates every action through policy, dispatches to the sandbox, records to the db, emits events.

| | |
|---|---|
| **Owns** | Loop state machine, policy gate (allow / ask / deny), routing to sandbox or knowledge, session primer and write-back, budgets, interrupts, retries |
| **Does not own** | How thinking, execution, storage or rendering work |
| **Contract** | Consumes `Brain`, `Sandbox`, `Store`, `Knowledge`, `ApprovalPort`, `EventSink` |
| **Depends on** | `contracts` only |
| **Comes alive** | Minimal in Stage 2, full policy in Stage 9 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. No code yet (Stage 0).
