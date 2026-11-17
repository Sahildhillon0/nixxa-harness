# contracts — the shared language

Types, traits, events and error enums that every other module speaks. No logic.

| | |
|---|---|
| **Owns** | Domain types, module traits (`Brain`, `Sandbox`, `Store`, `ApprovalPort`, `EventSink`), `Event`, error enums, conformance test suites |
| **Does not own** | Any logic or I/O |
| **Contract** | Defines all contracts |
| **Depends on** | nothing in the workspace |
| **Comes alive** | Stage 1 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole. No code yet (Stage 0).
