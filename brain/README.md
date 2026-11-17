# brain — thinks

Turns a `Context` into a stream of brain events ending in an `Intent` (say / act / done). Never executes anything.

| | |
|---|---|
| **Owns** | LLM provider adapters (`providers/`), prompt assembly, tool schemas, token counting, context trimming, streaming |
| **Does not own** | Executing actions, policy decisions, storage |
| **Contract** | Implements `Brain` |
| **Depends on** | `contracts` only (gets `Credentials` from the harness) |
| **Comes alive** | Fake in Stage 2, first provider in Stage 3, all providers in Stage 8 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. No code yet (Stage 0).
