# sandbox — acts

Executes an `Action` in isolation and returns an `Observation`. The only place anything actually happens. Enforces its own hard limits.

| | |
|---|---|
| **Owns** | Isolation (bubblewrap + Landlock + seccomp), workspace mount, resource limits, network policy, built-in actions, output capture |
| **Does not own** | Deciding what to run |
| **Contract** | Implements `Sandbox` |
| **Depends on** | `contracts` only |
| **Comes alive** | Fake in Stage 2, isolated out-of-process in Stage 4 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. No code yet (Stage 0).
